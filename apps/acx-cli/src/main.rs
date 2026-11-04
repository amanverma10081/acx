//! `acx` command-line interface (PRD §6). Options appear only once they are
//! implemented. Exit codes follow PRD §6.4; clap exits with 2 on bad
//! arguments.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::Ordering;

use acx_archive::{EntryKind, Listing};
use acx_core::{
    AcxError, ArchiveOptions, CancelFlag, CodecChoice, CompressionMode, DEFAULT_MAX_OUTPUT,
    DedupMode, ErrorPolicy, ExtractOptions, OverwritePolicy, ResourceBudget, VerifyOptions,
};
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(name = "acx", version, about = "ACX lossless media archiver")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create an archive from files and directories.
    Create {
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        #[arg(short, long)]
        output: PathBuf,
        /// Replace an existing archive.
        #[arg(long)]
        overwrite: bool,
        #[command(flatten)]
        encoding: Encoding,
    },
    /// Extract an archive into a directory.
    Extract {
        archive: PathBuf,
        #[arg(short, long)]
        output: PathBuf,
        /// Replace existing files.
        #[arg(long, conflicts_with = "skip_existing")]
        overwrite: bool,
        /// Leave existing files untouched.
        #[arg(long)]
        skip_existing: bool,
        /// Refuse archives that would write more than this, e.g. 500GiB.
        #[arg(long, value_parser = parse_size)]
        max_output: Option<u64>,
        #[command(flatten)]
        resources: Resources,
    },
    /// List archive contents.
    List {
        archive: PathBuf,
        /// Also print the block index.
        #[arg(long)]
        blocks: bool,
    },
    /// Check archive integrity.
    Verify {
        archive: PathBuf,
        /// Decode every block and recompute file hashes.
        #[arg(long)]
        full: bool,
    },
    /// Benchmark create and extract on a dataset, verifying every run.
    Benchmark {
        /// A directory or file to archive repeatedly.
        dataset: PathBuf,
        /// Unreported runs first, to warm caches.
        #[arg(long, default_value_t = 1)]
        warmup: usize,
        /// Measured runs.
        #[arg(long, default_value_t = 5)]
        runs: usize,
        /// Write the full report as JSON.
        #[arg(long)]
        json: Option<PathBuf>,
        /// Write one CSV row per measured run.
        #[arg(long)]
        csv: Option<PathBuf>,
        /// Directory for temporary archives and extracted copies
        /// [default: the system temp directory].
        #[arg(long)]
        work_dir: Option<PathBuf>,
        #[command(flatten)]
        encoding: Encoding,
    },
    /// Print the ACX version.
    Version,
}

/// How `create` (and `benchmark`) encode their input.
#[derive(Args)]
struct Encoding {
    #[arg(long, value_enum, default_value_t = Mode::Balanced)]
    mode: Mode,
    #[arg(long, value_enum, default_value_t = Codec::Auto)]
    codec: Codec,
    /// Block size, e.g. 16MiB (64KiB to 256MiB).
    #[arg(long, value_parser = parse_size, default_value = "16MiB")]
    block_size: u64,
    #[command(flatten)]
    resources: Resources,
    /// Warn about and leave out unreadable files, symlinks, and names that
    /// are not valid Unicode, instead of failing.
    #[arg(long)]
    skip_errors: bool,
    /// Store identical blocks once.
    #[arg(long, value_enum, default_value_t = Dedup::Blocks)]
    dedup: Dedup,
    /// Byte-compare each duplicate with the stored block before reusing it,
    /// instead of trusting length plus BLAKE3.
    #[arg(long)]
    verify_dedup: bool,
}

impl Encoding {
    fn options(&self, overwrite: bool, cancel: CancelFlag) -> ArchiveOptions {
        ArchiveOptions {
            error_policy: if self.skip_errors {
                ErrorPolicy::Skip
            } else {
                ErrorPolicy::Strict
            },
            mode: match self.mode {
                Mode::Fast => CompressionMode::Fast,
                Mode::Balanced => CompressionMode::Balanced,
                Mode::Maximum => CompressionMode::Maximum,
            },
            codec: match self.codec {
                Codec::Auto => CodecChoice::Auto,
                Codec::Raw => CodecChoice::Raw,
                Codec::Zstd => CodecChoice::Zstd,
            },
            dedup: match self.dedup {
                Dedup::None => DedupMode::None,
                Dedup::Blocks => DedupMode::Blocks,
            },
            verify_dedup: self.verify_dedup,
            budget: self.resources.budget(self.block_size),
            overwrite,
            cancel,
            ..ArchiveOptions::default()
        }
    }
}

