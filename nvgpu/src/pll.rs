//! Pixel clocks (phase 5.5 of the plan): the heads' video PLLs, as nouveau
//! programs them in supervisor 2.1 (`nv50_disp_super_2_1`,
//! `engine/disp/nv50.c:1307-1315` → `ga100_devinit_pll_set`,
//! `subdev/devinit/ga100.c:29-64`):
//!
//! - the limits of `VPLL<head>` from the VBIOS PLL table (`nvbios_pll_parse`,
//!   `subdev/bios/pll.c`; this board's table is version 0x50);
//! - coefficients by `gt215_pll_calc` (`subdev/clk/pllgt215.c:30-86`), in
//!   its fractional form (`fN`), the only one GA100's devinit uses, **except
//!   for how `fN` is encoded** (below);
//! - four register writes.
//!
//! **Deviation from nouveau, measured on the Ryzen (boots #78/#79).**
//! `gt215_pll_calc` rounds N down and stores `fN` as a fraction in 1/8192
//! steps minus 4096, i.e. around +0.5. GA106's VPLL reads
//! `refclk * (N + fN / 8192) / (M * P)` with no offset: the GOP leaves
//! `N 55 fN 0` for 148.5 MHz (`0xef18 = 0x370000`), and nouveau's
//! `N 54 fN 0x1000` gave 147.15 MHz (vblank 59.447 Hz against the GOP's
//! 59.988 on the same counter; `N 59 fN 0x2ab P 13` gave 49.572 Hz, not 50).
//! So here N = the whole part and `fN` = the fraction × 8192, rounded.

use crate::vbios::Bios;
use crate::Mmio;

/// `enum nvbios_pll_type` (`include/nvkm/subdev/bios/pll.h:28-43`):
/// `PLL_VPLL0 + head`.
pub const PLL_VPLL0: u8 = 0x80;
pub const VPLLS: u32 = 4;

/// `struct nvbios_pll` (`pll.h:44-73`), the fields a version 0x50 table
/// fills (`pll.c:367-380`) and `gt215_pll_calc` reads. Frequencies in kHz.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Limits {
    pub kind: u8,
    pub refclk: u32,
    pub min_p: u8,
    pub max_p: u8,
    pub vco_min: u32,
    pub vco_max: u32,
    pub in_min: u32,
    pub in_max: u32,
    pub min_m: u8,
    pub max_m: u8,
    pub min_n: u8,
    pub max_n: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PllError {
    /// No BIT 'C' PLL table, or not version 0x50 (the only one parsed:
    /// this board's; the older layouts are for pre-Fermi boards).
    NoTable(u8),
    /// The table has no entry of this type (`pll.c:245-246`: -ENOENT).
    NoEntry,
    /// `refclk` is 0: nouveau would take the crystal (`pll.c:386-387`),
    /// which this driver does not read.
    NoRefclk,
    /// `gt215_pll_calc` found nothing in range (`pllgt215.c:81-84`), or
    /// the clock is 0.
    NoCoefficients,
}

/// `pll_limits_table` (`pll.c:80-111`): BIT 'C' version 1 (u16 at +8) or 2
/// (u32 at +0) points at the table; header bytes: version, header length,
/// entry length, count.
fn limits_table(bios: &Bios) -> Option<(u32, u8, u8, u8, u8)> {
    let c = bios.bit_entry(b'C')?;
    let data = match c.version {
        1 if c.length >= 10 => bios.rd16(c.offset as u32 + 8) as u32,
        2 if c.length >= 4 => bios.rd32(c.offset as u32),
        _ => 0,
    };
    if data == 0 {
        return None;
    }
    Some((data, bios.rd08(data), bios.rd08(data + 1), bios.rd08(data + 2), bios.rd08(data + 3)))
}

