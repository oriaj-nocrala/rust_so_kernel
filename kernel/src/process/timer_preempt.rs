// kernel/src/process/timer_preempt.rs
//
// Timer interrupt handler with time-slice-based preemption.
//
// PREVIOUS DESIGN:
//   Context switch every N ticks (modulo counter).  No concept of
//   time slices — just a fixed throttle.
//
// CURRENT DESIGN:
//   Every tick: send EOI, call scheduler.tick() which decrements the
//   running process's remaining time slice and handles aging.
//   When tick() returns true (slice exhausted): do full context switch.
//   Otherwise: return immediately (same process continues).

use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};
use super::trapframe::TrapFrame;

static TICK_COUNT: AtomicU64 = AtomicU64::new(0);

global_asm!(
    ".global timer_interrupt_entry",
    "timer_interrupt_entry:",
    
    // Save ALL registers
    "push rax",
    "push rbx",
    "push rcx",
    "push rdx",
    "push rsi",
    "push rdi",
    "push rbp",
    "push r8",
    "push r9",
    "push r10",
    "push r11",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    
    // Call handler with pointer to current TrapFrame
    "mov rdi, rsp",
    "call timer_preempt_handler",
    
    // Handler returns new TrapFrame pointer in RAX
    // Switch RSP to new TrapFrame (may be same or different process)
    "mov rsp, rax",
    
    // Restore registers from the (possibly new) process
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop r11",
    "pop r10",
    "pop r9",
    "pop r8",
    "pop rbp",
    "pop rdi",
    "pop rsi",
    "pop rdx",
    "pop rcx",
    "pop rbx",
    "pop rax",
    
    // IRETQ to the (possibly new) process
    "iretq",
);

extern "C" {
    pub fn timer_interrupt_entry();
}

#[no_mangle]
pub extern "C" fn timer_preempt_handler(current_tf: *const TrapFrame) -> *const TrapFrame {
    // ── 1. EOI (must be first — acknowledge interrupt) ────────────────
    unsafe {
        use x86_64::instructions::port::PortWriteOnly;
        PortWriteOnly::<u8>::new(0x20).write(0x20);
    }

    // ── 2. Advance jiffies counter ────────────────────────────────────
    // crate::time::clockevent::tick();

    // let tick_n = TICK_COUNT.fetch_add(1, Ordering::Relaxed);
    // if tick_n % 50 == 0 {
    //     crate::serial_println!("[TICK] {}", tick_n);
    // }

    // ── 3. Fire expired hrtimers ──────────────────────────────────────
    //
    // tick() acquires QUEUE, drains expired timers, releases QUEUE, then
    // returns a list of PIDs to wake.  QUEUE is always released before we
    // acquire the scheduler lock below (ABBA-deadlock prevention).
    let mut wake_pids = [0usize; 8];
    let wake_count = {
        let now_ns = crate::time::ktime_get();
        crate::time::hrtimer::tick(now_ns, &mut wake_pids)
    };

    // ── 4. Scheduler: wake hrtimer PIDs + tick time slice ────────────
    //
    // Acquire scheduler lock once for all wakeups + the tick decision.
    // Release it before clearing POLL_WAITERS to obey lock order:
    //   POLL_WAITERS → SCHEDULER (never the reverse).
    let next_tf = {
        let mut scheduler = super::scheduler::local_scheduler();

        for &pid in &wake_pids[..wake_count] {
            crate::serial_println!("[ISR] hrtimer waking PID {}", pid);
            scheduler.wake(pid);
        }

        if !scheduler.tick() {
            // Slice still has ticks remaining — continue current process,
            // but it may have just been sent a signal (e.g. by another
            // process's kill() while this one was running) — check before
            // resuming it. Still clear poll waiters for any pids woken by
            // hrtimer either way.
            let tf = scheduler.resolve_signals(current_tf);
            drop(scheduler);
            for &pid in &wake_pids[..wake_count] {
                crate::process::syscall::poll_clear_on_timeout(pid);
            }
            return tf;
        }

        // ── 5. Time slice exhausted — context switch ──────────────────
        let tf = scheduler.switch_to_next(current_tf);
        scheduler.resolve_signals(tf)
        // scheduler lock released here
    };

    // Clear stale POLL_WAITERS slots for PIDs woken by hrtimer timeout.
    // Must happen after the scheduler lock is released (lock-order rule).
    for &pid in &wake_pids[..wake_count] {
        crate::process::syscall::poll_clear_on_timeout(pid);
    }

    next_tf
}