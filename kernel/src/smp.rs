// kernel/src/smp.rs
//
// Starting the application processors — stage 4 of docs/smp/smp-plan.md.
//
// Each AP the MADT lists is woken with INIT-SIPI-SIPI, climbs from real mode
// to long mode through a trampoline in low memory, runs the same
// `cpu::init_this_cpu` the BSP ran, and then sits in `sti; hlt`, inert, until
// the BSP starts the first process: `release_aps` then sends every AP into
// the scheduler (stage 7), on its own idle process, with its LAPIC timer
// unmasked (`interrupts::apic::start_timer_on_ap`). Built with
// `CONSTANOS_NOSMP=1` they stay inert for good — the single-CPU comparison
// decision 6 of the plan asks for. The I/O APIC routes nothing to an AP.
// Two IPIs besides the scheduler's reach it: a TLB shootdown
// (`memory::tlb`), and `WAKE_VECTOR`, after which its idle loop runs whatever
// `run_on` left in its mailbox — the TLB self-test's way onto an AP.
//
// The pure half — where the trampoline goes, ICR encodings, the sequence
// and its delays, CPU numbering — is `hal::smp`, host-tested.
//
// ── The trampoline ──────────────────────────────────────────────────────
//
// Four pages below 640 KiB, carved out of the memory map before the Buddy
// allocator sees it (`init::memory::init_core`):
//
//   base + 0x0000  the code below, copied in, with its data fields patched
//   base + 0x1000  PML4: a copy of the kernel's, plus entry 0 →
//   base + 0x2000  PDPT, entry 0 →
//   base + 0x3000  PD, entry 0 = 2 MiB page at physical 0 (identity)
//
// The identity mapping exists only for the instructions between "paging on"
// and the jump to the kernel's higher half. The first thing the Rust entry
// does is load the kernel's own CR3, so no live page table ever changes.
// The PML4 must be below 4 GiB (loaded from 32-bit protected mode), which
// its place below 1 MiB guarantees.
//
// APs are started one at a time and the BSP waits for each to finish
// `init_this_cpu` before the next, so one trampoline and one set of data
// fields serve all of them. An AP that does not answer in time is sent an
// INIT again, which parks it in wait-for-SIPI: otherwise it could wake up
// late, read the next AP's fields and run on its stack.
//
// Every wait is bounded by the TSC and by a spin count, like every boot-time
// wait here: an AP that never answers is logged and left out, never a hang.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use hal::smp::{self as hsmp, Step};

use crate::cpu::MAX_CPUS;
use crate::serial_println;

