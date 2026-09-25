// kernel/src/cpu/init.rs
//
// `init_this_cpu` — stage 3 of docs/smp/smp-plan.md: every piece of state a
// CPU holds for itself, set up in one place, then read back from the
// hardware.
//
// The list is the plan's inventory of per-CPU registers, and it is the whole
// point: an AP (stage 4) gets exactly what this function does and nothing
// else, so a step missing here is a CPU running with the firmware's value —
// a write-through framebuffer, a `syscall` that lands nowhere, a page table
// the CPU reads with NX reserved. The BSP does some of it earlier in boot too
// (the IDT, so early exceptions panic instead of triple-faulting; the PAT,
// which `program_pat` decides by writing it), but it then comes through here
// like any other CPU, and every step is idempotent so that is harmless.
//
// The read-back is what makes the list checkable at all: each step has a
// `verify_*` that inspects the register it set, and the per-CPU result goes
// to `/proc/kdebug` (`cpu_init:`). `hw_tests::init_this_cpu_restores_what_an_ap_lacks`
// resets the BSP's registers to what an AP would come up with, re-runs this,
// and checks every step brought its register back.

use core::sync::atomic::{AtomicU32, Ordering};

use super::MAX_CPUS;

/// Steps, in the order they run. The names are what `/proc/kdebug` shows.
const STEPS: [&str; 8] = ["ctlregs", "gdt+tss", "idt", "gs", "syscall", "pat", "sse", "lapic"];

