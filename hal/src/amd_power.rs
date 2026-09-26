//! AMD Zen idle states and energy counters: the pure half of `cpu::idle`.
//!
//! **C2 through an I/O read.** `hlt` puts a Zen core in C1, which stops its
//! clock but keeps it powered. The deeper state Linux uses when idle —
//! `acpi_idle`'s C2, the core's CC6, which power-gates it — is entered by
//! *reading an I/O port* the core traps: MSR `C001_0073` (CStateBaseAddr,
//! bits 15:0) holds the base, and a read of `base + n` requests the action
//! the firmware configured for slot `n`. ACPI's `_CST` names the port; this
//! kernel has no AML interpreter, so it takes `base + 1` — the slot the
//! target machine's `_CST` advertises as C2 (`ACPI IOPORT 0x414` in Linux's
//! cpuidle sysfs, with a base of `0x413`) — and the kernel reports both, so
//! a board where they differ is visible rather than silently wrong.
//!
//! **Energy.** RAPL on AMD (family 17h+): MSR `C001_0299` gives the energy
//! unit (bits 12:8, ESU: one count = 1/2^ESU J), `C001_029B` the package's
//! consumed energy as a wrapping 32-bit counter. Power over an interval is
//! the counter's delta, and it answers "is the machine idling well" long
//! before a temperature settles.

/// `MSR C001_0073`: CStateBaseAddr.
pub const MSR_CSTATE_BASE: u32 = 0xC001_0073;
/// `MSR C001_0299`: RAPL power unit.
pub const MSR_RAPL_UNIT: u32 = 0xC001_0299;
/// `MSR C001_029B`: package energy status.
pub const MSR_PKG_ENERGY: u32 = 0xC001_029B;

/// Whether the MSRs above exist: AMD family 17h (Zen) or later. Reading
/// them anywhere else is a #GP.
pub fn has_zen_msrs(vendor: &[u8; 12], family: u32) -> bool {
    vendor == b"AuthenticAMD" && family >= 0x17
}

/// The C2 port from CStateBaseAddr: `base + 1`. `None` when the core traps
/// no addresses (base 0) or the value is one the MSR cannot hold (the PPR
/// makes writes above `0xFFF8` a #GP).
pub fn c2_port(cstate_base_msr: u64) -> Option<u16> {
    let base = (cstate_base_msr & 0xFFFF) as u16;
    (base != 0 && base <= 0xFFF8).then(|| base + 1)
}

/// The energy status unit (ESU) from the RAPL unit register: one count is
/// 1/2^ESU J, so µJ = counts * 1_000_000 >> ESU.
pub fn energy_unit_shift(rapl_unit_msr: u64) -> u32 {
    ((rapl_unit_msr >> 8) & 0x1F) as u32
}

/// Energy between two readings of the 32-bit package counter, in µJ. One
/// wrap at most between readings (at ESU 16 the counter wraps every
/// 65536 J — minutes even at full load — so readers must sample more often
/// than that; the kernel does every tick).
pub fn energy_uj(prev: u32, now: u32, esu: u32) -> u64 {
    let counts = now.wrapping_sub(prev) as u64;
    (counts * 1_000_000) >> esu
}

/// `/proc/sensors`' energy line, as Linux's `amd_energy` hwmon driver
/// names it: `amd_energy<TAB>energy<TAB>Esocket0<TAB>microjoules`, the
/// package's energy since boot. A reader turns two of them into watts.
pub fn render_energy(package_uj: u64, out: &mut impl core::fmt::Write) -> core::fmt::Result {
    writeln!(out, "amd_energy\tenergy\tEsocket0\t{package_uj}")
}

/// C0 residency in permille over an interval: MPERF counts at the TSC's
/// rate but only while the core is in C0.
pub fn c0_permille(d_mperf: u64, d_tsc: u64) -> u32 {
    if d_tsc == 0 {
        return 0;
    }
    (d_mperf.saturating_mul(1000) / d_tsc).min(1000) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zen_only() {
        assert!(has_zen_msrs(b"AuthenticAMD", 0x19));
        assert!(has_zen_msrs(b"AuthenticAMD", 0x17));
        assert!(!has_zen_msrs(b"AuthenticAMD", 0x15));
        assert!(!has_zen_msrs(b"GenuineIntel", 0x19));
    }

    #[test]
    fn c2_port_is_base_plus_one() {
        // The target machine's _CST says C2 is a read of 0x414.
        assert_eq!(c2_port(0x413), Some(0x414));
        // Upper bits are reserved, not part of the address.
        assert_eq!(c2_port(0xDEAD_0000_0000_0413), Some(0x414));
        assert_eq!(c2_port(0), None);
        assert_eq!(c2_port(0xFFFF), None);
    }

    #[test]
    fn energy_units_and_wrap() {
        // Zen's usual unit register: ESU = 16 (15.3 µJ per count).
        let esu = energy_unit_shift(0x000A_1003);
        assert_eq!(esu, 16);
        assert_eq!(energy_uj(0, 65_536, esu), 1_000_000); // 1 J
        assert_eq!(energy_uj(u32::MAX - 65_535, 0, esu), 1_000_000); // across the wrap
        assert_eq!(energy_uj(5, 5, esu), 0);
    }

    #[test]
    fn energy_line() {
        let mut s = alloc::string::String::new();
        render_energy(18_000_000, &mut s).unwrap();
        assert_eq!(s, "amd_energy\tenergy\tEsocket0\t18000000\n");
    }

    #[test]
    fn c0_residency() {
        assert_eq!(c0_permille(50, 1000), 50);
        assert_eq!(c0_permille(2000, 1000), 1000);
        assert_eq!(c0_permille(1, 0), 0);
    }
}
