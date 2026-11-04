//! Experimental reversible transforms for images and video (PRD §2.3
//! `acx-research`, TASK 8).
//!
//! Nothing here is used by the archive format or `acx create`. Everything is
//! compiled only with `--features experimental`, and every transform must
//! round-trip exactly: these are lossless building blocks, not codecs.
//!
//! Frames are plain byte buffers: 8-bit grayscale (`width * height` bytes)
//! for motion search and residuals, interleaved RGB8 for colour transforms.

#![cfg(feature = "experimental")]

pub mod frame_hashing {
    //! Exact duplicate-frame detection. Hash equality only nominates a
    //! candidate; frames are equal only if their bytes are (PRD: never use
    //! perceptual hashes as proof of equality).

    use std::collections::HashMap;

    pub fn frame_digest(frame: &[u8]) -> [u8; 32] {
        *blake3::hash(frame).as_bytes()
    }

    /// For each frame, the index of the first earlier frame with identical
    /// bytes, or `None` if it is the first of its kind.
    pub fn find_duplicate_frames<F: AsRef<[u8]>>(frames: &[F]) -> Vec<Option<usize>> {
        let mut first: HashMap<[u8; 32], Vec<usize>> = HashMap::new();
        frames
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let f = f.as_ref();
                let candidates = first.entry(frame_digest(f)).or_default();
                let hit = candidates
                    .iter()
                    .copied()
                    .find(|&j| frames[j].as_ref() == f);
                if hit.is_none() {
                    candidates.push(i);
                }
                hit
            })
            .collect()
    }
}

pub mod color_transforms {
    //! YCoCg-R: an integer, exactly reversible RGB transform. It
    //! decorrelates channels so later prediction and entropy coding see
    //! smaller numbers. Co and Cg need 9 bits, so they are stored as `i16`.

    /// RGB8 pixel to (Y, Co, Cg).
    pub fn rgb_to_ycocg_r(r: u8, g: u8, b: u8) -> (i16, i16, i16) {
        let (r, g, b) = (i16::from(r), i16::from(g), i16::from(b));
        let co = r - b;
        let t = b + (co >> 1);
        let cg = g - t;
        let y = t + (cg >> 1);
        (y, co, cg)
    }

    /// Exact inverse of [`rgb_to_ycocg_r`] for every value it produces.
    pub fn ycocg_r_to_rgb(y: i16, co: i16, cg: i16) -> (u8, u8, u8) {
        let t = y - (cg >> 1);
        let g = cg + t;
        let b = t - (co >> 1);
        let r = b + co;
        (r as u8, g as u8, b as u8)
    }

    /// Interleaved RGB8 to three planes. `rgb.len()` must be a multiple of 3.
    pub fn forward(rgb: &[u8]) -> [Vec<i16>; 3] {
        let mut planes: [Vec<i16>; 3] = Default::default();
        for px in rgb.chunks_exact(3) {
            let (y, co, cg) = rgb_to_ycocg_r(px[0], px[1], px[2]);
            planes[0].push(y);
            planes[1].push(co);
            planes[2].push(cg);
        }
        planes
    }

    pub fn inverse(planes: &[Vec<i16>; 3]) -> Vec<u8> {
        let [y, co, cg] = planes;
        y.iter()
            .zip(co)
            .zip(cg)
            .flat_map(|((&y, &co), &cg)| {
                let (r, g, b) = ycocg_r_to_rgb(y, co, cg);
                [r, g, b]
            })
            .collect()
    }
}

pub mod block_matching {
    //! Motion search between grayscale frames. The interface is what a
    //! temporal codec needs; the exhaustive SAD search is the simplest
    //! correct implementation and the baseline any faster search must match.

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct MotionVector {
        pub dx: i32,
        pub dy: i32,
    }