/// `nvbios_pll_parse` by type (`pll.c:229-383`, through `pll_map_type`,
/// `pll.c:184-226`): the first entry whose byte 0 is `kind`.
pub fn parse(bios: &Bios, kind: u8) -> Result<Limits, PllError> {
    let Some((table, ver, hdr, len, cnt)) = limits_table(bios) else { return Err(PllError::NoTable(0)) };
    if ver != 0x50 {
        return Err(PllError::NoTable(ver));
    }
    let data = (0..cnt as u32)
        .map(|i| table + hdr as u32 + i * len as u32)
        .find(|&e| bios.rd08(e) == kind)
        .ok_or(PllError::NoEntry)?;
    let l = Limits {
        kind,
        refclk: bios.rd16(data + 1) as u32 * 1000,
        vco_min: bios.rd16(data + 5) as u32 * 1000,
        vco_max: bios.rd16(data + 7) as u32 * 1000,
        in_min: bios.rd16(data + 9) as u32 * 1000,
        in_max: bios.rd16(data + 11) as u32 * 1000,
        min_m: bios.rd08(data + 13),
        max_m: bios.rd08(data + 14),
        min_n: bios.rd08(data + 15),
        max_n: bios.rd08(data + 16),
        min_p: bios.rd08(data + 17),
        max_p: bios.rd08(data + 18),
    };
    if l.refclk == 0 {
        return Err(PllError::NoRefclk);
    }
    Ok(l)
}

/// PLL coefficients: `refclk * (N + fN / 8192) / M / P` (measured; see
/// the module comment).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Coeffs {
    pub n: u32,
    /// 16-bit field as written: 0..8192.
    pub fn_: u32,
    pub m: u32,
    pub p: u32,
}

impl Coeffs {
    /// The clock these coefficients give, in Hz, for the log. `fN` is
    /// rounded to 1/8192 of `refclk`, so this can be a few tens of Hz off
    /// the clock asked for.
    pub fn hz(&self, refclk_khz: u32) -> u64 {
        let num = refclk_khz as i64 * 1000 * (self.n as i64 * 8192 + self.fn_ as i64);
        (num / (8192 * self.m as i64 * self.p as i64)) as u64
    }
}

/// `gt215_pll_calc` with `pfN` (`pllgt215.c:30-86`) for P and M; N and
/// `fN` as this GPU reads them (module comment). The first M in range
/// wins: with `pfN` the loop returns on its first iteration that passes
/// the N bounds.
pub fn calc(l: &Limits, khz: u32) -> Result<Coeffs, PllError> {
    if khz == 0 || l.in_max == 0 || l.in_min == 0 {
        return Err(PllError::NoCoefficients);
    }
    // In nouveau's order (not `clamp`, which panics on min > max).
    let p = (l.vco_max / khz).min(l.max_p as u32).max(l.min_p as u32).max(1);
    let lm = ((l.refclk + l.in_max) / l.in_max).max(l.min_m as u32);
    let hm = ((l.refclk + l.in_min) / l.in_min).min(l.max_m as u32);
    let lm = lm.min(hm);
    for m in lm..=hm {
        let tmp = khz as u64 * p as u64 * m as u64;
        let mut n = (tmp / l.refclk as u64) as i64;
        let f = (tmp % l.refclk as u64) as i64;
        let mut fn_ = ((f << 13) + (l.refclk / 2) as i64) / l.refclk as i64;
        if fn_ == 8192 {
            n += 1;
            fn_ = 0;
        }
        if n < l.min_n as i64 {
            continue;
        }
        if n > l.max_n as i64 {
            break;
        }
        return Ok(Coeffs { n: n as u32, fn_: fn_ as u32, m, p });
    }
    Err(PllError::NoCoefficients)
}

/// The writes `ga100_devinit_pll_set` makes for `VPLL<head>`
/// (`ga100.c:52-55`), in order.
pub fn vpll_writes(head: u32, c: &Coeffs) -> [(u32, u32); 4] {
    [
        (0x00_ef00 + head * 0x40, 0x0208_0004),
        (0x00_ef18 + head * 0x40, (c.n << 16) | c.fn_),
        (0x00_ef04 + head * 0x40, (c.p << 16) | c.m),
        (0x00_e9c0 + head * 0x04, 0x0000_0001),
    ]
}

