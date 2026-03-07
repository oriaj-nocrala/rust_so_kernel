// kernel/src/time/clocksource.rs
//
// Clocksource: selects the best available time source at boot.
//
// Sources (highest rating wins):
//   tsc    — TSC via cpu::tsc::uptime_ns() — rating 300
//   jiffies — jiffy counter × PERIOD_NS    — rating 50
//
// ACTIVE_IDX is set once by select_best() during init and then only read,
// so no lock is needed for ktime_get().

use core::sync::atomic::{AtomicUsize, Ordering};
use crate::serial_println;

pub struct ClockSourceInfo {
    pub name: &'static str,
    pub rating: u32,
    /// Function returning nanoseconds since boot.
    pub read_ns: fn() -> u64,
}

fn tsc_read_ns() -> u64 {
    crate::cpu::tsc::uptime_ns()
}

fn jiffies_read_ns() -> u64 {
    super::clockevent::jiffies_to_ns(super::clockevent::jiffies())
}

static SOURCES: &[ClockSourceInfo] = &[
    ClockSourceInfo { name: "tsc",     rating: 300, read_ns: tsc_read_ns     },
    ClockSourceInfo { name: "jiffies", rating:  50, read_ns: jiffies_read_ns },
];

/// Index into SOURCES of the currently active clocksource.
static ACTIVE_IDX: AtomicUsize = AtomicUsize::new(0);

/// Select the highest-rated clocksource and print its name to serial.
/// Call once during boot after TSC is calibrated.
pub fn select_best() {
    let best = SOURCES
        .iter()
        .enumerate()
        .max_by_key(|(_, s)| s.rating)
        .map(|(i, _)| i)
        .unwrap_or(0);

    ACTIVE_IDX.store(best, Ordering::Relaxed);
    serial_println!(
        "clocksource: selected '{}' (rating {})",
        SOURCES[best].name,
        SOURCES[best].rating
    );
}

/// Returns nanoseconds since boot using the active clocksource.
#[inline]
pub fn ktime_get() -> u64 {
    let idx = ACTIVE_IDX.load(Ordering::Relaxed);
    (SOURCES[idx].read_ns)()
}

/// Returns the name of the currently active clocksource.
pub fn clocksource_name() -> &'static str {
    let idx = ACTIVE_IDX.load(Ordering::Relaxed);
    SOURCES[idx].name
}
