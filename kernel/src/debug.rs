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

/// Extends the `LockDiag` idea above to name *who* holds a lock, not just
/// *where* it was acquired — built for `vfs::ramfs::RamDirNode::entries`,
/// found stuck taken during the 2026-08-05 hang hunt (RIP fixed at
/// `_mm_pause+2` inside `SpinMutex::lock` inlined into `RamDirNode::mkdir`
/// — a real spin on a lock nobody would ever release, not slowness). The
/// root cause turned out to be a trapframe copy corrupted by DF=1, which
/// resumed a process onto a stale frame and abandoned the syscall holding
/// this guard (see docs/hang-hunt-bug2-findings.md; fixed by `tss.rs`'s
/// FMASK DF bit plus the `cld` in both entry stubs). `file:line` alone —
/// what `LockDiag` tracks — can't tell two `mkdir()` calls from different
/// PIDs apart, since they all lock from the exact same inlined site; this
/// tracks PID + operation name instead.
///
/// Kept as a permanent, passive counter: two relaxed atomics per lock/
/// unlock, no prints, no panics, rendered by `/proc/kdebug` and the panic
/// snapshot. An `outstanding` that never returns to 0 while the system is
/// idle is the signature of exactly the abandonment above.
///
/// NOTE: unlike `LockDiag`, `outstanding` isn't strictly a 0-or-1 leak
/// signal — every `vfs::ramfs::RamDirNode` (one per ramfs directory) owns its own
/// `Mutex`, and they all report into this one shared counter, so
/// well-formed nesting (e.g. `rmdir` calling `readdir` on a *child* node
/// while still holding the parent's lock) can transiently show
/// `outstanding > 1` with nothing wrong.
pub struct DirLockDiag {
    acquires:     AtomicU64,
    releases:     AtomicU64,
    last_pid:     AtomicU64,
    last_op_ptr:  AtomicUsize,
    last_op_len:  AtomicU32,
}

impl DirLockDiag {
    pub const fn new() -> Self {
        Self {
            acquires: AtomicU64::new(0),
            releases: AtomicU64::new(0),
            last_pid: AtomicU64::new(u64::MAX),
            last_op_ptr: AtomicUsize::new(0),
            last_op_len: AtomicU32::new(0),
        }
    }

    /// Call once the real mutex is actually held — never before it, or a
    /// caller still spinning for the lock gets recorded as its holder (one
    /// of the ten defective instruments this hunt produced). `op` must be a
    /// `'static` string constant (`"mkdir"`, `"symlink"`, ...).
    /// Allocation-free and print-free, just atomic stores.
    pub fn record_acquire(&self, pid: u64, op: &'static str) {
        self.last_pid.store(pid, Ordering::Relaxed);
        self.last_op_ptr.store(op.as_ptr() as usize, Ordering::Relaxed);
        self.last_op_len.store(op.len() as u32, Ordering::Relaxed);
        self.acquires.fetch_add(1, Ordering::Relaxed);
    }

    /// Call from the guard wrapper's `Drop` impl — i.e. only when the
    /// critical section actually finishes normally. If a process's
    /// continuation is hijacked mid-critical-section (what the DF bug did),
    /// this simply never fires for that acquire, which is exactly the
    /// signal we want: `outstanding` stays nonzero forever and
    /// `last_acquirer` still names the culprit.
    pub fn record_release(&self) {
        self.releases.fetch_add(1, Ordering::Relaxed);
    }

