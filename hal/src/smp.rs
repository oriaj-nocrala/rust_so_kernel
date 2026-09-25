//! Starting the application processors (APs) — the pure half of stage 4 of
//! `docs/smp/smp-plan.md`.
//!
//! An AP comes out of reset halted, waiting for an INIT and then a STARTUP
//! IPI (SIPI) whose 8-bit vector names the 4 KiB page, below 1 MiB, where it
//! starts executing in real mode. Everything that decides *what* gets sent
//! and *where* lives here: where the trampoline goes, the ICR encodings, the
//! INIT-SIPI-SIPI sequence with its delays, and which MADT CPU gets which
//! kernel CPU index. The kernel side (`kernel/src/smp.rs`) writes the
//! registers, waits and runs the trampoline.
//!
//! None of this fails loudly when wrong. A SIPI vector off by one page starts
//! the AP in whatever happens to be there; a wrong delivery mode is silently
//! ignored by the target; a trampoline the buddy allocator also handed out
//! gets overwritten under the AP's feet. Hence tested here.

use alloc::vec::Vec;

pub const PAGE: u64 = 4096;

/// The trampoline window: its code page, then the three page-table pages
/// (PML4, PDPT, PD) that identity-map it while the AP turns paging on. All
/// four must be below 4 GiB (the AP loads CR3 from 32-bit protected mode)
/// and the code page below 1 MiB (the SIPI vector is `address >> 12`, 8 bits).
pub const TRAMPOLINE_PAGES: u64 = 4;

/// Lowest address considered: page 0 holds the real-mode IVT and is where a
/// null pointer lands.
const LOW_START: u64 = 0x1000;
/// Conventional memory ends at 640 KiB; the EBDA, VGA and option ROMs sit
/// above it whatever the memory map says.
const LOW_END: u64 = 0xA_0000;

/// Picks the trampoline window from the bootloader's usable regions
/// (`[start, end)` pairs): the highest `TRAMPOLINE_PAGES`-page, page-aligned
/// window inside one usable region and inside `[4 KiB, 640 KiB)`. The caller
/// must keep those pages out of the physical allocator. `None` if conventional
/// memory has no such window (then the machine stays on one CPU).
pub fn pick_trampoline(usable: impl IntoIterator<Item = (u64, u64)>) -> Option<u64> {
    let size = TRAMPOLINE_PAGES * PAGE;
    let mut best = None;
    for (start, end) in usable {
        let start = align_up(start.max(LOW_START), PAGE);
        let end = end.min(LOW_END) & !(PAGE - 1);
        if end > start && end - start >= size {
            let base = end - size;
            if best.map_or(true, |b| base > b) {
                best = Some(base);
            }
        }
    }
    best
}

const fn align_up(x: u64, a: u64) -> u64 {
    (x + a - 1) & !(a - 1)
}

/// The SIPI vector that starts an AP at `base`: its page number. `None` if
/// no vector can name it (not page-aligned, page 0, or at/above 1 MiB).
pub const fn sipi_vector(base: u64) -> Option<u8> {
    if base % PAGE != 0 || base == 0 || base >= 0x10_0000 {
        None
    } else {
        Some((base >> 12) as u8)
    }
}

/// Interrupt Command Register low-dword encodings (SDM vol. 3, "Interrupt
/// Command Register").
pub mod icr {
    const DELIVERY_INIT: u32 = 0b101 << 8;
    const DELIVERY_STARTUP: u32 = 0b110 << 8;
    const LEVEL_ASSERT: u32 = 1 << 14;
    const TRIGGER_LEVEL: u32 = 1 << 15;
    /// xAPIC only: set while the previous IPI is still being sent.
    pub const DELIVERY_PENDING: u32 = 1 << 12;

    /// INIT, level-triggered, asserted: resets the target into
    /// wait-for-SIPI. Also how an AP that never answered is parked again.
    pub const fn init_assert() -> u32 {
        TRIGGER_LEVEL | LEVEL_ASSERT | DELIVERY_INIT
    }
    /// INIT level de-assert. Only the 82489DX needed it; Linux still sends
    /// it, and every later APIC ignores it.
    pub const fn init_deassert() -> u32 {
        TRIGGER_LEVEL | DELIVERY_INIT
    }
    /// STARTUP: begin at `vector << 12` in real mode.
    pub const fn startup(vector: u8) -> u32 {
        DELIVERY_STARTUP | vector as u32
    }
    /// xAPIC ICR high dword: physical destination in bits 24..31.
    pub const fn xapic_dest(apic_id: u32) -> u32 {
        apic_id << 24
    }
    /// x2APIC: one 64-bit write, destination in the high half.
    pub const fn x2apic(apic_id: u32, low: u32) -> u64 {
        (apic_id as u64) << 32 | low as u64
    }
}

/// One step of starting an AP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Write this ICR low dword to the target.
    Send(u32),
    /// Wait at least this many microseconds.
    Delay(u64),
}

/// INIT-SIPI-SIPI with the SDM's delays (vol. 3, "MP initialization
/// protocol algorithm"): INIT, 10 ms, then two SIPIs 200 µs apart. The second
/// SIPI is sent unconditionally, as Linux does: an AP that already started on
/// the first is no longer in wait-for-SIPI and ignores it.
pub const fn startup_sequence(vector: u8) -> [Step; 7] {
    [
        Step::Send(icr::init_assert()),
        Step::Delay(10_000),
        Step::Send(icr::init_deassert()),
        Step::Send(icr::startup(vector)),
        Step::Delay(200),
        Step::Send(icr::startup(vector)),
        Step::Delay(200),
    ]
}