core::arch::global_asm!(
    r#"
    .pushsection .rodata.ap_trampoline, "a"
    .balign 16
    .global ap_trampoline_start
ap_trampoline_start:
    .code16
    jmp ap_tramp_real

    /* ── data, patched by the BSP before each SIPI ── */
    .balign 8
    .global ap_tramp_gdt
ap_tramp_gdt:
    .quad 0
    .quad 0x00CF9A000000FFFF        /* 0x08: 32-bit code, flat */
    .quad 0x00CF92000000FFFF        /* 0x10: data, flat */
    .quad 0x00AF9A000000FFFF        /* 0x18: 64-bit code */
ap_tramp_gdt_end:
    .global ap_tramp_gdtr
ap_tramp_gdtr:
    .word ap_tramp_gdt_end - ap_tramp_gdt - 1
    .long 0                         /* physical address of ap_tramp_gdt */
    .balign 8
    .global ap_tramp_pm_far
ap_tramp_pm_far:
    .long 0                         /* physical address of ap_tramp_pm */
    .word 0x08
    .balign 8
    .global ap_tramp_lm_far
ap_tramp_lm_far:
    .long 0                         /* physical address of ap_tramp_lm */
    .word 0x18
    .balign 8
    .global ap_tramp_cr3
ap_tramp_cr3:   .quad 0             /* the trampoline's own PML4 */
    .global ap_tramp_stack
ap_tramp_stack: .quad 0             /* top of this AP's stack (virtual) */
    .global ap_tramp_entry
ap_tramp_entry: .quad 0             /* smp::ap_entry (virtual) */
    .global ap_tramp_cpu
ap_tramp_cpu:   .quad 0             /* kernel CPU index */
    .global ap_tramp_stage
ap_tramp_stage: .quad 0             /* progress: 1 = protected, 2 = long mode */

    .set T_GDTR,  ap_tramp_gdtr  - ap_trampoline_start
    .set T_PMFAR, ap_tramp_pm_far - ap_trampoline_start
    .set T_LMFAR, ap_tramp_lm_far - ap_trampoline_start
    .set T_CR3,   ap_tramp_cr3   - ap_trampoline_start
    .set T_STACK, ap_tramp_stack - ap_trampoline_start
    .set T_ENTRY, ap_tramp_entry - ap_trampoline_start
    .set T_CPU,   ap_tramp_cpu   - ap_trampoline_start
    .set T_STAGE, ap_tramp_stage - ap_trampoline_start

    /* ── real mode: CS = base >> 4, IP = 0 ── */
ap_tramp_real:
    cli
    cld
    movw %cs, %ax
    movw %ax, %ds
    xorl %ebx, %ebx
    movw %ax, %bx
    shll $4, %ebx                   /* ebx = physical base, kept to the end */
    lgdtl T_GDTR
    movl %cr0, %eax
    orl $1, %eax                    /* PE */
    movl %eax, %cr0
    ljmpl *T_PMFAR

    /* ── 32-bit protected mode, flat ── */
    .code32
    .global ap_tramp_pm
ap_tramp_pm:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movw %ax, %fs
    movw %ax, %gs
    movl $1, T_STAGE(%ebx)
    movl %cr4, %eax
    orl $(1 << 5), %eax             /* PAE */
    movl %eax, %cr4
    movl T_CR3(%ebx), %eax
    movl %eax, %cr3
    movl $0xC0000080, %ecx          /* EFER */
    rdmsr
    orl $((1 << 8) | (1 << 11)), %eax   /* LME, and NXE: kernel PTEs set bit 63 */
    wrmsr
    movl %cr0, %eax
    orl $0x80000001, %eax           /* PG | PE */
    movl %eax, %cr0
    ljmpl *T_LMFAR(%ebx)

    /* ── 64-bit, still on the identity-mapped page ── */
    .code64
    .global ap_tramp_lm
ap_tramp_lm:
    movl %ebx, %ebx                 /* upper halves are undefined after the switch */
    movq $2, T_STAGE(%rbx)
    movq T_STACK(%rbx), %rsp
    movq T_CPU(%rbx), %rdi
    movq T_ENTRY(%rbx), %rax
    xorl %ebp, %ebp
    pushq $0                        /* fake return address: ABI stack alignment */
    jmpq *%rax

    .global ap_trampoline_end
ap_trampoline_end:
    .popsection
    "#,
    options(att_syntax)
);

extern "C" {
    static ap_trampoline_start: u8;
    static ap_trampoline_end: u8;
    static ap_tramp_gdt: u8;
    static ap_tramp_gdtr: u8;
    static ap_tramp_pm_far: u8;
    static ap_tramp_lm_far: u8;
    static ap_tramp_pm: u8;
    static ap_tramp_lm: u8;
    static ap_tramp_cr3: u8;
    static ap_tramp_stack: u8;
    static ap_tramp_entry: u8;
    static ap_tramp_cpu: u8;
    static ap_tramp_stage: u8;
}

/// Offset of a trampoline label from its start.
fn off(label: &u8) -> u64 {
    // SAFETY: only the addresses are taken.
    let start = unsafe { &ap_trampoline_start } as *const u8 as u64;
    label as *const u8 as u64 - start
}

