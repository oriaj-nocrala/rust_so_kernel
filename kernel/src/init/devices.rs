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
        idt.add_handler(44, mouse_interrupt_handler);
        // Every other 8259 vector gets a handler too. An empty IDT slot is
        // not "ignored": delivering to it is a #GP, i.e. a kernel panic —
        // and IRQ7/IRQ15 fire spuriously on real hardware whatever the
        // masks say (see `pic::is_spurious`).
        idt.add_handler(34, irq2_handler);
        idt.add_handler(35, irq3_handler);
        idt.add_handler(37, irq5_handler);
        idt.add_handler(38, irq6_handler);
        idt.add_handler(39, irq7_handler);
        idt.add_handler(40, irq8_handler);
        idt.add_handler(41, irq9_handler);
        idt.add_handler(42, irq10_handler);
        idt.add_handler(43, irq11_handler);
        idt.add_handler(45, irq13_handler);
        idt.add_handler(46, irq14_handler);
        idt.add_handler(47, irq15_handler);
        // The LAPIC's own spurious vector (see `apic::SPURIOUS_VECTOR`).
        idt.add_handler(crate::interrupts::apic::SPURIOUS_VECTOR, lapic_spurious_handler);
        // Inter-processor interrupts (stage 5 of `docs/smp/smp-plan.md`).
        idt.add_handler(crate::memory::tlb::SHOOTDOWN_VECTOR, tlb_shootdown_handler);
        idt.add_handler(crate::smp::WAKE_VECTOR, wake_ipi_handler);
        // Stage 7: a raw entry like the timer's — it may switch processes.
        idt.entries[crate::process::scheduler::RESCHED_VECTOR as usize]
            .set_handler_addr(crate::process::timer_preempt::resched_interrupt_entry as u64);
        // Syscalls are now handled via the `syscall` instruction (LSTAR MSR),
        // not via int 0x80.  No IDT entry needed.
        idt
    });
}

/// Shared by every CPU; each loads it itself (`cpu::init_this_cpu`). The
/// BSP also loads it early in boot, so exceptions before that panic rather
/// than triple-fault.
pub fn load_idt() {
    IDT.get().unwrap().load();
}

/// Is this CPU's IDTR the kernel IDT?
pub fn verify_idt() -> Result<(), &'static str> {
    let idtr = x86_64::instructions::tables::sidt();
    let idt = IDT.get().ok_or("IDT not built")?;
    if idtr.base.as_u64() != idt as *const _ as u64
        || idtr.limit as usize != core::mem::size_of::<InterruptDescriptorTable>() - 1
    {
        return Err("IDTR is not the kernel IDT");
    }
    Ok(())
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

extern "x86-interrupt" fn keyboard_interrupt_handler(_: ExceptionStackFrame) {
    let scancode = unsafe {
        x86_64::instructions::port::PortReadOnly::<u8>::new(0x60).read()
    };
    keyboard::process_scancode(scancode);
    // Wake any process blocked on stdin read.
    crate::process::syscall::stdin_wakeup();
    // Wake any process blocked in poll/epoll_wait watching stdin for POLLIN.
    crate::process::syscall::poll_wakeup_for_fd0();
    crate::interrupts::eoi(crate::interrupts::pic::Irq::Keyboard.as_u8());
}

/// COM1 receive interrupt — lets serial input act as stdin, alongside the
/// PS/2 keyboard.  Bytes are pushed into the same ring buffer the keyboard
/// ISR feeds (`keyboard_buffer::KEYBOARD_BUFFER`) and the same wakeup path
/// is used, so fd 0 (hardcoded to that buffer in `sys_read`) doesn't care
/// which physical source a byte came from.  This is what lets `qemu
/// -serial stdio` be used to type/pipe input into the shell instead of the
/// QEMU-monitor `sendkey` workaround.
extern "x86-interrupt" fn serial_interrupt_handler(_: ExceptionStackFrame) {
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
    crate::interrupts::eoi(crate::interrupts::pic::Irq::Com1.as_u8());
}

/// IRQ12 — PS/2 auxiliary device (mouse). Each byte belongs to a 3-byte
/// packet; `mouse::process_byte` does the reassembly/decode, same shape
/// as `keyboard::process_scancode` does for IRQ1.
extern "x86-interrupt" fn mouse_interrupt_handler(_: ExceptionStackFrame) {
    let data = unsafe {
        x86_64::instructions::port::PortReadOnly::<u8>::new(0x60).read()
    };
    crate::mouse::process_byte(data);
    crate::interrupts::eoi(crate::interrupts::pic::Irq::Mouse.as_u8());
}

