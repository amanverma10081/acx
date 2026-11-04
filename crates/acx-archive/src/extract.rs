//! Safe extraction (PRD §2.3 `acx-archive::extract`).

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use acx_container::{EntryKind, IndexEntry, ManifestEntry, read_block_record, validate_path};
use acx_core::{
    AcxError, ExtractOptions, MAX_BLOCK_SIZE, MIN_BLOCK_SIZE, OverwritePolicy, ProgressEvent,
    ResourceBudget,
};
use filetime::FileTime;
use tempfile::NamedTempFile;

use crate::{decode_record, io_err, open, scheduler};

#[derive(Debug, Clone, Default)]
pub struct ExtractReport {
    pub files: u64,
    pub directories: u64,
    pub bytes: u64,
    /// Paths left alone under `OverwritePolicy::SkipExisting`.
    pub skipped: Vec<String>,
}

/// Extracts every entry of `archive` under `out_dir`.
///
/// All paths, the total output size, and name collisions are checked before
/// anything is written. Each file is written to a temporary file, checked
/// against its BLAKE3, and only then renamed into place, so a corrupt block
/// never leaves a partial or wrong file behind.
pub fn extract_archive(
    archive: &Path,
    out_dir: &Path,
    options: &ExtractOptions,
    progress: impl Fn(ProgressEvent) + Send + Sync,
) -> Result<ExtractReport, AcxError> {
    let reader = open(archive)?;
    let entries = reader.entries().to_vec();
    let blocks = reader.blocks().to_vec();
    drop(reader);

    let total = entries
        .iter()
        .try_fold(0u64, |sum, e| sum.checked_add(e.size))
        .filter(|&t| t <= options.max_output_bytes)
        .ok_or_else(|| {
            AcxError::ResourceLimit(format!(
                "archive would write more than the {}-byte output limit",
                options.max_output_bytes
            ))
        })?;
    let targets = entries
        .iter()
        .map(|e| safe_relative_path(&e.path))
        .collect::<Result<Vec<_>, _>>()?;
    if cfg!(any(windows, target_os = "macos")) {
        check_case_collisions(&entries)?;
    }
    progress(ProgressEvent::Scanned {
        files: entries.len() as u64,
        bytes: total,
    });

    fs::create_dir_all(out_dir).map_err(io_err(format!("creating {}", out_dir.display())))?;
    // On Windows this yields a `\\?\` path, so every path joined below works
    // past 260 characters, including in tempfile's rename.
    let out_dir =
        &fs::canonicalize(out_dir).map_err(io_err(format!("resolving {}", out_dir.display())))?;
    let full_paths: Vec<PathBuf> = targets.iter().map(|rel| out_dir.join(rel)).collect();

    // Size the worker pool for the largest block this archive holds.
    let largest = blocks.iter().map(|b| b.original_size).max().unwrap_or(0);
    let budget = ResourceBudget {
        block_size: largest.clamp(MIN_BLOCK_SIZE, MAX_BLOCK_SIZE),
        ..options.budget
    };
    budget.validate()?;

    let mut report = ExtractReport::default();
    let mut dirs = Vec::new();
    let mut open_file: Option<OpenFile> = None;
    let (mut files, mut bytes) = (0u64, 0u64);
    let mut finish = |f: OpenFile| -> Result<(), AcxError> {
        let index = f.index;
        f.finish(&entries[index], &full_paths[index], options.overwrite)?;
        let entry = &entries[index];
        files += 1;
        bytes += entry.size;
        progress(ProgressEvent::BytesProcessed { bytes: entry.size });
        Ok(())
    };
    scheduler::run_ordered(
        budget.effective_threads(),
        scheduler::window(&budget),
        &options.cancel,
        |submit| {
            for (index, entry) in entries.iter().enumerate() {
                prepare_parents(out_dir, &targets[index])?;
                let target = &full_paths[index];
                match entry.kind {
                    EntryKind::Directory => {
                        let created = ensure_dir(target)?;
                        if created || options.overwrite == OverwritePolicy::Overwrite {
                            dirs.push((target, entry));
                        }
                        report.directories += 1;
                    }
                    EntryKind::File => {
                        if !may_write(target, options.overwrite)? {
                            report.skipped.push(entry.path.clone());
                            continue;
                        }
                        submit(Job::Start(index))?;
                        for r in &entry.refs {
                            submit(Job::Part {
                                // In range: the manifest was validated against the index.
                                block: blocks[r.block.0 as usize],
                                offset: r.offset,
                                len: r.len,
                            })?;
                        }
                    }
                }
            }
            Ok(())
        },
        |job| match job {
            Job::Start(index) => Ok(Done::Start(index)),
            Job::Part { block, offset, len } => {
                // shortcut: one archive open per block; cache a handle per
                // worker if archives of many tiny files extract slowly.
                let mut file = File::open(archive)
                    .map_err(io_err(format!("opening {}", archive.display())))?;
                let mut data = decode_record(&read_block_record(&mut file, &block)?, &block)?;
                if offset != 0 || len != data.len() as u64 {
                    data = data[offset as usize..(offset + len) as usize].to_vec();
                }
                Ok(Done::Part(data))
            }
        },
        |done| match done {
            Done::Start(index) => {
                if let Some(f) = open_file.take() {
                    finish(f)?;
                }
                open_file = Some(OpenFile::create(index, &full_paths[index])?);
                Ok(())
            }
            Done::Part(data) => open_file
                .as_mut()
                .expect("every part follows its file's start")
                .write(&data, &full_paths),
        },
    )?;
    if let Some(f) = open_file.take() {
        finish(f)?;
    }
    report.files = files;
    report.bytes = bytes;

    // Last, children before parents: writing into a directory changes its
    // mtime, and a read-only mode would block writing its contents.
    for (dir, entry) in dirs.iter().rev() {
        set_mtime(dir, entry.mtime_ns)?;
        set_mode(dir, entry.mode)?;
    }
    progress(ProgressEvent::Finished);
    Ok(report)
}

