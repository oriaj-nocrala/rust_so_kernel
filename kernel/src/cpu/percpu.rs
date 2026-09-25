// kernel/src/cpu/percpu.rs
//
// Per-CPU data (stage 2 of docs/smp/smp-plan.md).
//
// DESIGN: `gs` is a scratch pointer for exactly one piece of code — the
// `syscall` entry stub — and nothing else in the kernel ever reads it.
//
// `syscall` is the one entry with no free register and no kernel stack: it
// needs *this CPU's* kernel stack top before it can push anything. So
// `syscall_entry_fast` does `swapgs`, saves the user RSP and loads the kernel
// RSP through `gs:`, pushes the user RSP, and `swapgs`es straight back — four
// instructions, IF=0. Everywhere else (the timer stub, every `x86-interrupt`
// handler, `jump_to_trapframe`) GS is left exactly as user mode had it.
//
// Why not Linux's "GS_BASE is per-CPU whenever the kernel runs": that needs a
// matching `swapgs` on every ring-3 entry *and* exit, and rustc's
// `x86-interrupt` shims do neither — a page fault from user mode would run
// its Rust handler with the user's GS, and the kill path leaves through
// `jump_to_trapframe`, which could not know whether its entry had swapped.
// A `swapgs` too many or too few does not fail on the spot; it fails later,
// somewhere else. Here the pair sits four instructions apart in one stub, so
// there is nothing to get out of step.
//
// The invariant, then: outside that window, IA32_KERNEL_GS_BASE holds
// `&PERCPU[cpu]` and IA32_GS_BASE holds whatever user mode left there (which
// the kernel never reads, so user code changing it — a `mov %gs` load,
// `wrgsbase` — cannot hurt the kernel). `check_gs_invariant` asserts it on
// every timer tick.
//
// Rust code reaches its CPU's data through `cpu_id()`, which reads the task
// register instead of `gs:` — valid on every path, whatever GS holds. Each CPU
// has its own TSS descriptor slot in the one GDT (`process::tss`), so `str`
// names the CPU.

use core::sync::atomic::{AtomicU64, Ordering};
use super::MAX_CPUS;

/// One CPU's data. `#[repr(C)]` because `syscall_entry_fast` addresses the
/// fields by offset (`OFF_*`).
#[repr(C, align(64))]
pub struct PerCpu {
    /// Its own address — what `check_gs_invariant` compares against.
    self_ptr: AtomicU64,
    /// Top of the running process's kernel stack: where `syscall` lands, and
    /// mirrored into this CPU's `TSS.rsp0` for interrupts from ring 3.
    kernel_rsp: AtomicU64,
    /// Scratch for `syscall_entry_fast`: the user RSP between the stack
    /// switch and the push that saves it. Meaningless outside the stub.
    user_rsp_scratch: AtomicU64,
    cpu_id: AtomicU64,
}

pub const OFF_KERNEL_RSP: usize = core::mem::offset_of!(PerCpu, kernel_rsp);
pub const OFF_USER_RSP_SCRATCH: usize = core::mem::offset_of!(PerCpu, user_rsp_scratch);

const fn new_percpu() -> PerCpu {
    PerCpu {
        self_ptr: AtomicU64::new(0),
        kernel_rsp: AtomicU64::new(0),
        user_rsp_scratch: AtomicU64::new(0),
        cpu_id: AtomicU64::new(0),
    }
}

static PERCPU: [PerCpu; MAX_CPUS] = [const { new_percpu() }; MAX_CPUS];

const IA32_GS_BASE: u32 = 0xC000_0101;
const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;

/// The GDT selector of CPU 0's TSS; CPU n's is `FIRST_TSS_SELECTOR + 16 * n`
/// (a TSS descriptor is 16 bytes in long mode). `process::tss` asserts the
/// GDT really puts it there.
pub const FIRST_TSS_SELECTOR: u16 = 0x28;

/// Set up this CPU's `PerCpu` and point `IA32_KERNEL_GS_BASE` at it. Must run
/// before the first `syscall` can execute. Per-CPU step of
/// `cpu::init_this_cpu`.
pub fn init_this_cpu(cpu: usize) {
    let pc = &PERCPU[cpu];
    let addr = pc as *const PerCpu as u64;
    pc.self_ptr.store(addr, Ordering::Relaxed);
    pc.cpu_id.store(cpu as u64, Ordering::Relaxed);
    unsafe {
        // GS_BASE is user mode's; start it at 0, which is also what loading
        // any user selector into %gs would give.
        wrmsr(IA32_GS_BASE, 0);
        wrmsr(IA32_KERNEL_GS_BASE, addr);
    }
}

/// This CPU's id, from the task register: each CPU loads its own TSS
/// selector in `cpu::init_this_cpu`. Before that TR reads 0 — only the BSP
/// runs that early, so that is CPU 0.
#[inline(always)]
pub fn cpu_id() -> usize {
    let tr: u16;
    unsafe {
        core::arch::asm!("str {0:x}", out(reg) tr, options(nomem, nostack, preserves_flags));
    }
    cpu_from_tr(tr)
}

