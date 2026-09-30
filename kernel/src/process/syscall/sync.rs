// kernel/src/process/syscall/sync.rs
//
// futex(202) — wait/wake, backs mlibc mutexes/condvars.

use crate::sync::Mutex;
use crate::process::TrapFrame;
use super::{errno, SyscallResult, validate_user_buffer, current_tf_ptr};
use alloc::collections::BTreeMap;

// ── futex(202) ─────────────────────────────────────────────────────────────

const FUTEX_WAIT: i32 = 0;
const FUTEX_WAKE: i32 = 1;
const FUTEX_REQUEUE: i32 = 3;
const FUTEX_CMP_REQUEUE: i32 = 4;
const FUTEX_WAIT_BITSET: i32 = 9;
const FUTEX_WAKE_BITSET: i32 = 10;
const FUTEX_PRIVATE_FLAG: i32 = 128;
const FUTEX_CLOCK_REALTIME: i32 = 256;
const FUTEX_BITSET_MATCH_ANY: u32 = 0xffff_ffff;

/// futex(202): long futex(uint32_t *uaddr, int futex_op, uint32_t val,
///                        const struct timespec *timeout, uint32_t *uaddr2, uint32_t val3)
///
/// - `WAIT` blocks the caller if `*uaddr == val` until a matching WAKE, a signal, or `timeout` (a *relative* timespec;
///   `ETIMEDOUT`). `WAIT_BITSET` is the same with an *absolute* timeout on `CLOCK_MONOTONIC` (`CLOCK_REALTIME` with
///   `FUTEX_CLOCK_REALTIME`) and a bit mask `val3`: Rust's `std` waits with it (`FUTEX_BITSET_MATCH_ANY`, no timeout, is
///   its plain wait).
/// - `WAKE` wakes up to `val` waiters on `uaddr`; `WAKE_BITSET` only those whose mask shares a bit with `val3`.
/// - `REQUEUE`/`CMP_REQUEUE` (musl's condition variables) wake up to `val` and move up to `val2` (the `timeout` argument
///   read as an integer) more to `uaddr2`; `CMP_REQUEUE` first checks `*uaddr == val3` (`EAGAIN`).
///
/// Waiters are scoped by (uaddr, address space) — not raw uaddr alone —
/// because every process's anonymous mmap region starts at the same fixed
/// base (see USER_MMAP_BASE), so two unrelated processes can easily end up
/// with numerically identical uaddrs for e.g. mlibc's internal malloc lock.
/// Without this scoping a WAKE in one process could wake a waiter in a
/// completely unrelated one.
pub(super) fn sys_futex(uaddr: u64, futex_op: i32, val: i32, timeout: u64, uaddr2: u64, val3: u32) -> SyscallResult {
    let op = futex_op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
    let realtime = futex_op & FUTEX_CLOCK_REALTIME != 0;
    match op {
        FUTEX_WAIT => futex_wait(uaddr, val, timeout, false, realtime, FUTEX_BITSET_MATCH_ANY),
        FUTEX_WAIT_BITSET => futex_wait(uaddr, val, timeout, true, realtime, val3),
        FUTEX_WAKE => futex_wake(uaddr, val, FUTEX_BITSET_MATCH_ANY),
        FUTEX_WAKE_BITSET => {
            if val3 == 0 {
                return errno::EINVAL;
            }
            futex_wake(uaddr, val, val3)
        }
        FUTEX_REQUEUE | FUTEX_CMP_REQUEUE => futex_requeue(uaddr, val, timeout as i32, uaddr2, if op == FUTEX_CMP_REQUEUE { Some(val3 as i32) } else { None }),
        _ => errno::ENOSYS,
    }
}

/// The `ktime` (ns) a futex timeout names, or `Err` for a bad pointer or timespec.
fn futex_expiry(timeout: u64, absolute: bool, realtime: bool) -> Result<u64, SyscallResult> {
    if validate_user_buffer(timeout, 16).is_err() {
        return Err(errno::EFAULT);
    }
    // SAFETY: validated as a user-space range.
    let (sec, nsec) = unsafe { (*(timeout as *const i64), *((timeout + 8) as *const i64)) };
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return Err(errno::EINVAL);
    }
    let ns = (sec as u64).saturating_mul(1_000_000_000).saturating_add(nsec as u64);
    let up = crate::time::ktime_get();
    Ok(if !absolute {
        up.saturating_add(ns)
    } else if realtime {
        let wall_now = crate::time::now_unix_secs().saturating_mul(1_000_000_000).saturating_add(up % 1_000_000_000);
        up.saturating_add(ns.saturating_sub(wall_now))
    } else {
        ns
    })
}

