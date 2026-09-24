//! AMD SP5100 / SB800 / FCH "TCO" watchdog — the register protocol.
//!
//! Exists for the unattended bare-metal loop (docs/metal/autonomous-loop-plan.md,
//! phase 4): a job that hangs the kernel must still come back to Linux, and
//! the reset that brings it back cannot come from constanos itself. It was
//! measured on the target board (ASUS PRIME B450M-A II, FCH SMBus
//! `1022:790b` rev `0x61`) that arming this watchdog from Linux does not help:
//! the reset into constanos disarms it. So constanos arms it itself.
//!
//! Ported from Linux's `drivers/watchdog/sp5100_tco.{c,h}`, `efch_mmio`
//! layout only — the one this board uses, and the only one that can be
//! tested on real hardware here. The older layouts reach the same registers
//! through the 0xCD6/0xCD7 index/data ports; `layout()` still classifies
//! them so the adapter can say "unsupported" instead of "not found".
//!
//! Two register windows, both MMIO, both 8 bytes:
//!
//! * **PM** at `0xFED8_0300` (ACPI MMIO + 0x300), byte-wide: `DECODEEN`
//!   (0x00) bit 7 enables decoding of the watchdog window *and* the timer;
//!   `DECODEEN3` (0x03) holds the resolution (bits 1:0) and two disable
//!   bits (3:2); `ISACONTROL` (0x04) bit 1 says the alternate window at
//!   ACPI MMIO + 0xB00 is decoded too.
//! * **WDT** at `0xFEB0_0000`, dword-wide: control (0x00) and count (0x04).
//!
//! Same split as every `hal` driver: this module *decides and sequences*
//! through the [`Regs`] seam; the kernel maps the windows and owns the
//! globals. No logging here — outcomes come back as data.

/// Physical address of the PM register window (`EFCH_PM_ACPI_MMIO_PM_ADDR`).
pub const PM_MMIO_ADDR: u64 = 0xFED8_0000 + 0x300;
/// Physical address of the watchdog window when `DECODEEN_WDT_TMREN` is set
/// (`EFCH_PM_WDT_ADDR`). This is what Linux logged on the target board.
pub const WDT_MMIO_ADDR: u64 = 0xFEB0_0000;
/// Alternate watchdog window (`EFCH_PM_ACPI_MMIO_ADDR + WDT_OFFSET`).
pub const WDT_ALT_MMIO_ADDR: u64 = 0xFED8_0000 + 0xB00;
/// Both windows are this long.
pub const WINDOW_LEN: usize = 8;

// PM window (byte registers).
const PM_DECODEEN: usize = 0x00;
const PM_DECODEEN_WDT_TMREN: u8 = 1 << 7;
const PM_DECODEEN3: usize = 0x03;
const PM_DECODEEN3_SECOND_RES: u8 = 0b0000_0011;
const PM_DECODEEN3_WDT_DISABLE: u8 = 0b0000_1100;
const PM_ISACONTROL: usize = 0x04;
const PM_ISACONTROL_MMIOEN: u8 = 1 << 1;

// WDT window (dword registers).
const WDT_CONTROL: usize = 0x00;
const WDT_COUNT: usize = 0x04;
const CTL_START: u32 = 1 << 0;
const CTL_FIRED: u32 = 1 << 1;
const CTL_ACTION_POWEROFF: u32 = 1 << 2;
const CTL_DISABLED: u32 = 1 << 3;
const CTL_TRIGGER: u32 = 1 << 7;

/// PCI IDs of the SMBus function the TCO hangs off (`sp5100_tco_pci_tbl`).
pub const VENDOR_ATI: u16 = 0x1002;
pub const VENDOR_AMD: u16 = 0x1022;
pub const VENDOR_HYGON: u16 = 0x1D94;
pub const DEVICE_ATI_SBX00_SMBUS: u16 = 0x4385;
pub const DEVICE_AMD_HUDSON2_SMBUS: u16 = 0x780B;
pub const DEVICE_AMD_KERNCZ_SMBUS: u16 = 0x790B;

/// A byte/dword register window, addressed by offset from its base. The
/// kernel implements it over an uncached MMIO mapping; tests over an array.
pub trait Regs {
    fn read8(&self, off: usize) -> u8;
    fn write8(&self, off: usize, val: u8);
    fn read32(&self, off: usize) -> u32;
    fn write32(&self, off: usize, val: u32);
}

/// Which generation of the register interface a chipset uses — Linux's
/// `enum tco_reg_layout`, same decision tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// SP5100 / SB7x0: index/data ports + PCI config. Not implemented here.
    Sp5100,
    /// SB8x0 and later pre-Zen FCHs: index/data ports. Not implemented here.
    Sb800,
    /// Embedded FCH through the index/data ports. Not implemented here.
    Efch,
    /// Zen-era FCH (SMBus rev >= 0x51): everything through MMIO. Implemented.
    EfchMmio,
}

