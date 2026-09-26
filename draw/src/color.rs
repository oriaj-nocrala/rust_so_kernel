//! `0x00RRGGBB` colours in integer arithmetic. Factors are in 256ths:
//! `scale(c, 256)` is `c`, `mix(a, b, 0)` is `a`, `mix(a, b, 256)` is `b`.

/// Packs channels, clamping each to `0..=255`.
pub fn rgb(r: i32, g: i32, b: i32) -> u32 {
    (r.clamp(0, 255) as u32) << 16 | (g.clamp(0, 255) as u32) << 8 | b.clamp(0, 255) as u32
}

/// The `(r, g, b)` channels of `c`.
pub fn channels(c: u32) -> (i32, i32, i32) {
    ((c >> 16 & 0xFF) as i32, (c >> 8 & 0xFF) as i32, (c & 0xFF) as i32)
}

/// `c` scaled by `k`/256 (values over 256 brighten, clamped).
pub fn scale(c: u32, k: i32) -> u32 {
    let (r, g, b) = channels(c);
    rgb(r * k >> 8, g * k >> 8, b * k >> 8)
}

/// From `a` towards `b` by `t`/256.
pub fn mix(a: u32, b: u32, t: i32) -> u32 {
    let (ar, ag, ab) = channels(a);
    let (br, bg, bb) = channels(b);
    rgb(ar + ((br - ar) * t >> 8), ag + ((bg - ag) * t >> 8), ab + ((bb - ab) * t >> 8))
}

/// Additive light: channel sums, saturating.
pub fn add(a: u32, b: u32) -> u32 {
    let (ar, ag, ab) = channels(a);
    let (br, bg, bb) = channels(b);
    rgb(ar + br, ag + bg, ab + bb)
}

/// Hue in `0..1536` (six sextants of 256: red, yellow, green, cyan, blue,
/// magenta; any value wraps), saturation and value in `0..=255`.
pub fn hsv(h: i32, s: i32, v: i32) -> u32 {
    let h = h.rem_euclid(1536);
    let f = h & 255;
    let p = v * (255 - s) / 255;
    let q = v * (255 - s * f / 255) / 255;
    let t = v * (255 - s * (255 - f) / 255) / 255;
    match h >> 8 {
        0 => rgb(v, t, p),
        1 => rgb(q, v, p),
        2 => rgb(p, v, t),
        3 => rgb(p, q, v),
        4 => rgb(t, p, v),
        _ => rgb(v, p, q),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_clamps_and_round_trips() {
        assert_eq!(rgb(300, -5, 128), 0xFF0080);
        assert_eq!(channels(0x123456), (0x12, 0x34, 0x56));
        assert_eq!(rgb(0x12, 0x34, 0x56), 0x123456);
    }

    #[test]
    fn scale_identity_black_and_saturation() {
        assert_eq!(scale(0x80C0FF, 256), 0x80C0FF);
        assert_eq!(scale(0x80C0FF, 0), 0);
        assert_eq!(scale(0x808080, 128), 0x404040);
        assert_eq!(scale(0x80C0FF, 1024), 0xFFFFFF);
    }

    #[test]
    fn mix_endpoints_and_midpoint() {
        assert_eq!(mix(0x102030, 0xF0E0D0, 0), 0x102030);
        assert_eq!(mix(0x102030, 0xF0E0D0, 256), 0xF0E0D0);
        assert_eq!(mix(0x000000, 0xFEFEFE, 128), 0x7F7F7F);
    }

    #[test]
    fn add_saturates_per_channel() {
        assert_eq!(add(0x102030, 0x010203), 0x112233);
        assert_eq!(add(0xF0F0F0, 0x202020), 0xFFFFFF);
        assert_eq!(add(0xFF0000, 0x00FF00), 0xFFFF00);
    }

    #[test]
    fn hsv_primaries_grey_and_wrap() {
        assert_eq!(hsv(0, 255, 255), 0xFF0000);
        assert_eq!(hsv(512, 255, 255), 0x00FF00);
        assert_eq!(hsv(1024, 255, 255), 0x0000FF);
        assert_eq!(hsv(256, 255, 255), 0xFFFF00);
        assert_eq!(hsv(700, 0, 200), 0xC8C8C8); // no saturation: grey
        assert_eq!(hsv(-1536, 255, 255), hsv(0, 255, 255));
        assert_eq!(hsv(1536 + 512, 255, 255), hsv(512, 255, 255));
    }
}
