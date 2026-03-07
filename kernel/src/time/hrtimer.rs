// kernel/src/time/hrtimer.rs
//
// High-resolution timer queue.
//
// DESIGN:
//   - Timers are stored sorted by expiry_ns (soonest first).
//   - tick() is called from the timer ISR; it drains expired timers,
//     calls KernelFn actions inline, and fills a small fixed-size array
//     with PIDs to wake (no alloc in ISR path).
//   - start()/cancel() acquire QUEUE lock; safe under cli.
//
// LOCKING DISCIPLINE (see timer_preempt.rs for full analysis):
//   ISR path:  QUEUE lock (brief) → release → scheduler lock
//   nanosleep: cli → scheduler lock → QUEUE lock → release → block_current
//   These never overlap because cli prevents the ISR from firing while
//   nanosleep holds the scheduler lock.
//
// INVARIANT for KernelFn:
//   The callback must NOT attempt to re-acquire QUEUE.

use alloc::vec::Vec;
use spin::Mutex;

pub enum HrTimerAction {
    /// Wake the process with this PID.
    WakePid(usize),
    /// Call a kernel function (must not lock QUEUE).
    KernelFn(fn()),
}

pub struct HrTimer {
    pub id: u32,
    pub expiry_ns: u64,
    pub action: HrTimerAction,
}

struct HrTimerQueue {
    timers: Vec<HrTimer>,
    next_id: u32,
}

impl HrTimerQueue {
    const fn new() -> Self {
        HrTimerQueue {
            timers: Vec::new(),
            next_id: 1,
        }
    }
}

static QUEUE: Mutex<HrTimerQueue> = Mutex::new(HrTimerQueue::new());

/// Schedule a new hrtimer.
///
/// Returns a unique timer ID that can be passed to `cancel`.
/// Timers are stored sorted by expiry so that `tick` can drain from the front.
pub fn start(expiry_ns: u64, action: HrTimerAction) -> u32 {
    let mut q = QUEUE.lock();
    let id = q.next_id;
    q.next_id = q.next_id.wrapping_add(1).max(1); // never 0

    // Insertion sort by expiry_ns (list is usually very short).
    let pos = q.timers.partition_point(|t| t.expiry_ns <= expiry_ns);
    q.timers.insert(pos, HrTimer { id, expiry_ns, action });
    id
}

/// Cancel a pending timer by ID.
///
/// Returns true if the timer was found and removed, false if it had already
/// fired or never existed.
pub fn cancel(id: u32) -> bool {
    let mut q = QUEUE.lock();
    if let Some(pos) = q.timers.iter().position(|t| t.id == id) {
        q.timers.remove(pos);
        true
    } else {
        false
    }
}

/// Called from the timer ISR once per tick.
///
/// Drains all timers whose `expiry_ns <= now_ns`.
///   - `KernelFn` actions are called *while holding QUEUE* (see invariant above).
///   - `WakePid` PIDs are collected into `pids_out`; QUEUE is released first.
///
/// Returns the number of PIDs written into `pids_out`.
/// If more than 8 timers with WakePid fire in the same tick, extras are
/// silently dropped — they will be processed in the next tick at most 10 ms later.
pub fn tick(now_ns: u64, pids_out: &mut [usize; 8]) -> usize {
    let mut count = 0usize;

    let mut q = QUEUE.lock();

    // Walk from the front (soonest first) and drain expired timers.
    while let Some(t) = q.timers.first() {
        if t.expiry_ns > now_ns {
            break; // remaining timers are in the future
        }
        let timer = q.timers.remove(0);
        match timer.action {
            HrTimerAction::KernelFn(f) => {
                // Call while holding the lock — caller's invariant says f won't re-lock.
                f();
            }
            HrTimerAction::WakePid(pid) => {
                if count < 8 {
                    pids_out[count] = pid;
                    count += 1;
                }
                // If count == 8, drop the PID; it will be retried next tick.
            }
        }
    }

    count
}
