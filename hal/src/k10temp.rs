//! AMD Zen CPU temperatures, read the way Linux's `k10temp` reads them.
//!
//! Family 17h (Zen, Zen+, Zen 2) and 19h (Zen 3, Zen 4) report their
//! temperatures in registers of the System Management Network (SMN), an
//! address space internal to the SoC. The host reaches it through an
//! index/data pair in the root complex's PCI configuration space
//! (00:00.0, offsets [`SMN_INDEX`]/[`SMN_DATA`]) — Linux's `amd_smn_read`.
//! The registers are:
//!
//! - **Tctl** ([`REPORTED_TEMP_CTRL`], bits 31:21): the *control*
//!   temperature the cooling is driven by, in 1/8 °C. With bit 19 set, or
//!   bits 17:16 both set, its range is -49..206 °C instead of 0..255, and
//!   49 °C must be subtracted. On a few early parts (see
//!   [`TCTL_OFFSETS`]) Tctl is deliberately offset above the die
//!   temperature; there `Tdie` is Tctl minus that offset.
//! - **Tccd*n*** (`REPORTED_TEMP_CTRL + ccd_offset + 4n`, bits 10:0, valid
//!   bit 11): each core complex die's own temperature, in 1/8 °C from
//!   -49 °C. A CCD that is not populated never sets the valid bit, which is
//!   how Linux decides which Tccd labels exist (`k10temp_get_ccd_support`).
//!
//! Everything below mirrors `drivers/hwmon/k10temp.c` — the constants,
//! the per-model CCD table and the Tctl offset table — and is pure: the
//! kernel does the SMN reads and hands the values in, or passes a closure
//! that does them.

use core::fmt::{self, Write};

/// Root complex (bus 0, device 0, function 0) config offsets of the SMN
/// index and data registers.
pub const SMN_INDEX: u8 = 0x60;
pub const SMN_DATA: u8 = 0x64;

/// The root complex must be AMD's for the pair above to mean anything.
pub const AMD_VENDOR: u16 = 0x1022;

/// `ZEN_REPORTED_TEMP_CTRL_BASE`: Tctl, and the base the CCD registers are
/// offset from.
pub const REPORTED_TEMP_CTRL: u32 = 0x0005_9800;

const CUR_TEMP_SHIFT: u32 = 21;
const CUR_TEMP_RANGE_SEL: u32 = 1 << 19;
const CUR_TEMP_TJ_SEL: u32 = 3 << 16;
const CCD_TEMP_VALID: u32 = 1 << 11;
const CCD_TEMP_MASK: u32 = 0x7FF;

/// Most CCDs any supported model has (Genoa's twelve). Family 1Ah (Zen 5,
/// up to sixteen, CCDs at offset 0x1F0) is left out: nothing here runs on
/// one to check it against.
pub const MAX_CCDS: usize = 12;

/// Early parts whose Tctl reads above the die temperature, matched as a
/// substring of the brand string, with the offset in millidegrees
/// (`tctl_offset_table`).
pub const TCTL_OFFSETS: &[(&str, i32)] = &[
    ("AMD Ryzen 5 1600X", 20_000),
    ("AMD Ryzen 7 1700X", 20_000),
    ("AMD Ryzen 7 1800X", 20_000),
    ("AMD Ryzen 7 2700X", 10_000),
    ("AMD Ryzen Threadripper 19", 27_000),
    ("AMD Ryzen Threadripper 29", 27_000),
];

/// What a CPU model supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Model {
    /// Offset of Tccd1 from [`REPORTED_TEMP_CTRL`]; meaningless when
    /// `ccd_limit` is 0.
    pub ccd_offset: u32,
    /// How many CCD registers to probe for the valid bit.
    pub ccd_limit: usize,
    /// Tctl − Tdie in millidegrees; 0 when they are the same, in which case
    /// there is no separate Tdie (as in Linux).
    pub tctl_offset: i32,
}

/// Whether this CPU is one `k10temp`'s Zen path covers, and how.
/// `vendor` is CPUID leaf 0's string, `family`/`model` the folded values
/// (`cpuid::signature`), `brand` the trimmed brand string.
pub fn model(vendor: &[u8; 12], family: u32, model: u32, brand: &str) -> Option<Model> {
    if vendor != b"AuthenticAMD" {
        return None;
    }
    let (ccd_offset, ccd_limit) = match (family, model) {
        (0x17, 0x01 | 0x08 | 0x11 | 0x18) => (0x154, 4),
        (0x17, 0x31 | 0x47 | 0x60 | 0x68 | 0x71) => (0x154, 8),
        (0x17, 0xA0..=0xAF) => (0x300, 8),
        (0x17, _) => (0, 0),
        (0x19, 0x00..=0x01 | 0x08 | 0x21 | 0x50..=0x5F) => (0x154, 8),
        (0x19, 0x40..=0x4F) => (0x300, 8),
        (0x19, 0x60..=0x7F) => (0x308, 8),
        (0x19, 0x10..=0x1F | 0xA0..=0xAF) => (0x300, 12),
        (0x19, _) => (0, 0),
        _ => return None,
    };
    let tctl_offset = if family == 0x17 {
        TCTL_OFFSETS.iter().find(|(id, _)| brand.contains(id)).map_or(0, |&(_, o)| o)
    } else {
        0
    };
    Some(Model { ccd_offset, ccd_limit, tctl_offset })
}

