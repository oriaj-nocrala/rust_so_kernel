// kernel/src/cpu/idle.rs
//
// How an idle CPU waits, and the two instruments that say how well it
// does: package energy (RAPL) and each CPU's C0 residency (MPERF / TSC).
//
// `hlt` is C1 on Zen: the core's clock stops but it stays powered. Linux
// on the same machine spends nearly all its idle time in ACPI C2 — a read
// of an I/O port the core traps, which power-gates it (CC6). The port and
// why `base + 1` are `hal::amd_power`. `MODE` picks between the two at
// run time (`kdebug idle hlt|c2`), so one boot can measure both — and on
// the Ryzen they measured the same (see `init`).

use core::sync::atomic::{AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};

use hal::amd_power;

use super::MAX_CPUS;

const HLT: u8 = 0;
const C2: u8 = 1;

static MODE: AtomicU8 = AtomicU8::new(HLT);
/// The C2 port; 0 when there is none (every non-Zen machine, QEMU).
static C2_PORT: AtomicU16 = AtomicU16::new(0);
static CSTATE_BASE: AtomicU64 = AtomicU64::new(0);
static HLT_ENTRIES: AtomicU64 = AtomicU64::new(0);
static C2_ENTRIES: AtomicU64 = AtomicU64::new(0);

/// RAPL: whether the counters exist, the unit, and the package energy
/// accumulated since boot (CPU 0's tick; one wrap per ~11 min at 100 W).
static RAPL: AtomicU8 = AtomicU8::new(0);
static RAPL_ESU: AtomicU32 = AtomicU32::new(0);
static PKG_LAST: AtomicU32 = AtomicU32::new(0);
static PKG_UJ: AtomicU64 = AtomicU64::new(0);

/// C0 residency: each CPU's window start (TSC, MPERF) and its last result,
/// in permille over about one second. Written only by that CPU's tick.
static WIN_TSC: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static WIN_MPERF: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static C0_PERMILLE: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(u32::MAX) }; MAX_CPUS];

/// Decide once, on the BSP, after `freq::init`. The mode stays `hlt`: on
/// the target machine C2 measured no different (boot #56: package 17.5 and
/// 21.4 W in C2, 18.0 and 19.8 W in `hlt`, the same 31 °C Tctl), so the
/// proven path stays the default and C2 is `kdebug idle c2`.
pub fn init() {
    use core::arch::x86_64::__cpuid;
    let l0 = __cpuid(0);
    let mut vendor = [0u8; 12];
    vendor[0..4].copy_from_slice(&l0.ebx.to_le_bytes());
    vendor[4..8].copy_from_slice(&l0.edx.to_le_bytes());
    vendor[8..12].copy_from_slice(&l0.ecx.to_le_bytes());
    let family = hal::cpuid::signature(__cpuid(1).eax).family;
    if !amd_power::has_zen_msrs(&vendor, family) || under_hypervisor() {
        crate::serial_println!("[idle] hlt (no Zen C-state/RAPL MSRs)");
        return;
    }
    let base = unsafe { super::freq::rdmsr(amd_power::MSR_CSTATE_BASE) };
    CSTATE_BASE.store(base, Ordering::Relaxed);
    if let Some(port) = amd_power::c2_port(base) {
        C2_PORT.store(port, Ordering::Relaxed);
    }
    let esu = amd_power::energy_unit_shift(unsafe { super::freq::rdmsr(amd_power::MSR_RAPL_UNIT) });
    RAPL_ESU.store(esu, Ordering::Relaxed);
    PKG_LAST.store(unsafe { super::freq::rdmsr(amd_power::MSR_PKG_ENERGY) } as u32, Ordering::Relaxed);
    RAPL.store(1, Ordering::Relaxed);
    crate::serial_println!(
        "[idle] {} (CStateBaseAddr {:#x}, C2 port {:#x}); RAPL esu={}",
        if MODE.load(Ordering::Relaxed) == C2 { "c2" } else { "hlt" },
        base & 0xFFFF, C2_PORT.load(Ordering::Relaxed), esu
    );
}

/// A hypervisor may advertise a Zen CPU and #GP on its model-specific
/// MSRs (QEMU with `-cpu host` does); CPUID 1 ECX bit 31.
fn under_hypervisor() -> bool {
    core::arch::x86_64::__cpuid(1).ecx & (1 << 31) != 0
}

