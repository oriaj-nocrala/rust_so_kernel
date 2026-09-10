//! The seam between the scheduler core and wherever time actually comes
//! from — real hardware in the kernel, a deterministic fake in tests.
//!
//! `Clock` plays the same role for `sched` that `hal::PortIo`/`hal::PhysMem`
//! play for `hal`, that `mm::PhysMap`/`mm::FrameSource` play for `mm`, and
//! that `diag::IrqControl` plays for `diag`: a small trait standing in for
//! something this host-testable crate cannot provide for itself. Every
//! other seam in this crate's family exists because the *real* thing (port
//! I/O, physical memory, interrupt control) genuinely cannot run on a test
//! host. `Clock` exists for a related but distinct reason: `SchedCore` used
//! to own its own tick counter (`global_ticks: u32`, incremented once per
//! call inside `advance_ticks`) and there was nothing wrong with that on
//! correctness grounds — but it meant a test could only ever reach an aging
//! epoch by calling `advance_ticks` fifty real times in a loop, and no test
//! could express "what if the aging window were measured in wall-clock time"
//! or "what happens right after a burst of ticks lands at once" without
//! actually driving fifty (or five thousand) real calls. Injecting the clock
//! instead of owning a counter turns "reach epoch 40" from a fifty-iteration
//! loop into a single `FakeClock::set(...)` call, and makes the passage of
//! time itself something a test can construct instead of only simulate by
//! brute force.
//!
//! See [`crate::core::SchedCore::advance_ticks`] for the one place this
//! trait is consumed, and its doc comment for exactly how the aging-epoch
//! check is defined in terms of it (a crossing check, not a modulo — the
//! distinction matters the moment a clock can jump by more than one tick
//! between calls, which a real `FakeClock` in a test deliberately does and a
//! `% AGING_EPOCH == 0` check would silently miss).
pub trait Clock {
    /// Monotonic tick count since boot. Must never go backwards — callers
    /// are entitled to assume `now_ticks()` never decreases across
    /// successive calls on the same clock instance.
    fn now_ticks(&self) -> u64;
}

/// A [`Clock`] a test controls directly, instead of advancing one real tick
/// at a time.
///
/// Interior mutability (`Cell`, not a `&mut self` API) is deliberate: the
/// clock needs to be read through a shared `&impl Clock` (that's the shape
/// [`crate::core::SchedCore::advance_ticks`] takes, matching every other
/// seam in this crate's family — `SchedEntity`'s methods all take `&self`/
/// `&mut self` on the entity itself, never threading a second `&mut`
/// alongside it), while still letting a test advance it from outside that
/// borrow. `Cell`, not `RefCell` or an atomic: this crate's tests are
/// single-threaded (see the crate-level doc comment's "no `rand`, no
/// threads" stance in `invariants.rs`), so there is no concurrent access to
/// guard against and no `Sync` requirement to satisfy — a bare `Cell<u64>`
/// is the simplest thing that is still correct.
///
/// `pub` (not `#[cfg(test)]`-gated): a fake clock is exactly the kind of
/// thing a *consumer* of this crate might also want for its own tests (the
/// kernel adapter's own test-only code, should it ever want one, or a
/// future host-testable crate that also needs to fast-forward scheduler
/// time) — same reasoning `mm`/`hal` use for exporting their own test
/// doubles rather than keeping them crate-private. Exporting it costs
/// nothing extra here since it adds no dependency (see below) and no
/// unsafe code.
///
/// No new dependency: built entirely out of `core::cell::Cell`, keeping
/// this crate's "zero dependencies" property (see the crate-level doc
/// comment and `invariants.rs`'s note on rolling its own PRNG for the same
/// reason) intact.
pub struct FakeClock {
    ticks: core::cell::Cell<u64>,
}

impl FakeClock {
    /// Starts at tick 0 — matches `SchedCore::new()`'s zeroed tick state
    /// (the old `global_ticks: u32` also started at 0), so a fresh
    /// `FakeClock` paired with a fresh `SchedCore` reproduces the same
    /// "epoch 1 is `AGING_EPOCH` calls away" starting point production code
    /// gets from a real, freshly-booted tick source.
    pub const fn new() -> Self {
        Self { ticks: core::cell::Cell::new(0) }
    }

    /// Jump directly to `ticks`, without regard to the previous value.
    ///
    /// Unlike [`Self::advance`], this can move the clock backwards — that is
    /// intentionally allowed here even though [`Clock::now_ticks`]'s
    /// contract forbids it for a *real* clock: `set` exists for a test that
    /// wants to construct a specific clock reading directly (e.g. "one tick
    /// before the next epoch") rather than only ever moving forward from
    /// wherever the clock currently sits. Nothing in `SchedCore` assumes
    /// monotonicity beyond what `advance_ticks`'s own crossing check
    /// tolerates (see that method's doc comment) — it is never violated by
    /// the one production clock this crate ships an adapter contract for,
    /// only by a test that asks for it on purpose.
    pub fn set(&self, ticks: u64) {
        self.ticks.set(ticks);
    }

    /// Move the clock forward by `delta` ticks (`delta == 1` reproduces the
    /// kernel's real "one call per timer tick" cadence; anything larger is
    /// exactly the "what if ticks arrive in a burst" scenario a real,
    /// one-tick-at-a-time counter could never let a test express directly).
    /// Wrapping, to match [`Clock::now_ticks`]'s `u64` domain and
    /// [`crate::core::SchedCore::advance_ticks`]'s own `wrapping_sub`-based
    /// crossing check.
    pub fn advance(&self, delta: u64) {
        self.ticks.set(self.ticks.get().wrapping_add(delta));
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for FakeClock {
    fn now_ticks(&self) -> u64 {
        self.ticks.get()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh `FakeClock` reads 0, matching `SchedCore::new()`'s starting
    /// state.
    #[test]
    fn fake_clock_starts_at_zero() {
        let clock = FakeClock::new();
        assert_eq!(clock.now_ticks(), 0);
    }

    /// `advance` is additive, not a replacement — three separate `advance
    /// (1)` calls read the same as one `advance(3)`.
    #[test]
    fn fake_clock_advance_accumulates() {
        let clock = FakeClock::new();
        clock.advance(1);
        clock.advance(1);
        clock.advance(1);
        assert_eq!(clock.now_ticks(), 3);
    }

    /// `set` overwrites the reading outright, including moving it backwards
    /// — deliberately allowed for a fake clock, see `set`'s doc comment.
    #[test]
    fn fake_clock_set_overwrites_and_allows_going_backwards() {
        let clock = FakeClock::new();
        clock.advance(100);
        clock.set(10);
        assert_eq!(clock.now_ticks(), 10);
    }
}
