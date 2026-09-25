//! The spin lock every lock in this crate uses: `spin::Mutex` with a relax
//! strategy the kernel can hook.
//!
//! The kernel needs each CPU that spins for a lock with interrupts off to
//! keep answering TLB-shootdown requests (stage 6 of
//! `docs/smp/smp-plan.md`, `kernel/src/sync.rs`), and ramfs holds its
//! locks across copies into user buffers — a page fault there can send a
//! shootdown to the very CPU spinning on the same lock. This crate can't
//! name the kernel, so the kernel registers the function to call instead
//! ([`set_relax_hook`]); unset (host tests), spinning is a plain
//! `spin_loop`.

use core::sync::atomic::{AtomicPtr, Ordering};

pub type Mutex<T> = spin::mutex::Mutex<T, HookRelax>;
pub use spin::MutexGuard;

static RELAX_HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

/// Call `hook` on every failed attempt to take a lock of this crate. It
/// runs in whatever context the spinner is in (IF=0 included) and must not
/// take a lock.
pub fn set_relax_hook(hook: fn()) {
    RELAX_HOOK.store(hook as *mut (), Ordering::Release);
}

pub struct HookRelax;

impl spin::RelaxStrategy for HookRelax {
    #[inline]
    fn relax() {
        let p = RELAX_HOOK.load(Ordering::Acquire);
        if !p.is_null() {
            // SAFETY: only ever stored from a `fn()` in `set_relax_hook`.
            let hook: fn() = unsafe { core::mem::transmute(p) };
            hook();
        }
        core::hint::spin_loop();
    }
}
