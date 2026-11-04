//! Data block records and the index (FORMAT_V1 §6, §8).

use acx_core::{AcxError, BlockId, CodecId};

use crate::fixed::{HEADER_LEN, Span};
use crate::le::{digest_at, invalid, u16_at, u32_at, u64_at};
use crate::record::{FRAME_LEN, Frame, RecordType};

/// Block fields between the frame and the encoded payload.
pub const BLOCK_HEAD_LEN: usize = 64;
/// Frame plus block fields: the payload starts at this offset in the record.
pub const BLOCK_RECORD_HEAD_LEN: usize = FRAME_LEN + BLOCK_HEAD_LEN;
pub const INDEX_ENTRY_LEN: usize = 72;
pub const INDEX_SCHEMA_VERSION: u32 = 1;
const INDEX_HEAD_LEN: usize = 8;

/// One block as described by the index. The writer builds it with
/// [`IndexEntry::new`]; readers get it from [`decode_index`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    pub id: BlockId,
    pub codec: CodecId,
    /// Offset and total length of the block record.
    pub record: Span,
    pub original_size: u64,
    pub compressed_size: u64,
    /// BLAKE3 of the decoded bytes.
    pub digest: [u8; 32],
}

impl IndexEntry {
    /// For a block whose record starts at `offset`; `compressed_size` must be
    /// a real payload length (at most `MAX_BLOCK_SIZE`).
    pub fn new(
        id: BlockId,
        codec: CodecId,
        offset: u64,
        original_size: u64,
        compressed_size: u64,
        digest: [u8; 32],
    ) -> Self {
        Self {
            id,
            codec,
            record: Span {
                offset,
                len: BLOCK_RECORD_HEAD_LEN as u64 + compressed_size,
            },
            original_size,
            compressed_size,
            digest,
        }
    }
}

/// Frame and block fields for `entry`; write the encoded payload right after.
pub fn block_record_head(entry: &IndexEntry, payload_crc: u32) -> [u8; BLOCK_RECORD_HEAD_LEN] {
    let mut b = [0; BLOCK_RECORD_HEAD_LEN];
    let payload_len = BLOCK_HEAD_LEN as u64 + entry.compressed_size;
    b[..FRAME_LEN].copy_from_slice(&Frame::encode(RecordType::Block, payload_len));
    let h = &mut b[FRAME_LEN..];
    h[0..4].copy_from_slice(&entry.id.0.to_le_bytes());
    h[4..6].copy_from_slice(&entry.codec.as_u16().to_le_bytes());
    // [6, 8) block flags: none in v1.
    h[8..16].copy_from_slice(&entry.original_size.to_le_bytes());
    h[16..24].copy_from_slice(&entry.compressed_size.to_le_bytes());
    h[24..56].copy_from_slice(&entry.digest);
    h[56..60].copy_from_slice(&payload_crc.to_le_bytes());
    // [60, 64) reserved.
    b
}

/// Validates a whole block record (the `entry.record.len` bytes read at
/// `entry.record.offset`) against its index entry and returns the encoded
/// payload. Decoding and [`check_decoded`] are up to the caller.
pub fn read_block<'a>(record: &'a [u8], entry: &IndexEntry) -> Result<&'a [u8], AcxError> {
    let id = entry.id.0;
    if record.len() as u64 != entry.record.len || record.len() < BLOCK_RECORD_HEAD_LEN {
        return Err(invalid(format!(
            "block {id}: record is {} bytes, index says {}",
            record.len(),
            entry.record.len
        )));
    }
    let frame: &[u8; FRAME_LEN] = record[..FRAME_LEN].try_into().expect("length checked");
    Frame::expect(frame, RecordType::Block, entry.record.len)?;

    let h = &record[FRAME_LEN..BLOCK_RECORD_HEAD_LEN];
    let mismatch = |field: &str| {
        AcxError::Integrity(format!(
            "block {id}: {field} in block record disagrees with index"
        ))
    };
    if u32_at(h, 0) != id {
        return Err(mismatch("block ID"));
    }
    if u16_at(h, 4) != entry.codec.as_u16() {
        return Err(mismatch("codec ID"));
    }
    if u16_at(h, 6) != 0 || u32_at(h, 60) != 0 {
        return Err(invalid(format!(
            "block {id}: non-zero flags or reserved bytes"
        )));
    }
    if u64_at(h, 8) != entry.original_size {
        return Err(mismatch("original size"));
    }
    if u64_at(h, 16) != entry.compressed_size {
        return Err(mismatch("compressed size"));
    }
    if digest_at(h, 24) != entry.digest {
        return Err(mismatch("BLAKE3 digest"));
    }
    // The index already tied record length to compressed size, so this is the
    // whole payload.
    let payload = &record[BLOCK_RECORD_HEAD_LEN..];
    let stored = u32_at(h, 56);
    let actual = crc32c::crc32c(payload);
    if stored != actual {
        return Err(AcxError::Integrity(format!(
            "block {id}: payload CRC32C mismatch (stored 0x{stored:08x}, computed 0x{actual:08x})"
        )));
    }
    Ok(payload)
}