/// Physical base of the reserved trampoline window.
static TRAMPOLINE: spin::Once<u64> = spin::Once::new();

/// Called by `init::memory::init_core`, which kept these pages away from the
/// Buddy allocator.
pub fn set_trampoline(base: u64) {
    TRAMPOLINE.call_once(|| base);
}

/// The kernel's CR3, loaded by each AP first thing in Rust.
static KERNEL_CR3: AtomicU64 = AtomicU64::new(0);

/// Wakes an AP from `hlt` to look at its mailbox (`run_on`). Next to the
/// shootdown's 0xF0, in the same (highest) priority class.
pub const WAKE_VECTOR: u8 = 0xF1;

/// Per-CPU mailbox: a `fn(usize)` as `usize`, 0 when empty. The AP clears
/// it after the call returns.
static MAILBOX: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];

/// Set by `release_aps`: the APs leave their boot loop for the scheduler.
static GO: AtomicBool = AtomicBool::new(false);

/// Built with `CONSTANOS_NOSMP=1` (any value but `0`): the APs still start
/// — the TLB self-test needs them — but never run processes.
pub fn nosmp() -> bool {
    matches!(option_env!("CONSTANOS_NOSMP"), Some(v) if v != "0")
}

/// Sends every online AP into the scheduler. BSP, once, as the first
/// process starts (`process::start_first_process`), with every AP's idle
/// process already created.
pub fn release_aps() {
    if nosmp() {
        return;
    }
    GO.store(true, Ordering::Release);
    for cpu in (1..MAX_CPUS).filter(|&c| is_online_ap(c)) {
        x86_64::instructions::interrupts::without_interrupts(|| {
            crate::interrupts::apic::send_ipi(apic_id(cpu), hal::smp::icr::fixed(WAKE_VECTOR))
        });
    }
}

/// CPUs that will run processes: the BSP, and every online AP unless
/// `nosmp()`.
pub fn scheduling_cpus() -> impl Iterator<Item = usize> {
    (0..MAX_CPUS).filter(|&c| c == 0 || (!nosmp() && is_online_ap(c)))
}

/// One pass of an idle process's loop: run what `run_on` left in this CPU's
/// mailbox, else `hlt` until the next interrupt. Entered and left with IF=1.
pub fn idle_once() {
    let cpu = crate::cpu::cpu_id();
    x86_64::instructions::interrupts::disable();
    let work = MAILBOX[cpu].load(Ordering::Acquire);
    if work != 0 {
        // SAFETY: only `run_on` stores here, and only `fn(usize)`s.
        let f: fn(usize) = unsafe { core::mem::transmute(work) };
        x86_64::instructions::interrupts::enable();
        f(cpu);
        MAILBOX[cpu].store(0, Ordering::Release);
        return;
    }
    // SAFETY: `sti; hlt` as one sequence: a `WAKE_VECTOR` sent after the
    // check above is still pending at the `hlt` and wakes it.
    unsafe { core::arch::asm!("sti; hlt", options(nomem, nostack)) };
}

/// Each AP's stack; they are never freed (an inert AP lives on it forever).
const AP_STACK_SIZE: usize = 64 * 1024;

// ── Per-CPU outcome, for /proc/kdebug ───────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ApState {
    Absent = 0,
    Bsp,
    Starting,
    Online,
    /// Reached `init_this_cpu`, which failed a step.
    InitFailed,
    /// Never answered; the stage it reached is in `STAGE_REACHED`.
    NoResponse,
    /// An IPI stayed pending in the ICR.
    IpiStuck,
}

impl ApState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Bsp,
            2 => Self::Starting,
            3 => Self::Online,
            4 => Self::InitFailed,
            5 => Self::NoResponse,
            6 => Self::IpiStuck,
            _ => Self::Absent,
        }
    }
}

