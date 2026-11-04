//! Sequential archive writer and random-access reader (FORMAT_V1 §2).

use std::io::{self, Read, Seek, SeekFrom, Write};

use acx_core::{AcxError, BlockId, CodecId, GIB};

use crate::block::{IndexEntry, block_record_head, decode_index, encode_index, read_block};
use crate::fixed::{FOOTER_LEN, Footer, HEADER_LEN, Header, Span};
use crate::le::invalid;
use crate::manifest::{ManifestEntry, decode_manifest, encode_manifest};
use crate::record::{FRAME_LEN, Frame, RecordType};

/// Largest manifest or index record a reader will load.
pub const MAX_METADATA_LEN: u64 = GIB;

fn io_err(context: impl Into<String>) -> impl FnOnce(io::Error) -> AcxError {
    let context = context.into();
    move |source| AcxError::Io { context, source }
}

/// Writes header, blocks, manifest, index, and footer in one pass.
pub struct ArchiveWriter<W: Write> {
    out: W,
    offset: u64,
    blocks: Vec<IndexEntry>,
}

impl<W: Write> ArchiveWriter<W> {
    pub fn new(mut out: W) -> Result<Self, AcxError> {
        out.write_all(&Header::default().encode())
            .map_err(io_err("writing archive header"))?;
        Ok(Self {
            out,
            offset: HEADER_LEN as u64,
            blocks: Vec::new(),
        })
    }

    /// Appends a block whose decoded bytes are `original_size` long with
    /// BLAKE3 `digest`, stored as `payload` encoded with `codec`.
    pub fn write_block(
        &mut self,
        codec: CodecId,
        original_size: u64,
        digest: [u8; 32],
        payload: &[u8],
    ) -> Result<BlockId, AcxError> {
        let id = u32::try_from(self.blocks.len())
            .map(BlockId)
            .map_err(|_| AcxError::ResourceLimit("more than u32::MAX blocks".into()))?;
        let entry = IndexEntry::new(
            id,
            codec,
            self.offset,
            original_size,
            payload.len() as u64,
            digest,
        );
        let head = block_record_head(&entry, crc32c::crc32c(payload));
        self.out
            .write_all(&head)
            .and_then(|()| self.out.write_all(payload))
            .map_err(io_err("writing block"))?;
        self.offset += entry.record.len;
        self.blocks.push(entry);
        Ok(id)
    }

    /// Blocks written so far, in Block ID order.
    pub fn blocks(&self) -> &[IndexEntry] {
        &self.blocks
    }

    /// Flushes buffered output, so blocks written so far can be read back
    /// through another handle to the same file.
    pub fn flush(&mut self) -> Result<(), AcxError> {
        self.out.flush().map_err(io_err("flushing archive"))
    }

    /// Writes manifest, index, and footer, and returns the flushed output.
    /// The manifest is checked with the reader's rules first, so a writer
    /// never produces an archive its reader would reject.
    pub fn finish(mut self, manifest: &[ManifestEntry]) -> Result<W, AcxError> {
        let manifest_payload = encode_manifest(manifest)?;
        decode_manifest(&manifest_payload, &self.blocks)?;
        let index_payload = encode_index(&self.blocks)?;
        let manifest = self.write_record(RecordType::Manifest, &manifest_payload)?;
        let index = self.write_record(RecordType::Index, &index_payload)?;
        let footer = Footer {
            manifest,
            index,
            archive_len: self.offset + FOOTER_LEN as u64,
            manifest_crc: crc32c::crc32c(&manifest_payload),
            index_crc: crc32c::crc32c(&index_payload),
        };
        self.out
            .write_all(&footer.encode())
            .and_then(|()| self.out.flush())
            .map_err(io_err("writing archive footer"))?;
        Ok(self.out)
    }

    fn write_record(&mut self, kind: RecordType, payload: &[u8]) -> Result<Span, AcxError> {
        let span = Span {
            offset: self.offset,
            len: (FRAME_LEN + payload.len()) as u64,
        };
        self.out
            .write_all(&Frame::encode(kind, payload.len() as u64))
            .and_then(|()| self.out.write_all(payload))
            .map_err(io_err(format!("writing {kind:?} record")))?;
        self.offset += span.len;
        Ok(span)
    }
}

/// An opened archive whose header, footer, index, and manifest have passed
/// every structural and CRC check.
pub struct ArchiveReader<R: Read + Seek> {
    inner: R,
    header: Header,
    footer: Footer,
    blocks: Vec<IndexEntry>,
    entries: Vec<ManifestEntry>,
}

