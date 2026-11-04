# ACX — Adaptive Compression eXchange

A CPU-first, lossless media archiver in Rust. Archives restore original
files byte-for-byte (`FileExact`). See [docs/PRD.md](docs/PRD.md) for
requirements, architecture (§2), and the v1 binary format (§4).

**Status:** all PRD tasks (0–8) done: files and directory trees,
compressed and extracted in parallel with a bounded memory budget,
identical blocks stored once, a benchmark harness, and a feature-gated
research crate ([docs/RESEARCH.md](docs/RESEARCH.md)). Output is identical
for any thread count. Tested in CI on Linux, Windows, and macOS.

## Benchmark results

570 MiB reproducible mixed-media corpus (logs, raw screen-capture frames,
incompressible photos/videos, a backup copy of half the photos, 2,000
small files). AMD Ryzen 5 7600X, Windows 11, NVMe, warm cache; median of
3 runs after a warm-up, and every extracted copy verified byte for byte.

| Tool | Archive | Saved | Create | Extract |
| --- | ---: | ---: | ---: | ---: |
| **ACX balanced, 12 threads** (default) | **235.4 MiB** | **58.7%** | 3.95 s | 2.56 s |
| ACX balanced, 1 thread | 235.4 MiB | 58.7% | 5.26 s | 2.77 s |
| ACX fast, 12 threads | 238.9 MiB | 58.1% | 3.71 s | 2.55 s |
| ACX maximum, 12 threads | 231.8 MiB | 59.3% | 26.57 s | 2.54 s |
| zip (Deflate) | 341.4 MiB | 40.1% | 8.01 s | 1.03 s |
| tar.gz (gzip -6) | 340.9 MiB | 40.1% | 7.99 s | 1.04 s |
| tar.zst (zstd -3) | 341.7 MiB | 40.0% | **1.02 s** | 1.07 s |
| tar.xz (xz -6) | 330.0 MiB | 42.1% | 143.08 s | 1.69 s |

- ACX archives are about 31% smaller than zip and 29% smaller than xz,
  mostly because duplicate files and repeated frames are stored once.
- ACX creates 2× faster than zip/gzip. Plain tar.zst creates faster, and
  every baseline extracts about 2.5× faster: ACX checks BLAKE3 for every
  block and file and syncs each file before renaming it into place.

Method, corpus details, a known memory issue with `--mode maximum`, and
how to reproduce: [docs/BENCHMARKS.md](docs/BENCHMARKS.md).

```bash
acx create photos/ video.mkv -o media.acx
```

```bash
acx verify --full media.acx
```

```bash
acx extract media.acx -o restored/
```

`acx list --blocks media.acx` shows the entries and the block index.
Symlinks, unreadable files, and non-Unicode names stop `create` unless
`--skip-errors` is given. `--threads` and `--memory` (default: all logical
CPUs, 1 GiB) bound the work in flight; Ctrl-C stops cleanly and removes
temporary files. The format is specified in
[docs/FORMAT_V1.md](docs/FORMAT_V1.md).

## Benchmarks

```bash
acx benchmark photos/ --runs 5 --warmup 1 --json report.json --csv runs.csv
```

Each run creates and extracts the dataset and checks the extracted tree
against the dataset's manifest (paths, sizes, BLAKE3); warm-up runs are
checked but not reported. Timings are reported as median and MAD. The
report records the dataset's manifest hash, settings (threads, memory,
block size, mode, codec, dedup), CPU, OS, zstd version, and the process's
peak memory (Linux and Windows). `create`'s encoding options all apply.
Use `--work-dir` to measure a particular disk and `--threads 1` for a
single-thread baseline. The exit code is 4 if any run fails verification.

## Layout

| Path                   | Responsibility                                               |
| ---------------------- | ------------------------------------------------------------ |
| `crates/acx-core`      | Shared types, options, resource budget, errors               |
| `crates/acx-container` | v1 binary format read/write; no codecs or filesystem walking |
| `crates/acx-codecs`    | RAW and Zstandard block codecs                               |
| `crates/acx-archive`   | Scan, dedup, schedule, create, extract, verify               |
| `crates/acx-bench`     | Benchmark runs, statistics, JSON/CSV reports                 |
| `crates/acx-research`  | Experimental reversible transforms (`--features experimental`) |
| `benchmarks/`          | Corpus generator, comparison script, published reports       |
| `apps/acx-cli`         | `acx` binary                                                 |

Dependencies flow `acx-cli →
acx-bench → acx-archive → acx-container / acx-codecs → acx-core`.

## Build and check

The toolchain is pinned in `rust-toolchain.toml`; rustup installs it
automatically.

```bash
cargo build --workspace
```

```bash
cargo fmt --all --check
```

```bash
cargo clippy --workspace --all-targets -- -D warnings
```

```bash
cargo test --workspace
```

```bash
cargo run -p acx-cli -- version
```

CI runs the same checks, plus release-mode tests, on Linux and Windows
(`.github/workflows/ci.yml`).

## License

MIT OR Apache-2.0