    fn last_op(&self) -> &'static str {
        let ptr = self.last_op_ptr.load(Ordering::Relaxed);
        let len = self.last_op_len.load(Ordering::Relaxed) as usize;
        if ptr != 0 && len > 0 && len < 64 {
            unsafe {
                let bytes = core::slice::from_raw_parts(ptr as *const u8, len);
                core::str::from_utf8(bytes).unwrap_or("<non-utf8>")
            }
        } else {
            "<none yet>"
        }
    }

    /// One `/proc/kdebug` line: acquires/releases/outstanding plus the last
    /// acquirer's PID and operation — exactly what's needed to answer "who
    /// left this held, doing what".
    pub fn render(&self, name: &str) -> alloc::string::String {
        use alloc::format;
        let acq = self.acquires.load(Ordering::Relaxed);
        let rel = self.releases.load(Ordering::Relaxed);
        format!(
            "{name}: acquires={} releases={} outstanding={} last_acquirer=pid={} op={}\n",
            acq, rel, acq.saturating_sub(rel),
            self.last_pid.load(Ordering::Relaxed), self.last_op(),
        )
    }

    /// Allocation-free variant for the panic handler — see
    /// `print_panic_snapshot`.
    pub fn print_panic_line(&self, name: &str) {
        crate::serial_println_raw!(
            "  {}: acquires={} releases={} outstanding={} last_acquirer pid={} op={}",
            name,
            self.acquires.load(Ordering::Relaxed),
            self.releases.load(Ordering::Relaxed),
            self.acquires.load(Ordering::Relaxed).saturating_sub(self.releases.load(Ordering::Relaxed)),
            self.last_pid.load(Ordering::Relaxed),
            self.last_op(),
        );
    }
}

/// See `DirLockDiag`'s doc comment. `vfs::ramfs::RamDirNode::lock_entries`
/// (via the `KernelDirLockObserver` seam in `kernel/src/fs/ramfs.rs`) is
/// the only thing that acquires it.
pub static RAMFS_ENTRIES_LOCK: DirLockDiag = DirLockDiag::new();

/// Detects a resumed/saved `TrapFrame` sequence going backward, or a
/// double-save with no intervening resume — see each `Process`'s own
/// `tf_seq`/`tf_awaiting_resume`/`tf_last_resumed_seq` fields and
/// `process::scheduler::tf_note_save`/`tf_note_resume`, which call
/// `record()` here the instant either anomaly is detected. A nonzero count
/// is direct, mechanical confirmation that some process's execution got
/// "rewound" onto a stale copy of its own `TrapFrame` — the mechanism
/// behind the 2026-08-05 DF bug (a `rep movsb` copying backward wrote the
/// 160 bytes *before* the trapframe box instead of into it, leaving the box
/// holding an older frame). Permanent: three plain field updates per
/// context switch, and it prints only when a rewind actually happens, which
/// should be never.
pub struct TfRewindDiag {
    count:         AtomicU64,
    last_pid:      AtomicU64,
    last_site_ptr: AtomicUsize,
    last_site_len: AtomicU32,
    last_old_seq:  AtomicU64,
    last_new_seq:  AtomicU64,
}

impl TfRewindDiag {
    pub const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            last_pid: AtomicU64::new(u64::MAX),
            last_site_ptr: AtomicUsize::new(0),
            last_site_len: AtomicU32::new(0),
            last_old_seq: AtomicU64::new(0),
            last_new_seq: AtomicU64::new(0),
        }
    }

    pub fn record(&self, pid: u64, site: &'static str, old_seq: u64, new_seq: u64) {
        self.last_pid.store(pid, Ordering::Relaxed);
        self.last_site_ptr.store(site.as_ptr() as usize, Ordering::Relaxed);
        self.last_site_len.store(site.len() as u32, Ordering::Relaxed);
        self.last_old_seq.store(old_seq, Ordering::Relaxed);
        self.last_new_seq.store(new_seq, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        // Loud and immediate, not just recorded for later — a rewind is
        // rare enough (should be zero, ever) that the cost of printing on
        // every occurrence is irrelevant, unlike the per-acquire prints
        // this investigation already learned to avoid (see `RAMFS_ENTRIES_
        // LOCK`'s doc comment and the module-level "watch the cost" note).
        crate::serial_println_raw!(
            "[HANGHUNT-DIAG] TF-REWIND: pid={} site={} seq {} -> {} (stale/rewound trapframe)",
            pid, site, old_seq, new_seq,
        );
    }

    fn last_site(&self) -> &'static str {
        let ptr = self.last_site_ptr.load(Ordering::Relaxed);
        let len = self.last_site_len.load(Ordering::Relaxed) as usize;
        if ptr != 0 && len > 0 && len < 64 {
            unsafe {
                let bytes = core::slice::from_raw_parts(ptr as *const u8, len);
                core::str::from_utf8(bytes).unwrap_or("<non-utf8>")
            }
        } else {
            "<none yet>"
        }
    }

    pub fn render(&self) -> alloc::string::String {
        use alloc::format;
        format!(
            "tf_rewind: count={} last_pid={} last_site={} last_seq={}->{}\n",
            self.count.load(Ordering::Relaxed),
            self.last_pid.load(Ordering::Relaxed),
            self.last_site(),
            self.last_old_seq.load(Ordering::Relaxed),
            self.last_new_seq.load(Ordering::Relaxed),
        )
    }

    pub fn print_panic_line(&self) {
        crate::serial_println_raw!(
            "  tf_rewind: count={} last_pid={} last_site={} last_seq={}->{}",
            self.count.load(Ordering::Relaxed),
            self.last_pid.load(Ordering::Relaxed),
            self.last_site(),
            self.last_old_seq.load(Ordering::Relaxed),
            self.last_new_seq.load(Ordering::Relaxed),
        );
    }
}

