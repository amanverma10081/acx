//! Shared ACX types: identifiers, options, resource limits, errors, and
//! progress events.
//!
//! No filesystem traversal, codec implementation, or archive parsing lives
//! here (PRD §2.3).

use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub const KIB: u64 = 1024;
pub const MIB: u64 = 1024 * KIB;
pub const GIB: u64 = 1024 * MIB;
pub const TIB: u64 = 1024 * GIB;

pub const DEFAULT_BLOCK_SIZE: u64 = 16 * MIB;
pub const MIN_BLOCK_SIZE: u64 = 64 * KIB;
pub const MAX_BLOCK_SIZE: u64 = 256 * MIB;
pub const DEFAULT_MEMORY: u64 = GIB;
pub const DEFAULT_MAX_OUTPUT: u64 = TIB;

/// Buffers each worker holds at once: input block, codec output, and one in
/// flight to the writer.
pub const BUFFERS_PER_WORKER: u64 = 3;

/// Errors shared by all ACX crates. Each variant maps to a CLI exit code
/// (PRD §6.4).
#[derive(Debug, thiserror::Error)]
pub enum AcxError {
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("invalid archive: {0}")]
    InvalidArchive(String),
    #[error("integrity check failed: {0}")]
    Integrity(String),
    #[error("unsupported codec ID 0x{0:04x}")]
    UnsupportedCodec(u16),
    #[error("unsupported archive version {major}.{minor}")]
    UnsupportedVersion { major: u16, minor: u16 },
    #[error("unsupported archive feature: {0}")]
    UnsupportedFeature(String),
    #[error("{context}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    #[error("resource limit exceeded: {0}")]
    ResourceLimit(String),
    #[error("cancelled")]
    Cancelled,
}

impl AcxError {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::InvalidArgument(_) => 2,
            Self::InvalidArchive(_) => 3,
            Self::Integrity(_) => 4,
            Self::UnsupportedCodec(_)
            | Self::UnsupportedVersion { .. }
            | Self::UnsupportedFeature(_) => 5,
            Self::Io { .. } => 6,
            Self::ResourceLimit(_) => 7,
            Self::Cancelled => 8,
        }
    }
}

/// Stable codec IDs stored in archives (PRD §3.1). Never reuse a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodecId {
    Raw,
    Zstd,
}

impl CodecId {
    pub const fn as_u16(self) -> u16 {
        match self {
            Self::Raw => 0x0000,
            Self::Zstd => 0x0001,
        }
    }
}

impl TryFrom<u16> for CodecId {
    type Error = AcxError;

    /// Unknown IDs are an error, never a fallback to another decoder.
    /// 0x0002 is reserved for LZMA2 and becomes a variant when implemented.
    fn try_from(id: u16) -> Result<Self, AcxError> {
        match id {
            0x0000 => Ok(Self::Raw),
            0x0001 => Ok(Self::Zstd),
            other => Err(AcxError::UnsupportedCodec(other)),
        }
    }
}

/// Block IDs are assigned in write order: entry `i` of the index is block `i`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub u32);

/// File IDs follow manifest order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompressionMode {
    Fast,
    #[default]
    Balanced,
    Maximum,
}