impl<R: Read + Seek> ArchiveReader<R> {
    /// Rejects blocks larger than `max_block_size` before allocating.
    pub fn open(mut inner: R, max_block_size: u64) -> Result<Self, AcxError> {
        let file_len = inner
            .seek(SeekFrom::End(0))
            .map_err(io_err("sizing archive"))?;
        if file_len < (HEADER_LEN + FOOTER_LEN) as u64 {
            return Err(invalid(format!(
                "{file_len} bytes is too short to be an ACX archive"
            )));
        }
        let mut header = [0; HEADER_LEN];
        read_at(&mut inner, 0, &mut header)?;
        let header = Header::decode(&header)?;
        let mut footer = [0; FOOTER_LEN];
        read_at(&mut inner, file_len - FOOTER_LEN as u64, &mut footer)?;
        let footer = Footer::decode(&footer, file_len)?;

        let index = read_metadata(
            &mut inner,
            footer.index,
            RecordType::Index,
            footer.index_crc,
        )?;
        let blocks = decode_index(&index, footer.manifest.offset, max_block_size)?;
        let manifest = read_metadata(
            &mut inner,
            footer.manifest,
            RecordType::Manifest,
            footer.manifest_crc,
        )?;
        let entries = decode_manifest(&manifest, &blocks)?;
        Ok(Self {
            inner,
            header,
            footer,
            blocks,
            entries,
        })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn footer(&self) -> &Footer {
        &self.footer
    }

    pub fn blocks(&self) -> &[IndexEntry] {
        &self.blocks
    }

    pub fn entries(&self) -> &[ManifestEntry] {
        &self.entries
    }

    /// Reads one block record and checks it against its index entry and
    /// payload CRC. The encoded payload is `&record[BLOCK_RECORD_HEAD_LEN..]`.
    pub fn read_block_record(&mut self, entry: &IndexEntry) -> Result<Vec<u8>, AcxError> {
        read_block_record(&mut self.inner, entry)
    }

    /// Checks that any gaps between blocks hold only optional records of
    /// unknown types, exactly filling the gap (FORMAT_V1 §8).
    pub fn check_gaps(&mut self) -> Result<(), AcxError> {
        let mut pos = HEADER_LEN as u64;
        let starts: Vec<(u64, u64)> = self
            .blocks
            .iter()
            .map(|b| (b.record.offset, b.record.len))
            .chain([(self.footer.manifest.offset, 0)])
            .collect();
        for (start, len) in starts {
            while pos < start {
                let mut frame = [0; FRAME_LEN];
                if start - pos < FRAME_LEN as u64 {
                    return Err(invalid(format!(
                        "{} stray bytes at offset {pos}",
                        start - pos
                    )));
                }
                read_at(&mut self.inner, pos, &mut frame)?;
                let f = Frame::decode(&frame)?;
                if f.record_type.is_some() || !f.optional {
                    return Err(invalid(format!("unexpected record at offset {pos}")));
                }
                pos = (FRAME_LEN as u64)
                    .checked_add(f.payload_len)
                    .and_then(|n| pos.checked_add(n))
                    .filter(|&end| end <= start)
                    .ok_or_else(|| {
                        invalid(format!("record at offset {pos} overruns the next block"))
                    })?;
            }
            pos = start + len; // blocks were checked not to overflow or overlap
        }
        Ok(())
    }
}

/// [`ArchiveReader::read_block_record`] on any handle to the same archive, so
/// parallel workers can each read with their own file handle. `entry` must
/// come from a validated index, which bounds the allocation.
pub fn read_block_record<R: Read + Seek>(
    r: &mut R,
    entry: &IndexEntry,
) -> Result<Vec<u8>, AcxError> {
    let mut record = vec![0; entry.record.len as usize];
    read_at(r, entry.record.offset, &mut record)?;
    read_block(&record, entry)?;
    Ok(record)
}

fn read_at<R: Read + Seek>(r: &mut R, offset: u64, buf: &mut [u8]) -> Result<(), AcxError> {
    r.seek(SeekFrom::Start(offset))
        .and_then(|_| r.read_exact(buf))
        .map_err(io_err(format!(
            "reading {} bytes at offset {offset}",
            buf.len()
        )))
}

fn read_metadata<R: Read + Seek>(
    r: &mut R,
    span: Span,
    kind: RecordType,
    crc: u32,
) -> Result<Vec<u8>, AcxError> {
    if span.len > MAX_METADATA_LEN {
        return Err(AcxError::ResourceLimit(format!(
            "{kind:?} record of {} bytes exceeds the {MAX_METADATA_LEN}-byte limit",
            span.len
        )));
    }
    let mut frame = [0; FRAME_LEN];
    read_at(r, span.offset, &mut frame)?;
    Frame::expect(&frame, kind, span.len)?;
    let mut payload = vec![0; span.len as usize - FRAME_LEN]; // span.len >= 16, <= 1 GiB
    read_at(r, span.offset + FRAME_LEN as u64, &mut payload)?;
    let actual = crc32c::crc32c(&payload);
    if actual != crc {
        return Err(AcxError::Integrity(format!(
            "{kind:?} CRC32C mismatch (stored 0x{crc:08x}, computed 0x{actual:08x})"
        )));
    }
    Ok(payload)
}
