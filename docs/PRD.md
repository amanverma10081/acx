# ACX --- Adaptive Compression eXchange

## Implementation-Ready PRD, Technical Design & Cursor Instructions

**Document version:** 1.1\
**Status:** Architecture baseline / implementation specification\
**Initial platforms:** Windows x86-64 and Linux x86-64\
**Future platform:** macOS\
**Primary language:** Rust\
**Product class:** Lossless media archiver and compression research
platform\
**Primary goals:** Strong compression, high throughput, exact
preservation, bounded memory, reliable recovery

## Changes in 1.1

-   Manifest moved after the data blocks so a single-pass sequential
    writer can produce it; the footer is now the authoritative locator
    for manifest and index.
-   Consistent field widths: Block ID, file ID and all counts are `u32`;
    all byte offsets and lengths are `u64`. Files larger than 4 GiB are
    representable.
-   Fixed-size chunking is P0 (bounded memory). Content-defined chunking
    stays P1.
-   Reference blocks removed. Dedup is expressed by several manifest
    references pointing to the same stored block.
-   CRC ranges, record lengths, required/optional flags, and the
    optional record bit are defined precisely. Redundant `raw` block
    flag removed.
-   Compression acceptance rule corrected and made overflow-safe.
-   Manifest gains entry types (file, directory), Unix mode bits, and
    signed mtime. Paths are stored as given, never normalized.
-   Extraction rules for Windows names, case collisions, long paths, and
    dedup amplification added. Archive creation is atomic.
-   Crates reduced to five for v0.x; single-implementation traits
    removed.
-   Deprecated or unneeded dependencies removed; CLI limited to options
    that are implemented.

------------------------------------------------------------------------

# 0. Executive Summary

ACX is a CPU-first, adaptive, lossless media archive engine designed for
image, video, audio, and mixed media collections. It combines
established codecs, content-aware routing, optional exact deduplication,
parallel processing, and a custom versioned archive container.

ACX must not claim that one algorithm can beat every established codec
on all data. Its advantage is system-level: select the appropriate
reversible representation and codec, avoid pointless recompression,
detect exact duplicate content, exploit parallelism, and preserve
original files whenever possible.

## Product split

### ACX Archive (production path)

-   Archives original files byte-for-byte by default.
-   May store exact duplicate files/chunks as references.
-   May use lossless transformations only when the selected preservation
    contract permits them.
-   Supports parallel compression/extraction, integrity verification,
    bounded memory, and recovery from damaged independent blocks.
-   Uses a versioned `.acx` container.

### ACX Research Codec (experimental path)

-   Experiments with reversible image transforms and temporal video
    prediction.
-   Is not enabled in the stable archive path until it passes
    correctness, compatibility, and benchmark gates.
-   Never uses perceptual hashes as proof of equality.
-   Never uses lossy transforms in lossless mode.

## Non-goals for v1

-   No lossy compression.
-   No proprietary or undocumented codec format.
-   No GPU dependency.
-   No cloud service or account.
-   No archive encryption in v1.
-   No claim of byte-identical reconstruction after transcoding unless
    the original bitstream is explicitly stored or a codec supports
    exact bitstream reconstruction.
-   No Android implementation in the initial release.
-   No custom image/video codec required for the first usable release.

------------------------------------------------------------------------

# 1. Product Requirements

## 1.1 Target users and use cases

-   Users archiving photo libraries and video collections.
-   Users preserving screen recordings and desktop capture.
-   Users maintaining media backups with repeated files.
-   Developers benchmarking lossless media compression.
-   Users who need exact restoration and archive verification.

## 1.2 Preservation contracts

ACX exposes two explicit contracts:

### `FileExact` (default)

The original file bytes are reconstructed exactly. ACX may: - Store the
original file as a raw payload. - Store the original file through a
lossless generic codec. - Store exact duplicate files or chunks as
references to previously stored payloads.

ACX must not silently transcode the source file.

### `MediaExact` (opt-in)

Decoded media samples must be reconstructed exactly under a documented
sample-level contract. Metadata preservation must be separately
declared. A transcode is permitted only if: - The decoder/encoder path
is demonstrably lossless for the relevant sample representation. - The
output is verified against the defined sample comparison. - The archive
records the transformation and original media properties. - The user
explicitly chose this contract.

**V1 default:** `FileExact`. The first release should not transcode
original media files.

## 1.3 Functional requirements

  ID       Requirement                                         Priority
  -------- --------------------------------------------------- ----------
  FR-001   Archive files and directory trees                   P0
  FR-002   Restore original file bytes                         P0
  FR-003   Verify archive integrity                            P0
  FR-004   Support parallel compression and extraction         P0
  FR-005   Bound memory through configurable limits            P0
  FR-006   Detect exact duplicate files and blocks             P0
  FR-007   Store file manifest and metadata                    P0
  FR-008   Support independent fixed-size data blocks          P0
  FR-009   Support raw storage fallback                        P0
  FR-010   Support Zstandard payload compression               P0
  FR-011   Support optional LZMA2 payload compression          P1
  FR-012   Support content-defined chunk deduplication         P1
  FR-013   Support partial file extraction                     P1
  FR-014   Detect corrupted blocks and report locations        P0
  FR-015   Support deterministic benchmark runs                P0
  FR-016   Support resume after interrupted archive creation   P2
  FR-017   Support optional lossless media transcode           P3
  FR-018   Support macOS                                       P2