fn futex_wait(uaddr: u64, val: i32, timeout: u64, absolute: bool, realtime: bool, bitset: u32) -> SyscallResult {
    if validate_user_buffer(uaddr, 4).is_err() { return errno::EFAULT; }
    if bitset == 0 {
        return errno::EINVAL;
    }
    let expiry = if timeout != 0 {
        match futex_expiry(timeout, absolute, realtime) {
            Ok(e) => Some(e),
            Err(e) => return e,
        }
    } else {
        None
    };

    let tf_ptr = current_tf_ptr();

    // `_irq` is deliberately never dropped on the WAIT path — it
    // ends in `jump_to_user` (`-> !`), so interrupts intentionally
    // stay off across that jump; see `sys_read`'s WouldBlock arm
    // for the same reasoning.
    let _irq = crate::process::irq_guard::InterruptGuard::new();

    // Check, register and block as one step with respect to WAKE
    // (stage 6 of `docs/smp/smp-plan.md`). They used to be three
    // steps kept together only by IF=0 on one CPU; with a WAKE on
    // another CPU, a value change + WAKE between the check and the
    // registration finds nobody to wake, and a WAKE between the
    // registration and the block "wakes" a process that is still
    // running — either way the waiter then sleeps forever. Holding
    // `FUTEX_WAITERS` from the check to the block closes the first
    // (WAKE takes it to look waiters up), holding the scheduler
    // lock closes the second (WAKE takes it to wake). Order
    // scheduler → `FUTEX_WAITERS`, as `cancel_all_waiters` (which
    // runs under the scheduler lock on the signal-kill path).
    let next_tf = {
        let mut sched = crate::process::scheduler::local_scheduler();
        let Some(proc) = sched.running_ref() else { return errno::ESRCH };
        let (pid, as_id) = (proc.pid.0, proc.address_space.root_frame().start_address().as_u64());
        let mut waiters = FUTEX_WAITERS.lock();
        // Through the address space, not `*uaddr`: a fault here
        // would reach the kill path, which takes the scheduler lock
        // this code holds. (Order scheduler → `FUTEX_WAITERS` → the
        // address space's lock; nothing takes them the other way.)
        let mut word = [0u8; 4];
        if unsafe { proc.address_space.copy_from_user(uaddr, &mut word) } != 4 {
            return errno::EFAULT;
        }
        if i32::from_ne_bytes(word) != val {
            return errno::EAGAIN;
        }
        // a timeout already past: the value matched, so this is the timeout, not a wait
        if let Some(e) = expiry {
            if e <= crate::time::ktime_get() {
                return errno::ETIMEDOUT;
            }
        }
        let cell = sched.begin_wait();
        waiters.insert(pid, FutexWaiter { uaddr, as_id, bitset, cell: cell.clone() });
        // the timer's wake leaves this in rax; a WAKE sets 0 itself (`wake_with_retval`)
        unsafe { (*(tf_ptr as *mut TrapFrame)).rax = if expiry.is_some() { errno::ETIMEDOUT as u64 } else { 0 }; }
        let cleanup = match expiry {
            Some(e) => crate::process::wait::Cleanup::Timer(crate::time::hrtimer::start(e, crate::time::hrtimer::HrTimerAction::Wake { pid, cell })),
            None => crate::process::wait::Cleanup::None,
        };
        let ret_rip = unsafe { (*tf_ptr).rip };
        let next = sched.block_current(tf_ptr, crate::process::wait::Wait::cell(
            202, ret_rip, crate::process::wait::RestartPolicy::SaRestart, cleanup,
        ));
        drop(waiters);
        next
    };
    unsafe { crate::process::trapframe::jump_to_user(next_tf) }
}