    #[derive(Debug, Clone, Copy)]
    pub struct Frame<'a> {
        pub pixels: &'a [u8],
        pub width: usize,
        pub height: usize,
    }

    impl Frame<'_> {
        /// Pixel at (x, y), clamped to the frame edge, so every vector is
        /// valid and decoders need no special cases.
        pub fn at(&self, x: i64, y: i64) -> u8 {
            let x = x.clamp(0, self.width as i64 - 1) as usize;
            let y = y.clamp(0, self.height as i64 - 1) as usize;
            self.pixels[y * self.width + x]
        }
    }

    pub trait BlockMatcher {
        /// Best vector for the `block`×`block` square at (`bx`, `by`) in
        /// `cur`, predicting from `prev`.
        fn best_match(
            &self,
            prev: Frame,
            cur: Frame,
            bx: usize,
            by: usize,
            block: usize,
        ) -> MotionVector;
    }

    /// Tries every vector within ±`range`; ties go to the smallest vector.
    pub struct ExhaustiveSad {
        pub range: i32,
    }

    impl BlockMatcher for ExhaustiveSad {
        fn best_match(
            &self,
            prev: Frame,
            cur: Frame,
            bx: usize,
            by: usize,
            block: usize,
        ) -> MotionVector {
            let sad = |mv: MotionVector| -> u64 {
                let mut total = 0u64;
                for y in by..(by + block).min(cur.height) {
                    for x in bx..(bx + block).min(cur.width) {
                        let p = prev.at(x as i64 + i64::from(mv.dx), y as i64 + i64::from(mv.dy));
                        total += u64::from(cur.pixels[y * cur.width + x].abs_diff(p));
                    }
                }
                total
            };
            let mut best = (sad(MotionVector::default()), 0, MotionVector::default());
            for dy in -self.range..=self.range {
                for dx in -self.range..=self.range {
                    let mv = MotionVector { dx, dy };
                    let candidate = (sad(mv), dx.abs() + dy.abs(), mv);
                    if (candidate.0, candidate.1) < (best.0, best.1) {
                        best = candidate;
                    }
                }
            }
            best.2
        }
    }
}

pub mod temporal_residuals {
    //! Motion-compensated prediction and residuals. Residuals are the
    //! wrapping byte difference, so reconstruction is exact for any
    //! prediction, good or bad: prediction quality changes only size.

    use crate::block_matching::{BlockMatcher, Frame, MotionVector};

    /// One vector per block, row-major.
    pub fn motion_field(
        matcher: &impl BlockMatcher,
        prev: Frame,
        cur: Frame,
        block: usize,
    ) -> Vec<MotionVector> {
        let mut field = Vec::new();
        for by in (0..cur.height).step_by(block) {
            for bx in (0..cur.width).step_by(block) {
                field.push(matcher.best_match(prev, cur, bx, by, block));
            }
        }
        field
    }

    fn predict(prev: Frame, field: &[MotionVector], block: usize) -> Vec<u8> {
        let blocks_x = prev.width.div_ceil(block);
        let mut out = Vec::with_capacity(prev.width * prev.height);
        for y in 0..prev.height {
            for x in 0..prev.width {
                let mv = field[(y / block) * blocks_x + x / block];
                out.push(prev.at(x as i64 + i64::from(mv.dx), y as i64 + i64::from(mv.dy)));
            }
        }
        out
    }

    pub fn residual(prev: Frame, cur: Frame, field: &[MotionVector], block: usize) -> Vec<u8> {
        predict(prev, field, block)
            .iter()
            .zip(cur.pixels)
            .map(|(p, c)| c.wrapping_sub(*p))
            .collect()
    }

    pub fn reconstruct(
        prev: Frame,
        residual: &[u8],
        field: &[MotionVector],
        block: usize,
    ) -> Vec<u8> {
        predict(prev, field, block)
            .iter()
            .zip(residual)
            .map(|(p, r)| p.wrapping_add(*r))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::block_matching::{BlockMatcher, ExhaustiveSad, Frame, MotionVector};
    use super::color_transforms::*;
    use super::frame_hashing::find_duplicate_frames;
    use super::temporal_residuals::{motion_field, reconstruct, residual};

    const W: usize = 96;
    const H: usize = 64;

    /// A textured frame shifted by (sx, sy), plus a little seeded noise.
    fn frame(sx: i64, sy: i64, noise_seed: u64) -> Vec<u8> {
        let mut n = noise_seed | 1;
        (0..H as i64)
            .flat_map(|y| (0..W as i64).map(move |x| (x, y)))
            .map(|(x, y)| {
                let (u, v) = (x - sx, y - sy);
                let base = ((u * 7 + v * 3) ^ (u * v / 5)) as u8;
                n ^= n << 13;
                n ^= n >> 7;
                n ^= n << 17;
                base.wrapping_add(if noise_seed == 0 { 0 } else { (n % 3) as u8 })
            })
            .collect()
    }

    fn view(p: &[u8]) -> Frame<'_> {
        Frame {
            pixels: p,
            width: W,
            height: H,
        }
    }

