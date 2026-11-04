# ACX benchmarks

## Mixed-media corpus, 2026-11-04

**Machine:** AMD Ryzen 5 7600X (6 cores, 12 threads), Windows 11 Pro,
NVMe SSD, warm page cache. ACX 0.1.0 (release build, zstd 1.5.7).
Baselines: bsdtar 3.8.9 / libarchive (zlib 1.2.13, liblzma 5.8.3,
libzstd 1.5.7), the `tar.exe` shipped with Windows.

**Dataset:** `python benchmarks/make_corpus.py OUT_DIR`, 2,161 files,
597,231,799 bytes (570 MiB). ACX manifest hash
`e488481737d91dce60088fa61dff76c43a5e9bcc875da1c23272aa726958c77e`. If
yours differs, the numbers are not comparable.

| Part | Size | Stands in for |
| --- | ---: | --- |
| `logs/` | 98 MiB | server logs, CSV catalogs (very compressible) |
| `frames/` | 159 MiB | raw 1280×720 RGB screen capture, mostly static, every 4th frame an exact repeat |
| `photos/` | 209 MiB | already-compressed media (JPEG/HEIC/MP4): incompressible |
| `backup/` | 102 MiB | exact copies of half the photos |
| `small/` | 8 MiB | 2,000 small metadata files |

**Method:** each tool runs 1 warm-up and 3 measured runs; times are
medians of wall-clock create and extract. Every extracted tree, warm-up
included, was compared file by file with the dataset, and all matched.
Reproduce with `python benchmarks/compare.py`, which writes
[`benchmarks/reports/mixed-corpus-ryzen5-7600x.json`](../benchmarks/reports/mixed-corpus-ryzen5-7600x.json).

| Tool | Archive | Saved | Create | Extract | Peak RSS |
| --- | ---: | ---: | ---: | ---: | ---: |
| **ACX balanced, 12 threads** (default) | **235.4 MiB** | **58.7%** | 3.95 s | 2.56 s | 300 MiB |
| ACX balanced, 1 thread | 235.4 MiB | 58.7% | 5.26 s | 2.77 s | 53 MiB |
| ACX fast, 12 threads | 238.9 MiB | 58.1% | 3.71 s | 2.55 s | 121 MiB |
| ACX maximum, 12 threads | 231.8 MiB | 59.3% | 26.57 s | 2.54 s | 1,145 MiB |
| zip (Deflate) | 341.4 MiB | 40.1% | 8.01 s | 1.03 s | – |
| tar.gz (gzip -6) | 340.9 MiB | 40.1% | 7.99 s | 1.04 s | – |
| tar.zst (zstd -3, 1 thread) | 341.7 MiB | 40.0% | **1.02 s** | 1.07 s | – |
| tar.xz (xz -6) | 330.0 MiB | 42.1% | 143.08 s | 1.69 s | – |

Peak RSS is reported by `acx benchmark`; baselines run in separate
processes and were not measured.

### What this shows

- **Size: ACX wins clearly**: 106 MiB (31%) smaller than zip and 95 MiB
  smaller than xz. Most of the gap is dedup. The 102 MiB `backup/` copy and
  the repeated frames are stored once, which no stream archiver can do.
- **Speed vs zip/gzip: ACX creates 2× faster** and archives 30% smaller.
- **Speed vs plain tar.zst: ACX is slower.** tar.zst creates in 1.0 s
  against 4.0 s. ACX hashes every block twice (block and file BLAKE3),
  tries Zstandard on the 311 MiB of incompressible photos before storing
  them RAW, uses a higher level (9 vs 3), and syncs the archive to disk.
  That trial is the known shortcut that adaptive routing (PRD M4) removes.
- **Extract: ACX is about 2.5× slower than every baseline** (2.6 s vs
  1.0 s). It checks each block's and each file's BLAKE3 and fsyncs every
  file before renaming it into place; the baselines check a CRC at most and
  never sync. The single writer thread is the bottleneck.
- **Threads help less here than on compressible data** (1.3× vs 2.8× on
  build output in an earlier run), because most CPU time is the wasted
  trial on incompressible data plus single-threaded reading and hashing.

### Known issue found by this run

`--mode maximum` peaked at **1,145 MiB**, over the default 1 GiB budget.
The budget counts block buffers only; Zstandard level 19 also allocates a
large match-finder context per worker (about 70 MiB each, 12 workers).
Until the budget accounts for codec memory, use `--threads` or
`--memory` to stay within limits on maximum mode. Balanced and fast stay
well under budget.

### Not covered

Cold-cache runs, other machines and operating systems, real camera
footage and photo libraries, and the media-specific baselines in PRD
§7.1 (JPEG XL, FFV1, FLAC, 7-Zip).