/// SMN address of CCD `i`'s register (0-based).
pub fn ccd_reg(m: &Model, i: usize) -> u32 {
    REPORTED_TEMP_CTRL + m.ccd_offset + 4 * i as u32
}

/// Tctl in millidegrees from the [`REPORTED_TEMP_CTRL`] register, clamped
/// at 0 as Linux clamps it.
pub fn tctl(reg: u32) -> i32 {
    let mut t = (reg >> CUR_TEMP_SHIFT) as i32 * 125;
    if reg & CUR_TEMP_RANGE_SEL != 0 || reg & CUR_TEMP_TJ_SEL == CUR_TEMP_TJ_SEL {
        t -= 49_000;
    }
    t.max(0)
}

/// A CCD's temperature in millidegrees, or `None` if its valid bit is
/// clear.
pub fn ccd(reg: u32) -> Option<i32> {
    (reg & CCD_TEMP_VALID != 0).then(|| (reg & CCD_TEMP_MASK) as i32 * 125 - 49_000)
}

/// Which CCDs exist, as a bit mask (bit `i` = Tccd`i+1`). Done once: an
/// absent CCD never becomes present.
pub fn probe_ccds(m: &Model, mut smn_read: impl FnMut(u32) -> u32) -> u16 {
    let mut mask = 0u16;
    for i in 0..m.ccd_limit.min(MAX_CCDS) {
        if ccd(smn_read(ccd_reg(m, i))).is_some() {
            mask |= 1 << i;
        }
    }
    mask
}

/// One reading of every sensor, in millidegrees Celsius.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reading {
    pub tctl: i32,
    /// Only on parts with a Tctl offset.
    pub tdie: Option<i32>,
    /// `None` for a CCD not in the probe mask, or one that read invalid
    /// this time.
    pub ccd: [Option<i32>; MAX_CCDS],
}

/// Read every sensor the model has and the probe found.
pub fn read(m: &Model, ccds: u16, mut smn_read: impl FnMut(u32) -> u32) -> Reading {
    let tctl = tctl(smn_read(REPORTED_TEMP_CTRL));
    let mut r = Reading { tctl, tdie: (m.tctl_offset != 0).then(|| tctl - m.tctl_offset), ..Reading::default() };
    for i in 0..m.ccd_limit.min(MAX_CCDS) {
        if ccds & (1 << i) != 0 {
            r.ccd[i] = ccd(smn_read(ccd_reg(m, i)));
        }
    }
    r
}

/// The chip name every line of `/proc/sensors` starts with.
pub const CHIP: &str = "k10temp";