## 1.4 Non-functional requirements

-   **Correctness:** exact file-byte reconstruction under `FileExact`.
-   **Reliability:** malformed archives must fail safely; no silent
    corruption.
-   **Security:** validate all sizes, offsets, path components, and
    integer arithmetic; prevent path traversal on extraction.
-   **Memory:** configurable global memory budget; bounded queues.
-   **Concurrency:** no nested unbounded thread pools.
-   **Portability:** stable Rust where possible; native codec
    dependencies isolated.
-   **Observability:** structured logs, progress events, benchmark
    metrics.
-   **Compatibility:** archive format versioned and documented.
-   **Performance:** benchmark against appropriate baselines; no
    arbitrary universal speed/ratio guarantee.

------------------------------------------------------------------------

# 2. Architecture

## 2.1 Data flow

``` text
CLI / API
  |
  v
Input Scanner ----> File Manifest
  |
  v
Content Analyzer
  |-- file signature / media probe
  |-- size and entropy sampling
  |-- duplicate detection
  |-- codec eligibility
  |
  v
Adaptive Scheduler
  |-- raw passthrough
  |-- Zstandard
  |-- optional LZMA2
  |-- reuse of an identical earlier block
  |
  v
Bounded worker queue
  |
  v
ACX Container Writer (single pass, temp file + rename)
  |-- header
  |-- data blocks
  |-- manifest
  |-- block index
  |-- footer
  |
  v
ACX archive
```

Extraction:

``` text
ACX archive
  |
  v
Header / footer validation
  |
  v
Manifest + index
  |
  v
Dependency validation
  |
  v
Parallel block decode
  |
  v
Checksum verification
  |
  v
Safe filesystem reconstruction
```

## 2.2 Crate layout

Five crates for v0.x. `acx-container` stays separate so the parser and
its fuzz targets have no codec or filesystem dependencies. The
remaining responsibilities in §2.3 are modules of `acx-archive`, not
crates. `acx-bench` is added with TASK 7 and `acx-research` with TASK 8;
neither is created earlier.

``` text
acx/
├── Cargo.toml
├── rust-toolchain.toml
├── crates/
│   ├── acx-core/        # shared types, errors, options, limits
│   ├── acx-container/   # binary format read/write (no codecs, no fs walk)
│   ├── acx-codecs/      # PayloadCodec, RAW, Zstd (LZMA2 later)
│   ├── acx-archive/     # scanner, analyzer, dedup, scheduler,
│   │                    # create, extract, verify modules
│   ├── acx-bench/       # added in TASK 7
│   └── acx-research/    # added in TASK 8, feature-gated
├── apps/
│   └── acx-cli/
├── tests/
│   ├── fixtures/
│   ├── roundtrip/
│   ├── corruption/
│   ├── security/
│   └── integration/
├── benchmarks/
│   ├── images/
│   ├── video/
│   ├── audio/
│   ├── mixed/
│   └── reports/
├── docs/
│   ├── PRD.md
│   ├── CURSOR_RULES.md
│   ├── FORMAT_V1.md
│   ├── CODECS.md
│   ├── BENCHMARKS.md
│   └── SECURITY.md
└── README.md
```

## 2.3 Module specifications

### `acx-core`

**Responsibility:** shared types, configuration, errors, limits,
progress events.

Public types: - `ArchiveOptions` - `ExtractOptions` - `VerifyOptions` -
`PreservationMode` - `CompressionMode` - `CodecId` - `BlockId` -
`FileId` - `AcxError` - `ProgressEvent` - `ResourceBudget`

Rules: - No filesystem traversal logic. - No codec implementation. - No
archive byte parsing beyond shared primitives. - Errors must be typed
and actionable.

Single-implementation traits are not introduced. A trait is added only
when a second implementation exists.

### `acx-archive::scanner`

**Responsibility:** safely scan paths and produce a manifest.

API:

``` rust
pub fn scan(roots: &[PathBuf], options: &ScanOptions)
    -> Result<ScanManifest, AcxError>;
```

Must: - Support files and directories, including empty directories. -
Preserve relative paths exactly as the OS reports them; never apply
Unicode normalization. - Record file size, mtime, and Unix mode bits
where available. - Avoid following symlinks by default. - v1 rejects
symlinks (strict policy) or skips them (skip policy); symlink entries
are reserved for a later version. - Reject paths that are not valid
Unicode (non-UTF-8 bytes on Unix, unpaired surrogates on Windows) under
strict policy, or skip them under skip policy. - Detect file signatures
by content, not extension alone. - Handle permission errors according to
strict/skip policy. - Reject exact path collisions and invalid archive
paths.

### `acx-archive::analyzer`

**Responsibility:** estimate compression opportunities without modifying
data.

API:

``` rust
pub fn analyze(input: &AnalysisInput, budget: &AnalysisBudget)
    -> Result<AnalysisReport, AcxError>;
```

Analysis report fields: - `detected_media_type` - `signature` -
`sampled_entropy` - `sampled_compressibility` -
`already_compressed_hint` - `duplicate_candidate` -
`recommended_codec` - `confidence` - `analysis_cost`

Rules: - Sampling is bounded. - Entropy is a heuristic, not a
guarantee. - No perceptual similarity may be used for exact
deduplication. - The analyzer must not decode full videos during default
archive creation.

### `acx-codecs`

