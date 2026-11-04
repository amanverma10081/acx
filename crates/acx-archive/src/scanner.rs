//! Input scanning (PRD §2.3 `acx-archive::scanner`).

use std::collections::HashSet;
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};

use acx_container::{EntryKind, validate_path};
use acx_core::{AcxError, ErrorPolicy};
use walkdir::WalkDir;

use crate::{describe, io_err};

#[derive(Debug)]
pub struct ScanItem {
    pub source: PathBuf,
    /// Archive path: the root's own name, then the components below it as
    /// the OS reported them, joined with `/`. Never normalized.
    pub path: String,
    pub kind: EntryKind,
    /// Of the entry itself; symlinks are never followed.
    pub metadata: Metadata,
}

#[derive(Debug, Default)]
pub struct ScanManifest {
    /// Parents before children, siblings sorted by name.
    pub items: Vec<ScanItem>,
    /// Inputs left out under `ErrorPolicy::Skip`.
    pub warnings: Vec<String>,
}

/// Walks every root. Each root is stored under its own name, so
/// `photos/2024/a.jpg` is archived as `photos/2024/a.jpg` however `photos`
/// was spelled on the command line.
///
/// Unreadable entries, symlinks, special files, and names that are not
/// valid Unicode fail the scan under `Strict` and become warnings under
/// `Skip`. Two inputs with the same archive path always fail: dropping one
/// would lose data.
pub fn scan(roots: &[PathBuf], policy: ErrorPolicy) -> Result<ScanManifest, AcxError> {
    let mut out = ScanManifest::default();
    let mut seen = HashSet::new();
    let problem = |out: &mut ScanManifest, err: AcxError| match policy {
        ErrorPolicy::Strict => Err(err),
        ErrorPolicy::Skip => {
            out.warnings.push(describe(&err));
            Ok(())
        }
    };

    for root in roots {
        let name = root_name(root)?;
        let mut walk = WalkDir::new(root)
            .follow_links(false)
            .follow_root_links(false)
            .sort_by_file_name()
            .into_iter();
        while let Some(next) = walk.next() {
            let entry = match next {
                Ok(e) => e,
                Err(e) => {
                    let context = format!("reading {}", e.path().unwrap_or(root).display());
                    problem(&mut out, io_err(context)(io::Error::from(e)))?;
                    continue;
                }
            };
            let unsupported =
                |why: &str| AcxError::InvalidArgument(format!("{}: {why}", entry.path().display()));
            let ft = entry.file_type();
            let kind = if ft.is_dir() {
                EntryKind::Directory
            } else if ft.is_file() {
                EntryKind::File
            } else {
                // Not followed, so there is no subtree to skip.
                problem(
                    &mut out,
                    unsupported("symlinks and special files are not supported"),
                )?;
                continue;
            };

            let rel = entry.path().strip_prefix(root).unwrap_or(Path::new(""));
            let path = rel.components().try_fold(name.clone(), |mut p, c| {
                p.push('/');
                p.push_str(c.as_os_str().to_str()?);
                Some(p)
            });
            let Some(path) = path.filter(|p| validate_path(p).is_ok()) else {
                problem(
                    &mut out,
                    unsupported("name is not valid Unicode or not a safe archive path"),
                )?;
                if kind == EntryKind::Directory {
                    walk.skip_current_dir();
                }
                continue;
            };
            if !seen.insert(path.clone()) {
                return Err(AcxError::InvalidArgument(format!(
                    "two inputs map to the same archive path {path:?}"
                )));
            }
            // Asked of the entry itself: on Windows, walkdir's metadata comes
            // from the parent's directory listing, which NTFS updates lazily,
            // so sizes and times of just-written entries can be stale.
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(m) => m,
                Err(e) => {
                    let context = format!("reading metadata of {}", entry.path().display());
                    problem(&mut out, io_err(context)(e))?;
                    continue;
                }
            };
            out.items.push(ScanItem {
                source: entry.into_path(),
                path,
                kind,
                metadata,
            });
        }
    }
    Ok(out)
}

/// The name a root is stored under. `.` and `dir/..` take the name of the
/// directory they resolve to; a filesystem root has no name and is refused.
fn root_name(root: &Path) -> Result<String, AcxError> {
    let name = match root.file_name() {
        Some(n) => n.to_owned(),
        None => fs::canonicalize(root)
            .map_err(io_err(format!("resolving {}", root.display())))?
            .file_name()
            .ok_or_else(|| {
                AcxError::InvalidArgument(format!(
                    "{} is a filesystem root; name a directory inside it",
                    root.display()
                ))
            })?
            .to_owned(),
    };
    name.into_string().map_err(|_| {
        AcxError::InvalidArgument(format!("{}: name is not valid Unicode", root.display()))
    })
}
