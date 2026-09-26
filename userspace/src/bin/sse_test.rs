#![no_std]
#![no_main]

//! `sse_test`: the userspace target has hardware SSE2 now (see
//! `userspace/x86_64-constanos.json`), and this checks what that relies on:
//!
//! - A: `f64` arithmetic compiles to real SSE and gives IEEE results.
//! - B: XMM registers survive preemption — several processes, each holding
//!   its own pattern in all sixteen registers across a long integer-only
//!   spin, more of them than there are CPUs.
//! - C: a signal handler that overwrites every XMM register and MXCSR does
//!   not leak them into the code it interrupted (the kernel keeps the
//!   interrupted FXSAVE image in the signal frame).
//! - D: a handler that writes garbage into that saved MXCSR does not make
//!   `sigreturn` fault in the kernel; the reserved bits are dropped.
//!
//! B–D hold every register inside one `asm!` block, so nothing the
//! compiler emits can move a value in or out of them behind the test's back.

use core::arch::{asm, naked_asm};
use userspace::{println, syscall};

const MXCSR_DEFAULT: u32 = 0x1F80;

// ── A: arithmetic ───────────────────────────────────────────────────────

/// `sqrtsd`: IEEE requires it correctly rounded, so it must equal the
/// constant bit for bit.
fn sqrt(x: f64) -> f64 {
    use core::arch::x86_64::{_mm_cvtsd_f64, _mm_set_sd, _mm_sqrt_pd};
    unsafe { _mm_cvtsd_f64(_mm_sqrt_pd(_mm_set_sd(x))) }
}

fn case_a() -> bool {
    // `black_box` keeps the computation at run time, in SSE instructions,
    // rather than folded into a constant by the compiler.
    let two = core::hint::black_box(2.0f64);
    let r = sqrt(two);
    let sqrt_ok = r.to_bits() == core::f64::consts::SQRT_2.to_bits();

    // Basel sum to 10^5 terms: pi^2/6 - 1/n within its tail bound.
    let n = core::hint::black_box(100_000u32);
    let mut s = 0.0f64;
    for k in 1..=n {
        let k = k as f64;
        s += 1.0 / (k * k);
    }
    let basel = core::f64::consts::PI * core::f64::consts::PI / 6.0;
    let basel_ok = (basel - s - 1.0 / n as f64).abs() < 1e-9;

    let third = core::hint::black_box(1.0f64) / 3.0;
    let ieee_ok = third.to_bits() == 0x3FD5_5555_5555_5555;

    println!(
        "sse_test: A sqrt(2)={:.17} basel={:.12} 1/3={:#x} -> {}",
        r, s, third.to_bits(), verdict(sqrt_ok && basel_ok && ieee_ok)
    );
    sqrt_ok && basel_ok && ieee_ok
}

// ── Register helpers ────────────────────────────────────────────────────

/// Sixteen XMM registers' worth of bytes, `movdqu`-addressable.
#[repr(C, align(16))]
struct Xmm([u8; 256]);

fn pattern(seed: u64) -> Xmm {
    let mut x = Xmm([0; 256]);
    let mut v = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for chunk in x.0.chunks_mut(8) {
        v ^= v << 13;
        v ^= v >> 7;
        v ^= v << 17;
        chunk.copy_from_slice(&v.to_le_bytes());
    }
    x
}

/// How many of the sixteen registers differ between two images.
fn changed_regs(a: &Xmm, b: &Xmm) -> usize {
    (0..16).filter(|i| a.0[i * 16..i * 16 + 16] != b.0[i * 16..i * 16 + 16]).count()
}

/// This processor's MXCSR_MASK, from an `fxsave` (0 there means the
/// architectural default, 0xFFBF — SDM 11.6.6).
fn mxcsr_mask() -> u32 {
    // 512 bytes, 16-aligned: two `Xmm`s back to back.
    let mut image = [Xmm([0; 256]), Xmm([0; 256])];
    unsafe { asm!("fxsave [{}]", in(reg) image.as_mut_ptr(), options(nostack)) };
    let m = u32::from_le_bytes(image[0].0[28..32].try_into().unwrap());
    if m == 0 { 0xFFBF } else { m }
}

fn read_mxcsr() -> u32 {
    let mut m: u32 = 0;
    unsafe { asm!("stmxcsr [{}]", in(reg) &mut m, options(nostack)) };
    m
}

