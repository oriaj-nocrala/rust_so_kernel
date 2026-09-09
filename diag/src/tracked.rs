//! `TrackedMutex` — a `spin::Mutex` that reports its own acquire/release to
//! an observer, with the *ordering* of those reports enforced structurally
//! instead of re-derived by hand at every lock site.
//!
//! # Why this exists
//!
//! Two places in this kernel already wrap a `spin::Mutex` in exactly this
//! pattern — `process::scheduler::TrackedSchedulerGuard` (reporting into
//! `LockDiag`) and `vfs::ramfs::TrackedEntriesGuard` (reporting into
//! `DirLockDiag`) — and the ordering rules they must obey are subtle enough
//! that the two disagreed on one of them:
//!
//! - **Acquire must be recorded strictly AFTER the mutex is held.** Recording
//!   first names a caller that is still spinning as the lock's holder, which
//!   is one of the ten defective instruments the 2026-08-05 hang hunt
//!   produced (`docs/hang-hunt-bug2-findings.md`). Both existing wrappers get
//!   this right, but nothing enforced it: moving the call one line earlier in
//!   `ramfs` left all 150 host tests green until a contention probe was
//!   written specifically to make the ordering observable.
//! - **Whatever context the observer needs from *outside* the critical
//!   section must be read BEFORE the lock.** `ramfs` reads the current pid
//!   there because the kernel's real observer takes the `SCHEDULER` lock and
//!   ends with an unconditional `sti`, neither of which may happen inside
//!   this critical section.
//! - **Release ordering.** Here the two wrappers genuinely disagree:
//!   `TrackedSchedulerGuard::drop` drops the real guard first and records
//!   after; `TrackedEntriesGuard::drop` records first and lets the guard field
//!   drop afterward (a `Drop` impl's body runs before its fields drop). This
//!   type picks the scheduler's order — unlock, then record — because it is
//!   the one that can never under-report: recording first leaves a window in
//!   which the mutex is still held while the counter already says it is free,
//!   and a panic snapshot taken in that window would report `outstanding=0`
//!   for a lock that is genuinely held.
//!
//!   In fairness to `ramfs`: for the failure mode `DirLockDiag` was actually
//!   built to catch — a critical section *abandoned* mid-flight, so `Drop`
//!   never runs at all — the order is irrelevant, since neither variant
//!   records anything. The difference only shows up in a snapshot taken
//!   inside those few instructions.
//!
//! # What this does NOT do
//!
//! It does not enforce interrupt discipline. `TrackedSchedulerGuard` also
//! asserts IF=0 on both ends, and `mm`'s allocator locks must be taken inside
//! `without_interrupts` — both are `x86_64`-specific and belong in the
//! observer implementation on the kernel side, not here.


/// The reporting seam for [`TrackedMutex`]. One implementation per real
/// lock; kernel-side implementations forward into `LockDiag`/`DirLockDiag`.
pub trait LockObserver {
    /// Whatever must be read *before* the lock is taken and handed to
    /// [`Self::on_acquire`] afterward — a pid, a `&'static Location`, `()`.
    /// See the module doc: reading it inside the critical section is a real
    /// hazard, not a style preference.
    type Ctx;

    /// A per-call-site label, passed straight through to
    /// [`Self::on_acquire`] — `ramfs` uses the operation name (`"mkdir"`),
    /// the scheduler uses `()`. Distinct from `Ctx` because it is supplied
    /// by the caller, not read from the environment.
    type Tag: Copy;

    /// Called before the mutex is taken. Must not itself take the mutex.
    fn before_lock(&self) -> Self::Ctx;

    /// Called once the mutex is genuinely held — never before.
    fn on_acquire(&self, ctx: Self::Ctx, tag: Self::Tag);

    /// Called once the mutex is genuinely released — never before.
    fn on_release(&self);
}

/// A `spin::Mutex` whose acquire/release is reported through an
/// [`LockObserver`] in the one correct order. See the module doc comment.
pub struct TrackedMutex<T, O> {
    inner: spin::Mutex<T>,
    obs:   O,
}

impl<T, O> TrackedMutex<T, O> {
    /// `const` so this can live in a `static` (the kernel's `SCHEDULERS`
    /// array is built this way).
    pub const fn new(value: T, obs: O) -> Self {
        Self { inner: spin::Mutex::new(value), obs }
    }

    /// The observer, for tests and for diagnostics that want to read the
    /// counters without locking.
    pub fn observer(&self) -> &O {
        &self.obs
    }

    /// Whether the underlying mutex is currently held. Non-blocking, and
    /// deliberately not routed through the observer — this is for probes
    /// asserting things *about* the instrument, not part of the instrument.
    pub fn is_locked(&self) -> bool {
        self.inner.is_locked()
    }
}