static STATE: [AtomicU8; MAX_CPUS] = [const { AtomicU8::new(0) }; MAX_CPUS];
static APIC_ID: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];
/// Microseconds from the first IPI to the AP reporting in.
static UP_US: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];
static STAGE_REACHED: [AtomicU8; MAX_CPUS] = [const { AtomicU8::new(0) }; MAX_CPUS];
/// The first failing step of an AP's `init_this_cpu`.
static INIT_ERROR: [spin::Once<(&'static str, &'static str)>; MAX_CPUS] =
    [const { spin::Once::new() }; MAX_CPUS];
static MADT_CPUS: AtomicU32 = AtomicU32::new(0);
static DROPPED: AtomicU32 = AtomicU32::new(0);
static NOT_TRIED: spin::Once<&'static str> = spin::Once::new();

/// How long an AP gets, after its last SIPI, to finish `init_this_cpu`.
const AP_TIMEOUT_US: u64 = 200_000;

/// Starts every AP in the MADT and leaves it inert. BSP only, IF=0, after
/// `cpu::init_this_cpu(0)` (the APs copy its decisions) with the APIC live
/// (IPIs go through it). Never fails the boot: whatever does not come up is
/// recorded for `/proc/kdebug` and left out.
pub fn start_aps() {
    STATE[0].store(ApState::Bsp as u8, Ordering::Relaxed);
    let reason = match try_start_aps() {
        Ok(()) => None,
        Err(r) => Some(r),
    };
    if let Some(r) = reason {
        NOT_TRIED.call_once(|| r);
    }
    serial_println!("{}", render());
}

fn try_start_aps() -> Result<(), &'static str> {
    let topo = crate::acpi::topology().ok_or("no MADT")?;
    MADT_CPUS.store(topo.cpus.len() as u32, Ordering::Relaxed);
    if !crate::interrupts::apic::active() {
        return Err("no local APIC in use");
    }
    let base = *TRAMPOLINE.get().ok_or("no trampoline window below 640 KiB")?;
    let vector = hsmp::sipi_vector(base).ok_or("trampoline window not SIPI-addressable")?;

    let bsp_id = crate::interrupts::apic::this_lapic_id();
    APIC_ID[0].store(bsp_id, Ordering::Relaxed);
    let madt: alloc::vec::Vec<u32> = topo.cpus.iter().map(|c| c.apic_id as u32).collect();
    let (order, dropped) = hsmp::cpu_order(&madt, bsp_id, MAX_CPUS);
    DROPPED.store(dropped as u32, Ordering::Relaxed);
    if order.len() < 2 {
        return Ok(());
    }

    // From here on the BSP may be sent shootdowns by the APs, and waits for
    // theirs.
    crate::memory::tlb::this_cpu_ready(0);
    prepare_trampoline(base)?;

    for (cpu, &apic_id) in order.iter().enumerate().skip(1) {
        APIC_ID[cpu].store(apic_id, Ordering::Relaxed);
        start_one(cpu, apic_id, vector);
    }
    if online() > 1 {
        crate::tlb_selftest::prepare();
    }
    Ok(())
}

