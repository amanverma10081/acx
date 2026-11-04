use std::process::Command;

fn acx(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_acx"))
        .args(args)
        .output()
        .expect("run acx")
}

#[test]
fn version_prints_crate_version() {
    let out = acx(&["version"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim(),
        format!("acx {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn unknown_command_exits_2() {
    assert_eq!(acx(&["frobnicate"]).status.code(), Some(2));
    assert_eq!(acx(&[]).status.code(), Some(2));
    assert_eq!(
        acx(&["create", "x", "-o", "y", "--block-size", "lots"])
            .status
            .code(),
        Some(2)
    );
}

fn s(p: &std::path::Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn create_list_verify_extract_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("notes.txt");
    let data = "hello, archive!\n".repeat(10_000);
    std::fs::write(&input, &data).unwrap();
    let archive = dir.path().join("notes.acx");
    let out = dir.path().join("out");

    let create = acx(&[
        "create",
        s(&input),
        "-o",
        s(&archive),
        "--block-size",
        "64KiB",
        "--threads",
        "3",
        "--memory",
        "64MiB",
        "--verify-dedup",
    ]);
    assert!(create.status.success(), "{create:?}");
    // A 16-byte line divides 64 KiB evenly, so the full blocks are identical.
    assert!(String::from_utf8_lossy(&create.stdout).contains("duplicate block(s) stored once"));
    assert_eq!(
        acx(&["create", s(&input), "-o", s(&archive)]).status.code(),
        Some(2)
    );

    let list = acx(&["list", s(&archive), "--blocks"]);
    assert!(list.status.success());
    let listing = String::from_utf8(list.stdout).unwrap();
    assert!(
        listing.contains("notes.txt") && listing.contains("Zstd"),
        "{listing}"
    );

    for args in [
        &["verify", s(&archive)][..],
        &["verify", "--full", s(&archive)],
    ] {
        assert!(acx(args).status.success(), "{args:?}");
    }
    assert!(
        acx(&["extract", s(&archive), "-o", s(&out), "--threads", "2"])
            .status
            .success()
    );
    assert_eq!(
        std::fs::read_to_string(out.join("notes.txt")).unwrap(),
        data
    );
    assert_eq!(
        acx(&["extract", s(&archive), "-o", s(&out)]).status.code(),
        Some(6)
    );
    assert!(
        acx(&["extract", s(&archive), "-o", s(&out), "--skip-existing"])
            .status
            .success()
    );
}

#[test]
fn damaged_and_invalid_archives_have_distinct_exit_codes() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("a.bin");
    std::fs::write(&input, vec![7u8; 100_000]).unwrap();
    let archive = dir.path().join("a.acx");
    assert!(
        acx(&["create", s(&input), "-o", s(&archive)])
            .status
            .success()
    );

    let mut bytes = std::fs::read(&archive).unwrap();
    bytes[64 + 80] ^= 0xFF; // first payload byte of block 0
    std::fs::write(&archive, &bytes).unwrap();
    let verify = acx(&["verify", s(&archive)]);
    assert_eq!(verify.status.code(), Some(4));
    assert!(
        String::from_utf8(verify.stdout)
            .unwrap()
            .contains("DAMAGED block 0 at offset 64")
    );
    let out = dir.path().join("out");
    assert_eq!(
        acx(&["extract", s(&archive), "-o", s(&out)]).status.code(),
        Some(4)
    );

    std::fs::write(&archive, vec![0u8; 500]).unwrap();
    assert_eq!(acx(&["verify", s(&archive)]).status.code(), Some(3));
    assert_eq!(
        acx(&["list", s(&dir.path().join("missing.acx"))])
            .status
            .code(),
        Some(6)
    );
}

#[test]
fn benchmark_reports_and_writes_json_and_csv() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::write(data.join("a.txt"), "frame\n".repeat(50_000)).unwrap();
    let json = dir.path().join("r.json");
    let csv = dir.path().join("r.csv");
    let out = acx(&[
        "benchmark",
        s(&data),
        "--runs",
        "2",
        "--warmup",
        "0",
        "--block-size",
        "64KiB",
        "--json",
        s(&json),
        "--csv",
        s(&csv),
        "--work-dir",
        s(dir.path()),
    ]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("every run verified: true"), "{text}");
    assert!(text.contains("create   median"), "{text}");
    assert!(
        std::fs::read_to_string(json)
            .unwrap()
            .contains("\"manifest_blake3\"")
    );
    assert_eq!(std::fs::read_to_string(csv).unwrap().lines().count(), 3);
    assert_eq!(
        acx(&["benchmark", s(&data), "--runs", "0"]).status.code(),
        Some(2)
    );
}