impl<T, O: LockObserver> TrackedMutex<T, O> {
    /// Lock, reporting through the observer. Blocks (spins) if held.
    ///
    /// The ordering below is the entire point of this type: `before_lock`
    /// outside the critical section, `on_acquire` strictly after
    /// `inner.lock()` has returned.
    pub fn lock(&self, tag: O::Tag) -> TrackedGuard<'_, T, O> {
        let ctx = self.obs.before_lock();
        let guard = self.inner.lock();
        self.obs.on_acquire(ctx, tag);
        TrackedGuard { guard: Some(guard), obs: &self.obs }
    }

    /// Non-blocking lock. Reports nothing on failure — a caller that never
    /// held the lock must not appear in the counters at all.
    pub fn try_lock(&self, tag: O::Tag) -> Option<TrackedGuard<'_, T, O>> {
        let ctx = self.obs.before_lock();
        let guard = self.inner.try_lock()?;
        self.obs.on_acquire(ctx, tag);
        Some(TrackedGuard { guard: Some(guard), obs: &self.obs })
    }
}

/// RAII guard from [`TrackedMutex::lock`]. `Deref`/`DerefMut` straight
/// through, so call sites read like the plain guard they replace.
pub struct TrackedGuard<'a, T, O: LockObserver> {
    /// `Option` so `Drop` can release the real mutex *before* recording —
    /// a struct's fields drop only after its `Drop::drop` body has already
    /// run, so a plain field would be released after. See the module doc.
    guard: Option<spin::MutexGuard<'a, T>>,
    obs:   &'a O,
}

impl<'a, T, O: LockObserver> core::ops::Deref for TrackedGuard<'a, T, O> {
    type Target = T;
    fn deref(&self) -> &T {
        self.guard.as_ref().unwrap()
    }
}

impl<'a, T, O: LockObserver> core::ops::DerefMut for TrackedGuard<'a, T, O> {
    fn deref_mut(&mut self) -> &mut T {
        self.guard.as_mut().unwrap()
    }
}

impl<'a, T, O: LockObserver> Drop for TrackedGuard<'a, T, O> {
    fn drop(&mut self) {
        // Release the real mutex FIRST, then report. Reversing these two
        // lines leaves a window where the counters say the lock is free
        // while it is still held.
        self.guard = None;
        self.obs.on_release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};

    /// Observer that answers the one question the counters alone can't:
    /// *was the mutex actually held at the moment each callback fired?*
    /// It probes the mutex itself with a non-blocking `try_lock`, which is
    /// what makes the ordering observable from a single thread.
    struct ProbingObserver {
        /// The mutex this observer reports for. `&'static` because the
        /// observer is owned by the mutex, so it can't hold a plain
        /// back-reference.
        target: spin::Once<&'static TrackedMutex<u32, ProbingObserver>>,
        held_during_before_lock: AtomicBool,
        held_during_on_acquire:  AtomicBool,
        held_during_on_release:  AtomicBool,
        inside:                  AtomicUsize,
        max_inside:              AtomicUsize,
    }

    impl ProbingObserver {
        const fn new() -> Self {
            Self {
                target: spin::Once::new(),
                held_during_before_lock: AtomicBool::new(false),
                held_during_on_acquire: AtomicBool::new(false),
                held_during_on_release: AtomicBool::new(false),
                inside: AtomicUsize::new(0),
                max_inside: AtomicUsize::new(0),
            }
        }

        fn target_is_locked(&self) -> bool {
            self.target.get().map(|m| m.is_locked()).unwrap_or(false)
        }
    }

    impl LockObserver for ProbingObserver {
        type Ctx = ();
        type Tag = &'static str;

        fn before_lock(&self) -> () {
            self.held_during_before_lock.store(self.target_is_locked(), SeqCst);
        }

        fn on_acquire(&self, _ctx: (), _tag: &'static str) {
            self.held_during_on_acquire.store(self.target_is_locked(), SeqCst);
            let n = self.inside.fetch_add(1, SeqCst) + 1;
            self.max_inside.fetch_max(n, SeqCst);
        }

        fn on_release(&self) {
            self.held_during_on_release.store(self.target_is_locked(), SeqCst);
            self.inside.fetch_sub(1, SeqCst);
        }
    }