/// Checks decoded bytes against the index: exact size and BLAKE3.
pub fn check_decoded(data: &[u8], entry: &IndexEntry) -> Result<(), AcxError> {
    let id = entry.id.0;
    if data.len() as u64 != entry.original_size {
        return Err(AcxError::Integrity(format!(
            "block {id}: decoded {} bytes, expected {}",
            data.len(),
            entry.original_size
        )));
    }
    if *blake3::hash(data).as_bytes() != entry.digest {
        return Err(AcxError::Integrity(format!(
            "block {id}: decoded BLAKE3 mismatch"
        )));
    }
    Ok(())
}

/// Index payload (frame excluded) for `entries`, which must be in Block ID
/// order starting at 0.
pub fn encode_index(entries: &[IndexEntry]) -> Result<Vec<u8>, AcxError> {
    let count = u32::try_from(entries.len()).map_err(|_| {
        AcxError::ResourceLimit(format!(
            "{} blocks exceed the u32 block count",
            entries.len()
        ))
    })?;
    let mut out = Vec::with_capacity(INDEX_HEAD_LEN + entries.len() * INDEX_ENTRY_LEN);
    out.extend_from_slice(&INDEX_SCHEMA_VERSION.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    for e in entries {
        out.extend_from_slice(&e.id.0.to_le_bytes());
        out.extend_from_slice(&e.codec.as_u16().to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&e.record.offset.to_le_bytes());
        out.extend_from_slice(&e.record.len.to_le_bytes());
        out.extend_from_slice(&e.original_size.to_le_bytes());
        out.extend_from_slice(&e.compressed_size.to_le_bytes());
        out.extend_from_slice(&e.digest);
    }
    Ok(out)
}

/// Parses an index payload and checks every rule of FORMAT_V1 §8: IDs in
/// order, sizes within `max_block_size`, record lengths consistent, and
/// blocks in order without overlap between the header and the manifest.
pub fn decode_index(
    payload: &[u8],
    manifest_offset: u64,
    max_block_size: u64,
) -> Result<Vec<IndexEntry>, AcxError> {
    if payload.len() < INDEX_HEAD_LEN {
        return Err(invalid("index payload shorter than its 8-byte header"));
    }
    let version = u32_at(payload, 0);
    if version != INDEX_SCHEMA_VERSION {
        return Err(AcxError::UnsupportedFeature(format!(
            "index schema version {version}"
        )));
    }
    let count = u32_at(payload, 4) as u64;
    let expected = count * INDEX_ENTRY_LEN as u64 + INDEX_HEAD_LEN as u64; // < 2^39, no overflow
    if payload.len() as u64 != expected {
        return Err(invalid(format!(
            "index holds {count} entries, so it should be {expected} bytes, not {}",
            payload.len()
        )));
    }

    let mut entries = Vec::with_capacity(count as usize); // bounded by payload.len()
    let mut free_from = HEADER_LEN as u64;
    for (i, b) in payload[INDEX_HEAD_LEN..]
        .chunks_exact(INDEX_ENTRY_LEN)
        .enumerate()
    {
        let id = u32_at(b, 0);
        if id as usize != i {
            return Err(invalid(format!("index entry {i} has block ID {id}")));
        }
        let codec = CodecId::try_from(u16_at(b, 4))?;
        if u16_at(b, 6) != 0 {
            return Err(invalid(format!("block {id}: non-zero index flags")));
        }
        let e = IndexEntry {
            id: BlockId(id),
            codec,
            record: Span {
                offset: u64_at(b, 8),
                len: u64_at(b, 16),
            },
            original_size: u64_at(b, 24),
            compressed_size: u64_at(b, 32),
            digest: digest_at(b, 40),
        };
        if e.original_size > max_block_size || e.compressed_size > max_block_size {
            return Err(AcxError::ResourceLimit(format!(
                "block {id} is larger than the {max_block_size}-byte block limit"
            )));
        }
        if codec == CodecId::Raw && e.compressed_size != e.original_size {
            return Err(invalid(format!(
                "block {id}: RAW block with differing sizes"
            )));
        }
        if Some(e.record.len) != (BLOCK_RECORD_HEAD_LEN as u64).checked_add(e.compressed_size) {
            return Err(invalid(format!(
                "block {id}: record length does not match payload size"
            )));
        }
        if e.record.offset < free_from {
            return Err(invalid(format!("block {id} overlaps the previous record")));
        }
        free_from = e
            .record
            .end()
            .filter(|&end| end <= manifest_offset)
            .ok_or_else(|| invalid(format!("block {id} extends past the manifest")))?;
        entries.push(e);
    }
    Ok(entries)
}
