use std::fs;
use std::path::{Path, PathBuf};

use acx_archive::{create_archive, extract_archive, list_archive, verify_archive};
use acx_container::{ArchiveWriter, BlockRef, EntryKind, ManifestEntry};
use acx_core::{
    AcxError, ArchiveOptions, CodecChoice, CodecId, ExtractOptions, KIB, OverwritePolicy,
    ResourceBudget, VerifyOptions,
};
use proptest::prelude::*;
use tempfile::TempDir;

const SMALL_BLOCK: u64 = 64 * KIB;

fn options(block_size: u64, codec: CodecChoice) -> ArchiveOptions {
    ArchiveOptions {
        codec,
        budget: ResourceBudget {
            block_size,
            ..ResourceBudget::default()
        },
        ..ArchiveOptions::default()
    }
}

/// Deterministic, incompressible bytes.
fn noise(len: usize) -> Vec<u8> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn text(len: usize) -> Vec<u8> {
    b"the quick brown fox jumps over the lazy dog\n"
        .iter()
        .copied()
        .cycle()
        .take(len)
        .collect()
}

struct Case {
    dir: TempDir,
    input: PathBuf,
    archive: PathBuf,
}

impl Case {
    fn new(data: &[u8]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.bin");
        fs::write(&input, data).unwrap();
        let archive = dir.path().join("out.acx");
        Self {
            dir,
            input,
            archive,
        }
    }

    fn create(&self, opts: &ArchiveOptions) -> Result<acx_archive::ArchiveReport, AcxError> {
        create_archive(
            std::slice::from_ref(&self.input),
            &self.archive,
            opts,
            |_| {},
        )
    }

    fn extract_to(&self, name: &str, policy: OverwritePolicy) -> Result<PathBuf, AcxError> {
        let out = self.dir.path().join(name);
        let opts = ExtractOptions {
            overwrite: policy,
            ..ExtractOptions::default()
        };
        extract_archive(&self.archive, &out, &opts, |_| {})?;
        Ok(out)
    }

    fn verify(&self, full: bool) -> acx_archive::VerifyReport {
        verify_archive(&self.archive, &VerifyOptions { full }).unwrap()
    }
}

fn leftovers(dir: &Path) -> Vec<String> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect()
}

fn roundtrip(data: &[u8], opts: &ArchiveOptions) -> acx_archive::ArchiveReport {
    let case = Case::new(data);
    let report = case.create(opts).unwrap();
    assert!(case.verify(false).is_ok());
    assert!(case.verify(true).is_ok());
    let out = case.extract_to("restored", OverwritePolicy::Error).unwrap();
    let restored = out.join("input.bin");
    assert_eq!(fs::read(&restored).unwrap(), data);
    assert_eq!(
        fs::metadata(&restored).unwrap().modified().unwrap(),
        fs::metadata(&case.input).unwrap().modified().unwrap()
    );
    assert!(leftovers(case.dir.path()).is_empty() && leftovers(&out).is_empty());
    report
}

#[test]
fn block_boundaries_round_trip() {
    let b = SMALL_BLOCK as usize;
    for (len, blocks) in [
        (0, 0),
        (1, 1),
        (b - 1, 1),
        (b, 1),
        (b + 1, 2),
        (3 * b + 5, 4),
    ] {
        for codec in [CodecChoice::Auto, CodecChoice::Raw] {
            let r = roundtrip(&noise(len), &options(SMALL_BLOCK, codec));
            assert_eq!(
                (r.input_bytes, r.blocks),
                (len as u64, blocks),
                "{len} {codec:?}"
            );
        }
    }
}

#[test]
fn routing_keeps_zstd_only_when_it_pays() {
    let r = roundtrip(&text(200_000), &options(SMALL_BLOCK, CodecChoice::Auto));
    assert_eq!(r.compressed_blocks, r.blocks);
    assert!(r.archive_bytes < 20_000, "{}", r.archive_bytes);

    let r = roundtrip(&noise(200_000), &options(SMALL_BLOCK, CodecChoice::Zstd));
    assert_eq!(r.compressed_blocks, 0, "incompressible data must stay RAW");

    let r = roundtrip(&text(200_000), &options(SMALL_BLOCK, CodecChoice::Raw));
    assert_eq!(r.compressed_blocks, 0);
}

