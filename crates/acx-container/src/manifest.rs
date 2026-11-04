//! Manifest payload (FORMAT_V1 §7).

use std::collections::HashSet;

use acx_core::{AcxError, BlockId};

use crate::block::IndexEntry;
use crate::le::{digest_at, invalid, u16_at, u32_at, u64_at};

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;
const MANIFEST_HEAD_LEN: usize = 8;
/// Entry fields before the path.
const ENTRY_FIXED_LEN: usize = 36;
/// Digest and reference count after the path.
const ENTRY_TAIL_LEN: usize = 36;
const REF_LEN: usize = 20;
const MIN_ENTRY_LEN: usize = ENTRY_FIXED_LEN + 1 + ENTRY_TAIL_LEN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

/// `len` bytes starting at `offset` in the decoded bytes of `block`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRef {
    pub block: BlockId,
    pub offset: u64,
    pub len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub kind: EntryKind,
    /// Relative, `/`-separated, exactly as scanned.
    pub path: String,
    /// `mode & 0o7777` where the platform has Unix modes.
    pub mode: Option<u32>,
    pub size: u64,
    /// Unix nanoseconds.
    pub mtime_ns: Option<i64>,
    /// BLAKE3 of the whole file; zero for directories.
    pub digest: [u8; 32],
    /// Concatenated in order to rebuild the file.
    pub refs: Vec<BlockRef>,
}

/// Format-level path rules (FORMAT_V1 §7). Extraction applies these again
/// plus platform rules before touching the filesystem.
pub fn validate_path(path: &str) -> Result<(), AcxError> {
    let bad = |why: &str| invalid(format!("unsafe archive path {path:?}: {why}"));
    if path.is_empty() || path.len() > u16::MAX as usize {
        return Err(bad("empty or longer than 65535 bytes"));
    }
    if path.contains('\0') {
        return Err(bad("contains NUL"));
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(bad("absolute or UNC"));
    }
    let first = path.split('/').next().unwrap_or_default().as_bytes();
    if first.len() >= 2 && first[0].is_ascii_alphabetic() && first[1] == b':' {
        return Err(bad("drive prefix"));
    }
    if path
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return Err(bad("empty, `.`, or `..` component"));
    }
    Ok(())
}

pub fn encode_manifest(entries: &[ManifestEntry]) -> Result<Vec<u8>, AcxError> {
    let too_big = |what: String| AcxError::ResourceLimit(what);
    let count = u32::try_from(entries.len())
        .map_err(|_| too_big(format!("{} manifest entries", entries.len())))?;
    let mut out = Vec::new();
    out.extend_from_slice(&MANIFEST_SCHEMA_VERSION.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    for (id, e) in entries.iter().enumerate() {
        validate_path(&e.path).map_err(|err| AcxError::InvalidArgument(err.to_string()))?;
        let entry_len = ENTRY_FIXED_LEN + e.path.len() + ENTRY_TAIL_LEN + e.refs.len() * REF_LEN;
        let entry_len = u32::try_from(entry_len)
            .map_err(|_| too_big(format!("{}: too many block references", e.path)))?;
        out.extend_from_slice(&entry_len.to_le_bytes());
        out.extend_from_slice(&(id as u32).to_le_bytes()); // id < count, which fits u32
        out.push(match e.kind {
            EntryKind::File => 1,
            EntryKind::Directory => 2,
        });
        out.push(1); // UTF-8
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&e.mode.unwrap_or(u32::MAX).to_le_bytes());
        out.extend_from_slice(&e.size.to_le_bytes());
        out.extend_from_slice(&e.mtime_ns.unwrap_or(i64::MIN).to_le_bytes());
        out.extend_from_slice(&(e.path.len() as u16).to_le_bytes()); // checked by validate_path
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(e.path.as_bytes());
        out.extend_from_slice(&e.digest);
        out.extend_from_slice(&(e.refs.len() as u32).to_le_bytes()); // < entry_len
        for r in &e.refs {
            out.extend_from_slice(&r.block.0.to_le_bytes());
            out.extend_from_slice(&r.offset.to_le_bytes());
            out.extend_from_slice(&r.len.to_le_bytes());
        }
    }
    Ok(out)
}

