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

use crate::process::TrapFrame;

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
        // Raw entries for the faults of user code (`fault_entry!`): they hand the Rust side a full TrapFrame, so a signal
        // handler can be run on the faulting context.
        idt.entries[0].set_handler_addr(divide_error_entry as u64);
        idt.entries[6].set_handler_addr(invalid_opcode_entry as u64);
        // IST index is 1-based in the IDT entry.  TSS defines
        // DOUBLE_FAULT_IST_INDEX = 0 (array index), so CPU IST = 0 + 1 = 1.
        idt.add_double_fault_handler(
            8,
            double_fault_handler,
            (crate::process::tss::DOUBLE_FAULT_IST_INDEX + 1) as u16,
        );
        idt.entries[13].set_handler_addr(general_protection_entry as u64);
        idt.entries[14].set_handler_addr(page_fault_entry as u64);
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
        // MSI vectors: stubs now, handlers when a driver finds its device.
        crate::interrupts::msi::register_idt(&mut idt);
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
const PF_INSTR:    u64 = 1 << 4;   // 1 = instruction fetch

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
    // Wake any process blocked in poll/epoll_wait watching stdin for POLLIN,
    // or `/dev/input/event0`.
    crate::process::syscall::poll_wakeup_for_fd0();
    crate::process::syscall::poll_wakeup_for_input(crate::drivers::evdev::QUEUE_KEYBOARD);
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
    if crate::mouse::process_byte(data) {
        crate::process::syscall::poll_wakeup_for_input(crate::drivers::evdev::QUEUE_MOUSE);
    }
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

/// A CPU exception entry that hands Rust the faulting context as a `TrapFrame`.
///
/// The CPU pushes `[err][rip][cs][rflags][rsp][ss]` (a fault without an error code gets a dummy 0 first, so both shapes are
/// alike). `xchg` puts the error code in RAX and the caller's RAX in that slot, which then is the `rax` field of the
/// `TrapFrame` the pushes below build: `[r15 … rbx][rax][rip][cs][rflags][rsp][ss]`. `$handler(frame, error_code)` may edit the
/// frame (a signal handler is run by rewriting it); the pops and `iretq` resume whatever it holds. Interrupts stay off (an
/// interrupt gate), as in the `x86-interrupt` shims this replaces, and `cld` is ours to do.
macro_rules! fault_entry {
    ($entry:literal, $handler:literal, error_code) => {
        fault_entry!(@emit $entry, $handler, "");
    };
    ($entry:literal, $handler:literal, no_error_code) => {
        fault_entry!(@emit $entry, $handler, "push 0");
    };
    (@emit $entry:literal, $handler:literal, $dummy:literal) => {
        core::arch::global_asm!(
            concat!(".global ", $entry),
            concat!($entry, ":"),
            $dummy,
            "cld",
            "xchg rax, [rsp]",
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
            "mov rdi, rsp",
            "mov rsi, rax",
            concat!("call ", $handler),
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
            "iretq",
        );
    };
}

fault_entry!("divide_error_entry", "divide_error_rust", no_error_code);
fault_entry!("invalid_opcode_entry", "invalid_opcode_rust", no_error_code);
fault_entry!("general_protection_entry", "general_protection_rust", error_code);
fault_entry!("page_fault_entry", "page_fault_rust", error_code);

extern "C" {
    fn divide_error_entry();
    fn invalid_opcode_entry();
    fn general_protection_entry();
    fn page_fault_entry();
}

const USER_CS: u64 = 0x23;

/// A fault of user code that nothing resolves: run the process's handler for `sig` if it has one, else kill it. Kernel-mode
/// faults never come here (they panic).
fn user_fault(tf: &mut TrapFrame, sig: u32, si_code: i32, addr: u64, reason: &str) {
    let delivered = {
        let mut sched = crate::process::scheduler::local_scheduler();
        match sched.running_mut() {
            Some(proc) => unsafe { crate::process::signal::deliver_fault(proc, tf, sig, si_code, addr) },
            None => false,
        }
    };
    if !delivered {
        report_user_backtrace(tf);
        kill_current_user_process(reason, sig);
        // unreachable — kill_current_user_process diverges
    }
}

