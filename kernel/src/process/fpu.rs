// kernel/src/process/fpu.rs
//
// Per-process FPU/SSE/AVX state (ymm0-15, x87, MXCSR) save/restore across
// context switches via `xsave`/`xrstor` (`fxsave`/`fxrstor`, xmm only, on a
// CPU without AVX). Previously `TrapFrame` only
// carried general-purpose registers — fine for everything that runs today
// (BusyBox, mlibc, the C tests, DOOM's deliberately-fixed-point engine),
// but any real floating-point-heavy program would see its XMM/x87 state
// silently corrupted by a preemption landing mid-computation, since
// nothing ever saved or restored it.
//
// `init_this_cpu()` (CR0/CR4, per CPU — run from `cpu::init_this_cpu`)
// must run before any process is created; `init()` then captures
// `TEMPLATE`, a real `fxsave` of the resulting clean reset state, used to initialize every new process/thread
// and to reset on `exec()` (real `execve()` resets FPU state too).
// `sys_fork` is the one exception: a forked child gets a *copy* of the
// parent's actual live registers (real `fork()` semantics), not the
// template — see `syscall::sys_fork`.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};

/// XCR0 with x87, SSE and AVX: the state components this kernel enables.
/// Nothing beyond AVX (no AVX-512, no MPK): the target is Zen 3.
const XCR0: u64 = 0b111;
/// A standard-format XSAVE image for `XCR0`: the 512-byte legacy region
/// (the FXSAVE image, same offsets), the 64-byte XSAVE header, and the
/// upper halves of ymm0-15 at offset 576.
const XSAVE_SIZE: usize = 832;
const XSAVE_HEADER: usize = 512;

/// Whether this machine switches state with `xsave` (AVX enabled) rather
/// than `fxsave`. Decided by `init_this_cpu` on the BSP, before any process
/// exists; every CPU is the same model, so the APs agree.
static USE_XSAVE: AtomicBool = AtomicBool::new(false);

/// One `xsave`/`xrstor` image (or an `fxsave`/`fxrstor` one in its first
/// 512 bytes). `xsave` faults (#GP) unless the operand is 64-byte aligned,
/// hence `repr(align)`.
#[repr(C, align(64))]
#[derive(Clone)]
pub struct FpuState(pub [u8; XSAVE_SIZE]);

impl FpuState {
    /// All zeros — also a valid, empty XSAVE header, which `xsave` leaves
    /// alone apart from XSTATE_BV and `xrstor` requires zero.
    pub const fn zeroed() -> FpuState {
        FpuState([0; XSAVE_SIZE])
    }
}

static TEMPLATE: spin::Once<FpuState> = spin::Once::new();

/// Capture the clean reset state as the template every new process/thread
/// starts from. Call exactly once at boot, after `cpu::init_this_cpu` has
/// enabled SSE on the BSP and before `init::processes::init_all()` creates
/// the first `Process`.
pub fn init() {
    if let Err(e) = verify_this_cpu() {
        panic!("fpu::init before SSE is enabled: {}", e);
    }
    let mut area = FpuState::zeroed();
    unsafe {
        save(&mut area);
    }
    TEMPLATE.call_once(|| area);
    let kind = if USE_XSAVE.load(Ordering::Relaxed) { "XSAVE (x87+SSE+AVX)" } else { "FXSAVE" };
    crate::serial_println!("fpu: default {} template captured", kind);
}

/// CR0.EM=0 (no #NM trap on SSE/x87 instructions — this kernel isn't
/// lazily switching FPU state, it's unconditionally saved/restored on
/// every context switch, so there's no reason to trap), CR0.MP=1 (so a
/// `wait`/FPU instruction that should trap under TS still does — real
/// hardware convention, harmless since TS is never set here either),
/// CR4.OSFXSR=1 (enables `fxsave`/`fxrstor` and legacy SSE), CR4.OSXMMEXCPT=1
/// (unmasked SIMD FP exceptions reported via #XM instead of silently
/// disabled — matches what every real OS sets).
///
/// Then, if the CPU has XSAVE and AVX: CR4.OSXSAVE=1 and XCR0=x87|SSE|AVX,
/// so user code may use AVX/AVX2 (without OSXSAVE every VEX instruction is
/// #UD) and the switch points save the full ymm registers.
pub fn init_this_cpu() {
    // SAFETY: only SSE/x87/AVX enable bits change; the kernel itself is
    // built soft-float, so nothing in flight depends on their old values.
    unsafe { enable_sse() }
    if avx_supported() {
        unsafe { enable_xsave() }
        USE_XSAVE.store(true, Ordering::Relaxed);
    }
}

const CR0_EM: u64 = 1 << 2;
const CR0_MP: u64 = 1 << 1;
const CR4_SSE: u64 = (1 << 9) | (1 << 10); // OSFXSR, OSXMMEXCPT
const CR4_OSXSAVE: u64 = 1 << 18;

