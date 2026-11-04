use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use acx_archive::{create_archive, extract_archive, list_archive, verify_archive};
use acx_core::{
    AcxError, ArchiveOptions, ErrorPolicy, ExtractOptions, KIB, ResourceBudget, VerifyOptions,
};
use filetime::FileTime;
use tempfile::TempDir;

fn options(policy: ErrorPolicy) -> ArchiveOptions {
    ArchiveOptions {
        error_policy: policy,
        budget: ResourceBudget {
            block_size: 64 * KIB,
            ..ResourceBudget::default()
        },
        ..ArchiveOptions::default()
    }
}

fn put(root: &Path, rel: &str, data: &[u8]) -> PathBuf {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, data).unwrap();
    p
}

/// Relative `/`-joined path -> file contents, or `None` for a directory.
fn snapshot(dir: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    walkdir::WalkDir::new(dir)
        .min_depth(1)
        .into_iter()
        .map(|e| {
            let e = e.unwrap();
            let rel = e.path().strip_prefix(dir).unwrap();
            let key = rel
                .components()
                .map(|c| c.as_os_str().to_str().unwrap())
                .collect::<Vec<_>>()
                .join("/");
            let content = e.file_type().is_file().then(|| fs::read(e.path()).unwrap());
            (key, content)
        })
        .collect()
}

struct Run {
    tmp: TempDir,
    archive: PathBuf,
    out: PathBuf,
}

impl Run {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("t.acx");
        let out = tmp.path().join("out");
        Self { tmp, archive, out }
    }

    fn src(&self) -> PathBuf {
        self.tmp.path().join("src")
    }

    fn create(
        &self,
        roots: &[PathBuf],
        policy: ErrorPolicy,
    ) -> Result<acx_archive::ArchiveReport, AcxError> {
        create_archive(roots, &self.archive, &options(policy), |_| {})
    }

    fn extract(&self) {
        extract_archive(&self.archive, &self.out, &ExtractOptions::default(), |_| {}).unwrap();
        assert!(
            verify_archive(&self.archive, &VerifyOptions { full: true })
                .unwrap()
                .is_ok()
        );
    }

    fn paths(&self) -> Vec<String> {
        list_archive(&self.archive)
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.path)
            .collect()
    }
}

fn unicode_names() -> Vec<&'static str> {
    let mut names = vec![
        "日本語.txt",
        "🎞 clip.mp4",
        "spaces (and) [brackets].txt",
        "Ünïcödé",
    ];
    // APFS treats NFC and NFD spellings as the same name.
    if !cfg!(target_os = "macos") {
        names.extend(["caf\u{e9}", "cafe\u{301}"]);
    }
    names
}

#[test]
fn directory_tree_round_trips_exactly() {
    let run = Run::new();
    let src = run.src();
    put(&src, "a/b/c/deep.txt", b"deep");
    put(&src, "a/empty-file", b"");
    put(&src, "big.bin", &vec![42u8; 200_000]); // 4 blocks
    fs::create_dir_all(src.join("empty/nested-empty")).unwrap();
    for name in unicode_names() {
        put(&src, &format!("names/{name}"), name.as_bytes());
    }

    let r = run
        .create(std::slice::from_ref(&src), ErrorPolicy::Strict)
        .unwrap();
    assert_eq!(r.files, 3 + unicode_names().len() as u64);
    assert_eq!(r.directories, 7); // src, a, a/b, a/b/c, empty, empty/nested-empty, names
    assert!(r.warnings.is_empty());
    run.extract();
    assert_eq!(snapshot(&run.out.join("src")), snapshot(&src));

    // Parents come before children, siblings in name order.
    let paths = run.paths();
    assert_eq!(paths[0], "src");
    for (i, p) in paths.iter().enumerate() {
        if let Some((parent, _)) = p.rsplit_once('/') {
            assert!(
                paths[..i].iter().any(|q| q == parent),
                "{p} before its parent"
            );
        }
    }
}