/// Wait for the next interrupt. Called with IF=0 after the idle loop has
/// checked for work; returns with IF=1 (a wakeup that arrived after the
/// check is pending, and either instruction returns at once for it).
pub fn wait() {
    if MODE.load(Ordering::Relaxed) == C2 {
        let port = C2_PORT.load(Ordering::Relaxed);
        C2_ENTRIES.fetch_add(1, Ordering::Relaxed);
        // As Linux's `acpi_idle` enters C2: IF=0, the read, then IF=1 so
        // the interrupt that ended it is taken. No dummy PM-timer read —
        // Linux skips it on AMD.
        unsafe {
            x86_64::instructions::port::Port::<u8>::new(port).read();
            x86_64::instructions::interrupts::enable();
        }
    } else {
        HLT_ENTRIES.fetch_add(1, Ordering::Relaxed);
        // `sti; hlt` as one sequence: a wakeup sent after the caller's
        // check is still pending at the `hlt` and wakes it.
        unsafe { core::arch::asm!("sti; hlt", options(nomem, nostack)) };
    }
}

/// `kdebug idle hlt|c2`: false if C2 was asked for and there is no port.
pub fn set_c2(on: bool) -> bool {
    if on && C2_PORT.load(Ordering::Relaxed) == 0 {
        return false;
    }
    MODE.store(if on { C2 } else { HLT }, Ordering::Relaxed);
    crate::serial_println!("[idle] mode -> {}", if on { "c2" } else { "hlt" });
    true
}

/// Timer ISR, every scheduling CPU: this CPU's C0 window, and on CPU 0 the
/// package energy.
pub fn tick() {
    let cpu = super::cpu_id();
    if cpu == 0 && RAPL.load(Ordering::Relaxed) != 0 {
        let now = unsafe { super::freq::rdmsr(amd_power::MSR_PKG_ENERGY) } as u32;
        let prev = PKG_LAST.swap(now, Ordering::Relaxed);
        PKG_UJ.fetch_add(amd_power::energy_uj(prev, now, RAPL_ESU.load(Ordering::Relaxed)), Ordering::Relaxed);
    }
    if !super::freq::supported() || cpu >= MAX_CPUS {
        return;
    }
    let tsc = unsafe { core::arch::x86_64::_rdtsc() };
    let mperf = unsafe { super::freq::rdmsr(super::freq::IA32_MPERF) };
    let start = WIN_TSC[cpu].load(Ordering::Relaxed);
    if start == 0 {
        WIN_TSC[cpu].store(tsc, Ordering::Relaxed);
        WIN_MPERF[cpu].store(mperf, Ordering::Relaxed);
        return;
    }
    let dt = tsc.wrapping_sub(start);
    if dt >= super::tsc::freq_hz() {
        let dm = mperf.wrapping_sub(WIN_MPERF[cpu].load(Ordering::Relaxed));
        C0_PERMILLE[cpu].store(amd_power::c0_permille(dm, dt), Ordering::Relaxed);
        WIN_TSC[cpu].store(tsc, Ordering::Relaxed);
        WIN_MPERF[cpu].store(mperf, Ordering::Relaxed);
    }
}

/// The `/proc/kdebug` lines.
pub fn render() -> alloc::string::String {
    use core::fmt::Write;
    let mut out = alloc::string::String::new();
    let _ = write!(
        out,
        "idle: mode={} cstate_base={:#x} c2_port={:#x} hlt_entries={} c2_entries={}\nrapl: {}\nc0_permille:",
        if MODE.load(Ordering::Relaxed) == C2 { "c2" } else { "hlt" },
        CSTATE_BASE.load(Ordering::Relaxed) & 0xFFFF,
        C2_PORT.load(Ordering::Relaxed),
        HLT_ENTRIES.load(Ordering::Relaxed),
        C2_ENTRIES.load(Ordering::Relaxed),
        if RAPL.load(Ordering::Relaxed) != 0 {
            alloc::format!("package_uj={} esu={}", PKG_UJ.load(Ordering::Relaxed), RAPL_ESU.load(Ordering::Relaxed))
        } else {
            alloc::string::String::from("absent")
        },
    );
    for c in crate::process::scheduler::scheduling_cpus() {
        match C0_PERMILLE.get(c).map(|p| p.load(Ordering::Relaxed)) {
            Some(u32::MAX) | None => { let _ = write!(out, " cpu{c}=-"); }
            Some(p) => { let _ = write!(out, " cpu{c}={p}"); }
        }
    }
    out
}
