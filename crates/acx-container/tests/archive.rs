use std::io::Cursor;

use acx_container::{
    ArchiveReader, ArchiveWriter, BLOCK_RECORD_HEAD_LEN, BlockRef, EntryKind, FLAG_OPTIONAL,
    FRAME_LEN, Footer, Frame, Header, IndexEntry, ManifestEntry, RecordType, Span,
    block_record_head, decode_manifest, encode_index, encode_manifest, validate_path,
};
use acx_core::{AcxError, BlockId, CodecId, MAX_BLOCK_SIZE};
use proptest::prelude::*;

const GOLDEN_MANIFEST: &[u8] = include_bytes!("fixtures/manifest_v1.bin");

fn stand_in_digest() -> [u8; 32] {
    std::array::from_fn(|i| i as u8)
}

fn golden_entries() -> Vec<ManifestEntry> {
    vec![
        ManifestEntry {
            kind: EntryKind::File,
            path: "hello.txt".into(),
            mode: Some(0o644),
            size: 5,
            mtime_ns: Some(1_700_000_000_123_456_789),
            digest: stand_in_digest(),
            refs: vec![BlockRef {
                block: BlockId(0),
                offset: 0,
                len: 5,
            }],
        },
        ManifestEntry {
            kind: EntryKind::Directory,
            path: "docs".into(),
            mode: None,
            size: 0,
            mtime_ns: None,
            digest: [0; 32],
            refs: vec![],
        },
    ]
}

fn five_byte_block() -> Vec<IndexEntry> {
    vec![IndexEntry::new(
        BlockId(0),
        CodecId::Raw,
        64,
        5,
        5,
        stand_in_digest(),
    )]
}

fn file(path: &str, data: &[u8], refs: Vec<BlockRef>) -> ManifestEntry {
    ManifestEntry {
        kind: EntryKind::File,
        path: path.into(),
        mode: None,
        size: data.len() as u64,
        mtime_ns: None,
        digest: *blake3::hash(data).as_bytes(),
        refs,
    }
}

fn r(block: u32, offset: u64, len: u64) -> BlockRef {
    BlockRef {
        block: BlockId(block),
        offset,
        len,
    }
}

// ---- manifest ----

#[test]
fn manifest_matches_golden_fixture() {
    assert_eq!(encode_manifest(&golden_entries()).unwrap(), GOLDEN_MANIFEST);
    assert_eq!(
        decode_manifest(GOLDEN_MANIFEST, &five_byte_block()).unwrap(),
        golden_entries()
    );
}

#[test]
fn path_rules() {
    for ok in [
        "a",
        "a/b/c.txt",
        ".hidden",
        "...",
        "a b/c d",
        "日本語/ファイル.txt",
        "e\u{301}",
    ] {
        validate_path(ok).unwrap();
    }
    for bad in [
        "", "/a", "\\a", "a/", "a//b", ".", "..", "./a", "a/.", "a/../b", "C:", "c:/x", "a\0b",
    ] {
        assert!(validate_path(bad).is_err(), "{bad:?}");
    }
    assert!(validate_path(&"x".repeat(65536)).is_err());
}

#[test]
fn nfc_and_nfd_names_stay_distinct() {
    let nfc = file("\u{e9}", b"", vec![]);
    let nfd = file("e\u{301}", b"", vec![]);
    let decoded =
        decode_manifest(&encode_manifest(&[nfc.clone(), nfd.clone()]).unwrap(), &[]).unwrap();
    assert_eq!(decoded, vec![nfc, nfd]);
}

fn manifest_err(entries: &[ManifestEntry], blocks: &[IndexEntry]) -> AcxError {
    decode_manifest(&encode_manifest(entries).unwrap(), blocks).unwrap_err()
}

#[test]
fn manifest_rejects_bad_references() {
    let blocks = five_byte_block();
    let data = b"hello";
    let cases = [
        vec![file("a", data, vec![r(1, 0, 5)])], // missing block
        vec![file("a", data, vec![r(0, 1, 5)])], // past block end
        vec![file("a", data, vec![r(0, u64::MAX, 5)])], // overflow
        vec![file("a", data, vec![r(0, 0, 4)])], // sum != size
        vec![file("a", data, vec![r(0, 0, 5), r(0, 0, u64::MAX)])], // sum overflow
        vec![file("a", b"", vec![]), file("a", b"", vec![])], // duplicate path
    ];
    for entries in cases {
        assert!(
            matches!(manifest_err(&entries, &blocks), AcxError::InvalidArchive(_)),
            "{entries:?}"
        );
    }
    let mut dir = golden_entries()[1].clone();
    dir.refs = vec![r(0, 0, 0)];
    assert!(matches!(
        manifest_err(&[dir], &blocks),
        AcxError::InvalidArchive(_)
    ));
}

