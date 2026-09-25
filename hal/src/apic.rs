//! Local APIC + I/O APIC: register layout and the arithmetic around it.
//!
//! Stage 1 of `docs/smp/smp-plan.md` replaces the 8259 PIC + PIT with the
//! LAPIC timer and the I/O APIC, still on one CPU. Everything here is pure:
//! register offsets, the encodings of the LVT timer / divide configuration /
//! I/O APIC redirection entries, the timer's calibration arithmetic, and the
//! ISA-IRQ → GSI routing with the MADT's interrupt source overrides applied.
//! The kernel side (`kernel/src/interrupts/apic.rs`) owns the MMIO window,
//! `rdmsr`/`wrmsr`, and the calibration wait.
//!
//! The routing half is where real machines differ from QEMU (QEMU's IRQ0
//! goes to GSI 2, a Ryzen's IRQ9 is level-triggered active-low), and a wrong
//! polarity or trigger mode does not fault — the line simply never fires, or
//! fires forever. That is why it is here, tested, rather than inline.

use crate::acpi::Iso;

// ── Local APIC ──────────────────────────────────────────────────────────────

/// `IA32_APIC_BASE`.
pub const IA32_APIC_BASE_MSR: u32 = 0x1B;

/// Local APIC register offsets (xAPIC MMIO layout; see [`x2apic_msr`] for
/// the x2APIC equivalent).
pub mod lapic {
    pub const ID: u32 = 0x020;
    pub const VERSION: u32 = 0x030;
    pub const TPR: u32 = 0x080;
    pub const EOI: u32 = 0x0B0;
    pub const SVR: u32 = 0x0F0;
    /// In-Service Register: eight 32-bit registers, 0x10 apart.
    pub const ISR_BASE: u32 = 0x100;
    pub const ESR: u32 = 0x280;
    /// Interrupt Command Register: the low dword sends when written; the
    /// high dword holds the xAPIC destination. One 64-bit MSR in x2APIC
    /// (`x2apic_msr(ICR_LOW)`), where the destination is the high half.
    pub const ICR_LOW: u32 = 0x300;
    pub const ICR_HIGH: u32 = 0x310;
    pub const LVT_TIMER: u32 = 0x320;
    pub const LVT_LINT0: u32 = 0x350;
    pub const LVT_LINT1: u32 = 0x360;
    pub const LVT_ERROR: u32 = 0x370;
    pub const TIMER_INITIAL: u32 = 0x380;
    pub const TIMER_CURRENT: u32 = 0x390;
    pub const TIMER_DIVIDE: u32 = 0x3E0;

    /// SVR bit 8: APIC software enable.
    pub const SVR_ENABLE: u32 = 1 << 8;
    /// LVT bit 16: masked.
    pub const LVT_MASKED: u32 = 1 << 16;
}

/// The x2APIC MSR that mirrors xAPIC register `offset` (SDM Vol. 3
/// §11.12.1.2: `0x800 + offset / 16`).
pub const fn x2apic_msr(offset: u32) -> u32 {
    0x800 + (offset >> 4)
}

/// Decoded `IA32_APIC_BASE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApicBase {
    /// Physical address of the xAPIC MMIO window (bits 12..51).
    pub phys: u64,
    /// Bit 8: this is the bootstrap processor.
    pub bsp: bool,
    /// Bit 10: x2APIC mode is on. Once the firmware has turned it on it
    /// cannot be turned back to xAPIC without disabling the APIC entirely,
    /// so the kernel has to speak whichever mode it finds.
    pub x2apic: bool,
    /// Bit 11: the APIC is globally enabled.
    pub enabled: bool,
}

impl ApicBase {
    pub const fn decode(msr: u64) -> Self {
        ApicBase {
            phys: msr & 0x000F_FFFF_FFFF_F000,
            bsp: msr & (1 << 8) != 0,
            x2apic: msr & (1 << 10) != 0,
            enabled: msr & (1 << 11) != 0,
        }
    }
}

