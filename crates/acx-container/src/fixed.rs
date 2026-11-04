//! Fixed 64-byte header and footer (FORMAT_V1 §3, §4).

use acx_core::AcxError;

use crate::le::{invalid, u16_at, u32_at, u64_at};
use crate::record::FRAME_LEN;

pub const HEADER_LEN: usize = 64;
pub const FOOTER_LEN: usize = 64;
pub const HEADER_MAGIC: [u8; 8] = *b"ACXARCH1";
pub const FOOTER_MAGIC: [u8; 8] = *b"ACXEND01";
pub const VERSION_MAJOR: u16 = 1;
pub const VERSION_MINOR: u16 = 0;

/// Both fixed structures carry a CRC32C of `[0, 56)` at offset 56.
const CRC_AT: usize = 56;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub minor_version: u16,
    /// v1 defines no optional flags; unknown bits are kept and ignored.
    pub optional_flags: u32,
}

impl Default for Header {
    fn default() -> Self {
        Self {
            minor_version: VERSION_MINOR,
            optional_flags: 0,
        }
    }
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0; HEADER_LEN];
        b[0..8].copy_from_slice(&HEADER_MAGIC);
        b[8..10].copy_from_slice(&VERSION_MAJOR.to_le_bytes());
        b[10..12].copy_from_slice(&self.minor_version.to_le_bytes());
        b[12..16].copy_from_slice(&(HEADER_LEN as u32).to_le_bytes());
        // [16, 20) required flags: none defined in v1, left zero.
        b[20..24].copy_from_slice(&self.optional_flags.to_le_bytes());
        seal(&mut b);
        b
    }

    /// Validates in the order given by FORMAT_V1 §3.
    pub fn decode(b: &[u8; HEADER_LEN]) -> Result<Self, AcxError> {
        if b[0..8] != HEADER_MAGIC {
            return Err(invalid("not an ACX archive (bad header magic)"));
        }
        let major = u16_at(b, 8);
        let minor = u16_at(b, 10);
        if major != VERSION_MAJOR {
            return Err(AcxError::UnsupportedVersion { major, minor });
        }
        check_crc(b, "header")?;
        let size = u32_at(b, 12);
        if size != HEADER_LEN as u32 {
            return Err(invalid(format!(
                "header size {size}, expected {HEADER_LEN}"
            )));
        }
        let required = u32_at(b, 16);
        if required != 0 {
            return Err(AcxError::UnsupportedFeature(format!(
                "required header flags 0x{required:08x}"
            )));
        }
        if b[24..CRC_AT].iter().chain(&b[60..64]).any(|&x| x != 0) {
            return Err(invalid("non-zero reserved bytes in header"));
        }
        Ok(Self {
            minor_version: minor,
            optional_flags: u32_at(b, 20),
        })
    }
}

/// A record's position: `offset` from the start of the archive and total
/// length including its 16-byte frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub offset: u64,
    pub len: u64,
}

impl Span {
    /// `None` on overflow.
    pub fn end(&self) -> Option<u64> {
        self.offset.checked_add(self.len)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footer {
    pub manifest: Span,
    pub index: Span,
    pub archive_len: u64,
    /// CRC32C of the manifest payload (frame excluded).
    pub manifest_crc: u32,
    /// CRC32C of the index payload (frame excluded).
    pub index_crc: u32,
}

impl Footer {
    pub fn encode(&self) -> [u8; FOOTER_LEN] {
        let mut b = [0; FOOTER_LEN];
        b[0..8].copy_from_slice(&FOOTER_MAGIC);
        b[8..16].copy_from_slice(&self.manifest.offset.to_le_bytes());
        b[16..24].copy_from_slice(&self.manifest.len.to_le_bytes());
        b[24..32].copy_from_slice(&self.index.offset.to_le_bytes());
        b[32..40].copy_from_slice(&self.index.len.to_le_bytes());
        b[40..48].copy_from_slice(&self.archive_len.to_le_bytes());
        b[48..52].copy_from_slice(&self.manifest_crc.to_le_bytes());
        b[52..56].copy_from_slice(&self.index_crc.to_le_bytes());
        seal(&mut b);
        b
    }

    /// Decodes the last 64 bytes of a file that is `file_len` bytes long and
    /// checks that manifest and index are placed as FORMAT_V1 §4 requires.
    pub fn decode(b: &[u8; FOOTER_LEN], file_len: u64) -> Result<Self, AcxError> {
        let min_len = (HEADER_LEN + FOOTER_LEN) as u64;
        if file_len < min_len {
            return Err(invalid(format!(
                "file is {file_len} bytes, shorter than the {min_len}-byte minimum"
            )));
        }
        if b[0..8] != FOOTER_MAGIC {
            return Err(invalid(
                "bad footer magic (truncated or incomplete archive)",
            ));
        }
        check_crc(b, "footer")?;
        if b[60..64] != [0; 4] {
            return Err(invalid("non-zero reserved bytes in footer"));
        }
        let f = Self {
            manifest: Span {
                offset: u64_at(b, 8),
                len: u64_at(b, 16),
            },
            index: Span {
                offset: u64_at(b, 24),
                len: u64_at(b, 32),
            },
            archive_len: u64_at(b, 40),
            manifest_crc: u32_at(b, 48),
            index_crc: u32_at(b, 52),
        };
        if f.archive_len != file_len {
            return Err(invalid(format!(
                "footer records {} bytes but the file has {file_len}",
                f.archive_len
            )));
        }
        let frame = FRAME_LEN as u64;
        if f.manifest.len < frame || f.index.len < frame {
            return Err(invalid("manifest or index shorter than a record frame"));
        }
        if f.manifest.offset < HEADER_LEN as u64 {
            return Err(invalid("manifest overlaps the header"));
        }
        if f.manifest.end() != Some(f.index.offset) {
            return Err(invalid("index does not start where the manifest ends"));
        }
        if f.index.end() != Some(file_len - FOOTER_LEN as u64) {
            return Err(invalid("index does not end where the footer starts"));
        }
        Ok(f)
    }
}

fn seal(b: &mut [u8; 64]) {
    let crc = crc32c::crc32c(&b[..CRC_AT]);
    b[CRC_AT..CRC_AT + 4].copy_from_slice(&crc.to_le_bytes());
}

fn check_crc(b: &[u8; 64], what: &str) -> Result<(), AcxError> {
    let stored = u32_at(b, CRC_AT);
    let actual = crc32c::crc32c(&b[..CRC_AT]);
    if stored != actual {
        return Err(AcxError::Integrity(format!(
            "{what} CRC32C mismatch: stored 0x{stored:08x}, computed 0x{actual:08x}"
        )));
    }
    Ok(())
}
