use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Instant;

use acx_archive::{create_archive, extract_archive, verify_archive};
use acx_core::{
    AcxError, ArchiveOptions, ExtractOptions, KIB, MIB, ProgressEvent, ResourceBudget,
    VerifyOptions,
};

const BLOCK: u64 = 64 * KIB;

fn budget(threads: usize, memory_bytes: u64) -> ResourceBudget {
    ResourceBudget {
        threads,
        memory_bytes,
        block_size: BLOCK,
    }
}

fn create_opts(threads: usize, memory_bytes: u64) -> ArchiveOptions {
    ArchiveOptions {
        budget: budget(threads, memory_bytes),
        ..ArchiveOptions::default()
    }
}

/// Text-like data that compresses, mixed with noise that does not.
fn mixed(len: usize, seed: u64) -> Vec<u8> {
    let words: [&[u8]; 6] = [b"frame ", b"pixel ", b"audio ", b"track ", b"0042 ", b"\n"];
    let mut x = seed | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        if x % 4 == 0 {
            out.extend_from_slice(&x.to_le_bytes());
        } else {
            out.extend_from_slice(words[(x % 6) as usize]);
        }
    }
    out.truncate(len);
    out
}

/// A tree whose files span many blocks, so workers really overlap.
fn make_tree(root: &Path, files: usize, size: usize) {
    for i in 0..files {
        let p = root.join(format!("d{}/f{i}.bin", i % 3));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, mixed(size + i * 1000, i as u64 + 1)).unwrap();
    }
    fs::write(root.join("empty"), b"").unwrap();
}

fn leftovers(dir: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .map(|e| e.unwrap().into_path())
        .filter(|p| p.to_string_lossy().ends_with(".tmp"))
        .collect()
}

#[test]
fn output_is_identical_for_any_thread_count() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_tree(&src, 6, 300_000);
    let mut archives = Vec::new();
    for threads in [1, 2, 8] {
        let archive = tmp.path().join(format!("t{threads}.acx"));
        create_archive(
            std::slice::from_ref(&src),
            &archive,
            &create_opts(threads, 64 * MIB),
            |_| {},
        )
        .unwrap();
        archives.push(fs::read(&archive).unwrap());
    }
    for w in archives.windows(2) {
        let first = w[0].iter().zip(&w[1]).position(|(a, b)| a != b);
        assert!(
            w[0] == w[1],
            "archives differ: {} vs {} bytes, first difference at {first:?}",
            w[0].len(),
            w[1].len()
        );
    }

    let archive = tmp.path().join("t1.acx");
    for threads in [1, 8] {
        let out = tmp.path().join(format!("out{threads}"));
        let opts = ExtractOptions {
            budget: budget(threads, 64 * MIB),
            ..ExtractOptions::default()
        };
        extract_archive(&archive, &out, &opts, |_| {}).unwrap();
        for i in 0..6 {
            let rel = format!("src/d{}/f{i}.bin", i % 3);
            assert_eq!(
                fs::read(out.join(&rel)).unwrap(),
                fs::read(tmp.path().join(&rel)).unwrap()
            );
        }
    }
}

#[test]
fn smallest_memory_budget_still_works_with_one_worker() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_tree(&src, 3, 200_000);
    let archive = tmp.path().join("a.acx");
    let opts = create_opts(16, 3 * BLOCK);
    assert_eq!(opts.budget.effective_threads(), 1);
    create_archive(std::slice::from_ref(&src), &archive, &opts, |_| {}).unwrap();
    assert!(
        verify_archive(&archive, &VerifyOptions { full: true })
            .unwrap()
            .is_ok()
    );

    let too_small = create_opts(16, 3 * BLOCK - 1);
    let err = create_archive(&[src], &tmp.path().join("b.acx"), &too_small, |_| {}).unwrap_err();
    assert!(matches!(err, AcxError::InvalidArgument(_)), "{err}");
}

