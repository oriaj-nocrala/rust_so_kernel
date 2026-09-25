// kernel/src/memory/tlb.rs
//
// The one place a stale translation is dropped after a page-table change —
// on this CPU and, since stage 5 of `docs/smp/smp-plan.md`, on every other
// CPU that can hold it (TLB shootdown).
//
// **Rule:** after changing a PTE, invalidate through this module. Never
// `x86_64::instructions::tlb::*` or `MapperFlush::flush()` directly —
// consume a `MapperFlush` with `.ignore()` and call `invalidate_page`
// (`MapperFlush` does not expose its page, so the caller passes it). Every
// CR3 load goes through `switch_to` too: which CPU has which table loaded
// is what decides who gets told.
//
// Nothing fails visibly without this: a CPU that keeps an old entry reads
// and writes a frame that now belongs to someone else.
//
// ── Who is told (`hal::tlb`, host-tested) ───────────────────────────────
//
//   * `invalidate_page(pml4, addr)` — a user mapping in the table at
//     `pml4`. Without PCIDs a CR3 write drops every non-global entry, so
//     only the CPUs that have `pml4` loaded *now* (`LOADED`) can hold it.
//   * `invalidate_kernel_page(addr)` — a kernel mapping: shared by every
//     address space and possibly GLOBAL, so every CPU that is up.
//   * `invalidate_all_this_cpu` — not a PTE change at all; a step of a
//     per-CPU register procedure (the PAT sequence) each CPU runs itself.
//
// "Up" is `READY`: a CPU joins it (`this_cpu_ready`) only once it can take
// the IPI, and then flushes its whole TLB, globals included — whatever it
// cached before joining can't have been missed.
//
// ── The protocol ────────────────────────────────────────────────────────
//
// One request at a time, in a global slot owned by whoever holds `SENDER`:
// the sender writes the request, sets the targets' bits in `PENDING`, sends
// each an IPI on `SHOOTDOWN_VECTOR`, invalidates locally and spins until
// every bit is clear. A target's handler (`service_pending`) invalidates
// and clears its own bit. Everything runs with IF=0 on the sender.
//
// A CPU waiting with IF=0 can't take the IPI, so every IF=0 wait that
// could be on the other end of a shootdown calls `service_pending` itself:
// the spin for `SENDER` (two CPUs shooting each other down), every
// `IrqMutex` spin (through `diag::IrqControl::relax` — the sender may hold
// `BUDDY`, see `unmap_page_and_free_2m_with_buddy`), every kernel
// `crate::sync::Mutex` spin (its relax strategy — the scheduler's lock
// among them, since stage 6), and the USB transfer waits, which poll the
// controller with IF=0 for up to seconds. **A new IF=0 busy-wait must do
// the same**; `spin::Mutex` itself must not be used in the kernel.
//
// The ordering that makes "who has it loaded" safe to read without a lock:
// a CPU publishes `LOADED[cpu]` *before* writing CR3, and the sender reads
// it only after a full fence that follows its PTE store. If the sender
// misses a CPU's new value, that CPU's CR3 write — which flushes every
// non-global entry — comes after the PTE store, so it sees the new PTE.
//
// No range variants yet: every caller changes one page at a time. With a
// shootdown, one IPI per range instead of one per page is exactly what a
// range variant would be for — `/proc/kdebug`'s `tlb:` line says whether
// it is needed.

use core::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, Ordering};

use hal::tlb::{self as htlb, Scope};
use x86_64::instructions::tlb;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::PhysFrame;
use x86_64::{PhysAddr, VirtAddr};

use crate::cpu::MAX_CPUS;

/// The IPI a target is sent. Above every device vector (ISA lines are
/// 32..47) and below the LAPIC's spurious 0xFF; its priority class (15) is
/// the highest, so a CPU busy in a device handler still answers it.
pub const SHOOTDOWN_VECTOR: u8 = 0xF0;

/// How long a sender waits for its targets before declaring the machine
/// broken. A target only needs to leave an IF=0 section; the longest known
/// is a USB transfer (~1 ms per 64 KiB).
const ACK_TIMEOUT_NS: u64 = 1_000_000_000;

/// The page table each CPU has loaded (physical address of its PML4).
static LOADED: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// CPUs that take shootdowns.
static READY: AtomicU32 = AtomicU32::new(0);

