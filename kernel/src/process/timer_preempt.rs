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
    
    // The direction flag (DF) is NOT cleared by interrupt delivery: a tick
    // landing between a memmove's `std` and its `cld` would otherwise run the
    // whole ISR (and its memcpys — including the trapframe box copy) with
    // DF=1, copying BACKWARD. That was the root cause of months of
    // intermittent hangs and heap-jump faults; see
    // docs/hang-hunt-bug2-findings.md. The rustc x86-interrupt shims emit
    // `cld` for the IDT handlers; this hand-written asm must too.
    "cld",
    
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

/// Validate a TrapFrame that `timer_interrupt_entry`'s asm is about to iretq
/// from. Cheap permanent safety net kept from the 2026-08-05 hang hunt, where
/// the failure mode was exactly this: a corrupted boxed trapframe gets
/// restored and the iretq jumps into the heap (instruction fetch inside the
/// /tmp entries BTreeMap), destroying the CPU state that would have explained
/// it. If the frame is corrupt, panic immediately with full context —
/// freezing the seed instant is worth far more than surviving it. The asm is
/// deliberately untouched; this runs in Rust on the pointer
/// `switch_to_next`/`resolve_signals` returned, on BOTH the switch resume and
/// the no-switch return.
///
/// O(1), no allocation: pure comparisons against values already in hand
/// (`kstack_top` is the running process's kernel stack top, read once). The
/// code-region boundary uses the real runtime `physical_memory_offset()`
/// (kernel code is never mapped at/above it — that region is the physical-map
/// heap), not a hardcoded .text range.
fn validate_resume_frame(tf: *const TrapFrame, kstack_top: u64, site: &'static str) {
    const USER_CS: u64 = 0x23;
    const KERNEL_CS: u64 = 0x08;
    const KERNEL_SS: u64 = 0x10;
    const USER_SS: u64 = 0x1b;
    const USER_SPACE_MAX: u64 = 0x0000_8000_0000_0000;

    let frame = unsafe { &*tf };
    let (cs, ss, rip, rflags, rsp) = (frame.cs, frame.ss, frame.rip, frame.rflags, frame.rsp);

    // RFLAGS bit 1 is architecturally reserved and always 1.
    let bad_rflags = rflags & 0x2 == 0;

    let phys_offset = crate::memory::physical_memory_offset().as_u64();
    let kstack_lo = kstack_top - (1u64 << crate::init::processes::KERNEL_STACK_ORDER);

    let bad = match cs {
        // Ring-0 frame: kernel code is below the physical-map offset and
        // above the null page. The interrupted stack pointer must be inside
        // this process's own kernel stack (kstacks live at/above the
        // physical-map offset) — with ONE legitimate exception: the boot
        // transition (the first tick right after `start_first_process`'s
        // `sti`) still runs on the bootloader's boot stack, which is BELOW
        // the physical-map offset (e.g. 0x18000014df0 in these boots). So:
        // tiny rsp (< 0x100000) or heap rsp (>= phys_offset and outside this
        // kstack) is corrupt; the [0x100000, phys_offset) band is the boot
        // stack and is allowed.
        KERNEL_CS => {
            ss != KERNEL_SS
                || rip >= phys_offset
                || rip < 0x1000
                || rsp < 0x100000
                || (rsp >= phys_offset && (rsp < kstack_lo || rsp >= kstack_top))
        }
        // Ring-3 frame: both RIP and RSP must be in user space.
        USER_CS => ss != USER_SS || rip >= USER_SPACE_MAX || rsp >= USER_SPACE_MAX,
        // Anything else (the panic's cs=0x3) is invalid on its face.
        _ => true,
    } || bad_rflags;

    if bad {
        crate::serial_println_raw!(
            "\n=== HANGHUNT BAD RESUME FRAME ===\n  tf={:#x} kstack=[{:#x},{:#x})\n  cs={:#x} ss={:#x} rip={:#x} rflags={:#x} rsp={:#x}\n  pid={}",
            tf as u64, kstack_lo, kstack_top,
            cs, ss, rip, rflags, rsp,
            super::scheduler::current_pid_fast(),
        );
        panic!("corrupt resume frame at {} (bad iretq target)", site);
    }
}

#[no_mangle]
pub extern "C" fn timer_preempt_handler(current_tf: *const TrapFrame) -> *const TrapFrame {
    // ── 1. EOI (must be first — acknowledge interrupt) ────────────────
    unsafe {
        use x86_64::instructions::port::PortWriteOnly;
        PortWriteOnly::<u8>::new(0x20).write(0x20);
    }

    crate::drivers::framebuffer_console::tick_cursor_blink();

    // ── 1b. USB keyboard input ────────────────────────────────────────
    // The xHCI driver has no interrupt of its own (see `usb/mod.rs`), so
    // its event ring is drained here, at 100 Hz. Cheap in the common case:
    // one uncached read of a TRB's cycle bit per controller. Runs before
    // the scheduler lock is taken below — `poll` feeds decoded keys
    // through `tty::feed_input`, which can take that same lock to deliver
    // SIGINT, and a spin lock is not reentrant.
    crate::usb::poll();

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
            // ktrace, not serial_println!: once per sleep (a game sleeps
            // every frame) flooded the klog ring, and taking the SERIAL
            // lock from the timer ISR can wait forever on the code it
            // interrupted.
            crate::ktrace!(crate::debug::SCHED, "hrtimer waking PID {}", pid);
            scheduler.wake(pid);
        }

        if !scheduler.tick(unsafe { (*current_tf).rsp }) {
            // Slice still has ticks remaining — continue current process,
            // but it may have just been sent a signal (e.g. by another
            // process's kill() while this one was running) — check before
            // resuming it. Still clear poll waiters for any pids woken by
            // hrtimer either way.
            let tf = scheduler.resolve_signals(current_tf);
            scheduler.resolve_wait_status();
            let kstack_top = scheduler.running_ref().map(|p| p.kernel_stack.as_u64()).unwrap_or(0);
            drop(scheduler);
            for &pid in &wake_pids[..wake_count] {
                crate::process::syscall::poll_clear_on_timeout(pid);
            }
            validate_resume_frame(tf, kstack_top, "timer-no-switch");
            return tf;
        }

        // ── 5. Time slice exhausted — context switch ──────────────────
        let tf = scheduler.switch_to_next(current_tf);
        let tf = scheduler.resolve_signals(tf);
        // A process woken from a blocked waitpid() (see `Scheduler::
        // notify_child_death`) most commonly gets picked up right here —
        // this is the scheduler's main "what runs next" decision point,
        // called on every exhausted time slice. Must flush its pending
        // status now, same as every other "about to return to user mode"
        // site (`trapframe::jump_to_user`, the syscall-return epilogue).
        scheduler.resolve_wait_status();
        let kstack_top = scheduler.running_ref().map(|p| p.kernel_stack.as_u64()).unwrap_or(0);
        validate_resume_frame(tf, kstack_top, "timer-switch");
        tf
        // scheduler lock released here
    };

    // Clear stale POLL_WAITERS slots for PIDs woken by hrtimer timeout.
    // Must happen after the scheduler lock is released (lock-order rule).
    for &pid in &wake_pids[..wake_count] {
        crate::process::syscall::poll_clear_on_timeout(pid);
    }

    next_tf
}