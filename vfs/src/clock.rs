//! Wall-clock time for filesystem timestamps.
//!
//! This crate can't read the kernel's clock, so the kernel registers a
//! function returning Unix seconds ([`set_clock`], the same shape as
//! [`lock::set_relax_hook`](crate::lock::set_relax_hook)). Unset — host
//! tests that don't care — every timestamp reads 0, the epoch.

use core::sync::atomic::{AtomicPtr, Ordering};

static CLOCK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Use `now` for every timestamp this crate stamps from here on. It must
/// not take a lock of this crate (it runs with directory locks held).
pub fn set_clock(now: fn() -> u64) {
    CLOCK.store(now as *mut (), Ordering::Release);
}

/// Unix seconds according to the registered clock, 0 without one.
pub fn now() -> u64 {
    let p = CLOCK.load(Ordering::Acquire);
    if p.is_null() {
        return 0;
    }
    // SAFETY: only ever stored from a `fn() -> u64` in `set_clock`.
    let f: fn() -> u64 = unsafe { core::mem::transmute(p) };
    f()
}