**Responsibility:** normalized codec interface and implementations.

``` rust
pub trait PayloadCodec: Send + Sync {
    fn id(&self) -> CodecId;
    fn name(&self) -> &'static str;
    fn compress(
        &self,
        input: &[u8],
        options: &CodecOptions,
    ) -> Result<Vec<u8>, CodecError>;
    fn decompress(
        &self,
        input: &[u8],
        expected_size: usize,
        options: &CodecOptions,
    ) -> Result<Vec<u8>, CodecError>;
}
```

Initial implementations: - `RawCodec`: identity; always available. -
`ZstdCodec`: primary general-purpose codec. - `Lzma2Codec`: optional P1
codec.

JXL and FFV1 adapters live in `acx-research`, not `acx-codecs`, until
they pass preservation tests.

Input to `compress` is one block, never a whole file. Block size is
bounded by the archive's configured block size (FORMAT_V1 §6), so codec memory
is bounded.

Codec requirements: - Report output size and encode/decode time. -
Validate expected decoded size. - Return errors on truncated/corrupt
input. - Never silently return partial output. - Codec IDs must be
stable in format specification. - Native library versions recorded in
archive metadata only as optional informational fields; decoder must not
require exact version unless codec compatibility requires it.

### `acx-archive::dedup`

**Responsibility:** exact block deduplication.

P0 (fixed-size blocks): - Files are split into fixed-size blocks
starting at offset 0. - Each block is keyed by (decoded length, BLAKE3 of
decoded bytes). - If the key was already stored, the manifest reference
points at the existing Block ID and no new block is written. - Identical
files therefore share all of their blocks, which covers whole-file
dedup with a single read of each file. - The index is an in-memory
`HashMap<(u64, [u8; 32]), BlockId>`; at ~48 bytes per entry, 1 TiB of
input at 16 MiB blocks needs about 3 MiB. - BLAKE3-256 equality is
treated as proof of equality by default. `--verify-dedup` additionally
re-reads and byte-compares the earlier block before reuse.

P1: - Content-defined chunking (CDC) to find duplicates at shifted
offsets. - Bounded or disk-backed index for very large inputs.

There are no reference blocks and no reference graph. Sharing is
expressed only through manifest references (FORMAT_V1 §7).

### `acx-archive::scheduler`

**Responsibility:** parallel task execution and resource control.

Must: - Use a single global concurrency budget. - Avoid nested unbounded
codec thread pools. - Use bounded queues. - Support cancellation. -
Respect configured memory limit. - Separate CPU tasks and archive
writer. - Keep archive output deterministic where requested: Block IDs
are assigned in write order, and blocks are written in (manifest order,
offset within file) order regardless of which worker finishes first.

Initial implementation: - Rayon for CPU tasks. - Dedicated writer thread
or bounded channel. - `rayon::ThreadPoolBuilder` with explicit thread
count. - Per-codec internal threading disabled or coordinated with
global budget.

### `acx-container`

**Responsibility:** binary format serialization and parsing.

Must: - Implement ACX v1 header, records, index, footer. - Use explicit
little-endian serialization. - Never serialize Rust structs by raw
memory layout. - Validate all offsets, lengths, counts, and
arithmetic. - Enforce configured maximum sizes before allocation. -
Support sequential writer and random-access reader. - Maintain format
documentation and golden fixtures.

### `acx-archive::create`

**Responsibility:** orchestration of scan, analysis, scheduling,
compression, deduplication, and container writing.

API:

``` rust
pub fn create_archive(
    roots: &[PathBuf],
    output: &Path,
    options: &ArchiveOptions,
    progress: impl Fn(ProgressEvent) + Send + Sync,
) -> Result<ArchiveReport, AcxError>;
```

Must: - Write to a temporary file in the output directory, flush and
sync it, then rename over `output`. An interrupted run never leaves a
partial archive at `output` and never destroys an existing archive. -
Read each file once: the file BLAKE3 and every block BLAKE3 are computed
from the same bytes that are compressed. - If the number of bytes read
differs from the scanned size, fail under strict policy (the file
changed during archiving) or record the bytes actually read under skip
policy, with a warning.

### `acx-archive::extract`

**Responsibility:** safe extraction.

Must: - Validate archive before writing. - Reject absolute paths, drive
paths, UNC paths, and `..` traversal. - Prevent symlink escape,
including through symlinks that already exist inside the destination
directory. - Support overwrite policy. - Decode independent blocks in
parallel. - Validate checksums before committing files. - Write
temporary files then atomically rename where supported. - Enforce a
total output size limit computed from the manifest (sum of file sizes,
so a block shared by N files counts N times) before writing anything,
and check free space where practical. - On Windows: reject reserved
device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`,
with or without an extension), names ending in `.` or space, and the
characters `<>:"|?*` and U+0000–U+001F; use `\\?\` paths so names over
260 characters work. - On Windows and macOS: reject the archive before
writing if two paths are equal after Unicode simple case folding,
because one would overwrite the other.

### `acx-archive::verify`

**Responsibility:** integrity and correctness.

Modes: - `Quick`: validate header, footer, index, and block checksums. -
`Full`: decompress all blocks and validate decoded checksums. -
`Compare`: compare restored tree against original tree using size and
BLAKE3.

### `acx-bench`

**Responsibility:** reproducible codec and archive evaluation.