/// `user backtrace: pid N (name) rip=.. #0 .. #1 ..`: a frame-pointer walk
/// (`diag::backtrace`) of the process about to be killed, read through its
/// address space. The C programs keep frame pointers; `scripts/run-abi-suite.sh`
/// turns the addresses into function:line. Returns before the kill so that
/// nothing here (the address-space `Arc`) is alive across the `-> !` call.
fn report_user_backtrace(tf: &TrapFrame) {
    use core::fmt::Write;
    let found = {
        let mut sched = crate::process::scheduler::local_scheduler();
        sched.running_mut().map(|p| (p.pid.0, p.name, p.address_space.clone()))
    };
    let Some((pid, name, space)) = found else { return };
    let read = |addr: u64| {
        space.find_vma(addr)?;
        let mut b = [0u8; 8];
        // SAFETY: a read of the faulting process's own mapped memory, through its page tables.
        (unsafe { space.copy_from_user(addr, &mut b) } == 8).then(|| u64::from_le_bytes(b))
    };
    let frames = diag::backtrace::walk(tf.rip, tf.rbp, read);
    let mut line = StackStr::<512>::new();
    let _ = write!(
        line,
        "user backtrace: pid {} ({}) rsp={:#x}",
        pid,
        core::str::from_utf8(&name).unwrap_or("?").trim_end_matches('\0'),
        tf.rsp
    );
    for (i, pc) in frames.as_slice().iter().enumerate() {
        let _ = write!(line, " #{} {:#x}", i, pc);
    }
    serial_println!("{}", line.as_str());
}

