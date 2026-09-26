//! `draw` — software 2D drawing for userspace programs.
//!
//! Everything here writes into a plain `&mut [u32]` of `0x00RRGGBB` pixels
//! and knows nothing about where that buffer came from or where it goes:
//! `userspace::gfx` owns the window or the console, this crate owns the
//! pixels. Together they are this system's small SDL — `gfx` the core
//! (window, events, present), `draw` the SDL_gfx/SDL_ttf-like part. There
//! is no GPU driver, so every pixel is the CPU's.
//!
//! `no_std`, no `alloc`, no floating point: written when the userspace
//! target was soft-float, so sub-pixel positions are fixed point ([`FP`]
//! units per pixel) and colours are integer arithmetic. The target has
//! SSE2 now (`userspace/x86_64-constanos.json`); nothing here needed it.
//!
//! - [`color`]: packing, scaling, mixing, additive light, HSV.
//! - [`canvas`]: [`Canvas`], a clipped view of a pixel buffer (with a
//!   stride, so it can be a framebuffer row pitch) and its primitives.
//! - [`font`]: two pixel fonts (3x5 and 5x7) and text drawing.
//! - [`wave`]: a sine-shaped wave without libm, for animation.
//! - `smooth` (feature `noto`): antialiased Noto Sans Mono text, blended
//!   over whatever is already drawn.

#![no_std]

pub mod canvas;
pub mod color;
pub mod font;
#[cfg(feature = "noto")]
pub mod smooth;

pub use canvas::Canvas;
pub use font::{Font, FONT_3X5, FONT_5X7};

/// Fixed point for sub-pixel coordinates and radii: 1/256 of a pixel.
pub const FP: i32 = 256;

/// A sine-shaped wave in `-256..=256` of period `period` (any unit, e.g.
/// ms): 0 at `t = 0`, 256 at a quarter period. Two parabolic arches, within
/// 6% of a real sine — enough for pulsing and bobbing.
pub fn wave(t: i64, period: i64) -> i32 {
    let p = (t.rem_euclid(period) * 1024 / period) as i32;
    let (sign, q) = if p < 512 { (1, p) } else { (-1, p - 512) };
    sign * q * (512 - q) / 256
}

#[cfg(test)]
mod tests {
    use super::wave;

    #[test]
    fn wave_hits_its_quarter_points() {
        assert_eq!(wave(0, 1000), 0);
        assert_eq!(wave(250, 1000), 256);
        assert_eq!(wave(500, 1000), 0);
        assert_eq!(wave(750, 1000), -256);
        assert_eq!(wave(1000, 1000), 0);
    }

    #[test]
    fn wave_is_periodic_and_handles_negative_time() {
        for t in [-1234i64, -1, 0, 7, 999, 5000] {
            assert_eq!(wave(t, 800), wave(t + 800, 800));
            assert!((-256..=256).contains(&wave(t, 800)));
        }
    }
}
