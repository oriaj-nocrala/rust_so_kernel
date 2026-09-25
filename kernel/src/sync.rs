// kernel/src/sync.rs
//
// The kernel's spin lock: `spin::Mutex` with a relax strategy that answers
// TLB shootdowns while it waits (stage 6 of `docs/smp/smp-plan.md`).
//
// A shootdown sender spins with IF=0 until every target acknowledges, and a
// target acknowledges from the IPI handler — which a CPU spinning with IF=0
// never runs. So a CPU that waits for a lock with IF=0 while the holder
// sends it a shootdown waits forever, and so does the holder: the scheduler
// lock is the obvious one (taken with IF=0 everywhere, and a holder can
// free a kernel stack, whose guard page is a kernel PTE change), but any
// lock taken with IF=0 on one side and held across a PTE change on the
// other is the same deadlock. `diag::IrqMutex` already answers in its spin
// (`KernelIrq::relax`); this does the same for every plain lock, so the
// rule is structural rather than a per-lock audit that the next lock added
// would have to repeat.
//
// **Use `crate::sync::Mutex` (or `IrqLock` below, or `diag::IrqMutex`),
// never `spin::Mutex`, in the kernel.** The cost is one load of `PENDING`
// per failed attempt, only while contended. `vfs`'s locks (ramfs,
// `MountTable`) get the same through `vfs::lock::set_relax_hook`, set in
// `init::boot`; `diag::IrqMutex` through `KernelIrq::relax`.

/// `spin::Mutex` whose contended spin services pending shootdowns.
pub type Mutex<T> = spin::mutex::Mutex<T, ServiceTlb>;

/// Relax strategy for [`Mutex`]. `service_pending` is idempotent and takes
/// no lock, so calling it with IF=1 too (when the IPI would be delivered
/// anyway) is harmless and saves a flags read per spin.
pub struct ServiceTlb;

impl spin::RelaxStrategy for ServiceTlb {
    #[inline]
    fn relax() {
        crate::memory::tlb::service_pending();
        core::hint::spin_loop();
    }
}

/// A lock also taken from interrupt context: `lock()` disables interrupts
/// *before* spinning and the guard restores the previous state on drop —
/// Linux's `spin_lock_irqsave`.
///
/// A plain [`Mutex`] that an ISR takes is a deadlock on one CPU the moment
/// a syscall holds it with IF=1 and that ISR lands on the same CPU (the
/// ISR spins on a holder that cannot run until the ISR returns). Syscalls
/// enter with IF=0 (`IA32_FMASK`) but most turn it back on at their first
/// guard, so "the other side is always IF=0" was a convention each call
/// site had to keep — and `TERMIOS` (IRQ1's `tty::feed_input` vs. the
/// `TCSETS` ioctl) and `EPOLL_INSTANCES` (IRQ1's stdin wake vs. an epoll
/// fd's `close`) did not keep it. `diag::IrqMutex` solves the same problem
/// with a closure-only API; this keeps a guard, for locks whose call sites
/// hold the guard across early returns.
///
/// **Drop guards in reverse order of acquisition** (Rust's scoping does
/// this by itself; an explicit `drop` of an outer guard while an inner one
/// is alive would re-enable interrupts with the inner lock held). And, as
/// for any guard, never hold one across a diverging `jump_to_user`.
pub struct IrqLock<T> {
    inner: Mutex<T>,
}

pub struct IrqLockGuard<'a, T> {
    guard: core::mem::ManuallyDrop<spin::MutexGuard<'a, T>>,
    restore_if: bool,
}

impl<T> IrqLock<T> {
    pub const fn new(value: T) -> Self {
        Self { inner: Mutex::new(value) }
    }

    pub fn lock(&self) -> IrqLockGuard<'_, T> {
        let restore_if = x86_64::instructions::interrupts::are_enabled();
        x86_64::instructions::interrupts::disable();
        IrqLockGuard { guard: core::mem::ManuallyDrop::new(self.inner.lock()), restore_if }
    }

    pub fn try_lock(&self) -> Option<IrqLockGuard<'_, T>> {
        let restore_if = x86_64::instructions::interrupts::are_enabled();
        x86_64::instructions::interrupts::disable();
        match self.inner.try_lock() {
            Some(g) => Some(IrqLockGuard { guard: core::mem::ManuallyDrop::new(g), restore_if }),
            None => {
                if restore_if {
                    x86_64::instructions::interrupts::enable();
                }
                None
            }
        }
    }
}

impl<T> core::ops::Deref for IrqLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> core::ops::DerefMut for IrqLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for IrqLockGuard<'_, T> {
    fn drop(&mut self) {
        // Release first, then (maybe) re-enable: the other order opens the
        // exact window this type exists to close.
        unsafe { core::mem::ManuallyDrop::drop(&mut self.guard) };
        if self.restore_if {
            x86_64::instructions::interrupts::enable();
        }
    }
}