/// The global worker and memory budget (PRD §2.3 scheduler).
#[derive(Args)]
struct Resources {
    /// Worker threads [default: logical CPU count]. Reduced if the memory
    /// budget cannot hold that many blocks in flight.
    #[arg(long)]
    threads: Option<usize>,
    /// Memory budget for block buffers, e.g. 4GiB [default: 1GiB].
    #[arg(long, value_parser = parse_size)]
    memory: Option<u64>,
}

impl Resources {
    fn budget(&self, block_size: u64) -> ResourceBudget {
        let default = ResourceBudget::default();
        ResourceBudget {
            threads: self.threads.unwrap_or(default.threads),
            memory_bytes: self.memory.unwrap_or(default.memory_bytes),
            block_size,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum Mode {
    Fast,
    Balanced,
    Maximum,
}

#[derive(Clone, Copy, ValueEnum)]
enum Dedup {
    None,
    Blocks,
}

#[derive(Clone, Copy, ValueEnum)]
enum Codec {
    Auto,
    Raw,
    Zstd,
}

fn main() -> ExitCode {
    let command = Cli::parse().command;
    let cancel = CancelFlag::default();
    let flag = cancel.clone();
    // First Ctrl-C: stop cleanly, removing temporary files. Second: quit now.
    let _ = ctrlc::set_handler(move || {
        if flag.swap(true, Ordering::Relaxed) {
            std::process::exit(8);
        }
        eprintln!("acx: cancelling (press Ctrl-C again to quit at once)");
    });
    match run(command, cancel) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("acx: error: {}", acx_archive::describe(&e));
            ExitCode::from(e.exit_code())
        }
    }
}

fn run(command: Command, cancel: CancelFlag) -> Result<ExitCode, AcxError> {
    match command {
        Command::Create {
            inputs,
            output,
            overwrite,
            encoding,
        } => {
            let options = encoding.options(overwrite, cancel);
            let r = acx_archive::create_archive(&inputs, &output, &options, |_| {})?;
            for w in &r.warnings {
                eprintln!("acx: warning: {w}");
            }
            let pct = if r.input_bytes == 0 {
                100.0
            } else {
                r.archive_bytes as f64 * 100.0 / r.input_bytes as f64
            };
            println!(
                "{} file(s), {} director(ies), {} bytes -> {} bytes ({pct:.1}%), {} block(s), {} compressed",
                r.files,
                r.directories,
                r.input_bytes,
                r.archive_bytes,
                r.blocks,
                r.compressed_blocks
            );
            if r.duplicate_blocks > 0 {
                println!(
                    "{} duplicate block(s) stored once, saving {} bytes",
                    r.duplicate_blocks, r.duplicate_bytes
                );
            }
        }
        Command::Extract {
            archive,
            output,
            overwrite,
            skip_existing,
            max_output,
            resources,
        } => {
            let options = ExtractOptions {
                overwrite: if overwrite {
                    OverwritePolicy::Overwrite
                } else if skip_existing {
                    OverwritePolicy::SkipExisting
                } else {
                    OverwritePolicy::Error
                },
                max_output_bytes: max_output.unwrap_or(DEFAULT_MAX_OUTPUT),
                // Extraction sizes its blocks from the archive itself.
                budget: resources.budget(acx_core::DEFAULT_BLOCK_SIZE),
                cancel,
            };
            let r = acx_archive::extract_archive(&archive, &output, &options, |_| {})?;
            for path in &r.skipped {
                eprintln!("acx: skipped existing {path}");
            }
            println!(
                "{} file(s), {} director(ies), {} bytes extracted",
                r.files, r.directories, r.bytes
            );
        }
        Command::List { archive, blocks } => {
            print_listing(&acx_archive::list_archive(&archive)?, blocks)
        }
        Command::Verify { archive, full } => {
            let r = acx_archive::verify_archive(&archive, &VerifyOptions { full })?;
            for p in &r.problems {
                println!("DAMAGED {p}");
            }
            if !r.is_ok() {
                eprintln!("acx: {} problem(s) found", r.problems.len());
                return Ok(ExitCode::from(4));
            }
            let mode = if full { "full" } else { "quick" };
            println!("OK ({mode}): {} file(s), {} block(s)", r.files, r.blocks);
        }
        Command::Benchmark {
            dataset,
            warmup,
            runs,
            json,
            csv,
            work_dir,
            encoding,
        } => {
            let config = acx_bench::BenchConfig {
                archive: encoding.options(true, cancel),
                warmup,
                runs,
                work_dir,
            };
            let report = acx_bench::run_benchmark(&dataset, &config)?;
            if let Some(path) = &json {
                acx_bench::write_json(&report, path)?;
            }
            if let Some(path) = &csv {
                acx_bench::write_csv(&report, path)?;
            }
            print_benchmark(&report);
            if !report.summary.all_verified {
                eprintln!("acx: an extracted copy did not match the dataset");
                return Ok(ExitCode::from(4));
            }
        }
        Command::Version => println!("acx {}", env!("CARGO_PKG_VERSION")),
    }
    Ok(ExitCode::SUCCESS)
}

fn print_benchmark(r: &acx_bench::BenchReport) {
    const MIB: f64 = (1u64 << 20) as f64;
    let (d, st, s) = (&r.dataset, &r.settings, &r.summary);
    println!(
        "dataset {}: {} file(s), {} director(ies), {:.1} MiB, manifest {}",
        d.id,
        d.files,
        d.directories,
        d.bytes as f64 / MIB,
        &d.manifest_blake3[..16]
    );
    println!(
        "settings: {} thread(s) ({} effective), block {} MiB, mode {}, codec {}, dedup {}, {} warm-up + {} run(s)",
        st.threads,
        st.effective_threads,
        st.block_size >> 20,
        st.mode,
        st.codec,
        st.dedup,
        st.warmup,
        st.runs
    );
    for (name, t, rate) in [
        ("create ", s.create_secs, s.create_mib_s),
        ("extract", s.extract_secs, s.extract_mib_s),
    ] {
        println!(
            "{name}  median {:.3}s  MAD {:.3}s  range {:.3}-{:.3}s  {rate:.1} MiB/s",
            t.median, t.mad, t.min, t.max
        );
    }
    println!(
        "archive  {:.1} MiB, ratio {:.3}, {:.1}% saved; every run verified: {}",
        s.archive_bytes as f64 / MIB,
        s.ratio,
        s.saved_percent,
        s.all_verified
    );
    match r.peak_rss_bytes {
        Some(b) => println!(
            "peak memory (process RSS, all runs): {:.1} MiB",
            b as f64 / MIB
        ),
        None => println!("peak memory: not measured on this platform"),
    }
}

fn print_listing(l: &Listing, blocks: bool) {
    for e in &l.entries {
        match e.kind {
            EntryKind::File => println!("{:>14}  {}", e.size, e.path),
            EntryKind::Directory => println!("{:>14}  {}/", "<dir>", e.path),
        }
    }
    if blocks {
        println!(
            "\n{:>6}  {:<5} {:>14} {:>14} {:>14}",
            "block", "codec", "offset", "original", "stored"
        );
        for b in &l.blocks {
            println!(
                "{:>6}  {:<5} {:>14} {:>14} {:>14}",
                b.id.0,
                format!("{:?}", b.codec),
                b.record.offset,
                b.original_size,
                b.compressed_size
            );
        }
    }
    println!(
        "{} entr(ies), {} block(s), archive {} bytes",
        l.entries.len(),
        l.blocks.len(),
        l.archive_len
    );
}

/// Parses `123`, `64KiB`, `16MiB`, `1GiB`, `2TiB` (binary units; `K`, `M`,
/// `G`, `T` are accepted as the same, case-insensitively).
fn parse_size(s: &str) -> Result<u64, String> {
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: u64 = num.parse().map_err(|_| format!("invalid size {s:?}"))?;
    let mult = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kib" => acx_core::KIB,
        "m" | "mib" => acx_core::MIB,
        "g" | "gib" => acx_core::GIB,
        "t" | "tib" => acx_core::TIB,
        _ => {
            return Err(format!(
                "unknown size unit in {s:?} (use KiB, MiB, GiB, TiB)"
            ));
        }
    };
    num.checked_mul(mult)
        .ok_or_else(|| format!("size {s:?} is too large"))
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("123"), Ok(123));
        assert_eq!(parse_size("64KiB"), Ok(64 * 1024));
        assert_eq!(parse_size("16mib"), Ok(16 << 20));
        assert_eq!(parse_size("1G"), Ok(1 << 30));
        assert_eq!(parse_size("2TiB"), Ok(2 << 40));
        for bad in ["", "MiB", "1.5GiB", "-1", "10XB", "99999999999TiB"] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
    }
}
