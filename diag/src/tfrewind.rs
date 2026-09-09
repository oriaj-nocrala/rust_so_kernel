use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// What [`TfRewindDiag::record`] just recorded, handed back to the caller
/// so the *kernel adapter* can print it — this crate cannot name
/// `crate::serial_println_raw!`. Same "events come back as data, the
/// adapter prints" discipline the `mm` extraction established for
/// `mm::buddy::PhantomEvent`/`mm::slab::AllocEvent`.
///
/// Before the extraction, `record()` printed the line itself,
/// unconditionally and immediately. `kernel/src/debug.rs::tf_record`
/// reproduces that exact line from this struct, so the serial output is
/// unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RewindEvent {
    pub pid:     u64,
    pub site:    &'static str,
    pub old_seq: u64,
    pub new_seq: u64,
}

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
/// context switch, and the adapter prints only when a rewind actually
/// happens, which should be never.
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

    /// Record one detected rewind and return it, for the caller to print.
    ///
    /// The pre-extraction version printed here directly ("loud and
    /// immediate, not just recorded for later — a rewind is rare enough
    /// (should be zero, ever) that the cost of printing on every occurrence
    /// is irrelevant, unlike the per-acquire prints this investigation
    /// already learned to avoid"). That reasoning still holds; only the
    /// *place* the print happens moved, to `kernel/src/debug.rs::tf_record`.
    pub fn record(&self, pid: u64, site: &'static str, old_seq: u64, new_seq: u64) -> RewindEvent {
        self.last_pid.store(pid, Ordering::Relaxed);
        self.last_site_ptr.store(site.as_ptr() as usize, Ordering::Relaxed);
        self.last_site_len.store(site.len() as u32, Ordering::Relaxed);
        self.last_old_seq.store(old_seq, Ordering::Relaxed);
        self.last_new_seq.store(new_seq, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        RewindEvent { pid, site, old_seq, new_seq }
    }

    /// Rewinds detected since boot. Should be 0, ever. Allocation-free.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// PID of the most recent rewind, or `u64::MAX` if none. Allocation-free.
    pub fn last_pid(&self) -> u64 {
        self.last_pid.load(Ordering::Relaxed)
    }

    /// Call site of the most recent rewind, or `"<none yet>"`.
    /// Allocation-free.
    ///
    /// # Safety reasoning
    /// `site` is a `&'static str` literal in rodata; a pointer captured
    /// from it stays valid to reconstruct at any later point.
    pub fn last_site(&self) -> &'static str {
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

    /// Sequence number the most recent rewind came *from*. Allocation-free.
    pub fn last_old_seq(&self) -> u64 {
        self.last_old_seq.load(Ordering::Relaxed)
    }

    /// Sequence number the most recent rewind went *to*. Allocation-free.
    pub fn last_new_seq(&self) -> u64 {
        self.last_new_seq.load(Ordering::Relaxed)
    }

    pub fn render(&self) -> alloc::string::String {
        use alloc::format;
        format!(
            "tf_rewind: count={} last_pid={} last_site={} last_seq={}->{}\n",
            self.count(), self.last_pid(), self.last_site(),
            self.last_old_seq(), self.last_new_seq(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Family C (instrument audit).

    #[test]
    fn fresh_tf_rewind_diag_renders_the_never_fired_line() {
        let d = TfRewindDiag::new();
        assert_eq!(d.count(), 0);
        assert_eq!(d.last_site(), "<none yet>");
        assert_eq!(
            d.render(),
            "tf_rewind: count=0 last_pid=18446744073709551615 last_site=<none yet> last_seq=0->0\n"
        );
    }

    #[test]
    fn record_returns_the_event_and_increments_count() {
        let d = TfRewindDiag::new();
        let ev = d.record(4, "switch_to_next", 11, 9);
        assert_eq!(
            ev,
            RewindEvent { pid: 4, site: "switch_to_next", old_seq: 11, new_seq: 9 },
            "the returned event is what the kernel adapter prints — every field must \
             be exactly what was passed in, or the serial line lies about the rewind"
        );
        assert_eq!(d.count(), 1);
        assert_eq!(d.last_pid(), 4);
        assert_eq!(d.last_site(), "switch_to_next");
        assert_eq!(d.last_old_seq(), 11);
        assert_eq!(d.last_new_seq(), 9);
    }

    #[test]
    fn repeated_records_count_up_and_keep_only_the_latest_details() {
        let d = TfRewindDiag::new();
        d.record(1, "block_current", 2, 1);
        d.record(2, "start_first", 5, 4);
        assert_eq!(d.count(), 2);
        assert_eq!(d.last_pid(), 2);
        assert_eq!(d.last_site(), "start_first");
        assert_eq!(
            d.render(),
            "tf_rewind: count=2 last_pid=2 last_site=start_first last_seq=5->4\n"
        );
    }

    /// Same `len < 64` guard as `DirLockDiag::last_op` — pinned as current
    /// behavior, not endorsed. Every real `site` is a short literal
    /// (`"switch_to_next"`, `"sys_exec"`, ...), so this is never hit in
    /// practice.
    #[test]
    fn site_len_guard_63_shows_64_falls_back() {
        let d = TfRewindDiag::new();
        let s63: &'static str = alloc::boxed::Box::leak("s".repeat(63).into_boxed_str());
        let s64: &'static str = alloc::boxed::Box::leak("s".repeat(64).into_boxed_str());

        d.record(1, s63, 0, 0);
        assert_eq!(d.last_site(), s63);

        d.record(1, s64, 0, 0);
        assert_eq!(d.last_site(), "<none yet>");
    }
}
