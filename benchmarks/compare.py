"""Compares ACX with common archivers on one dataset.

    python benchmarks/compare.py ACX_EXE DATASET WORK_DIR OUT_JSON [--runs N]

ACX is measured with `acx benchmark` (1 warm-up + N runs, every run
verified). Every other tool gets the same treatment here: one warm-up run,
then N timed create and extract runs, and every extracted tree is checked
file by file (BLAKE2b) against the dataset. Reported time is the median.
Baselines use bsdtar (`tar.exe` on Windows, built on libarchive), so all
codecs come from the same, mainstream libraries.
"""

import hashlib
import json
import os
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path

TAR = "tar.exe" if os.name == "nt" else "bsdtar"

BASELINES = [
    # name, archive file, create args, extract args
    ("zip (Deflate)", "out.zip", ["-a", "-cf"], ["-xf"]),
    ("tar.gz (gzip -6)", "out.tar.gz", ["-czf"], ["-xzf"]),
    ("tar.zst (zstd -3)", "out.tar.zst", ["--zstd", "-cf"], ["--zstd", "-xf"]),
    ("tar.xz (xz -6)", "out.tar.xz", ["-cJf"], ["-xJf"]),
]

ACX_CONFIGS = [
    ("ACX balanced, all threads", []),
    ("ACX balanced, 1 thread", ["--threads", "1"]),
    ("ACX fast, all threads", ["--mode", "fast"]),
    ("ACX maximum, all threads", ["--mode", "maximum"]),
]


def tree(root: Path) -> dict:
    out = {}
    for p in sorted(root.rglob("*")):
        if p.is_file():
            out[p.relative_to(root).as_posix()] = hashlib.blake2b(p.read_bytes()).hexdigest()
    return out


def run(cmd, cwd) -> float:
    t = time.perf_counter()
    subprocess.run(cmd, cwd=cwd, check=True, stdout=subprocess.DEVNULL)
    return time.perf_counter() - t


def baseline(name, archive, cargs, xargs, dataset: Path, work: Path, runs: int, expected) -> dict:
    creates, extracts = [], []
    for i in range(runs + 1):
        a = work / archive
        out = work / "x"
        a.unlink(missing_ok=True)
        shutil.rmtree(out, ignore_errors=True)
        out.mkdir()
        c = run([TAR, *cargs, str(a), dataset.name], cwd=dataset.parent)
        x = run([TAR, *xargs, str(a), "-C", str(out)], cwd=work)
        if tree(out / dataset.name) != expected:
            sys.exit(f"{name}: extracted tree does not match the dataset")
        if i:
            creates.append(c)
            extracts.append(x)
    size = (work / archive).stat().st_size
    (work / archive).unlink()
    shutil.rmtree(work / "x")
    return {"tool": name, "archive_bytes": size, "create_secs": statistics.median(creates),
            "extract_secs": statistics.median(extracts), "verified": True}


def acx(name, extra, exe, dataset: Path, work: Path, runs: int) -> dict:
    report = work / "acx.json"
    subprocess.run([exe, "benchmark", str(dataset), "--runs", str(runs), "--warmup", "1",
                    "--work-dir", str(work), "--json", str(report), *extra],
                   check=True, stdout=subprocess.DEVNULL)
    r = json.loads(report.read_text())
    s = r["summary"]
    return {"tool": name, "archive_bytes": s["archive_bytes"],
            "create_secs": s["create_secs"]["median"], "extract_secs": s["extract_secs"]["median"],
            "verified": s["all_verified"], "peak_rss_bytes": r["peak_rss_bytes"],
            "acx_report": r}


def main() -> None:
    exe, dataset, work, out_json = sys.argv[1], Path(sys.argv[2]).resolve(), Path(sys.argv[3]), sys.argv[4]
    runs = int(sys.argv[sys.argv.index("--runs") + 1]) if "--runs" in sys.argv else 3
    work.mkdir(parents=True, exist_ok=True)
    expected = tree(dataset)
    source = sum((dataset / p).stat().st_size for p in expected)
    results = []
    for name, extra in ACX_CONFIGS:
        results.append(acx(name, extra, exe, dataset, work, runs))
        print(f"done: {name}", flush=True)
    for b in BASELINES:
        results.append(baseline(*b, dataset, work, runs, expected))
        print(f"done: {b[0]}", flush=True)
    tar_version = subprocess.run([TAR, "--version"], capture_output=True, text=True).stdout.strip()
    Path(out_json).write_text(json.dumps(
        {"source_bytes": source, "files": len(expected), "runs": runs, "baseline_tool": tar_version,
         "results": results}, indent=2) + "\n")


if __name__ == "__main__":
    main()
