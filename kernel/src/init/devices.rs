// kernel/src/init/devices.rs
//
// IDT construction, interrupt handlers, PIC/PIT init, boot screen.
//
// The page fault handler lives here because it bridges memory and
// process layers.  User-mode segfaults kill the process; only
// kernel-mode faults panic.

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

extern "C" {
    fn syscall_entry();
}

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
        idt.entries[0x80]
            .set_handler_addr(syscall_entry as u64)
            .set_privilege_level(3);
        idt
    });
}

fn load_idt() {
    IDT.get().unwrap().load();
}

// ============================================================================
// Page fault error code bits
// ============================================================================

const PF_USER: u64 = 1 << 2;

// ============================================================================
// INTERRUPT HANDLERS
// ============================================================================

extern "x86-interrupt" fn keyboard_interrupt_handler(_: &mut ExceptionStackFrame) {
    let scancode = unsafe {
        x86_64::instructions::port::PortReadOnly::<u8>::new(0x60).read()
    };
    keyboard::process_scancode(scancode);
    crate::interrupts::pic::end_of_interrupt(crate::interrupts::pic::Irq::Keyboard.as_u8());
}

extern "x86-interrupt" fn divide_by_zero_handler(sf: &mut ExceptionStackFrame) {
    if sf.code_segment & 0x3 != 0 {
        kill_current_user_process("DIVIDE BY ZERO", sf);
        return;
    }
    panic!("DIVIDE BY ZERO at {:#x}", sf.instruction_pointer);
}

extern "x86-interrupt" fn invalid_opcode_handler(sf: &mut ExceptionStackFrame) {
    if sf.code_segment & 0x3 != 0 {
        kill_current_user_process("INVALID OPCODE", sf);
        return;
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
        kill_current_user_process("GENERAL PROTECTION FAULT", sf);
        return;
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

    // Step 1: Is this fault potentially demand-pageable?
    if let Err(reason) = demand_paging::is_demand_pageable(error_code) {
        if is_user {
            serial_println!(
                "⚠️  User page fault at {:#x} (error {:#b}): {}",
                fault_addr, error_code, reason
            );
            kill_current_user_process("PAGE FAULT (not demand-pageable)", sf);
            return;
        }
        panic!(
            "PAGE FAULT (kernel)\n  Address: {:#x}\n  Error: {:#b}\n  Reason: {}\n  RIP: {:#x}",
            fault_addr, error_code, reason, sf.instruction_pointer
        );
    }

    // Step 2: VMA lookup
    let (pid, vma) = match crate::process::scheduler::find_current_vma(fault_addr) {
        Some(result) => result,
        None => {
            if is_user {
                serial_println!(
                    "⚠️  Segfault: PID ? accessed {:#x} (no VMA)",
                    fault_addr
                );
                kill_current_user_process("SEGFAULT (no VMA for address)", sf);
                return;
            }
            panic!(
                "PAGE FAULT (kernel, no VMA)\n  Address: {:#x}\n  Error: {:#b}\n  RIP: {:#x}",
                fault_addr, error_code, sf.instruction_pointer
            );
        }
    };

    // Step 3: Map the page
    if let Err(reason) = demand_paging::map_demand_page(fault_addr, &vma, pid) {
        if is_user {
            serial_println!(
                "⚠️  Demand paging failed for PID {}: {} (addr {:#x})",
                pid, reason, fault_addr
            );
            kill_current_user_process("DEMAND PAGING FAILED", sf);
            return;
        }
        panic!(
            "PAGE FAULT (kernel, map failed)\n  Address: {:#x}\n  Reason: {}\n  RIP: {:#x}",
            fault_addr, reason, sf.instruction_pointer
        );
    }

    // Success — CPU retries the faulting instruction on iret.
}

// ============================================================================
// Kill user process and schedule next
// ============================================================================

/// Kill the current user process and switch to the next Ready process.
///
/// Called from exception handlers when the fault originated in user mode
/// (Ring 3).  Overwrites the exception stack frame so `iretq` lands on
/// the next process.
fn kill_current_user_process(reason: &str, sf: &mut ExceptionStackFrame) {
    use crate::process::scheduler::SCHEDULER;

    let mut scheduler = SCHEDULER.lock();
    let frame = scheduler.kill_and_switch(reason);

    // Overwrite exception frame → iretq jumps to next process
    sf.instruction_pointer = frame.rip;
    sf.code_segment = frame.cs;
    sf.cpu_flags = frame.rflags;
    sf.stack_pointer = frame.rsp;
    sf.stack_segment = frame.ss;

    serial_println!("  → Switched to next process");
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
    load_idt();

    crate::pit::init(100);
}