/// Copies the code in, patches the absolute addresses, and builds the
/// trampoline's page tables. Nothing here is live: the pages are reserved
/// and no CPU runs on them yet.
fn prepare_trampoline(base: u64) -> Result<(), &'static str> {
    let phys_off = crate::memory::physical_memory_offset().as_u64();
    let virt = |pa: u64| (phys_off + pa) as *mut u8;
    // SAFETY: the symbols bound the assembled blob; the destination is the
    // reserved window, mapped by the physical-memory window.
    unsafe {
        let start = &ap_trampoline_start as *const u8;
        let len = &ap_trampoline_end as *const u8 as usize - start as usize;
        if len > hsmp::PAGE as usize {
            return Err("trampoline code larger than a page");
        }
        core::ptr::write_bytes(virt(base), 0, (hsmp::TRAMPOLINE_PAGES * hsmp::PAGE) as usize);
        core::ptr::copy_nonoverlapping(start, virt(base), len);

        let w32 = |label: &u8, v: u64| {
            core::ptr::write_unaligned(virt(base + off(label)) as *mut u32, v as u32)
        };
        // The GDTR's base follows its 2-byte limit.
        core::ptr::write_unaligned(
            virt(base + off(&ap_tramp_gdtr) + 2) as *mut u32,
            (base + off(&ap_tramp_gdt)) as u32,
        );
        w32(&ap_tramp_pm_far, base + off(&ap_tramp_pm));
        w32(&ap_tramp_lm_far, base + off(&ap_tramp_lm));
        write_field(&ap_tramp_cr3, base + hsmp::PAGE);
        write_field(&ap_tramp_entry, ap_entry as *const () as u64);
    }

    // Page tables. PML4 = a copy of the kernel's with entry 0 replaced by
    // the identity mapping. The kernel's own entry 0 is not empty — it holds
    // the bootloader's identity mapping of its context-switch page, used once
    // to jump into the kernel and never since (user address spaces skip it:
    // `OwnedPageTable::new_user`). Overwriting it in the copy costs nothing:
    // the AP loads the kernel's real CR3 before touching anything but the
    // higher half.
    let (cr3_frame, _) = x86_64::registers::control::Cr3::read();
    let kernel_cr3 = cr3_frame.start_address().as_u64();
    KERNEL_CR3.store(kernel_cr3, Ordering::Relaxed);
    const P_W: u64 = 0b11; // present, writable
    const HUGE: u64 = 1 << 7;
    // SAFETY: reading the live PML4 through the physical window; writing
    // only the reserved window.
    unsafe {
        let kernel_pml4 = virt(kernel_cr3) as *const u64;
        let pml4 = virt(base + hsmp::PAGE) as *mut u64;
        core::ptr::copy_nonoverlapping(kernel_pml4, pml4, 512);
        *pml4 = (base + 2 * hsmp::PAGE) | P_W;
        *(virt(base + 2 * hsmp::PAGE) as *mut u64) = (base + 3 * hsmp::PAGE) | P_W;
        *(virt(base + 3 * hsmp::PAGE) as *mut u64) = HUGE | P_W; // 0..2 MiB
    }
    Ok(())
}

/// Writes one 64-bit data field of the live trampoline copy.
fn write_field(label: &u8, v: u64) {
    let base = *TRAMPOLINE.get().unwrap();
    let p = (crate::memory::physical_memory_offset().as_u64() + base + off(label)) as *mut u64;
    // SAFETY: inside the reserved window; 8-aligned by the `.balign 8`s.
    unsafe { core::ptr::write_volatile(p, v) }
}

fn read_field(label: &u8) -> u64 {
    let base = *TRAMPOLINE.get().unwrap();
    let p = (crate::memory::physical_memory_offset().as_u64() + base + off(label)) as *const u64;
    // SAFETY: as in `write_field`.
    unsafe { core::ptr::read_volatile(p) }
}