#[test]
fn listing_shows_entry_and_blocks() {
    let case = Case::new(&text(100_000));
    case.create(&options(SMALL_BLOCK, CodecChoice::Auto))
        .unwrap();
    let l = list_archive(&case.archive).unwrap();
    assert_eq!(l.entries.len(), 1);
    assert_eq!(
        (l.entries[0].path.as_str(), l.entries[0].size),
        ("input.bin", 100_000)
    );
    assert_eq!(l.blocks.len(), 2);
    assert_eq!(l.archive_len, fs::metadata(&case.archive).unwrap().len());
}

#[test]
fn create_respects_overwrite_and_leaves_no_temp_files() {
    let case = Case::new(b"new");
    fs::write(&case.archive, b"old archive").unwrap();
    let opts = options(SMALL_BLOCK, CodecChoice::Auto);
    assert!(matches!(
        case.create(&opts),
        Err(AcxError::InvalidArgument(_))
    ));
    assert_eq!(fs::read(&case.archive).unwrap(), b"old archive");

    case.create(&ArchiveOptions {
        overwrite: true,
        ..opts.clone()
    })
    .unwrap();
    assert!(case.verify(true).is_ok());

    let missing = create_archive(
        &[case.dir.path().join("nope")],
        &case.dir.path().join("x.acx"),
        &opts,
        |_| {},
    );
    assert!(matches!(missing, Err(AcxError::Io { .. })));
    let none = create_archive(&[], &case.dir.path().join("y.acx"), &opts, |_| {});
    assert!(matches!(none, Err(AcxError::InvalidArgument(_))));
    assert!(leftovers(case.dir.path()).is_empty());
    assert!(!case.dir.path().join("x.acx").exists() && !case.dir.path().join("y.acx").exists());
}

#[test]
fn extract_overwrite_policies() {
    let case = Case::new(b"payload");
    case.create(&options(SMALL_BLOCK, CodecChoice::Auto))
        .unwrap();
    let out = case.extract_to("o", OverwritePolicy::Error).unwrap();
    let target = out.join("input.bin");
    fs::write(&target, b"local edit").unwrap();

    assert!(matches!(
        case.extract_to("o", OverwritePolicy::Error),
        Err(AcxError::Io { .. })
    ));
    case.extract_to("o", OverwritePolicy::SkipExisting).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"local edit");
    case.extract_to("o", OverwritePolicy::Overwrite).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"payload");
}

#[test]
fn extract_enforces_output_limit_before_writing() {
    let case = Case::new(&text(10_000));
    case.create(&options(SMALL_BLOCK, CodecChoice::Auto))
        .unwrap();
    let out = case.dir.path().join("limited");
    let opts = ExtractOptions {
        max_output_bytes: 9_999,
        ..ExtractOptions::default()
    };
    let err = extract_archive(&case.archive, &out, &opts, |_| {}).unwrap_err();
    assert!(matches!(err, AcxError::ResourceLimit(_)));
    assert!(!out.exists());
}

#[test]
fn damaged_block_is_reported_and_never_extracted() {
    for codec in [CodecChoice::Auto, CodecChoice::Raw] {
        let case = Case::new(&text(3 * SMALL_BLOCK as usize));
        case.create(&options(SMALL_BLOCK, codec)).unwrap();
        let l = list_archive(&case.archive).unwrap();
        let mut bytes = fs::read(&case.archive).unwrap();
        let b1 = l.blocks[1];
        bytes[(b1.record.offset + b1.record.len - 1) as usize] ^= 0xFF; // last payload byte
        fs::write(&case.archive, &bytes).unwrap();

        for full in [false, true] {
            let r = case.verify(full);
            assert!(!r.is_ok());
            assert!(
                r.problems[0].starts_with(&format!("block 1 at offset {}", b1.record.offset)),
                "{:?}",
                r.problems
            );
        }
        assert!(
            case.verify(true)
                .problems
                .iter()
                .any(|p| p.contains("input.bin"))
        );

        let err = case.extract_to("bad", OverwritePolicy::Error).unwrap_err();
        assert!(matches!(err, AcxError::Integrity(_)), "{err}");
        let out = case.dir.path().join("bad");
        assert!(!out.join("input.bin").exists());
        assert!(leftovers(&out).is_empty());
    }
}