/// `/proc/sensors`' lines for one reading: `chip<TAB>temp<TAB>label<TAB>
/// millidegrees` — hwmon's name, attribute type, `tempN_label` and
/// `tempN_input` — one sensor per line.
pub fn render(r: &Reading, out: &mut impl Write) -> fmt::Result {
    writeln!(out, "{CHIP}\ttemp\tTctl\t{}", r.tctl)?;
    if let Some(t) = r.tdie {
        writeln!(out, "{CHIP}\ttemp\tTdie\t{t}")?;
    }
    for (i, t) in r.ccd.iter().enumerate() {
        if let Some(t) = t {
            writeln!(out, "{CHIP}\ttemp\tTccd{}\t{t}", i + 1)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    const AMD: &[u8; 12] = b"AuthenticAMD";

    /// Registers encoding what Linux's k10temp reported on the target
    /// machine (Ryzen 9 5900X, family 19h model 21h, two CCDs) on
    /// 2026-09-26: Tctl 32875, Tccd1 40250, Tccd2 30750. Derived from those
    /// values by inverting the formulas (reading SMN needs root), so this
    /// checks the arithmetic against Linux's output, not the register
    /// layout against the silicon — the metal run does that.
    fn ryzen_smn(addr: u32) -> u32 {
        match addr {
            REPORTED_TEMP_CTRL => 263 << 21,               // 263 * 125 = 32875
            0x5_9954 => CCD_TEMP_VALID | 714,              // 714 * 125 - 49000 = 40250
            0x5_9958 => CCD_TEMP_VALID | 638,              // 638 * 125 - 49000 = 30750
            _ => 0,
        }
    }

    #[test]
    fn target_machine_model() {
        let m = model(AMD, 0x19, 0x21, "AMD Ryzen 9 5900X 12-Core Processor").unwrap();
        assert_eq!(m, Model { ccd_offset: 0x154, ccd_limit: 8, tctl_offset: 0 });
        assert_eq!(ccd_reg(&m, 0), 0x5_9954);
    }

    #[test]
    fn target_machine_reading_matches_linux() {
        let m = model(AMD, 0x19, 0x21, "AMD Ryzen 9 5900X 12-Core Processor").unwrap();
        let mask = probe_ccds(&m, ryzen_smn);
        assert_eq!(mask, 0b11);
        let r = read(&m, mask, ryzen_smn);
        assert_eq!(r.tctl, 32_875);
        assert_eq!(r.tdie, None);
        assert_eq!(&r.ccd[..3], &[Some(40_250), Some(30_750), None]);
        let mut s = String::new();
        render(&r, &mut s).unwrap();
        assert_eq!(s, "k10temp\ttemp\tTctl\t32875\nk10temp\ttemp\tTccd1\t40250\nk10temp\ttemp\tTccd2\t30750\n");
    }

    #[test]
    fn not_amd_or_not_zen_is_unsupported() {
        assert_eq!(model(b"GenuineIntel", 0x19, 0x21, ""), None);
        assert_eq!(model(AMD, 0x10, 0x04, ""), None); // K10: the non-SMN path, not ported
        assert_eq!(model(AMD, 0x1A, 0x44, ""), None); // Zen 5: not covered
    }

    #[test]
    fn models_without_ccd_registers_still_have_tctl() {
        let m = model(AMD, 0x17, 0x20, "AMD Ryzen 3 3200U").unwrap();
        assert_eq!(m.ccd_limit, 0);
        assert_eq!(probe_ccds(&m, |_| u32::MAX), 0);
        let r = read(&m, 0, |_| 400 << 21);
        assert_eq!(r.tctl, 50_000);
        assert_eq!(r.ccd, [None; MAX_CCDS]);
    }

    #[test]
    fn per_model_ccd_offsets() {
        assert_eq!(model(AMD, 0x19, 0x61, "").unwrap().ccd_offset, 0x308);
        assert_eq!(model(AMD, 0x19, 0x44, "").unwrap().ccd_offset, 0x300);
        assert_eq!(model(AMD, 0x17, 0xA0, "").unwrap().ccd_offset, 0x300);
        let genoa = model(AMD, 0x19, 0x11, "").unwrap();
        assert_eq!((genoa.ccd_offset, genoa.ccd_limit), (0x300, 12));
        assert_eq!(model(AMD, 0x17, 0x71, "").unwrap().ccd_limit, 8);
        assert_eq!(model(AMD, 0x17, 0x01, "").unwrap().ccd_limit, 4);
    }

    #[test]
    fn range_select_subtracts_49() {
        assert_eq!(tctl((400 << 21) | CUR_TEMP_RANGE_SEL), 1_000);
        assert_eq!(tctl((400 << 21) | CUR_TEMP_TJ_SEL), 1_000);
        // One of the two TJ_SEL bits is not enough.
        assert_eq!(tctl((400 << 21) | (1 << 16)), 50_000);
        // Below 0 clamps.
        assert_eq!(tctl((100 << 21) | CUR_TEMP_RANGE_SEL), 0);
        // The top of the field.
        assert_eq!(tctl(0x7FF << 21), 255_875);
    }

    #[test]
    fn ccd_needs_valid_bit_and_ignores_high_bits() {
        assert_eq!(ccd(714), None);
        assert_eq!(ccd(0xFFFF_F000 | CCD_TEMP_VALID | 392), Some(0));
        assert_eq!(ccd(CCD_TEMP_VALID), Some(-49_000));
    }

    #[test]
    fn tctl_offset_gives_tdie_on_early_parts_only() {
        let m = model(AMD, 0x17, 0x01, "AMD Ryzen 7 1800X Eight-Core Processor").unwrap();
        assert_eq!(m.tctl_offset, 20_000);
        let r = read(&m, 0, |_| 560 << 21); // 70 °C
        assert_eq!((r.tctl, r.tdie), (70_000, Some(50_000)));
        let mut s = String::new();
        render(&r, &mut s).unwrap();
        assert_eq!(s, "k10temp\ttemp\tTctl\t70000\nk10temp\ttemp\tTdie\t50000\n");
        assert_eq!(model(AMD, 0x17, 0x01, "AMD Ryzen 7 1700 Eight-Core Processor").unwrap().tctl_offset, 0);
        assert_eq!(model(AMD, 0x17, 0x08, "AMD Ryzen Threadripper 2990WX").unwrap().tctl_offset, 27_000);
    }

    #[test]
    fn a_ccd_that_goes_invalid_reads_none() {
        let m = model(AMD, 0x19, 0x21, "").unwrap();
        let r = read(&m, 0b11, |a| if a == 0x5_9958 { 638 } else { ryzen_smn(a) });
        assert_eq!(&r.ccd[..2], &[Some(40_250), None]);
    }
}
