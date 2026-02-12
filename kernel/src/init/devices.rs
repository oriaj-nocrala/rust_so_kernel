// kernel/src/init/devices.rs
//
// IDT construction, interrupt handlers, PIC/PIT init, boot screen.
//
// The page fault handler lives here because it bridges the memory and
// process layers:
//   - Calls demand_paging (memory layer) for error code filtering + mapping
//   - Calls scheduler (process layer) for VMA lookup
// This keeps the dependency one-way: init → {memory, process}.

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
        idt.add_double_fault_handler(8, double_fault_handler);
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
    panic!("DIVIDE BY ZERO at {:#x}", sf.instruction_pointer);
}

extern "x86-interrupt" fn invalid_opcode_handler(sf: &mut ExceptionStackFrame) {
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
    panic!("GENERAL PROTECTION FAULT (error: {}) at {:#x}", error_code, sf.instruction_pointer);
}

// ✅ Page fault handler — bridges memory and process layers.
//
// Flow:
//   1. Pre-filter via demand_paging::is_demand_pageable (memory layer, pure)
//   2. VMA lookup via scheduler::find_current_vma (process layer)
//   3. Map page via demand_paging::map_demand_page (memory layer, pure)
//
// This is the ONLY place where both layers meet.
extern "x86-interrupt" fn page_fault_handler(
    sf: &mut ExceptionStackFrame,
    error_code: u64
) {
    use crate::memory::demand_paging;

    let fault_addr = demand_paging::read_cr2();

    // Step 1: Is this fault potentially demand-pageable? (pure memory check)
    if let Err(reason) = demand_paging::is_demand_pageable(error_code) {
        panic!(
            "PAGE FAULT (unhandled)\n  Address: {:#x}\n  Error code: {:#b}\n  Reason: {}\n  RIP: {:#x}",
            fault_addr, error_code, reason, sf.instruction_pointer
        );
    }

    // Step 2: Look up VMA via the scheduler (process layer)
    let (pid, vma) = match crate::process::scheduler::find_current_vma(fault_addr) {
        Some(result) => result,
        None => {
            panic!(
                "PAGE FAULT (segmentation fault)\n  Address: {:#x}\n  Error code: {:#b}\n  Reason: no VMA for address\n  RIP: {:#x}",
                fault_addr, error_code, sf.instruction_pointer
            );
        }
    };

    // Step 3: Map the page (pure memory operation)
    if let Err(reason) = demand_paging::map_demand_page(fault_addr, &vma, pid) {
        panic!(
            "PAGE FAULT (demand paging failed)\n  Address: {:#x}\n  Error code: {:#b}\n  Reason: {}\n  RIP: {:#x}",
            fault_addr, error_code, reason, sf.instruction_pointer
        );
    }

    // Success — CPU will retry the faulting instruction on iret.
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