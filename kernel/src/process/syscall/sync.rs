// kernel/src/process/syscall/sync.rs
//
// futex(202) — wait/wake, backs mlibc mutexes/condvars.

use spin::Mutex;
use core::sync::atomic::Ordering;
use crate::process::TrapFrame;
use super::{errno, SyscallResult, validate_user_buffer, CURRENT_SYSCALL_TF};
use super::ipc::MAX_PROCS;

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
            let current = unsafe { *(uaddr as *const i32) };
            if current != val {
                return errno::EAGAIN;
            }

            let tf_ptr = CURRENT_SYSCALL_TF.load(Ordering::Relaxed) as *const TrapFrame;

            // `_irq` is deliberately never dropped on the WAIT path — it
            // ends in `jump_to_user` (`-> !`), so interrupts intentionally
            // stay off across that jump; see `sys_read`'s WouldBlock arm
            // for the same reasoning.
            let _irq = crate::process::irq_guard::InterruptGuard::new();

            let (pid, as_id) = {
                let sched = crate::process::scheduler::local_scheduler();
                match sched.running_ref() {
                    Some(proc) => (proc.pid.0, proc.address_space.root_frame().start_address().as_u64()),
                    None => return errno::ESRCH,
                }
            };

            if pid < MAX_PROCS {
                FUTEX_WAITERS.lock()[pid] = Some(FutexWaiter { uaddr, as_id });
            }

            let next_tf = {
                let mut scheduler = crate::process::scheduler::local_scheduler();
                unsafe { (*(tf_ptr as *mut TrapFrame)).rax = 0; }
                scheduler.block_current(tf_ptr)
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
                for (pid, slot) in waiters.iter_mut().enumerate() {
                    if woken_count >= woken_pids.len() || woken_count as i32 >= max_wake {
                        break;
                    }
                    if let Some(w) = slot {
                        if w.uaddr == uaddr && w.as_id == as_id {
                            woken_pids[woken_count] = pid;
                            woken_count += 1;
                            *slot = None;
                        }
                    }
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

/// One outstanding FUTEX_WAIT per PID — mirrors POLL_WAITERS/RECV_WAITER.
static FUTEX_WAITERS: Mutex<[Option<FutexWaiter>; MAX_PROCS]> = Mutex::new([None; MAX_PROCS]);

/// Clear a pending futex wait for a process (called on exit).
pub(super) fn futex_cancel_waiter(pid: usize) {
    if pid < MAX_PROCS {
        FUTEX_WAITERS.lock()[pid] = None;
    }
}