Must record: - Dataset identifier and manifest hash. - Source bytes. -
Archive bytes. - Compression and decompression time. - Throughput. -
Peak memory. - CPU/thread configuration. - Codec and library versions. -
Run count and summary statistics. - Correctness status.

### `acx-research`

**Responsibility:** experimental reversible transformations.

Modules: - `image_predictors` - `color_transforms` - `frame_hashing` -
`block_matching` - `temporal_residuals` - `experimental_entropy`

All research APIs must be feature-gated and excluded from stable archive
behavior until approved.

------------------------------------------------------------------------

# 3. Codec Integration API and Routing

## 3.1 V1 codec identifiers

Stable IDs (do not reuse): - `0x0000`: RAW - `0x0001`: Zstandard -
`0x0002`: LZMA2 - `0x0100+`: media codecs / transformations reserved -
`0x8000+`: experimental codecs

Unknown codec ID must produce a clear unsupported-codec error, not a
fallback to another decoder.

## 3.2 Default routing

  -----------------------------------------------------------------------
  Input condition                     V1 action
  ----------------------------------- -----------------------------------
  File already compressed and         RAW or Zstd only if gain threshold
  sampling shows no gain              passes

  Any file where compressed output is RAW
  larger                              

  Exact duplicate file or block       Reuse existing Block ID in
                                      manifest

  Unknown media                       Zstd trial

  Small file                          Zstd or RAW based on overhead and
                                      measured gain

  Any file larger than block size     Split into fixed-size blocks and
                                      compress in parallel

  Existing JPEG/MP4/MKV               Preserve bytes; generic compression
                                      only if it demonstrably saves space

  Raw image/video                     Store original bytes in v1;
                                      research adapters later
  -----------------------------------------------------------------------

Default compression acceptance rule (per block): - Keep the compressed
payload only if `compressed_size + min_saving <= original_size`,
evaluated with checked `u64` arithmetic (an overflow means reject). -
Block framing overhead is identical for RAW and compressed blocks, so it
is not part of the comparison. - Default `min_saving`: configurable;
initial 32 bytes or 1% of `original_size`, whichever is larger. Blocks
smaller than `min_saving` are therefore always RAW. - Never choose a
compressed payload solely from sampled entropy if actual output is
larger.

## 3.3 Zstandard API

Use the Rust `zstd` crate initially. Pin a compatible version in
workspace dependencies and update only with regression tests.

Example wrapper:

``` rust
pub struct ZstdCodec {
    level: i32,
}

impl PayloadCodec for ZstdCodec {
    fn id(&self) -> CodecId { CodecId::Zstd }

    fn compress(
        &self,
        input: &[u8],
        options: &CodecOptions,
    ) -> Result<Vec<u8>, CodecError> {
        // Use configured level and bounded output handling.
        // Return a complete frame.
        todo!("implement with zstd crate")
    }

    fn decompress(
        &self,
        input: &[u8],
        expected_size: usize,
        options: &CodecOptions,
    ) -> Result<Vec<u8>, CodecError> {
        // Enforce expected_size and decompression limits.
        todo!("implement with zstd crate")
    }
}
```

Do not leave `todo!()` in shipped code. This snippet illustrates
interface only.

## 3.4 LZMA2 integration

P1. Use a maintained Rust binding or native library with documented
licensing and Windows/Linux build support. Isolate it behind
`PayloadCodec`. Avoid introducing a dependency into core/container
crates.

## 3.5 Media codec policy

-   FFmpeg is an optional media probe and research dependency.
-   Do not require FFmpeg to archive arbitrary files.
-   V1 must not transcode media under `FileExact`.
-   JXL and FFV1 adapters are experimental until preservation tests are
    defined.
-   External process invocation must use argument arrays, never shell
    interpolation.
-   Record tool version and command parameters for reproducibility.

------------------------------------------------------------------------

# 4. ACX Binary Format V1

## 4.1 Design principles

-   Explicit little-endian fields.
-   Fixed magic and version.
-   No native struct serialization.
-   Independent payload blocks.
-   Central index for random access.
-   Checksums for header, block payload, manifest, and index.
-   Unknown optional records may be skipped only if length-delimited.
-   Unknown required features must fail closed.
-   All sizes and offsets validated before allocation or seeking.

## 4.2 Canonical specification

The complete v1 format (layout, header, footer, record framing,
manifest, data block, index, codec IDs, validation order, and evolution
rules) is specified in `docs/FORMAT_V1.md`. That document is canonical;
this section only states the principles above. Key decisions:

-   Layout: header, data blocks, manifest, index, footer. The header
    holds no offsets; the footer locates manifest and index.
-   Block IDs, file IDs, and counts are `u32`; offsets and lengths are
    `u64`.
-   Dedup is expressed only by manifest references sharing a Block ID.
-   CRC32C protects header, footer, manifest, index, and encoded
    payloads; BLAKE3 protects decoded blocks and whole files.

------------------------------------------------------------------------

# 5. Rust Workspace Dependencies

Use a workspace and central dependency declarations. Pin exact versions
in `Cargo.lock`; the versions below are initial compatibility targets,
not permission to ignore security updates.

## 5.1 Workspace manifest