#[test]
fn several_roots_keep_their_own_names() {
    let run = Run::new();
    let src = run.src();
    put(&src, "photos/x.jpg", b"jpg");
    let single = put(&src, "notes.txt", b"notes");
    run.create(&[src.join("photos"), single], ErrorPolicy::Strict)
        .unwrap();
    assert_eq!(run.paths(), ["photos", "photos/x.jpg", "notes.txt"]);

    // `dir/sub/..` is named after the directory it resolves to.
    fs::create_dir_all(src.join("photos/sub")).unwrap();
    fs::remove_file(&run.archive).unwrap();
    run.create(
        &[src.join("photos").join("sub").join("..")],
        ErrorPolicy::Strict,
    )
    .unwrap();
    assert_eq!(run.paths()[0], "photos");
}

#[test]
fn colliding_roots_are_refused() {
    let run = Run::new();
    let src = run.src();
    put(&src, "one/x/a", b"1");
    put(&src, "two/x/b", b"2");
    let err = run
        .create(&[src.join("one/x"), src.join("two/x")], ErrorPolicy::Skip)
        .unwrap_err();
    assert!(matches!(err, AcxError::InvalidArgument(_)), "{err}");
    assert!(!run.archive.exists());
}

fn ft(secs: i64) -> FileTime {
    FileTime::from_unix_time(secs, 123_456_700)
}

#[test]
fn mtimes_restored_for_files_and_directories() {
    let run = Run::new();
    let src = run.src();
    let file = put(&src, "d/f.txt", b"x");
    let old = put(&src, "d/1969.txt", b"before the epoch");
    filetime::set_file_mtime(&file, ft(1_000_000_000)).unwrap();
    filetime::set_file_mtime(&old, ft(-31_536_000)).unwrap();
    filetime::set_file_mtime(src.join("d"), ft(1_100_000_000)).unwrap();
    filetime::set_file_mtime(&src, ft(1_200_000_000)).unwrap();

    run.create(std::slice::from_ref(&src), ErrorPolicy::Strict)
        .unwrap();
    run.extract();
    let mtime = |p: PathBuf| FileTime::from_last_modification_time(&fs::metadata(p).unwrap());
    let out = run.out.join("src");
    assert_eq!(mtime(out.join("d/f.txt")), ft(1_000_000_000));
    assert_eq!(mtime(out.join("d/1969.txt")), ft(-31_536_000));
    assert_eq!(mtime(out.join("d")), ft(1_100_000_000));
    assert_eq!(mtime(out), ft(1_200_000_000));
}

#[test]
fn existing_output_inside_an_input_is_not_archived() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    put(&src, "data.txt", b"data");
    let archive = src.join("self.acx");
    let mut opts = options(ErrorPolicy::Strict);
    create_archive(std::slice::from_ref(&src), &archive, &opts, |_| {}).unwrap();
    opts.overwrite = true;
    create_archive(std::slice::from_ref(&src), &archive, &opts, |_| {}).unwrap();
    let paths: Vec<_> = list_archive(&archive)
        .unwrap()
        .entries
        .into_iter()
        .map(|e| e.path)
        .collect();
    assert_eq!(paths, ["src", "src/data.txt"]);
}

#[test]
fn long_paths_round_trip() {
    let run = Run::new();
    let src = run.src();
    let segment = "a-fairly-long-directory-name-for-testing-paths-0123";
    let rel = format!("{}/leaf.txt", [segment; 7].join("/"));
    assert!(run.out.join("src").join(&rel).as_os_str().len() > 300);
    put(&src, &rel, b"far away");
    run.create(std::slice::from_ref(&src), ErrorPolicy::Strict)
        .unwrap();
    run.extract();
    assert_eq!(
        fs::read(run.out.join("src").join(&rel)).unwrap(),
        b"far away"
    );
}

/// Creates a file symlink, or returns `false` where that needs privileges.
fn try_symlink(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file(target, link);
    made.is_ok()
}

#[test]
fn symlinks_fail_strict_and_are_skipped_with_warning() {
    let run = Run::new();
    let src = run.src();
    let target = put(&src, "real.txt", b"real");
    if !try_symlink(&target, &src.join("link.txt")) {
        eprintln!("skipping: cannot create symlinks here");
        return;
    }
    let err = run
        .create(std::slice::from_ref(&src), ErrorPolicy::Strict)
        .unwrap_err();
    assert!(matches!(err, AcxError::InvalidArgument(_)), "{err}");

    let r = run
        .create(std::slice::from_ref(&src), ErrorPolicy::Skip)
        .unwrap();
    assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
    assert!(r.warnings[0].contains("link.txt"));
    assert_eq!(run.paths(), ["src", "src/real.txt"]);
}