/// Work items for the extraction pipeline, in manifest order.
enum Job {
    /// The following parts belong to entry `.0`.
    Start(usize),
    Part {
        block: IndexEntry,
        offset: u64,
        len: u64,
    },
}

enum Done {
    Start(usize),
    Part(Vec<u8>),
}

/// Re-checks an archive path independently of manifest parsing and turns it
/// into a relative OS path.
fn safe_relative_path(path: &str) -> Result<PathBuf, AcxError> {
    validate_path(path)?;
    if cfg!(windows) && !path.split('/').all(windows_name_ok) {
        return Err(AcxError::InvalidArchive(format!(
            "{path:?} cannot be created safely on Windows"
        )));
    }
    Ok(path.split('/').collect())
}

/// Whether one path component is a safe, ordinary Windows file name.
pub fn windows_name_ok(name: &str) -> bool {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let bad_char = |c: char| c < ' ' || "<>:\"/\\|?*".contains(c);
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    !name.is_empty()
        && !name.contains(bad_char)
        && !name.ends_with(['.', ' '])
        && !RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem))
}

/// On case-insensitive file systems two such paths would overwrite each other.
fn check_case_collisions(entries: &[ManifestEntry]) -> Result<(), AcxError> {
    let mut seen = HashSet::with_capacity(entries.len());
    for e in entries {
        // shortcut: to_lowercase approximates Unicode simple case folding;
        // switch to a case-folding crate if exotic scripts collide.
        if !seen.insert(e.path.to_lowercase()) {
            return Err(AcxError::InvalidArchive(format!(
                "{:?} differs from another path only by case",
                e.path
            )));
        }
    }
    Ok(())
}

/// Creates the parent directories of `rel` under `root`, refusing to pass
/// through anything that is not a plain directory (such as a symlink or
/// junction planted in the destination). Returns the full target path.
fn prepare_parents(root: &Path, rel: &Path) -> Result<PathBuf, AcxError> {
    let mut dir = root.to_path_buf();
    if let Some(parent) = rel.parent() {
        for part in parent.components() {
            dir.push(part);
            ensure_dir(&dir)?;
        }
    }
    Ok(root.join(rel))
}

/// Returns whether the directory was created (rather than already present).
fn ensure_dir(dir: &Path) -> Result<bool, AcxError> {
    match fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => Ok(false), // symlink_metadata: a symlink is never is_dir
        Ok(_) => Err(AcxError::Io {
            context: format!("{} exists and is not a plain directory", dir.display()),
            source: io::Error::from(io::ErrorKind::AlreadyExists),
        }),
        Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(dir)
            .map(|()| true)
            .map_err(io_err(format!("creating {}", dir.display()))),
        Err(e) => Err(io_err(format!("checking {}", dir.display()))(e)),
    }
}