#[test]
fn cancelled_create_leaves_nothing_behind() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_tree(&src, 4, 300_000);
    let archive = tmp.path().join("a.acx");

    // Cancelled before it starts, and again after the first chunk is read.
    for mid_run in [false, true] {
        let opts = create_opts(4, 64 * MIB);
        opts.cancel.store(!mid_run, Ordering::Relaxed);
        let cancel = opts.cancel.clone();
        let err = create_archive(std::slice::from_ref(&src), &archive, &opts, |e| {
            if matches!(e, ProgressEvent::BytesProcessed { .. }) {
                cancel.store(true, Ordering::Relaxed);
            }
        })
        .unwrap_err();
        assert!(matches!(err, AcxError::Cancelled), "{err}");
        assert_eq!(err.exit_code(), 8);
        assert!(!archive.exists());
        assert!(leftovers(tmp.path()).is_empty());
    }
}

#[test]
fn cancelled_extract_removes_partial_files() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_tree(&src, 4, 300_000);
    let archive = tmp.path().join("a.acx");
    create_archive(
        std::slice::from_ref(&src),
        &archive,
        &create_opts(4, 64 * MIB),
        |_| {},
    )
    .unwrap();

    let out = tmp.path().join("out");
    let opts = ExtractOptions {
        budget: budget(4, 64 * MIB),
        ..ExtractOptions::default()
    };
    let cancel = opts.cancel.clone();
    let err = extract_archive(&archive, &out, &opts, |e| {
        if matches!(e, ProgressEvent::BytesProcessed { .. }) {
            cancel.store(true, Ordering::Relaxed);
        }
    })
    .unwrap_err();
    assert!(matches!(err, AcxError::Cancelled), "{err}");
    assert!(leftovers(&out).is_empty());
    // Whatever was finished is complete and correct.
    for i in 0..4 {
        let rel = format!("src/d{}/f{i}.bin", i % 3);
        if let Ok(data) = fs::read(out.join(&rel)) {
            assert_eq!(data, fs::read(tmp.path().join(&rel)).unwrap());
        }
    }
}

/// Run with `cargo test --release -p acx-archive --test parallel -- --ignored --nocapture`.
#[test]
#[ignore = "benchmark; run in release"]
fn scaling_one_thread_vs_all() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    make_tree(&src, 16, 16 << 20); // ~256 MiB
    let input: u64 = walkdir::WalkDir::new(&src)
        .into_iter()
        .map(|e| e.unwrap().metadata().unwrap().len())
        .sum();
    let all = ResourceBudget::default().threads;
    println!(
        "input {} MiB, {all} logical CPUs, block 16 MiB, zstd balanced",
        input >> 20
    );
    for threads in [1, all] {
        let archive = tmp.path().join(format!("b{threads}.acx"));
        let opts = ArchiveOptions {
            budget: ResourceBudget {
                threads,
                memory_bytes: 4 << 30,
                block_size: 16 * MIB,
            },
            ..ArchiveOptions::default()
        };
        let t = Instant::now();
        create_archive(std::slice::from_ref(&src), &archive, &opts, |_| {}).unwrap();
        let create = t.elapsed().as_secs_f64();
        let out = tmp.path().join(format!("o{threads}"));
        let t = Instant::now();
        extract_archive(
            &archive,
            &out,
            &ExtractOptions {
                budget: opts.budget,
                ..ExtractOptions::default()
            },
            |_| {},
        )
        .unwrap();
        let extract = t.elapsed().as_secs_f64();
        let mib = input as f64 / (1 << 20) as f64;
        println!(
            "threads {threads:>2}: create {create:6.2}s ({:7.1} MiB/s)  extract {extract:6.2}s ({:7.1} MiB/s)  archive {} MiB",
            mib / create,
            mib / extract,
            fs::metadata(&archive).unwrap().len() >> 20
        );
    }
}
