//! Interruptible waits: who gets to end one, and how a signal ends it.
//!
//! A blocked process is registered with whatever will wake it — a pipe's
//! waiter queue, `POLL_WAITERS`, an hrtimer, `FUTEX_WAITERS`, a socket's
//! waiter list. A signal that should interrupt the wait cannot clean those
//! registrations up: every one of them is locked *before* the scheduler,
//! and the signal is sent with the scheduler lock held. So the
//! registrations stay where they are, and what decides the race is a
//! [`WaitCell`], shared (behind an `Arc`, kernel side) by the process and
//! every registration of its current wait:
//!
//! - a waker [`claim`](WaitCell::claim)s it before it does anything to the
//!   waiter — consume pipe bytes for it, write `revents` into its memory,
//!   hand it a key — and skips the registration if the claim fails;
//! - a signal [`cancel`](WaitCell::cancel)s it, and interrupts the wait
//!   only if that succeeds.
//!
//! Exactly one of them wins, lock-free. A cancelled cell turns every
//! registration of that wait stale at once, and stale ones are dropped by
//! whoever finds them next. A claimed cell means a waker is completing the
//! call: the signal then waits for it, which is also what Linux does — a
//! completed call returns its result and the handler runs after it.
//!
//! [`restarts`] is the other half: once interrupted, whether the call
//! re-executes or returns `EINTR` depends on what the signal turns out to
//! do, decided when it is delivered (Linux's `ERESTARTSYS` handling in
//! `handle_signal`).

use core::sync::atomic::{AtomicU8, Ordering};

const ARMED: u8 = 0;
const CLAIMED: u8 = 1;
const CANCELLED: u8 = 2;

/// One wait's outcome, decided once: armed until a waker claims it or a
/// signal cancels it, whichever comes first.
#[derive(Debug)]
pub struct WaitCell {
    state: AtomicU8,
}

impl Default for WaitCell {
    fn default() -> Self {
        Self::new()
    }
}

impl WaitCell {
    pub const fn new() -> Self {
        Self { state: AtomicU8::new(ARMED) }
    }

    /// A waker takes the wait over. `false`: a signal (or the waiter
    /// itself, abandoning a wait it never slept in) got there first, and
    /// the waker must leave this waiter alone.
    pub fn claim(&self) -> bool {
        self.state.compare_exchange(ARMED, CLAIMED, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }

    /// A signal (or the waiter) ends the wait. `false`: a waker has
    /// claimed it and will wake the process with the call's result.
    pub fn cancel(&self) -> bool {
        self.state.compare_exchange(ARMED, CANCELLED, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }

    /// Nobody has claimed or cancelled it yet.
    pub fn is_armed(&self) -> bool {
        self.state.load(Ordering::Acquire) == ARMED
    }
}

/// How an interrupted call behaves when the signal runs a handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    /// Re-executed if the handler was installed with `SA_RESTART`, `EINTR`
    /// otherwise: `read`/`write` on pipes, ttys and sockets, `waitpid`,
    /// `futex` (Linux's `ERESTARTSYS`).
    SaRestart,
    /// `EINTR` whenever a handler runs, whatever its flags: `nanosleep`,
    /// `poll`, `epoll_wait` (Linux's `ERESTARTNOHAND`).
    NoHandlerOnly,
}

/// What the interrupting signal does once delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalEffect {
    Handler { sa_restart: bool },
    Terminate,
    Stop,
    /// Nothing deliverable after all.
    Nothing,
}

/// Whether the interrupted call re-executes (`true`) or returns `EINTR`.
/// When no handler runs — the process is stopped and later continued, or
/// the signal went away — the call re-executes and the interruption is
/// invisible, as on Linux. A terminated process never returns at all.
pub fn restarts(policy: RestartPolicy, effect: SignalEffect) -> bool {
    match effect {
        SignalEffect::Handler { sa_restart } => sa_restart && policy == RestartPolicy::SaRestart,
        SignalEffect::Terminate => false,
        SignalEffect::Stop | SignalEffect::Nothing => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;
    use std::vec::Vec;

    #[test]
    fn claim_then_cancel_fails() {
        let c = WaitCell::new();
        assert!(c.is_armed());
        assert!(c.claim());
        assert!(!c.is_armed());
        assert!(!c.cancel());
        assert!(!c.claim(), "a cell is claimed once");
    }

    #[test]
    fn cancel_then_claim_fails() {
        let c = WaitCell::new();
        assert!(c.cancel());
        assert!(!c.claim());
        assert!(!c.cancel(), "a cell is cancelled once");
    }

    /// Family B (a real race, not a sequential script): one waker and one
    /// signal hit the same cell from two threads, many times over. Exactly
    /// one wins every round — never both (a pipe's bytes consumed for a
    /// reader that has gone on to return EINTR), never neither (a process
    /// left asleep with a signal pending).
    #[test]
    fn exactly_one_of_waker_and_signal_wins_a_race() {
        const ROUNDS: usize = 20_000;
        let claims = Arc::new(AtomicUsize::new(0));
        let cancels = Arc::new(AtomicUsize::new(0));
        for _ in 0..ROUNDS {
            let cell = Arc::new(WaitCell::new());
            let handles: Vec<_> = [true, false]
                .into_iter()
                .map(|waker| {
                    let (cell, claims, cancels) = (cell.clone(), claims.clone(), cancels.clone());
                    std::thread::spawn(move || {
                        if waker {
                            if cell.claim() {
                                claims.fetch_add(1, Ordering::Relaxed);
                            }
                        } else if cell.cancel() {
                            cancels.fetch_add(1, Ordering::Relaxed);
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
            assert!(!cell.is_armed());
        }
        assert_eq!(claims.load(Ordering::Relaxed) + cancels.load(Ordering::Relaxed), ROUNDS);
    }

    #[test]
    fn sa_restart_policy_follows_the_handler_flag() {
        let p = RestartPolicy::SaRestart;
        assert!(restarts(p, SignalEffect::Handler { sa_restart: true }));
        assert!(!restarts(p, SignalEffect::Handler { sa_restart: false }));
    }

    #[test]
    fn no_handler_only_policy_is_eintr_whenever_a_handler_runs() {
        let p = RestartPolicy::NoHandlerOnly;
        assert!(!restarts(p, SignalEffect::Handler { sa_restart: true }));
        assert!(!restarts(p, SignalEffect::Handler { sa_restart: false }));
    }

    #[test]
    fn no_handler_means_the_interruption_is_invisible() {
        for p in [RestartPolicy::SaRestart, RestartPolicy::NoHandlerOnly] {
            assert!(restarts(p, SignalEffect::Stop));
            assert!(restarts(p, SignalEffect::Nothing));
            assert!(!restarts(p, SignalEffect::Terminate));
        }
    }
}
