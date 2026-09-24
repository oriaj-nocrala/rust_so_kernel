// kernel/src/watchdog.rs
//
// The AMD FCH "TCO" hardware watchdog, for unattended runs
// (docs/metal/autonomous-loop-plan.md, phase 4). A job that hangs the kernel
// — or just a spinning process that never lets the job finish — must still
// hand the machine back to Linux. Nothing here pings the watchdog: if the job
// has not called reboot(2) by the time it expires, the reset is the point.
//
// Armed on EVERY boot, early (`arm_early`, right after the framebuffer is
// set up, before ACPI/USB/storage), then disarmed once `autorun::detect`
// says there is no job (`settle`). Whether a boot is unattended is only
// known once `/mnt` is mounted, and arming only then left a hole: a kernel
// that hung or panicked in the drivers before that point — USB bring-up is
// the likeliest place on metal — stranded the machine. The cost is that a
// manual boot that hangs before `/mnt` also resets after TIMEOUT_SECS, which
// is no worse than hanging.
//
// Why constanos has to arm it itself: measured on the target board, the
// reset that boots constanos disarms whatever Linux armed (systemd's 10-min
// shutdown watchdog never fired on a constanos boot that ran 900 s).
//
// The register protocol is `hal::sp5100_tco` (pure, host-tested); this file
// finds the SMBus function, maps the two MMIO windows uncached, and keeps
// the mapping so `/proc/kdebug` can show the time left.
//
// What it does not cover: firmware → bootloader → the first few steps of
// `init::boot` (IDT, memory, PAT, framebuffer), before `arm_early` runs.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use hal::sp5100_tco::{self, Layout, Regs};
use x86_64::PhysAddr;

use crate::hal::{Driver, DriverError};
use crate::serial_println;

/// Seconds from arming (early boot) to the reset. Generous: every job so far
/// finishes in seconds, and a false reset would read as a HANG.
pub const TIMEOUT_SECS: u16 = 300;

/// Virtual address of the mapped watchdog window, 0 until armed.
static WDT_VIRT: AtomicU64 = AtomicU64::new(0);
static ARMED_SECS: AtomicU32 = AtomicU32::new(0);
/// 0 = never armed, 1 = armed, 2 = disarmed by `settle` (no autorun job).
static STATE: AtomicU32 = AtomicU32::new(0);

/// An uncached MMIO window (from `memory::mmio::map`).
struct MmioRegs(u64);

impl Regs for MmioRegs {
    fn read8(&self, off: usize) -> u8 {
        // SAFETY: `off` < WINDOW_LEN, inside a window mapped by `map_window`.
        unsafe { core::ptr::read_volatile((self.0 + off as u64) as *const u8) }
    }
    fn write8(&self, off: usize, val: u8) {
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile((self.0 + off as u64) as *mut u8, val) }
    }
    fn read32(&self, off: usize) -> u32 {
        // SAFETY: as above; offsets used are 0 and 4, dword-aligned.
        unsafe { core::ptr::read_volatile((self.0 + off as u64) as *const u32) }
    }
    fn write32(&self, off: usize, val: u32) {
        // SAFETY: as above.
        unsafe { core::ptr::write_volatile((self.0 + off as u64) as *mut u32, val) }
    }
}

fn map_window(phys: u64) -> Option<MmioRegs> {
    // SAFETY: both addresses are FCH register windows (Linux maps the same
    // ones), not RAM.
    unsafe { crate::memory::mmio::map(PhysAddr::new(phys), sp5100_tco::WINDOW_LEN) }
        .map(|v| MmioRegs(v.as_u64()))
}

pub struct WatchdogDriver;

impl Driver for WatchdogDriver {
    fn name(&self) -> &str {
        "sp5100_tco"
    }