static SENDER: AtomicBool = AtomicBool::new(false);
/// The request, valid while `SENDER` is held and `PENDING` is nonzero.
static REQ_PML4: AtomicU64 = AtomicU64::new(0);
static REQ_ADDR: AtomicU64 = AtomicU64::new(0);
/// `REQ_PML4` for a kernel mapping (no page table lives at the top of the
/// physical address space).
const REQ_KERNEL: u64 = u64::MAX;
/// Targets that have not acknowledged yet.
static PENDING: AtomicU32 = AtomicU32::new(0);

// ── Statistics, for `/proc/kdebug` ──────────────────────────────────────
static SHOOTDOWNS: AtomicU64 = AtomicU64::new(0);
static IPIS: AtomicU64 = AtomicU64::new(0);
static SERVICED: AtomicU64 = AtomicU64::new(0);
static WAIT_CYCLES: AtomicU64 = AtomicU64::new(0);
static MAX_WAIT_CYCLES: AtomicU64 = AtomicU64::new(0);

/// A user mapping at `addr` in the page table whose PML4 is at `pml4`
/// changed (unmapped, remapped, permissions changed). A 2 MiB/1 GiB page is
/// dropped whole by any address inside it. `pml4` need not be loaded
/// anywhere (a fork child being built): then nobody is told.
#[inline]
pub fn invalidate_page(pml4: PhysAddr, addr: VirtAddr) {
    invalidate(Scope::AddressSpace(pml4.as_u64()), addr);
}

/// A kernel mapping at `addr` changed. `invlpg` drops the entry even when
/// it is GLOBAL.
#[inline]
pub fn invalidate_kernel_page(addr: VirtAddr) {
    invalidate(Scope::Kernel, addr);
}

/// Reload CR3 on this CPU only, for a per-CPU procedure that requires it
/// (the PAT change sequence in `memory::memtype::program_pat`). Each CPU
/// runs such a procedure itself, so there is nothing to shoot down — and
/// that is the only legitimate use.
#[inline]
pub fn invalidate_all_this_cpu() {
    tlb::flush_all();
}

/// Load `pml4` into CR3 on this CPU, publishing it first (see the module
/// comment for why first). IF=0 across the pair: an interrupt between them
/// could itself switch tables.
///
/// # Safety
/// As `Cr3::write`: `pml4` must map the running code, stack and data.
pub unsafe fn switch_to(pml4: PhysFrame) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        LOADED[crate::cpu::cpu_id()].store(pml4.start_address().as_u64(), Ordering::SeqCst);
        Cr3::write(pml4, x86_64::registers::control::Cr3Flags::empty());
    });
}

/// This CPU (`cpu`, IDT and LAPIC already live) starts taking shootdowns.
/// Called once per CPU with IF=0: by the BSP before it starts the APs, by
/// each AP once `init_this_cpu` has succeeded. A CPU that never gets here
/// is never waited for.
pub fn this_cpu_ready(cpu: usize) {
    let (frame, _) = Cr3::read();
    LOADED[cpu].store(frame.start_address().as_u64(), Ordering::SeqCst);
    READY.fetch_or(1 << cpu, Ordering::SeqCst);
    // Everything cached before the line above could have missed a
    // shootdown, so drop all of it, GLOBAL entries included: toggling
    // CR4.PGE is the architectural way (a CR3 reload keeps globals).
    // SAFETY: PGE off and back on changes no mapping.
    unsafe {
        use x86_64::registers::control::{Cr4, Cr4Flags};
        let cr4 = Cr4::read();
        if cr4.contains(Cr4Flags::PAGE_GLOBAL) {
            Cr4::write(cr4 - Cr4Flags::PAGE_GLOBAL);
            Cr4::write(cr4);
        } else {
            tlb::flush_all();
        }
    }
}

fn invalidate(scope: Scope, addr: VirtAddr) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let (cr3, _) = Cr3::read();
        if htlb::holds(scope, cr3.start_address().as_u64()) {
            tlb::flush(addr);
        }
        // The caller's PTE store before anyone's `LOADED`/`READY`.
        fence(Ordering::SeqCst);
        let ready = READY.load(Ordering::SeqCst);
        let me = crate::cpu::cpu_id();
        if ready & !(1 << me) == 0 {
            return; // nobody else is up (the whole boot before the APs, or -smp 1)
        }
        let loaded: [u64; MAX_CPUS] = core::array::from_fn(|c| LOADED[c].load(Ordering::SeqCst));
        let targets = htlb::targets(scope, &loaded, ready, me);
        if targets != 0 {
            shoot(scope, addr, targets);
        }
    });
}

