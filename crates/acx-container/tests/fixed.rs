use acx_container::{FOOTER_LEN, Footer, HEADER_LEN, Header, Span};
use acx_core::AcxError;
use proptest::prelude::*;

const GOLDEN_HEADER: &[u8; HEADER_LEN] = include_bytes!("fixtures/header_v1.bin");
const GOLDEN_FOOTER: &[u8; FOOTER_LEN] = include_bytes!("fixtures/footer_v1.bin");
const GOLDEN_FILE_LEN: u64 = 176;

fn golden_footer() -> Footer {
    Footer {
        manifest: Span {
            offset: 64,
            len: 24,
        },
        index: Span {
            offset: 88,
            len: 24,
        },
        archive_len: GOLDEN_FILE_LEN,
        manifest_crc: 0x1122_3344,
        index_crc: 0x5566_7788,
    }
}

/// Recomputes the CRC so a mutation reaches the check after it.
fn resealed(mut b: [u8; 64]) -> [u8; 64] {
    let crc = crc32c::crc32c(&b[..56]);
    b[56..60].copy_from_slice(&crc.to_le_bytes());
    b
}

fn header_with(at: usize, bytes: &[u8]) -> [u8; 64] {
    let mut b = *GOLDEN_HEADER;
    b[at..at + bytes.len()].copy_from_slice(bytes);
    resealed(b)
}

fn footer_with(at: usize, bytes: &[u8]) -> [u8; 64] {
    let mut b = *GOLDEN_FOOTER;
    b[at..at + bytes.len()].copy_from_slice(bytes);
    resealed(b)
}

#[test]
fn crc32c_matches_spec_check_value() {
    assert_eq!(crc32c::crc32c(b"123456789"), 0xE306_9283);
}

#[test]
fn header_matches_golden_fixture() {
    assert_eq!(&Header::default().encode(), GOLDEN_HEADER);
    assert_eq!(Header::decode(GOLDEN_HEADER).unwrap(), Header::default());
}

#[test]
fn footer_matches_golden_fixture() {
    assert_eq!(&golden_footer().encode(), GOLDEN_FOOTER);
    assert_eq!(
        Footer::decode(GOLDEN_FOOTER, GOLDEN_FILE_LEN).unwrap(),
        golden_footer()
    );
}

#[test]
fn header_rejects_bad_magic() {
    let b = header_with(0, b"ACXARCH2");
    assert!(matches!(
        Header::decode(&b),
        Err(AcxError::InvalidArchive(_))
    ));
}

#[test]
fn header_reports_other_major_version_before_crc() {
    let mut b = *GOLDEN_HEADER;
    b[8..10].copy_from_slice(&2u16.to_le_bytes()); // CRC deliberately left stale
    assert!(matches!(
        Header::decode(&b),
        Err(AcxError::UnsupportedVersion { major: 2, minor: 0 })
    ));
}

#[test]
fn header_accepts_newer_minor_and_optional_flags() {
    let b = header_with(10, &7u16.to_le_bytes());
    assert_eq!(Header::decode(&b).unwrap().minor_version, 7);
    let b = header_with(20, &0x8000_0001u32.to_le_bytes());
    let h = Header::decode(&b).unwrap();
    assert_eq!(h.optional_flags, 0x8000_0001);
    assert_eq!(h.encode(), b);
}

#[test]
fn header_rejects_required_flags() {
    for bit in 0..32 {
        let b = header_with(16, &(1u32 << bit).to_le_bytes());
        let err = Header::decode(&b).unwrap_err();
        assert!(matches!(err, AcxError::UnsupportedFeature(_)));
        assert_eq!(err.exit_code(), 5);
    }
}

#[test]
fn header_rejects_wrong_size_and_reserved_bytes() {
    let b = header_with(12, &65u32.to_le_bytes());
    assert!(matches!(
        Header::decode(&b),
        Err(AcxError::InvalidArchive(_))
    ));
    for at in (24..56).chain(60..64) {
        let b = header_with(at, &[1]);
        assert!(
            matches!(Header::decode(&b), Err(AcxError::InvalidArchive(_))),
            "reserved byte {at}"
        );
    }
}

#[test]
fn every_single_bit_flip_is_rejected() {
    for byte in 0..64 {
        for bit in 0..8 {
            let mut h = *GOLDEN_HEADER;
            h[byte] ^= 1 << bit;
            assert!(Header::decode(&h).is_err(), "header byte {byte} bit {bit}");
            let mut f = *GOLDEN_FOOTER;
            f[byte] ^= 1 << bit;
            assert!(
                Footer::decode(&f, GOLDEN_FILE_LEN).is_err(),
                "footer byte {byte} bit {bit}"
            );
        }
    }
}

