# ACX research notes

`crates/acx-research` holds experimental, exactly reversible building
blocks for image and video coding (PRD §2.3, TASK 8). It is compiled only
with `--features experimental`, nothing in the archive format or
`acx create` uses it, and it stays that way until a transform passes the
PRD's correctness, compatibility, and benchmark gates (§7.5).

| Module               | What it does                                                          | Exactness check                                   |
| -------------------- | --------------------------------------------------------------------- | ------------------------------------------------- |
| `frame_hashing`      | Finds exact duplicate frames: BLAKE3 nominates, byte equality decides | One changed byte is never a duplicate             |
| `color_transforms`   | YCoCg-R integer RGB transform (Y 8-bit, Co/Cg 9-bit)                  | All 16,777,216 RGB values round-trip              |
| `block_matching`     | `BlockMatcher` trait; exhaustive SAD search baseline                  | Recovers a known (−3, +2) motion exactly          |
| `temporal_residuals` | Motion-compensated prediction; wrapping byte residuals                | Exact reconstruction for good, bad, and zero motion fields |

```bash
cargo test -p acx-research --features experimental --release -- --include-ignored --nocapture
```

## Experiment 1: residual coding vs raw frames

Question: on moving content, does Zstandard compress motion-compensated
residuals better than the raw frames? 30 grayscale frames, 96×64, a
textured pattern moving (1, 0.5) px per frame plus ±1 noise; 16×16 blocks,
search range ±4, Zstandard level 9 (ACX balanced) for both.

| Input                         | Bytes   | vs raw  |
| ----------------------------- | ------: | ------: |
| Raw frames                    | 184,320 |  100%   |
| Zstandard(raw frames)         | 169,634 |   92%   |
| Zstandard(residuals) + vectors | 69,091 |   37%   |

Every residual frame reconstructed its source exactly. Plain Zstandard
barely helps because the content moves; prediction removes the motion and
leaves mostly noise, which compresses 2.5× better.

Limits: synthetic content, tiny frames, grayscale only, and exhaustive
search (slow, O(range²) per block). Next steps before any integration:
real screen recordings and camera footage, colour via YCoCg-R planes, a
faster search that must match exhaustive SAD on quality, and comparison
against FFV1 and lossless AV1 (PRD §7.1). Under `FileExact`, a stored
transform would also need the original container bytes, so this belongs
to the opt-in `MediaExact` contract (PRD §1.2).
