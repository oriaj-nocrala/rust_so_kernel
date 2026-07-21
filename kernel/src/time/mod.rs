// kernel/src/time/mod.rs
//
// Time subsystem: clocksource selection, jiffies counter, hrtimers, and a
// real wall-clock epoch (CMOS RTC, read once at boot — see `crate::rtc`).
//
// INIT ORDER (called from init/mod.rs after TSC calibration):
//   time::init() → clocksource::select_best() → crate::rtc::read_unix_time()

use core::sync::atomic::{AtomicU64, Ordering};

pub mod clockevent;
pub mod clocksource;
pub mod hrtimer;

pub use clocksource::ktime_get;

/// Unix epoch seconds at the moment the RTC was read during boot, or 0 if
/// no RTC ever answered (`now_unix_secs()` then just degrades to reporting
/// uptime, same "boot = epoch" fallback this kernel used everywhere before
/// a real RTC reading existed).
static BOOT_UNIX_SECS: AtomicU64 = AtomicU64::new(0);

/// Initialise the time subsystem.
///
/// Must be called after `cpu::tsc::init()` so that the TSC clocksource
/// returns meaningful values.
pub fn init() {
    clocksource::select_best();

    match crate::rtc::read_unix_time() {
        Some(secs) => {
            BOOT_UNIX_SECS.store(secs, Ordering::Relaxed);
            crate::serial_println!("rtc: read {} (unix epoch seconds)", secs);
        }
        None => {
            crate::serial_println!("rtc: no response — real-time clock unavailable, falling back to boot=epoch");
        }
    }
}

/// Current real (wall-clock) Unix epoch time in seconds: the RTC reading
/// captured once at boot, plus monotonic uptime since then. Never reads
/// the RTC hardware again after boot — there's no periodic RTC IRQ wired
/// up, and none is needed for this.
pub fn now_unix_secs() -> u64 {
    BOOT_UNIX_SECS.load(Ordering::Relaxed) + ktime_get() / 1_000_000_000
}