/// Wake up to `max_wake` (`<= 0` means all) waiters of (`uaddr`, this address space) whose mask shares a bit with `bitset`.
/// Returns how many were woken.
fn futex_wake(uaddr: u64, val: i32, bitset: u32) -> SyscallResult {
    let _irq = crate::process::irq_guard::InterruptGuard::new();

    let as_id = {
        let sched = crate::process::scheduler::local_scheduler();
        match sched.running_ref() {
            Some(proc) => proc.address_space.root_frame().start_address().as_u64(),
            None => return errno::ESRCH,
        }
    };

    let max_wake = if val <= 0 { i32::MAX } else { val };
    let mut woken_pids: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
    {
        let mut waiters = FUTEX_WAITERS.lock();
        // A waiter a signal or a timeout ended is dropped without counting:
        // the wakeup goes to one still asleep instead of being lost
        // on one that has gone back to its loop.
        waiters.retain(|&pid, w| {
            if woken_pids.len() as i32 >= max_wake {
                return true;
            }
            if w.uaddr != uaddr || w.as_id != as_id || w.bitset & bitset == 0 {
                return true;
            }
            if w.cell.claim() {
                woken_pids.push(pid);
            }
            false
        });
    }

    if !woken_pids.is_empty() {
        let mut sched = crate::process::scheduler::local_scheduler();
        for &pid in &woken_pids {
            sched.wake_with_retval(pid, 0);
        }
    }
    woken_pids.len() as i64
}

/// FUTEX_REQUEUE / FUTEX_CMP_REQUEUE: wake up to `nr_wake` waiters of `uaddr`, then move up to `nr_requeue` of the rest to
/// `uaddr2` (they keep sleeping; a later WAKE on `uaddr2` finds them). `cmp` is `CMP_REQUEUE`'s expected value of `*uaddr`.
fn futex_requeue(uaddr: u64, nr_wake: i32, nr_requeue: i32, uaddr2: u64, cmp: Option<i32>) -> SyscallResult {
    if nr_wake < 0 || nr_requeue < 0 {
        return errno::EINVAL;
    }
    if validate_user_buffer(uaddr, 4).is_err() || validate_user_buffer(uaddr2, 4).is_err() {
        return errno::EFAULT;
    }
    let _irq = crate::process::irq_guard::InterruptGuard::new();
    let as_id = {
        let sched = crate::process::scheduler::local_scheduler();
        let Some(proc) = sched.running_ref() else { return errno::ESRCH };
        if let Some(expected) = cmp {
            let mut word = [0u8; 4];
            if unsafe { proc.address_space.copy_from_user(uaddr, &mut word) } != 4 {
                return errno::EFAULT;
            }
            if i32::from_ne_bytes(word) != expected {
                return errno::EAGAIN;
            }
        }
        proc.address_space.root_frame().start_address().as_u64()
    };
    let mut woken_pids: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
    let mut requeued = 0i32;
    {
        let mut waiters = FUTEX_WAITERS.lock();
        waiters.retain(|&pid, w| {
            if w.uaddr != uaddr || w.as_id != as_id {
                return true;
            }
            if (woken_pids.len() as i32) < nr_wake {
                if w.cell.claim() {
                    woken_pids.push(pid);
                }
                return false;
            }
            if requeued < nr_requeue {
                w.uaddr = uaddr2;
                requeued += 1;
            }
            true
        });
    }
    if !woken_pids.is_empty() {
        let mut sched = crate::process::scheduler::local_scheduler();
        for &pid in &woken_pids {
            sched.wake_with_retval(pid, 0);
        }
    }
    (woken_pids.len() as i32 + requeued) as i64
}

struct FutexWaiter {
    uaddr: u64,
    as_id: u64,
    /// `FUTEX_WAIT_BITSET`'s mask (`MATCH_ANY` for a plain wait): a `WAKE_BITSET` wakes it only if they share a bit.
    bitset: u32,
    /// The wait's cell (`process::wait`): claimed by FUTEX_WAKE, cancelled
    /// by a signal.
    cell: alloc::sync::Arc<crate::process::wait::WaitCell>,
}

/// One outstanding FUTEX_WAIT per PID — mirrors POLL_WAITERS. Keyed by pid
/// with no bound: it was a `[_; 32]` array, and a pid >= 32 blocked in
/// FUTEX_WAIT without registering, so no FUTEX_WAKE ever found it —
/// `pthread_join` hung for good once pids passed 32 (found by the SMP
/// stage-5 metal job, whose 100-exec warm-up made the first thread pid 104).
static FUTEX_WAITERS: Mutex<BTreeMap<usize, FutexWaiter>> = Mutex::new(BTreeMap::new());

/// Clear a pending futex wait for a process (called on exit).
pub(super) fn futex_cancel_waiter(pid: usize) {
    FUTEX_WAITERS.lock().remove(&pid);
}