pub static TF_REWIND: TfRewindDiag = TfRewindDiag::new();

/// Always-on check for `memory::cow.rs`'s stated invariant ("all accesses
/// must be under `cli` — single CPU, no atomics needed"). Every public
/// accessor (`inc_ref`/`dec_ref`/`set_ref`/`get_ref`) reports here on
/// entry; if any of them is ever reached with IF=1 (interrupts enabled),
/// the non-atomic `FRAME_REFCOUNTS[idx] = ...saturating_add/sub(1)` RMW
/// can be interrupted mid-read-modify-write by the timer ISR re-entering
/// the same table (e.g. via another process's page fault or a scheduler
/// tick that runs COW teardown), losing an update. Built to get direct
/// evidence for/against that mechanism instead of reasoning about every
/// call site by hand — see the `busybox_install_fork_flake` investigation.
/// `outstanding`-style leak tracking doesn't apply here (this isn't a
/// lock), so it's a simpler plain counter + last-call-site, read via
/// `/proc/kdebug`'s `cow_if_violations` line.
pub struct IfViolationDiag {
    count:         AtomicU64,
    last_file_ptr: AtomicUsize,
    last_file_len: AtomicU32,
    last_line:     AtomicU32,
}

impl IfViolationDiag {
    pub const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            last_file_ptr: AtomicUsize::new(0),
            last_file_len: AtomicU32::new(0),
            last_line: AtomicU32::new(0),
        }
    }

    /// Call from a `#[track_caller]` wrapper right after observing
    /// `interrupts::are_enabled() == true` in a context that must not
    /// allow that.
    pub fn record(&self, loc: &core::panic::Location) {
        self.last_file_ptr.store(loc.file().as_ptr() as usize, Ordering::Relaxed);
        self.last_file_len.store(loc.file().len() as u32, Ordering::Relaxed);
        self.last_line.store(loc.line(), Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    pub fn render(&self, name: &str) -> alloc::string::String {
        use alloc::format;
        let count = self.count.load(Ordering::Relaxed);
        let ptr = self.last_file_ptr.load(Ordering::Relaxed);
        let len = self.last_file_len.load(Ordering::Relaxed) as usize;
        let line = self.last_line.load(Ordering::Relaxed);
        // Safe: same reasoning as `LockDiag::render` — `loc.file()` always
        // points into 'static rodata.
        let file: &str = if ptr != 0 && len > 0 && len < 512 {
            unsafe {
                let bytes = core::slice::from_raw_parts(ptr as *const u8, len);
                core::str::from_utf8(bytes).unwrap_or("<non-utf8>")
            }
        } else {
            "<none yet>"
        };
        format!("{name}: count={} last_caller={}:{}\n", count, file, line)
    }
}

/// See `IfViolationDiag`'s doc comment — tracks `memory::cow.rs` accessor
/// calls reached with interrupts enabled, split one counter per accessor
/// (`set_ref` is a plain write into an exclusively-owned index, not
/// itself racy; `inc_ref`/`dec_ref` are the real non-atomic
/// read-modify-write lost-update hazard; `get_ref` is a plain read,
/// tracked for completeness) so a violation in one doesn't hide a
/// same-boot violation in another behind a single shared "last caller".
pub static COW_IF_VIOLATIONS_SET_REF: IfViolationDiag = IfViolationDiag::new();
pub static COW_IF_VIOLATIONS_INC_REF: IfViolationDiag = IfViolationDiag::new();
pub static COW_IF_VIOLATIONS_DEC_REF: IfViolationDiag = IfViolationDiag::new();
pub static COW_IF_VIOLATIONS_GET_REF: IfViolationDiag = IfViolationDiag::new();

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
/// Full context switches (`Scheduler::switch_to_next` landing on a
/// different process) since boot — added while verifying `process::fpu`'s
/// FPU/SSE save/restore actually exercises the switch path during
/// `fpu_test` (a per-switch `serial_println!` was tried first to check
/// this and made exec()/page-fault-heavy boot phases crawl, since it ran
/// on literally every timer preemption; a plain atomic counter is free by
/// comparison and, per this kernel's own convention of keeping bug-hunting
/// instrumentation around instead of deleting it, useful for the next
/// scheduler investigation too).
static SWITCHES_TOTAL: AtomicU64 = AtomicU64::new(0);
/// TEMPORARY (hang-hunt investigation, 2026-07-25, see the
/// `debug_hang_selfdeadlock` session): counts every time `SlabGlobalAlloc::
/// alloc`/`dealloc` found `SLAB_ALLOCATOR` already locked via a non-blocking
/// `try_lock()` probe, immediately before falling back to the real blocking
/// `.lock()`. This kernel is single-core, and neither `SLAB_ALLOCATOR` nor
/// the global allocator critical section ever does `cli` — so the *only*
/// way this lock can ever be found already held is if the holder is the
/// same CPU, interrupted mid-critical-section by the timer ISR, whose own
/// `Scheduler::switch_to_next` → `VecDeque::push_back` path can itself need
/// to allocate (growing a run queue). `spin::Mutex` isn't reentrant, so
/// that reentrant `.lock()` call spins forever — this counter fires at the
/// exact instant that fatal reentrant acquisition begins, before it starts
/// spinning, turning what previously needed a live gdbstub session to catch
/// into a plain, always-on, zero-cost (one extra `try_lock` per allocation)
/// counter. A nonzero value here after a hang is direct, deterministic
/// confirmation of this exact mechanism, independent of whatever backtrace
/// gdb happens to show once attached.
static SLAB_LOCK_CONTENDED: AtomicU64 = AtomicU64::new(0);

pub fn inc_forks()         { FORKS_TOTAL.fetch_add(1, Ordering::Relaxed); }
pub fn inc_execs()         { EXECS_TOTAL.fetch_add(1, Ordering::Relaxed); }
pub fn inc_reaps()         { REAPS_TOTAL.fetch_add(1, Ordering::Relaxed); }
pub fn inc_cow_resolved()  { COW_FAULTS_RESOLVED.fetch_add(1, Ordering::Relaxed); }
pub fn inc_cow_failed()    { COW_FAULTS_FAILED.fetch_add(1, Ordering::Relaxed); }
pub fn inc_switches()      { SWITCHES_TOTAL.fetch_add(1, Ordering::Relaxed); }
/// See `SLAB_LOCK_CONTENDED`'s doc comment. Deliberately allocation-free
/// (a single atomic increment) — called from inside the global allocator
/// itself, so anything that allocated here would recurse.
pub fn inc_slab_lock_contended() { SLAB_LOCK_CONTENDED.fetch_add(1, Ordering::Relaxed); }
pub fn slab_lock_contended_count() -> u64 { SLAB_LOCK_CONTENDED.load(Ordering::Relaxed) }
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
         switches_total: {}\n\
         slab_lock_contended: {}\n\
         {}{}{}{}",
        mask, enabled,
        FORKS_TOTAL.load(Ordering::Relaxed),
        EXECS_TOTAL.load(Ordering::Relaxed),
        REAPS_TOTAL.load(Ordering::Relaxed),
        COW_FAULTS_RESOLVED.load(Ordering::Relaxed),
        COW_FAULTS_FAILED.load(Ordering::Relaxed),
        ORPHAN_BLOCKS_RECLAIMED.load(Ordering::Relaxed),
        ORPHAN_INODES_RECLAIMED.load(Ordering::Relaxed),
        SWITCHES_TOTAL.load(Ordering::Relaxed),
        SLAB_LOCK_CONTENDED.load(Ordering::Relaxed),
        SCHEDULER_LOCK.render("scheduler"),
        RAMFS_ENTRIES_LOCK.render("ramfs_entries_lock"),
        TF_REWIND.render(),
        alloc::format!(
            "{}{}{}{}",
            COW_IF_VIOLATIONS_SET_REF.render("cow_if_violations_set_ref"),
            COW_IF_VIOLATIONS_INC_REF.render("cow_if_violations_inc_ref"),
            COW_IF_VIOLATIONS_DEC_REF.render("cow_if_violations_dec_ref"),
            COW_IF_VIOLATIONS_GET_REF.render("cow_if_violations_get_ref"),
        ),
    )
}

