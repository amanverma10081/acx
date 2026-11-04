//! ACX payload codecs (PRD §3, FORMAT_V1 §9).
//!
//! A `match` on [`CodecId`] stands in for the PRD's `PayloadCodec` trait:
//! same contract, less code. Input is always one block, never a whole file.

use std::borrow::Cow;

use acx_core::{AcxError, CodecId, MAX_BLOCK_SIZE};

/// Version of the linked Zstandard library, for benchmark reports.
pub fn zstd_version() -> &'static str {
    zstd::zstd_safe::version_string()
}

/// Encodes one block. RAW borrows the input instead of copying it.
pub fn compress(codec: CodecId, input: &[u8], zstd_level: i32) -> Result<Cow<'_, [u8]>, AcxError> {
    match codec {
        CodecId::Raw => Ok(Cow::Borrowed(input)),
        // One complete frame with the content size; zstd's own threading stays off.
        CodecId::Zstd => zstd::bulk::compress(input, zstd_level)
            .map(Cow::Owned)
            .map_err(|source| AcxError::Io {
                context: "zstd compression failed".into(),
                source,
            }),
    }
}

/// Decodes one block and returns exactly `expected_size` bytes, or an error.
/// Never allocates more than `expected_size`, which is capped at
/// [`MAX_BLOCK_SIZE`]; never returns partial output.
pub fn decompress(
    codec: CodecId,
    input: &[u8],
    expected_size: u64,
) -> Result<Cow<'_, [u8]>, AcxError> {
    if expected_size > MAX_BLOCK_SIZE {
        return Err(AcxError::ResourceLimit(format!(
            "block of {expected_size} bytes exceeds the {MAX_BLOCK_SIZE}-byte maximum"
        )));
    }
    let out =
        match codec {
            CodecId::Raw => Cow::Borrowed(input),
            CodecId::Zstd => {
                // Fits: expected_size <= MAX_BLOCK_SIZE (256 MiB).
                let capacity = expected_size as usize;
                Cow::Owned(zstd::bulk::decompress(input, capacity).map_err(|e| {
                    AcxError::Integrity(format!("zstd payload does not decode: {e}"))
                })?)
            }
        };
    if out.len() as u64 != expected_size {
        return Err(AcxError::Integrity(format!(
            "{codec:?} payload decoded to {} bytes, expected {expected_size}",
            out.len()
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const CODECS: [CodecId; 2] = [CodecId::Raw, CodecId::Zstd];

    fn sample() -> Vec<u8> {
        b"ACX ".iter().copied().cycle().take(10_000).collect()
    }

    #[test]
    fn round_trips_and_zstd_shrinks_repetitive_data() {
        let data = sample();
        for codec in CODECS {
            let enc = compress(codec, &data, 3).unwrap();
            assert_eq!(
                decompress(codec, &enc, data.len() as u64).unwrap(),
                &data[..]
            );
        }
        assert!(compress(CodecId::Zstd, &data, 3).unwrap().len() < 100);
    }

    #[test]
    fn empty_input_round_trips() {
        for codec in CODECS {
            let enc = compress(codec, &[], 3).unwrap();
            assert!(decompress(codec, &enc, 0).unwrap().is_empty());
        }
    }

    #[test]
    fn wrong_expected_size_is_integrity_error() {
        let data = sample();
        let n = data.len() as u64;
        for codec in CODECS {
            let enc = compress(codec, &data, 3).unwrap();
            for wrong in [0, n - 1, n + 1] {
                let err = decompress(codec, &enc, wrong).unwrap_err();
                assert!(
                    matches!(err, AcxError::Integrity(_)),
                    "{codec:?} {wrong}: {err}"
                );
            }
        }
    }

    #[test]
    fn corrupt_zstd_is_integrity_error() {
        let data = sample();
        let enc = compress(CodecId::Zstd, &data, 3).unwrap().into_owned();
        let n = data.len() as u64;
        let truncated = &enc[..enc.len() - 1];
        let mut trailing = enc.clone();
        trailing.push(0);
        for bad in [truncated, &trailing[..], &[], b"not zstd at all"] {
            assert!(matches!(
                decompress(CodecId::Zstd, bad, n),
                Err(AcxError::Integrity(_))
            ));
        }
    }

    #[test]
    fn oversized_expected_size_is_rejected_before_allocating() {
        let err = decompress(CodecId::Zstd, &[], MAX_BLOCK_SIZE + 1).unwrap_err();
        assert!(matches!(err, AcxError::ResourceLimit(_)));
        assert!(decompress(CodecId::Raw, &[], u64::MAX).is_err());
    }

    proptest! {
        #[test]
        fn arbitrary_data_round_trips(data in prop::collection::vec(any::<u8>(), 0..4096),
                                      level in 1i32..=9) {
            for codec in CODECS {
                let enc = compress(codec, &data, level).unwrap();
                prop_assert_eq!(&decompress(codec, &enc, data.len() as u64).unwrap()[..], &data[..]);
            }
        }

        #[test]
        fn arbitrary_zstd_input_never_panics(input in prop::collection::vec(any::<u8>(), 0..512),
                                             size in 0u64..1 << 20) {
            let _ = decompress(CodecId::Zstd, &input, size);
        }
    }
}