    fn leaked_probe() -> &'static TrackedMutex<u32, ProbingObserver> {
        let m: &'static TrackedMutex<u32, ProbingObserver> =
            alloc::boxed::Box::leak(alloc::boxed::Box::new(TrackedMutex::new(0u32, ProbingObserver::new())));
        m.observer().target.call_once(|| m);
        m
    }

    /// Family C (instrument audit), single-threaded and deterministic.
    ///
    /// This is the test the `vfs::ramfs` observer could not have: by probing
    /// the real mutex from inside each callback, the acquire/release ordering
    /// becomes observable without needing a second thread at all. Contrast
    /// `vfs/src/ramfs.rs`'s
    /// `record_acquire_never_fires_while_another_thread_holds_the_same_entries_lock`,
    /// which needs genuine contention precisely because its observer has no
    /// way to see the mutex.
    #[test]
    fn callbacks_fire_on_the_correct_side_of_the_real_lock() {
        let m = leaked_probe();
        {
            let mut g = m.lock("probe");
            *g += 1;
        }
        let o = m.observer();
        assert!(
            !o.held_during_before_lock.load(SeqCst),
            "before_lock ran while the mutex was already held — whatever it reads \
             (a pid, via a path that itself takes the SCHEDULER lock and ends in \
             `sti`) would then run inside the critical section"
        );
        assert!(
            o.held_during_on_acquire.load(SeqCst),
            "on_acquire ran while the mutex was NOT held — the instrument would name \
             a still-spinning caller as the lock's holder, which is exactly the \
             misattribution docs/hang-hunt-bug2-findings.md catalogued"
        );
        assert!(
            !o.held_during_on_release.load(SeqCst),
            "on_release ran while the mutex was STILL held — leaves a window where the \
             counters say the lock is free but it is not, so a panic snapshot taken \
             there reports outstanding=0 for a genuinely held lock"
        );
    }

    #[test]
    fn guard_derefs_through_and_mutates_the_protected_value() {
        let m = leaked_probe();
        {
            let mut g = m.lock("write");
            *g = 42;
        }
        assert_eq!(*m.lock("read"), 42);
    }

    #[test]
    fn try_lock_on_a_held_mutex_reports_nothing_at_all() {
        let m = leaked_probe();
        let _held = m.lock("outer");
        let before = m.observer().inside.load(SeqCst);
        assert!(m.try_lock("inner").is_none(), "try_lock must fail while held");
        assert_eq!(
            m.observer().inside.load(SeqCst),
            before,
            "a failed try_lock must not report an acquire — a caller that never held \
             the lock must not appear in the counters"
        );
    }

    /// Family B (contention probe with host threads). Models host-level
    /// contention, NOT this kernel's single-core execution.
    ///
    /// # What this measured, and the invariant it disproved
    ///
    /// This test was first written asserting `max_inside == 1` — "two threads
    /// can never both be between `on_acquire` and `on_release` on one mutex".
    /// That assertion **failed against correct code** (`left: 2`), and the
    /// failure was right:
    ///
    /// `TrackedGuard::drop` releases the real mutex and only *then* calls
    /// `on_release`. In the window between those two lines the mutex is
    /// genuinely free, so another thread can legitimately acquire it and call
    /// `on_acquire` before the first thread has recorded its release. Two
    /// "inside" reports, one real holder, nothing wrong.
    ///
    /// So the two release orders are not "one correct, one buggy" — they trade
    /// the *direction* of the error:
    ///
    /// - unlock-then-record (this type, and `TrackedSchedulerGuard`): can
    ///   transiently **over**-report (`outstanding` briefly too high), never
    ///   under-report a held lock.
    /// - record-then-unlock (`vfs::ramfs::TrackedEntriesGuard`): can
    ///   transiently **under**-report (`outstanding=0` for a held lock), never
    ///   over-report.
    ///
    /// This matters for how `LockDiag`'s output is read. Its doc comment says
    /// "anything but 0/1 means a guard leaked" — true for `SCHEDULER_LOCK`
    /// only because that lock is always taken with interrupts disabled, so
    /// nothing can run in the window at all. It is **not** a property of the
    /// counter itself, and would not survive this kernel gaining a second CPU.
    ///
    /// What this test asserts instead is what genuinely holds under
    /// contention: mutual exclusion is real (no lost updates) and every
    /// acquire is matched by a release. The ordering itself is pinned by
    /// `callbacks_fire_on_the_correct_side_of_the_real_lock` above, which is
    /// single-threaded precisely so `is_locked()` unambiguously means "held by
    /// me" — under contention it cannot tell "I hold it" from "someone else
    /// does", so it would be blind to exactly the misattribution it is
    /// checking for.
    #[test]
    fn mutual_exclusion_and_balanced_counts_hold_under_real_contention() {
        const THREADS: usize = 8;
        const ITERS: usize = 500;

        let m = leaked_probe();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
        let (tx, rx) = std::sync::mpsc::channel::<()>();

        for _ in 0..THREADS {
            let b = barrier.clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                b.wait();
                for _ in 0..ITERS {
                    let mut g = m.lock("hammer");
                    *g = g.wrapping_add(1);
                }
                tx.send(()).unwrap();
            });
        }
        drop(tx);

        for i in 0..THREADS {
            rx.recv_timeout(std::time::Duration::from_secs(30)).unwrap_or_else(|_| {
                panic!("watchdog: thread {i} did not finish in 30s — TrackedMutex likely wedged")
            });
        }

        let o = m.observer();
        assert_eq!(
            o.inside.load(SeqCst),
            0,
            "every acquire must have a matching release once every thread has finished"
        );
        // `max_inside` is deliberately observed, not bounded — see this
        // test's doc comment. Under this type's unlock-then-record order it
        // legitimately exceeds 1 under contention; asserting otherwise is what
        // this test originally got wrong.
        assert!(
            o.max_inside.load(SeqCst) >= 1,
            "the observer never saw a single acquire — the threads did not run at all"
        );
        assert_eq!(
            *m.lock("final"),
            (THREADS * ITERS) as u32,
            "the protected value lost updates — mutual exclusion itself is broken"
        );
    }
}
