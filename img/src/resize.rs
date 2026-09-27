//! Resampling an [`Image`] to another size: icons drawn at a `HIDPI`
//! screen's scale, or a theme's nearest size fitted to the one asked for.
//!
//! Done once, when the icon is loaded — never per frame: the result is
//! blitted 1:1 by `Canvas::blit_over`.
//!
//! A separable Catmull-Rom (bicubic, `a = -0.5`) filter, in premultiplied
//! space so a transparent pixel's colour never bleeds into an edge. When
//! shrinking, the filter is stretched by the reduction factor, so every
//! source pixel contributes (area-like averaging, no aliasing); when
//! enlarging it interpolates. Near the borders the taps that fall outside
//! are dropped and the rest renormalised. Ringing is clamped: every result
//! is a valid premultiplied pixel (each colour at most its alpha).
//!
//! `f32`: load time, not frame time, so clarity over speed.

use alloc::vec;
use alloc::vec::Vec;

use crate::Image;

/// Catmull-Rom: support 2, passes through the samples.
fn cubic(x: f32) -> f32 {
    let x = if x < 0.0 { -x } else { x };
    if x < 1.0 {
        (1.5 * x - 2.5) * x * x + 1.0
    } else if x < 2.0 {
        ((-0.5 * x + 2.5) * x - 4.0) * x + 2.0
    } else {
        0.0
    }
}

/// For each of `dst` output positions along one axis: the first source
/// index and the (normalised) weights of the taps from there.
struct Taps {
    first: Vec<usize>,
    /// `n` weights per output position (zero-padded).
    weights: Vec<f32>,
    n: usize,
}

fn taps(src: usize, dst: usize) -> Taps {
    let ratio = src as f32 / dst as f32;
    // Shrinking: stretch the filter so it covers `ratio` source pixels.
    let stretch = if ratio > 1.0 { ratio } else { 1.0 };
    let support = 2.0 * stretch;
    let n = (libm_ceil(support) as usize) * 2 + 1;
    let mut first = Vec::with_capacity(dst);
    let mut weights = vec![0.0f32; dst * n];
    for o in 0..dst {
        // Centre of output pixel `o` in source coordinates.
        let centre = (o as f32 + 0.5) * ratio;
        let lo = libm_floor(centre - support).max(0.0) as usize;
        let hi = (libm_ceil(centre + support) as usize).min(src);
        let w = &mut weights[o * n..(o + 1) * n];
        let mut sum = 0.0;
        for (k, i) in (lo..hi).enumerate().take(n) {
            let v = cubic((i as f32 + 0.5 - centre) / stretch);
            w[k] = v;
            sum += v;
        }
        if sum != 0.0 {
            for v in w.iter_mut() {
                *v /= sum;
            }
        }
        first.push(lo);
    }
    Taps { first, weights, n }
}

// `core` has no float rounding functions without `std`; these are exact
// for the ranges used here (|x| < 2^23).
fn libm_floor(x: f32) -> f32 {
    let t = x as i32 as f32;
    if t > x { t - 1.0 } else { t }
}

fn libm_ceil(x: f32) -> f32 {
    let t = x as i32 as f32;
    if t < x { t + 1.0 } else { t }
}

/// The channels of a premultiplied pixel as `[a, r, g, b]`.
fn split(p: u32) -> [f32; 4] {
    [(p >> 24) as f32, (p >> 16 & 0xFF) as f32, (p >> 8 & 0xFF) as f32, (p & 0xFF) as f32]
}

/// Rounds and clamps back to a valid premultiplied pixel.
fn join(c: [f32; 4]) -> u32 {
    let round = |v: f32, max: u32| -> u32 {
        let v = v + 0.5;
        if v <= 0.0 { 0 } else { (v as u32).min(max) }
    };
    let a = round(c[0], 255);
    a << 24 | round(c[1], a) << 16 | round(c[2], a) << 8 | round(c[3], a)
}

impl Image {
    /// This image resampled to `w x h`. An empty source or target gives an
    /// empty image; the same size gives a copy.
    ///
    /// Panics if `w` or `h` exceeds [`MAX_SIDE`](crate::MAX_SIDE).
    pub fn resized(&self, w: usize, h: usize) -> Image {
        assert!(w <= crate::MAX_SIDE && h <= crate::MAX_SIDE, "resize to {}x{}", w, h);
        if w == 0 || h == 0 || self.w == 0 || self.h == 0 {
            return Image { w, h, px: vec![0; w * h] };
        }
        if (w, h) == (self.w, self.h) {
            return self.clone();
        }
        // Horizontal pass: self.h rows of `w` pixels, 4 channels each.
        let tx = taps(self.w, w);
        let mut mid = vec![[0.0f32; 4]; w * self.h];
        for y in 0..self.h {
            let row = &self.px[y * self.w..(y + 1) * self.w];
            for x in 0..w {
                let first = tx.first[x];
                let mut acc = [0.0f32; 4];
                for (k, &wt) in tx.weights[x * tx.n..(x + 1) * tx.n].iter().enumerate() {
                    if wt == 0.0 || first + k >= self.w {
                        continue;
                    }
                    let c = split(row[first + k]);
                    for ch in 0..4 {
                        acc[ch] += wt * c[ch];
                    }
                }
                mid[y * w + x] = acc;
            }
        }
        // Vertical pass.
        let ty = taps(self.h, h);
        let mut px = vec![0u32; w * h];
        for y in 0..h {
            let first = ty.first[y];
            for x in 0..w {
                let mut acc = [0.0f32; 4];
                for (k, &wt) in ty.weights[y * ty.n..(y + 1) * ty.n].iter().enumerate() {
                    if wt == 0.0 || first + k >= self.h {
                        continue;
                    }
                    let c = mid[(first + k) * w + x];
                    for ch in 0..4 {
                        acc[ch] += wt * c[ch];
                    }
                }
                px[y * w + x] = join(acc);
            }
        }
        Image { w, h, px }
    }
}

