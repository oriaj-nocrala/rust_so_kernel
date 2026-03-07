// kernel/src/cpu/tsc.rs
//
// Time Stamp Counter — read and calibration.
//
// CALIBRATION: One full PIT period (10 ms at 100 Hz) is measured with the
// PIT channel-0 count read by busy-polling port I/O.  Called once during
// boot after pit::init() and before sti.
//
// USAGE:
//   cpu::tsc::init()        — calibrate (boot only)
//   cpu::tsc::read()        — raw 64-bit TSC
//   cpu::tsc::freq_hz()     — calibrated Hz (0 before init)
//   cpu::tsc::uptime_ns()   — nanoseconds since init
//   cpu::tsc::uptime_ms()   — milliseconds since init

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// TSC value captured by `init()`.
static TSC_BOOT: AtomicU64 = AtomicU64::new(0);

/// Calibrated TSC frequency in Hz; 0 until `init()` is called.
static TSC_FREQ_HZ: AtomicU64 = AtomicU64::new(0);

// ── Low-level ──────────────────────────────────────────────────────────────

/// Read the TSC with an `lfence` fence to prevent CPU reordering.
#[inline]
pub fn read() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!(
            "lfence",
            "rdtsc",
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack),
        );
    }
    ((hi as u64) << 32) | lo as u64
}

/// Latch and read the PIT channel-0 16-bit down-counter.
///
/// Sends a count-latch command to port 0x43 then reads two bytes from 0x40.
fn read_pit_count() -> u16 {
    let lo: u8;
    let hi: u8;
    unsafe {
        // Latch count for channel 0  (command byte = 0x00)
        asm!(
            "out dx, al",
            in("dx") 0x43u16,
            in("al") 0x00u8,
            options(nomem, nostack, preserves_flags)
        );
        asm!(
            "in al, dx",
            out("al") lo,
            in("dx") 0x40u16,
            options(nomem, nostack, preserves_flags)
        );
        asm!(
            "in al, dx",
            out("al") hi,
            in("dx") 0x40u16,
            options(nomem, nostack, preserves_flags)
        );
    }
    ((hi as u16) << 8) | lo as u16
}

// ── Calibration ────────────────────────────────────────────────────────────

/// Measure TSC ticks in exactly one PIT period (10 ms at 100 Hz).
///
/// Algorithm:
///   1. Sync to a period boundary by waiting for the counter to wrap
///      (cur > prev means the down-count rolled over from 0 to divisor).
///   2. Record `t0` at that boundary.
///   3. Busy-poll a second wrap; when it happens record `t1`.
///   4. `freq = (t1 - t0) * PIT_HZ`
fn calibrate() -> u64 {
    const PIT_HZ: u64 = 100; // must match pit::init(100) in init/devices.rs

    // ── Phase 1: synchronize to a period boundary ──────────────────────────
    let mut prev = read_pit_count();
    loop {
        let cur = read_pit_count();
        if cur > prev {
            // Wrap detected — we are now at the very start of a new period.
            break;
        }
        prev = cur;
    }

    // ── Phase 2: measure one full period ──────────────────────────────────
    let t0 = read();
    let start = read_pit_count();

    // We need to detect the next wrap: count must first descend below `start`
    // (ensuring we don't false-trigger on the same reading), then exceed it
    // again (which only happens on a reload from 0 to divisor).
    let mut seen_below = false;
    loop {
        let cur = read_pit_count();
        if !seen_below && cur < start {
            seen_below = true;
        }
        if seen_below && cur >= start {
            // Second wrap — one full period elapsed.
            let t1 = read();
            return (t1 - t0) * PIT_HZ;
        }
    }
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Calibrate the TSC and record the boot timestamp.
///
/// Must be called once, after `pit::init()`, while interrupts are still
/// masked.  Calling it a second time is harmless (overwrites the values).
pub fn init() {
    let freq = calibrate();
    TSC_FREQ_HZ.store(freq, Ordering::Relaxed);
    TSC_BOOT.store(read(), Ordering::Relaxed);
}

/// Returns the calibrated TSC frequency in Hz.
/// Returns 0 if `init()` has not been called yet.
pub fn freq_hz() -> u64 {
    TSC_FREQ_HZ.load(Ordering::Relaxed)
}

/// Returns nanoseconds elapsed since `init()` was called.
/// Returns 0 if not calibrated.
pub fn uptime_ns() -> u64 {
    let freq = TSC_FREQ_HZ.load(Ordering::Relaxed);
    if freq == 0 {
        return 0;
    }
    let boot = TSC_BOOT.load(Ordering::Relaxed);
    let delta = read().wrapping_sub(boot);
    // u128 multiplication prevents overflow for deltas up to ~580 years
    ((delta as u128 * 1_000_000_000) / freq as u128) as u64
}

/// Returns milliseconds elapsed since `init()` was called.
/// Returns 0 if not calibrated.
pub fn uptime_ms() -> u64 {
    uptime_ns() / 1_000_000
}