fn start_one(cpu: usize, apic_id: u32, vector: u8) {
    let stack = alloc::vec![0u8; AP_STACK_SIZE].leak();
    let top = (stack.as_mut_ptr() as u64 + AP_STACK_SIZE as u64) & !0xF;
    // SAFETY: only the address is taken.
    unsafe {
        write_field(&ap_tramp_stack, top);
        write_field(&ap_tramp_cpu, cpu as u64);
        write_field(&ap_tramp_stage, 0);
    }
    STATE[cpu].store(ApState::Starting as u8, Ordering::Release);

    let t0 = crate::cpu::tsc::read();
    for step in hsmp::startup_sequence(vector) {
        match step {
            Step::Send(icr) => {
                if !crate::interrupts::apic::send_ipi(apic_id, icr) {
                    STATE[cpu].store(ApState::IpiStuck as u8, Ordering::Relaxed);
                    return;
                }
            }
            Step::Delay(us) => {
                // Stop waiting early once it has reported in: the second
                // SIPI is then pointless (and ignored).
                if wait_us(us, || reported(cpu)) {
                    break;
                }
            }
        }
    }

    wait_us(AP_TIMEOUT_US, || reported(cpu));
    let us = crate::cpu::tsc::read().wrapping_sub(t0) * 1_000_000 / crate::cpu::tsc::freq_hz().max(1);
    UP_US[cpu].store(us.min(u32::MAX as u64) as u32, Ordering::Relaxed);
    // SAFETY: only the address is taken.
    STAGE_REACHED[cpu].store(unsafe { read_field(&ap_tramp_stage) } as u8, Ordering::Relaxed);

    if !reported(cpu) {
        STATE[cpu].store(ApState::NoResponse as u8, Ordering::Relaxed);
        // Park it, so it can't wake up late on the next AP's stack.
        crate::interrupts::apic::send_ipi(apic_id, hsmp::icr::init_assert());
    }
}

/// Has this AP left `Starting` (for better or worse)?
fn reported(cpu: usize) -> bool {
    STATE[cpu].load(Ordering::Acquire) != ApState::Starting as u8
}

/// Spins up to `us` microseconds of TSC time (and a spin bound), returning
/// early with `true` as soon as `done()` holds.
fn wait_us(us: u64, done: impl Fn() -> bool) -> bool {
    let cycles = crate::cpu::tsc::freq_hz() / 1_000_000 * us;
    let t0 = crate::cpu::tsc::read();
    let mut spins: u64 = 0;
    while crate::cpu::tsc::read().wrapping_sub(t0) < cycles {
        if done() {
            return true;
        }
        spins += 1;
        if spins > us.saturating_mul(10_000) {
            break;
        }
        core::hint::spin_loop();
    }
    done()
}

/// Where the trampoline lands, on the AP's own stack, with IF=0, still on
/// the trampoline's page tables and GDT.
extern "sysv64" fn ap_entry(cpu: u64) -> ! {
    let cpu = cpu as usize;
    // SAFETY: the kernel's own PML4, which maps everything this code touches
    // (the trampoline's copy of its higher half is what got us here).
    // Not `memory::tlb::switch_to`: TR is not loaded yet, so `cpu_id()`
    // would say 0 and publish this CR3 as the BSP's. `this_cpu_ready`
    // publishes it below, before this CPU can be told anything.
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) KERNEL_CR3.load(Ordering::Relaxed), options(nostack));
    }
    match crate::cpu::init_this_cpu(cpu) {
        Ok(()) => {
            crate::memory::tlb::this_cpu_ready(cpu);
            STATE[cpu].store(ApState::Online as u8, Ordering::Release);
            // Inert: nothing is routed here and the timer is masked. IF=0
            // while the mailbox is checked, so a `WAKE_VECTOR` sent after
            // the check is still pending at the `hlt` and wakes it.
            loop {
                if GO.load(Ordering::Acquire) {
                    crate::process::start_ap_scheduling();
                }
                let work = MAILBOX[cpu].load(Ordering::Acquire);
                if work != 0 {
                    // SAFETY: only `run_on` stores here, and only `fn(usize)`s.
                    let f: fn(usize) = unsafe { core::mem::transmute(work) };
                    x86_64::instructions::interrupts::enable();
                    f(cpu);
                    x86_64::instructions::interrupts::disable();
                    MAILBOX[cpu].store(0, Ordering::Release);
                    continue;
                }
                // SAFETY: `sti; hlt` as one sequence: no wakeup is lost
                // between them (STI's one-instruction shadow).
                unsafe { core::arch::asm!("sti; hlt; cli", options(nomem, nostack)) };
            }
        }
        Err(e) => {
            INIT_ERROR[cpu].call_once(|| e);
            STATE[cpu].store(ApState::InitFailed as u8, Ordering::Release);
            loop {
                // SAFETY: a CPU whose per-CPU state is wrong must run nothing.
                unsafe { core::arch::asm!("cli; hlt", options(nomem, nostack)) };
            }
        }
    }
}