/// The LAPIC ID from the ID register's raw value: bits 31:24 in xAPIC
/// mode, the whole register in x2APIC mode.
pub const fn lapic_id(raw: u32, x2apic: bool) -> u32 {
    if x2apic { raw } else { raw >> 24 }
}

/// Where vector `v`'s bit lives in the In-Service Register: (register
/// offset, bit within it).
pub const fn isr_bit(vector: u8) -> (u32, u32) {
    (lapic::ISR_BASE + 0x10 * (vector as u32 / 32), vector as u32 % 32)
}

/// Divide Configuration Register encoding for a divisor (SDM Figure 11-10:
/// bits 0,1,3; bit 2 is reserved). `None` for anything but a power of two
/// in 1..=128.
pub const fn divide_config(divisor: u32) -> Option<u32> {
    Some(match divisor {
        1 => 0b1011,
        2 => 0b0000,
        4 => 0b0001,
        8 => 0b0010,
        16 => 0b0011,
        32 => 0b1000,
        64 => 0b1001,
        128 => 0b1010,
        _ => return None,
    })
}

/// LVT timer mode (bits 17-18). TSC-deadline is left out on purpose: it
/// needs its own calibration story and nothing here wants it yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerMode {
    OneShot = 0,
    Periodic = 1,
}