/// Supervisor 2.1 for one head: limits, coefficients, writes.
pub fn set_vpll(m: &dyn Mmio, bios: &Bios, head: u32, khz: u32) -> Result<Coeffs, PllError> {
    if head >= VPLLS {
        return Err(PllError::NoEntry);
    }
    let l = parse(bios, PLL_VPLL0 + head as u8)?;
    let c = calc(&l, khz)?;
    for (o, v) in vpll_writes(head, &c) {
        m.wr32(o, v);
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmio::testing::TableMmio;
    use crate::vbios::tests::{oracle_vbios, tiny_image};

    fn bios() -> Option<Bios> {
        oracle_vbios().map(|rom| Bios::new(rom).unwrap())
    }

    /// VPLL0-3 of this board (VBIOS 94.06.37.00.40, PLL table at 0x4f51,
    /// version 0x50, entries 7-10).
    fn vpll() -> Limits {
        Limits {
            kind: PLL_VPLL0,
            refclk: 27_000,
            min_p: 1,
            max_p: 63,
            vco_min: 800_000,
            vco_max: 1_620_000,
            in_min: 19_000,
            in_max: 38_000,
            min_m: 1,
            max_m: 1,
            min_n: 29,
            max_n: 255,
        }
    }

    #[test]
    fn vpll_limits_from_the_oracle_vbios() {
        let Some(bios) = bios() else { return };
        for head in 0..VPLLS as u8 {
            assert_eq!(parse(&bios, PLL_VPLL0 + head), Ok(Limits { kind: PLL_VPLL0 + head, ..vpll() }));
        }
        assert_eq!(parse(&bios, 0x99), Err(PllError::NoEntry));
    }

    #[test]
    fn no_table_is_an_error() {
        let bios = Bios::new(tiny_image()).unwrap();
        assert_eq!(parse(&bios, PLL_VPLL0), Err(PllError::NoTable(0)));
    }

    #[test]
    fn calc_gives_the_gops_148_5_mhz() {
        // What the GOP left in VPLL0 (Ryzen #79): 0xef18 = 0x00370000,
        // 0xef04 = 0x000a0001 (N 55, fN 0, M 1, P 10). nouveau's trace
        // wrote N 54 fN 0x1000 for VPLL1, which runs at 147.15 MHz.
        let c = calc(&vpll(), 148_500).unwrap();
        assert_eq!(c, Coeffs { n: 55, fn_: 0, m: 1, p: 10 });
        assert_eq!(c.hz(27_000), 148_500_000);
        assert_eq!(Coeffs { n: 54, fn_: 0x1000, m: 1, p: 10 }.hz(27_000), 147_150_000);
    }

    #[test]
    fn calc_for_other_clocks() {
        // 1080p at 50 Hz on the same raster (2200x1125): 123.75 MHz, P 13.
        // 1.60875 GHz = 59.583 x 27 MHz: fN = 0.583 x 8192 = 4779.
        let c = calc(&vpll(), 123_750).unwrap();
        assert_eq!(c, Coeffs { n: 59, fn_: 4779, m: 1, p: 13 });
        // fN steps are refclk / 8192 / P: 84 Hz off here.
        assert_eq!(c.hz(27_000), 123_750_084);
        // 1080p at 180 Hz (CVT-RB2 is ~ 390 MHz): P 4.
        let c = calc(&vpll(), 390_000).unwrap();
        assert_eq!((c.m, c.p), (1, 4));
        assert!(c.hz(27_000).abs_diff(390_000_000) < 2_000);
        // A small remainder stays with N (nouveau moved N down one): 100
        // MHz, P 16, 1.6 GHz = 59 x 27 MHz + 7 MHz, fN = 7/27 x 8192.
        let c = calc(&vpll(), 100_000).unwrap();
        assert_eq!(c, Coeffs { n: 59, fn_: 2124, m: 1, p: 16 });
        assert_eq!(c.hz(27_000), 100_000_030);
        // A fraction that rounds up to 8192 carries into N: 139.909 MHz x
        // P 11 = 56 x 27 MHz + 26.999 MHz.
        let c = calc(&vpll(), 139_909).unwrap();
        assert_eq!((c.n, c.fn_, c.p), (57, 0, 11));
        // A clock the VCO cannot divide down to: N below its minimum.
        assert_eq!(calc(&Limits { min_n: 200, ..vpll() }, 148_500), Err(PllError::NoCoefficients));
        assert_eq!(calc(&vpll(), 0), Err(PllError::NoCoefficients));
    }

    #[test]
    fn set_vpll_writes_like_the_trace_with_the_gops_n() {
        let Some(bios) = bios() else { return };
        let m = TableMmio::new(&[]);
        set_vpll(&m, &bios, 1, 148_500).unwrap();
        // nvgpu/fixtures/super-round2.txt:562-565, except 0xef58: the
        // trace has nouveau's 0x00361000, the GOP's N/fN is 0x00370000.
        assert_eq!(*m.writes.borrow(), [(0xef40, 0x0208_0004), (0xef58, 0x0037_0000), (0xef44, 0x000a_0001), (0xe9c4, 1)]);
        assert!(m.reads.borrow().is_empty());
    }
}
