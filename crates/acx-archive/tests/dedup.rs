use std::fs;
use std::path::{Path, PathBuf};

use acx_archive::{ArchiveReport, create_archive, extract_archive, list_archive, verify_archive};
use acx_core::{
    ArchiveOptions, DedupMode, ExtractOptions, KIB, MIB, ResourceBudget, VerifyOptions,
};

const BLOCK: usize = 64 * KIB as usize;

fn opts(dedup: DedupMode, verify_dedup: bool, threads: usize) -> ArchiveOptions {
    ArchiveOptions {
        dedup,
        verify_dedup,
        budget: ResourceBudget {
            threads,
            memory_bytes: 64 * MIB,
            block_size: BLOCK as u64,
        },
        ..ArchiveOptions::default()
    }
}

/// Distinct, incompressible bytes for each seed.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

struct Tree {
    tmp: tempfile::TempDir,
}

impl Tree {
    fn new(files: &[(&str, Vec<u8>)]) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        for (name, data) in files {
            let p = tmp.path().join("src").join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, data).unwrap();
        }
        Self { tmp }
    }

    fn archive(&self, name: &str, o: &ArchiveOptions) -> (PathBuf, ArchiveReport) {
        let archive = self.tmp.path().join(name);
        let r = create_archive(&[self.tmp.path().join("src")], &archive, o, |_| {}).unwrap();
        (archive, r)
    }

    /// Archives, checks, extracts, and compares every file.
    fn round_trip(&self, o: &ArchiveOptions) -> ArchiveReport {
        let (archive, r) = self.archive("t.acx", o);
        assert!(
            verify_archive(&archive, &VerifyOptions { full: true })
                .unwrap()
                .is_ok()
        );
        let out = self.tmp.path().join("out");
        extract_archive(&archive, &out, &ExtractOptions::default(), |_| {}).unwrap();
        compare(&self.tmp.path().join("src"), &out.join("src"));
        fs::remove_dir_all(&out).unwrap();
        fs::remove_file(&archive).unwrap();
        r
    }
}

fn compare(a: &Path, b: &Path) {
    for e in walkdir::WalkDir::new(a) {
        let e = e.unwrap();
        let other = b.join(e.path().strip_prefix(a).unwrap());
        if e.file_type().is_file() {
            assert_eq!(
                fs::read(e.path()).unwrap(),
                fs::read(&other).unwrap(),
                "{other:?}"
            );
        }
    }
}

#[test]
fn identical_files_are_stored_once() {
    let photo = noise(3 * BLOCK + 100, 1); // 4 blocks
    let tree = Tree::new(&[
        ("a/photo.jpg", photo.clone()),
        ("backup/photo.jpg", photo.clone()),
        ("backup/old/photo copy.jpg", photo),
        ("other.bin", noise(BLOCK, 2)),
    ]);
    let with = tree.round_trip(&opts(DedupMode::Blocks, false, 4));
    assert_eq!(with.blocks, 5);
    assert_eq!(with.duplicate_blocks, 8);
    assert_eq!(with.duplicate_bytes, 2 * (3 * BLOCK as u64 + 100));

    let without = tree.round_trip(&opts(DedupMode::None, false, 4));
    assert_eq!((without.blocks, without.duplicate_blocks), (13, 0));
    assert!(with.archive_bytes * 2 < without.archive_bytes);
}

#[test]
fn files_sharing_a_prefix_share_its_blocks() {
    let base = noise(3 * BLOCK, 3);
    let mut longer = base.clone();
    longer.extend(noise(BLOCK / 2, 4)); // same first 3 blocks, new tail
    let mut retail = base[..2 * BLOCK].to_vec();
    retail.extend(noise(BLOCK, 5)); // same first 2 blocks, new third
    let tree = Tree::new(&[("base", base), ("longer", longer), ("retail", retail)]);
    let r = tree.round_trip(&opts(DedupMode::Blocks, false, 4));
    assert_eq!(r.duplicate_blocks, 3 + 2);
    assert_eq!(r.blocks, 3 + 1 + 1);
}

#[test]
fn near_duplicates_share_only_unchanged_aligned_blocks() {
    let original = noise(4 * BLOCK, 6);
    let mut edited = original.clone();
    edited[BLOCK + 10] ^= 1; // one byte in block 1
    let mut shifted = vec![0u8];
    shifted.extend_from_slice(&original); // every block boundary moves
    let tree = Tree::new(&[
        ("edited", edited),
        ("original", original),
        ("shifted", shifted),
    ]);
    let r = tree.round_trip(&opts(DedupMode::Blocks, false, 4));
    // `edited` comes first: 4 blocks. `original` reuses 3. `shifted` reuses none:
    // fixed-size blocks cannot see shifted content (content-defined chunking is P1).
    assert_eq!(r.duplicate_blocks, 3);
    assert_eq!(r.blocks, 4 + 1 + 5);
}

#[test]
fn repeated_blocks_inside_one_file_and_short_tails_dedup() {
    let a = noise(BLOCK, 7);
    let b = noise(BLOCK, 8);
    let file = [a.clone(), b, a.clone(), a.clone()].concat();
    let tail = noise(1000, 9);
    let tree = Tree::new(&[
        ("repeats", file),
        ("t1", [a.clone(), tail.clone()].concat()),
        ("t2", tail),
    ]);
    let r = tree.round_trip(&opts(DedupMode::Blocks, false, 4));
    // repeats: a, b, (a), (a). t1: (a), tail. t2: (tail).
    assert_eq!((r.blocks, r.duplicate_blocks), (3, 4));
}

#[test]
fn verify_dedup_gives_the_same_archive() {
    let data = noise(2 * BLOCK + 7, 10);
    let tree = Tree::new(&[
        ("x", data.clone()),
        ("y", data),
        ("z", vec![b'a'; 3 * BLOCK]),
    ]);
    let (fast, r1) = tree.archive("fast.acx", &opts(DedupMode::Blocks, false, 4));
    let (checked, r2) = tree.archive("checked.acx", &opts(DedupMode::Blocks, true, 4));
    // `z` is three identical compressible blocks: checks the Zstd path too.
    assert_eq!((r1.blocks, r1.duplicate_blocks), (4, 5));
    assert_eq!((r2.blocks, r2.duplicate_blocks), (4, 5));
    assert_eq!(fs::read(fast).unwrap(), fs::read(checked).unwrap());
}

#[test]
fn block_ids_are_deterministic_with_dedup() {
    let shared = noise(5 * BLOCK, 11);
    let tree = Tree::new(&[
        ("a", shared.clone()),
        ("b", [noise(BLOCK, 12), shared.clone()].concat()),
        ("c", shared),
        ("d", vec![7u8; 6 * BLOCK]),
    ]);
    let archives: Vec<Vec<u8>> = [1, 2, 8]
        .iter()
        .map(|&t| {
            fs::read(
                tree.archive(&format!("t{t}.acx"), &opts(DedupMode::Blocks, false, t))
                    .0,
            )
            .unwrap()
        })
        .collect();
    assert!(archives.windows(2).all(|w| w[0] == w[1]));

    // Every manifest reference spans exactly one existing block.
    let l = list_archive(&tree.tmp.path().join("t1.acx")).unwrap();
    for e in &l.entries {
        for r in &e.refs {
            assert_eq!(l.blocks[r.block.0 as usize].original_size, r.len);
        }
    }
}