/// An LVT timer entry.
pub const fn lvt_timer(vector: u8, mode: TimerMode, masked: bool) -> u32 {
    vector as u32 | (mode as u32) << 17 | if masked { lapic::LVT_MASKED } else { 0 }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalibrationError {
    /// The counter did not move during the measurement: the timer is not
    /// running (or the measurement window was zero).
    NoTicks,
    /// The TSC is not calibrated, or the window measured zero TSC cycles.
    NoReference,
    /// The period does not fit the 32-bit initial-count register at this
    /// divisor, or rounds to zero.
    OutOfRange,
}

/// Initial count for a periodic timer at `target_hz`, given that the timer
/// counted `lapic_ticks` while the TSC advanced `tsc_cycles` at `tsc_hz`.
///
/// Measured against the TSC rather than the PIT because the TSC is already
/// calibrated against the PIT once at boot (`cpu::tsc`); chaining through
/// it means the PIT is needed exactly once, and later CPUs (which have no
/// PIT of their own) can calibrate the same way.
pub fn periodic_initial_count(
    lapic_ticks: u64,
    tsc_cycles: u64,
    tsc_hz: u64,
    target_hz: u32,
) -> Result<u32, CalibrationError> {
    if lapic_ticks == 0 {
        return Err(CalibrationError::NoTicks);
    }
    if tsc_cycles == 0 || tsc_hz == 0 {
        return Err(CalibrationError::NoReference);
    }
    if target_hz == 0 {
        return Err(CalibrationError::OutOfRange);
    }
    // ticks/s = lapic_ticks * tsc_hz / tsc_cycles; u128 because a 5 GHz TSC
    // times a large tick count overflows u64.
    let ticks_per_sec = lapic_ticks as u128 * tsc_hz as u128 / tsc_cycles as u128;
    let count = (ticks_per_sec + target_hz as u128 / 2) / target_hz as u128;
    if count == 0 || count > u32::MAX as u128 {
        return Err(CalibrationError::OutOfRange);
    }
    Ok(count as u32)
}

/// The timer's input frequency implied by a measurement, for the boot log
/// (`lapic_ticks * divisor` per `tsc_cycles`). 0 when unmeasurable.
pub fn timer_input_hz(lapic_ticks: u64, divisor: u32, tsc_cycles: u64, tsc_hz: u64) -> u64 {
    if tsc_cycles == 0 {
        return 0;
    }
    (lapic_ticks as u128 * divisor as u128 * tsc_hz as u128 / tsc_cycles as u128) as u64
}

// ── I/O APIC ────────────────────────────────────────────────────────────────

pub mod ioapic {
    /// Register select (write the register index here)…
    pub const IOREGSEL: u64 = 0x00;
    /// …then read/write its value here.
    pub const IOWIN: u64 = 0x10;

    pub const REG_ID: u32 = 0x00;
    pub const REG_VERSION: u32 = 0x01;

    /// Index of the low dword of redirection entry `n`; the high dword is
    /// the next index.
    pub const fn redirection(n: u32) -> u32 {
        0x10 + 2 * n
    }

    /// Number of redirection entries, from the version register (bits
    /// 16..23 hold the index of the *last* entry).
    pub const fn entry_count(version: u32) -> u32 {
        ((version >> 16) & 0xFF) + 1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polarity {
    ActiveHigh,
    ActiveLow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Edge,
    Level,
}

/// One I/O APIC redirection entry: fixed delivery, physical destination.
/// Those are the only modes this kernel uses; everything else is zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Redirection {
    pub vector: u8,
    pub polarity: Polarity,
    pub trigger: Trigger,
    pub masked: bool,
    /// Destination LAPIC ID (physical mode: 8 bits).
    pub dest: u8,
}

impl Redirection {
    /// The 64-bit entry: low dword first (bits 0..31), high dword after.
    pub const fn encode(&self) -> u64 {
        let mut low = self.vector as u64; // delivery mode 000 = fixed, dest mode 0 = physical
        if matches!(self.polarity, Polarity::ActiveLow) {
            low |= 1 << 13;
        }
        if matches!(self.trigger, Trigger::Level) {
            low |= 1 << 15;
        }
        if self.masked {
            low |= 1 << 16;
        }
        low | (self.dest as u64) << 56
    }
}

/// A masked entry — what every pin is set to before the ones in use are
/// routed, so nothing the firmware left programmed fires into this kernel.
pub const MASKED_ENTRY: u64 = 1 << 16;

/// Where an ISA IRQ arrives on the I/O APICs, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IsaRoute {
    pub gsi: u32,
    pub polarity: Polarity,
    pub trigger: Trigger,
}

/// Routes ISA `irq` through the MADT's interrupt source overrides.
///
/// Without an override, ISA IRQ n is GSI n, edge-triggered, active high.
/// An override remaps the GSI and carries MPS INTI flags: bits 0-1 are
/// polarity (00 = bus default, 01 = high, 11 = low), bits 2-3 trigger
/// (00 = bus default, 01 = edge, 11 = level). "Bus default" for ISA is
/// edge/high. The reserved value 10 is treated as the default too: a
/// firmware bug should not turn into a line that never fires.
pub fn isa_route(irq: u8, overrides: &[Iso]) -> IsaRoute {
    let Some(iso) = overrides.iter().find(|o| o.bus == 0 && o.source == irq) else {
        return IsaRoute { gsi: irq as u32, polarity: Polarity::ActiveHigh, trigger: Trigger::Edge };
    };
    IsaRoute {
        gsi: iso.gsi,
        polarity: if iso.flags & 0b11 == 0b11 { Polarity::ActiveLow } else { Polarity::ActiveHigh },
        trigger: if (iso.flags >> 2) & 0b11 == 0b11 { Trigger::Level } else { Trigger::Edge },
    }
}

/// Which I/O APIC serves `gsi`, given each one's `(gsi_base, entry_count)`;
/// returns (index into the slice, pin on that I/O APIC).
pub fn ioapic_for_gsi(ioapics: &[(u32, u32)], gsi: u32) -> Option<(usize, u32)> {
    ioapics
        .iter()
        .position(|&(base, count)| gsi >= base && gsi - base < count)
        .map(|i| (i, gsi - ioapics[i].0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iso(source: u8, gsi: u32, flags: u16) -> Iso {
        Iso { bus: 0, source, gsi, flags }
    }

    #[test]
    fn apic_base_decodes_qemu_bsp_value() {
        // QEMU's BSP after firmware: 0xFEE00000 | enable | BSP.
        let b = ApicBase::decode(0xFEE0_0900);
        assert_eq!(b, ApicBase { phys: 0xFEE0_0000, bsp: true, x2apic: false, enabled: true });
        let b = ApicBase::decode(0xFEE0_0D00);
        assert!(b.x2apic && b.enabled && b.bsp);
    }

    #[test]
    fn x2apic_msrs_match_the_sdm_table() {
        assert_eq!(x2apic_msr(lapic::ID), 0x802);
        assert_eq!(x2apic_msr(lapic::EOI), 0x80B);
        assert_eq!(x2apic_msr(lapic::SVR), 0x80F);
        assert_eq!(x2apic_msr(lapic::ISR_BASE), 0x810);
        assert_eq!(x2apic_msr(lapic::LVT_TIMER), 0x832);
        assert_eq!(x2apic_msr(lapic::TIMER_DIVIDE), 0x83E);
    }

    #[test]
    fn lapic_id_by_mode() {
        assert_eq!(lapic_id(0x0300_0000, false), 3);
        assert_eq!(lapic_id(0x0000_0103, true), 0x103);
    }

    #[test]
    fn isr_bit_location() {
        assert_eq!(isr_bit(32), (0x110, 0));
        assert_eq!(isr_bit(33), (0x110, 1));
        assert_eq!(isr_bit(0xFF), (0x170, 31));
        assert_eq!(isr_bit(7), (0x100, 7));
    }

    #[test]
    fn divide_config_encodings() {
        assert_eq!(divide_config(1), Some(0b1011));
        assert_eq!(divide_config(16), Some(0b0011));
        assert_eq!(divide_config(128), Some(0b1010));
        assert_eq!(divide_config(3), None);
        assert_eq!(divide_config(0), None);
        assert_eq!(divide_config(256), None);
    }

    #[test]
    fn lvt_timer_encoding() {
        assert_eq!(lvt_timer(32, TimerMode::Periodic, false), 0x0002_0020);
        assert_eq!(lvt_timer(32, TimerMode::OneShot, true), 0x0001_0020);
    }

    #[test]
    fn calibration_qemu_like() {
        // QEMU: 1 GHz APIC bus / 16 = 62.5 MHz. 10 ms of a 3 GHz TSC.
        let c = periodic_initial_count(625_000, 30_000_000, 3_000_000_000, 100).unwrap();
        assert_eq!(c, 625_000);
        assert_eq!(timer_input_hz(625_000, 16, 30_000_000, 3_000_000_000), 1_000_000_000);
    }

    #[test]
    fn calibration_ryzen_like() {
        // 100 MHz reference / 16, 3.7 GHz TSC, a window that isn't exactly
        // 10 ms: the result depends on rates, not on the window length.
        let c = periodic_initial_count(78_125, 46_250_000, 3_700_000_000, 100).unwrap();
        assert_eq!(c, 62_500);
    }

    #[test]
    fn calibration_rejects_nonsense() {
        assert_eq!(periodic_initial_count(0, 1, 1, 100), Err(CalibrationError::NoTicks));
        assert_eq!(periodic_initial_count(1, 0, 1, 100), Err(CalibrationError::NoReference));
        assert_eq!(periodic_initial_count(1, 1, 0, 100), Err(CalibrationError::NoReference));
        assert_eq!(periodic_initial_count(1, 1, 1, 0), Err(CalibrationError::OutOfRange));
        // Rounds to zero: a timer far too slow for the rate.
        assert_eq!(periodic_initial_count(1, 1_000_000, 1_000_000, 100), Err(CalibrationError::OutOfRange));
        // Doesn't fit 32 bits.
        assert_eq!(
            periodic_initial_count(u32::MAX as u64, 1, 1_000, 1),
            Err(CalibrationError::OutOfRange)
        );
    }

    #[test]
    fn redirection_encoding() {
        let r = Redirection {
            vector: 33, polarity: Polarity::ActiveHigh, trigger: Trigger::Edge, masked: false, dest: 0,
        };
        assert_eq!(r.encode(), 33);
        let r = Redirection {
            vector: 0x29, polarity: Polarity::ActiveLow, trigger: Trigger::Level, masked: true, dest: 5,
        };
        assert_eq!(r.encode(), 0x0500_0000_0000_0000 | 1 << 16 | 1 << 15 | 1 << 13 | 0x29);
        assert_eq!(MASKED_ENTRY, 0x1_0000);
    }

    #[test]
    fn ioapic_registers() {
        assert_eq!(ioapic::redirection(0), 0x10);
        assert_eq!(ioapic::redirection(23), 0x3E);
        // QEMU / most chipsets: version 0x11, 24 entries.
        assert_eq!(ioapic::entry_count(0x0017_0011), 24);
    }

    /// The overrides QEMU i440fx's MADT carries (the ACPI selftest checks
    /// the first one).
    fn qemu_overrides() -> [Iso; 5] {
        [iso(0, 2, 0), iso(5, 5, 0x0D), iso(9, 9, 0x0D), iso(10, 10, 0x0D), iso(11, 11, 0x0D)]
    }

    #[test]
    fn isa_route_without_override_is_identity_edge_high() {
        let r = isa_route(1, &qemu_overrides());
        assert_eq!(r, IsaRoute { gsi: 1, polarity: Polarity::ActiveHigh, trigger: Trigger::Edge });
        let r = isa_route(12, &[]);
        assert_eq!(r.gsi, 12);
    }

    #[test]
    fn isa_route_applies_gsi_remap() {
        let r = isa_route(0, &qemu_overrides());
        assert_eq!(r, IsaRoute { gsi: 2, polarity: Polarity::ActiveHigh, trigger: Trigger::Edge });
    }

    #[test]
    fn isa_route_applies_flags() {
        // QEMU's 0x0D = trigger level (11), polarity high (01).
        let r = isa_route(9, &qemu_overrides());
        assert_eq!(r, IsaRoute { gsi: 9, polarity: Polarity::ActiveHigh, trigger: Trigger::Level });
        // A typical AMD board's SCI: IRQ9 level, active low (0x0F).
        let r = isa_route(9, &[iso(9, 9, 0x0F)]);
        assert_eq!(r.polarity, Polarity::ActiveLow);
        assert_eq!(r.trigger, Trigger::Level);
    }

    #[test]
    fn isa_route_ignores_other_buses_and_reserved_flags() {
        let other_bus = Iso { bus: 1, source: 1, gsi: 20, flags: 0x0F };
        assert_eq!(isa_route(1, &[other_bus]).gsi, 1);
        // Reserved 10 in both fields → bus default.
        let r = isa_route(4, &[iso(4, 4, 0b1010)]);
        assert_eq!((r.polarity, r.trigger), (Polarity::ActiveHigh, Trigger::Edge));
    }

    #[test]
    fn gsi_to_ioapic() {
        // Two I/O APICs, like an AMD board with the FCH's and the root
        // complex's: 24 pins at 0, 32 at 24.
        let ios = [(0, 24), (24, 32)];
        assert_eq!(ioapic_for_gsi(&ios, 0), Some((0, 0)));
        assert_eq!(ioapic_for_gsi(&ios, 23), Some((0, 23)));
        assert_eq!(ioapic_for_gsi(&ios, 24), Some((1, 0)));
        assert_eq!(ioapic_for_gsi(&ios, 55), Some((1, 31)));
        assert_eq!(ioapic_for_gsi(&ios, 56), None);
        assert_eq!(ioapic_for_gsi(&[], 0), None);
    }
}
