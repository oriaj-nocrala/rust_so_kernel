// kernel/src/process/syscall/sync.rs
//
// futex(202) — wait/wake, backs mlibc mutexes/condvars.

use crate::sync::Mutex;
use crate::process::TrapFrame;
use super::{errno, SyscallResult, validate_user_buffer, current_tf_ptr};
use alloc::collections::BTreeMap;

// ── futex(202) ─────────────────────────────────────────────────────────────

/// futex(202): long futex(uint32_t *uaddr, int futex_op, uint32_t val,
///                        const struct timespec *timeout, ...)
///
/// WAIT blocks the caller if `*uaddr == val` until a matching WAKE (timeouts
/// are not supported — `_timeout` is ignored, matching the previous stub).
/// WAKE wakes up to `val` waiters registered on the same `uaddr`.
///
/// Waiters are scoped by (uaddr, address space) — not raw uaddr alone —
/// because every process's anonymous mmap region starts at the same fixed
/// base (see USER_MMAP_BASE), so two unrelated processes can easily end up
/// with numerically identical uaddrs for e.g. mlibc's internal malloc lock.
/// Without this scoping a WAKE in one process could wake a waiter in a
/// completely unrelated one. There is no real thread-sharing yet (sys_clone
/// is ENOSYS), so today this is one-waiter-per-address-space in practice,
/// but the scoping is what makes it correct once real threads land.
pub(super) fn sys_futex(uaddr: u64, futex_op: i32, val: i32, _timeout: u64) -> SyscallResult {
    const FUTEX_WAIT: i32 = 0;
    const FUTEX_WAKE: i32 = 1;
    const FUTEX_PRIVATE_FLAG: i32 = 128;

    let op = futex_op & !FUTEX_PRIVATE_FLAG;

    match op {
        FUTEX_WAIT => {
            if validate_user_buffer(uaddr, 4).is_err() { return errno::EFAULT; }

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
                waiters.insert(pid, FutexWaiter { uaddr, as_id });
                unsafe { (*(tf_ptr as *mut TrapFrame)).rax = 0; }
                let next = sched.block_current(tf_ptr);
                drop(waiters);
                next
            };
            unsafe { crate::process::trapframe::jump_to_user(next_tf) }
        }
        FUTEX_WAKE => {
            let _irq = crate::process::irq_guard::InterruptGuard::new();

            let as_id = {
                let sched = crate::process::scheduler::local_scheduler();
                match sched.running_ref() {
                    Some(proc) => proc.address_space.root_frame().start_address().as_u64(),
                    None => return errno::ESRCH,
                }
            };

            let max_wake = if val <= 0 { i32::MAX } else { val };
            let mut woken_pids = [0usize; 8];
            let mut woken_count = 0usize;
            {
                let mut waiters = FUTEX_WAITERS.lock();
                for (&pid, w) in waiters.iter() {
                    if woken_count >= woken_pids.len() || woken_count as i32 >= max_wake {
                        break;
                    }
                    if w.uaddr == uaddr && w.as_id == as_id {
                        woken_pids[woken_count] = pid;
                        woken_count += 1;
                    }
                }
                for pid in &woken_pids[..woken_count] {
                    waiters.remove(pid);
                }
            }

            if woken_count > 0 {
                let mut sched = crate::process::scheduler::local_scheduler();
                for &pid in &woken_pids[..woken_count] {
                    sched.wake_with_retval(pid, 0);
                }
            }
            woken_count as i64
        }
        _ => errno::ENOSYS,
    }
}

#[derive(Clone, Copy)]
struct FutexWaiter {
    uaddr: u64,
    as_id: u64,
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

