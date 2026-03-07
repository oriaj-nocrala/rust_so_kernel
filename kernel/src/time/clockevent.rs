// kernel/src/time/clockevent.rs
//
// Clockevent: jiffies counter driven by the PIT at 100 Hz.
//
// JIFFIES is incremented once per timer interrupt (every 10 ms).
// Atomic operations keep it ISR-safe without a lock.

use core::sync::atomic::{AtomicU64, Ordering};

/// Global jiffy counter. Each PIT interrupt increments this by 1.
static JIFFIES: AtomicU64 = AtomicU64::new(0);

/// PIT tick period in nanoseconds (10 ms at 100 Hz).
pub const PERIOD_NS: u64 = 10_000_000;

/// Called from the timer ISR. Increments the jiffy counter and returns the
/// new value. Using Relaxed ordering: the ISR is single-CPU and the atomic
/// guarantees no torn writes.
#[inline]
pub fn tick() -> u64 {
    JIFFIES.fetch_add(1, Ordering::Relaxed) + 1
}

/// Returns the current jiffy count (ticks since boot, 10 ms each).
#[inline]
pub fn jiffies() -> u64 {
    JIFFIES.load(Ordering::Relaxed)
}

/// Convert a jiffy count to nanoseconds.
#[inline]
pub fn jiffies_to_ns(j: u64) -> u64 {
    j * PERIOD_NS
}

/// Stub for future one-shot clockevent programming.
/// Currently the PIT runs in periodic mode at 100 Hz; this is a no-op.
#[allow(dead_code)]
#[inline]
pub fn set_next_ns(_ns: u64) {}