/// The local APIC ID of kernel CPU `cpu` (what an IPI is addressed to).
pub fn apic_id(cpu: usize) -> u32 {
    APIC_ID[cpu].load(Ordering::Relaxed)
}

/// Is `cpu` an AP that came up (and so runs its mailbox)?
pub fn is_online_ap(cpu: usize) -> bool {
    cpu < MAX_CPUS && ApState::from_u8(STATE[cpu].load(Ordering::Acquire)) == ApState::Online
}

/// Runs `f(cpu)` on the online AP `cpu`, with IF=1, from its idle loop
/// (the boot loop before the scheduler starts, its idle process after —
/// so the job waits while the AP runs a process). The scheduler does not
/// take the AP off its idle process while the job runs (`ap_busy`).
/// Returns once it is posted; `ap_busy` says when it has returned. `false`
/// if `cpu` is not an online AP or its mailbox is still busy. For
/// self-tests (see `tlb_selftest`).
pub fn run_on(cpu: usize, f: fn(usize)) -> bool {
    if !is_online_ap(cpu) {
        return false;
    }
    if MAILBOX[cpu]
        .compare_exchange(0, f as usize, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return false;
    }
    x86_64::instructions::interrupts::without_interrupts(|| {
        crate::interrupts::apic::send_ipi(apic_id(cpu), hal::smp::icr::fixed(WAKE_VECTOR))
    })
}

/// Is `cpu` still running what `run_on` gave it?
pub fn ap_busy(cpu: usize) -> bool {
    cpu < MAX_CPUS && MAILBOX[cpu].load(Ordering::Acquire) != 0
}

/// How many CPUs are running (the BSP included).
pub fn online() -> usize {
    STATE.iter()
        .filter(|s| matches!(ApState::from_u8(s.load(Ordering::Relaxed)), ApState::Bsp | ApState::Online))
        .count()
}

/// `/proc/kdebug` line.
pub fn render() -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::format!(
        "smp: madt {} cpus, {} online",
        MADT_CPUS.load(Ordering::Relaxed),
        online(),
    );
    if let Some(r) = NOT_TRIED.get() {
        let _ = write!(s, " (APs not started: {})", r);
    }
    let dropped = DROPPED.load(Ordering::Relaxed);
    if dropped > 0 {
        let _ = write!(s, " ({} beyond MAX_CPUS={})", dropped, MAX_CPUS);
    }
    s.push(':');
    for cpu in 0..MAX_CPUS {
        let st = ApState::from_u8(STATE[cpu].load(Ordering::Relaxed));
        let id = APIC_ID[cpu].load(Ordering::Relaxed);
        let us = UP_US[cpu].load(Ordering::Relaxed);
        match st {
            ApState::Absent => {}
            ApState::Bsp => { let _ = write!(s, " cpu{}=apic{}/bsp", cpu, id); }
            ApState::Online => { let _ = write!(s, " cpu{}=apic{}/{}us", cpu, id, us); }
            ApState::Starting => { let _ = write!(s, " cpu{}=apic{}/STARTING", cpu, id); }
            ApState::IpiStuck => { let _ = write!(s, " cpu{}=apic{}/IPI-STUCK", cpu, id); }
            ApState::NoResponse => {
                let _ = write!(
                    s, " cpu{}=apic{}/NO-RESPONSE(stage {})",
                    cpu, id, STAGE_REACHED[cpu].load(Ordering::Relaxed)
                );
            }
            ApState::InitFailed => {
                let (step, why) = INIT_ERROR[cpu].get().copied().unwrap_or(("?", "?"));
                let _ = write!(s, " cpu{}=apic{}/INIT-FAILED({}: {})", cpu, id, step, why);
            }
        }
    }
    s
}