``` toml
[workspace]
resolver = "2"
members = [
  "crates/acx-core",
  "crates/acx-container",
  "crates/acx-codecs",
  "crates/acx-archive",
  "apps/acx-cli",
  # "crates/acx-bench",    added in TASK 7
  # "crates/acx-research", added in TASK 8
]

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "MIT OR Apache-2.0"
rust-version = "1.85"

[workspace.dependencies]
anyhow = "1"
thiserror = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
clap = { version = "4", features = ["derive"] }
rayon = "1"
crossbeam-channel = "0.5"
zstd = "0.13"
blake3 = "1"
crc32c = "0.6"
walkdir = "2"
globset = "0.4"
filetime = "0.2"
tempfile = "3"
indicatif = "0.17"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
criterion = "0.5"
proptest = "1"
arbitrary = { version = "1", features = ["derive"] }
```

## 5.2 Dependency rules

-   `acx-core` should have minimal dependencies.
-   Not used: `serde_yaml` (deprecated upstream), `memmap2` (mapping
    input files that may change during archiving risks SIGBUS and torn
    reads), `bytes` and `ignore` (not needed; `walkdir` plus `globset`
    cover scanning and `--exclude`).
-   Only add a dependency in the task that first uses it.
-   Container parsing must not depend on FFmpeg/JXL.
-   Native codec bindings stay in `acx-codecs` or research crates.
-   CLI depends on archive/extractor/verifier, not implementation
    internals.
-   Benchmark crate may depend on all codecs.
-   Avoid unnecessary async runtime in v1; synchronous I/O plus bounded
    worker pools is sufficient.
-   Use `tracing` rather than ad hoc prints.
-   Use `thiserror` for library errors and `anyhow` only at CLI
    boundaries.

## 5.3 Native codec build policy

-   Zstandard is the first required native/generic codec.
-   LZMA2 is optional.
-   FFmpeg is optional and not required to build the archive engine.
-   JXL is optional and not required to build the archive engine.
-   CI must have a minimal build that works without FFmpeg or JXL
    installed.
-   Windows/Linux CI must test both debug and release builds.

------------------------------------------------------------------------

# 6. CLI Specification

## 6.1 Commands

``` text
acx create <INPUT...> --output <ARCHIVE>
acx extract <ARCHIVE> --output <DIR>
acx list <ARCHIVE> [--blocks]
acx verify <ARCHIVE> [--full]
acx benchmark <DATASET> [--codec <CODEC>]   # added in TASK 7
acx version
```

`list --blocks` also prints the block index (codec, sizes, offsets),
covering what a separate `inspect` command would. `verify` without
`--full` is the quick check, so there is no separate `test` command.

## 6.2 Create options

``` text
--mode fast|balanced|maximum
--threads <N>
--memory <SIZE>
--block-size <SIZE>
--codec auto|raw|zstd
--dedup none|blocks
--verify-dedup
--overwrite
--skip-errors        # skip policy: warn about and leave out unreadable
                     # files, symlinks, and non-Unicode names
--exclude <GLOB>
--quiet
--json-progress
```

Defaults: - mode: balanced (zstd level 1 for fast, 9 for balanced, 19
for maximum) - codec: auto - dedup: blocks - block size: 16 MiB -
memory: 1 GiB - threads: logical CPU count, reduced so that
`threads × 3 × block size` fits the memory budget (at least 1) -
preservation: always FileExact

Options are added only when they are implemented: `--codec lzma2` with
LZMA2 (P1), `--dedup chunks` with CDC (P1), `--follow-symlinks` with
symlink support, and `--preserve media-exact` with MediaExact (P3).

## 6.3 Extract options

``` text
--overwrite
--skip-existing
--threads <N>
--memory <SIZE>
--max-output <SIZE>
--select <GLOB>      # P1, FR-013
```

Safe path validation is always enabled and has no switch.
`--max-output` defaults to 1 TiB. Extraction is refused before writing
anything if the manifest's total file size exceeds it.

## 6.4 Exit codes

-   `0`: success
-   `1`: generic failure
-   `2`: invalid arguments
-   `3`: invalid archive
-   `4`: integrity failure
-   `5`: unsupported codec/version
-   `6`: permission or filesystem failure
-   `7`: resource limit exceeded
-   `8`: cancellation

------------------------------------------------------------------------

# 7. Benchmark Methodology

## 7.1 Baselines

### Images

-   JPEG XL lossless / JPEG reconstruction where applicable.
-   WebP lossless.
-   PNG + Oxipng/Zopfli.
-   Zstandard and 7z for original-file archive comparisons.

### Video

-   FFV1.
-   Lossless AV1 mode where verified.
-   Zstandard/7z on original file bytes.
-   ACX raw/passthrough.
-   ACX experimental temporal codec.

### Audio

-   FLAC.
-   WavPack lossless where supported.
-   Zstandard/7z on original file bytes.

### Archive

-   7-Zip (7z and ZIP).
-   Zstandard.
-   XZ where appropriate.

## 7.2 Test corpus

Create a manifest with license and SHA-256/BLAKE3 hashes for: - RAW
photographs. - JPEG, PNG, WebP, TIFF. - High-bit-depth and HDR images. -
Screenshots, UI images, diagrams, pixel art. - H.264/H.265/AV1 video. -
Uncompressed RGB/YUV video. - Screen recordings with static regions. -
High-motion footage. - Repeated-frame animations. - WAV/PCM and FLAC. -
Mixed folders with duplicates. - Synthetic low-entropy, high-entropy,
repetitive and random data.

