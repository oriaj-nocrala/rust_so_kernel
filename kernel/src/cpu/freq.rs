// kernel/src/cpu/freq.rs
//
// Each CPU's real running frequency, from its own APERF/MPERF (the
// arithmetic, and why those two counters, is `hal::cpufreq`). What
// `/proc/cpuinfo`'s `cpu MHz` reports per core, and `cpumon` draws.
//
// Per-CPU work on the timer tick: a CPU's MSRs can only be read by that
// CPU, so every scheduling CPU samples its own pair in
// `timer_preempt_handler`, before the scheduler lock — two `rdmsr`s at
// 100 Hz, next to `percpu::check_gs_invariant`'s. The result goes into
// `KHZ[cpu]`, which anyone reads.

use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use diag::IrqMutex;
use hal::cpufreq::{self, Window};

use super::MAX_CPUS;
use crate::allocator::KernelIrq;

pub(super) const IA32_MPERF: u32 = 0xE7;
const IA32_APERF: u32 = 0xE8;

const UNKNOWN: u8 = 0;
const YES: u8 = 1;
const NO: u8 = 2;

/// Whether this CPU model has the counters (CPUID 6, ECX bit 0); every
/// core of one package answers the same, so the BSP decides for all.
static SUPPORTED: AtomicU8 = AtomicU8::new(UNKNOWN);

/// Each CPU's window, touched only by that CPU from its own tick.
static WINDOWS: [IrqMutex<Window, KernelIrq>; MAX_CPUS] =
    [const { IrqMutex::new(Window { last_aperf: 0, last_mperf: 0, acc_aperf: 0, acc_mperf: 0, primed: false }) }; MAX_CPUS];

/// Each CPU's last measured frequency in kHz; 0 until it has run for
/// `hal::cpufreq::MIN_ACTIVE_US` in total.
static KHZ: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// Decide once, on the BSP, after `tsc::init` (the TSC frequency is the
/// MPERF rate).
pub fn init() {
    let leaf0 = unsafe { core::arch::x86_64::__cpuid(0) };
    let ecx = if leaf0.eax >= 6 { unsafe { core::arch::x86_64::__cpuid(6) }.ecx } else { 0 };
    let yes = cpufreq::has_aperfmperf(ecx) && super::tsc::freq_hz() != 0;
    SUPPORTED.store(if yes { YES } else { NO }, Ordering::Relaxed);
    crate::serial_println!("[cpufreq] APERF/MPERF {}", if yes { "present: per-core MHz measured" } else { "absent: cpu MHz is the TSC's" });
}

/// Whether frequencies are measured at all.
pub fn supported() -> bool {
    SUPPORTED.load(Ordering::Relaxed) == YES
}

/// Sample this CPU's counters. Timer ISR, IF=0, every scheduling CPU.
pub fn tick() {
    if !supported() {
        return;
    }
    let cpu = super::cpu_id();
    if cpu >= MAX_CPUS {
        return;
    }
    // Read MPERF first: APERF read an instant later can only be ahead, by
    // a few cycles, never behind.
    let mperf = unsafe { rdmsr(IA32_MPERF) };
    let aperf = unsafe { rdmsr(IA32_APERF) };
    let base_khz = super::tsc::freq_hz() / 1000;
    // `try_with`: only this CPU ever takes it, but an ISR never waits.
    WINDOWS[cpu].try_with(|w| {
        let (next, khz) = cpufreq::sample(*w, aperf, mperf, base_khz);
        *w = next;
        if let Some(k) = khz {
            KHZ[cpu].store(k, Ordering::Relaxed);
        }
    });
}

/// `cpu`'s measured frequency in kHz, if there is one yet.
pub fn khz(cpu: usize) -> Option<u64> {
    match KHZ.get(cpu)?.load(Ordering::Relaxed) {
        0 => None,
        k => Some(k),
    }
}

#[inline]
pub(super) unsafe fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nostack, nomem));
    }
    ((hi as u64) << 32) | lo as u64
}
