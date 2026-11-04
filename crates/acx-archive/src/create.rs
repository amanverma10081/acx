//! Archive creation (PRD §2.3 `acx-archive::create`).

use std::collections::HashMap;
use std::fs::{self, File, Metadata};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use acx_container::{
    ArchiveWriter, BLOCK_RECORD_HEAD_LEN, BlockRef, EntryKind, ManifestEntry, read_block_record,
};
use acx_core::{
    AcxError, ArchiveOptions, BlockId, CodecChoice, CodecId, DedupMode, ErrorPolicy, ProgressEvent,
};

use crate::scanner::{ScanItem, scan};
use crate::{describe, io_err, scheduler};

#[derive(Debug, Clone, Default)]
pub struct ArchiveReport {
    pub files: u64,
    pub directories: u64,
    pub input_bytes: u64,
    pub archive_bytes: u64,
    pub blocks: u64,
    /// Blocks stored with Zstandard rather than RAW.
    pub compressed_blocks: u64,
    /// Chunks not stored because an identical block already was.
    pub duplicate_blocks: u64,
    /// Input bytes those duplicate chunks held.
    pub duplicate_bytes: u64,
    /// Inputs left out or changed, allowed by the skip policy.
    pub warnings: Vec<String>,
}

/// PRD §3.2: keep a compressed block only if it saves at least 32 bytes or
/// 1% of the original, whichever is larger.
pub fn accept_compressed(original: u64, compressed: u64) -> bool {
    let min_saving = (original / 100).max(32);
    compressed
        .checked_add(min_saving)
        .is_some_and(|needed| needed <= original)
}

/// Archives files and directory trees into `output`.
///
/// Writes to a temporary file next to `output` and renames it into place, so
/// a failed or interrupted run never leaves a partial archive at `output`
/// and never damages an existing one.
pub fn create_archive(
    inputs: &[PathBuf],
    output: &Path,
    options: &ArchiveOptions,
    progress: impl Fn(ProgressEvent) + Send + Sync,
) -> Result<ArchiveReport, AcxError> {
    options.budget.validate()?;
    if inputs.is_empty() {
        return Err(AcxError::InvalidArgument("no inputs given".into()));
    }
    if !options.overwrite && output.exists() {
        return Err(AcxError::InvalidArgument(format!(
            "{} already exists (use --overwrite)",
            output.display()
        )));
    }
    let mut scanned = scan(inputs, options.error_policy)?;
    drop_existing_output(&mut scanned.items, output);
    let mut report = ArchiveReport {
        warnings: scanned.warnings,
        ..ArchiveReport::default()
    };
    progress(ProgressEvent::Scanned {
        files: scanned.items.len() as u64,
        bytes: scanned.items.iter().map(|i| i.metadata.len()).sum(),
    });

    let dir = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    // A `\\?\` path on Windows, so long output paths survive tempfile's rename.
    let dir = &fs::canonicalize(dir).map_err(io_err(format!("resolving {}", dir.display())))?;
    let output = &dir.join(output.file_name().ok_or_else(|| {
        AcxError::InvalidArgument(format!("{} is not a file path", output.display()))
    })?);
    let tmp = tempfile::Builder::new()
        .prefix(".acx-")
        .suffix(".tmp")
        .tempfile_in(dir)
        .map_err(io_err(format!(
            "creating a temporary file in {}",
            dir.display()
        )))?;
    // A second handle on the archive, to read blocks back for --verify-dedup.
    let mut reread = options
        .verify_dedup
        .then(|| tmp.reopen())
        .transpose()
        .map_err(io_err("reopening the temporary archive"))?;
    let mut writer = ArchiveWriter::new(BufWriter::new(tmp))?;
    // First block stored for each (decoded length, BLAKE3). Filled by the
    // single writer in chunk order, so Block IDs do not depend on threads.
    let mut stored: HashMap<(u64, [u8; 32]), BlockId> = HashMap::new();
    let (mut duplicates, mut duplicate_bytes) = (0u64, 0u64);
    let mut entries = Vec::with_capacity(scanned.items.len());
    // Until the pipeline finishes, refs hold chunk numbers; this maps each
    // chunk number to the block it became.
    let mut block_ids: Vec<BlockId> = Vec::new();
    let (mut blocks, mut compressed) = (0u64, 0u64);
    let budget = &options.budget;
    scheduler::run_ordered(
        budget.effective_threads(),
        scheduler::window(budget),
        &options.cancel,
        |submit| {
            let mut chunks = 0u32;
            for item in &scanned.items {
                let entry = match item.kind {
                    EntryKind::Directory => {
                        report.directories += 1;
                        ManifestEntry {
                            kind: EntryKind::Directory,
                            path: item.path.clone(),
                            mode: unix_mode(&item.metadata),
                            size: 0,
                            mtime_ns: mtime_ns(&item.metadata),
                            digest: [0; 32],
                            refs: Vec::new(),
                        }
                    }
                    EntryKind::File => {
                        match read_file(item, options, submit, &mut chunks, &mut report, &progress)?
                        {
                            Ok(entry) => {
                                report.files += 1;
                                report.input_bytes += entry.size;
                                entry
                            }
                            Err(e) if options.error_policy == ErrorPolicy::Skip => {
                                // shortcut: blocks already sent for a file that
                                // fails mid-read stay in the archive unreferenced.
                                report.warnings.push(describe(&e));
                                continue;
                            }
                            Err(e) => return Err(e),
                        }
                    }
                };
                entries.push(entry);
            }
            Ok(())
        },
        |chunk| encode(chunk, options),
        // shortcut: duplicates are still compressed by a worker before the
        // writer drops them; hash on the reading thread first if that CPU
        // time shows up in duplicate-heavy benchmarks.
        |block: Encoded| {
            let key = (block.original_size, block.digest);
            if options.dedup == DedupMode::Blocks {
                if let Some(&id) = stored.get(&key) {
                    let same = match reread.as_mut() {
                        Some(file) => stored_block_matches(&mut writer, file, id, &block)?,
                        None => true,
                    };
                    if same {
                        block_ids.push(id);
                        duplicates += 1;
                        duplicate_bytes += block.original_size;
                        return Ok(());
                    }
                }
            }
            let id = writer.write_block(
                block.codec,
                block.original_size,
                block.digest,
                &block.payload,
            )?;
            stored.entry(key).or_insert(id);
            block_ids.push(id);
            blocks += 1;
            compressed += u64::from(block.codec == CodecId::Zstd);
            Ok(())
        },
    )?;
    report.blocks = blocks;
    report.compressed_blocks = compressed;
    report.duplicate_blocks = duplicates;
    report.duplicate_bytes = duplicate_bytes;
    for r in entries.iter_mut().flat_map(|e| &mut e.refs) {
        r.block = block_ids[r.block.0 as usize];
    }

    let tmp = writer
        .finish(&entries)?
        .into_inner()
        .map_err(|e| io_err("flushing archive")(e.into_error()))?;
    tmp.as_file()
        .sync_all()
        .map_err(io_err("syncing archive"))?;
    let persisted = if options.overwrite {
        tmp.persist(output)
    } else {
        tmp.persist_noclobber(output)
    };
    persisted.map_err(|e| io_err(format!("moving archive to {}", output.display()))(e.error))?;

    report.archive_bytes = fs::metadata(output)
        .map_err(io_err(format!("reading {}", output.display())))?
        .len();
    progress(ProgressEvent::Finished);
    Ok(report)
}