Never redistribute benchmark data without permission. Store dataset
acquisition instructions and hashes.

## 7.3 Run protocol

-   Record CPU model, core count, RAM, OS, filesystem, compiler, build
    flags, codec versions.
-   Run one warm-up.
-   Run at least five measured repetitions for standard tests; more for
    noisy results.
-   Report median and dispersion (MAD or confidence interval).
-   Separate cold-cache and warm-cache tests.
-   Separate single-thread and all-core tests.
-   Separate compression and decompression.
-   Separate source-media transcode tests from original-file archive
    tests.
-   Do not compare different preservation contracts.
-   Include archive overhead and dedup index size.
-   Verify output after every benchmark run.
-   Never report a speed result without reporting output size.

## 7.4 Metrics

-   Source bytes.
-   Archive bytes.
-   Compression ratio.
-   Percentage saved.
-   Encode MB/s.
-   Decode MB/s.
-   Wall-clock latency.
-   CPU utilization.
-   Peak RSS/working set.
-   Energy if hardware measurement is available.
-   Files/second.
-   Dedup hit rate.
-   Bytes saved through dedup.
-   Verification result.

## 7.5 Benchmark gates

No universal ratio target is imposed before baseline measurement.

A codec or routing change is accepted only if: - It passes exact
reconstruction tests. - It does not regress security or archive
compatibility. - It improves a defined workload without unacceptable
regressions elsewhere. - Its tradeoff is documented. - Results are
reproducible.

------------------------------------------------------------------------

# 8. Testing Strategy

## 8.1 Unit tests

-   Header serialization/deserialization.
-   Record framing.
-   Manifest path validation.
-   Codec roundtrips.
-   Checksum calculation.
-   Dedup lookup and collision handling.
-   Resource budget arithmetic.
-   Scheduler cancellation.
-   Offset/length overflow handling.

## 8.2 Property tests

Use `proptest`: - Arbitrary byte input -\> archive -\> extract -\> exact
bytes. - Arbitrary valid paths -\> encode/decode manifest. - Random
block sequences -\> index roundtrip. - Random chunk boundaries -\> exact
reconstruction. - Codec output always decodes to original for supported
input. - Corrupted lengths/offsets never panic or allocate unbounded
memory.

## 8.3 Integration tests

-   Empty directory.
-   Empty file.
-   One file.
-   Large file.
-   Nested directories.
-   Duplicate files.
-   Unicode paths.
-   Spaces and unusual characters.
-   Permission denied.
-   Interrupted output.
-   Existing destination behavior.
-   Parallel extraction.
-   Archive with RAW and Zstd blocks.
-   Archive with shared (deduplicated) blocks, within one file and
    across files.
-   File exactly one block, one byte over, and larger than 4 GiB
    (sparse or generated).
-   File modified during archiving.
-   Interrupted create leaves no file at the output path and keeps an
    existing archive intact.
-   Windows path edge cases, including reserved names, trailing dots,
    long paths, and `a.txt`/`A.txt` collisions.
-   Linux symlink edge cases, including a pre-existing symlink inside
    the extraction directory.
-   NFC and NFD forms of the same name round-trip unchanged on Linux.

## 8.4 Corruption tests

Mutate: - Header magic. - Header CRC. - Required flags. - Manifest
length. - Manifest path. - Manifest reference Block ID, offset, and
length. - Block codec ID. - Block compressed size. - Block header field
that disagrees with the index. - Block checksum. - Index offsets. -
Footer CRC. - Unknown record type with and without the optional bit. -
Truncate at every structural boundary. - Append trailing garbage.

Expected behavior: clear error, no unsafe extraction, no panic, no
unbounded allocation.

## 8.5 Security tests

-   Path traversal (`../`, absolute paths, drive paths).
-   Windows reserved names.
-   Symlink escape.
-   Integer overflow.
-   Decompression bombs, including one block referenced by many files.
-   Excessive file count.
-   Excessive manifest length.
-   Manifest reference to a nonexistent block or out-of-range slice.
-   Duplicate or out-of-order block IDs.
-   Duplicate paths and case-folded path collisions.
-   Invalid UTF-8.
-   Resource budget exhaustion.

## 8.6 Fuzzing

Fuzz targets: - `parse_header` - `parse_record` - `parse_manifest` -
`parse_index` - `parse_footer` - `extract_path` - codec wrappers with
malformed input

Run fuzzers in CI for a bounded time and longer in scheduled jobs.

------------------------------------------------------------------------

# 9. Milestones and Release Plan

## M0 --- Repository foundation (Week 1)

-   Workspace.
-   CI. 
-   Coding rules.
-   Typed errors.
-   CLI skeleton.
-   Format specification draft.

Exit criteria: - Windows/Linux compile. - `cargo fmt --check`. -
`cargo clippy --workspace --all-targets -- -D warnings`. - Unit test
workflow passes.

## M1 --- Single-file archive (Weeks 2--3)

-   Header/footer.
-   Manifest serialization (file entries).
-   RAW block.
-   Zstandard block.
-   Fixed-size blocks.
-   Create/extract/verify.
-   Exact byte roundtrip.

Exit criteria: - Golden archive fixture. - Corruption detection. - No
unsafe path extraction.

## M2 --- Directory archive (Weeks 4--5)

-   Scanner.
-   Directory entries in the manifest.
-   Multiple files.
-   Safe path handling.
-   Progress reporting.
-   Block-level dedup (covers identical files).

