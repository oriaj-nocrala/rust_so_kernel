// kernel/src/init/devices.rs
//
// IDT construction, interrupt handlers, PIC/PIT init, boot screen.
//
// The page fault handler lives here because it bridges memory and
// process layers.  User-mode segfaults kill the process; only
// kernel-mode faults panic.
//
// HISTORY:
//   - kill_current_user_process now performs a FULL context switch
//     via jump_to_trapframe (restores all GPRs + iretq).
//     Previously it only overwrote the 5-field ExceptionStackFrame,
//     leaking RAX..R15 from the killed process into the next one.

use spin::Once;

use crate::{
    framebuffer::{self, Color},
    interrupts::{
        exception::ExceptionStackFrame,
        idt::InterruptDescriptorTable,
    },
    keyboard,
    serial_println,
};

// ============================================================================
// IDT
// ============================================================================

static IDT: Once<InterruptDescriptorTable> = Once::new();

pub fn init_idt() {
    IDT.call_once(|| {
        let mut idt = InterruptDescriptorTable::new();
        idt.add_handler(0, divide_by_zero_handler);
        idt.add_handler(6, invalid_opcode_handler);
        // IST index is 1-based in the IDT entry.  TSS defines
        // DOUBLE_FAULT_IST_INDEX = 0 (array index), so CPU IST = 0 + 1 = 1.
        idt.add_double_fault_handler(
            8,
            double_fault_handler,
            (crate::process::tss::DOUBLE_FAULT_IST_INDEX + 1) as u16,
        );
        idt.add_handler_with_error(13, general_protection_fault_handler);
        idt.add_handler_with_error(14, page_fault_handler);
        idt.entries[32].set_handler_addr(crate::process::timer_preempt::timer_interrupt_entry as u64);
        idt.add_handler(33, keyboard_interrupt_handler);
        idt.add_handler(36, serial_interrupt_handler);
        // Syscalls are now handled via the `syscall` instruction (LSTAR MSR),
        // not via int 0x80.  No IDT entry needed.
        idt
    });
}

fn load_idt() {
    IDT.get().unwrap().load();
}

// ============================================================================
// Page fault error code bits
// ============================================================================

const PF_PRESENT:  u64 = 1 << 0;   // 1 = protection violation, 0 = not present
const PF_WRITE:    u64 = 1 << 1;   // 1 = write fault
const PF_USER:     u64 = 1 << 2;   // 1 = user mode
const PF_RESERVED: u64 = 1 << 3;   // 1 = reserved PTE bit set

// ============================================================================
// INTERRUPT HANDLERS
// ============================================================================

extern "x86-interrupt" fn keyboard_interrupt_handler(_: &mut ExceptionStackFrame) {
    let scancode = unsafe {
        x86_64::instructions::port::PortReadOnly::<u8>::new(0x60).read()
    };
    keyboard::process_scancode(scancode);
    // Wake any process blocked on stdin read.
    crate::process::syscall::stdin_wakeup();
    // Wake any process blocked in poll/epoll_wait watching stdin for POLLIN.
    crate::process::syscall::poll_wakeup_for_fd0();
    crate::interrupts::pic::end_of_interrupt(crate::interrupts::pic::Irq::Keyboard.as_u8());
}

/// COM1 receive interrupt — lets serial input act as stdin, alongside the
/// PS/2 keyboard.  Bytes are pushed into the same ring buffer the keyboard
/// ISR feeds (`keyboard_buffer::KEYBOARD_BUFFER`) and the same wakeup path
/// is used, so fd 0 (hardcoded to that buffer in `sys_read`) doesn't care
/// which physical source a byte came from.  This is what lets `qemu
/// -serial stdio` be used to type/pipe input into the shell instead of the
/// QEMU-monitor `sendkey` workaround.
extern "x86-interrupt" fn serial_interrupt_handler(_: &mut ExceptionStackFrame) {
    use x86_64::instructions::port::Port;
    const LSR: u16 = 0x3FD;
    const RBR: u16 = 0x3F8;
    const DATA_READY: u8 = 0x01;

    unsafe {
        let mut lsr: Port<u8> = Port::new(LSR);
        let mut rbr: Port<u8> = Port::new(RBR);
        // The 16550 FIFO may hold several bytes by the time we get to run.
        while lsr.read() & DATA_READY != 0 {
            let byte = rbr.read();
            // Same ISIG line discipline the PS/2 path goes through (see
            // `keyboard::push`/`tty::feed_input`) — a byte consumed as a
            // signal (Ctrl-C over `-serial stdio`, say) never becomes input,
            // so skip the wakeups too: there's nothing new for a stdin
            // reader to consume.
            if crate::tty::feed_input(byte as char) {
                crate::keyboard_buffer::KEYBOARD_BUFFER.push(byte as char);
                crate::process::syscall::stdin_wakeup();
                crate::process::syscall::poll_wakeup_for_fd0();
            }
        }
    }
    crate::interrupts::pic::end_of_interrupt(crate::interrupts::pic::Irq::Com1.as_u8());
}

