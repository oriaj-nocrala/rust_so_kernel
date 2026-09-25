//! Colours, as `0x00RRGGBB` — the pixel format of the compositor's
//! buffers. Copied from `kernel/src/drivers/framebuffer_console.rs`, which
//! is where their reasons are written down (One Dark-like, because VGA's
//! blue is unreadable on black; palette entry 0 is a grey as a foreground
//! but real black as a background).

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << 16 | (g as u32) << 8 | b as u32
}

pub const DEFAULT_FG: u32 = rgb(0xD8, 0xDB, 0xE0);
pub const DEFAULT_BG: u32 = rgb(0, 0, 0);

pub const ANSI: [u32; 8] = [
    rgb(0x3F, 0x44, 0x4E), // black (a dark grey, so it still shows)
    rgb(0xE0, 0x6C, 0x75), // red
    rgb(0x98, 0xC3, 0x79), // green
    rgb(0xE5, 0xC0, 0x7B), // yellow
    rgb(0x61, 0xAF, 0xEF), // blue
    rgb(0xC6, 0x78, 0xDD), // magenta
    rgb(0x56, 0xB6, 0xC2), // cyan
    rgb(0xD8, 0xDB, 0xE0), // white
];

pub const BRIGHT: [u32; 8] = [
    rgb(0x7F, 0x84, 0x8E),
    rgb(0xFF, 0x7A, 0x85),
    rgb(0xB5, 0xE8, 0x90),
    rgb(0xFF, 0xD6, 0x8A),
    rgb(0x8C, 0xC8, 0xFF),
    rgb(0xDD, 0x9C, 0xF5),
    rgb(0x7F, 0xD8, 0xE3),
    rgb(0xFF, 0xFF, 0xFF),
];

/// `SGR 30-37` / `90-97`.
pub fn fg(idx: u8, bright: bool) -> u32 {
    let i = idx as usize & 7;
    if bright { BRIGHT[i] } else { ANSI[i] }
}

/// `SGR 40-47`: entry 0 is the screen's black, not the foreground grey.
pub fn bg(idx: u8) -> u32 {
    if idx & 7 == 0 { DEFAULT_BG } else { fg(idx, false) }
}

/// `SGR 48;5;n`, with the same black rule as [`bg`].
pub fn bg256(n: u8) -> u32 {
    if n == 0 { DEFAULT_BG } else { color256(n) }
}

/// `SGR 38;5;n`: the 16 colours, the 6x6x6 cube and 24 greys.
pub fn color256(n: u8) -> u32 {
    match n {
        0..=7 => ANSI[n as usize],
        8..=15 => BRIGHT[(n - 8) as usize],
        16..=231 => {
            let i = n - 16;
            let scale = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            rgb(scale(i / 36), scale((i / 6) % 6), scale(i % 6))
        }
        232..=255 => {
            let v = 8 + (n - 232) * 10;
            rgb(v, v, v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_and_greys_match_xterm() {
        assert_eq!(color256(16), 0x000000);
        assert_eq!(color256(196), 0xFF0000);
        assert_eq!(color256(231), 0xFFFFFF);
        assert_eq!(color256(232), 0x080808);
        assert_eq!(color256(255), 0xEEEEEE);
    }

    #[test]
    fn black_is_grey_as_text_and_black_as_background() {
        assert_ne!(fg(0, false), DEFAULT_BG);
        assert_eq!(bg(0), DEFAULT_BG);
        assert_eq!(bg256(0), DEFAULT_BG);
        assert_eq!(bg(1), fg(1, false));
    }
}