/// Classifies an SMBus function, or `None` if it is not one the TCO lives
/// on. Mirrors `tco_reg_layout()` plus the PCI match table.
pub fn layout(vendor: u16, device: u16, revision: u8) -> Option<Layout> {
    let amdish = vendor == VENDOR_AMD || vendor == VENDOR_HYGON;
    match (vendor, device) {
        (VENDOR_ATI, DEVICE_ATI_SBX00_SMBUS) if revision < 0x40 => Some(Layout::Sp5100),
        (VENDOR_ATI, DEVICE_ATI_SBX00_SMBUS) => Some(Layout::Sb800),
        (_, DEVICE_AMD_KERNCZ_SMBUS) if amdish && revision >= 0x51 => Some(Layout::EfchMmio),
        (_, DEVICE_AMD_KERNCZ_SMBUS) if amdish && revision >= 0x49 => Some(Layout::Efch),
        (_, DEVICE_AMD_HUDSON2_SMBUS) if amdish && revision >= 0x41 => Some(Layout::Efch),
        (_, DEVICE_AMD_KERNCZ_SMBUS) | (_, DEVICE_AMD_HUDSON2_SMBUS) if amdish => Some(Layout::Sb800),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcoError {
    /// `DECODEEN_WDT_TMREN` did not stick after being set.
    DecodeNotEnabled,
    /// The control register reads all-ones: nothing answers at the window.
    NoDevice,
    /// The control register reports the watchdog hardware disabled.
    HardwareDisabled,
    /// A zero timeout would reset immediately (Linux's `min_timeout` is 1).
    ZeroTimeout,
    /// The start bit did not read back set after starting.
    DidNotStart,
}

/// What `enable_decode` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decode {
    /// Whether `DECODEEN_WDT_TMREN` was already set (by the firmware) or
    /// had to be set here.
    pub was_enabled: bool,
    /// The alternate window, if the FCH decodes it (`ISACONTROL_MMIOEN`).
    /// Informational: Linux uses it only if the primary is taken.
    pub alt_addr: Option<u64>,
}

/// `sp5100_tco_setupdevice_mmio` up to choosing a base, plus
/// `tco_timer_enable_mmio`: enable decoding of the watchdog window, then
/// set 1-second resolution and clear the disable bits.
pub fn enable_decode<R: Regs>(pm: &R) -> Result<Decode, TcoError> {
    let before = pm.read8(PM_DECODEEN);
    let was_enabled = before & PM_DECODEEN_WDT_TMREN != 0;
    if !was_enabled {
        pm.write8(PM_DECODEEN, before | PM_DECODEEN_WDT_TMREN);
    }
    if pm.read8(PM_DECODEEN) & PM_DECODEEN_WDT_TMREN == 0 {
        return Err(TcoError::DecodeNotEnabled);
    }
    let alt_addr = (pm.read8(PM_ISACONTROL) & PM_ISACONTROL_MMIOEN != 0).then_some(WDT_ALT_MMIO_ADDR);

    let d3 = pm.read8(PM_DECODEEN3);
    pm.write8(PM_DECODEEN3, (d3 & !PM_DECODEEN3_WDT_DISABLE) | PM_DECODEEN3_SECOND_RES);
    Ok(Decode { was_enabled, alt_addr })
}

/// What `arm` found and did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Armed {
    /// Control register as first read.
    pub control_before: u32,
    /// `WatchDogFired` was set: the previous reset was this watchdog's.
    /// (Writing the control value back clears it, as Linux does.)
    pub was_fired: bool,
    /// Seconds, as written to the count register.
    pub timeout_secs: u16,
}

/// `sp5100_tco_timer_init` + `tco_timer_start`: action = reset, count =
/// `timeout_secs`, stop, then start. Nothing pings it afterwards: expiring
/// is the point.
pub fn arm<R: Regs>(wdt: &R, timeout_secs: u16) -> Result<Armed, TcoError> {
    if timeout_secs == 0 {
        return Err(TcoError::ZeroTimeout);
    }
    let control_before = wdt.read32(WDT_CONTROL);
    if control_before == u32::MAX {
        return Err(TcoError::NoDevice);
    }
    if control_before & CTL_DISABLED != 0 {
        return Err(TcoError::HardwareDisabled);
    }
    let was_fired = control_before & CTL_FIRED != 0;

    // Action: reset, not power off.
    let val = control_before & !CTL_ACTION_POWEROFF;
    wdt.write32(WDT_CONTROL, val);
    wdt.write32(WDT_COUNT, timeout_secs as u32);

    // Stop before starting, so the start can't race a stale count.
    let val = wdt.read32(WDT_CONTROL) & !CTL_START;
    wdt.write32(WDT_CONTROL, val);

    // Start, then trigger (reload the count). Linux: "This must be a
    // distinct write."
    let val = wdt.read32(WDT_CONTROL) | CTL_START;
    wdt.write32(WDT_CONTROL, val);
    wdt.write32(WDT_CONTROL, val | CTL_TRIGGER);

    if wdt.read32(WDT_CONTROL) & CTL_START == 0 {
        return Err(TcoError::DidNotStart);
    }
    Ok(Armed { control_before, was_fired, timeout_secs })
}