extern "x86-interrupt" fn divide_by_zero_handler(sf: &mut ExceptionStackFrame) {
    if sf.code_segment & 0x3 != 0 {
        kill_current_user_process("DIVIDE BY ZERO");
        // unreachable — kill_current_user_process diverges
    }
    panic!("DIVIDE BY ZERO at {:#x}", sf.instruction_pointer);
}

extern "x86-interrupt" fn invalid_opcode_handler(sf: &mut ExceptionStackFrame) {
    if sf.code_segment & 0x3 != 0 {
        kill_current_user_process("INVALID OPCODE");
        // unreachable — kill_current_user_process diverges
    }
    panic!("INVALID OPCODE at {:#x}", sf.instruction_pointer);
}

extern "x86-interrupt" fn double_fault_handler(
    sf: &mut ExceptionStackFrame,
    error_code: u64
) -> ! {
    panic!("DOUBLE FAULT (error: {}) at {:#x}", error_code, sf.instruction_pointer);
}

extern "x86-interrupt" fn general_protection_fault_handler(
    sf: &mut ExceptionStackFrame,
    error_code: u64
) {
    if sf.code_segment & 0x3 != 0 {
        kill_current_user_process("GENERAL PROTECTION FAULT");
        // unreachable — kill_current_user_process diverges
    }
    panic!("GENERAL PROTECTION FAULT (error: {}) at {:#x}", error_code, sf.instruction_pointer);
}

/// Page fault handler — bridges memory and process layers.
///
/// Flow:
///   1. Pre-filter via demand_paging::is_demand_pageable
///   2. VMA lookup via scheduler
///   3. Map page via demand_paging::map_demand_page
///   4. On failure: kill user process OR panic (kernel fault)
extern "x86-interrupt" fn page_fault_handler(
    sf: &mut ExceptionStackFrame,
    error_code: u64
) {
    use crate::memory::demand_paging;

    let fault_addr = demand_paging::read_cr2();
    let is_user = error_code & PF_USER != 0;
    let is_write = error_code & PF_WRITE != 0;

    let _ = sf; // ExceptionStackFrame values unreliable for user-mode PFs

    // ── COW write fault: page present + user write, no reserved bit ───
    //
    // This must be checked BEFORE is_demand_pageable, which returns Err
    // for present pages (treating them as protection violations).
    //
    // Uses the lock-free fast path: find_vma_fast + current_as_fast.
    // Safe because the fault handler runs with IF=0 (no preemption).
    if (error_code & (PF_PRESENT | PF_WRITE | PF_USER)) == (PF_PRESENT | PF_WRITE | PF_USER)
        && (error_code & PF_RESERVED) == 0
    {
        let handled = unsafe {
            if let Some((_, vma)) = crate::process::scheduler::find_vma_fast(fault_addr) {
                let vma_flags = vma.page_table_flags();
                crate::process::scheduler::current_as_fast()
                    .map(|as_| as_.handle_cow_fault(fault_addr, vma_flags))
                    .unwrap_or(Err("no AS"))
                    .is_ok()
            } else {
                false
            }
        };

        if handled {
            return;
        }

        serial_println!(
            "⚠️  COW fault failed at {:#x} (error {:#b})",
            fault_addr, error_code
        );
        kill_current_user_process("COW FAULT FAILED");
        // unreachable
    }

    // Step 1: Is this fault potentially demand-pageable?
    if let Err(reason) = demand_paging::is_demand_pageable(error_code) {
        if is_user {
            serial_println!(
                "⚠️  User page fault at {:#x} (error {:#b}): {}",
                fault_addr, error_code, reason
            );
            kill_current_user_process("PAGE FAULT (not demand-pageable)");
            // unreachable — kill_current_user_process diverges
        }
        let (cr3, _) = x86_64::registers::control::Cr3::read();
        panic!(
            "PAGE FAULT (kernel)\n  Address: {:#x}\n  Error: {:#b}\n  Reason: {}\n  RIP: {:#x}\n  CS: {:#x}\n  RSP: {:#x}\n  CR3: {:#x}\n  running PID: {}",
            fault_addr, error_code, reason, sf.instruction_pointer, sf.code_segment, sf.stack_pointer,
            cr3.start_address().as_u64(),
            crate::process::scheduler::current_pid_fast()
        );
    }

    // Step 2: VMA lookup — lock-free fast path.
    let (pid, vma) = match unsafe { crate::process::scheduler::find_vma_fast(fault_addr) } {
        Some(result) => result,
        None => {
            if is_user {
                serial_println!(
                    "⚠️  Segfault: PID {} accessed {:#x} (no VMA)",
                    crate::process::scheduler::current_pid_fast(), fault_addr
                );
                kill_current_user_process("SEGFAULT (no VMA for address)");
                // unreachable — kill_current_user_process diverges
            }
            panic!(
                "PAGE FAULT (kernel, no VMA)\n  Address: {:#x}\n  Error: {:#b}\n  RIP: {:#x}",
                fault_addr, error_code, sf.instruction_pointer
            );
        }
    };

    // Step 3: Map the page (passes is_write for zero-page optimisation).
    if let Err(reason) = demand_paging::map_demand_page(fault_addr, &vma, pid, is_write) {
        if is_user {
            serial_println!(
                "⚠️  Demand paging failed for PID {}: {} (addr {:#x})",
                pid, reason, fault_addr
            );
            kill_current_user_process("DEMAND PAGING FAILED");
            // unreachable — kill_current_user_process diverges
        }
        panic!(
            "PAGE FAULT (kernel, map failed)\n  Address: {:#x}\n  Reason: {}\n  RIP: {:#x}",
            fault_addr, reason, sf.instruction_pointer
        );
    }

    // Success — CPU retries the faulting instruction on iret.
}

