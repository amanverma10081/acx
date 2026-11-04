# ACX — Adaptive Compression eXchange

A CPU-first, lossless media archiver in Rust. Archives restore original
files byte-for-byte (`FileExact`). See [docs/PRD.md](docs/PRD.md) for
requirements, architecture (§2), and the v1 binary format (§4).

**Status:** all PRD tasks (0–8) done: files and directory trees,
compressed and extracted in parallel with a bounded memory budget,
identical blocks stored once, a benchmark harness, and a feature-gated
research crate ([docs/RESEARCH.md](docs/RESEARCH.md)). Output is identical
for any thread count. Tested in CI on Linux, Windows, and macOS.

On a 570 MiB mixed-media corpus ACX's archive is 31% smaller than zip's
and created 2× faster; see [docs/BENCHMARKS.md](docs/BENCHMARKS.md) for
the full comparison, including where plain tar.zst is faster.

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