/// Applies the overwrite policy to an existing target. Returns `false` if
/// the file should be skipped.
fn may_write(target: &Path, policy: OverwritePolicy) -> Result<bool, AcxError> {
    match fs::symlink_metadata(target) {
        Ok(m) => match policy {
            OverwritePolicy::SkipExisting => Ok(false),
            OverwritePolicy::Error => Err(AcxError::Io {
                context: format!(
                    "{} exists (use --overwrite or --skip-existing)",
                    target.display()
                ),
                source: io::Error::from(io::ErrorKind::AlreadyExists),
            }),
            OverwritePolicy::Overwrite if m.is_dir() => Err(AcxError::Io {
                context: format!("{} is a directory", target.display()),
                source: io::Error::from(io::ErrorKind::AlreadyExists),
            }),
            OverwritePolicy::Overwrite => Ok(true),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(io_err(format!("checking {}", target.display()))(e)),
    }
}

/// A file being restored into a temporary file next to its target.
struct OpenFile {
    index: usize,
    out: BufWriter<NamedTempFile>,
    hasher: blake3::Hasher,
}

impl OpenFile {
    fn create(index: usize, target: &Path) -> Result<Self, AcxError> {
        let dir = target.parent().unwrap_or(Path::new("."));
        let tmp = tempfile::Builder::new()
            .prefix(".acx-")
            .suffix(".tmp")
            .tempfile_in(dir)
            .map_err(io_err(format!(
                "creating a temporary file in {}",
                dir.display()
            )))?;
        Ok(Self {
            index,
            out: BufWriter::new(tmp),
            hasher: blake3::Hasher::new(),
        })
    }

    fn write(&mut self, data: &[u8], full_paths: &[PathBuf]) -> Result<(), AcxError> {
        self.hasher.update(data);
        self.out.write_all(data).map_err(io_err(format!(
            "writing {}",
            full_paths[self.index].display()
        )))
    }

    /// Checks the BLAKE3, restores metadata, and renames into place. A
    /// mismatch leaves nothing behind: the temporary file is deleted on drop.
    fn finish(
        self,
        entry: &ManifestEntry,
        target: &Path,
        policy: OverwritePolicy,
    ) -> Result<(), AcxError> {
        if *self.hasher.finalize().as_bytes() != entry.digest {
            return Err(AcxError::Integrity(format!(
                "{}: restored bytes do not match the stored BLAKE3",
                entry.path
            )));
        }
        let tmp = self
            .out
            .into_inner()
            .map_err(|e| io_err(format!("writing {}", target.display()))(e.into_error()))?;
        set_mtime(tmp.path(), entry.mtime_ns)?;
        set_mode(tmp.path(), entry.mode)?;
        tmp.as_file()
            .sync_all()
            .map_err(io_err(format!("syncing {}", target.display())))?;
        let persisted = match policy {
            OverwritePolicy::Overwrite => tmp.persist(target),
            // Fails rather than replacing a file created since `may_write`.
            _ => tmp.persist_noclobber(target),
        };
        persisted.map_err(|e| io_err(format!("moving into {}", target.display()))(e.error))?;
        Ok(())
    }
}

fn set_mtime(path: &Path, mtime_ns: Option<i64>) -> Result<(), AcxError> {
    let Some(ns) = mtime_ns else { return Ok(()) };
    let t = FileTime::from_unix_time(
        ns.div_euclid(1_000_000_000),
        ns.rem_euclid(1_000_000_000) as u32,
    );
    filetime::set_file_mtime(path, t)
        .map_err(io_err(format!("setting mtime of {}", path.display())))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: Option<u32>) -> Result<(), AcxError> {
    use std::os::unix::fs::PermissionsExt;
    match mode {
        Some(m) => fs::set_permissions(path, fs::Permissions::from_mode(m))
            .map_err(io_err(format!("setting mode of {}", path.display()))),
        None => Ok(()),
    }
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: Option<u32>) -> Result<(), AcxError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_names() {
        for ok in ["a", "file.txt", "COM10", "CONSOLE", ".hidden", "日本"] {
            assert!(windows_name_ok(ok), "{ok}");
        }
        for bad in [
            "",
            "CON",
            "con",
            "Con.txt",
            "con.txt.bak",
            "NUL.tar.gz",
            "com1",
            "LPT9.x",
            "aux ",
            "a.",
            "a ",
            "a:b",
            "a\\b",
            "a*b",
            "a?b",
            "a|b",
            "a<b",
            "a>b",
            "a\"b",
            "a\u{1}b",
        ] {
            assert!(!windows_name_ok(bad), "{bad:?}");
        }
    }

    #[test]
    fn safe_paths_reject_traversal() {
        for bad in [
            "../x",
            "a/../../x",
            "/etc/passwd",
            "C:/x",
            "c:x",
            "\\\\server\\x",
            "a//b",
            "./a",
            "",
        ] {
            assert!(safe_relative_path(bad).is_err(), "{bad:?}");
        }
        assert_eq!(
            safe_relative_path("a/b/c.txt").unwrap(),
            Path::new("a").join("b").join("c.txt")
        );
    }
}