/// `tco_timer_stop`: clear the start bit. The count and action stay as
/// they were, so a later `arm` starts from a known state anyway.
pub fn disarm<R: Regs>(wdt: &R) {
    let val = wdt.read32(WDT_CONTROL) & !CTL_START;
    wdt.write32(WDT_CONTROL, val);
}

/// Seconds left before the reset (`tco_timer_get_timeleft`).
pub fn time_left<R: Regs>(wdt: &R) -> u32 {
    wdt.read32(WDT_COUNT)
}

/// Whether the timer is running.
pub fn is_running<R: Regs>(wdt: &R) -> bool {
    wdt.read32(WDT_CONTROL) & CTL_START != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    /// An 8-byte window with a write log, and bits the "hardware" refuses
    /// to change (to model a stuck enable or a start that doesn't take).
    struct Window {
        bytes: RefCell<[u8; 8]>,
        stuck_clear: [u8; 8],
        log: RefCell<Vec<(usize, u32)>>,
    }

    impl Window {
        fn new(init: [u8; 8]) -> Self {
            Window { bytes: RefCell::new(init), stuck_clear: [0; 8], log: RefCell::new(Vec::new()) }
        }
        fn with_dwords(control: u32, count: u32) -> Self {
            let mut b = [0u8; 8];
            b[..4].copy_from_slice(&control.to_le_bytes());
            b[4..].copy_from_slice(&count.to_le_bytes());
            Self::new(b)
        }
        fn writes(&self) -> Vec<(usize, u32)> {
            self.log.borrow().clone()
        }
    }

    impl Regs for Window {
        fn read8(&self, off: usize) -> u8 {
            self.bytes.borrow()[off]
        }
        fn write8(&self, off: usize, val: u8) {
            self.log.borrow_mut().push((off, val as u32));
            self.bytes.borrow_mut()[off] = val & !self.stuck_clear[off];
        }
        fn read32(&self, off: usize) -> u32 {
            let b = self.bytes.borrow();
            u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
        }
        fn write32(&self, off: usize, val: u32) {
            self.log.borrow_mut().push((off, val));
            let mut b = self.bytes.borrow_mut();
            for i in 0..4 {
                b[off + i] = (val >> (8 * i)) as u8 & !self.stuck_clear[off + i];
            }
        }
    }

    #[test]
    fn layout_matches_linux_decision_tree() {
        // The target board: FCH SMBus 1022:790b rev 0x61.
        assert_eq!(layout(VENDOR_AMD, DEVICE_AMD_KERNCZ_SMBUS, 0x61), Some(Layout::EfchMmio));
        assert_eq!(layout(VENDOR_AMD, DEVICE_AMD_KERNCZ_SMBUS, 0x51), Some(Layout::EfchMmio));
        assert_eq!(layout(VENDOR_HYGON, DEVICE_AMD_KERNCZ_SMBUS, 0x51), Some(Layout::EfchMmio));
        assert_eq!(layout(VENDOR_AMD, DEVICE_AMD_KERNCZ_SMBUS, 0x50), Some(Layout::Efch));
        assert_eq!(layout(VENDOR_AMD, DEVICE_AMD_KERNCZ_SMBUS, 0x49), Some(Layout::Efch));
        assert_eq!(layout(VENDOR_AMD, DEVICE_AMD_KERNCZ_SMBUS, 0x48), Some(Layout::Sb800));
        assert_eq!(layout(VENDOR_AMD, DEVICE_AMD_HUDSON2_SMBUS, 0x41), Some(Layout::Efch));
        assert_eq!(layout(VENDOR_AMD, DEVICE_AMD_HUDSON2_SMBUS, 0x40), Some(Layout::Sb800));
        assert_eq!(layout(VENDOR_ATI, DEVICE_ATI_SBX00_SMBUS, 0x3F), Some(Layout::Sp5100));
        assert_eq!(layout(VENDOR_ATI, DEVICE_ATI_SBX00_SMBUS, 0x40), Some(Layout::Sb800));
        // Not a TCO host: another vendor's SMBus, or an AMD non-SMBus device.
        assert_eq!(layout(0x8086, DEVICE_AMD_KERNCZ_SMBUS, 0x61), None);
        assert_eq!(layout(VENDOR_AMD, 0x1480, 0x00), None);
    }

    #[test]
    fn enable_decode_sets_tmren_resolution_and_clears_disable() {
        // DECODEEN3 starts with both disable bits set and resolution 0.
        let pm = Window::new([0x05, 0, 0, 0b1111_0000 | PM_DECODEEN3_WDT_DISABLE, PM_ISACONTROL_MMIOEN, 0, 0, 0]);
        let d = enable_decode(&pm).unwrap();
        assert!(!d.was_enabled);
        assert_eq!(d.alt_addr, Some(WDT_ALT_MMIO_ADDR));
        assert_eq!(pm.read8(PM_DECODEEN), 0x85, "TMREN set, other bits preserved");
        assert_eq!(pm.read8(PM_DECODEEN3), 0b1111_0011, "disable bits cleared, 1 s resolution, rest preserved");
    }

    #[test]
    fn enable_decode_leaves_decodeen_alone_when_firmware_set_it() {
        let pm = Window::new([0x80, 0, 0, 0, 0, 0, 0, 0]);
        let d = enable_decode(&pm).unwrap();
        assert!(d.was_enabled);
        assert_eq!(d.alt_addr, None);
        assert!(pm.writes().iter().all(|&(off, _)| off != PM_DECODEEN), "no write to DECODEEN");
    }

    #[test]
    fn enable_decode_fails_when_tmren_does_not_stick() {
        let mut pm = Window::new([0; 8]);
        pm.stuck_clear[PM_DECODEEN] = PM_DECODEEN_WDT_TMREN;
        assert_eq!(enable_decode(&pm), Err(TcoError::DecodeNotEnabled));
        assert!(pm.writes().iter().all(|&(off, _)| off != PM_DECODEEN3), "gave up before DECODEEN3");
    }

    #[test]
    fn arm_sequence_matches_linux() {
        // Firmware left the action at power-off and the timer stopped.
        let wdt = Window::with_dwords(CTL_ACTION_POWEROFF, 0);
        let a = arm(&wdt, 300).unwrap();
        assert_eq!(a, Armed { control_before: CTL_ACTION_POWEROFF, was_fired: false, timeout_secs: 300 });
        assert_eq!(
            wdt.writes(),
            [
                (WDT_CONTROL, 0),                        // action = reset
                (WDT_COUNT, 300),                        // heartbeat
                (WDT_CONTROL, 0),                        // stop
                (WDT_CONTROL, CTL_START),                // start...
                (WDT_CONTROL, CTL_START | CTL_TRIGGER),  // ...then trigger, a distinct write
            ]
        );
        assert!(is_running(&wdt));
        assert_eq!(time_left(&wdt), 300);
    }

    #[test]
    fn arm_reports_a_previous_watchdog_reset() {
        let wdt = Window::with_dwords(CTL_FIRED, 0);
        assert!(arm(&wdt, 60).unwrap().was_fired);
    }

    #[test]
    fn arm_refuses_disabled_absent_and_zero() {
        assert_eq!(arm(&Window::with_dwords(CTL_DISABLED, 0), 60), Err(TcoError::HardwareDisabled));
        assert_eq!(arm(&Window::with_dwords(u32::MAX, u32::MAX), 60), Err(TcoError::NoDevice));
        let wdt = Window::with_dwords(0, 0);
        assert_eq!(arm(&wdt, 0), Err(TcoError::ZeroTimeout));
        assert!(wdt.writes().is_empty(), "refusals write nothing");
        let wdt = Window::with_dwords(CTL_DISABLED, 0);
        let _ = arm(&wdt, 60);
        assert!(wdt.writes().is_empty());
    }

    #[test]
    fn disarm_clears_only_the_start_bit() {
        let wdt = Window::with_dwords(0, 0);
        arm(&wdt, 300).unwrap();
        let before = wdt.read32(WDT_CONTROL);
        disarm(&wdt);
        assert!(!is_running(&wdt));
        assert_eq!(wdt.read32(WDT_CONTROL), before & !CTL_START);
        assert_eq!(wdt.writes().last(), Some(&(WDT_CONTROL, before & !CTL_START)));
        assert_eq!(time_left(&wdt), 300, "count untouched");
    }

    #[test]
    fn arm_detects_a_start_that_does_not_take() {
        let mut wdt = Window::with_dwords(0, 0);
        wdt.stuck_clear[WDT_CONTROL] = CTL_START as u8;
        assert_eq!(arm(&wdt, 60), Err(TcoError::DidNotStart));
    }
}
