use acx_container::{
    BLOCK_RECORD_HEAD_LEN, FLAG_OPTIONAL, FRAME_LEN, Frame, IndexEntry, RecordType, Span,
    block_record_head, check_decoded, decode_index, encode_index, read_block,
};
use acx_core::{AcxError, BlockId, CodecId, MAX_BLOCK_SIZE};
use proptest::prelude::*;

const GOLDEN_BLOCK: &[u8] = include_bytes!("fixtures/block_raw_v1.bin");
const GOLDEN_INDEX: &[u8] = include_bytes!("fixtures/index_v1.bin");

fn golden_entry() -> IndexEntry {
    let digest: [u8; 32] = std::array::from_fn(|i| i as u8);
    IndexEntry::new(BlockId(0), CodecId::Raw, 64, 5, 5, digest)
}

/// Encodes `data` as block `id` at `offset`, the way the writer will.
fn make_block(id: u32, codec: CodecId, data: &[u8], offset: u64) -> (Vec<u8>, IndexEntry) {
    let payload = acx_codecs::compress(codec, data, 3).unwrap();
    let entry = IndexEntry::new(
        BlockId(id),
        codec,
        offset,
        data.len() as u64,
        payload.len() as u64,
        *blake3::hash(data).as_bytes(),
    );
    let mut record = block_record_head(&entry, crc32c::crc32c(&payload)).to_vec();
    record.extend_from_slice(&payload);
    (record, entry)
}

/// The full read path for one block: validate record, decode, verify.
fn decode_block(record: &[u8], entry: &IndexEntry) -> Result<Vec<u8>, AcxError> {
    let payload = read_block(record, entry)?;
    let data = acx_codecs::decompress(entry.codec, payload, entry.original_size)?;
    check_decoded(&data, entry)?;
    Ok(data.into_owned())
}

fn sample() -> Vec<u8> {
    (0..5000u32).flat_map(|i| (i % 251).to_le_bytes()).collect()
}

fn frame_bytes(magic: &[u8; 4], code: u16, flags: u16, len: u64) -> [u8; FRAME_LEN] {
    let mut b = [0; FRAME_LEN];
    b[0..4].copy_from_slice(magic);
    b[4..6].copy_from_slice(&code.to_le_bytes());
    b[6..8].copy_from_slice(&flags.to_le_bytes());
    b[8..16].copy_from_slice(&len.to_le_bytes());
    b
}

// ---- frames ----

#[test]
fn frames_round_trip() {
    for t in [RecordType::Manifest, RecordType::Block, RecordType::Index] {
        let f = Frame::decode(&Frame::encode(t, 1234)).unwrap();
        assert_eq!(
            (f.record_type, f.optional, f.payload_len),
            (Some(t), false, 1234)
        );
        Frame::expect(&Frame::encode(t, 1234), t, 1250).unwrap();
    }
}

#[test]
fn frame_rejects_bad_magic_and_reserved_flags() {
    let b = frame_bytes(b"ACXM", 0x0002, 0, 0); // block type, manifest magic
    assert!(matches!(
        Frame::decode(&b),
        Err(AcxError::InvalidArchive(_))
    ));
    for flags in [2, 0x8000, 0xFFFF] {
        let b = frame_bytes(b"ACXB", 0x0002, flags, 0);
        assert!(matches!(
            Frame::decode(&b),
            Err(AcxError::InvalidArchive(_))
        ));
    }
}

#[test]
fn unknown_record_types_need_the_optional_bit() {
    for code in [0x0000, 0x0004, 0x7777, 0xFFFF] {
        let required = frame_bytes(b"ACXG", code, 0, 10);
        assert!(matches!(
            Frame::decode(&required),
            Err(AcxError::UnsupportedFeature(_))
        ));
        let optional = Frame::decode(&frame_bytes(b"ZZZZ", code, FLAG_OPTIONAL, 10)).unwrap();
        assert_eq!((optional.record_type, optional.payload_len), (None, 10));
        // Skippable, but never acceptable where a known record is required.
        assert!(
            Frame::expect(
                &frame_bytes(b"ZZZZ", code, FLAG_OPTIONAL, 10),
                RecordType::Block,
                26
            )
            .is_err()
        );
    }
}

#[test]
fn frame_expect_checks_type_and_length() {
    let b = Frame::encode(RecordType::Index, 100);
    assert!(Frame::expect(&b, RecordType::Manifest, 116).is_err());
    assert!(Frame::expect(&b, RecordType::Index, 115).is_err());
    assert!(Frame::expect(&b, RecordType::Index, 117).is_err());
    let huge = Frame::encode(RecordType::Index, u64::MAX);
    assert!(Frame::expect(&huge, RecordType::Index, u64::MAX).is_err());
}

// ---- blocks ----

