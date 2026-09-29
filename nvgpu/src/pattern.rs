//! The picture the kernel puts on the HP (phase 5.8): something nothing
//! else draws, so seeing it means the second head scans its own surface.
//!
//! From the top: seven colour bars (75 % white, yellow, cyan, green,
//! magenta, red, blue), a grey ramp, a strip of steps, then a dark panel
//! the kernel writes text on ([`panel`]). A white border 6 pixels wide
//! frames it, so a monitor that overscans (the HP's EDID says it
//! underscans, and the AVI infoframe asks for it) shows a cut border.
//! XRGB8888: `0x00RRGGBB`.

/// Border width in pixels.
pub const BORDER: u32 = 6;
/// The panel's colour: dark blue-grey.
pub const PANEL_BG: u32 = 0x0010_1828;
const BARS: [u32; 7] = [0x00bf_bfbf, 0x00bf_bf00, 0x0000_bfbf, 0x0000_bf00, 0x00bf_00bf, 0x00bf_0000, 0x0000_00bf];

/// Where the text goes: `(x, y, w, h)` of the dark panel, in a `w`x`h`
/// picture (inside the border, the bottom 30 %).
pub fn panel(w: u32, h: u32) -> (u32, u32, u32, u32) {
    let y = h * 70 / 100;
    (BORDER, y, w.saturating_sub(2 * BORDER), h.saturating_sub(y + BORDER))
}

/// The pixel at `(x, y)` of a `w`x`h` picture.
pub fn pixel(x: u32, y: u32, w: u32, h: u32) -> u32 {
    if x < BORDER || y < BORDER || x >= w.saturating_sub(BORDER) || y >= h.saturating_sub(BORDER) {
        return 0x00ff_ffff;
    }
    let (bars_end, ramp_end, steps_end) = (h * 45 / 100, h * 55 / 100, h * 70 / 100);
    if y < bars_end {
        return BARS[((x * 7 / w.max(1)) as usize).min(6)];
    }
    // Position along the picture, 0..=255.
    let level = (x * 256 / w.max(1)).min(255);
    if y < ramp_end {
        return level * 0x0001_0101;
    }
    if y < steps_end {
        // Sixteen steps.
        let step = (level & 0xf0) | (level >> 4);
        return step * 0x0001_0101;
    }
    PANEL_BG
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn border_bars_ramp_and_panel() {
        let (w, h) = (1920, 1080);
        assert_eq!(pixel(0, 500, w, h), 0xffffff);
        assert_eq!(pixel(w - 1, 500, w, h), 0xffffff);
        assert_eq!(pixel(500, 0, w, h), 0xffffff);
        assert_eq!(pixel(500, h - 1, w, h), 0xffffff);
        assert_eq!(pixel(BORDER, BORDER, w, h), BARS[0]);
        // Seven bars, in order, the last one reaching the border.
        let bars: alloc::vec::Vec<u32> = (0..7).map(|i| pixel(i * w / 7 + 10, 100, w, h)).collect();
        assert_eq!(bars, BARS);
        assert_eq!(pixel(w - BORDER - 1, 100, w, h), BARS[6]);
        // The ramp rises and the steps do not change inside a step.
        assert!(pixel(1800, 500, w, h) > pixel(200, 500, w, h));
        assert_eq!(pixel(200, 620, w, h), pixel(205, 620, w, h));
        assert_ne!(pixel(200, 620, w, h), pixel(1800, 620, w, h));
        // The panel starts where `panel` says.
        let (px, py, pw, ph) = panel(w, h);
        assert_eq!(pixel(px + pw / 2, py + ph / 2, w, h), PANEL_BG);
        assert_eq!((px + pw + BORDER, py + ph + BORDER), (w, h));
    }

    #[test]
    fn a_tiny_picture_does_not_panic() {
        for (w, h) in [(0, 0), (1, 1), (12, 12), (13, 40)] {
            for y in 0..h {
                for x in 0..w {
                    pixel(x, y, w, h);
                }
            }
            panel(w, h);
        }
    }
}
