//! `IrqMutex` — a `spin::Mutex` whose only access path bakes in "disable
//! interrupts, then lock" instead of leaving that ordering to be re-derived
//! by hand at every call site.
//!
//! # The bug this exists to make structurally impossible
//!
//! This kernel runs on a single core. `BUDDY` and `SLAB_ALLOCATOR`
//! (`kernel/src/allocator/mod.rs`) are plain `spin::Mutex`es with no
//! interrupt discipline of their own. Ordinary, interruptible kernel code
//! allocates constantly — e.g. `vfs::resolve_inner` allocates a `Vec<&str>`
//! just to split a path. If the timer fires while that code holds one of
//! these locks, the timer ISR calls `Scheduler::switch_to_next`, which can
//! itself need to allocate (growing a run queue's `VecDeque`) — reentering
//! the SAME non-reentrant `spin::Mutex` on the SAME core. Since there is
//! only one core, that reentrant `.lock()` call can only ever be from the
//! code it just interrupted: it spins forever, 100% CPU, no progress. This
//! was a real ~1-in-10 debug-build boot hang that went unexplained for
//! months (see CLAUDE.md's "Key Design Invariants" and the "Interrupt
//! safety" comment in `kernel/src/allocator/mod.rs`). The fix (`ab58dba`)
//! was wrapping every critical section in `without_interrupts` instead of a
//! bare `cli`/`sti` pair — `without_interrupts` restores the *previous*
//! interrupt-enable state, so it composes correctly with a caller that
//! already has interrupts off (the timer ISR itself), where a hand-rolled
//! `cli; ...; sti` would incorrectly force interrupts back on.
//!
//! Today that discipline is held up by nothing but a comment and the
//! programmer's own care — every acquisition site has to remember to wrap
//! itself in `without_interrupts`, and nothing stops a new call site from
//! getting the order backwards (lock first, disable after) or leaving the
//! wrapping off entirely. `IrqMutex` makes the ordering the type's own job
//! instead: there is exactly one way to reach the protected value
//! (`with`/`try_with`), and that path always disables interrupts before it
//! ever touches the real lock.
//!
//! # Relationship to `tracked::TrackedMutex`
//!
//! This type is the interrupt-discipline complement to
//! [`crate::tracked::TrackedMutex`], not a replacement for it — the two
//! target different halves of the same "spin lock, safely" problem.
//! `TrackedMutex` fixes the *reporting order* around a lock (acquire
//! recorded strictly after the mutex is held, release recorded in the one
//! direction that can only over-report, never under-report a held lock).
//! `TrackedMutex`'s own module doc says explicitly that it "does not
//! enforce interrupt discipline" and that this is exactly the gap left for
//! an `x86_64`-specific type to fill. `IrqMutex` is that type: it enforces
//! *interrupt discipline* (disable before lock, restore after) and reports
//! to nobody.
//!
//! # What this does NOT guarantee
//!
//! - **No reporting.** Unlike `TrackedMutex`, there is no observer seam
//!   here at all — no acquire/release counters, no `/proc/kdebug` line.
//!   Composing the two (a `TrackedMutex` that is also interrupt-safe) is a
//!   real future need, but it is out of scope for this type.
//! - **Not reentrant.** `IrqMutex` prevents interrupts from ever
//!   reentering the *same* critical section from an ISR — that is the
//!   entire point — but it does nothing about a direct reentrant call from
//!   plain, non-interrupt code (calling `with` again from inside a `with`
//!   body on the same mutex still deadlocks, exactly like a bare
//!   `spin::Mutex` would). It also assumes a single core: on real
//!   multi-core hardware, disabling interrupts on one CPU says nothing
//!   about what another CPU is doing, so mutual exclusion there still rests
//!   entirely on the underlying `spin::Mutex`, same as it always did.

/// Interrupt-control seam. Associated functions only (no `self`) — the
/// real kernel-side implementation is a zero-sized type over the CPU's
/// interrupt flag, the same shape `hal::PortIo`/`hal::PhysMem` already use
/// for their own hardware seams.
pub trait IrqControl {
    /// Whether interrupts are currently enabled on this CPU.
    fn are_enabled() -> bool;