#[inline(always)]
fn cpu_from_tr(tr: u16) -> usize {
    if tr < FIRST_TSS_SELECTOR {
        return 0;
    }
    let cpu = ((tr - FIRST_TSS_SELECTOR) / 16) as usize;
    debug_assert!(cpu < MAX_CPUS, "TR {:#x} names no CPU", tr);
    cpu
}

/// Record the running process's kernel stack top for this CPU (the TSS's
/// `rsp0` is written alongside, in `process::tss::set_kernel_stack`).
pub fn set_kernel_rsp(top: u64) {
    PERCPU[cpu_id()].kernel_rsp.store(top, Ordering::Relaxed);
}

/// The running process's kernel stack top on this CPU; 0 before any process
/// has run.
pub fn kernel_rsp() -> u64 {
    PERCPU[cpu_id()].kernel_rsp.load(Ordering::Relaxed)
}

/// Panic unless `IA32_KERNEL_GS_BASE` points at this CPU's own `PerCpu`, and
/// that `PerCpu` knows its own address. Called on every syscall (right
/// after the stub's GS window) and every timer tick: one `rdmsr` each. The
/// only code that swaps GS is `syscall_entry_fast`; if a change there ever
/// leaves the pair out of step, the next `syscall` would load its kernel
/// stack from wherever the user's GS_BASE points and die as a double fault
/// far from the cause. Checking on the syscall itself names it instead —
/// verified by deleting the stub's second `swapgs`: without this call that
/// sabotage was a bare DOUBLE FAULT, with it the panic below.
pub fn check_gs_invariant() {
    let cpu = cpu_id();
    let expected = &PERCPU[cpu] as *const PerCpu as u64;
    let kgs = unsafe { rdmsr(IA32_KERNEL_GS_BASE) };
    let self_ptr = PERCPU[cpu].self_ptr.load(Ordering::Relaxed);
    if kgs != expected || self_ptr != expected {
        let gs = unsafe { rdmsr(IA32_GS_BASE) };
        panic!(
            "per-CPU GS invariant broken on cpu {}: KERNEL_GS_BASE={:#x} GS_BASE={:#x} expected={:#x} self={:#x}",
            cpu, kgs, gs, expected, self_ptr
        );
    }
}

/// `check_gs_invariant` without the panic, for `cpu::init_this_cpu`'s
/// read-back.
pub fn verify_this_cpu(cpu: usize) -> Result<(), &'static str> {
    let expected = &PERCPU[cpu] as *const PerCpu as u64;
    if unsafe { rdmsr(IA32_KERNEL_GS_BASE) } != expected {
        return Err("KERNEL_GS_BASE is not this CPU's PerCpu");
    }
    if PERCPU[cpu].self_ptr.load(Ordering::Relaxed) != expected
        || PERCPU[cpu].cpu_id.load(Ordering::Relaxed) != cpu as u64
    {
        return Err("PerCpu not initialised for this CPU");
    }
    Ok(())
}

/// Cycles per `cpu_id()`, measured once at boot for `/proc/kdebug` (stage 2
/// of the SMP plan chose `str` over RDPID or the LAPIC ID by this number).
static CPU_ID_CYCLES_X100: AtomicU64 = AtomicU64::new(0);
static RDPID_CYCLES_X100: AtomicU64 = AtomicU64::new(0);

/// Time 1000 `cpu_id()` calls, and 1000 RDPIDs when the CPU has it, for
/// comparison. Needs the TSC calibrated; values are x100 to keep a decimal.
pub fn measure_cpu_id_cost() {
    const N: u64 = 1000;
    let t0 = crate::cpu::tsc::read();
    for _ in 0..N {
        core::hint::black_box(cpu_id());
    }
    let t1 = crate::cpu::tsc::read();
    CPU_ID_CYCLES_X100.store((t1 - t0) * 100 / N, Ordering::Relaxed);

    // CPUID.(EAX=7,ECX=0):ECX[22] = RDPID.
    let has_rdpid = core::arch::x86_64::__cpuid_count(7, 0).ecx & (1 << 22) != 0;
    if has_rdpid {
        let t0 = crate::cpu::tsc::read();
        for _ in 0..N {
            let v: u64;
            unsafe {
                core::arch::asm!("rdpid {0}", out(reg) v, options(nomem, nostack, preserves_flags));
            }
            core::hint::black_box(v);
        }
        let t1 = crate::cpu::tsc::read();
        RDPID_CYCLES_X100.store((t1 - t0) * 100 / N, Ordering::Relaxed);
    }
}

/// `/proc/kdebug` line.
pub fn render() -> alloc::string::String {
    let fmt = |x100: u64| alloc::format!("{}.{:02}", x100 / 100, x100 % 100);
    let c = CPU_ID_CYCLES_X100.load(Ordering::Relaxed);
    let r = RDPID_CYCLES_X100.load(Ordering::Relaxed);
    alloc::format!(
        "percpu: cpu_id() via str = {} (TSC cycles/call; rdpid {})",
        fmt(c),
        if r == 0 { alloc::string::String::from("n/a") } else { fmt(r) },
    )
}

#[inline]
unsafe fn wrmsr(msr: u32, value: u64) {
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nostack, nomem),
        );
    }
}

#[inline]
unsafe fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") lo,
            out("edx") hi,
            options(nostack, nomem),
        );
    }
    lo as u64 | ((hi as u64) << 32)
}
