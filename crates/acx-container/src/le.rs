//! Little-endian field readers. Callers check the slice length first, so the
//! `expect`s never fire on archive input.

use acx_core::AcxError;

pub(crate) fn invalid(msg: impl Into<String>) -> AcxError {
    AcxError::InvalidArchive(msg.into())
}

pub(crate) fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().expect("length checked by caller"))
}

pub(crate) fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("length checked by caller"))
}

pub(crate) fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("length checked by caller"))
}

pub(crate) fn digest_at(b: &[u8], at: usize) -> [u8; 32] {
    b[at..at + 32].try_into().expect("length checked by caller")
}
