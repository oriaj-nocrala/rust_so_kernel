use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Always-on diagnostics for one lock: acquire/release counts (a
/// persistent gap between them means a guard leaked — the lock will
/// never be released again) and the `file:line` of whoever acquired it
/// *most recently*. On a single core that's sufficient to name the
/// culprit outright: whoever is stuck holding a lock forever must be the
/// last one who successfully locked it (nothing else could have raced in
/// after). See `kernel/src/debug.rs`'s module doc comment for how this was
/// born (a real, hours-long single-core deadlock hunt).
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
    /// around the real lock call — see `local_scheduler()` in the kernel's
    /// `process::scheduler`.
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

    /// Total acquires recorded so far.
    pub fn acquires(&self) -> u64 {
        self.acquires.load(Ordering::Relaxed)
    }

    /// Total releases recorded so far.
    pub fn releases(&self) -> u64 {
        self.releases.load(Ordering::Relaxed)
    }

    /// `acquires - releases`, saturating at 0 — a persistent nonzero value
    /// (outside 0/1) means a guard leaked. Saturating, not subtracting,
    /// because a caller instrumented mid-critical-section (a release
    /// recorded without a matching acquire ever having been recorded, e.g.
    /// right after this diagnostic itself was added) must never panic on
    /// overflow just for reporting a number.
    pub fn outstanding(&self) -> u64 {
        self.acquires().saturating_sub(self.releases())
    }

    /// The `file:line` of the most recent `record_acquire` call, or
    /// `("<none yet>", 0)` before the first one. Allocation-free — safe to
    /// call from the panic handler.
    ///
    /// # Safety reasoning
    /// `loc.file()` (`core::panic::Location`) always points into the
    /// binary's rodata — a real `'static str` that's never freed — so a
    /// pointer captured from it stays valid to reconstruct and read back at
    /// any later point, from any context, including this one.
    pub fn last_file(&self) -> &'static str {
        let ptr = self.last_file_ptr.load(Ordering::Relaxed);
        let len = self.last_file_len.load(Ordering::Relaxed) as usize;
        if ptr != 0 && len > 0 && len < 512 {
            unsafe {
                let bytes = core::slice::from_raw_parts(ptr as *const u8, len);
                core::str::from_utf8(bytes).unwrap_or("<non-utf8>")
            }
        } else {
            "<none yet>"
        }
    }

    /// The line number of the most recent `record_acquire` call, or `0`
    /// before the first one.
    pub fn last_line(&self) -> u32 {
        self.last_line.load(Ordering::Relaxed)
    }

    /// One `/proc/kdebug` line: `{name}_lock: acquires=.. releases=..
    /// outstanding=.. last_acquirer=file:line`.
    pub fn render(&self, name: &str) -> alloc::string::String {
        use alloc::format;
        // Snapshot both counters ONCE and derive `outstanding` from that
        // snapshot, exactly as the pre-extraction code did. Calling
        // `self.outstanding()` here instead would re-load both atomics, so a
        // concurrent update (this is read from `/proc/kdebug` while the timer
        // ISR keeps acquiring locks) could render a line whose `outstanding`
        // doesn't match its own `acquires`/`releases` — an internally
        // inconsistent diagnostic line, which is precisely the class of
        // lying instrument this crate exists to stop shipping.
        let acq = self.acquires();
        let rel = self.releases();
        format!(
            "{name}_lock: acquires={} releases={} outstanding={} last_acquirer={}:{}\n",
            acq, rel, acq.saturating_sub(rel), self.last_file(), self.last_line(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Family C (instrument audit): these tests check that the instrument
    // reports what it says it reports — not the kernel behavior it was
    // built to observe.

    #[test]
    fn fresh_lock_diag_reports_zero_and_none_yet() {
        let d = LockDiag::new();
        assert_eq!(d.acquires(), 0);
        assert_eq!(d.releases(), 0);
        assert_eq!(d.outstanding(), 0);
        assert_eq!(d.last_file(), "<none yet>");
        assert_eq!(d.last_line(), 0);
        assert_eq!(
            d.render("scheduler"),
            "scheduler_lock: acquires=0 releases=0 outstanding=0 last_acquirer=<none yet>:0\n"
        );
    }

    #[test]
    fn outstanding_is_acquires_minus_releases() {
        let d = LockDiag::new();
        let loc = core::panic::Location::caller();
        d.record_acquire(loc);
        d.record_acquire(loc);
        d.record_acquire(loc);
        d.record_release();
        assert_eq!(d.acquires(), 3);
        assert_eq!(d.releases(), 1);
        assert_eq!(d.outstanding(), 2);
    }

    #[test]
    fn outstanding_saturates_at_zero_when_releases_exceed_acquires() {
        // This can legitimately happen right after this diagnostic itself
        // is added to a lock that was already held — a release fires with
        // no matching recorded acquire. Must never underflow/panic.
        let d = LockDiag::new();
        d.record_release();
        d.record_release();
        assert_eq!(d.acquires(), 0);
        assert_eq!(d.releases(), 2);
        assert_eq!(d.outstanding(), 0);
    }

    #[test]
    fn record_acquire_captures_file_and_line() {
        let d = LockDiag::new();
        let loc = core::panic::Location::caller();
        d.record_acquire(loc);
        assert_eq!(d.last_file(), loc.file());
        assert_eq!(d.last_line(), loc.line());
    }

    #[test]
    fn render_full_line_after_activity() {
        let d = LockDiag::new();
        let loc = core::panic::Location::caller();
        d.record_acquire(loc);
        d.record_acquire(loc);
        d.record_release();
        let rendered = d.render("scheduler");
        assert_eq!(
            rendered,
            alloc::format!(
                "scheduler_lock: acquires=2 releases=1 outstanding=1 last_acquirer={}:{}\n",
                loc.file(), loc.line(),
            )
        );
    }

    /// Documents the `len < 512` range guard as it stands today — does NOT
    /// approve of it as correct behavior. A `file` string of exactly 512
    /// bytes falls back to "<none yet>" instead of being shown, purely
    /// because the guard is a strict `<`, not `<=`.
    #[test]
    fn file_len_guard_511_shows_512_falls_back() {
        let d = LockDiag::new();
        let s511: alloc::string::String = "a".repeat(511);
        let s512: alloc::string::String = "a".repeat(512);

        d.last_file_ptr.store(s511.as_ptr() as usize, Ordering::Relaxed);
        d.last_file_len.store(511, Ordering::Relaxed);
        assert_eq!(d.last_file(), s511.as_str());

        d.last_file_ptr.store(s512.as_ptr() as usize, Ordering::Relaxed);
        d.last_file_len.store(512, Ordering::Relaxed);
        assert_eq!(d.last_file(), "<none yet>");
    }
}