fn write_mxcsr(m: u32) {
    unsafe { asm!("ldmxcsr [{}]", in(reg) &m, options(nostack)) };
}

// ── B: preemption ───────────────────────────────────────────────────────

/// Load `pat` into xmm0-15, spin on the TSC for `cycles` touching only
/// general-purpose registers, store xmm0-15 into `out`.
fn hold_across_spin(pat: &Xmm, out: &mut Xmm, cycles: u64) {
    unsafe {
        asm!(
            "movdqu xmm0,  [r8 + 0x00]", "movdqu xmm1,  [r8 + 0x10]",
            "movdqu xmm2,  [r8 + 0x20]", "movdqu xmm3,  [r8 + 0x30]",
            "movdqu xmm4,  [r8 + 0x40]", "movdqu xmm5,  [r8 + 0x50]",
            "movdqu xmm6,  [r8 + 0x60]", "movdqu xmm7,  [r8 + 0x70]",
            "movdqu xmm8,  [r8 + 0x80]", "movdqu xmm9,  [r8 + 0x90]",
            "movdqu xmm10, [r8 + 0xa0]", "movdqu xmm11, [r8 + 0xb0]",
            "movdqu xmm12, [r8 + 0xc0]", "movdqu xmm13, [r8 + 0xd0]",
            "movdqu xmm14, [r8 + 0xe0]", "movdqu xmm15, [r8 + 0xf0]",
            "rdtsc",
            "shl rdx, 32",
            "or rax, rdx",
            "lea r10, [rax + r11]",
            "2:",
            "rdtsc",
            "shl rdx, 32",
            "or rax, rdx",
            "cmp rax, r10",
            "jb 2b",
            "movdqu [r9 + 0x00], xmm0",  "movdqu [r9 + 0x10], xmm1",
            "movdqu [r9 + 0x20], xmm2",  "movdqu [r9 + 0x30], xmm3",
            "movdqu [r9 + 0x40], xmm4",  "movdqu [r9 + 0x50], xmm5",
            "movdqu [r9 + 0x60], xmm6",  "movdqu [r9 + 0x70], xmm7",
            "movdqu [r9 + 0x80], xmm8",  "movdqu [r9 + 0x90], xmm9",
            "movdqu [r9 + 0xa0], xmm10", "movdqu [r9 + 0xb0], xmm11",
            "movdqu [r9 + 0xc0], xmm12", "movdqu [r9 + 0xd0], xmm13",
            "movdqu [r9 + 0xe0], xmm14", "movdqu [r9 + 0xf0], xmm15",
            in("r8") pat.0.as_ptr(),
            in("r9") out.0.as_mut_ptr(),
            in("r11") cycles,
            out("rax") _, out("rdx") _, out("r10") _,
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _,
            out("xmm4") _, out("xmm5") _, out("xmm6") _, out("xmm7") _,
            out("xmm8") _, out("xmm9") _, out("xmm10") _, out("xmm11") _,
            out("xmm12") _, out("xmm13") _, out("xmm14") _, out("xmm15") _,
            options(nostack),
        );
    }
}

const B_CHILDREN: i64 = 32;
const B_SPIN_CYCLES: u64 = 300_000_000;

fn case_b() -> bool {
    let switches_before = switches_total();
    let mut pids = [0i64; B_CHILDREN as usize];
    for (i, slot) in pids.iter_mut().enumerate() {
        let pid = syscall::fork();
        if pid == 0 {
            let pat = pattern(0xB000 + i as u64);
            let mut out = Xmm([0; 256]);
            hold_across_spin(&pat, &mut out, B_SPIN_CYCLES);
            syscall::exit(changed_regs(&pat, &out) as i32);
        }
        if pid < 0 {
            println!("sse_test: B fork failed ({})", pid);
            return false;
        }
        *slot = pid;
    }
    // Each child's exit code is how many registers came back changed.
    // This kernel's status word: `0x200 | code` for a normal exit.
    let mut bad = 0;
    let mut regs = 0;
    for pid in pids {
        let (_, status) = syscall::waitpid_status(pid);
        if status & 0x200 == 0 {
            println!("sse_test: B child {} did not exit normally (status {:#x})", pid, status);
            bad += 1;
        } else if status & 0xFF != 0 {
            bad += 1;
            regs += status & 0xFF;
        }
    }
    let switches = switches_total().saturating_sub(switches_before);
    println!(
        "sse_test: B {} processes, {} corrupted ({} registers), {} context switches meanwhile -> {}",
        B_CHILDREN, bad, regs, switches, verdict(bad == 0)
    );
    bad == 0
}