    fn init(&mut self) -> Result<(), DriverError> {
        // The TCO hangs off the FCH's SMBus function (class 0C/05/00).
        let mut found = None;
        crate::pci::for_each_by_class(0x0C, 0x05, 0x00, 4, |f| {
            let rev = crate::pci::revision_id(f.bus, f.device, f.function);
            if found.is_none() {
                if let Some(l) = sp5100_tco::layout(f.vendor, f.device_id, rev) {
                    found = Some((f, rev, l));
                }
            }
        });
        let Some((f, rev, layout)) = found else {
            serial_println!("watchdog: no AMD FCH SMBus — no hardware watchdog, a hang will not reset");
            return Err(DriverError::NotFound);
        };
        serial_println!(
            "watchdog: SMBus {:04x}:{:04x} rev {:#04x} at {:02x}:{:02x}.{} — layout {:?}",
            f.vendor, f.device_id, rev, f.bus, f.device, f.function, layout
        );
        if layout != Layout::EfchMmio {
            serial_println!("watchdog: layout {:?} not implemented (only EfchMmio) — a hang will not reset", layout);
            return Err(DriverError::NotFound);
        }

        let Some(pm) = map_window(sp5100_tco::PM_MMIO_ADDR) else {
            serial_println!("watchdog: cannot map the PM window");
            return Err(DriverError::NotFound);
        };
        let decode = sp5100_tco::enable_decode(&pm).map_err(|e| {
            serial_println!("watchdog: enabling decode failed: {:?}", e);
            DriverError::Invalid
        })?;
        let Some(wdt) = map_window(sp5100_tco::WDT_MMIO_ADDR) else {
            serial_println!("watchdog: cannot map the watchdog window");
            return Err(DriverError::NotFound);
        };
        let armed = sp5100_tco::arm(&wdt, TIMEOUT_SECS).map_err(|e| {
            serial_println!("watchdog: arming failed: {:?}", e);
            DriverError::Invalid
        })?;

        WDT_VIRT.store(wdt.0, Ordering::Relaxed);
        ARMED_SECS.store(TIMEOUT_SECS as u32, Ordering::Relaxed);
        STATE.store(1, Ordering::Relaxed);
        serial_println!(
            "watchdog: ARMED early, reset in {} s unless disarmed (count now {}); control was {:#x}{}; decode {}{}",
            armed.timeout_secs,
            sp5100_tco::time_left(&wdt),
            armed.control_before,
            if armed.was_fired { " — the previous reset was this watchdog's" } else { "" },
            if decode.was_enabled { "already on" } else { "enabled here" },
            if decode.alt_addr.is_some() { ", alt window decoded" } else { "" },
        );
        Ok(())
    }
}

/// Arms the watchdog on every boot, as early as MMIO mappings exist. Call
/// right after the framebuffer setup in `init::boot`; `settle` undoes it for
/// boots that turn out not to be unattended.
pub fn arm_early() {
    crate::hal::run_all(&mut [&mut WatchdogDriver]);
}

/// Keeps the watchdog for an unattended run, disarms it otherwise. Call
/// right after `autorun::detect()`.
pub fn settle() {
    let virt = WDT_VIRT.load(Ordering::Relaxed);
    if virt == 0 {
        return;
    }
    let wdt = MmioRegs(virt);
    if crate::autorun::enabled() {
        serial_println!("watchdog: kept armed for the autorun job, {} s left", sp5100_tco::time_left(&wdt));
    } else {
        sp5100_tco::disarm(&wdt);
        STATE.store(2, Ordering::Relaxed);
        serial_println!(
            "watchdog: disarmed (no autorun job), {}",
            if sp5100_tco::is_running(&wdt) { "BUT STILL RUNNING" } else { "stopped" }
        );
    }
}

/// Test hook for the early-hang path: a kernel built with
/// `CONSTANOS_TEST_HANG_BEFORE_FS=1` in the environment spins forever right
/// before mounting `/mnt`, which is exactly the stretch `arm_early` exists
/// to cover. On the metal loop it must come back as a watchdog reset
/// (`metal-run.sh --collect`: `NO-BOOT ... [watchdog reset: bootstatus=32]`,
/// since the log partition is claimed after `/mnt`). Never set otherwise.
pub fn test_hang_before_fs() {
    if option_env!("CONSTANOS_TEST_HANG_BEFORE_FS").is_some() {
        serial_println!("watchdog: CONSTANOS_TEST_HANG_BEFORE_FS — hanging before fs::init on purpose");
        loop {
            core::hint::spin_loop();
        }
    }
}

/// One line for `/proc/kdebug`.
pub fn render() -> alloc::string::String {
    let virt = WDT_VIRT.load(Ordering::Relaxed);
    if virt == 0 {
        return alloc::string::String::from("watchdog: off (no supported hardware)");
    }
    let wdt = MmioRegs(virt);
    alloc::format!(
        "watchdog: {} (timeout {} s), {} s left, {}",
        if STATE.load(Ordering::Relaxed) == 2 { "disarmed, no autorun job" } else { "armed" },
        ARMED_SECS.load(Ordering::Relaxed),
        sp5100_tco::time_left(&wdt),
        if sp5100_tco::is_running(&wdt) { "running" } else { "stopped" }
    )
}
