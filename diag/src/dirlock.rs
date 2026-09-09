use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Extends the `LockDiag` idea to name *who* holds a lock, not just
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
    acquires:    AtomicU64,
    releases:    AtomicU64,
    last_pid:    AtomicU64,
    last_op_ptr: AtomicUsize,
    last_op_len: AtomicU32,
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
    ///
    /// That "never before the lock" half of the invariant is enforced on
    /// the caller's side, not here — see
    /// `vfs::ramfs::RamDirNode::lock_entries` and the contention probe
    /// `record_acquire_never_fires_while_another_thread_holds_the_same_entries_lock`
    /// in `vfs/src/ramfs.rs`, which is what actually makes it observable.
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

    /// Total acquires recorded so far. Allocation-free — safe from the
    /// panic handler.
    pub fn acquires(&self) -> u64 {
        self.acquires.load(Ordering::Relaxed)
    }

    /// Total releases recorded so far. Allocation-free.
    pub fn releases(&self) -> u64 {
        self.releases.load(Ordering::Relaxed)
    }

    /// `acquires - releases`, saturating at 0. See the type's doc comment
    /// for why values above 1 are not automatically a leak here.
    pub fn outstanding(&self) -> u64 {
        self.acquires().saturating_sub(self.releases())
    }

    /// PID of the most recent `record_acquire` caller, or `u64::MAX` before
    /// the first one. Allocation-free.
    pub fn last_pid(&self) -> u64 {
        self.last_pid.load(Ordering::Relaxed)
    }

    /// Operation name of the most recent `record_acquire` call, or
    /// `"<none yet>"` before the first one. Allocation-free.
    ///
    /// # Safety reasoning
    /// `op` is a `&'static str` (a string constant in the binary's rodata),
    /// so a pointer captured from it stays valid to reconstruct at any
    /// later point, from any context.
    pub fn last_op(&self) -> &'static str {
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
        // One snapshot of both counters, `outstanding` derived from it —
        // see `LockDiag::render`'s comment for why re-loading would be
        // wrong.
        let acq = self.acquires();
        let rel = self.releases();
        format!(
            "{name}: acquires={} releases={} outstanding={} last_acquirer=pid={} op={}\n",
            acq, rel, acq.saturating_sub(rel), self.last_pid(), self.last_op(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Family C (instrument audit): these check that the instrument reports
    // what it claims to report — not the kernel behavior it observes.

    #[test]
    fn fresh_dir_lock_diag_reports_zero_none_yet_and_pid_u64_max() {
        let d = DirLockDiag::new();
        assert_eq!(d.acquires(), 0);
        assert_eq!(d.releases(), 0);
        assert_eq!(d.outstanding(), 0);
        assert_eq!(d.last_op(), "<none yet>");
        // Deliberately `u64::MAX`, not 0: pid 0 is a real pid in this
        // kernel (the idle process), so a sentinel that could be mistaken
        // for a real holder would be a lying instrument.
        assert_eq!(d.last_pid(), u64::MAX);
        assert_eq!(
            d.render("ramfs_entries_lock"),
            concat!(
                "ramfs_entries_lock: acquires=0 releases=0 outstanding=0 ",
                "last_acquirer=pid=18446744073709551615 op=<none yet>\n",
            )
        );
    }

    #[test]
    fn record_acquire_captures_pid_and_op() {
        let d = DirLockDiag::new();
        d.record_acquire(7, "mkdir");
        assert_eq!(d.last_pid(), 7);
        assert_eq!(d.last_op(), "mkdir");
        assert_eq!(d.acquires(), 1);
        assert_eq!(d.outstanding(), 1);
    }

    #[test]
    fn outstanding_saturates_at_zero_when_releases_exceed_acquires() {
        let d = DirLockDiag::new();
        d.record_release();
        d.record_release();
        assert_eq!(d.outstanding(), 0);
    }

    #[test]
    fn render_full_line_after_activity() {
        let d = DirLockDiag::new();
        d.record_acquire(3, "symlink");
        d.record_acquire(4, "unlink");
        d.record_release();
        assert_eq!(
            d.render("ramfs_entries_lock"),
            "ramfs_entries_lock: acquires=2 releases=1 outstanding=1 last_acquirer=pid=4 op=unlink\n"
        );
    }

    /// Documents the `len < 64` range guard on `last_op()` **as it stands
    /// today** — this test pins the current behavior, it does not endorse
    /// it. An `op` of exactly 64 bytes silently falls back to
    /// `"<none yet>"` instead of being shown, purely because the guard is a
    /// strict `<`, not `<=`. Every real `op` in the kernel is a short
    /// literal (`"mkdir"`, `"readdir"`, ...), so the limit is never hit in
    /// practice; it is pinned here so that if someone widens or narrows it,
    /// they do so deliberately and see the consequence, rather than
    /// discovering it during the next hang hunt while reading a
    /// `/proc/kdebug` line that says `op=<none yet>` for a lock that
    /// definitely has a holder.
    #[test]
    fn op_len_guard_63_shows_64_falls_back() {
        let d = DirLockDiag::new();

        // `Box::leak` to get the `&'static str` the API demands; the exact
        // lengths are what's under test.
        let s63: &'static str = alloc::boxed::Box::leak("a".repeat(63).into_boxed_str());
        let s64: &'static str = alloc::boxed::Box::leak("a".repeat(64).into_boxed_str());

        d.record_acquire(1, s63);
        assert_eq!(d.last_op(), s63, "63 bytes must be shown");

        d.record_acquire(1, s64);
        assert_eq!(
            d.last_op(),
            "<none yet>",
            "64 bytes hits the strict `len < 64` guard and falls back — \
             pinned as current behavior, not endorsed"
        );
    }

    /// An empty `op` also falls back (`len > 0` guard), which matters
    /// because `""` is a legal `&'static str` a careless call site could
    /// pass.
    #[test]
    fn empty_op_falls_back_to_none_yet() {
        let d = DirLockDiag::new();
        d.record_acquire(9, "");
        assert_eq!(d.last_op(), "<none yet>");
        // ...but the acquire itself still counted, and the pid is real.
        assert_eq!(d.acquires(), 1);
        assert_eq!(d.last_pid(), 9);
    }
}