Exit criteria: - Mixed directory roundtrip. - Windows/Linux integration
tests.

## M3 --- Parallel engine (Weeks 6--7)

-   Rayon worker pool.
-   Bounded queues.
-   Memory budget.
-   Parallel independent file compression.
-   Parallel extraction.

Exit criteria: - Scaling benchmark. - No oversubscription. -
Resource-limit tests.

## M4 --- Adaptive routing (Weeks 8--9)

-   Sampling.
-   Zstd candidate selection.
-   Output-size acceptance.
-   Already-compressed detection.
-   Mode settings.

Exit criteria: - Routing benchmark. - No regressions in exact
restoration.

## M5 --- Chunk dedup (Weeks 10--11)

-   Content-defined chunking (CDC).
-   Exact chunk hashes.
-   Manifest references with non-zero block offsets.
-   Bounded or disk-backed dedup index.

Exit criteria: - Savings on a dataset with shifted duplicate content. -
Corruption tests for out-of-range references.

## M6 --- Research codec prototype (Weeks 12--13+)

-   Image reversible transforms.
-   Frame hashing.
-   Duplicate-frame detection.
-   Block matching experiments.
-   Residual prototype.
-   Comparative report.

Exit criteria: - Exact reconstruction. - Reproducible benchmark. -
Research results documented.

## Release stages

-   `0.1.0`: experimental archive, RAW/Zstd, fixed-size blocks, block
    dedup.
-   `0.2.0`: parallel compression/extraction, adaptive routing.
-   `0.3.0`: content-defined chunk dedup.
-   `0.4.0`: research codec prototype.
-   `1.0.0`: stable format after compatibility and security review.

------------------------------------------------------------------------

# 10. Cursor Development Instructions

## 10.1 Paste this into Cursor project rules (`.cursor/rules/acx.mdc`)

``` md
# ACX Engineering Rules

You are implementing ACX, a CPU-first lossless media archive engine in Rust.

## Architecture
- Follow docs/PRD.md and docs/FORMAT_V1.md.
- Keep crates separated by responsibility.
- Do not place codec logic in the container crate.
- Do not place filesystem traversal in acx-core.
- Keep experimental media transforms behind explicit features or research crates.
- Stable archive behavior must use FileExact preservation by default.

## Correctness
- Exact byte reconstruction is mandatory for FileExact.
- Never use lossy operations in lossless mode.
- Never use perceptual hashes as proof of equality.
- Dedup requires equal decoded length and BLAKE3; byte comparison only with --verify-dedup.
- Dedup is expressed only by manifest references sharing a Block ID; there are no reference blocks.
- Do not silently fall back when an archive codec ID is unknown.
- Every decode must enforce expected output size and resource limits.

## Binary format
- Follow docs/FORMAT_V1.md exactly.
- Never serialize Rust structs by memory layout.
- Use explicit little-endian field encoding.
- Validate offsets, lengths, counts, arithmetic, and paths before allocation or writing.
- Keep golden fixtures for format compatibility.
- Do not change format fields without updating the spec and versioning.

## Concurrency
- Use one global concurrency budget.
- Use bounded queues.
- Avoid nested unbounded thread pools.
- Respect configured memory limits.
- Cancellation must stop scheduling new work and clean up temporary files.

## Security
- Reject absolute paths, `..`, path traversal, invalid path encodings, and unsafe symlink behavior.
- Do not trust extensions or archive metadata.
- Treat every archive as untrusted input.
- Avoid shell interpolation for external tools.
- No panics on malformed user input or archives.

## Quality
- Use typed errors with thiserror.
- Use tracing for logs.
- Add unit/property/integration tests for every feature.
- Run cargo fmt, clippy, and tests.
- Do not leave TODO, unimplemented!, or placeholder behavior in completed tasks.
- Keep dependencies minimal and justify new dependencies.
- Do not add a trait until it has a second implementation.
- Do not add CLI options before they are implemented.
- Do not claim performance improvements without benchmark evidence.

## Workflow
- Implement one milestone/task at a time.
- Before coding, summarize the plan and affected crates.
- Keep changes small and compile frequently.
- After coding, report files changed, tests run, results, and known limitations.
- If requirements conflict, stop and ask rather than silently changing the format.
```

## 10.2 Initial Cursor prompt

Paste this into Cursor Agent:

``` text
You are the principal Rust engineer implementing ACX.

Read:
- docs/PRD.md
- docs/FORMAT_V1.md
- docs/CURSOR_RULES.md

Do not implement the whole project in one pass.

TASK 0 — Repository foundation:
1. Create the Rust workspace and the five crates in PRD §2.2 (not acx-bench or acx-research yet).
2. Create the acx-cli binary with the `version` command only; other commands arrive with their tasks.
3. Implement acx-core shared types, typed errors, configuration structs, and resource budget types.
4. Create CI workflows for Windows and Linux.
5. Add formatting, clippy, and test commands to README.
6. Add architecture documentation.
7. Do not implement archive compression yet.
8. Ensure the workspace builds and all tests pass.

Before editing:
- Explain the files you will create.
- Identify dependency choices.
- Flag any incompatibility with the current Rust toolchain.

After editing:
- Run cargo fmt.
- Run cargo clippy --workspace --all-targets -- -D warnings.
- Run cargo test --workspace.
- Report exact commands and results.
- Do not claim a command passed unless it was actually run.
```

