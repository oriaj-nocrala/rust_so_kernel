//! Identity look-up tables for a window's input LUT and a head's output LUT
//! (phase 5.8), in the memory layout nouveau writes on GA10x
//! (`dispnv50/lut.c:32-46`, `wndwc57e.c:144-194`, `headc57d.c:165-199`):
//! a 0x20-byte zero header, 1024 entries of three 16-bit values (R, G, B)
//! and 2 pad bytes in an 8-byte stride, then the last entry repeated (the
//! INTERPOLATE modes read one more). The identity ramp is `i << 6` (`(i << 16)
//! >> 10`). The input LUT stores each value as a half-float
//! (`fixedU0_16_FP16`), the output LUT as is.
//!
//! Neither is needed to show a picture through window 0 (the GOP's state
//! has both off); nouveau always programs them, and its first UPDATE of a
//! new window is what the hardware was seen to accept (Ryzen boot #89
//! raised INVALID_STATE on a window UPDATE without them).

use alloc::vec::Vec;

/// Entries in the table.
pub const ENTRIES: usize = 1024;
/// Header, entries and the replicated last entry.
pub const BYTES: usize = 0x20 + (ENTRIES + 1) * 8;
/// `SET_ILUT_CONTROL` / `HEAD_SET_OLUT_CONTROL` SIZE field: header (4) +
/// entries + 1 (`wndwc57e.c:191`, `headc57d.c:192`).
pub const SIZE_FIELD: u32 = 4 + ENTRIES as u32 + 1;

/// `SET_ILUT_CONTROL` as nouveau writes it: SIZE, MODE = DIRECT10 (2),
/// INTERPOLATE off (`wndwc57e.c:134-137,187-192`).
pub const ILUT_CONTROL: u32 = SIZE_FIELD << 8 | 2 << 2;
/// `HEAD_SET_OLUT_CONTROL`: the same with INTERPOLATE on
/// (`headc57d.c:120-124,191-193`).
pub const OLUT_CONTROL: u32 = ILUT_CONTROL | 1;

/// `fixedU0_16_FP16` (`wndwc57e.c:144-155`): a U0.16 fixed point value as
/// a half float, no sign.
pub fn fixed_u0_16_to_fp16(fixed: u16) -> u16 {
    if fixed == 0 {
        return 0;
    }
    let mut fixed = fixed;
    let mut exp: i32 = 0;
    loop {
        exp -= 1;
        if exp == 0 || fixed & 0x8000 != 0 {
            break;
        }
        fixed <<= 1;
    }
    let man = (((fixed as u32) << 1) & 0xffc0) >> 6;
    let exp = exp + 15;
    ((exp as u32) << 10 | man) as u16
}

fn table(conv: impl Fn(u16) -> u16) -> Vec<u8> {
    let mut v = alloc::vec![0u8; BYTES];
    let mut entry = |i: usize, value: u16| {
        let at = 0x20 + i * 8;
        for c in 0..3 {
            v[at + c * 2..at + c * 2 + 2].copy_from_slice(&conv(value).to_le_bytes());
        }
    };
    for i in 0..ENTRIES {
        entry(i, ((i as u32) << 16 >> 10) as u16);
    }
    // The replicated last entry.
    let last = 0x20 + (ENTRIES - 1) * 8;
    let copy: [u8; 6] = v[last..last + 6].try_into().unwrap();
    v[last + 8..last + 14].copy_from_slice(&copy);
    v
}

/// The window's input LUT: identity, half floats.
pub fn ilut_identity() -> Vec<u8> {
    table(fixed_u0_16_to_fp16)
}

/// The head's output LUT: identity, 16-bit values.
pub fn olut_identity() -> Vec<u8> {
    table(|v| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_words_are_nouveaus() {
        // The trace: window 2 `SET_ILUT_CONTROL` and head 1 `HEAD_SET_OLUT_CONTROL`.
        assert_eq!(ILUT_CONTROL, 0x0004_0508);
        assert_eq!(OLUT_CONTROL, 0x0004_0509);
    }

    #[test]
    fn fp16_of_the_ramp() {
        assert_eq!(fixed_u0_16_to_fp16(0), 0);
        // 0.5 = 0x8000 -> 2^-1 = exponent 14, mantissa 0.
        assert_eq!(fixed_u0_16_to_fp16(0x8000), 0x3800);
        // 0xffc0 (top of the ramp) -> just under 1.0.
        assert_eq!(fixed_u0_16_to_fp16(0xffc0), 0x3800 | 0x3fe); // (0xff80 >> 6) = 1022: the C truncation
        // 64 = 2^-10 -> exponent 5, mantissa 0.
        assert_eq!(fixed_u0_16_to_fp16(64), 5 << 10);
        // Monotonic over the ramp.
        let mut last = 0;
        for i in 1..ENTRIES as u32 {
            let f = fixed_u0_16_to_fp16((i << 6) as u16);
            assert!(f > last, "{i}");
            last = f;
        }
    }

    #[test]
    fn table_layout() {
        let o = olut_identity();
        assert_eq!(o.len(), BYTES);
        assert!(o[..0x20].iter().all(|b| *b == 0));
        let at = |i: usize, c: usize| u16::from_le_bytes([o[0x20 + i * 8 + c * 2], o[0x20 + i * 8 + c * 2 + 1]]);
        assert_eq!((at(0, 0), at(1, 0), at(512, 1), at(1023, 2)), (0, 64, 0x8000, 1023 * 64));
        // The pad bytes stay 0, and the last entry is repeated.
        assert_eq!(&o[0x20 + 6..0x20 + 8], &[0, 0]);
        assert_eq!(at(1024, 0), at(1023, 0));
        assert_eq!(at(1024, 2), at(1023, 2));
        let i = ilut_identity();
        let ia = |n: usize| u16::from_le_bytes([i[0x20 + n * 8], i[0x20 + n * 8 + 1]]);
        assert_eq!((ia(0), ia(512)), (0, 0x3800));
        assert_eq!(ia(1024), ia(1023));
    }
}