#[test]
fn block_and_index_match_golden_fixtures() {
    let entry = golden_entry();
    let mut block = block_record_head(&entry, crc32c::crc32c(b"hello")).to_vec();
    block.extend_from_slice(b"hello");
    assert_eq!(block, GOLDEN_BLOCK);
    assert_eq!(encode_index(&[entry]).unwrap(), GOLDEN_INDEX);

    let manifest_offset = 64 + GOLDEN_BLOCK.len() as u64;
    assert_eq!(
        decode_index(GOLDEN_INDEX, manifest_offset, MAX_BLOCK_SIZE).unwrap(),
        vec![entry]
    );
    assert_eq!(read_block(GOLDEN_BLOCK, &entry).unwrap(), b"hello");
}

#[test]
fn raw_and_zstd_blocks_round_trip() {
    let data = sample();
    for codec in [CodecId::Raw, CodecId::Zstd] {
        let (record, entry) = make_block(7, codec, &data, 64);
        assert_eq!(record.len() as u64, entry.record.len);
        assert_eq!(decode_block(&record, &entry).unwrap(), data);
    }
    let (empty, entry) = make_block(0, CodecId::Raw, &[], 64);
    assert_eq!(empty.len(), BLOCK_RECORD_HEAD_LEN);
    assert!(decode_block(&empty, &entry).unwrap().is_empty());
}

#[test]
fn every_byte_corruption_is_detected() {
    let data = sample();
    for codec in [CodecId::Raw, CodecId::Zstd] {
        let (record, entry) = make_block(3, codec, &data, 64);
        for at in 0..record.len() {
            let mut bad = record.clone();
            bad[at] ^= 0x01;
            assert!(decode_block(&bad, &entry).is_err(), "{codec:?} byte {at}");
        }
    }
}

#[test]
fn header_fields_disagreeing_with_index_are_integrity_errors() {
    let (record, entry) = make_block(3, CodecId::Zstd, &sample(), 64);
    let others = [
        IndexEntry {
            id: BlockId(4),
            ..entry
        },
        IndexEntry {
            codec: CodecId::Raw,
            ..entry
        },
        IndexEntry {
            original_size: entry.original_size + 1,
            ..entry
        },
        IndexEntry {
            digest: [0; 32],
            ..entry
        },
    ];
    for other in others {
        assert!(
            matches!(read_block(&record, &other), Err(AcxError::Integrity(_))),
            "{other:?}"
        );
    }
}

#[test]
fn truncated_or_padded_records_are_rejected() {
    let (record, entry) = make_block(0, CodecId::Zstd, &sample(), 64);
    for n in 0..record.len() {
        assert!(
            read_block(&record[..n], &entry).is_err(),
            "truncated to {n}"
        );
    }
    let mut padded = record.clone();
    padded.push(0);
    assert!(read_block(&padded, &entry).is_err());
}

#[test]
fn check_decoded_rejects_wrong_size_or_content() {
    let data = sample();
    let (_, entry) = make_block(0, CodecId::Raw, &data, 64);
    check_decoded(&data, &entry).unwrap();
    assert!(matches!(
        check_decoded(&data[1..], &entry),
        Err(AcxError::Integrity(_))
    ));
    let mut changed = data.clone();
    changed[100] ^= 1;
    assert!(matches!(
        check_decoded(&changed, &entry),
        Err(AcxError::Integrity(_))
    ));
}

// ---- index ----

fn two_blocks() -> (Vec<IndexEntry>, u64) {
    let a = make_block(0, CodecId::Zstd, &sample(), 64).1;
    let b = make_block(1, CodecId::Raw, b"tail", a.record.end().unwrap()).1;
    let manifest_offset = b.record.end().unwrap();
    (vec![a, b], manifest_offset)
}

fn index_err(entries: &[IndexEntry], manifest_offset: u64) -> AcxError {
    decode_index(
        &encode_index(entries).unwrap(),
        manifest_offset,
        MAX_BLOCK_SIZE,
    )
    .unwrap_err()
}

