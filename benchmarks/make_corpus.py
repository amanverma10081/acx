"""Generates the ACX mixed-media benchmark corpus, deterministically.

    python benchmarks/make_corpus.py OUT_DIR      (Python 3.9 or later)

Same bytes on every machine (seeded), so results are comparable without
redistributing data (PRD §7.2). `acx benchmark` prints the corpus manifest
hash; check it against docs/BENCHMARKS.md before comparing numbers.

About 575 MiB in five parts that stand in for a real media library:
  logs/      text logs and CSV                         (very compressible)
  frames/    raw RGB 1280x720 screen-capture frames,   (compressible,
             mostly static, some exact repeats          many duplicates)
  photos/    incompressible files standing in for JPEG/MP4/HEIC
  backup/    exact copies of half the photos           (dedup target)
  small/     2,000 small JSON-like files               (per-file overhead)
"""

import random
import sys
from pathlib import Path

MiB = 1 << 20


def write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data)


def logs(rng: random.Random, out: Path) -> None:
    levels = ["INFO", "WARN", "DEBUG", "ERROR"]
    paths = ["/api/v1/assets", "/api/v1/upload", "/thumbs", "/health", "/stream/hls"]
    for day in range(10):
        lines = []
        size = 0
        while size < 8 * MiB:
            line = (
                f"2026-10-{day + 1:02d}T{rng.randrange(24):02d}:{rng.randrange(60):02d}:"
                f"{rng.randrange(60):02d}.{rng.randrange(1000):03d}Z {rng.choice(levels)} "
                f"req={rng.getrandbits(48):012x} path={rng.choice(paths)} "
                f"status={rng.choice([200, 200, 200, 201, 304, 404, 500])} "
                f"bytes={rng.randrange(100, 5_000_000)} ms={rng.randrange(1, 900)}\n"
            )
            lines.append(line)
            size += len(line)
        write(out / "logs" / f"server-{day + 1:02d}.log", "".join(lines).encode())
    rows = ["id,camera,iso,shutter,aperture,width,height,taken"]
    for i in range(400_000):
        rows.append(
            f"{i},{rng.choice(['A7IV', 'R6', 'Z8', 'X-T5'])},{rng.choice([100, 200, 400, 800, 3200])},"
            f"1/{rng.choice([60, 125, 250, 1000])},f/{rng.choice([1.8, 2.8, 4, 8])},6000,4000,"
            f"2026-{rng.randrange(1, 13):02d}-{rng.randrange(1, 29):02d}"
        )
    write(out / "logs" / "catalog.csv", ("\n".join(rows) + "\n").encode())


def frames(rng: random.Random, out: Path) -> None:
    w, h = 1280, 720
    # A desktop: gradient background, a few flat windows, a moving cursor.
    bg = bytearray()
    for y in range(h):
        for x in range(w):
            bg += bytes((x * 255 // w, y * 255 // h, 128))
    previous = None
    for i in range(60):
        if previous is not None and i % 4 == 3:
            data = previous  # an exact repeat: nothing changed on screen
        else:
            f = bytearray(bg)
            for k in range(3):  # windows
                x0, y0 = 100 + k * 350, 120 + k * 60
                colour = bytes((240 - k * 30, 240, 245))
                for y in range(y0, y0 + 300):
                    off = (y * w + x0) * 3
                    f[off:off + 320 * 3] = colour * 320
            cx, cy = (i * 19) % (w - 16), (i * 11) % (h - 16)  # cursor
            for y in range(cy, cy + 16):
                off = (y * w + cx) * 3
                f[off:off + 48] = b"\x00" * 48
            data = bytes(f)
        write(out / "frames" / f"frame-{i:04d}.rgb", data)
        previous = data


def photos(rng: random.Random, out: Path) -> None:
    exts = ["jpg", "heic", "mp4", "mov"]
    for i in range(60):
        size = rng.randrange(1 * MiB, 6 * MiB)
        data = rng.randbytes(size)
        name = f"IMG_{4000 + i}.{exts[i % 4]}"
        write(out / "photos" / f"2026-{i % 12 + 1:02d}" / name, data)
        if i % 2 == 0:
            write(out / "backup" / "phone-2026" / name, data)


def small(rng: random.Random, out: Path) -> None:
    for i in range(2000):
        body = "{" + ", ".join(
            f'"k{j}": {rng.randrange(10_000)}' for j in range(rng.randrange(20, 200))
        ) + "}\n"
        write(out / "small" / f"{i // 100:02d}" / f"meta-{i:04d}.json", body.encode())


def main() -> None:
    out = Path(sys.argv[1])
    if out.exists() and any(out.iterdir()):
        sys.exit(f"{out} is not empty")
    rng = random.Random(20261104)
    for part in (logs, frames, photos, small):
        part(rng, out)
        print("wrote", part.__name__)


if __name__ == "__main__":
    main()