/// A PIC line with no driver behind it. IRQ7/IRQ15 are checked for the
/// 8259's spurious case first, which needs a different EOI (see
/// `pic::is_spurious`); anything else is a real interrupt on a line that
/// should be masked — counted, EOI'd, and otherwise dropped.
///
/// Under the APIC these vectors can come from two places: an I/O APIC pin
/// (the LAPIC has it in service and needs the EOI) or a leftover from the
/// masked 8259 (the LAPIC does not, and must not get one — it would end
/// whatever else is in service). The LAPIC's ISR says which.
fn unhandled_pic_irq(line: u8) {
    use crate::interrupts::{apic, pic};
    if apic::active() && apic::in_service(pic::PIC1_OFFSET + line) {
        crate::debug::note_unexpected_irq(line);
        apic::eoi();
        return;
    }
    if (line == 7 || line == 15) && pic::is_spurious(line) {
        crate::debug::inc_spurious_irqs();
        pic::end_of_spurious(line);
        return;
    }
    crate::debug::note_unexpected_irq(line);
    pic::end_of_interrupt(pic::PIC1_OFFSET + line);
}

macro_rules! unhandled_irq_handlers {
    ($($name:ident => $line:expr),* $(,)?) => {$(
        extern "x86-interrupt" fn $name(_: ExceptionStackFrame) {
            unhandled_pic_irq($line);
        }
    )*};
}

unhandled_irq_handlers! {
    irq2_handler => 2, irq3_handler => 3, irq5_handler => 5, irq6_handler => 6,
    irq7_handler => 7, irq8_handler => 8, irq9_handler => 9, irq10_handler => 10,
    irq11_handler => 11, irq13_handler => 13, irq14_handler => 14, irq15_handler => 15,
}

/// A LAPIC spurious interrupt: no ISR bit is set for it, so no EOI.
extern "x86-interrupt" fn lapic_spurious_handler(_: ExceptionStackFrame) {
    crate::debug::inc_spurious_irqs();
}

/// Another CPU changed a mapping this one may cache (`memory::tlb`). The
/// request may already have been answered by a spin loop here with IF=0,
/// in which case this finds nothing to do.
extern "x86-interrupt" fn tlb_shootdown_handler(_: ExceptionStackFrame) {
    crate::memory::tlb::service_pending();
    crate::interrupts::apic::eoi();
}

/// Only wakes a CPU from `hlt` so it looks at its mailbox (`smp::run_on`).
extern "x86-interrupt" fn wake_ipi_handler(_: ExceptionStackFrame) {
    crate::interrupts::apic::eoi();
}

extern "x86-interrupt" fn divide_by_zero_handler(sf: ExceptionStackFrame) {
    if sf.code_segment & 0x3 != 0 {
        kill_current_user_process("DIVIDE BY ZERO");
        // unreachable — kill_current_user_process diverges
    }
    panic!("DIVIDE BY ZERO at {:#x}", sf.instruction_pointer);
}

extern "x86-interrupt" fn invalid_opcode_handler(sf: ExceptionStackFrame) {
    if sf.code_segment & 0x3 != 0 {
        kill_current_user_process("INVALID OPCODE");
        // unreachable — kill_current_user_process diverges
    }
    panic!("INVALID OPCODE at {:#x}", sf.instruction_pointer);
}

extern "x86-interrupt" fn double_fault_handler(
    sf: ExceptionStackFrame,
    error_code: u64
) -> ! {
    panic!("DOUBLE FAULT (error: {}) at {:#x}", error_code, sf.instruction_pointer);
}

