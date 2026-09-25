// kernel/src/process/trapframe.rs
// TrapFrame con función para saltar al primer proceso

use core::arch::global_asm;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct TrapFrame {
    // Registros de propósito general (pushados por nuestro código)
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    
    // IRETQ frame (pushado por hardware)
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

// Jump to a TrapFrame: restore every register and `iretq`.
//
// RDI = the frame, RSI = this CPU's `scheduler::LEAVING` slot. The slot is
// cleared right after RSP leaves the stack this code was running on — from
// that instruction on, nothing touches that stack again, so another CPU may
// resume the process that owns it (stage 7 of docs/smp/smp-plan.md).
global_asm!(
    ".global jump_to_trapframe_raw",
    "jump_to_trapframe_raw:",
    
    "mov rsp, rdi",  // Apuntar RSP al TrapFrame
    "mov qword ptr [rsi], 0",
    
    // Restaurar registros generales
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
    
    // IRETQ lee: RIP, CS, RFLAGS, RSP, SS del stack
    "iretq",
);

extern "C" {
    fn jump_to_trapframe_raw(tf: *const TrapFrame, leaving: *mut u64) -> !;
}

/// Restore every register from `tf` and `iretq` into it, releasing the
/// kernel stack this CPU was on (`scheduler::LEAVING`) on the way.
pub unsafe fn jump_to_trapframe(tf: *const TrapFrame) -> ! {
    unsafe { jump_to_trapframe_raw(tf, super::scheduler::leaving_slot()) }
}

/// Every "about to iretq into a process" call site in this kernel should
/// call this instead of `jump_to_trapframe` directly (the one exception is
/// `start_first_process`, which runs before any process could possibly
/// have a pending signal). Delivers pending signals via
/// `Scheduler::resolve_signals` — see its doc comment — then jumps.
///
/// # Safety
/// `tf` must point at the TrapFrame of whichever process is currently
/// `Scheduler::running` on this CPU — true at every existing call site,
/// since it's always the direct return value of `switch_to_next`,
/// `block_current`, `kill_and_switch_tf`, or `start_first`.
pub unsafe fn jump_to_user(tf: *const TrapFrame) -> ! {
    unsafe { core::arch::asm!("cli"); }
    let tf = {
        let mut sched = super::scheduler::local_scheduler();
        let tf = sched.resolve_signals(tf);
        // Flush a pending reaped-child wait status into this process's own
        // memory now that its address space is (already) active — see
        // `Scheduler::resolve_wait_status`'s doc comment for why this can't
        // happen any earlier, from the child's own exit path.
        sched.resolve_wait_status();
        tf
    };
    unsafe { jump_to_trapframe(tf) }
}