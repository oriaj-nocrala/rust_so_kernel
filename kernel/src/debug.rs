// kernel/src/debug.rs
//
// Runtime-toggleable tracing + a handful of permanent lifecycle counters.
//
// WHY THIS EXISTS
// ────────────────
// Before this module, debugging a subsystem meant hand-adding
// `serial_println!`/`serial_println_raw!` calls, rebuilding, reproducing,
// reading the log, then manually stripping the prints back out — thrown
// away each time, so the next bug in a *different* subsystem starts from
// zero visibility again. It also meant several subsystems (COW faults,
// address-space teardown, exec) had PERMANENT, unconditional debug prints
// left in from past sessions — always on, drowning out whatever a future
// session actually needed to see (this is exactly what made the 2026-07-19
// leak/panic investigation slow: the one relevant line was buried under
// thousands of `[COW]` lines that fire on every single page fault).
//
// This module fixes both: named subsystems, gated by a runtime bitmask
// (default: everything off — silent unless asked for), so instrumentation
// can stay in the code permanently instead of being added and removed each
// time. Toggle a subsystem live via the `kdebug_ctl` syscall (403) — no
// rebuild needed — e.g. the `kdebug` userspace program: `kdebug mm on`.
//
// ADDING A NEW TRACEPOINT
// ───────────────────────
//   crate::ktrace!(crate::debug::MM, "fork: shared {} pages", n);
//
// ADDING A NEW COUNTER
// ────────────────────
//   Add an `AtomicU64` below, a matching `inc_*`/getter, and a line in
//   `render_report()` — then it shows up in `/proc/kdebug` for free.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

// ── Subsystems ───────────────────────────────────────────────────────────────

/// A named, independently-toggleable tracing subsystem.
pub struct Subsystem {
    pub bit:  u32,
    pub name: &'static str,
}

pub const MM:    Subsystem = Subsystem { bit: 1 << 0, name: "mm" };
pub const SCHED: Subsystem = Subsystem { bit: 1 << 1, name: "sched" };
pub const FS:    Subsystem = Subsystem { bit: 1 << 2, name: "fs" };
pub const PROC:  Subsystem = Subsystem { bit: 1 << 3, name: "proc" };

/// All subsystems, for `kdebug list` / mask validation.
pub const ALL_SUBSYSTEMS: &[&Subsystem] = &[&MM, &SCHED, &FS, &PROC];

/// Bitmask of currently-enabled subsystems. Off by default: tracing is
/// opt-in, never spamming the log unless explicitly turned on.
static TRACE_MASK: AtomicU32 = AtomicU32::new(0);

pub fn set_mask(mask: u32) -> u32 {
    TRACE_MASK.swap(mask, Ordering::Relaxed)
}

pub fn get_mask() -> u32 {
    TRACE_MASK.load(Ordering::Relaxed)
}

#[inline]
pub fn is_enabled(bit: u32) -> bool {
    TRACE_MASK.load(Ordering::Relaxed) & bit != 0
}

/// Trace a formatted line, gated on `$sub`'s bit in `TRACE_MASK` — a no-op
/// (one relaxed atomic load + branch) when that subsystem is disabled.
/// Uses the lock-free raw serial writer (like the debug prints it
/// replaces) since tracepoints can fire from contexts (page fault handler,
/// `Drop` impls mid-teardown) where taking the buffered serial lock would
/// risk deadlock.
#[macro_export]
macro_rules! ktrace {
    ($sub:expr, $($arg:tt)*) => {
        if $crate::debug::is_enabled($sub.bit) {
            $crate::serial_println_raw!("[{}] {}", $sub.name, format_args!($($arg)*));
        }
    };
}

// ── Permanent lifecycle counters ─────────────────────────────────────────────
//
// Unlike tracing, these are always on (a handful of atomic increments is
// unconditionally cheap) and never reset — cumulative since boot, read any
// time via `/proc/kdebug` instead of re-deriving them by grepping a log.

static FORKS_TOTAL:            AtomicU64 = AtomicU64::new(0);
static EXECS_TOTAL:            AtomicU64 = AtomicU64::new(0);
static REAPS_TOTAL:            AtomicU64 = AtomicU64::new(0);
static COW_FAULTS_RESOLVED:    AtomicU64 = AtomicU64::new(0);
static COW_FAULTS_FAILED:      AtomicU64 = AtomicU64::new(0);

pub fn inc_forks()         { FORKS_TOTAL.fetch_add(1, Ordering::Relaxed); }
pub fn inc_execs()         { EXECS_TOTAL.fetch_add(1, Ordering::Relaxed); }
pub fn inc_reaps()         { REAPS_TOTAL.fetch_add(1, Ordering::Relaxed); }
pub fn inc_cow_resolved()  { COW_FAULTS_RESOLVED.fetch_add(1, Ordering::Relaxed); }
pub fn inc_cow_failed()    { COW_FAULTS_FAILED.fetch_add(1, Ordering::Relaxed); }

/// Render the current state for `/proc/kdebug`: enabled subsystems (by
/// name, not just the raw mask) plus every counter above.
pub fn render_report() -> alloc::string::String {
    use alloc::format;
    use alloc::string::String;

    let mask = get_mask();
    let mut enabled = String::new();
    for sub in ALL_SUBSYSTEMS {
        if mask & sub.bit != 0 {
            if !enabled.is_empty() { enabled.push(','); }
            enabled.push_str(sub.name);
        }
    }
    if enabled.is_empty() {
        enabled.push_str("(none)");
    }

    format!(
        "trace_mask: {:#x} ({})\n\
         forks_total: {}\n\
         execs_total: {}\n\
         reaps_total: {}\n\
         cow_faults_resolved: {}\n\
         cow_faults_failed: {}\n",
        mask, enabled,
        FORKS_TOTAL.load(Ordering::Relaxed),
        EXECS_TOTAL.load(Ordering::Relaxed),
        REAPS_TOTAL.load(Ordering::Relaxed),
        COW_FAULTS_RESOLVED.load(Ordering::Relaxed),
        COW_FAULTS_FAILED.load(Ordering::Relaxed),
    )
}

/// Resolve a subsystem name (e.g. "mm") to its bit, for the `kdebug_ctl`
/// syscall's by-name form. Case-sensitive, matches `Subsystem::name`.
pub fn subsystem_bit_by_name(name: &str) -> Option<u32> {
    ALL_SUBSYSTEMS.iter().find(|s| s.name == name).map(|s| s.bit)
}
