//! Porter-Duff "over" for images with an alpha channel: icons, cursors,
//! anything decoded from a PNG (`img`).
//!
//! Source pixels are **premultiplied** `0xAARRGGBB`: each colour channel is
//! already multiplied by alpha, so `over` is `dst = src + dst·(255−a)/255`
//! per channel — no division by alpha, and scaling or filtering such an
//! image never bleeds the colour of transparent pixels into the edges.
//!
//! The destination is the usual opaque `0x00RRGGBB`; its top byte is left
//! exactly as it was.
//!
//! [`over_row`] picks the widest path this CPU has: AVX2 (8 pixels per
//! step; the target is a Zen 3, and the kernel enables AVX through XSAVE —
//! `kernel/src/process/fpu.rs`), else scalar. There is no GPU driver, so
//! this is where compositing happens. Both paths give identical bits:
//! `x/255` is rounded exactly, `(t + (t >> 8)) >> 8` with `t = x + 128`.

use core::sync::atomic::{AtomicU8, Ordering};

const ALPHA: u32 = 0xFF00_0000;

/// `x / 255` rounded to nearest, exact for `x <= 255 * 255`.
#[inline(always)]
fn div255(x: u32) -> u32 {
    let t = x + 128;
    (t + (t >> 8)) >> 8
}

/// One pixel of `src` over `dst`.
#[inline(always)]
pub fn over(dst: u32, src: u32) -> u32 {
    let ia = 255 - (src >> 24);
    let mut out = 0;
    for shift in [0, 8, 16] {
        let d = dst >> shift & 0xFF;
        let s = src >> shift & 0xFF;
        // Saturating: a colour above its alpha (not premultiplied) must not
        // carry into the next channel.
        out |= (s + div255(d * ia)).min(255) << shift;
    }
    out | dst & ALPHA
}

/// `src` over `dst`, pixel by pixel; `src` must be at least as long.
pub fn over_row(dst: &mut [u32], src: &[u32]) {
    let src = &src[..dst.len()];
    #[cfg(target_arch = "x86_64")]
    if has_avx2() {
        // SAFETY: AVX2 is present and enabled by the OS.
        unsafe { avx2::over_row(dst, src) };
        return;
    }
    over_row_scalar(dst, src);
}

/// The reference path, and the one on CPUs without AVX2.
pub fn over_row_scalar(dst: &mut [u32], src: &[u32]) {
    for (d, &s) in dst.iter_mut().zip(src) {
        match s >> 24 {
            0 if s == 0 => {}
            255 => *d = s & !ALPHA | *d & ALPHA,
            _ => *d = over(*d, s),
        }
    }
}

/// 0 unknown, 1 no, 2 yes.
static AVX2: AtomicU8 = AtomicU8::new(0);

/// AVX2 in CPUID, and the OS saving ymm state (OSXSAVE, XCR0 bits 1-2):
/// without the latter every VEX instruction is #UD.
#[cfg(target_arch = "x86_64")]
pub fn has_avx2() -> bool {
    match AVX2.load(Ordering::Relaxed) {
        0 => {
            let yes = detect_avx2();
            AVX2.store(if yes { 2 } else { 1 }, Ordering::Relaxed);
            yes
        }
        v => v == 2,
    }
}

#[cfg(target_arch = "x86_64")]
fn detect_avx2() -> bool {
    use core::arch::x86_64::{__cpuid, __cpuid_count};
    let leaf1 = __cpuid(1);
    if leaf1.ecx & (1 << 27) == 0 || leaf1.ecx & (1 << 28) == 0 {
        return false; // no OSXSAVE or no AVX
    }
    let xcr0: u32;
    // SAFETY: OSXSAVE is set, so XGETBV exists.
    unsafe { core::arch::asm!("xgetbv", in("ecx") 0, out("eax") xcr0, out("edx") _, options(nomem, nostack)) };
    xcr0 & 0b110 == 0b110 && __cpuid(0).eax >= 7 && __cpuid_count(7, 0).ebx & (1 << 5) != 0
}

#[cfg(target_arch = "x86_64")]
pub(crate) mod avx2 {
    use core::arch::x86_64::*;

    /// Eight pixels per step; the tail (under 8) goes through the scalar
    /// path. Each step first checks the two cases icons are mostly made
    /// of — all eight transparent (nothing to do) or all opaque (a copy) —
    /// before paying for the blend.
    ///
    /// # Safety
    /// The CPU must have AVX2 and the OS must have enabled it; `src.len()`
    /// must equal `dst.len()`.
    #[target_feature(enable = "avx2")]
    pub unsafe fn over_row(dst: &mut [u32], src: &[u32]) {
        debug_assert_eq!(dst.len(), src.len());
        let n = dst.len() / 8 * 8;
        let alpha = _mm256_set1_epi32(0xFF00_0000u32 as i32);
        let zero = _mm256_setzero_si256();
        let round = _mm256_set1_epi16(128);
        // Byte 3 of each pixel (its alpha) into all four of its bytes; the
        // shuffle works within each 128-bit lane, so the indices repeat.
        let spread = _mm256_setr_epi8(
            3, 3, 3, 3, 7, 7, 7, 7, 11, 11, 11, 11, 15, 15, 15, 15,
            3, 3, 3, 3, 7, 7, 7, 7, 11, 11, 11, 11, 15, 15, 15, 15,
        );
        let ones = _mm256_set1_epi8(-1);
        let mut i = 0;
        while i < n {
            // SAFETY: `i + 8 <= n <= len` for both slices.
            let sp = unsafe { src.as_ptr().add(i) } as *const __m256i;
            let dp = unsafe { dst.as_mut_ptr().add(i) } as *mut __m256i;
            let s = unsafe { _mm256_loadu_si256(sp) };
            if _mm256_testz_si256(s, s) != 0 {
                i += 8;
                continue;
            }
            let d = unsafe { _mm256_loadu_si256(dp) };
            let sa = _mm256_and_si256(s, alpha);
            let out = if _mm256_movemask_epi8(_mm256_cmpeq_epi32(sa, alpha)) == -1 {
                s
            } else {
                // 255 - a is !a for a byte.
                let ia = _mm256_xor_si256(_mm256_shuffle_epi8(s, spread), ones);
                let lo = scale(_mm256_unpacklo_epi8(d, zero), _mm256_unpacklo_epi8(ia, zero), round);
                let hi = scale(_mm256_unpackhi_epi8(d, zero), _mm256_unpackhi_epi8(ia, zero), round);
                _mm256_adds_epu8(_mm256_packus_epi16(lo, hi), s)
            };
            // Colour from the blend, top byte from the destination.
            let out = _mm256_or_si256(_mm256_andnot_si256(alpha, out), _mm256_and_si256(d, alpha));
            unsafe { _mm256_storeu_si256(dp, out) };
            i += 8;
        }
        super::over_row_scalar(&mut dst[n..], &src[n..]);
    }