/// XSAVE and AVX in CPUID, XCR0 able to hold x87|SSE|AVX, and a standard
/// image for them that fits `FpuState`.
fn avx_supported() -> bool {
    let leaf1 = core::arch::x86_64::__cpuid(1);
    if leaf1.ecx & (1 << 26) == 0 || leaf1.ecx & (1 << 28) == 0 {
        return false; // no XSAVE or no AVX
    }
    // EAX of leaf 0xD: the XCR0 bits this CPU supports.
    let leaf_d = core::arch::x86_64::__cpuid_count(0xD, 0);
    leaf_d.eax as u64 & XCR0 == XCR0 && avx_image_fits()
}

/// Where CPUID puts the AVX component (index 2) in the standard format:
/// it must be where `XSAVE_SIZE` assumes, right after the header.
fn avx_image_fits() -> bool {
    let avx = core::arch::x86_64::__cpuid_count(0xD, 2);
    avx.ebx as usize == XSAVE_HEADER + 64 && (avx.ebx + avx.eax) as usize <= XSAVE_SIZE
}

fn xgetbv0() -> u64 {
    let (lo, hi): (u32, u32);
    unsafe { asm!("xgetbv", in("ecx") 0, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) };
    (hi as u64) << 32 | lo as u64
}

unsafe fn enable_xsave() {
    let mut cr4: u64;
    unsafe { asm!("mov {}, cr4", out(reg) cr4, options(nostack, preserves_flags)); }
    cr4 |= CR4_OSXSAVE;
    unsafe { asm!("mov cr4, {}", in(reg) cr4, options(nostack, preserves_flags)); }
    unsafe {
        asm!("xsetbv", in("ecx") 0, in("eax") XCR0 as u32, in("edx") (XCR0 >> 32) as u32,
             options(nomem, nostack, preserves_flags));
    }
}

/// Reads back what `init_this_cpu` set.
pub fn verify_this_cpu() -> Result<(), &'static str> {
    let (cr0, cr4): (u64, u64);
    unsafe {
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
    }
    if cr0 & CR0_EM != 0 || cr0 & CR0_MP == 0 {
        return Err("CR0.EM/MP not set up for SSE");
    }
    if cr4 & CR4_SSE != CR4_SSE {
        return Err("CR4.OSFXSR/OSXMMEXCPT clear");
    }
    if USE_XSAVE.load(Ordering::Relaxed) {
        if cr4 & CR4_OSXSAVE == 0 {
            return Err("CR4.OSXSAVE clear");
        }
        if xgetbv0() != XCR0 {
            return Err("XCR0 is not x87|SSE|AVX");
        }
    }
    Ok(())
}

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
        if USE_XSAVE.load(Ordering::Relaxed) {
            asm!("xsave [{}]", in(reg) area.0.as_mut_ptr(),
                 in("eax") XCR0 as u32, in("edx") (XCR0 >> 32) as u32, options(nostack));
        } else {
            asm!("fxsave [{}]", in(reg) area.0.as_mut_ptr(), options(nostack));
        }
    }
}

/// Restore the FPU/SSE register state from `area`. Called on every
/// incoming context switch (the process about to start running).
#[inline(always)]
pub unsafe fn restore(area: &FpuState) {
    unsafe {
        if USE_XSAVE.load(Ordering::Relaxed) {
            asm!("xrstor [{}]", in(reg) area.0.as_ptr(),
                 in("eax") XCR0 as u32, in("edx") (XCR0 >> 32) as u32, options(nostack));
        } else {
            asm!("fxrstor [{}]", in(reg) area.0.as_ptr(), options(nostack));
        }
    }
}

/// Byte offsets of MXCSR and MXCSR_MASK in an FXSAVE image (SDM vol. 1,
/// table 10-2).
const MXCSR_OFFSET: usize = 24;
const MXCSR_MASK_OFFSET: usize = 28;
/// What MXCSR_MASK means when the processor stores 0 there (SDM 11.6.6).
const MXCSR_MASK_DEFAULT: u32 = 0xFFBF;

/// Clear the MXCSR bits this processor reserves, so `restore` of an image
/// that came from user memory — a signal frame the handler may have
/// scribbled on — cannot #GP inside the kernel. The mask comes from the
/// boot-captured template, never from `area` itself: that copy is user
/// data too. Linux does the same in `fpu__restore_sig`.
///
/// With XSAVE, also the header, which `xrstor` checks just as strictly:
/// XSTATE_BV limited to XCR0, XCOMP_BV (standard format) and the reserved
/// bytes zero.
pub fn sanitize(area: &mut FpuState) {
    let template = TEMPLATE
        .get()
        .expect("fpu::init() must run before any process is created");
    let word = |a: &[u8; XSAVE_SIZE], at: usize| u32::from_le_bytes(a[at..at + 4].try_into().unwrap());
    let mask = match word(&template.0, MXCSR_MASK_OFFSET) {
        0 => MXCSR_MASK_DEFAULT,
        m => m,
    };
    let mxcsr = word(&area.0, MXCSR_OFFSET) & mask;
    area.0[MXCSR_OFFSET..MXCSR_OFFSET + 4].copy_from_slice(&mxcsr.to_le_bytes());
    let header = &mut area.0[XSAVE_HEADER..XSAVE_HEADER + 64];
    let xstate_bv = u64::from_le_bytes(header[..8].try_into().unwrap()) & XCR0;
    header.fill(0);
    header[..8].copy_from_slice(&xstate_bv.to_le_bytes());
}