#[test]
fn manifest_rejects_bad_fields() {
    let blocks = five_byte_block();
    let good = GOLDEN_MANIFEST.to_vec();
    let decode = |p: &[u8]| decode_manifest(p, &blocks);
    let with = |at: usize, bytes: &[u8]| {
        let mut p = good.clone();
        p[at..at + bytes.len()].copy_from_slice(bytes);
        p
    };
    // First entry starts at 8; its fields at 8 + offset.
    let e = 8;
    assert!(matches!(
        decode(&with(0, &[2])),
        Err(AcxError::UnsupportedFeature(_))
    ));
    assert!(decode(&with(4, &[3])).is_err()); // count says 3
    assert!(decode(&with(4, &[0xFF, 0xFF, 0xFF, 0xFF])).is_err()); // huge count, no huge allocation
    assert!(decode(&with(e + 4, &[1])).is_err()); // file ID
    assert!(matches!(
        decode(&with(e + 8, &[3])),
        Err(AcxError::UnsupportedFeature(_))
    )); // symlink type
    assert!(matches!(
        decode(&with(e + 9, &[2])),
        Err(AcxError::UnsupportedFeature(_))
    )); // encoding
    assert!(decode(&with(e + 10, &[1])).is_err()); // flags
    assert!(decode(&with(e + 12, &0o10000u32.to_le_bytes())).is_err()); // mode bits
    assert!(decode(&with(e + 34, &[1])).is_err()); // reserved
    assert!(decode(&with(e + 32, &[200])).is_err()); // path length past entry
    assert!(decode(&with(e + 36, &[0xFF])).is_err()); // invalid UTF-8
    assert!(decode(&with(e + 36, b"../")).is_err()); // traversal
    assert!(decode(&with(e, &[100])).is_err()); // entry length
    let mut trailing = good.clone();
    trailing.push(0);
    assert!(decode(&trailing).is_err());
    for n in 0..good.len() {
        assert!(decode(&good[..n]).is_err(), "truncated to {n}");
    }
}

// ---- whole archives ----

/// Writes `blocks` (RAW) and a manifest with one file per block.
fn build(blocks: &[&[u8]]) -> Vec<u8> {
    let mut w = ArchiveWriter::new(Vec::new()).unwrap();
    let mut entries = Vec::new();
    for (i, data) in blocks.iter().enumerate() {
        let id = w
            .write_block(
                CodecId::Raw,
                data.len() as u64,
                *blake3::hash(data).as_bytes(),
                data,
            )
            .unwrap();
        entries.push(file(
            &format!("f{i}"),
            data,
            vec![r(id.0, 0, data.len() as u64)],
        ));
    }
    w.finish(&entries).unwrap()
}

#[test]
fn writer_output_reads_back() {
    let data: [&[u8]; 3] = [b"alpha", b"", b"gamma gamma"];
    let bytes = build(&data);
    let mut reader = ArchiveReader::open(Cursor::new(&bytes), MAX_BLOCK_SIZE).unwrap();
    assert_eq!(*reader.header(), Header::default());
    assert_eq!(reader.entries().len(), 3);
    reader.check_gaps().unwrap();
    for (b, want) in reader.blocks().to_vec().iter().zip(data) {
        let record = reader.read_block_record(b).unwrap();
        assert_eq!(&record[BLOCK_RECORD_HEAD_LEN..], want);
    }
}

#[test]
fn empty_archive_reads_back() {
    let bytes = ArchiveWriter::new(Vec::new()).unwrap().finish(&[]).unwrap();
    let mut reader = ArchiveReader::open(Cursor::new(&bytes), MAX_BLOCK_SIZE).unwrap();
    assert!(reader.entries().is_empty() && reader.blocks().is_empty());
    reader.check_gaps().unwrap();
}

#[test]
fn writer_refuses_manifest_its_reader_would_reject() {
    let mut w = ArchiveWriter::new(Vec::new()).unwrap();
    w.write_block(CodecId::Raw, 1, *blake3::hash(b"x").as_bytes(), b"x")
        .unwrap();
    assert!(w.finish(&[file("../x", b"x", vec![r(0, 0, 1)])]).is_err());
}