## 10.3 Subsequent Cursor task prompts

### TASK 1 --- Format primitives

``` text
Implement ACX v1 fixed header and footer serialization/deserialization in acx-container.

Requirements:
- Follow docs/FORMAT_V1.md.
- Explicit little-endian encoding.
- CRC32C validation.
- Reject any set bit in required flags; ignore optional flags.
- Validate all reserved fields.
- Footer locates manifest and index; header holds no offsets.
- Add golden fixtures and malformed-header tests.
- No unsafe code.
- Do not implement manifest or block payload yet.
```

### TASK 2 --- RAW/Zstandard blocks

``` text
Implement record framing, RAW/Zstandard payload blocks, and the index record.

Requirements:
- Enforce maximum block size before allocating.
- Honour the record optional bit for unknown record types.
- Cross-check block header fields against the index entry.
- Validate compressed CRC32C.
- Validate decoded BLAKE3.
- Validate expected decoded size.
- Add roundtrip/property tests.
- Reject truncated and malformed frames.
- Do not add LZMA2 yet.
```

### TASK 3 --- Single-file create/extract

``` text
Implement single-file ACX create, extract, and verify.

Requirements:
- FileExact only.
- Manifest serialization for file entries (FORMAT_V1 §7).
- Fixed-size blocks (default 16 MiB); files over 4 GiB must work.
- RAW fallback.
- Zstandard trial and per-block output-size acceptance (PRD §3.2).
- Single read of the input: file and block BLAKE3 from the same bytes.
- Atomic archive creation (temp file + rename) and atomic extraction writes.
- Verify restored bytes against original BLAKE3.
- Add integration tests and CLI commands.
```

### TASK 4 --- Directory scanner

``` text
Implement safe directory scanning and directory archives.

Requirements:
- Relative UTF-8 paths stored exactly as scanned, no normalization.
- Directory entries, including empty directories.
- Reject or skip non-Unicode paths per policy.
- No path traversal.
- Symlinks rejected (strict) or skipped (skip policy).
- Extraction rules for Windows names, long paths, and case collisions (PRD §2.3).
- Unicode and Windows path tests.
- Permission error policy.
- Record and restore size, mtime, and Unix mode bits.
```

### TASK 5 --- Parallel scheduler

``` text
Implement bounded parallel file compression and extraction.

Requirements:
- Rayon with explicit worker count.
- One global concurrency budget.
- Bounded channels.
- Dedicated archive writer.
- Memory budget.
- Cancellation and cleanup.
- Benchmark one-thread versus multi-thread.
```

### TASK 6 --- Exact file dedup

``` text
Implement block-level deduplication.

Requirements:
- Key each block by (decoded length, BLAKE3).
- On a hit, point the manifest reference at the existing Block ID; write no new block.
- Optional byte comparison with --verify-dedup.
- Deterministic Block ID assignment with parallel workers.
- Tests with duplicate files, files sharing a prefix, and near-duplicate files.
```

### TASK 7 --- Benchmark harness

``` text
Create the acx-bench crate and the `acx benchmark` CLI command.

Requirements:
- Dataset manifest.
- Repeated runs.
- Warm-up.
- Median and dispersion.
- Encode/decode throughput.
- Output size.
- Peak memory where supported.
- Correctness verification.
- JSON and CSV report output.
```

### TASK 8 --- Research codec

``` text
Create acx-research experimental crate.

Implement:
- Frame hashing.
- Exact duplicate-frame detection.
- Reversible RGB transform experiments.
- Block matching interface.
- Residual reconstruction tests.

Do not integrate experimental codec into stable ACX archive routing.
Document results and benchmark against appropriate baselines.
```

------------------------------------------------------------------------

# 11. Definition of Done

A feature is complete only when:

1.  It meets its specification.
2.  Unit tests exist.
3.  Property tests exist where data parsing or reconstruction is
    involved.
4.  Integration tests pass.
5.  Corruption/security cases are tested.
6.  Windows and Linux builds pass.
7.  Formatting and clippy pass.
8.  Documentation is updated.
9.  Benchmark results are available for performance-sensitive changes.
10. No unimplemented code or misleading success behavior remains.

------------------------------------------------------------------------

# 12. First Coding Tasks --- Recommended Order

Start with the following exact sequence:

1.  Repository/workspace foundation.
2.  `acx-core` types and errors.
3.  `acx-container` header/footer.
4.  Record framing and index.
5.  RAW and Zstandard codec.
6.  Manifest, fixed-size blocks, single-file create/extract/verify.
7.  Directory scanning.
8.  Parallel scheduler.
9.  Block dedup.
10. Benchmark harness.
11. Chunk dedup.
12. Image/video research.

Do not begin temporal video compression before the archive format,
correctness suite, and benchmark infrastructure are reliable.

------------------------------------------------------------------------

# 13. Product success criteria

ACX is successful when it can:

-   Restore archived files byte-for-byte.
-   Safely handle corrupted and hostile archives.
-   Compress and extract independent files concurrently.
-   Stay within configured memory limits.
-   Reduce storage for duplicate-heavy media libraries.
-   Avoid wasting CPU on already-compressed media.
-   Produce reproducible benchmark results.
-   Provide a clean foundation for original lossless image/video
    research.

The first goal is a reliable archive engine. The second goal is a
genuinely competitive compression research platform.