// ============================================================================
// Kill user process and perform FULL context switch
// ============================================================================

/// Kill the current user process and jump to the next Ready process.
///
/// Called from exception handlers when the fault originated in user mode
/// (Ring 3).  Uses `jump_to_trapframe` to perform a FULL context switch
/// that restores ALL registers (RAX..R15 + iret fields).
///
/// This function DIVERGES — it never returns to the calling exception
/// handler.  The `jump_to_trapframe` assembly does its own `iretq`
/// into the next process.
///
/// PREVIOUS BUG: The old implementation overwrote only the 5-field
/// ExceptionStackFrame (RIP, CS, RFLAGS, RSP, SS) and returned normally.
/// This leaked GPR values (RAX..R15) from the killed process into the
/// next process, causing data corruption and unpredictable behavior.
fn kill_current_user_process(reason: &str) -> ! {
    let tf_ptr = {
        let mut scheduler = crate::process::scheduler::local_scheduler();

        // Tag the about-to-die process so `waitpid()` reports a real
        // WIFSIGNALED/SIGSEGV status instead of a lying "exited(0)" — every
        // hardware fault this handler covers (divide-by-zero, invalid
        // opcode, GPF, unhandled page fault) is reported as SIGSEGV, since
        // this kernel doesn't distinguish fault kinds at the signal level.
        // Captured before `kill_and_switch_tf` takes the process out of
        // `self.running`.
        let (dead_pid, parent_pid) = match scheduler.running_mut() {
            Some(proc) => {
                proc.killed_by_signal = Some(crate::process::signal::SIGSEGV);
                let parent = if proc.is_thread { None } else { proc.parent_pid };
                (proc.pid.0, parent)
            }
            None => (0, None),
        };

        let ptr = scheduler.kill_and_switch_tf(reason);
        scheduler.notify_child_death(dead_pid, parent_pid);

        serial_println!("  → Switching to next process (full TrapFrame restore)");
        ptr
        // Lock is dropped here before we jump
    };

    // Perform FULL context switch: loads all GPRs + iretq.
    // This never returns.
    unsafe {
        crate::process::trapframe::jump_to_user(tf_ptr);
    }
}

extern "x86-interrupt" fn timer_handler(_sf: &mut ExceptionStackFrame) {
    unsafe {
        use x86_64::instructions::port::PortWriteOnly;
        PortWriteOnly::<u8>::new(0x20).write(0x20);
    }
}

// ============================================================================
// HARDWARE INIT
// ============================================================================

/// Draw the initial boot screen (after allocators are ready).
pub fn draw_boot_screen() {
    let mut fb = framebuffer::FRAMEBUFFER.lock();
    if let Some(fb) = fb.as_mut() {
        fb.clear(Color::rgb(0, 0, 0));
        fb.draw_text(10, 10, "ConstanOS v0.1", Color::rgb(0, 200, 255), Color::rgb(0, 0, 0), 2);
        fb.draw_text(10, 770, "Allocator: Ready", Color::rgb(0, 255, 0), Color::rgb(0, 0, 0), 2);
    }
}

/// PIC + PIT + load IDT.
pub fn init_hardware_interrupts() {
    crate::interrupts::pic::initialize();
    crate::interrupts::pic::enable_irq(0);
    crate::interrupts::pic::enable_irq(1);
    crate::interrupts::pic::enable_irq(4); // COM1 (serial stdin)
    load_idt();

    crate::serial::init_interrupts();
    crate::pit::init(100);
}