    /// `d · ia / 255` rounded, on sixteen 16-bit lanes (`d, ia <= 255`).
    #[inline]
    #[target_feature(enable = "avx2")]
    fn scale(d: __m256i, ia: __m256i, round: __m256i) -> __m256i {
        let t = _mm256_add_epi16(_mm256_mullo_epi16(d, ia), round);
        _mm256_srli_epi16(_mm256_add_epi16(t, _mm256_srli_epi16(t, 8)), 8)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::{vec, vec::Vec};

    /// Deterministic noise (xorshift).
    fn noise(seed: u64, n: usize) -> impl Iterator<Item = u32> {
        let mut v = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..n).map(move |_| {
            v ^= v << 13;
            v ^= v >> 7;
            v ^= v << 17;
            v as u32
        })
    }

    /// Premultiplied pixels with runs of fully transparent and fully
    /// opaque ones, so every AVX2 step kind (skip, copy, blend, mixed)
    /// occurs.
    fn icon_like(seed: u64, n: usize) -> Vec<u32> {
        noise(seed, n)
            .enumerate()
            .map(|(i, r)| {
                let a = match (i / 8 + seed as usize) % 4 {
                    0 => 0,
                    1 => 255,
                    _ => r >> 24,
                };
                let ch = |sh: u32| (r >> sh & 0xFF) * a / 255;
                a << 24 | ch(16) << 16 | ch(8) << 8 | ch(0)
            })
            .collect()
    }

    #[test]
    fn over_matches_exact_rounding_for_every_alpha_and_destination() {
        for a in 0..=255u32 {
            for d in 0..=255u32 {
                for s in [0, a / 3, a] {
                    let want = ((s as f64 + d as f64 * (255 - a) as f64 / 255.0).round() as u32).min(255);
                    let got = over(d * 0x010101, a << 24 | s * 0x010101);
                    assert_eq!(got, want * 0x010101, "a={a} d={d} s={s}");
                }
            }
        }
    }

    #[test]
    fn transparent_keeps_and_opaque_replaces() {
        assert_eq!(over(0x123456, 0), 0x123456);
        assert_eq!(over(0x123456, 0xFFABCDEF), 0xABCDEF);
    }

    #[test]
    fn destination_top_byte_is_untouched() {
        let mut dst = [0xAB12_3456u32; 20];
        let src = icon_like(7, 20);
        over_row(&mut dst, &src);
        assert!(dst.iter().all(|&p| p >> 24 == 0xAB));
    }

    #[test]
    fn colour_above_alpha_saturates_instead_of_carrying() {
        // Not premultiplied: 0xFF blue at alpha 0x80 over white. Blue
        // clamps at 0xFF; red and green get only white's 127/255 share.
        let mut dst = [0xFFFFFFu32; 9];
        let src = [0x8000_00FFu32; 9];
        over_row(&mut dst, &src);
        assert!(dst.iter().all(|&p| p == 0x7F7FFF), "{:x?}", dst);
    }

    #[test]
    fn avx2_is_bit_identical_to_scalar() {
        if !has_avx2() {
            std::eprintln!("no AVX2 on this host; nothing to compare");
            return;
        }
        for len in 0..70 {
            for seed in 0..8 {
                // Raw noise too: colours above alpha, every alpha.
                let srcs = [icon_like(seed, len), noise(seed + 100, len).collect()];
                for src in srcs {
                    let base: Vec<u32> = noise(seed + 1000, len).collect();
                    let (mut a, mut b) = (base.clone(), base);
                    over_row_scalar(&mut a, &src);
                    unsafe { avx2::over_row(&mut b, &src) };
                    assert_eq!(a, b, "len={len} seed={seed}");
                }
            }
        }
    }

    /// `cargo test --release -- --ignored --nocapture bench`: one 1080p
    /// frame's worth of blending, scalar against the dispatched path.
    #[test]
    #[ignore]
    fn bench_over_row() {
        let (w, h) = (1920, 1080);
        let src = icon_like(3, w);
        let mut dst = vec![0x336699u32; w];
        for (name, f) in [("scalar", over_row_scalar as fn(&mut [u32], &[u32])), ("over_row", over_row)] {
            let t = std::time::Instant::now();
            for _ in 0..h * 20 {
                f(&mut dst, core::hint::black_box(&src));
            }
            std::println!("{name}: {:.2} ms per 1920x1080 frame", t.elapsed().as_secs_f64() * 1e3 / 20.0);
        }
    }
}
