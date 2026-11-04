//! Record frames (FORMAT_V1 §5).

use acx_core::AcxError;

use crate::le::{invalid, u16_at, u64_at};

pub const FRAME_LEN: usize = 16;
/// Record flag bit 0: a reader that does not know the type may skip it.
pub const FLAG_OPTIONAL: u16 = 1;

/// Record types this reader understands. Type `0x0004` (metadata) is
/// reserved and unused in v1, so it is treated like any unknown type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordType {
    Manifest,
    Block,
    Index,
}

impl RecordType {
    pub const fn code(self) -> u16 {
        match self {
            Self::Manifest => 0x0001,
            Self::Block => 0x0002,
            Self::Index => 0x0003,
        }
    }

    pub const fn magic(self) -> [u8; 4] {
        match self {
            Self::Manifest => *b"ACXM",
            Self::Block => *b"ACXB",
            Self::Index => *b"ACXI",
        }
    }

    fn from_code(code: u16) -> Option<Self> {
        [Self::Manifest, Self::Block, Self::Index]
            .into_iter()
            .find(|t| t.code() == code)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    /// `None` for an unknown type, which `decode` only allows when `optional`.
    pub record_type: Option<RecordType>,
    pub optional: bool,
    pub payload_len: u64,
}

impl Frame {
    /// v1 writes only required records, so the flags are always zero.
    pub fn encode(record_type: RecordType, payload_len: u64) -> [u8; FRAME_LEN] {
        let mut b = [0; FRAME_LEN];
        b[0..4].copy_from_slice(&record_type.magic());
        b[4..6].copy_from_slice(&record_type.code().to_le_bytes());
        b[8..16].copy_from_slice(&payload_len.to_le_bytes());
        b
    }

    /// Accepts known types with matching magic, and unknown types only when
    /// flagged optional (the caller then skips `payload_len` bytes).
    pub fn decode(b: &[u8; FRAME_LEN]) -> Result<Self, AcxError> {
        let code = u16_at(b, 4);
        let flags = u16_at(b, 6);
        if flags & !FLAG_OPTIONAL != 0 {
            return Err(invalid(format!("reserved record flags 0x{flags:04x}")));
        }
        let optional = flags & FLAG_OPTIONAL != 0;
        let record_type = RecordType::from_code(code);
        match record_type {
            Some(t) if b[0..4] != t.magic() => {
                return Err(invalid(format!(
                    "{t:?} record has magic {:?}",
                    String::from_utf8_lossy(&b[0..4])
                )));
            }
            None if !optional => {
                return Err(AcxError::UnsupportedFeature(format!(
                    "required record type 0x{code:04x}"
                )));
            }
            _ => {}
        }
        Ok(Self {
            record_type,
            optional,
            payload_len: u64_at(b, 8),
        })
    }

    /// Decodes a frame that must be a required record of `expected` type and
    /// exactly `total_len` bytes long, as given by the footer or index.
    pub fn expect(
        b: &[u8; FRAME_LEN],
        expected: RecordType,
        total_len: u64,
    ) -> Result<Self, AcxError> {
        let f = Self::decode(b)?;
        if f.record_type != Some(expected) || f.optional {
            return Err(invalid(format!(
                "expected a required {expected:?} record, found type {:?} (optional: {})",
                f.record_type, f.optional
            )));
        }
        if (FRAME_LEN as u64).checked_add(f.payload_len) != Some(total_len) {
            return Err(invalid(format!(
                "{expected:?} record frame gives payload {} bytes, but the record is {total_len} bytes",
                f.payload_len
            )));
        }
        Ok(f)
    }
}
