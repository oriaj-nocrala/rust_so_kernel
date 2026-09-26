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

/// Timer interrupts taken since boot. `/proc/kdebug` shows it beside the
/// uptime, which is how the tick rate is checked on a machine with no
/// serial: it should read 100 per second of uptime, whichever timer drives it.
static TICK_COUNT: AtomicU64 = AtomicU64::new(0);

pub fn ticks_total() -> u64 {
    TICK_COUNT.load(Ordering::Relaxed)
}

/// What the timer and reschedule-IPI handlers return to their asm stubs,
/// in RAX:RDX: the frame to resume, and this CPU's `scheduler::LEAVING`
/// slot, cleared by the stub the moment RSP has left the old stack.
#[repr(C)]
pub struct Resume {
    tf: *const TrapFrame,
    leaving: *mut u64,
}

impl Resume {
    fn to(tf: *const TrapFrame) -> Self {
        Self { tf, leaving: super::scheduler::leaving_slot() }
    }
}

/// An interrupt entry that can switch processes: save every GPR on the
/// interrupted stack, call `$handler(frame)`, then resume whichever frame it
/// returned.
macro_rules! switching_entry {
    ($entry:literal, $handler:literal) => {
        global_asm!(
            concat!(".global ", $entry),
            concat!($entry, ":"),

            // The direction flag (DF) is NOT cleared by interrupt delivery: a
            // tick landing between a memmove's `std` and its `cld` would
            // otherwise run the whole ISR (and its memcpys — including the
            // trapframe box copy) with DF=1, copying BACKWARD. That was the
            // root cause of months of intermittent hangs and heap-jump faults;
            // see docs/hang-hunt-bug2-findings.md. The rustc x86-interrupt
            // shims emit `cld` for the IDT handlers; this hand-written asm
            // must too.
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
            concat!("call ", $handler),

            // Handler returns (new TrapFrame, LEAVING slot) in RAX:RDX.
            // Switch RSP to the new TrapFrame (may be same or different
            // process), then release the stack we were on.
            "mov rsp, rax",
            "mov qword ptr [rdx], 0",

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
    };
}

switching_entry!("timer_interrupt_entry", "timer_preempt_handler");
switching_entry!("resched_interrupt_entry", "resched_ipi_handler");

extern "C" {
    pub fn timer_interrupt_entry();
    pub fn resched_interrupt_entry();
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
pub extern "C" fn timer_preempt_handler(current_tf: *const TrapFrame) -> Resume {
    // ── 1. EOI (must be first — acknowledge interrupt) ────────────────
    // The LAPIC timer's, or the PIT's through the 8259 if the APIC switch
    // declined (`interrupts::apic::init`) — both on vector 32.
    crate::interrupts::eoi(crate::interrupts::apic::TIMER_VECTOR);

    // Per-CPU GS invariant (`cpu/percpu.rs`): two rdmsrs, 100 Hz.
    crate::cpu::percpu::check_gs_invariant();

    // Per-CPU work: this CPU's APERF/MPERF (`cpu/freq.rs`), two rdmsrs.
    crate::cpu::freq::tick();
    // Per-CPU C0 residency; package energy on CPU 0 (`cpu/idle.rs`).
    crate::cpu::idle::tick();

    // ── 2. Global work: CPU 0 only ────────────────────────────────────
    // Every CPU that schedules gets this tick (stage 7 of
    // docs/smp/smp-plan.md); what is not per-CPU runs once per period, on
    // the BSP (decision 3): the cursor, the USB poll, `TICK_COUNT` (the
    // 100 Hz check of `/proc/kdebug`) and the hrtimers.
    let bsp = crate::cpu::cpu_id() == 0;
    let mut wake_pids = [(0usize, 0u32); 8];
    let mut wake_count = 0;
    if bsp {
        crate::drivers::framebuffer_console::tick_cursor_blink();

        // The xHCI driver has no interrupt of its own (see `usb/mod.rs`), so
        // its event ring is drained here, at 100 Hz. Cheap in the common
        // case: one uncached read of a TRB's cycle bit per controller. Runs
        // before the scheduler lock is taken below — `poll` feeds decoded
        // keys through `tty::feed_input`, which can take that same lock to
        // deliver SIGINT, and a spin lock is not reentrant.
        crate::usb::poll();

        TICK_COUNT.fetch_add(1, Ordering::Relaxed);

        // tick() acquires QUEUE, drains expired timers, releases QUEUE, then
        // returns a list of PIDs to wake. QUEUE is always released before we
        // acquire the scheduler lock below (ABBA-deadlock prevention).
        let now_ns = crate::time::ktime_get();
        wake_count = crate::time::hrtimer::tick(now_ns, &mut wake_pids);

        // A timed-out poll/epoll waiter is removed *before* its process is
        // woken, not after: once woken it can run on another CPU at once and
        // register a new waiter under the same pid, which a clear after the
        // wake would then delete (the process would sleep forever). Lock
        // order POLL_WAITERS → SCHEDULER is kept either way: not nested.
        for &(pid, timer) in &wake_pids[..wake_count] {
            crate::process::syscall::poll_clear_on_timeout(pid, timer);
        }
    }

    // ── 3. Scheduler: wake hrtimer PIDs + tick time slice ────────────
    let mut scheduler = super::scheduler::local_scheduler();

    for &(pid, _) in &wake_pids[..wake_count] {
        // ktrace, not serial_println!: once per sleep (a game sleeps
        // every frame) flooded the klog ring, and taking the SERIAL
        // lock from the timer ISR can wait forever on the code it
        // interrupted.
        crate::ktrace!(crate::debug::SCHED, "hrtimer waking PID {}", pid);
        scheduler.wake(pid);
    }

    // Not scheduling yet (boot, before `start_first_process`; the QEMU
    // integration tests): there is nothing to preempt.
    if scheduler.running_ref().is_none() {
        return Resume::to(current_tf);
    }

    // RPL 3 in the saved CS: the tick interrupted user mode.
    let user_mode = unsafe { (*current_tf).cs } & 3 == 3;
    if !scheduler.tick(unsafe { (*current_tf).rsp }, user_mode) {
        // Slice still has ticks remaining — continue current process,
        // but it may have just been sent a signal (e.g. by another
        // process's kill() while this one was running) — check before
        // resuming it.
        let tf = scheduler.resolve_signals(current_tf);
        scheduler.resolve_wait_status();
        let kstack_top = scheduler.running_ref().map(|p| p.kernel_stack.as_u64()).unwrap_or(0);
        drop(scheduler);
        validate_resume_frame(tf, kstack_top, "timer-no-switch");
        return Resume::to(tf);
    }

    // ── 4. Time slice exhausted (or idle with work) — context switch ──
    let tf = switch_and_resolve(&mut scheduler, current_tf);
    let kstack_top = scheduler.running_ref().map(|p| p.kernel_stack.as_u64()).unwrap_or(0);
    drop(scheduler);
    validate_resume_frame(tf, kstack_top, "timer-switch");
    Resume::to(tf)
}

/// `switch_to_next`, then everything owed to a process about to return to
/// user mode: pending signals, and a reaped child's wait status (see
/// `Scheduler::notify_child_death`) — this is the scheduler's main "what
/// runs next" decision point, and a process woken from a blocked waitpid()
/// is most commonly picked up right here.
fn switch_and_resolve(
    scheduler: &mut super::scheduler::TrackedSchedulerGuard,
    current_tf: *const TrapFrame,
) -> *const TrapFrame {
    let tf = scheduler.switch_to_next(current_tf);
    let tf = scheduler.resolve_signals(tf);
    scheduler.resolve_wait_status();
    tf
}

/// The reschedule IPI (`scheduler::RESCHED_VECTOR`): another CPU made work
/// Ready while this one idles. Switch now instead of at the next tick.
/// Arriving anywhere else — the CPU stopped idling between the send and the
/// delivery — it is a no-op.
#[no_mangle]
pub extern "C" fn resched_ipi_handler(current_tf: *const TrapFrame) -> Resume {
    crate::interrupts::eoi(super::scheduler::RESCHED_VECTOR);
    let mut scheduler = super::scheduler::local_scheduler();
    if !scheduler.resched_due() {
        return Resume::to(current_tf);
    }
    let tf = switch_and_resolve(&mut scheduler, current_tf);
    let kstack_top = scheduler.running_ref().map(|p| p.kernel_stack.as_u64()).unwrap_or(0);
    drop(scheduler);
    validate_resume_frame(tf, kstack_top, "resched-ipi");
    Resume::to(tf)
}