/// Kernel CPU indices for the MADT's enabled CPUs: index 0 is the BSP
/// (whose MADT position is not necessarily first), the rest follow in MADT
/// order, without duplicates, up to `max` CPUs. Returns the APIC ID per
/// index and how many CPUs did not fit.
pub fn cpu_order(madt_apic_ids: &[u32], bsp_apic_id: u32, max: usize) -> (Vec<u32>, usize) {
    let mut order = alloc::vec![bsp_apic_id];
    let mut dropped = 0;
    for &id in madt_apic_ids {
        if order.contains(&id) {
            continue;
        }
        if order.len() < max {
            order.push(id);
        } else {
            dropped += 1;
        }
    }
    (order, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trampoline_takes_the_highest_window_below_640k() {
        // QEMU/OVMF-like: a small low region and a big one ending at 640 KiB.
        let regions = [(0x1000, 0x8000), (0x10000, 0xA0000), (0x10_0000, 0x800_0000)];
        assert_eq!(pick_trampoline(regions), Some(0xA0000 - 4 * PAGE));
    }

    #[test]
    fn trampoline_clips_regions_to_conventional_memory() {
        // A region straddling 640 KiB is clipped, not taken whole.
        assert_eq!(pick_trampoline([(0x90000, 0x20_0000)]), Some(0x9C000));
        // Everything above 1 MiB is useless for a SIPI.
        assert_eq!(pick_trampoline([(0x10_0000, 0x1000_0000)]), None);
    }

    #[test]
    fn trampoline_never_uses_page_zero_or_a_too_small_region() {
        assert_eq!(pick_trampoline([(0, 0x4000)]), None, "page 0 excluded, 3 pages left");
        assert_eq!(pick_trampoline([(0, 0x5000)]), Some(0x1000));
        assert_eq!(pick_trampoline([(0x2000, 0x5FFF)]), None, "3 whole pages only");
    }

    #[test]
    fn trampoline_aligns_unaligned_region_edges_inward() {
        assert_eq!(pick_trampoline([(0x1800, 0x6800)]), Some(0x2000));
    }

    #[test]
    fn trampoline_is_always_a_valid_sipi_target() {
        for regions in [
            &[(0x1000u64, 0xA0000u64)][..],
            &[(0x1234, 0x9_9999)],
            &[(0x500, 0x7_0000), (0x8_0000, 0x9_F000)],
        ] {
            let base = pick_trampoline(regions.iter().copied()).unwrap();
            assert!(sipi_vector(base).is_some(), "{base:#x}");
            assert!(base + TRAMPOLINE_PAGES * PAGE <= LOW_END);
            assert!(regions.iter().any(|&(s, e)| s <= base && base + TRAMPOLINE_PAGES * PAGE <= e));
        }
    }

    #[test]
    fn sipi_vector_is_the_page_number() {
        assert_eq!(sipi_vector(0x8000), Some(0x08));
        assert_eq!(sipi_vector(0x9C000), Some(0x9C));
        assert_eq!(sipi_vector(0xFF000), Some(0xFF));
        assert_eq!(sipi_vector(0x10_0000), None);
        assert_eq!(sipi_vector(0x8800), None);
        assert_eq!(sipi_vector(0), None);
    }

    #[test]
    fn icr_encodings_match_the_sdm() {
        // Linux: APIC_INT_LEVELTRIG | APIC_INT_ASSERT | APIC_DM_INIT etc.
        assert_eq!(icr::init_assert(), 0xC500);
        assert_eq!(icr::init_deassert(), 0x8500);
        assert_eq!(icr::startup(0x9C), 0x069C);
        assert_eq!(icr::xapic_dest(27), 27 << 24);
        assert_eq!(icr::x2apic(0x1B, 0x069C), 0x0000_001B_0000_069C);
    }

    #[test]
    fn startup_sequence_is_init_wait_sipi_sipi() {
        let s = startup_sequence(0x9C);
        assert_eq!(s[0], Step::Send(0xC500));
        assert_eq!(s[1], Step::Delay(10_000));
        let sipis: Vec<_> = s.iter().filter(|st| **st == Step::Send(0x069C)).collect();
        assert_eq!(sipis.len(), 2);
        // A delay between the two SIPIs and after the last one.
        let i = s.iter().position(|st| *st == Step::Send(0x069C)).unwrap();
        assert!(matches!(s[i + 1], Step::Delay(us) if us >= 200));
        assert!(matches!(s[s.len() - 1], Step::Delay(_)));
    }

    #[test]
    fn cpu_order_puts_the_bsp_first() {
        // The Ryzen's MADT: 0..11 then 16..27, with the BSP not necessarily first.
        let madt: Vec<u32> = (0..12).chain(16..28).collect();
        let (order, dropped) = cpu_order(&madt, 0, 32);
        assert_eq!(order.len(), 24);
        assert_eq!(order[0], 0);
        assert_eq!(order[12], 16);
        assert_eq!(dropped, 0);

        let (order, _) = cpu_order(&[1, 2, 3], 2, 8);
        assert_eq!(order, [2, 1, 3]);
    }

    #[test]
    fn cpu_order_caps_at_max_and_counts_the_rest() {
        let (order, dropped) = cpu_order(&[0, 1, 2, 3, 4, 5], 0, 4);
        assert_eq!(order, [0, 1, 2, 3]);
        assert_eq!(dropped, 2);
    }

    #[test]
    fn cpu_order_ignores_duplicates_and_a_bsp_missing_from_the_madt() {
        let (order, dropped) = cpu_order(&[3, 3, 5], 7, 8);
        assert_eq!(order, [7, 3, 5]);
        assert_eq!(dropped, 0);
    }
}
