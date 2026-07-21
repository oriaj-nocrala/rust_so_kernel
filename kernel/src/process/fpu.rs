// kernel/src/process/fpu.rs
//
// Per-process FPU/SSE state (xmm0-15, x87, MXCSR) save/restore across
// context switches via `fxsave`/`fxrstor`. Previously `TrapFrame` only
// carried general-purpose registers — fine for everything that runs today
// (BusyBox, mlibc, the C tests, DOOM's deliberately-fixed-point engine),
// but any real floating-point-heavy program would see its XMM/x87 state
// silently corrupted by a preemption landing mid-computation, since
// nothing ever saved or restored it.
//
// `enable_sse()` must run once at boot, before any process is created —
// `init()` does both that and capturing `TEMPLATE`, a real `fxsave` of the
// resulting clean reset state, used to initialize every new process/thread
// and to reset on `exec()` (real `execve()` resets FPU state too).
// `sys_fork` is the one exception: a forked child gets a *copy* of the
// parent's actual live registers (real `fork()` semantics), not the
// template — see `syscall::sys_fork`.

use core::arch::asm;

/// One `fxsave`/`fxrstor` image: 512 bytes, and the instructions fault
/// (#GP) if the memory operand isn't 16-byte aligned, hence `repr(align)`.
#[repr(C, align(16))]
#[derive(Clone)]
pub struct FpuState(pub [u8; 512]);

static TEMPLATE: spin::Once<FpuState> = spin::Once::new();

/// Enable SSE and capture the resulting clean reset state as the template
/// every new process/thread starts from. Call exactly once at boot,
/// before `init::processes::init_all()` creates the first `Process`.
pub fn init() {
    unsafe {
        enable_sse();
    }
    let mut area = FpuState([0u8; 512]);
    unsafe {
        save(&mut area);
    }
    TEMPLATE.call_once(|| area);
    crate::serial_println!("fpu: SSE enabled, default FXSAVE template captured");
}

/// CR0.EM=0 (no #NM trap on SSE/x87 instructions — this kernel isn't
/// lazily switching FPU state, it's unconditionally saved/restored on
/// every context switch, so there's no reason to trap), CR0.MP=1 (so a
/// `wait`/FPU instruction that should trap under TS still does — real
/// hardware convention, harmless since TS is never set here either),
/// CR4.OSFXSR=1 (enables `fxsave`/`fxrstor` and legacy SSE), CR4.OSXMMEXCPT=1
/// (unmasked SIMD FP exceptions reported via #XM instead of silently
/// disabled — matches what every real OS sets).
unsafe fn enable_sse() {
    let mut cr0: u64;
    unsafe { asm!("mov {}, cr0", out(reg) cr0, options(nostack, preserves_flags)); }
    cr0 &= !(1 << 2); // EM = 0
    cr0 |= 1 << 1; // MP = 1
    unsafe { asm!("mov cr0, {}", in(reg) cr0, options(nostack, preserves_flags)); }

    let mut cr4: u64;
    unsafe { asm!("mov {}, cr4", out(reg) cr4, options(nostack, preserves_flags)); }
    cr4 |= (1 << 9) | (1 << 10); // OSFXSR, OSXMMEXCPT
    unsafe { asm!("mov cr4, {}", in(reg) cr4, options(nostack, preserves_flags)); }
}

/// A fresh copy of the boot-captured default FPU/SSE state — used to
/// initialize every new process/thread (`Process::new_kernel`/`new_user`/
/// `new_thread`) and to reset on `exec()`.
pub fn default_state() -> FpuState {
    TEMPLATE
        .get()
        .expect("fpu::init() must run before any process is created")
        .clone()
}

/// Save the live FPU/SSE register state into `area`. Called on every
/// outgoing context switch (the process about to stop running), and by
/// `sys_fork` to capture the parent's *current* registers for the child
/// (which may differ from whatever was last saved at its previous
/// preemption — this process has been running live since then).
#[inline(always)]
pub unsafe fn save(area: &mut FpuState) {
    unsafe {
        asm!("fxsave [{}]", in(reg) area.0.as_mut_ptr(), options(nostack));
    }
}

/// Restore the FPU/SSE register state from `area`. Called on every
/// incoming context switch (the process about to start running).
#[inline(always)]
pub unsafe fn restore(area: &FpuState) {
    unsafe {
        asm!("fxrstor [{}]", in(reg) area.0.as_ptr(), options(nostack));
    }
}
