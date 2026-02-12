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
use super::trapframe::TrapFrame;

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

    // ── 2. Tick the scheduler ─────────────────────────────────────────
    //
    // tick() decrements the running process's time slice and handles
    // periodic aging.  Returns true if the slice is exhausted and a
    // context switch is needed.
    let mut scheduler = super::scheduler::SCHEDULER.lock();

    if !scheduler.tick() {
        // Slice still has ticks remaining — continue current process
        return current_tf;
    }

    // ── 3. Time slice exhausted — context switch ──────────────────────
    scheduler.switch_to_next(current_tf)
}