/// Per CPU: bit n = step n read back correctly; `DONE` = the CPU has run
/// `init_this_cpu` at all.
static VERIFIED: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];
const DONE: u32 = 1 << 31;
/// Per CPU: the first failing step's message. Written by that CPU during
/// its init, read by `/proc/kdebug`; no ISR touches it.
static FIRST_ERROR: [spin::Mutex<Option<&'static str>>; MAX_CPUS] =
    [const { spin::Mutex::new(None) }; MAX_CPUS];

/// Set up everything CPU `cpu` holds for itself, then verify it. Interrupts
/// must be off. The BSP calls this once in boot with `cpu = 0`; each AP will
/// call it with its own index, first thing in Rust.
///
/// Returns the first step that did not read back correctly; the caller
/// decides (the BSP panics: a boot CPU with a wrong GDT or LSTAR cannot run
/// a process).
pub fn init_this_cpu(cpu: usize) -> Result<(), (&'static str, &'static str)> {
    assert!(cpu < MAX_CPUS, "cpu {} beyond MAX_CPUS", cpu);
    // Before anything that could fault through a page with NX set.
    control_regs_init(cpu);
    crate::process::tss::init_this_cpu(cpu);
    crate::init::devices::load_idt();
    super::percpu::init_this_cpu(cpu);
    crate::process::tss::init_syscall_msrs();
    crate::memory::memtype::init_this_cpu();
    crate::process::fpu::init_this_cpu();
    crate::interrupts::apic::init_this_cpu();

    verify(cpu)
}

/// Read back every step on this CPU without changing anything; records the
/// result for `/proc/kdebug` like `init_this_cpu` does.
pub fn verify(cpu: usize) -> Result<(), (&'static str, &'static str)> {
    let results: [Result<(), &'static str>; STEPS.len()] = [
        control_regs_verify(),
        crate::process::tss::verify_this_cpu(cpu),
        crate::init::devices::verify_idt(),
        super::percpu::verify_this_cpu(cpu),
        crate::process::tss::verify_syscall_msrs(),
        crate::memory::memtype::verify_this_cpu(),
        crate::process::fpu::verify_this_cpu(),
        crate::interrupts::apic::verify_this_cpu(),
    ];
    let mut bits = DONE;
    let mut first = None;
    for (i, r) in results.iter().enumerate() {
        match r {
            Ok(()) => bits |= 1 << i,
            Err(e) if first.is_none() => first = Some((STEPS[i], *e)),
            Err(_) => {}
        }
    }
    VERIFIED[cpu].store(bits, Ordering::Relaxed);
    *FIRST_ERROR[cpu].lock() = first.map(|(_, msg)| msg);
    first.map_or(Ok(()), Err)
}

// ── Control registers ───────────────────────────────────────────────────
//
// The bootloader sets CR0.WP and EFER.NXE on the BSP only; the AP trampoline
// will bring a CPU into long mode with whatever it needs to get there, not
// with the bits the kernel relies on afterwards. So the BSP records its own
// values of those bits here and every other CPU copies them:
//   CR0.WP  — ring 0 honours read-only pages; without it a kernel write to a
//             COW page succeeds silently and two processes share a frame.
//   CR0.CD/NW — caches on.
//   CR4.PGE — global pages; the TLB API's "flush everything but globals"
//             means something different without it.
//   EFER.NXE — without it bit 63 of every NX PTE is a *reserved* bit, and the
//             first access through one is a page fault.
// CR0.EM/MP and CR4.OSFXSR/OSXMMEXCPT belong to the `sse` step, EFER.SCE to
// `syscall`.

const CR0_MASK: u64 = (1 << 16) | (1 << 29) | (1 << 30); // WP, NW, CD
const CR4_MASK: u64 = 1 << 7; // PGE
const EFER_MASK: u64 = 1 << 11; // NXE
const IA32_EFER: u32 = 0xC000_0080;

/// The BSP's masked CR0, CR4 and EFER.
static REFERENCE: spin::Once<(u64, u64, u64)> = spin::Once::new();

fn read_ctl() -> (u64, u64, u64) {
    let (cr0, cr4): (u64, u64);
    unsafe {
        core::arch::asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
        core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
    }
    // SAFETY: EFER exists on every x86-64 CPU.
    let efer = unsafe { x86_64::registers::model_specific::Msr::new(IA32_EFER).read() };
    (cr0, cr4, efer)
}

fn control_regs_init(cpu: usize) {
    let (cr0, cr4, efer) = read_ctl();
    if cpu == 0 {
        REFERENCE.call_once(|| (cr0 & CR0_MASK, cr4 & CR4_MASK, efer & EFER_MASK));
    }
    let &(r0, r4, re) = REFERENCE.get().expect("the BSP runs init_this_cpu first");
    // SAFETY: only the masked bits change, to the values the BSP — whose
    // page tables every CPU shares — already runs with. EFER first: once
    // NXE is on, CR4/CR0 writes cannot fault on NX PTEs.
    unsafe {
        if efer & EFER_MASK != re {
            x86_64::registers::model_specific::Msr::new(IA32_EFER).write((efer & !EFER_MASK) | re);
        }
        if cr4 & CR4_MASK != r4 {
            let v = (cr4 & !CR4_MASK) | r4;
            core::arch::asm!("mov cr4, {}", in(reg) v, options(nostack, preserves_flags));
        }
        if cr0 & CR0_MASK != r0 {
            let v = (cr0 & !CR0_MASK) | r0;
            core::arch::asm!("mov cr0, {}", in(reg) v, options(nostack, preserves_flags));
        }
    }
}

fn control_regs_verify() -> Result<(), &'static str> {
    let &(r0, r4, re) = REFERENCE.get().ok_or("no BSP reference")?;
    let (cr0, cr4, efer) = read_ctl();
    if cr0 & CR0_MASK != r0 {
        return Err("CR0.WP/CD/NW differ from the BSP's");
    }
    if cr4 & CR4_MASK != r4 {
        return Err("CR4.PGE differs from the BSP's");
    }
    if efer & EFER_MASK != re {
        return Err("EFER.NXE differs from the BSP's");
    }
    Ok(())
}

/// `/proc/kdebug` line: one entry per CPU that has run `init_this_cpu`.
pub fn render() -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::string::String::from("cpu_init:");
    for cpu in 0..MAX_CPUS {
        let bits = VERIFIED[cpu].load(Ordering::Relaxed);
        if bits & DONE == 0 {
            continue;
        }
        let all = (1u32 << STEPS.len()) - 1;
        if bits & all == all {
            let _ = write!(s, " cpu{} ok ({})", cpu, STEPS.len());
        } else {
            let _ = write!(s, " cpu{} FAILED:", cpu);
            for (i, name) in STEPS.iter().enumerate() {
                if bits & (1 << i) == 0 {
                    let _ = write!(s, " {}", name);
                }
            }
            if let Some(msg) = *FIRST_ERROR[cpu].lock() {
                let _ = write!(s, " ({})", msg);
            }
        }
    }
    s
}
