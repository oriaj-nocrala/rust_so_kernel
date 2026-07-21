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
//
// ADDING LOCK DIAGNOSTICS FOR A NEW LOCK
// ───────────────────────────────────────
//   See `LockDiag` below — grew out of a real, hours-long hunt for a
//   single-core deadlock (SCHEDULER held, interrupts re-enabled one
//   statement too early, timer ISR spins on `local_scheduler()` forever)
//   that could only be pinned down by manually reading raw memory through
//   the QEMU monitor (cross-referencing `nm` symbol addresses, decoding
//   an ASCII file path byte-by-byte by hand). `ktrace!` couldn't have
//   caught this even turned on ahead of time: it's print-based, and a
//   print inside the acquire/release path risks perturbing the exact
//   timing the race depends on. `LockDiag` is the generalized, permanent
//   version of the ad hoc atomics that actually found it — add one
//   `static FOO_LOCK: LockDiag = LockDiag::new();` per lock worth
//   watching, call `.record_acquire(core::panic::Location::caller())` /
//   `.record_release()` around it (see `scheduler::local_scheduler()`'s
//   `TrackedSchedulerGuard` for the pattern), and add a `.render(...)`
//   line to `render_report()`. Next time: `cat /proc/kdebug` shows
//   `outstanding` (acquires − releases; anything but 0/1 means a guard
//   leaked) and exactly which call site is holding it, live, with no
//   monitor session required.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Always-on diagnostics for one lock: acquire/release counts (a
/// persistent gap between them means a guard leaked — the lock will
/// never be released again) and the `file:line` of whoever acquired it
/// *most recently*. On a single core that's sufficient to name the
/// culprit outright: whoever is stuck holding a lock forever must be the
/// last one who successfully locked it (nothing else could have raced in
/// after). See the module doc comment above for how this was born.
pub struct LockDiag {
    acquires:      AtomicU64,
    releases:      AtomicU64,
    last_file_ptr: AtomicUsize,
    last_file_len: AtomicU32,
    last_line:     AtomicU32,
}

impl LockDiag {
    pub const fn new() -> Self {
        Self {
            acquires: AtomicU64::new(0),
            releases: AtomicU64::new(0),
            last_file_ptr: AtomicUsize::new(0),
            last_file_len: AtomicU32::new(0),
            last_line: AtomicU32::new(0),
        }
    }

    /// Call immediately after acquiring the lock, passing
    /// `core::panic::Location::caller()` from a `#[track_caller]` wrapper
    /// around the real lock call — see `local_scheduler()`.
    pub fn record_acquire(&self, loc: &core::panic::Location) {
        self.last_file_ptr.store(loc.file().as_ptr() as usize, Ordering::Relaxed);
        self.last_file_len.store(loc.file().len() as u32, Ordering::Relaxed);
        self.last_line.store(loc.line(), Ordering::Relaxed);
        self.acquires.fetch_add(1, Ordering::Relaxed);
    }

    /// Call from the guard wrapper's `Drop` impl.
    pub fn record_release(&self) {
        self.releases.fetch_add(1, Ordering::Relaxed);
    }

    /// One `/proc/kdebug` line: `{name}_lock: acquires=.. releases=..
    /// outstanding=.. last_acquirer=file:line`.
    pub fn render(&self, name: &str) -> alloc::string::String {
        use alloc::format;
        let acq = self.acquires.load(Ordering::Relaxed);
        let rel = self.releases.load(Ordering::Relaxed);
        let ptr = self.last_file_ptr.load(Ordering::Relaxed);
        let len = self.last_file_len.load(Ordering::Relaxed) as usize;
        let line = self.last_line.load(Ordering::Relaxed);
        // Safe: `loc.file()` (core::panic::Location) always points into the
        // binary's rodata — a real 'static str that's never freed — so a
        // pointer captured from it stays valid to reconstruct and read back
        // at any later point, from any context, including this one.
        let file: &str = if ptr != 0 && len > 0 && len < 512 {
            unsafe {
                let bytes = core::slice::from_raw_parts(ptr as *const u8, len);
                core::str::from_utf8(bytes).unwrap_or("<non-utf8>")
            }
        } else {
            "<none yet>"
        };
        format!(
            "{name}_lock: acquires={} releases={} outstanding={} last_acquirer={}:{}\n",
            acq, rel, acq.saturating_sub(rel), file, line,
        )
    }
}

/// Diagnostics for the scheduler's per-CPU lock — see `scheduler::
/// local_scheduler()`, which is the only thing that acquires it.
pub static SCHEDULER_LOCK: LockDiag = LockDiag::new();

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
/// Blocks/inodes `fs::ext2::Ext2Fs::reclaim_orphans` freed at mount time —
/// bitmap-set but unreachable from the root directory, i.e. left behind
/// by an unclean shutdown mid `create`/`mkdir`/`write` (see that
/// function's doc comment). Should read `0` on any boot that followed a
/// clean shutdown; a nonzero value here is direct evidence a previous
/// session ended uncleanly, independent of whether FS tracing happened to
/// be on on this boot to catch the `ktrace!` line reporting the same
/// thing.
static ORPHAN_BLOCKS_RECLAIMED: AtomicU64 = AtomicU64::new(0);
static ORPHAN_INODES_RECLAIMED: AtomicU64 = AtomicU64::new(0);

pub fn inc_forks()         { FORKS_TOTAL.fetch_add(1, Ordering::Relaxed); }
pub fn inc_execs()         { EXECS_TOTAL.fetch_add(1, Ordering::Relaxed); }
pub fn inc_reaps()         { REAPS_TOTAL.fetch_add(1, Ordering::Relaxed); }
pub fn inc_cow_resolved()  { COW_FAULTS_RESOLVED.fetch_add(1, Ordering::Relaxed); }
pub fn inc_cow_failed()    { COW_FAULTS_FAILED.fetch_add(1, Ordering::Relaxed); }
pub fn add_orphans_reclaimed(blocks: u64, inodes: u64) {
    ORPHAN_BLOCKS_RECLAIMED.fetch_add(blocks, Ordering::Relaxed);
    ORPHAN_INODES_RECLAIMED.fetch_add(inodes, Ordering::Relaxed);
}

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
         cow_faults_failed: {}\n\
         orphan_blocks_reclaimed: {}\n\
         orphan_inodes_reclaimed: {}\n\
         {}",
        mask, enabled,
        FORKS_TOTAL.load(Ordering::Relaxed),
        EXECS_TOTAL.load(Ordering::Relaxed),
        REAPS_TOTAL.load(Ordering::Relaxed),
        COW_FAULTS_RESOLVED.load(Ordering::Relaxed),
        COW_FAULTS_FAILED.load(Ordering::Relaxed),
        ORPHAN_BLOCKS_RECLAIMED.load(Ordering::Relaxed),
        ORPHAN_INODES_RECLAIMED.load(Ordering::Relaxed),
        SCHEDULER_LOCK.render("scheduler"),
    )
}

/// Resolve a subsystem name (e.g. "mm") to its bit, for the `kdebug_ctl`
/// syscall's by-name form. Case-sensitive, matches `Subsystem::name`.
pub fn subsystem_bit_by_name(name: &str) -> Option<u32> {
    ALL_SUBSYSTEMS.iter().find(|s| s.name == name).map(|s| s.bit)
}