#[test]
fn damaged_metadata_and_garbage_fail_to_open() {
    let case = Case::new(&text(1000));
    case.create(&options(SMALL_BLOCK, CodecChoice::Auto))
        .unwrap();
    let good = fs::read(&case.archive).unwrap();
    let mut cases = vec![
        good[..good.len() - 1].to_vec(),
        [good.clone(), vec![0]].concat(),
        b"definitely not an archive, but long enough to have a header and footer region......................................................................".to_vec(),
    ];
    let mut flipped = good.clone();
    let n = flipped.len();
    flipped[n - 64 - 20] ^= 1; // inside the index
    cases.push(flipped);
    for bytes in cases {
        fs::write(&case.archive, &bytes).unwrap();
        assert!(verify_archive(&case.archive, &VerifyOptions { full: true }).is_err());
        assert!(case.extract_to("never", OverwritePolicy::Error).is_err());
    }
    assert!(!case.dir.path().join("never").join("input.bin").exists());
}

/// Builds an archive by hand, bypassing `create_archive`'s own checks.
fn crafted(dir: &Path, paths: &[&str]) -> PathBuf {
    let mut w = ArchiveWriter::new(Vec::new()).unwrap();
    let data = b"x";
    let block = w
        .write_block(CodecId::Raw, 1, *blake3::hash(data).as_bytes(), data)
        .unwrap();
    let entries: Vec<_> = paths
        .iter()
        .map(|p| ManifestEntry {
            kind: EntryKind::File,
            path: (*p).into(),
            mode: None,
            size: 1,
            mtime_ns: None,
            digest: *blake3::hash(data).as_bytes(),
            refs: vec![BlockRef {
                block,
                offset: 0,
                len: 1,
            }],
        })
        .collect();
    let path = dir.join("crafted.acx");
    fs::write(&path, w.finish(&entries).unwrap()).unwrap();
    path
}

#[test]
fn nested_paths_and_shared_blocks_extract() {
    let dir = tempfile::tempdir().unwrap();
    let archive = crafted(dir.path(), &["a/b/one", "a/two", "three"]);
    let out = dir.path().join("out");
    extract_archive(&archive, &out, &ExtractOptions::default(), |_| {}).unwrap();
    for p in ["a/b/one", "a/two", "three"] {
        assert_eq!(fs::read(out.join(p)).unwrap(), b"x");
    }
}

#[cfg(unix)]
#[test]
fn symlink_in_destination_is_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let out = dir.path().join("out");
    fs::create_dir(&out).unwrap();
    std::os::unix::fs::symlink(&outside, out.join("a")).unwrap();
    let archive = crafted(dir.path(), &["a/escape"]);
    let err = extract_archive(&archive, &out, &ExtractOptions::default(), |_| {}).unwrap_err();
    assert!(matches!(err, AcxError::Io { .. }), "{err}");
    assert!(!outside.join("escape").exists());
}

#[cfg(windows)]
#[test]
fn windows_unsafe_names_are_refused_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    for paths in [
        &["ok", "CON"][..],
        &["ok", "a./b"],
        &["ok", "ab:c"],
        &["ok", "a\\..\\..\\b"],
        &["ok", "A.txt", "a.txt"],
    ] {
        let archive = crafted(dir.path(), paths);
        let out = dir.path().join("out");
        let err = extract_archive(&archive, &out, &ExtractOptions::default(), |_| {}).unwrap_err();
        assert!(
            matches!(err, AcxError::InvalidArchive(_)),
            "{paths:?}: {err}"
        );
        assert!(!out.join("ok").exists(), "{paths:?}");
    }
}

/// Run with `cargo test --release -- --ignored`: archives a sparse 4 GiB + 1
/// byte file to prove sizes and offsets above u32 work end to end.
#[test]
#[ignore = "writes and reads 4 GiB; run in release"]
fn file_larger_than_4_gib() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("big.bin");
    let f = fs::File::create(&input).unwrap();
    f.set_len((4 << 30) + 1).unwrap();
    drop(f);
    let archive = dir.path().join("big.acx");
    let opts = options(16 << 20, CodecChoice::Auto);
    let r = create_archive(&[input], &archive, &opts, |_| {}).unwrap();
    assert_eq!((r.input_bytes, r.blocks), ((4 << 30) + 1, 257));
    assert!(
        verify_archive(&archive, &VerifyOptions { full: true })
            .unwrap()
            .is_ok()
    );
    let out = dir.path().join("out");
    extract_archive(&archive, &out, &ExtractOptions::default(), |_| {}).unwrap();
    assert_eq!(
        fs::metadata(out.join("big.bin")).unwrap().len(),
        (4 << 30) + 1
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]
    #[test]
    fn arbitrary_files_round_trip(data in prop::collection::vec(any::<u8>(), 0..200_000),
                                  raw in any::<bool>()) {
        let codec = if raw { CodecChoice::Raw } else { CodecChoice::Auto };
        roundtrip(&data, &options(SMALL_BLOCK, codec));
    }
}