/// Of the sizes a theme has (`available`, square, any order), the one to
/// load for `target` pixels: the smallest at least as big (shrinking loses
/// less than enlarging), else the biggest. `None` if there are none.
pub fn pick_size(available: &[usize], target: usize) -> Option<usize> {
    let bigger = available.iter().copied().filter(|&s| s >= target).min();
    bigger.or_else(|| available.iter().copied().max())
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::premultiply;

    fn noise(seed: u32, n: usize) -> Vec<u32> {
        let mut v = seed | 1;
        (0..n)
            .map(|_| {
                v ^= v << 13;
                v ^= v >> 17;
                v ^= v << 5;
                let a = (v >> 24) as u8;
                premultiply(v as u8, (v >> 8) as u8, (v >> 16) as u8, a)
            })
            .collect()
    }

    fn solid(w: usize, h: usize, p: u32) -> Image {
        Image { w, h, px: vec![p; w * h] }
    }

    #[test]
    fn same_size_is_a_copy_and_empty_is_empty() {
        let im = Image { w: 5, h: 3, px: noise(1, 15) };
        assert_eq!(im.resized(5, 3), im);
        assert_eq!(im.resized(0, 7).px.len(), 0);
        assert_eq!(solid(0, 0, 0).resized(4, 4).px, vec![0; 16]);
    }

    #[test]
    fn a_flat_image_stays_flat_at_any_size() {
        for p in [0xFFFF_8040u32, 0x8040_2010, 0] {
            let im = solid(7, 5, p);
            for (w, h) in [(14, 10), (21, 15), (3, 2), (1, 1), (8, 13), (100, 3)] {
                let r = im.resized(w, h);
                assert!(r.px.iter().all(|&q| q == p), "{p:#x} to {w}x{h}: {:x?}", &r.px[..4.min(r.px.len())]);
            }
        }
    }

    #[test]
    fn every_result_is_valid_premultiplied() {
        // Noise is the worst case for bicubic ringing.
        let im = Image { w: 17, h: 11, px: noise(7, 17 * 11) };
        for (w, h) in [(34, 22), (51, 33), (8, 5), (3, 3), (40, 7)] {
            for p in im.resized(w, h).px {
                let a = p >> 24;
                assert!((p >> 16 & 0xFF) <= a && (p >> 8 & 0xFF) <= a && (p & 0xFF) <= a, "{p:#010x}");
            }
        }
    }

    #[test]
    fn transparent_neighbours_do_not_tint_an_edge() {
        // Opaque red beside fully transparent pixels (premultiplied: 0),
        // enlarged: every partly covered pixel is still pure red.
        let mut im = solid(4, 4, 0);
        for y in 0..4 {
            im.px[y * 4] = 0xFFFF_0000;
            im.px[y * 4 + 1] = 0xFFFF_0000;
        }
        for p in im.resized(12, 12).px {
            let (a, r, g, b) = (p >> 24, p >> 16 & 0xFF, p >> 8 & 0xFF, p & 0xFF);
            assert_eq!((g, b), (0, 0), "{p:#010x}");
            assert!(r.abs_diff(a) <= 1, "{p:#010x}");
        }
    }

    #[test]
    fn shrinking_averages_instead_of_aliasing() {
        // A one-pixel black/white checkerboard shrunk 3:1. Each output
        // centre falls on one source pixel's centre, so an unstretched
        // filter just samples it: a black/white checkerboard again.
        // Averaging gives grey (not exactly 128: a 3x3 block holds 5 of
        // one colour and 4 of the other).
        let (w, h) = (48, 48);
        let px = (0..w * h).map(|i| if (i % w + i / w) % 2 == 0 { 0xFFFF_FFFF } else { 0xFF00_0000 }).collect();
        let r = Image { w, h, px }.resized(16, 16);
        for y in 2..14 {
            for x in 2..14 {
                let g = r.px[y * 16 + x] & 0xFF;
                assert!(g.abs_diff(128) <= 24, "({x},{y}) = {g}");
            }
        }
    }

    #[test]
    fn enlarging_keeps_samples_and_a_step_monotone_away_from_it() {
        // A left-dark, right-bright step, doubled: the far ends keep their
        // values exactly, the interior ramps.
        let px = (0..8 * 2).map(|i| if i % 8 < 4 { 0xFF20_2020 } else { 0xFFE0_E0E0 }).collect();
        let r = Image { w: 8, h: 2, px }.resized(16, 4);
        let row: Vec<u32> = (0..16).map(|x| r.px[x] & 0xFF).collect();
        assert_eq!((row[0], row[15]), (0x20, 0xE0), "{row:?}");
        assert!(row[5] < row[8] && row[8] < row[10], "{row:?}");
    }

    #[test]
    fn pick_size_prefers_the_smallest_big_enough() {
        let sizes = [16, 32, 48, 64, 128, 256];
        assert_eq!(pick_size(&sizes, 64), Some(64));
        assert_eq!(pick_size(&sizes, 96), Some(128));
        assert_eq!(pick_size(&sizes, 20), Some(32));
        assert_eq!(pick_size(&sizes, 512), Some(256));
        assert_eq!(pick_size(&[48, 16], 8), Some(16));
        assert_eq!(pick_size(&[], 64), None);
    }
}