/// Parses a manifest payload and checks every rule of FORMAT_V1 §7 against
/// the already-validated index `blocks`.
pub fn decode_manifest(
    payload: &[u8],
    blocks: &[IndexEntry],
) -> Result<Vec<ManifestEntry>, AcxError> {
    if payload.len() < MANIFEST_HEAD_LEN {
        return Err(invalid("manifest payload shorter than its 8-byte header"));
    }
    let version = u32_at(payload, 0);
    if version != MANIFEST_SCHEMA_VERSION {
        return Err(AcxError::UnsupportedFeature(format!(
            "manifest schema version {version}"
        )));
    }
    let count = u32_at(payload, 4) as usize;
    let mut rest = &payload[MANIFEST_HEAD_LEN..];
    if count
        .checked_mul(MIN_ENTRY_LEN)
        .is_none_or(|min| min > rest.len())
    {
        return Err(invalid(format!(
            "manifest claims {count} entries but is only {} bytes",
            payload.len()
        )));
    }
    let mut entries = Vec::with_capacity(count); // bounded by the check above
    let mut seen = HashSet::with_capacity(count);
    for i in 0..count {
        if rest.len() < 4 {
            return Err(invalid(format!("manifest truncated at entry {i}")));
        }
        let len = u32_at(rest, 0) as usize;
        if len < MIN_ENTRY_LEN || len > rest.len() {
            return Err(invalid(format!("manifest entry {i} has bad length {len}")));
        }
        let (raw, tail) = rest.split_at(len);
        rest = tail;
        let entry = parse_entry(i, raw, blocks)?;
        if !seen.insert(entry.path.clone()) {
            return Err(invalid(format!("duplicate path {:?}", entry.path)));
        }
        entries.push(entry);
    }
    if !rest.is_empty() {
        return Err(invalid("trailing bytes after the last manifest entry"));
    }
    Ok(entries)
}

/// `e` is one whole entry, at least `MIN_ENTRY_LEN` bytes.
fn parse_entry(i: usize, e: &[u8], blocks: &[IndexEntry]) -> Result<ManifestEntry, AcxError> {
    let bad = |why: String| invalid(format!("manifest entry {i}: {why}"));
    if u32_at(e, 4) as usize != i {
        return Err(bad(format!("file ID {}", u32_at(e, 4))));
    }
    let kind = match e[8] {
        1 => EntryKind::File,
        2 => EntryKind::Directory,
        t => {
            return Err(AcxError::UnsupportedFeature(format!(
                "manifest entry type {t}"
            )));
        }
    };
    if e[9] != 1 {
        return Err(AcxError::UnsupportedFeature(format!(
            "path encoding {}",
            e[9]
        )));
    }
    if u16_at(e, 10) != 0 || u16_at(e, 34) != 0 {
        return Err(bad("non-zero flags or reserved bytes".into()));
    }
    let mode = match u32_at(e, 12) {
        u32::MAX => None,
        m if m <= 0o7777 => Some(m),
        m => return Err(bad(format!("mode 0o{m:o} has bits outside 0o7777"))),
    };
    let size = u64_at(e, 16);
    let mtime_ns = Some(u64_at(e, 24) as i64).filter(|&t| t != i64::MIN);

    let path_len = u16_at(e, 32) as usize;
    let tail_at = ENTRY_FIXED_LEN + path_len;
    if e.len() < tail_at + ENTRY_TAIL_LEN {
        return Err(bad("path runs past the entry".into()));
    }
    let path = std::str::from_utf8(&e[ENTRY_FIXED_LEN..tail_at])
        .map_err(|_| bad("path is not valid UTF-8".into()))?
        .to_owned();
    validate_path(&path)?;
    let digest = digest_at(e, tail_at);
    let ref_count = u32_at(e, tail_at + 32) as usize;
    let refs_bytes = &e[tail_at + ENTRY_TAIL_LEN..];
    if Some(refs_bytes.len()) != ref_count.checked_mul(REF_LEN) {
        return Err(bad(format!("{ref_count} references do not fill the entry")));
    }

    let mut refs = Vec::with_capacity(ref_count);
    let mut total: u64 = 0;
    for r in refs_bytes.chunks_exact(REF_LEN) {
        let r = BlockRef {
            block: BlockId(u32_at(r, 0)),
            offset: u64_at(r, 4),
            len: u64_at(r, 12),
        };
        let block = blocks
            .get(r.block.0 as usize)
            .ok_or_else(|| bad(format!("reference to missing block {}", r.block.0)))?;
        if r.offset
            .checked_add(r.len)
            .is_none_or(|end| end > block.original_size)
        {
            return Err(bad(format!("reference outside block {}", r.block.0)));
        }
        total = total
            .checked_add(r.len)
            .ok_or_else(|| bad("reference lengths overflow".into()))?;
        refs.push(r);
    }
    if total != size {
        return Err(bad(format!(
            "references cover {total} bytes, size is {size}"
        )));
    }
    if kind == EntryKind::Directory && (size != 0 || digest != [0; 32] || !refs.is_empty()) {
        return Err(bad("directory with content".into()));
    }
    Ok(ManifestEntry {
        kind,
        path,
        mode,
        size,
        mtime_ns,
        digest,
        refs,
    })
}