/// `switches_total` from `/proc/kdebug` — evidence that B actually ran
/// across preemptions, not just beside them.
fn switches_total() -> u64 {
    let mut buf = [0u8; 8192];
    let fd = syscall::with_cstr("/proc/kdebug", |p| syscall::open(p, 0));
    if fd < 0 {
        return 0;
    }
    let mut len = 0;
    loop {
        let n = syscall::read(fd as i32, &mut buf[len..]);
        if n <= 0 {
            break;
        }
        len += n as usize;
        if len == buf.len() {
            break;
        }
    }
    syscall::close(fd as i32);
    let text = core::str::from_utf8(&buf[..len]).unwrap_or("");
    for line in text.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("switches_total") {
            let digits = rest.trim_start_matches(|c: char| !c.is_ascii_digit());
            let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
            return digits[..end].parse().unwrap_or(0);
        }
    }
    0
}

// ── C/D: signal handlers ────────────────────────────────────────────────

/// Clobbers every XMM register and MXCSR, then returns.
#[unsafe(naked)]
extern "C" fn clobber_handler(_sig: i32) {
    naked_asm!(
        "pcmpeqd xmm0, xmm0", "pcmpeqd xmm1, xmm1",
        "pcmpeqd xmm2, xmm2", "pcmpeqd xmm3, xmm3",
        "pcmpeqd xmm4, xmm4", "pcmpeqd xmm5, xmm5",
        "pcmpeqd xmm6, xmm6", "pcmpeqd xmm7, xmm7",
        "pcmpeqd xmm8, xmm8", "pcmpeqd xmm9, xmm9",
        "pcmpeqd xmm10, xmm10", "pcmpeqd xmm11, xmm11",
        "pcmpeqd xmm12, xmm12", "pcmpeqd xmm13, xmm13",
        "pcmpeqd xmm14, xmm14", "pcmpeqd xmm15, xmm15",
        // Round toward zero, flush-to-zero: nothing like the default.
        "push 0xFF80",
        "ldmxcsr [rsp]",
        "add rsp, 8",
        "ret",
    )
}

/// Sets every bit of the MXCSR saved in its own signal frame. On entry
/// `rsp` is the trampoline return slot and the frame starts 8 bytes above
/// it with the FXSAVE image, whose MXCSR is at offset 24 (see
/// `kernel/src/process/signal.rs::SignalFrame`).
#[unsafe(naked)]
extern "C" fn corrupt_mxcsr_handler(_sig: i32) {
    naked_asm!("mov dword ptr [rsp + 32], 0xFFFFFFFF", "ret")
}