/// Sends the request to `targets` and waits for all of them. IF=0.
fn shoot(scope: Scope, addr: VirtAddr, targets: u32) {
    while SENDER
        .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        // Whoever holds it may be waiting for us.
        service_pending();
        core::hint::spin_loop();
    }

    REQ_PML4.store(
        match scope {
            Scope::Kernel => REQ_KERNEL,
            Scope::AddressSpace(p) => p,
        },
        Ordering::Relaxed,
    );
    REQ_ADDR.store(addr.as_u64(), Ordering::Relaxed);
    PENDING.store(targets, Ordering::Release);

    // An x2APIC ICR write is not serializing: without this the IPI can
    // overtake the stores above (Linux's `weak_wrmsr_fence`).
    // SAFETY: fences only.
    unsafe { core::arch::asm!("mfence; lfence", options(nostack, preserves_flags)) };
    let low = hal::smp::icr::fixed(SHOOTDOWN_VECTOR);
    for cpu in htlb::cpus(targets) {
        if !crate::interrupts::apic::send_ipi(crate::smp::apic_id(cpu), low) {
            panic!("TLB shootdown: IPI to cpu{} stuck in the ICR", cpu);
        }
    }

    let start = crate::cpu::tsc::read();
    let limit = crate::cpu::tsc::freq_hz().saturating_mul(ACK_TIMEOUT_NS) / 1_000_000_000;
    let mut spins: u64 = 0;
    loop {
        let left = PENDING.load(Ordering::Acquire) & targets;
        if left == 0 {
            break;
        }
        spins += 1;
        let waited = crate::cpu::tsc::read().wrapping_sub(start);
        // Spin bound too, in case the TSC was never calibrated.
        if (limit != 0 && waited > limit) || spins > 4_000_000_000 {
            panic!(
                "TLB shootdown of {:#x} ({}): cpus {:#x} never acknowledged",
                addr.as_u64(),
                if matches!(scope, Scope::Kernel) { "kernel" } else { "user" },
                left
            );
        }
        core::hint::spin_loop();
    }
    let waited = crate::cpu::tsc::read().wrapping_sub(start);

    SHOOTDOWNS.fetch_add(1, Ordering::Relaxed);
    IPIS.fetch_add(targets.count_ones() as u64, Ordering::Relaxed);
    WAIT_CYCLES.fetch_add(waited, Ordering::Relaxed);
    MAX_WAIT_CYCLES.fetch_max(waited, Ordering::Relaxed);

    SENDER.store(false, Ordering::Release);
}

/// Answers this CPU's pending shootdown, if it has one. Lock-free and safe
/// anywhere with IF=0: the IPI handler, and every spin that may be on the
/// other end of a sender (see the module comment). Returns whether there
/// was one.
pub fn service_pending() -> bool {
    let bit = 1u32 << crate::cpu::cpu_id();
    if PENDING.load(Ordering::Acquire) & bit == 0 {
        return false;
    }
    let pml4 = REQ_PML4.load(Ordering::Relaxed);
    let scope = if pml4 == REQ_KERNEL { Scope::Kernel } else { Scope::AddressSpace(pml4) };
    let (cr3, _) = Cr3::read();
    // A CPU that has since switched tables dropped the entry with the CR3
    // write; `invlpg` would be harmless anyway.
    if htlb::holds(scope, cr3.start_address().as_u64()) {
        tlb::flush(VirtAddr::new(REQ_ADDR.load(Ordering::Relaxed)));
    }
    SERVICED.fetch_add(1, Ordering::Relaxed);
    PENDING.fetch_and(!bit, Ordering::Release);
    true
}

/// `/proc/kdebug` line.
pub fn render() -> alloc::string::String {
    let n = SHOOTDOWNS.load(Ordering::Relaxed);
    let hz = crate::cpu::tsc::freq_hz().max(1);
    let us = |cycles: u64| cycles.saturating_mul(1_000_000) / hz;
    alloc::format!(
        "tlb: ready {:#x}, {} shootdowns ({} IPIs), {} serviced here or elsewhere, wait avg {} us max {} us",
        READY.load(Ordering::Relaxed),
        n,
        IPIS.load(Ordering::Relaxed),
        SERVICED.load(Ordering::Relaxed),
        us(WAIT_CYCLES.load(Ordering::Relaxed) / n.max(1)),
        us(MAX_WAIT_CYCLES.load(Ordering::Relaxed)),
    )
}
