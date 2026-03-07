// kernel/src/time/mod.rs
//
// Time subsystem: clocksource selection, jiffies counter, hrtimers.
//
// INIT ORDER (called from init/mod.rs after TSC calibration):
//   time::init() → clocksource::select_best()

pub mod clockevent;
pub mod clocksource;
pub mod hrtimer;

pub use clocksource::ktime_get;

/// Initialise the time subsystem.
///
/// Must be called after `cpu::tsc::init()` so that the TSC clocksource
/// returns meaningful values.
pub fn init() {
    clocksource::select_best();
}