impl CompressionMode {
    pub const fn zstd_level(self) -> i32 {
        match self {
            Self::Fast => 1,
            Self::Balanced => 9,
            Self::Maximum => 19,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodecChoice {
    /// Try Zstandard and keep it only if it passes the acceptance rule.
    #[default]
    Auto,
    Raw,
    Zstd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DedupMode {
    None,
    #[default]
    Blocks,
}

/// What to do with unreadable, non-Unicode, or changing input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ErrorPolicy {
    #[default]
    Strict,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverwritePolicy {
    #[default]
    Error,
    Overwrite,
    SkipExisting,
}

/// Global concurrency and memory budget shared by all workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceBudget {
    pub threads: usize,
    pub memory_bytes: u64,
    pub block_size: u64,
}

impl Default for ResourceBudget {
    fn default() -> Self {
        Self {
            threads: std::thread::available_parallelism().map_or(1, NonZeroUsize::get),
            memory_bytes: DEFAULT_MEMORY,
            block_size: DEFAULT_BLOCK_SIZE,
        }
    }
}

impl ResourceBudget {
    pub fn validate(&self) -> Result<(), AcxError> {
        if self.threads == 0 {
            return Err(AcxError::InvalidArgument(
                "threads must be at least 1".into(),
            ));
        }
        if !(MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&self.block_size) {
            return Err(AcxError::InvalidArgument(format!(
                "block size {} is outside {MIN_BLOCK_SIZE}..={MAX_BLOCK_SIZE}",
                self.block_size
            )));
        }
        let one_worker = self.block_size * BUFFERS_PER_WORKER;
        if self.memory_bytes < one_worker {
            return Err(AcxError::InvalidArgument(format!(
                "memory budget {} is below the {one_worker} bytes one worker needs",
                self.memory_bytes
            )));
        }
        Ok(())
    }

    /// Workers that fit the memory budget, capped by `threads`, at least 1.
    pub fn effective_threads(&self) -> usize {
        let per_worker = self.block_size.saturating_mul(BUFFERS_PER_WORKER).max(1);
        let fit = usize::try_from(self.memory_bytes / per_worker).unwrap_or(usize::MAX);
        self.threads.min(fit).max(1)
    }
}

/// Set to `true` (for example from a Ctrl-C handler) to stop an operation:
/// no new work is started, temporary files are removed, and the operation
/// returns [`AcxError::Cancelled`].
pub type CancelFlag = Arc<AtomicBool>;

#[derive(Debug, Clone, Default)]
pub struct ArchiveOptions {
    pub mode: CompressionMode,
    pub codec: CodecChoice,
    pub dedup: DedupMode,
    /// Byte-compare a block with its earlier match before reusing it.
    pub verify_dedup: bool,
    pub budget: ResourceBudget,
    pub overwrite: bool,
    pub exclude: Vec<String>,
    pub error_policy: ErrorPolicy,
    pub cancel: CancelFlag,
}

#[derive(Debug, Clone)]
pub struct ExtractOptions {
    pub overwrite: OverwritePolicy,
    pub budget: ResourceBudget,
    /// Refuse extraction if the manifest's total file size exceeds this.
    pub max_output_bytes: u64,
    pub cancel: CancelFlag,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            overwrite: OverwritePolicy::default(),
            budget: ResourceBudget::default(),
            max_output_bytes: DEFAULT_MAX_OUTPUT,
            cancel: CancelFlag::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct VerifyOptions {
    /// Decode every block and check BLAKE3, not just CRCs and structure.
    pub full: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressEvent {
    Scanned { files: u64, bytes: u64 },
    BytesProcessed { bytes: u64 },
    Finished,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_ids_round_trip_and_unknown_is_error() {
        for codec in [CodecId::Raw, CodecId::Zstd] {
            assert_eq!(CodecId::try_from(codec.as_u16()).unwrap(), codec);
        }
        for id in [0x0002, 0x0100, 0x8000, u16::MAX] {
            let err = CodecId::try_from(id).unwrap_err();
            assert!(matches!(err, AcxError::UnsupportedCodec(x) if x == id));
            assert_eq!(err.exit_code(), 5);
        }
    }

    fn budget(threads: usize, memory_bytes: u64, block_size: u64) -> ResourceBudget {
        ResourceBudget {
            threads,
            memory_bytes,
            block_size,
        }
    }

    #[test]
    fn effective_threads_fits_memory() {
        // 1 GiB / (3 × 16 MiB) = 21 workers.
        assert_eq!(budget(64, GIB, 16 * MIB).effective_threads(), 21);
        assert_eq!(budget(8, GIB, 16 * MIB).effective_threads(), 8);
        // Below one worker's need still yields 1; validate() rejects it.
        assert_eq!(budget(8, MIB, 16 * MIB).effective_threads(), 1);
        // No overflow at extremes.
        assert_eq!(
            budget(usize::MAX, u64::MAX, u64::MAX).effective_threads(),
            1
        );
        assert_eq!(budget(4, u64::MAX, 0).effective_threads(), 4);
    }

    #[test]
    fn validate_rejects_bad_budgets() {
        assert!(ResourceBudget::default().validate().is_ok());
        assert!(budget(0, GIB, 16 * MIB).validate().is_err());
        assert!(budget(1, GIB, MIN_BLOCK_SIZE - 1).validate().is_err());
        assert!(budget(1, GIB, MAX_BLOCK_SIZE + 1).validate().is_err());
        assert!(budget(1, 3 * 16 * MIB - 1, 16 * MIB).validate().is_err());
        assert!(budget(1, 3 * 16 * MIB, 16 * MIB).validate().is_ok());
    }

    #[test]
    fn defaults_match_prd() {
        let a = ArchiveOptions::default();
        assert_eq!(a.mode.zstd_level(), 9);
        assert_eq!(a.codec, CodecChoice::Auto);
        assert_eq!(a.dedup, DedupMode::Blocks);
        assert_eq!(a.budget.block_size, 16 * MIB);
        assert_eq!(a.budget.memory_bytes, GIB);
        assert_eq!(ExtractOptions::default().max_output_bytes, TIB);
    }
}
