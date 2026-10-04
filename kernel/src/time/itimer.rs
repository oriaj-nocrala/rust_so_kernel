// kernel/src/time/itimer.rs
//
// `ITIMER_REAL` (`setitimer(2)`, `getitimer(2)`, `alarm(2)`): one interval
// timer per process that sends `SIGALRM` when it expires. `ITIMER_VIRTUAL`
// and `ITIMER_PROF` (CPU-time timers) are not implemented (`EINVAL`).
//
// Each armed timer is one `hrtimer` with `HrTimerAction::Alarm`. The timer
// ISR drains it (`hrtimer::tick`), then calls `on_expiry` *before* it takes
// the scheduler lock: that re-arms a periodic timer, and says which pids to
// signal. The scheduler lock is only taken afterwards, to post the signal.
//
// Locking: `TIMERS` comes before the hrtimer `QUEUE` (`set` holds it while it
// starts or cancels the hrtimer); the ISR holds `QUEUE` only inside `tick`
// and takes `TIMERS` after releasing it. `TIMERS` never nests with the
// scheduler lock. It is an `IrqLock` because the ISR takes it.
//
// Stale expiries: `set` cancels the old hrtimer, but another CPU's `tick`
// may already have popped it. `on_expiry` therefore matches the hrtimer id
// it fired with against the timer's current id, and ignores a mismatch.

use alloc::vec::Vec;

use crate::sync::IrqLock;

use super::hrtimer::{self, HrTimerAction};

struct Timer {
    pid: usize,
    /// The queued hrtimer's id.
    id: u32,
    expiry_ns: u64,
    interval_ns: u64,
}

static TIMERS: IrqLock<Vec<Timer>> = IrqLock::new(Vec::new());

/// `(remaining ns, interval ns)`; `(0, _)` means disarmed.
pub type Setting = (u64, u64);

/// Arms (or, with `value_ns == 0`, disarms) `pid`'s timer and returns what
/// it held before.
pub fn set(pid: usize, value_ns: u64, interval_ns: u64, now_ns: u64) -> Setting {
    let mut timers = TIMERS.lock();
    let old = match timers.iter().position(|t| t.pid == pid) {
        Some(i) => {
            let t = timers.swap_remove(i);
            hrtimer::cancel(t.id);
            (t.expiry_ns.saturating_sub(now_ns), t.interval_ns)
        }
        None => (0, 0),
    };
    if value_ns > 0 {
        let expiry_ns = now_ns.saturating_add(value_ns);
        let id = hrtimer::start(expiry_ns, HrTimerAction::Alarm { pid });
        timers.push(Timer { pid, id, expiry_ns, interval_ns });
    }
    old
}

pub fn get(pid: usize, now_ns: u64) -> Setting {
    TIMERS
        .lock()
        .iter()
        .find(|t| t.pid == pid)
        .map_or((0, 0), |t| (t.expiry_ns.saturating_sub(now_ns), t.interval_ns))
}

/// Forgets `pid`'s timer (the process died).
pub fn cancel_for(pid: usize) {
    let mut timers = TIMERS.lock();
    if let Some(i) = timers.iter().position(|t| t.pid == pid) {
        let t = timers.swap_remove(i);
        hrtimer::cancel(t.id);
    }
}

/// The hrtimer `id` of `pid`'s timer fired. `true` if `pid` is to be sent
/// `SIGALRM`: the timer is the one that fired (not a replaced one). A
/// periodic timer is re-armed on its fixed cadence, skipping the periods
/// already missed; a one-shot one is dropped.
pub fn on_expiry(pid: usize, id: u32, now_ns: u64) -> bool {
    let mut timers = TIMERS.lock();
    let Some(i) = timers.iter().position(|t| t.pid == pid && t.id == id) else {
        return false;
    };
    if timers[i].interval_ns == 0 {
        timers.swap_remove(i);
        return true;
    }
    let t = &mut timers[i];
    let missed = now_ns.saturating_sub(t.expiry_ns) / t.interval_ns;
    t.expiry_ns = t.expiry_ns.saturating_add((missed + 1).saturating_mul(t.interval_ns));
    t.id = hrtimer::start(t.expiry_ns, HrTimerAction::Alarm { pid });
    true
}
