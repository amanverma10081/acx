use std::fs;
use std::path::Path;

use acx_bench::{BenchConfig, run_benchmark, tree_manifest, write_csv, write_json};
use acx_core::{AcxError, ArchiveOptions, KIB, MIB, ResourceBudget};

fn config(runs: usize, warmup: usize) -> BenchConfig {
    BenchConfig {
        archive: ArchiveOptions {
            budget: ResourceBudget {
                threads: 2,
                memory_bytes: 64 * MIB,
                block_size: 64 * KIB,
            },
            ..ArchiveOptions::default()
        },
        warmup,
        runs,
        work_dir: None,
    }
}

fn dataset(root: &Path) {
    let text = b"benchmark line\n".repeat(20_000); // ~300 KB, compressible
    fs::create_dir_all(root.join("clips/empty-dir")).unwrap();
    fs::write(root.join("clips/a.txt"), &text).unwrap();
    fs::write(root.join("clips/a copy.txt"), &text).unwrap();
    fs::write(root.join("empty.bin"), b"").unwrap();
}

#[test]
fn reports_measured_runs_and_verifies_each() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("media");
    dataset(&root);
    let r = run_benchmark(&root, &config(3, 1)).unwrap();

    assert_eq!(r.runs.len(), 3);
    assert!(r.summary.all_verified && r.runs.iter().all(|run| run.verified));
    assert_eq!(r.dataset.id, "media");
    assert_eq!((r.dataset.files, r.dataset.directories), (3, 3));
    assert_eq!(r.dataset.bytes, 2 * 300_000);
    assert!(
        r.runs
            .iter()
            .all(|run| run.archive_bytes == r.summary.archive_bytes)
    );
    // Duplicate file plus compressible text: well under a tenth.
    assert!(r.summary.ratio > 10.0, "{}", r.summary.ratio);
    assert!(r.summary.create_secs.min <= r.summary.create_secs.median);
    assert!(r.summary.create_mib_s > 0.0 && r.summary.extract_mib_s > 0.0);
    assert_eq!(r.settings.effective_threads, 2);
    assert!(!r.environment.zstd_version.is_empty());

    let json = tmp.path().join("r.json");
    let csv = tmp.path().join("r.csv");
    write_json(&r, &json).unwrap();
    write_csv(&r, &csv).unwrap();
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(&json).unwrap()).unwrap();
    assert_eq!(v["dataset"]["manifest_blake3"], r.dataset.manifest_blake3);
    assert_eq!(v["runs"].as_array().unwrap().len(), 3);
    assert_eq!(v["summary"]["all_verified"], true);
    let lines: Vec<String> = fs::read_to_string(&csv)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(lines.len(), 4);
    assert!(lines[0].starts_with("run,dataset_manifest_blake3,"));
    assert!(lines[1].starts_with(&format!("1,{},600000,", r.dataset.manifest_blake3)));
    assert!(lines[3].ends_with(",true"));
}

#[test]
fn manifest_hash_identifies_the_exact_dataset() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("media");
    dataset(&root);
    let first = run_benchmark(&root, &config(1, 0))
        .unwrap()
        .dataset
        .manifest_blake3;
    let again = run_benchmark(&root, &config(1, 0))
        .unwrap()
        .dataset
        .manifest_blake3;
    assert_eq!(first, again);

    let before = tree_manifest(&root, "media").unwrap();
    let mut data = fs::read(root.join("clips/a.txt")).unwrap();
    data[1000] ^= 1;
    fs::write(root.join("clips/a.txt"), data).unwrap();
    assert_ne!(tree_manifest(&root, "media").unwrap(), before);
    let changed = run_benchmark(&root, &config(1, 0))
        .unwrap()
        .dataset
        .manifest_blake3;
    assert_ne!(changed, first);
}

#[test]
fn single_file_datasets_work() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("clip.bin");
    fs::write(&file, vec![9u8; 200_000]).unwrap();
    let r = run_benchmark(&file, &config(2, 0)).unwrap();
    assert!(r.summary.all_verified);
    assert_eq!((r.dataset.id.as_str(), r.dataset.files), ("clip.bin", 1));
}

#[test]
fn zero_runs_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let err = run_benchmark(tmp.path(), &config(0, 1)).unwrap_err();
    assert!(matches!(err, AcxError::InvalidArgument(_)));
}