/// With `--overwrite` and the output inside an input directory, the old
/// archive would otherwise be archived into the new one.
fn drop_existing_output(items: &mut Vec<ScanItem>, output: &Path) {
    let Ok(out) = fs::canonicalize(output) else {
        return;
    };
    items.retain(|i| {
        i.kind != EntryKind::File
            || i.source.file_name() != output.file_name()
            || !fs::canonicalize(&i.source).is_ok_and(|p| p == out)
    });
}

/// Reads one file once, hashing it and submitting it as block-sized chunks
/// to the pipeline. The returned entry's refs hold chunk numbers.
///
/// The outer error is fatal (the pipeline stopped). The inner error is a
/// problem with this input only, which the skip policy may pass over.
fn read_file(
    item: &ScanItem,
    options: &ArchiveOptions,
    submit: &mut dyn FnMut(Vec<u8>) -> Result<(), AcxError>,
    chunks: &mut u32,
    report: &mut ArchiveReport,
    progress: &impl Fn(ProgressEvent),
) -> Result<Result<ManifestEntry, AcxError>, AcxError> {
    let read_err = || io_err(format!("reading {}", item.source.display()));
    let mut file = match File::open(&item.source) {
        Ok(f) => f,
        Err(e) => return Ok(Err(read_err()(e))),
    };
    // validate() capped block_size at MAX_BLOCK_SIZE.
    let block_size = options.budget.block_size as usize;
    let mut file_hash = blake3::Hasher::new();
    let mut refs = Vec::new();
    let mut size = 0u64;
    loop {
        // Fresh buffer per chunk, because it moves into the pipeline. Zeroed
        // pages that are never written are not committed.
        let mut chunk = vec![0; block_size];
        let n = match read_full(&mut file, &mut chunk) {
            Ok(n) => n,
            Err(e) => return Ok(Err(read_err()(e))),
        };
        if n == 0 {
            break;
        }
        chunk.truncate(n);
        chunk.shrink_to_fit();
        // The file hash comes from the same bytes that get stored.
        file_hash.update(&chunk);
        refs.push(BlockRef {
            block: BlockId(*chunks),
            offset: 0,
            len: n as u64,
        });
        *chunks = chunks
            .checked_add(1)
            .ok_or_else(|| AcxError::ResourceLimit("more than u32::MAX blocks".into()))?;
        submit(chunk)?;
        size += n as u64;
        progress(ProgressEvent::BytesProcessed { bytes: n as u64 });
        if n < block_size {
            break;
        }
    }
    if size != item.metadata.len() {
        let msg = format!(
            "{} changed while being archived ({} bytes expected, {size} read)",
            item.source.display(),
            item.metadata.len()
        );
        match options.error_policy {
            ErrorPolicy::Strict => {
                return Ok(Err(AcxError::Io {
                    context: msg,
                    source: io::Error::other("file changed during archiving"),
                }));
            }
            ErrorPolicy::Skip => report.warnings.push(msg),
        }
    }
    Ok(Ok(ManifestEntry {
        kind: EntryKind::File,
        path: item.path.clone(),
        mode: unix_mode(&item.metadata),
        size,
        mtime_ns: mtime_ns(&item.metadata),
        digest: *file_hash.finalize().as_bytes(),
        refs,
    }))
}