/// Load `pat` into xmm0-15, `kill(pid, sig)` (delivered to ourselves on the
/// way out of that very syscall), store xmm0-15 into `out`.
fn hold_across_signal(pat: &Xmm, out: &mut Xmm, pid: i64, sig: u32) -> i64 {
    let ret: i64;
    unsafe {
        asm!(
            "movdqu xmm0,  [r8 + 0x00]", "movdqu xmm1,  [r8 + 0x10]",
            "movdqu xmm2,  [r8 + 0x20]", "movdqu xmm3,  [r8 + 0x30]",
            "movdqu xmm4,  [r8 + 0x40]", "movdqu xmm5,  [r8 + 0x50]",
            "movdqu xmm6,  [r8 + 0x60]", "movdqu xmm7,  [r8 + 0x70]",
            "movdqu xmm8,  [r8 + 0x80]", "movdqu xmm9,  [r8 + 0x90]",
            "movdqu xmm10, [r8 + 0xa0]", "movdqu xmm11, [r8 + 0xb0]",
            "movdqu xmm12, [r8 + 0xc0]", "movdqu xmm13, [r8 + 0xd0]",
            "movdqu xmm14, [r8 + 0xe0]", "movdqu xmm15, [r8 + 0xf0]",
            "syscall",
            "movdqu [r9 + 0x00], xmm0",  "movdqu [r9 + 0x10], xmm1",
            "movdqu [r9 + 0x20], xmm2",  "movdqu [r9 + 0x30], xmm3",
            "movdqu [r9 + 0x40], xmm4",  "movdqu [r9 + 0x50], xmm5",
            "movdqu [r9 + 0x60], xmm6",  "movdqu [r9 + 0x70], xmm7",
            "movdqu [r9 + 0x80], xmm8",  "movdqu [r9 + 0x90], xmm9",
            "movdqu [r9 + 0xa0], xmm10", "movdqu [r9 + 0xb0], xmm11",
            "movdqu [r9 + 0xc0], xmm12", "movdqu [r9 + 0xd0], xmm13",
            "movdqu [r9 + 0xe0], xmm14", "movdqu [r9 + 0xf0], xmm15",
            in("r8") pat.0.as_ptr(),
            in("r9") out.0.as_mut_ptr(),
            inlateout("rax") 62i64 => ret, // kill
            in("rdi") pid,
            in("rsi") sig as u64,
            out("rcx") _, out("r11") _,
            out("xmm0") _, out("xmm1") _, out("xmm2") _, out("xmm3") _,
            out("xmm4") _, out("xmm5") _, out("xmm6") _, out("xmm7") _,
            out("xmm8") _, out("xmm9") _, out("xmm10") _, out("xmm11") _,
            out("xmm12") _, out("xmm13") _, out("xmm14") _, out("xmm15") _,
        );
    }
    ret
}

fn case_c() -> bool {
    if syscall::sigaction(syscall::SIGUSR1, clobber_handler as *const () as u64) < 0 {
        println!("sse_test: C sigaction failed");
        return false;
    }
    let pat = pattern(0xC000);
    let mut out = Xmm([0; 256]);
    write_mxcsr(MXCSR_DEFAULT);
    let r = hold_across_signal(&pat, &mut out, syscall::getpid(), syscall::SIGUSR1);
    let mxcsr = read_mxcsr();
    write_mxcsr(MXCSR_DEFAULT);
    syscall::sigaction(syscall::SIGUSR1, 0);

    let regs = changed_regs(&pat, &out);
    let ok = r == 0 && regs == 0 && mxcsr == MXCSR_DEFAULT;
    println!(
        "sse_test: C kill={} xmm registers changed by the handler: {}/16, mxcsr={:#x} -> {}",
        r, regs, mxcsr, verdict(ok)
    );
    ok
}

fn case_d() -> bool {
    if syscall::sigaction(syscall::SIGUSR2, corrupt_mxcsr_handler as *const () as u64) < 0 {
        println!("sse_test: D sigaction failed");
        return false;
    }
    let pat = pattern(0xD000);
    let mut out = Xmm([0; 256]);
    write_mxcsr(MXCSR_DEFAULT);
    let r = hold_across_signal(&pat, &mut out, syscall::getpid(), syscall::SIGUSR2);
    let mxcsr = read_mxcsr();
    write_mxcsr(MXCSR_DEFAULT);
    syscall::sigaction(syscall::SIGUSR2, 0);

    // Reaching this line at all is the main result: an unsanitized image
    // would have #GP'd in the kernel's fxrstor. What comes back must be
    // exactly the bits this processor allows — not a fixed 0xFFFF: AMD's
    // MXCSR_MASK is 0x2FFFF (bit 17, the misaligned-exception mask), found
    // on the Ryzen when this assumed bits 16-31 reserved everywhere.
    let expected = 0xFFFF_FFFF & mxcsr_mask();
    let ok = r == 0 && mxcsr == expected && out.0 == pat.0;
    println!(
        "sse_test: D mxcsr after sigreturn={:#x}, MXCSR_MASK {:#x} (frame had 0xffffffff) -> {}",
        mxcsr, expected, verdict(ok)
    );
    ok
}

fn verdict(ok: bool) -> &'static str {
    if ok { "ok" } else { "FAIL" }
}

userspace::entry!(main);

fn main(_args: userspace::args::Args) -> i32 {
    let results = [case_a(), case_b(), case_c(), case_d()];
    let pass = results.iter().all(|&r| r);
    println!("sse_test: {}", if pass { "PASS" } else { "FAIL" });
    if pass { 0 } else { 1 }
}