extern "x86-interrupt" fn general_protection_fault_handler(
    sf: ExceptionStackFrame,
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
    sf: ExceptionStackFrame,
    error_code: u64
) {
    use crate::memory::demand_paging;

    let fault_addr = demand_paging::read_cr2();
    let is_user = error_code & PF_USER != 0;
    let is_write = error_code & PF_WRITE != 0;

    let _ = &sf; // (was "unreliable for user-mode PFs": that was the by-reference ABI bug, see idt.rs)

    // ── COW write fault: page present + write, no reserved bit ───
    //
    // This must be checked BEFORE is_demand_pageable, which returns Err
    // for present pages (treating them as protection violations).
    //
    // Deliberately NOT gated on PF_USER: a syscall handler (read(),
    // getdents64(), etc.) writes into the *calling* process's own buffer
    // using its own CR3/address space, but executes in ring 0 — so the
    // exact same COW-shared-page write can fault with PF_USER clear
    // instead of set. This is routine for any BusyBox `APPLET_NOEXEC`
    // applet (`ls`, `sort`, ...): they fork but never call a real
    // execve(), so their own buffers stay COW-shared with the parent
    // shell until the applet's first read()/write() into them — which
    // happens inside the kernel, in kernel mode. Before this fix, that
    // fell through to the generic "kernel-mode, not demand-pageable"
    // panic below instead of being resolved exactly like a user-mode
    // COW fault would be (confirmed live: `sort`/`find` reliably paniced
    // the kernel this way). Safe to drop the check: find_vma_fast only
    // ever matches an address inside the *current* process's own
    // registered VMA, so a fault on real kernel memory still correctly
    // falls through un-resolved either way.
    //
    // `current_as_fast` is lock-free (per-CPU, valid while this process
    // runs here); the VMA lookup and the PTE change both happen under the
    // address space's own lock, inside `handle_cow_fault`.
    if (error_code & (PF_PRESENT | PF_WRITE)) == (PF_PRESENT | PF_WRITE)
        && (error_code & PF_RESERVED) == 0
    {
        let handled = unsafe {
            crate::process::scheduler::current_as_fast()
                .map(|as_| as_.handle_cow_fault(fault_addr).is_ok())
                .unwrap_or(false)
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

    // Step 2: VMA lookup + map, under the address space's lock (see
    // `AddressSpace::handle_not_present_fault`). Also grows a
    // GrowableStack VMA (the user stack) downward if the address is just
    // below it. A page a sibling thread mapped meanwhile is success.
    let result = match unsafe { crate::process::scheduler::current_as_fast() } {
        Some(as_) => unsafe { as_.handle_not_present_fault(fault_addr, is_write) },
        None => Err(crate::memory::address_space::FaultError::NoVma),
    };
    match result {
        Ok(()) => {}
        Err(crate::memory::address_space::FaultError::NoVma) => {
            if is_user {
                serial_println!(
                    "⚠️  Segfault: PID {} accessed {:#x} (no VMA) at rip {:#x} (error {:#b})",
                    crate::process::scheduler::current_pid_fast(), fault_addr,
                    sf.instruction_pointer, error_code
                );
                dump_user_stack(sf.stack_pointer);
                kill_current_user_process("SEGFAULT (no VMA for address)");
                // unreachable — kill_current_user_process diverges
            }
            panic!(
                "PAGE FAULT (kernel, no VMA)\n  Address: {:#x}\n  Error: {:#b}\n  RIP: {:#x}",
                fault_addr, error_code, sf.instruction_pointer
            );
        }
        Err(crate::memory::address_space::FaultError::Failed(reason)) => {
            if is_user {
                serial_println!(
                    "⚠️  Demand paging failed for PID {}: {} (addr {:#x})",
                    crate::process::scheduler::current_pid_fast(), reason, fault_addr
                );
                kill_current_user_process("DEMAND PAGING FAILED");
                // unreachable — kill_current_user_process diverges
            }
            panic!(
                "PAGE FAULT (kernel, map failed)\n  Address: {:#x}\n  Reason: {}\n  RIP: {:#x}",
                fault_addr, reason, sf.instruction_pointer
            );
        }
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
/// Prints the user stack around `rsp` of the process that is about to be
/// killed: the one post-mortem a segfault into garbage (`rip` 0, an RFLAGS
/// value, another binary's address) can be read from — the words there are
/// the return addresses of whoever led to it. Reads through the active page
/// table and skips unmapped pages, so it cannot fault itself. Built for the
/// 2026-09-24 hunt for `ash` dying at its `exit` builtin in autorun jobs.
fn dump_user_stack(rsp: u64) {
    use x86_64::structures::paging::{OffsetPageTable, PageTable, Translate};

    let phys_offset = crate::memory::physical_memory_offset();
    let (cr3, _) = x86_64::registers::control::Cr3::read();
    let pml4 = unsafe {
        &mut *((phys_offset + cr3.start_address().as_u64()).as_mut_ptr::<PageTable>())
    };
    let table = unsafe { OffsetPageTable::new(pml4, phys_offset) };

    serial_println!("  user stack at rsp={:#x}:", rsp);
    let start = rsp.wrapping_sub(8 * 8) & !7;
    for i in 0..24u64 {
        let addr = start.wrapping_add(i * 8);
        match table.translate_addr(x86_64::VirtAddr::try_new(addr).unwrap_or(x86_64::VirtAddr::zero())) {
            Some(phys) if addr >= 0x1000 && phys.as_u64() & 0xFFF <= 0xFF8 => {
                let v = unsafe { *((phys_offset + phys.as_u64()).as_ptr::<u64>()) };
                let mark = if addr == rsp { " <- rsp" } else { "" };
                serial_println!("    {:#x}: {:#018x}{}", addr, v, mark);
            }
            _ => serial_println!("    {:#x}: (unmapped)", addr),
        }
    }

    // A handler returns through the sigreturn trampoline; if its page no
    // longer holds the trampoline, the return runs whatever is there.
    use crate::memory::signal_trampoline::{TRAMPOLINE_CODE, TRAMPOLINE_VA};
    match table.translate_addr(x86_64::VirtAddr::new(TRAMPOLINE_VA)) {
        Some(phys) => {
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    (phys_offset + phys.as_u64()).as_ptr::<u8>(),
                    TRAMPOLINE_CODE.len(),
                )
            };
            serial_println!(
                "  sigreturn trampoline (phys {:#x}): {:02x?} {}",
                phys.as_u64(), bytes,
                if bytes == TRAMPOLINE_CODE { "intact" } else { "CORRUPTED" }
            );
        }
        None => serial_println!("  sigreturn trampoline: not mapped"),
    }
}

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

                // Say it on screen too, not just over serial. On hardware
                // with no serial capture this line is the *only* thing that
                // distinguishes "a process died" from "the machine hung" —
                // both otherwise look like a screen that stopped changing
                // under a still-blinking cursor. Never blocks; see
                // `kernel_alert`.
                crate::kalert!(
                    "PID {} ({}) killed: {}",
                    proc.pid.0,
                    core::str::from_utf8(&proc.name).unwrap_or("<?>").trim_end_matches('\0'),
                    reason,
                );

                (proc.pid.0, parent)
            }
            None => (0, None),
        };

        let ptr = scheduler.kill_and_switch_tf(reason);
        scheduler.notify_child_death(dead_pid, parent_pid);
        // Same side-table cleanup `sys_exit`/the uncaught-signal path do —
        // see `process::syscall::cancel_all_waiters`'s doc comment.
        crate::process::syscall::cancel_all_waiters(dead_pid);

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