/// A fixed-size string on the stack, for a kill reason built from numbers: unlike a `String` it has no `Drop`, so it may be
/// live when a `-> !` function is called. Text past `N` bytes is cut.
struct StackStr<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> StackStr<N> {
    fn new() -> Self {
        Self { buf: [0; N], len: 0 }
    }
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl<const N: usize> core::fmt::Write for StackStr<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = s.len().min(N - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

// siginfo `si_code`s of a fault.
const SEGV_MAPERR: i32 = 1;
const SEGV_ACCERR: i32 = 2;
const SI_KERNEL: i32 = 0x80;
const ILL_ILLOPN: i32 = 2;
const FPE_INTDIV: i32 = 1;

#[no_mangle]
extern "C" fn divide_error_rust(tf: &mut TrapFrame, _error_code: u64) {
    if tf.cs == USER_CS {
        user_fault(tf, crate::process::signal::SIGFPE, FPE_INTDIV, tf.rip, "DIVIDE BY ZERO");
        return;
    }
    panic!("DIVIDE BY ZERO at {:#x}", tf.rip);
}

#[no_mangle]
extern "C" fn invalid_opcode_rust(tf: &mut TrapFrame, _error_code: u64) {
    if tf.cs == USER_CS {
        user_fault(tf, crate::process::signal::SIGILL, ILL_ILLOPN, tf.rip, "INVALID OPCODE");
        return;
    }
    panic!("INVALID OPCODE at {:#x}", tf.rip);
}

extern "x86-interrupt" fn double_fault_handler(
    sf: ExceptionStackFrame,
    error_code: u64
) -> ! {
    panic!("DOUBLE FAULT (error: {}) at {:#x}", error_code, sf.instruction_pointer);
}

#[no_mangle]
extern "C" fn general_protection_rust(tf: &mut TrapFrame, error_code: u64) {
    if tf.cs == USER_CS {
        user_fault(tf, crate::process::signal::SIGSEGV, SI_KERNEL, 0, "GENERAL PROTECTION FAULT");
        return;
    }
    panic!("GENERAL PROTECTION FAULT (error: {}) at {:#x}", error_code, tf.rip);
}

/// Page fault handler — bridges memory and process layers.
///
/// Flow:
///   1. Pre-filter via demand_paging::is_demand_pageable
///   2. VMA lookup via scheduler
///   3. Map page via demand_paging::map_demand_page
///   4. On failure: kill user process OR panic (kernel fault)
#[no_mangle]
extern "C" fn page_fault_rust(tf: &mut TrapFrame, error_code: u64) {
    use crate::memory::demand_paging;

    let fault_addr = demand_paging::read_cr2();
    let is_user = error_code & PF_USER != 0;
    let is_write = error_code & PF_WRITE != 0;


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
        if is_user {
            user_fault(tf, crate::process::signal::SIGSEGV, SEGV_ACCERR, fault_addr, "COW FAULT FAILED");
            return;
        }
        kill_current_user_process("COW FAULT FAILED", crate::process::signal::SIGSEGV);
        // unreachable
    }

    // An instruction fetch from a present page: the page is NX (no PF_X segment, no PROT_EXEC). Say so (P1.1), with the
    // address, rather than a plain segmentation fault. The reason lives in a stack buffer: nothing with a `Drop` may be live
    // when `user_fault` diverges.
    if is_user && error_code & (PF_PRESENT | PF_INSTR) == (PF_PRESENT | PF_INSTR) && error_code & PF_RESERVED == 0 {
        let mut why = StackStr::<64>::new();
        let _ = core::fmt::write(&mut why, format_args!("EXECUTED NON-EXECUTABLE MEMORY at {:#x}", fault_addr));
        serial_println!(
            "⚠️  PID {} executed non-executable memory at {:#x} (error {:#b})",
            crate::process::scheduler::current_pid_fast(), fault_addr, error_code
        );
        user_fault(tf, crate::process::signal::SIGSEGV, SEGV_ACCERR, fault_addr, why.as_str());
        return;
    }

    // Step 1: Is this fault potentially demand-pageable?
    if let Err(reason) = demand_paging::is_demand_pageable(error_code) {
        if is_user {
            serial_println!(
                "⚠️  User page fault at {:#x} (error {:#b}): {}",
                fault_addr, error_code, reason
            );
            let code = if error_code & PF_PRESENT != 0 { SEGV_ACCERR } else { SEGV_MAPERR };
            user_fault(tf, crate::process::signal::SIGSEGV, code, fault_addr, "PAGE FAULT (not demand-pageable)");
            return;
        }
        let (cr3, _) = x86_64::registers::control::Cr3::read();
        panic!(
            "PAGE FAULT (kernel)\n  Address: {:#x}\n  Error: {:#b}\n  Reason: {}\n  RIP: {:#x}\n  CS: {:#x}\n  RSP: {:#x}\n  CR3: {:#x}\n  running PID: {}",
            fault_addr, error_code, reason, tf.rip, tf.cs, tf.rsp,
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
                    tf.rip, error_code
                );
                dump_user_stack(tf.rsp);
                user_fault(tf, crate::process::signal::SIGSEGV, SEGV_MAPERR, fault_addr, "SEGFAULT (no VMA for address)");
                return;
            }
            panic!(
                "PAGE FAULT (kernel, no VMA)\n  Address: {:#x}\n  Error: {:#b}\n  RIP: {:#x}",
                fault_addr, error_code, tf.rip
            );
        }
        Err(crate::memory::address_space::FaultError::Failed(reason)) => {
            if is_user {
                serial_println!(
                    "⚠️  Demand paging failed for PID {}: {} (addr {:#x})",
                    crate::process::scheduler::current_pid_fast(), reason, fault_addr
                );
                user_fault(tf, crate::process::signal::SIGSEGV, SEGV_ACCERR, fault_addr, "DEMAND PAGING FAILED");
                return;
            }
            panic!(
                "PAGE FAULT (kernel, map failed)\n  Address: {:#x}\n  Reason: {}\n  RIP: {:#x}",
                fault_addr, reason, tf.rip
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

fn kill_current_user_process(reason: &str, sig: u32) -> ! {
    let tf_ptr = {
        let mut scheduler = crate::process::scheduler::local_scheduler();

        // Tag the about-to-die process so `waitpid()` reports a real
        // WIFSIGNALED/SIGSEGV status instead of a lying "exited(0)" — every
        // hardware fault this handler covers (divide-by-zero, invalid
        // opcode, GPF, unhandled page fault) is reported as SIGSEGV, since
        // this kernel doesn't distinguish fault kinds at the signal level.
        // Captured before `kill_and_switch_tf` takes the process out of
        // `self.running`.
        scheduler.kill_thread_group(sig);
        let (dead_pid, parent_pid) = match scheduler.running_mut() {
            Some(proc) => {
                proc.killed_by_signal.get_or_insert(sig);
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