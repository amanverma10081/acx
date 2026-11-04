//! Reproducible archive benchmarks (PRD §7, `acx benchmark`).
//!
//! Each run archives the dataset, extracts it, and checks the extracted tree
//! against the dataset manifest. Warm-up runs are verified but not reported.
//! Timings are summarised as median and median absolute deviation (MAD).

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use acx_archive::{create_archive, extract_archive};
use acx_core::{AcxError, ArchiveOptions, ExtractOptions};
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct BenchConfig {
    pub archive: ArchiveOptions,
    pub warmup: usize,
    pub runs: usize,
    /// Where archives and extracted trees go [default: the system temp
    /// directory]. Point it at the disk you want to measure.
    pub work_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BenchReport {
    pub acx_version: &'static str,
    pub environment: Environment,
    pub settings: Settings,
    pub dataset: Dataset,
    /// Measured runs only.
    pub runs: Vec<Run>,
    pub summary: Summary,
    /// Highest resident memory of the whole process over all runs, warm-up
    /// included. `None` where the platform is not supported.
    pub peak_rss_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Environment {
    pub os: &'static str,
    pub arch: &'static str,
    pub cpu: Option<String>,
    pub logical_cpus: usize,
    pub zstd_version: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct Settings {
    pub threads: usize,
    pub effective_threads: usize,
    pub memory_bytes: u64,
    pub block_size: u64,
    pub mode: String,
    pub codec: String,
    pub dedup: String,
    pub verify_dedup: bool,
    pub warmup: usize,
    pub runs: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Dataset {
    /// The name the dataset is archived under.
    pub id: String,
    pub files: u64,
    pub directories: u64,
    pub bytes: u64,
    /// BLAKE3 over every path, size, and file BLAKE3: identifies the exact
    /// dataset a result belongs to.
    pub manifest_blake3: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub create_secs: f64,
    pub extract_secs: f64,
    pub archive_bytes: u64,
    /// The extracted tree matched the dataset manifest exactly.
    pub verified: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Stats {
    pub median: f64,
    /// Median absolute deviation from the median.
    pub mad: f64,
    pub min: f64,
    pub max: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub create_secs: Stats,
    pub extract_secs: Stats,
    /// Dataset bytes per second at the median time, in MiB/s.
    pub create_mib_s: f64,
    pub extract_mib_s: f64,
    pub archive_bytes: u64,
    /// Dataset bytes divided by archive bytes.
    pub ratio: f64,
    pub saved_percent: f64,
    /// Every run, warm-up included, restored the dataset exactly.
    pub all_verified: bool,
}

/// One entry per path, in walk order: `Some((size, BLAKE3))` for a file,
/// `None` for a directory.
pub type TreeManifest = Vec<(String, Option<(u64, [u8; 32])>)>;

/// Lists `root` with every path prefixed by `name`, the way `create` names
/// it, so a dataset and its extracted copy compare equal.
pub fn tree_manifest(root: &Path, name: &str) -> Result<TreeManifest, AcxError> {
    let io_err = |p: &Path, e: io::Error| AcxError::Io {
        context: format!("reading {}", p.display()),
        source: e,
    };
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .follow_root_links(false)
        .sort_by_file_name()
    {
        let entry = entry.map_err(|e| io_err(root, e.into()))?;
        let rel = entry.path().strip_prefix(root).unwrap_or(Path::new(""));
        let mut path = name.to_owned();
        for c in rel.components() {
            path.push('/');
            path.push_str(&c.as_os_str().to_string_lossy());
        }
        let ft = entry.file_type();
        let content = if ft.is_dir() {
            None
        } else if ft.is_file() {
            let mut hasher = blake3::Hasher::new();
            let file = File::open(entry.path()).map_err(|e| io_err(entry.path(), e))?;
            let size = hasher
                .update_reader(file)
                .map_err(|e| io_err(entry.path(), e))?
                .count();
            Some((size, *hasher.finalize().as_bytes()))
        } else {
            return Err(AcxError::InvalidArgument(format!(
                "{}: benchmark datasets may not contain symlinks or special files",
                entry.path().display()
            )));
        };
        out.push((path, content));
    }
    Ok(out)
}

fn summarize_dataset(id: String, manifest: &TreeManifest) -> Dataset {
    let mut hasher = blake3::Hasher::new();
    let (mut files, mut directories, mut bytes) = (0, 0, 0);
    for (path, content) in manifest {
        match content {
            Some((size, digest)) => {
                files += 1;
                bytes += size;
                hasher.update(format!("F\t{path}\t{size}\t").as_bytes());
                hasher.update(blake3::Hash::from_bytes(*digest).to_hex().as_bytes());
                hasher.update(b"\n");
            }
            None => {
                directories += 1;
                hasher.update(format!("D\t{path}\n").as_bytes());
            }
        }
    }
    Dataset {
        id,
        files,
        directories,
        bytes,
        manifest_blake3: hasher.finalize().to_hex().to_string(),
    }
}

pub fn run_benchmark(dataset: &Path, config: &BenchConfig) -> Result<BenchReport, AcxError> {
    if config.runs == 0 {
        return Err(AcxError::InvalidArgument(
            "--runs must be at least 1".into(),
        ));
    }
    let budget = config.archive.budget;
    budget.validate()?;
    let id = fs::canonicalize(dataset)
        .ok()
        .and_then(|p| p.file_name()?.to_str().map(str::to_owned))
        .ok_or_else(|| {
            AcxError::InvalidArgument(format!(
                "{} is not a named file or directory",
                dataset.display()
            ))
        })?;
    let source = tree_manifest(dataset, &id)?;

    let work = match &config.work_dir {
        Some(dir) => tempfile::tempdir_in(dir),
        None => tempfile::tempdir(),
    }
    .map_err(|e| AcxError::Io {
        context: "creating the benchmark work directory".into(),
        source: e,
    })?;
    let archive = work.path().join("bench.acx");
    let out = work.path().join("out");
    let extract = ExtractOptions {
        budget,
        cancel: config.archive.cancel.clone(),
        ..ExtractOptions::default()
    };
    let archive_options = ArchiveOptions {
        overwrite: true,
        ..config.archive.clone()
    };

    let mut runs = Vec::with_capacity(config.runs);
    let mut all_verified = true;
    for i in 0..config.warmup + config.runs {
        if out.exists() {
            fs::remove_dir_all(&out).map_err(|e| AcxError::Io {
                context: format!("clearing {}", out.display()),
                source: e,
            })?;
        }
        let t = Instant::now();
        let created = create_archive(&[dataset.to_path_buf()], &archive, &archive_options, |_| {})?;
        let create_secs = t.elapsed().as_secs_f64();
        let t = Instant::now();
        extract_archive(&archive, &out, &extract, |_| {})?;
        let extract_secs = t.elapsed().as_secs_f64();

        let verified = tree_manifest(&out.join(&id), &id)? == source;
        all_verified &= verified;
        if i >= config.warmup {
            runs.push(Run {
                create_secs,
                extract_secs,
                archive_bytes: created.archive_bytes,
                verified,
            });
        }
    }

    let dataset = summarize_dataset(id, &source);
    let create_secs = stats(&runs.iter().map(|r| r.create_secs).collect::<Vec<_>>());
    let extract_secs = stats(&runs.iter().map(|r| r.extract_secs).collect::<Vec<_>>());
    // Archives are deterministic, so every run has the same size.
    let archive_bytes = runs.iter().map(|r| r.archive_bytes).max().unwrap_or(0);
    let mib = dataset.bytes as f64 / (1u64 << 20) as f64;
    let summary = Summary {
        create_secs,
        extract_secs,
        create_mib_s: mib / create_secs.median.max(1e-9),
        extract_mib_s: mib / extract_secs.median.max(1e-9),
        archive_bytes,
        ratio: dataset.bytes as f64 / archive_bytes.max(1) as f64,
        saved_percent: match dataset.bytes {
            0 => 0.0,
            b => (1.0 - archive_bytes as f64 / b as f64) * 100.0,
        },
        all_verified,
    };
    let a = &config.archive;
    Ok(BenchReport {
        acx_version: env!("CARGO_PKG_VERSION"),
        environment: Environment {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            cpu: cpu_name(),
            logical_cpus: std::thread::available_parallelism().map_or(1, |n| n.get()),
            zstd_version: acx_codecs::zstd_version(),
        },
        settings: Settings {
            threads: budget.threads,
            effective_threads: budget.effective_threads(),
            memory_bytes: budget.memory_bytes,
            block_size: budget.block_size,
            mode: format!("{:?}", a.mode),
            codec: format!("{:?}", a.codec),
            dedup: format!("{:?}", a.dedup),
            verify_dedup: a.verify_dedup,
            warmup: config.warmup,
            runs: config.runs,
        },
        dataset,
        runs,
        summary,
        peak_rss_bytes: peak_rss_bytes(),
    })
}

/// Median, MAD, min, and max. `xs` must not be empty.
pub fn stats(xs: &[f64]) -> Stats {
    let median = median(xs);
    let deviations: Vec<f64> = xs.iter().map(|x| (x - median).abs()).collect();
    Stats {
        median,
        mad: median_of(deviations),
        min: xs.iter().copied().fold(f64::INFINITY, f64::min),
        max: xs.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    }
}

fn median(xs: &[f64]) -> f64 {
    median_of(xs.to_vec())
}

fn median_of(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(f64::total_cmp);
    let n = xs.len();
    match n {
        0 => f64::NAN,
        _ if n % 2 == 1 => xs[n / 2],
        _ => (xs[n / 2 - 1] + xs[n / 2]) / 2.0,
    }
}

pub fn write_json(report: &BenchReport, path: &Path) -> Result<(), AcxError> {
    let text = serde_json::to_string_pretty(report).expect("report types always serialize");
    fs::write(path, text + "\n").map_err(|e| AcxError::Io {
        context: format!("writing {}", path.display()),
        source: e,
    })
}

/// One row per measured run. Settings and environment are in the JSON.
pub fn write_csv(report: &BenchReport, path: &Path) -> Result<(), AcxError> {
    let mut text = String::from(
        "run,dataset_manifest_blake3,source_bytes,archive_bytes,create_secs,extract_secs,verified\n",
    );
    for (i, r) in report.runs.iter().enumerate() {
        text.push_str(&format!(
            "{},{},{},{},{:.6},{:.6},{}\n",
            i + 1,
            report.dataset.manifest_blake3,
            report.dataset.bytes,
            r.archive_bytes,
            r.create_secs,
            r.extract_secs,
            r.verified
        ));
    }
    File::create(path)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(|e| AcxError::Io {
            context: format!("writing {}", path.display()),
            source: e,
        })
}

fn cpu_name() -> Option<String> {
    if cfg!(target_os = "linux") {
        let info = fs::read_to_string("/proc/cpuinfo").ok()?;
        let line = info.lines().find(|l| l.starts_with("model name"))?;
        Some(line.split_once(':')?.1.trim().to_owned())
    } else {
        std::env::var("PROCESSOR_IDENTIFIER").ok()
    }
}

#[cfg(target_os = "linux")]
fn peak_rss_bytes() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn peak_rss_bytes() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: an all-zero PROCESS_MEMORY_COUNTERS is a valid plain C struct;
    // GetCurrentProcess returns a pseudo-handle that needs no closing; and
    // `counters` is a writable out-parameter of exactly `size` bytes.
    let (ok, counters) = unsafe {
        let mut counters: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        counters.cb = size;
        let ok = GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size);
        (ok, counters)
    };
    (ok != 0).then_some(counters.PeakWorkingSetSize as u64)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn peak_rss_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_and_mad() {
        let s = stats(&[3.0, 1.0, 2.0, 100.0, 2.5]);
        assert_eq!((s.median, s.min, s.max), (2.5, 1.0, 100.0));
        // Deviations 0.5, 1.5, 0.5, 97.5, 0 -> median 0.5: robust to the outlier.
        assert_eq!(s.mad, 0.5);
        let even = stats(&[4.0, 1.0, 3.0, 2.0]);
        assert_eq!((even.median, even.mad), (2.5, 1.0));
        assert_eq!(stats(&[7.0]).median, 7.0);
    }

    #[test]
    fn peak_memory_is_reported_where_supported() {
        let peak = peak_rss_bytes();
        if cfg!(any(target_os = "linux", windows)) {
            assert!(peak.is_some_and(|b| b > 1 << 20), "{peak:?}");
        }
    }
}