    #[test]
    fn ycocg_r_round_trips_every_rgb_value() {
        for r in 0..=255u8 {
            for g in 0..=255u8 {
                for b in 0..=255u8 {
                    let (y, co, cg) = rgb_to_ycocg_r(r, g, b);
                    assert_eq!(ycocg_r_to_rgb(y, co, cg), (r, g, b));
                    assert!((0..=255).contains(&y) && (-255..=255).contains(&co));
                }
            }
        }
        let img: Vec<u8> = (0..3000u32).map(|i| (i * 37 % 251) as u8).collect();
        assert_eq!(inverse(&forward(&img)), img);
    }

    #[test]
    fn duplicate_frames_are_found_exactly() {
        let a = frame(0, 0, 0);
        let b = frame(1, 0, 0);
        let mut a2 = a.clone();
        let frames = vec![a.clone(), b.clone(), a.clone(), b, a2.clone()];
        assert_eq!(
            find_duplicate_frames(&frames),
            [None, None, Some(0), Some(1), Some(0)]
        );
        a2[0] ^= 1; // one byte different: never a duplicate
        assert_eq!(find_duplicate_frames(&[a, a2]), [None, None]);
    }

    #[test]
    fn block_matching_recovers_known_motion() {
        let prev = frame(0, 0, 0);
        let cur = frame(3, -2, 0); // content moved right 3, up 2
        let mv = ExhaustiveSad { range: 4 }.best_match(view(&prev), view(&cur), 32, 16, 16);
        assert_eq!(mv, MotionVector { dx: -3, dy: 2 });
    }

    #[test]
    fn residuals_reconstruct_exactly_for_any_motion_field() {
        let prev = frame(0, 0, 11);
        let cur = frame(2, 1, 12);
        let good = motion_field(&ExhaustiveSad { range: 3 }, view(&prev), view(&cur), 16);
        let blocks = good.len();
        let bad = vec![MotionVector { dx: 7, dy: -9 }; blocks];
        for field in [good, bad, vec![MotionVector::default(); blocks]] {
            let res = residual(view(&prev), view(&cur), &field, 16);
            assert_eq!(reconstruct(view(&prev), &res, &field, 16), cur);
        }
    }

    /// The research question: does motion-compensated residual coding beat
    /// compressing raw frames with Zstandard? Results go in docs/RESEARCH.md.
    /// Run: `cargo test -p acx-research --features experimental --release -- --ignored --nocapture`
    #[test]
    #[ignore = "experiment; prints results"]
    fn residual_vs_raw_experiment() {
        let level = acx_core::CompressionMode::Balanced.zstd_level();
        let z = |d: &[u8]| {
            acx_codecs::compress(acx_core::CodecId::Zstd, d, level)
                .unwrap()
                .len()
        };
        let frames: Vec<Vec<u8>> = (0..30).map(|t| frame(t, t / 2, 100 + t as u64)).collect();
        let raw: Vec<u8> = frames.concat();
        let mut residuals = frames[0].clone();
        for t in 1..frames.len() {
            let (p, c) = (view(&frames[t - 1]), view(&frames[t]));
            let field = motion_field(&ExhaustiveSad { range: 4 }, p, c, 16);
            let res = residual(p, c, &field, 16);
            assert_eq!(reconstruct(p, &res, &field, 16), frames[t]);
            residuals.extend(res);
        }
        println!(
            "30 frames {W}x{H}, moving texture + noise: raw {} B, zstd(raw) {} B, zstd(residuals) {} B (+{} B vectors)",
            raw.len(),
            z(&raw),
            z(&residuals),
            29 * (W / 16) * (H / 16) * 2
        );
    }
}