#[test]
fn every_truncation_and_trailing_garbage_is_rejected() {
    let bytes = build(&[b"alpha", b"beta"]);
    for n in 0..bytes.len() {
        assert!(
            ArchiveReader::open(Cursor::new(&bytes[..n]), MAX_BLOCK_SIZE).is_err(),
            "truncated to {n}"
        );
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(ArchiveReader::open(Cursor::new(&longer), MAX_BLOCK_SIZE).is_err());
}

#[test]
fn metadata_corruption_is_detected_on_open() {
    let bytes = build(&[b"alpha", b"beta"]);
    let reader = ArchiveReader::open(Cursor::new(&bytes), MAX_BLOCK_SIZE).unwrap();
    let f = *reader.footer();
    for at in f.manifest.offset..f.manifest.end().unwrap() {
        let mut bad = bytes.clone();
        bad[at as usize] ^= 1;
        assert!(
            ArchiveReader::open(Cursor::new(&bad), MAX_BLOCK_SIZE).is_err(),
            "manifest byte {at}"
        );
    }
}

/// Header, `gap`, one RAW block, manifest, index, footer.
fn with_gap(gap: &[u8]) -> Vec<u8> {
    let mut a = Header::default().encode().to_vec();
    a.extend_from_slice(gap);
    let data = b"hello";
    let block = IndexEntry::new(
        BlockId(0),
        CodecId::Raw,
        a.len() as u64,
        5,
        5,
        *blake3::hash(data).as_bytes(),
    );
    a.extend_from_slice(&block_record_head(&block, crc32c::crc32c(data)));
    a.extend_from_slice(data);
    let record = |kind, payload: &[u8], a: &mut Vec<u8>| {
        let span = Span {
            offset: a.len() as u64,
            len: (FRAME_LEN + payload.len()) as u64,
        };
        a.extend_from_slice(&Frame::encode(kind, payload.len() as u64));
        a.extend_from_slice(payload);
        span
    };
    let manifest = encode_manifest(&[file("x", data, vec![r(0, 0, 5)])]).unwrap();
    let index = encode_index(&[block]).unwrap();
    let m = record(RecordType::Manifest, &manifest, &mut a);
    let i = record(RecordType::Index, &index, &mut a);
    let footer = Footer {
        manifest: m,
        index: i,
        archive_len: a.len() as u64 + 64,
        manifest_crc: crc32c::crc32c(&manifest),
        index_crc: crc32c::crc32c(&index),
    };
    a.extend_from_slice(&footer.encode());
    a
}

fn optional_record(code: u16, flags: u16, payload: &[u8]) -> Vec<u8> {
    let mut b = vec![0; FRAME_LEN];
    b[0..4].copy_from_slice(b"ZZZZ");
    b[4..6].copy_from_slice(&code.to_le_bytes());
    b[6..8].copy_from_slice(&flags.to_le_bytes());
    b[8..16].copy_from_slice(&(payload.len() as u64).to_le_bytes());
    b.extend_from_slice(payload);
    b
}

#[test]
fn gaps_may_hold_only_optional_records() {
    let ok = with_gap(&optional_record(0x0099, FLAG_OPTIONAL, b"future"));
    let mut reader = ArchiveReader::open(Cursor::new(&ok), MAX_BLOCK_SIZE).unwrap();
    reader.check_gaps().unwrap();
    let block = reader.blocks()[0];
    reader.read_block_record(&block).unwrap();

    let bad_gaps = [
        optional_record(0x0099, 0, b"future"), // unknown and required
        b"junk".to_vec(),                      // shorter than a frame
        vec![0; 40],                           // not a frame
        {
            let mut v = optional_record(0x0099, FLAG_OPTIONAL, b"x");
            v[8] = 50; // claims to run past the block
            v
        },
    ];
    for gap in bad_gaps {
        let bytes = with_gap(&gap);
        let mut reader = ArchiveReader::open(Cursor::new(&bytes), MAX_BLOCK_SIZE).unwrap();
        assert!(reader.check_gaps().is_err(), "{gap:?}");
    }
}

proptest! {
    #[test]
    fn arbitrary_manifest_payload_never_panics(payload in prop::collection::vec(any::<u8>(), 0..400)) {
        let _ = decode_manifest(&payload, &five_byte_block());
        let mut p = payload;
        if p.len() >= 8 {
            p[0..4].copy_from_slice(&1u32.to_le_bytes());
            p[4..8].copy_from_slice(&1u32.to_le_bytes());
            let _ = decode_manifest(&p, &five_byte_block());
        }
    }

    #[test]
    fn arbitrary_archive_bytes_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..400)) {
        let _ = ArchiveReader::open(Cursor::new(&bytes), MAX_BLOCK_SIZE);
    }

    #[test]
    fn valid_manifests_round_trip(names in prop::collection::btree_set("[a-z]{1,8}(/[a-z0-9 ._-]{1,8}){0,2}", 0..8),
                                  mode in prop::option::of(0u32..=0o7777),
                                  mtime in prop::option::of(i64::MIN + 1..i64::MAX)) {
        let entries: Vec<_> = names
            .into_iter()
            .filter(|n| validate_path(n).is_ok())
            .map(|n| ManifestEntry { mode, mtime_ns: mtime, ..file(&n, b"", vec![]) })
            .collect();
        let decoded = decode_manifest(&encode_manifest(&entries).unwrap(), &[]).unwrap();
        prop_assert_eq!(decoded, entries);
    }
}