/// Allocation-free counter dump for the panic handler (`panic.rs`) —
/// see its call site for why this can't just call `render_report()`
/// (that one builds a `String` via `format!`, unsafe to do from a panic
/// whose root cause might be heap corruption). Every line here goes
/// straight through `serial_println_raw!`, which formats lazily with no
/// allocation.
pub fn print_panic_snapshot() {
    crate::serial_println_raw!("--- /proc/kdebug snapshot at panic ---");
    crate::serial_println_raw!("  forks_total: {}", FORKS_TOTAL.load(Ordering::Relaxed));
    crate::serial_println_raw!("  execs_total: {}", EXECS_TOTAL.load(Ordering::Relaxed));
    crate::serial_println_raw!("  reaps_total: {}", REAPS_TOTAL.load(Ordering::Relaxed));
    crate::serial_println_raw!("  cow_faults_resolved: {}", COW_FAULTS_RESOLVED.load(Ordering::Relaxed));
    crate::serial_println_raw!("  cow_faults_failed: {}", COW_FAULTS_FAILED.load(Ordering::Relaxed));
    crate::serial_println_raw!("  switches_total: {}", SWITCHES_TOTAL.load(Ordering::Relaxed));
    crate::serial_println_raw!("  slab_lock_contended: {}", SLAB_LOCK_CONTENDED.load(Ordering::Relaxed));
    let acq = SCHEDULER_LOCK.acquires.load(Ordering::Relaxed);
    let rel = SCHEDULER_LOCK.releases.load(Ordering::Relaxed);
    crate::serial_println_raw!("  scheduler_lock: acquires={} releases={} outstanding={}", acq, rel, acq.saturating_sub(rel));
    RAMFS_ENTRIES_LOCK.print_panic_line("ramfs_entries_lock");
    TF_REWIND.print_panic_line();
    crate::serial_println_raw!(
        "  cow_if_violations: set_ref count={} last_line={} | inc_ref count={} last_line={} | dec_ref count={} last_line={} | get_ref count={} last_line={}",
        COW_IF_VIOLATIONS_SET_REF.count.load(Ordering::Relaxed), COW_IF_VIOLATIONS_SET_REF.last_line.load(Ordering::Relaxed),
        COW_IF_VIOLATIONS_INC_REF.count.load(Ordering::Relaxed), COW_IF_VIOLATIONS_INC_REF.last_line.load(Ordering::Relaxed),
        COW_IF_VIOLATIONS_DEC_REF.count.load(Ordering::Relaxed), COW_IF_VIOLATIONS_DEC_REF.last_line.load(Ordering::Relaxed),
        COW_IF_VIOLATIONS_GET_REF.count.load(Ordering::Relaxed), COW_IF_VIOLATIONS_GET_REF.last_line.load(Ordering::Relaxed),
    );
}

/// Resolve a subsystem name (e.g. "mm") to its bit, for the `kdebug_ctl`
/// syscall's by-name form. Case-sensitive, matches `Subsystem::name`.
pub fn subsystem_bit_by_name(name: &str) -> Option<u32> {
    ALL_SUBSYSTEMS.iter().find(|s| s.name == name).map(|s| s.bit)
}