// ============================================================================
// HARDWARE INIT
// ============================================================================

/// Draw the initial boot screen (after allocators are ready).
pub fn draw_boot_screen() {
    let mut fb = framebuffer::FRAMEBUFFER.lock();
    let mut bottom = 0;
    if let Some(fb) = fb.as_mut() {
        fb.clear(Color::rgb(0, 0, 0));
        let h = crate::drivers::framebuffer_console::draw_banner(
            fb, 12, 8, "ConstanOS v0.1", Color::rgb(0x61, 0xAF, 0xEF),
        );
        bottom = 8 + h;
    }
    drop(fb);
    // This banner is drawn straight to the framebuffer, not through the
    // text console, so the console's cursor is still at row 0 and the next
    // kernel notice would land on top of it. Park the cursor below the
    // banner instead — it made the first `kalert!` line genuinely hard to
    // read on the one screen that matters.
    crate::drivers::framebuffer_console::reserve_pixels_at_top(bottom + 8);
}

/// PIC + PIT + load IDT. The PIT is needed at least until `cpu::tsc::init`
/// has calibrated against it; `interrupts::apic::init` then retires both in
/// favour of the LAPIC timer and the I/O APIC, re-routing the ISA lines
/// enabled here.
pub fn init_hardware_interrupts() {
    crate::interrupts::pic::initialize();
    // IRQ0 directly, not through `enable_isa_irq`: under the APIC the timer
    // is the LAPIC's own, not an I/O APIC pin.
    crate::interrupts::pic::enable_irq(0);
    crate::interrupts::enable_isa_irq(1);
    crate::interrupts::enable_isa_irq(4); // COM1 (serial stdin)
    load_idt();

    crate::serial::init_interrupts();
    crate::pit::init(100);
}