#[cfg(target_os = "linux")]
#[test]
fn non_unicode_names_fail_strict_and_are_skipped_with_warning() {
    use std::os::unix::ffi::OsStrExt;
    let run = Run::new();
    let src = run.src();
    put(&src, "ok.txt", b"ok");
    let bad = src.join(std::ffi::OsStr::from_bytes(b"bad\xff"));
    fs::create_dir(&bad).unwrap();
    fs::write(bad.join("inside.txt"), b"hidden").unwrap();

    assert!(matches!(
        run.create(std::slice::from_ref(&src), ErrorPolicy::Strict),
        Err(AcxError::InvalidArgument(_))
    ));
    let r = run
        .create(std::slice::from_ref(&src), ErrorPolicy::Skip)
        .unwrap();
    assert_eq!(
        r.warnings.len(),
        1,
        "the subtree is skipped with one warning"
    );
    assert_eq!(run.paths(), ["src", "src/ok.txt"]);
}

#[cfg(windows)]
#[test]
fn unpaired_surrogate_names_fail_strict_and_are_skipped_with_warning() {
    use std::os::windows::ffi::OsStringExt;
    let run = Run::new();
    let src = run.src();
    put(&src, "ok.txt", b"ok");
    let name = std::ffi::OsString::from_wide(&[u16::from(b'x'), 0xD800]);
    if fs::write(src.join(name), b"bad").is_err() {
        eprintln!("skipping: file system refused the name");
        return;
    }
    assert!(matches!(
        run.create(std::slice::from_ref(&src), ErrorPolicy::Strict),
        Err(AcxError::InvalidArgument(_))
    ));
    let r = run
        .create(std::slice::from_ref(&src), ErrorPolicy::Skip)
        .unwrap();
    assert_eq!(r.warnings.len(), 1);
    assert_eq!(run.paths(), ["src", "src/ok.txt"]);
}

#[cfg(unix)]
#[test]
fn unix_modes_round_trip() {
    use std::os::unix::fs::PermissionsExt;
    let run = Run::new();
    let src = run.src();
    let script = put(&src, "bin/run.sh", b"#!/bin/sh\n");
    let secret = put(&src, "secret.txt", b"s");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(src.join("bin"), fs::Permissions::from_mode(0o750)).unwrap();
    run.create(std::slice::from_ref(&src), ErrorPolicy::Strict)
        .unwrap();
    run.extract();
    let mode = |p: &str| fs::metadata(run.out.join(p)).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode("src/bin/run.sh"), 0o755);
    assert_eq!(mode("src/secret.txt"), 0o600);
    assert_eq!(mode("src/bin"), 0o750);
}

#[cfg(unix)]
#[test]
fn unreadable_entries_follow_the_error_policy() {
    use std::os::unix::fs::PermissionsExt;
    let run = Run::new();
    let src = run.src();
    put(&src, "ok.txt", b"ok");
    let locked_file = put(&src, "locked.txt", b"no");
    let locked_dir = src.join("locked-dir");
    put(&src, "locked-dir/inside.txt", b"no");
    fs::set_permissions(&locked_file, fs::Permissions::from_mode(0o000)).unwrap();
    fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o000)).unwrap();
    let restore = || {
        fs::set_permissions(&locked_dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&locked_file, fs::Permissions::from_mode(0o644)).unwrap();
    };
    if fs::read(&locked_file).is_ok() {
        restore();
        eprintln!("skipping: running with privileges that ignore permissions");
        return;
    }

    let strict = run.create(std::slice::from_ref(&src), ErrorPolicy::Strict);
    assert!(matches!(strict, Err(AcxError::Io { .. })), "{strict:?}");
    let r = run
        .create(std::slice::from_ref(&src), ErrorPolicy::Skip)
        .unwrap();
    restore();
    assert_eq!(r.warnings.len(), 2, "{:?}", r.warnings);
    // The locked directory itself is listed; only its contents are unreadable.
    assert_eq!(run.paths(), ["src", "src/locked-dir", "src/ok.txt"]);
}