#[test]
fn footer_rejects_bad_magic_and_reserved() {
    let b = footer_with(0, b"ACXEND02");
    assert!(matches!(
        Footer::decode(&b, GOLDEN_FILE_LEN),
        Err(AcxError::InvalidArchive(_))
    ));
    let b = footer_with(60, &[0, 0, 0, 1]);
    assert!(matches!(
        Footer::decode(&b, GOLDEN_FILE_LEN),
        Err(AcxError::InvalidArchive(_))
    ));
}

#[test]
fn footer_rejects_crc_mismatch_as_integrity() {
    let mut b = *GOLDEN_FOOTER;
    b[48] ^= 1;
    let err = Footer::decode(&b, GOLDEN_FILE_LEN).unwrap_err();
    assert!(matches!(err, AcxError::Integrity(_)));
    assert_eq!(err.exit_code(), 4);
}

#[test]
fn footer_rejects_length_mismatch_and_short_files() {
    // Trailing data, truncation, and files too small to hold header + footer.
    for file_len in [0, 64, 127, GOLDEN_FILE_LEN - 1, GOLDEN_FILE_LEN + 1] {
        assert!(
            matches!(
                Footer::decode(GOLDEN_FOOTER, file_len),
                Err(AcxError::InvalidArchive(_))
            ),
            "file_len {file_len}"
        );
    }
}

fn layout_error(f: Footer) -> bool {
    let len = f.archive_len;
    matches!(
        Footer::decode(&f.encode(), len),
        Err(AcxError::InvalidArchive(_))
    )
}

#[test]
fn footer_rejects_bad_layouts() {
    let g = golden_footer();
    let span = |offset, len| Span { offset, len };
    let bad = [
        // Manifest overlapping the header.
        Footer {
            manifest: span(63, 25),
            ..g
        },
        // Records shorter than a frame.
        Footer {
            manifest: span(64, 15),
            index: span(79, 33),
            ..g
        },
        Footer {
            manifest: span(64, 33),
            index: span(97, 15),
            ..g
        },
        // Gap and overlap between manifest and index.
        Footer {
            index: span(89, 23),
            ..g
        },
        Footer {
            index: span(87, 25),
            ..g
        },
        // Index not ending at the footer.
        Footer {
            index: span(88, 23),
            ..g
        },
        // Overflowing offsets and lengths.
        Footer {
            manifest: span(u64::MAX, 24),
            ..g
        },
        Footer {
            manifest: span(64, u64::MAX),
            ..g
        },
        Footer {
            index: span(88, u64::MAX),
            ..g
        },
    ];
    for f in bad {
        assert!(layout_error(f), "{f:?}");
    }
}

#[test]
fn footer_accepts_blocks_before_manifest() {
    // 1000 bytes of blocks: manifest [1064, 1100), index [1100, 1200), file 1264.
    let f = Footer {
        manifest: Span {
            offset: 1064,
            len: 36,
        },
        index: Span {
            offset: 1100,
            len: 100,
        },
        archive_len: 1264,
        ..golden_footer()
    };
    assert_eq!(Footer::decode(&f.encode(), 1264).unwrap(), f);
}

proptest! {
    #[test]
    fn arbitrary_bytes_never_panic(bytes in prop::array::uniform32(any::<u8>()),
                                   more in prop::array::uniform32(any::<u8>()),
                                   file_len in any::<u64>()) {
        let mut b = [0u8; 64];
        b[..32].copy_from_slice(&bytes);
        b[32..].copy_from_slice(&more);
        let _ = Header::decode(&b);
        let _ = Footer::decode(&b, file_len);
        // Same again with valid magic and CRC, to reach the later checks.
        b[..8].copy_from_slice(b"ACXARCH1");
        let _ = Header::decode(&resealed(b));
        b[..8].copy_from_slice(b"ACXEND01");
        let _ = Footer::decode(&resealed(b), file_len);
    }

    #[test]
    fn header_round_trips(minor in any::<u16>(), optional_flags in any::<u32>()) {
        let h = Header { minor_version: minor, optional_flags };
        prop_assert_eq!(Header::decode(&h.encode()).unwrap(), h);
    }

    #[test]
    fn valid_footers_round_trip(blocks in 0u64..1 << 40,
                                manifest_len in 16u64..1 << 30,
                                index_len in 16u64..1 << 30,
                                manifest_crc in any::<u32>(),
                                index_crc in any::<u32>()) {
        let manifest = Span { offset: 64 + blocks, len: manifest_len };
        let index = Span { offset: manifest.offset + manifest_len, len: index_len };
        let archive_len = index.offset + index_len + 64;
        let f = Footer { manifest, index, archive_len, manifest_crc, index_crc };
        prop_assert_eq!(Footer::decode(&f.encode(), archive_len).unwrap(), f);
    }
}