/// `--verify-dedup`: reads the earlier block back and compares stored bytes.
/// Equal codec and payload prove equal content, since decoding is a function
/// of the payload. A mismatch (a BLAKE3 collision) stores the chunk anew.
fn stored_block_matches<W: Write>(
    writer: &mut ArchiveWriter<W>,
    file: &mut File,
    id: BlockId,
    block: &Encoded,
) -> Result<bool, AcxError> {
    writer.flush()?;
    let entry = writer.blocks()[id.0 as usize];
    let record = read_block_record(file, &entry)?;
    Ok(entry.codec == block.codec && record[BLOCK_RECORD_HEAD_LEN..] == block.payload[..])
}

/// One chunk, hashed and encoded, ready for the writer.
struct Encoded {
    codec: CodecId,
    original_size: u64,
    digest: [u8; 32],
    payload: Vec<u8>,
}

/// Runs on a worker thread: the CPU-heavy part of storing a chunk.
fn encode(chunk: Vec<u8>, options: &ArchiveOptions) -> Result<Encoded, AcxError> {
    let digest = *blake3::hash(&chunk).as_bytes();
    let original_size = chunk.len() as u64;
    let (codec, payload) = match options.codec {
        CodecChoice::Raw => (CodecId::Raw, chunk),
        // shortcut: Auto always tries Zstandard; M4 adds sampling to skip the
        // trial on incompressible data, which is what will set Auto apart.
        CodecChoice::Auto | CodecChoice::Zstd => {
            let z = acx_codecs::compress(CodecId::Zstd, &chunk, options.mode.zstd_level())?;
            if accept_compressed(original_size, z.len() as u64) {
                (CodecId::Zstd, z.into_owned())
            } else {
                (CodecId::Raw, chunk)
            }
        }
    };
    Ok(Encoded {
        codec,
        original_size,
        digest,
        payload,
    })
}

/// Fills `buf` unless the reader hits end of file first.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

fn mtime_ns(meta: &Metadata) -> Option<i64> {
    let t = meta.modified().ok()?;
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).ok(),
        Err(before) => i64::try_from(before.duration().as_nanos()).ok().map(|n| -n),
    }
}

#[cfg(unix)]
fn unix_mode(meta: &Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn unix_mode(_: &Metadata) -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acceptance_rule() {
        // 32-byte floor for small blocks.
        assert!(accept_compressed(100, 68));
        assert!(!accept_compressed(100, 69));
        // 1% for large blocks.
        assert!(accept_compressed(10_000, 9_900));
        assert!(!accept_compressed(10_000, 9_901));
        // Blocks below the floor are always RAW; no underflow or overflow.
        assert!(!accept_compressed(0, 0));
        assert!(!accept_compressed(31, 0));
        assert!(!accept_compressed(u64::MAX, u64::MAX));
    }

    #[test]
    fn read_full_fills_across_short_reads() {
        let data: Vec<u8> = (0..=255).collect();
        let mut r = io::Read::chain(&data[..100], &data[100..]);
        let mut buf = [0; 150];
        assert_eq!(read_full(&mut r, &mut buf).unwrap(), 150);
        assert_eq!(read_full(&mut r, &mut buf).unwrap(), 106);
        assert_eq!(read_full(&mut r, &mut buf).unwrap(), 0);
    }
}