#[test]
fn index_round_trips_including_empty() {
    let (entries, m) = two_blocks();
    let payload = encode_index(&entries).unwrap();
    assert_eq!(decode_index(&payload, m, MAX_BLOCK_SIZE).unwrap(), entries);
    assert!(
        decode_index(&encode_index(&[]).unwrap(), 64, MAX_BLOCK_SIZE)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn index_allows_gaps_reserved_for_optional_records() {
    let (mut entries, m) = two_blocks();
    entries[1].record.offset += 100;
    let payload = encode_index(&entries).unwrap();
    assert_eq!(
        decode_index(&payload, m + 100, MAX_BLOCK_SIZE).unwrap(),
        entries
    );
}

#[test]
fn index_rejects_bad_headers() {
    let (entries, m) = two_blocks();
    let good = encode_index(&entries).unwrap();
    let decode = |p: &[u8]| decode_index(p, m, MAX_BLOCK_SIZE);

    let mut v2 = good.clone();
    v2[0] = 2;
    assert!(matches!(decode(&v2), Err(AcxError::UnsupportedFeature(_))));
    assert!(decode(&good[..7]).is_err());
    assert!(decode(&good[..good.len() - 1]).is_err());
    let mut longer = good.clone();
    longer.push(0);
    assert!(decode(&longer).is_err());
    let mut count = good.clone();
    count[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(decode(&count).is_err());
}

#[test]
fn index_rejects_bad_entries() {
    let (e, m) = two_blocks();
    let with = |i: usize, f: &dyn Fn(&mut IndexEntry)| {
        let mut v = e.clone();
        f(&mut v[i]);
        v
    };
    let invalid = [
        with(1, &|x| x.id = BlockId(2)),
        with(0, &|x| x.id = BlockId(1)),
        with(1, &|x| x.compressed_size += 1), // RAW sizes differ and record length off
        with(0, &|x| x.record.len += 1),
        with(0, &|x| x.record.offset = 63),
        with(1, &|x| x.record.offset -= 1), // overlaps block 0
        with(1, &|x| x.record.offset = u64::MAX),
        with(1, &|x| x.record.offset += 1), // runs into the manifest
    ];
    for entries in invalid {
        assert!(
            matches!(index_err(&entries, m), AcxError::InvalidArchive(_)),
            "{entries:?}"
        );
    }

    let mut flags = encode_index(&e).unwrap();
    flags[8 + 6] = 1;
    assert!(matches!(
        decode_index(&flags, m, MAX_BLOCK_SIZE),
        Err(AcxError::InvalidArchive(_))
    ));

    let mut codec = encode_index(&e).unwrap();
    codec[8 + 4..8 + 6].copy_from_slice(&0x0002u16.to_le_bytes());
    assert!(matches!(
        decode_index(&codec, m, MAX_BLOCK_SIZE),
        Err(AcxError::UnsupportedCodec(2))
    ));
}

#[test]
fn index_enforces_block_size_limit_before_allocating() {
    let (e, m) = two_blocks();
    let err = decode_index(&encode_index(&e).unwrap(), m, e[0].original_size - 1).unwrap_err();
    assert!(matches!(err, AcxError::ResourceLimit(_)));
    // A RAW block claiming 1 TiB is refused before anything is read.
    let huge = IndexEntry::new(BlockId(0), CodecId::Raw, 64, 1 << 40, 1 << 40, [0; 32]);
    let err = index_err(&[huge], huge.record.end().unwrap());
    assert!(matches!(err, AcxError::ResourceLimit(_)));
}

proptest! {
    #[test]
    fn arbitrary_index_payload_never_panics(payload in prop::collection::vec(any::<u8>(), 0..600),
                                            manifest_offset in any::<u64>(),
                                            max in any::<u64>()) {
        let _ = decode_index(&payload, manifest_offset, max);
        let mut p = payload;
        if p.len() >= 8 {
            // Plausible header so entry parsing is reached.
            let count = ((p.len() - 8) / 72) as u32;
            p[0..4].copy_from_slice(&1u32.to_le_bytes());
            p[4..8].copy_from_slice(&count.to_le_bytes());
            p.truncate(8 + count as usize * 72);
            let _ = decode_index(&p, manifest_offset, max);
        }
    }

    #[test]
    fn arbitrary_frames_and_records_never_panic(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
        if let Ok(b) = <[u8; FRAME_LEN]>::try_from(&bytes[..bytes.len().min(16)]) {
            let _ = Frame::decode(&b);
        }
        let _ = read_block(&bytes, &golden_entry());
        let e = IndexEntry { record: Span { offset: 64, len: bytes.len() as u64 }, ..golden_entry() };
        let _ = read_block(&bytes, &e);
    }

    #[test]
    fn written_blocks_always_read_back(blocks in prop::collection::vec(
            (prop::collection::vec(any::<u8>(), 0..2000), any::<bool>()), 0..6)) {
        let mut offset = 64;
        let mut entries = Vec::new();
        let mut file = vec![0u8; 64];
        for (i, (data, zstd)) in blocks.iter().enumerate() {
            let codec = if *zstd { CodecId::Zstd } else { CodecId::Raw };
            let (record, entry) = make_block(i as u32, codec, data, offset);
            offset += record.len() as u64;
            file.extend_from_slice(&record);
            entries.push(entry);
        }
        let decoded = decode_index(&encode_index(&entries).unwrap(), offset, MAX_BLOCK_SIZE).unwrap();
        prop_assert_eq!(&decoded, &entries);
        for (e, (data, _)) in decoded.iter().zip(&blocks) {
            let r = &file[e.record.offset as usize..e.record.end().unwrap() as usize];
            prop_assert_eq!(&decode_block(r, e).unwrap(), data);
        }
    }
}
