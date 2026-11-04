//! Archive verification (PRD §2.3 `acx-archive::verify`).

use std::path::Path;

use acx_container::EntryKind;
use acx_core::{AcxError, VerifyOptions};

use crate::{decode_record, open};

#[derive(Debug, Clone, Default)]
pub struct VerifyReport {
    pub files: u64,
    pub blocks: u64,
    /// One line per damaged block or file, with its location.
    pub problems: Vec<String>,
}

impl VerifyReport {
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Quick mode checks structure, CRCs, and every block record against the
/// index. Full mode also decodes every block (size and BLAKE3) and rebuilds
/// every file's BLAKE3.
///
/// Damage to the header, footer, manifest, or index makes the whole archive
/// unreadable and is returned as `Err`. Damaged blocks are reported in
/// [`VerifyReport::problems`] so every bad location is listed, not just the
/// first.
pub fn verify_archive(archive: &Path, options: &VerifyOptions) -> Result<VerifyReport, AcxError> {
    let mut reader = open(archive)?;
    reader.check_gaps()?;
    let blocks = reader.blocks().to_vec();
    let mut report = VerifyReport {
        files: reader.entries().len() as u64,
        blocks: blocks.len() as u64,
        problems: Vec::new(),
    };

    let mut bad = vec![false; blocks.len()];
    for (i, b) in blocks.iter().enumerate() {
        let result = if options.full {
            reader
                .read_block_record(b)
                .and_then(|rec| decode_record(&rec, b))
                .map(drop)
        } else {
            reader.read_block_record(b).map(drop)
        };
        if let Err(e) = result {
            bad[i] = true;
            report.problems.push(format!(
                "block {} at offset {}: {e}",
                b.id.0, b.record.offset
            ));
        }
    }
    if !options.full {
        return Ok(report);
    }

    // shortcut: full mode decodes each block twice (block pass, then file
    // pass); merge the passes if full verification speed matters.
    for entry in reader.entries().to_vec() {
        if entry.kind == EntryKind::Directory {
            continue; // no content; the manifest decoder checked it has none
        }
        if entry.refs.iter().any(|r| bad[r.block.0 as usize]) {
            report
                .problems
                .push(format!("{}: uses a damaged block", entry.path));
            continue;
        }
        let mut hasher = blake3::Hasher::new();
        for r in &entry.refs {
            let b = &blocks[r.block.0 as usize];
            let data = decode_record(&reader.read_block_record(b)?, b)?;
            hasher.update(&data[r.offset as usize..(r.offset + r.len) as usize]);
        }
        if *hasher.finalize().as_bytes() != entry.digest {
            report
                .problems
                .push(format!("{}: file BLAKE3 mismatch", entry.path));
        }
    }
    Ok(report)
}