    /// Run `f` with interrupts disabled, restoring the PREVIOUS
    /// enable/disable state afterward — never unconditionally turning
    /// interrupts back on. That distinction is the entire reason this
    /// exists instead of a bare `cli`/`sti` pair: it must be safe to call
    /// from a context that already has interrupts disabled (the timer ISR
    /// itself), which a hand-rolled `cli; ...; sti` is not.
    fn without_interrupts<R>(f: impl FnOnce() -> R) -> R;
}

/// A `spin::Mutex` reachable only through a path that disables interrupts
/// first. See the module doc comment for the bug this closes.
pub struct IrqMutex<T, C: IrqControl> {
    inner: spin::Mutex<T>,
    _marker: core::marker::PhantomData<C>,
}

impl<T, C: IrqControl> IrqMutex<T, C> {
    /// `const` so this can live in a `static`, the same reason
    /// `TrackedMutex::new` and `BuddyAllocator::new` are `const`.
    pub const fn new(value: T) -> Self {
        Self { inner: spin::Mutex::new(value), _marker: core::marker::PhantomData }
    }

    /// The only way to reach the protected value. Disables interrupts via
    /// `C::without_interrupts` FIRST, and takes the real lock ONLY inside
    /// that closure — never the other way around. That order is the whole
    /// invariant this type exists to hold: inverting it (lock, then
    /// disable) reopens exactly the window the module doc describes, where
    /// a timer interrupt can fire while the lock is held and reenter this
    /// same critical section from the ISR.
    ///
    /// This type does not re-implement interrupt save/restore itself — it
    /// delegates entirely to `C::without_interrupts`, so the kernel-side
    /// implementation can forward literally to
    /// `x86_64::instructions::interrupts::without_interrupts` and get
    /// byte-for-byte the same runtime behavior production already has.
    ///
    /// There is deliberately no `lock()` method and no public RAII guard
    /// type here (contrast `TrackedMutex::lock`, which does return one) —
    /// the *absence* of any way to reach `T` except through this closure is
    /// itself the guarantee: nothing can hold the real lock without also
    /// holding interrupts disabled for as long as it does.
    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        C::without_interrupts(|| {
            let mut guard = self.inner.lock();
            f(&mut guard)
        })
    }

    /// Non-blocking form of [`Self::with`]. Same ordering rule applies:
    /// interrupts are disabled before the (non-blocking) lock attempt, and
    /// restored to whatever they were before this call regardless of
    /// whether the attempt succeeded — a failed attempt must leave the
    /// caller's interrupt state exactly as it found it.
    pub fn try_with<R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        C::without_interrupts(|| {
            let mut guard = self.inner.try_lock()?;
            Some(f(&mut guard))
        })
    }

    /// Whether the underlying mutex is currently held. Non-blocking, and
    /// deliberately not part of the `with`/`try_with` access path — this is
    /// for probes asserting things *about* the instrument (tests, and any
    /// future `/proc`-style introspection), not a way to peek at `T`.
    pub fn is_locked(&self) -> bool {
        self.inner.is_locked()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    // A fake `IrqControl` whose "interrupt flag" lives in a `thread_local`
    // `Cell<bool>` rather than real CPU state. `thread_local` is the right
    // stand-in for "IF, per CPU": family B's contention probe spawns real
    // `std::thread`s, and each one needs its own independent enable/disable
    // state, exactly like each real CPU core has its own EFLAGS.IF — a
    // single shared `Cell`/`AtomicBool` would conflate "this thread's
    // interrupts are off" with "some other thread's are", which is not the
    // property under test.
    std::thread_local! {
        static IF_ENABLED: Cell<bool> = Cell::new(true);
        // How many times `without_interrupts` actually restored the flag —
        // lets a test confirm the instrument did real work, not just that
        // the final state happens to look right.
        static RESTORE_COUNT: Cell<usize> = Cell::new(0);
        // Recursion guard for the simulated timer ISR (test 6 only) — an ISR
        // does not fire again inside itself, exactly like a real one runs
        // with interrupts masked once entered.
        static IN_ISR: Cell<bool> = Cell::new(false);
        static ISR_FIRE_COUNT: Cell<usize> = Cell::new(0);
        // The mutex the simulated timer ISR reenters, when armed. `'static`
        // because the fake ISR fires from a plain associated function with
        // no way to carry a borrowed reference.
        static TIMER_TARGET: RefCell<Option<&'static IrqMutex<u32, FakeIrq>>> = RefCell::new(None);
    }

    struct FakeIrq;

    impl FakeIrq {
        /// Reset all thread-local state. Cargo's default test harness runs
        /// each `#[test]` on its own fresh OS thread, so this is only
        /// belt-and-suspenders against a future harness change — but it
        /// costs nothing and makes every test's starting state explicit
        /// rather than assumed.
        fn reset() {
            IF_ENABLED.with(|c| c.set(true));
            RESTORE_COUNT.with(|c| c.set(0));
            IN_ISR.with(|c| c.set(false));
            ISR_FIRE_COUNT.with(|c| c.set(0));
            TIMER_TARGET.with(|c| *c.borrow_mut() = None);
        }

        fn set_enabled(v: bool) {
            IF_ENABLED.with(|c| c.set(v));
        }

        fn restore_count() -> usize {
            RESTORE_COUNT.with(|c| c.get())
        }

        fn isr_fire_count() -> usize {
            ISR_FIRE_COUNT.with(|c| c.get())
        }

        /// Arm the simulated timer ISR to reenter `target` the next time
        /// `are_enabled()` observes interrupts on. Test 6 only.
        fn arm_timer_isr(target: &'static IrqMutex<u32, FakeIrq>) {
            TIMER_TARGET.with(|c| *c.borrow_mut() = Some(target));
        }

        /// The simulated timer ISR. Fires only while armed, only while IF
        /// is observed on, and never re-enters itself.
        fn maybe_fire_timer_isr() {
            if IN_ISR.with(|c| c.get()) {
                return;
            }
            let target = TIMER_TARGET.with(|c| *c.borrow());
            if let Some(m) = target {
                IN_ISR.with(|c| c.set(true));
                ISR_FIRE_COUNT.with(|c| c.set(c.get() + 1));
                // The ISR's own body reenters the SAME mutex through the
                // SAME `with` path a real scheduler allocation would use —
                // this call is the entire mechanism test 6 depends on.
                m.with(|v| *v = v.wrapping_add(1));
                IN_ISR.with(|c| c.set(false));
            }
        }
    }

    impl IrqControl for FakeIrq {
        fn are_enabled() -> bool {
            let enabled = IF_ENABLED.with(|c| c.get());
            // Real hardware can still deliver an already-pending interrupt
            // for as long as IF reads as on — this is the checkpoint that
            // makes that observable in a synchronous, single-threaded test.
            if enabled {
                Self::maybe_fire_timer_isr();
            }
            enabled
        }

        fn without_interrupts<R>(f: impl FnOnce() -> R) -> R {
            // Checking `are_enabled()` first models the real gap between
            // "decide to mask interrupts" and the `cli` that actually does
            // it — on real hardware that gap is a handful of instructions;
            // here it is this one call. Everything test 6 depends on rides
            // on this line running BEFORE the flag is actually cleared.
            Self::are_enabled();
            let prev = IF_ENABLED.with(|c| c.get());
            IF_ENABLED.with(|c| c.set(false));
            let r = f();
            IF_ENABLED.with(|c| c.set(prev));
            RESTORE_COUNT.with(|c| c.set(c.get() + 1));
            r
        }
    }

    fn leaked_mutex() -> &'static IrqMutex<u32, FakeIrq> {
        alloc::boxed::Box::leak(alloc::boxed::Box::new(IrqMutex::new(0u32)))
    }

    /// (C) Instrument audit: the body passed to `with` must actually
    /// observe interrupts disabled, even though they were enabled at the
    /// call site — this is the property the whole type exists to provide.
    #[test]
    fn with_body_runs_with_interrupts_disabled_when_entered_enabled() {
        FakeIrq::reset();
        FakeIrq::set_enabled(true);
        let m = leaked_mutex();
        m.with(|_| {
            assert!(
                !FakeIrq::are_enabled(),
                "with()'s body ran while interrupts were still enabled"
            );
        });
    }

    /// (C) `without_interrupts` must restore the PREVIOUS interrupt state,
    /// not unconditionally turn interrupts back on. Entering enabled must
    /// leave interrupts enabled; entering already-disabled must leave them
    /// disabled. The second half is what distinguishes this from a bare
    /// `cli`/`sti` pair, and is exactly why `kernel/src/allocator/mod.rs`'s
    /// "Interrupt safety" comment insists on `without_interrupts` — a
    /// hand-rolled `sti` there would wrongly re-enable interrupts inside a
    /// caller (the timer ISR itself) that had them off on purpose.
    #[test]
    fn with_restores_the_previous_state_not_unconditionally_enabling() {
        FakeIrq::reset();
        let m = leaked_mutex();

        FakeIrq::set_enabled(true);
        m.with(|_| {});
        assert!(
            FakeIrq::are_enabled(),
            "entering ENABLED must leave interrupts enabled afterward"
        );

        FakeIrq::set_enabled(false);
        m.with(|_| {});
        assert!(
            !FakeIrq::are_enabled(),
            "entering DISABLED must leave interrupts DISABLED afterward — a plain \
             cli/sti pair would incorrectly turn them back on here"
        );
    }

    /// (C) Nesting one `IrqMutex`'s `with` inside another's must keep
    /// interrupts disabled continuously across both bodies, and restore to
    /// the original state exactly once the outer call fully returns — each
    /// `with()` call performs its own independent disable/restore pair.
    #[test]
    fn nested_with_keeps_interrupts_disabled_throughout_and_restores_once() {
        FakeIrq::reset();
        FakeIrq::set_enabled(true);
        let outer = leaked_mutex();
        let inner: &'static IrqMutex<u32, FakeIrq> =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(IrqMutex::new(0u32)));

        let restores_before = FakeIrq::restore_count();
        outer.with(|_| {
            assert!(!FakeIrq::are_enabled(), "outer body must see interrupts disabled");
            inner.with(|_| {
                assert!(
                    !FakeIrq::are_enabled(),
                    "inner body (a DIFFERENT IrqMutex, nested inside the outer one) \
                     must also see interrupts disabled"
                );
            });
            assert!(
                !FakeIrq::are_enabled(),
                "interrupts must still be disabled after the inner call returns, \
                 while still inside the outer body"
            );
        });
        assert!(
            FakeIrq::are_enabled(),
            "interrupts must be back to the pre-outer-call state once the outer \
             call fully returns"
        );
        assert_eq!(
            FakeIrq::restore_count() - restores_before,
            2,
            "exactly two with() calls happened, so exactly two restores must have run"
        );
    }

    /// (C) `try_with` on an already-held mutex must return `None`, and must
    /// leave the interrupt state exactly as it found it — checked both
    /// starting enabled and starting already-disabled, since a sabotaged
    /// implementation could plausibly get one right and not the other.
    #[test]
    fn try_with_on_a_held_mutex_returns_none_and_leaves_interrupt_state_unchanged() {
        FakeIrq::reset();
        let m = leaked_mutex();
        for &start in &[true, false] {
            FakeIrq::set_enabled(start);
            // Hold the real lock directly. This reaches the private `inner`
            // field, visible here only because `tests` is a child module of
            // `irqmutex` — never exposed as part of the public API (see the
            // module doc: no `lock()`, no public guard).
            let raw_guard = m.inner.lock();
            let before = FakeIrq::are_enabled();
            let result = m.try_with(|_| unreachable!("body must not run while the mutex is held"));
            assert!(result.is_none(), "try_with must fail while the mutex is held");
            assert_eq!(
                FakeIrq::are_enabled(),
                before,
                "a failed try_with must not perturb interrupt state — started at {start}"
            );
            drop(raw_guard);
        }
    }

    /// (C) `try_with` on a free mutex must succeed and run its body with
    /// interrupts disabled too, exactly like `with` — non-blocking is the
    /// only difference between the two.
    #[test]
    fn try_with_succeeds_and_runs_its_body_with_interrupts_disabled() {
        FakeIrq::reset();
        FakeIrq::set_enabled(true);
        let m = leaked_mutex();
        let result = m.try_with(|v| {
            assert!(
                !FakeIrq::are_enabled(),
                "try_with's body must run with interrupts disabled"
            );
            *v += 1;
            *v
        });
        assert_eq!(result, Some(1));
        assert!(
            FakeIrq::are_enabled(),
            "interrupts must be restored to enabled after a successful try_with"
        );
    }

    /// (A) Reentrancy probe — deterministic, no threads. Models the real
    /// single-core bug this type exists to make structurally impossible
    /// (see the module doc comment, `kernel/src/allocator/mod.rs`'s
    /// "Interrupt safety" comment, and commit `ab58dba`): ordinary
    /// interruptible kernel code can hold `BUDDY`'s lock when the timer
    /// fires; the ISR's own path (`Scheduler::switch_to_next` growing a run
    /// queue) can itself need to allocate, reentering the SAME
    /// non-reentrant `spin::Mutex` on the SAME core and spinning forever.
    ///
    /// Mechanism: `FakeIrq::without_interrupts` checks `are_enabled()` as
    /// its very FIRST action, before it flips the fake IF to false — this
    /// models the real hardware instant between deciding to mask
    /// interrupts and the `cli` that actually does it, during which an
    /// already-pending interrupt can still be delivered. `are_enabled()`
    /// fires a simulated "timer ISR" hook whenever it observes IF=true;
    /// that hook calls `with()` on the SAME `IrqMutex` under test, standing
    /// in for `switch_to_next`'s allocation reentering the allocator lock.
    /// A recursion guard (`IN_ISR`) stops the simulated ISR from firing
    /// inside itself, matching a real ISR running with interrupts masked
    /// once entered.
    ///
    /// **This test is written as a POSITIVE test: it passes normally.**
    /// With the correct order (`with` disables interrupts BEFORE ever
    /// touching the real lock), the hook's `are_enabled()` check runs
    /// before `self.inner.lock()` is reached, so when the simulated ISR
    /// fires, the mutex is not held yet — it acquires it, runs, releases,
    /// and the outer call proceeds normally.
    ///
    /// If `with`'s order is ever inverted (lock first, disable
    /// interrupts after), the mutex is already held by the time
    /// `without_interrupts`'s `are_enabled()` check runs, so the simulated
    /// ISR's own `with()` call spins forever trying to reacquire the same
    /// non-reentrant `spin::Mutex` from the same (only) thread of
    /// execution. **This test then HANGS — that hang IS the failure
    /// signal**, exactly like the real ~1-in-10 boot hang `ab58dba` fixed;
    /// run it under an external timeout
    /// (`timeout 45 cargo test simulated_timer_isr_cannot_fire_inside_the_critical_section`).
    #[test]
    fn simulated_timer_isr_cannot_fire_inside_the_critical_section() {
        FakeIrq::reset();
        FakeIrq::set_enabled(true);
        let m = leaked_mutex();
        FakeIrq::arm_timer_isr(m);

        m.with(|v| {
            *v += 1;
        });

        assert!(
            FakeIrq::isr_fire_count() >= 1,
            "the simulated timer ISR never fired at all — this test would pass \
             vacuously without exercising the reentrancy path it exists to check"
        );
        assert!(
            FakeIrq::are_enabled(),
            "interrupts must be back to enabled once the outer with() call returns"
        );
    }

    /// (B) Contention probe with real `std::thread`s. Models HOST-level
    /// contention across OS threads, NOT this kernel's single-core
    /// execution (there is exactly one CPU in this kernel; see the module
    /// doc comment and test 6 above for the single-core failure mode this
    /// type actually targets). What this proves instead: mutual exclusion
    /// itself holds under real concurrent access — 8 threads times 500
    /// increments through `with()` must land on exactly 4000, no lost
    /// updates.
    ///
    /// Synchronizes via `Barrier` (start together) and a watchdog `recv_timeout`
    /// on an `mpsc::channel` (never a bare `join()`, and no `sleep` as a
    /// synchronization mechanism) so a real deadlock reports as a clear
    /// panic instead of the test process hanging forever.
    #[test]
    fn mutual_exclusion_holds_under_real_contention() {
        FakeIrq::reset();
        const THREADS: usize = 8;
        const ITERS: usize = 500;

        let m = leaked_mutex();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
        let (tx, rx) = std::sync::mpsc::channel::<()>();

        for _ in 0..THREADS {
            let b = barrier.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                // Each real OS thread gets its own thread-local "IF", the
                // same way each real CPU core has its own EFLAGS.IF.
                FakeIrq::set_enabled(true);
                b.wait();
                for _ in 0..ITERS {
                    m.with(|v| *v += 1);
                }
                tx.send(()).unwrap();
            });
        }
        drop(tx);

        for i in 0..THREADS {
            rx.recv_timeout(std::time::Duration::from_secs(30)).unwrap_or_else(|_| {
                panic!("watchdog: thread {i} did not finish in 30s — IrqMutex likely wedged")
            });
        }

        assert_eq!(
            m.try_with(|v| *v).unwrap(),
            (THREADS * ITERS) as u32,
            "the protected value lost updates — mutual exclusion itself is broken"
        );
    }
}
