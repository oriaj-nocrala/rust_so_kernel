use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

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

    /// Violations recorded since boot. Should be 0. Allocation-free — safe
    /// from the panic handler.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// File of the most recent violation, or `"<none yet>"`.
    /// Allocation-free.
    ///
    /// # Safety reasoning
    /// Same as `LockDiag::last_file` — `loc.file()` always points into the
    /// binary's rodata, a real `'static str` that's never freed.
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

    /// Line of the most recent violation, or `0`. Allocation-free.
    pub fn last_line(&self) -> u32 {
        self.last_line.load(Ordering::Relaxed)
    }

    pub fn render(&self, name: &str) -> alloc::string::String {
        use alloc::format;
        format!("{name}: count={} last_caller={}:{}\n", self.count(), self.last_file(), self.last_line())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Family C (instrument audit).

    #[test]
    fn fresh_if_violation_diag_renders_the_clean_line() {
        let d = IfViolationDiag::new();
        assert_eq!(d.count(), 0);
        assert_eq!(d.last_file(), "<none yet>");
        assert_eq!(d.last_line(), 0);
        assert_eq!(
            d.render("cow_if_violations_inc_ref"),
            "cow_if_violations_inc_ref: count=0 last_caller=<none yet>:0\n"
        );
    }

    #[test]
    fn record_captures_caller_file_and_line_and_counts_up() {
        let d = IfViolationDiag::new();
        let loc = core::panic::Location::caller();
        d.record(loc);
        d.record(loc);
        assert_eq!(d.count(), 2);
        assert_eq!(d.last_file(), loc.file());
        assert_eq!(d.last_line(), loc.line());
        assert_eq!(
            d.render("cow_if_violations_dec_ref"),
            alloc::format!(
                "cow_if_violations_dec_ref: count=2 last_caller={}:{}\n",
                loc.file(), loc.line(),
            )
        );
    }

    /// The four `COW_IF_VIOLATIONS_*` statics are deliberately separate
    /// instruments, one per accessor, "so a violation in one doesn't hide a
    /// same-boot violation in another behind a single shared last caller".
    /// Pin that they really are independent — a shared counter would make
    /// the split pointless and the `/proc/kdebug` line misleading.
    #[test]
    fn separate_instances_do_not_share_state() {
        let set_ref = IfViolationDiag::new();
        let inc_ref = IfViolationDiag::new();
        let loc = core::panic::Location::caller();
        set_ref.record(loc);
        assert_eq!(set_ref.count(), 1);
        assert_eq!(inc_ref.count(), 0);
        assert_eq!(inc_ref.last_file(), "<none yet>");
    }
}
