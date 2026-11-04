//! ACX archive operations (PRD §2.3): scan, create, extract, verify, and
//! list. Creation and extraction run on a bounded parallel pipeline
//! (`scheduler`). Dedup follows in TASK 6.

mod create;
mod extract;
mod scanner;
mod scheduler;
mod verify;

use std::fs::File;
use std::io::{self, BufReader};
use std::path::Path;

use acx_container::ArchiveReader;
use acx_core::{AcxError, MAX_BLOCK_SIZE};

pub use acx_container::{BlockRef, EntryKind, IndexEntry, ManifestEntry};
pub use create::{ArchiveReport, accept_compressed, create_archive};
pub use extract::{ExtractReport, extract_archive, windows_name_ok};
pub use scanner::{ScanItem, ScanManifest, scan};
pub use verify::{VerifyReport, verify_archive};

/// Archive contents for `acx list`.
pub struct Listing {
    pub archive_len: u64,
    pub entries: Vec<ManifestEntry>,
    pub blocks: Vec<IndexEntry>,
}

pub fn list_archive(path: &Path) -> Result<Listing, AcxError> {
    let reader = open(path)?;
    Ok(Listing {
        archive_len: reader.footer().archive_len,
        entries: reader.entries().to_vec(),
        blocks: reader.blocks().to_vec(),
    })
}

/// The error and its sources on one line, for messages.
pub fn describe(e: &AcxError) -> String {
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        msg.push_str(&format!(": {s}"));
        source = s.source();
    }
    msg
}

fn io_err(context: impl Into<String>) -> impl FnOnce(io::Error) -> AcxError {
    let context = context.into();
    move |source| AcxError::Io { context, source }
}

fn open(path: &Path) -> Result<ArchiveReader<BufReader<File>>, AcxError> {
    let file = File::open(path).map_err(io_err(format!("opening {}", path.display())))?;
    ArchiveReader::open(BufReader::new(file), MAX_BLOCK_SIZE)
}

/// Decodes a block record from `read_block_record` and checks size and
/// BLAKE3.
fn decode_record(record: &[u8], entry: &IndexEntry) -> Result<Vec<u8>, AcxError> {
    let payload = &record[acx_container::BLOCK_RECORD_HEAD_LEN..];
    let data = acx_codecs::decompress(entry.codec, payload, entry.original_size)?;
    acx_container::check_decoded(&data, entry)?;
    Ok(data.into_owned())
}
