// kernel/src/process/tss.rs
//
// GDT, TSSs and the `syscall` MSRs — stage 3 of docs/smp/smp-plan.md.
//
// ONE GDT for every CPU, with one TSS descriptor slot per CPU: CPU n's TSS
// sits at `percpu::FIRST_TSS_SELECTOR + 16 * n`. Not one GDT per CPU with the
// TSS at the same index: `cpu::cpu_id()` reads the task register, and TR would
// then read the same on every CPU (see `cpu::percpu`'s module comment).
//
// Each CPU also has its own TSS (its own `rsp0`, the stack interrupts from
// ring 3 land on) and its own double-fault IST stack: two CPUs faulting at
// once must not share a stack.
//
// Everything here that is per CPU runs from `cpu::init_this_cpu`.

use core::cell::UnsafeCell;

use x86_64::VirtAddr;
use x86_64::structures::tss::TaskStateSegment;
use x86_64::structures::gdt::{GlobalDescriptorTable, Descriptor, SegmentSelector};
use spin::Once;

use crate::cpu::MAX_CPUS;
use crate::cpu::percpu::FIRST_TSS_SELECTOR;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// Kernel code/data, user data/code, then one 16-byte TSS descriptor (two
/// entries) per CPU. The null entry is not counted by `GlobalDescriptorTable`.
const GDT_LEN: usize = 5 + 2 * MAX_CPUS;

const IST_STACK_SIZE: usize = 4096 * 5;
const BOOT_RSP0_STACK_SIZE: usize = 4096 * 5;

struct Selectors {
    code_selector: SegmentSelector,
    data_selector: SegmentSelector,
    user_code_selector: SegmentSelector,
    user_data_selector: SegmentSelector,
    tss_selectors: [SegmentSelector; MAX_CPUS],
}

/// One CPU's TSS. The CPU reads it (RSP0, IST) on every interrupt through the
/// descriptor, so it must stay at a fixed address; only its own CPU writes it
/// (`set_kernel_stack`, `init_this_cpu`), with interrupts off.
#[repr(transparent)]
struct TssSlot(UnsafeCell<TaskStateSegment>);
// SAFETY: slot n is only written by CPU n (see above).
unsafe impl Sync for TssSlot {}

static TSS: [TssSlot; MAX_CPUS] =
    [const { TssSlot(UnsafeCell::new(TaskStateSegment::new())) }; MAX_CPUS];

/// A stack only the CPU itself ever runs on.
#[repr(C, align(16))]
struct Stack<const N: usize>(UnsafeCell<[u8; N]>);
// SAFETY: never accessed from Rust, only handed to the CPU as a stack top.
unsafe impl<const N: usize> Sync for Stack<N> {}

impl<const N: usize> Stack<N> {
    fn top(&self) -> VirtAddr {
        VirtAddr::from_ptr(self.0.get()) + N as u64
    }
}

static DOUBLE_FAULT_STACKS: [Stack<IST_STACK_SIZE>; MAX_CPUS] =
    [const { Stack(UnsafeCell::new([0; IST_STACK_SIZE])) }; MAX_CPUS];
/// RSP0 until the first process on this CPU sets its own kernel stack.
static BOOT_RSP0_STACKS: [Stack<BOOT_RSP0_STACK_SIZE>; MAX_CPUS] =
    [const { Stack(UnsafeCell::new([0; BOOT_RSP0_STACK_SIZE])) }; MAX_CPUS];

static GDT: Once<(GlobalDescriptorTable<GDT_LEN>, Selectors)> = Once::new();

fn gdt() -> &'static (GlobalDescriptorTable<GDT_LEN>, Selectors) {
    GDT.call_once(|| {
        let mut gdt = GlobalDescriptorTable::<GDT_LEN>::empty();

        // Order fixed by `init_syscall_msrs`'s STAR layout.
        let code_selector = gdt.append(Descriptor::kernel_code_segment());
        let data_selector = gdt.append(Descriptor::kernel_data_segment());
        let user_data_selector = gdt.append(Descriptor::user_data_segment());
        let user_code_selector = gdt.append(Descriptor::user_code_segment());

        let mut tss_selectors = [SegmentSelector(0); MAX_CPUS];
        for (cpu, sel) in tss_selectors.iter_mut().enumerate() {
            // SAFETY: `TSS[cpu]` is a static, so it outlives the GDT, and
            // nothing moves it.
            *sel = gdt.append(unsafe { Descriptor::tss_segment_unchecked(TSS[cpu].0.get()) });
            // `cpu::cpu_id()` reads the CPU number out of TR.
            assert_eq!(
                sel.0,
                FIRST_TSS_SELECTOR + 16 * cpu as u16,
                "TSS selector moved: cpu::cpu_id() would misread TR",
            );
        }

        (gdt, Selectors {
            code_selector,
            data_selector,
            user_code_selector,
            user_data_selector,
            tss_selectors,
        })
    })
}

/// This CPU's TSS stacks, the shared GDT, the kernel segments and this CPU's
/// TR. Idempotent (a second call on a CPU whose TR is already loaded skips
/// `ltr`, which would fault on a busy descriptor), so a test can re-run it.
pub fn init_this_cpu(cpu: usize) {
    // SAFETY: slot `cpu` belongs to this CPU, interrupts are off during
    // per-CPU init, and the TSS is not live until `ltr` below (or, on a
    // re-run, only read by the CPU on an interrupt, which cannot happen).
    unsafe {
        let tss = &mut *TSS[cpu].0.get();
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = DOUBLE_FAULT_STACKS[cpu].top();
        let rsp0 = tss.privilege_stack_table[0]; // packed: copy, don't borrow
        if rsp0.is_null() {
            tss.privilege_stack_table[0] = BOOT_RSP0_STACKS[cpu].top();
        }
    }

    let (gdt, sel) = gdt();
    gdt.load();

    unsafe {
        use x86_64::instructions::tables::load_tss;
        use x86_64::instructions::segmentation::{CS, DS, ES, SS, Segment};

        CS::set_reg(sel.code_selector);
        DS::set_reg(sel.data_selector);
        ES::set_reg(sel.data_selector);
        SS::set_reg(sel.data_selector);

        if read_tr() != sel.tss_selectors[cpu] {
            load_tss(sel.tss_selectors[cpu]);
        }
    }
}

/// Reads back what `init_this_cpu` set.
pub fn verify_this_cpu(cpu: usize) -> Result<(), &'static str> {
    use x86_64::instructions::segmentation::{CS, SS, Segment};
    let (gdt, sel) = gdt();

    let gdtr = x86_64::instructions::tables::sgdt();
    if gdtr.base.as_u64() != gdt.entries().as_ptr() as u64 || gdtr.limit != gdt.limit() {
        return Err("GDTR is not the kernel GDT");
    }
    if read_tr() != sel.tss_selectors[cpu] {
        return Err("TR is not this CPU's TSS slot");
    }
    if crate::cpu::cpu_id() != cpu {
        return Err("cpu_id() does not name this CPU");
    }
    if CS::get_reg() != sel.code_selector || SS::get_reg() != sel.data_selector {
        return Err("CS/SS are not the kernel selectors");
    }
    // SAFETY: read-only; this CPU's slot.
    // The TSS is packed: copy the arrays out rather than borrow into it.
    let (ist, rsp) = unsafe {
        let tss = &*TSS[cpu].0.get();
        (tss.interrupt_stack_table, tss.privilege_stack_table)
    };
    if ist[DOUBLE_FAULT_IST_INDEX as usize] != DOUBLE_FAULT_STACKS[cpu].top() {
        return Err("double-fault IST is not this CPU's stack");
    }
    if rsp[0].is_null() {
        return Err("TSS rsp0 unset");
    }
    Ok(())
}

/// Obtiene los selectores de segmento para user space
pub fn get_user_selectors() -> (SegmentSelector, SegmentSelector) {
    let selectors = &gdt().1;
    (selectors.user_code_selector, selectors.user_data_selector)
}

/// Sets the running process's kernel stack on this CPU: its TSS `rsp0`
/// (interrupts from ring 3) and its `PerCpu::kernel_rsp` (`syscall`).
///
/// Interrupts must be off: the TSS is live.
pub fn set_kernel_stack(stack_top: VirtAddr) {
    let cpu = crate::cpu::cpu_id();
    // SAFETY: this CPU's own slot, IF=0 (see above).
    unsafe {
        (*TSS[cpu].0.get()).privilege_stack_table[0] = stack_top;
    }
    crate::cpu::percpu::set_kernel_rsp(stack_top.as_u64());
}

const IA32_EFER:  u32 = 0xC000_0080;
const IA32_STAR:  u32 = 0xC000_0081;
const IA32_LSTAR: u32 = 0xC000_0082;
const IA32_FMASK: u32 = 0xC000_0084;

/// STAR[47:32] = 0x0008 → syscall sets CS=0x08, SS=0x10
/// STAR[63:48] = 0x0010 → sysretq would set CS=0x23, SS=0x1b  (we use iretq)
const STAR_VALUE: u64 = (0x0010u64 << 48) | (0x0008u64 << 32);
/// Clear IF (bit 9) so we enter with interrupts disabled. ALSO clear DF
/// (bit 10): user space may leave it set (e.g. a memmove interrupted between
/// std/cld); a `rep movsb` executed by the kernel with DF=1 would copy
/// BACKWARD (the root cause of the 2026-08-05 box-copy corruption — see
/// docs/hang-hunt-bug2-findings.md). Linux masks DF in FMASK for exactly this
/// reason.
const FMASK_VALUE: u64 = (1 << 9) | (1 << 10);

fn lstar_value() -> u64 {
    extern "C" { fn syscall_entry_fast(); }
    syscall_entry_fast as u64
}

/// Configure this CPU's MSRs so that the `syscall` instruction enters the
/// kernel via `syscall_entry_fast` (defined in syscall.rs).
///
/// GDT layout assumed (matches the append order in `gdt()`):
///   0x08 = kernel CS,  0x10 = kernel SS
///   0x1b = user SS,    0x23 = user CS
pub fn init_syscall_msrs() {
    unsafe {
        // Enable SCE (System Call Extensions) in EFER
        let efer = rdmsr(IA32_EFER);
        wrmsr(IA32_EFER, efer | 1);
        wrmsr(IA32_STAR, STAR_VALUE);
        wrmsr(IA32_LSTAR, lstar_value());
        wrmsr(IA32_FMASK, FMASK_VALUE);
    }
}

/// Reads back what `init_syscall_msrs` set.
pub fn verify_syscall_msrs() -> Result<(), &'static str> {
    unsafe {
        if rdmsr(IA32_EFER) & 1 == 0 {
            return Err("EFER.SCE clear");
        }
        if rdmsr(IA32_STAR) != STAR_VALUE {
            return Err("STAR differs");
        }
        if rdmsr(IA32_LSTAR) != lstar_value() {
            return Err("LSTAR is not syscall_entry_fast");
        }
        if rdmsr(IA32_FMASK) != FMASK_VALUE {
            return Err("FMASK differs");
        }
    }
    Ok(())
}

fn read_tr() -> SegmentSelector {
    let tr: u16;
    unsafe {
        core::arch::asm!("str {0:x}", out(reg) tr, options(nomem, nostack, preserves_flags));
    }
    SegmentSelector(tr)
}

#[inline]
unsafe fn wrmsr(msr: u32, value: u64) {
    core::arch::asm!(
        "wrmsr",
        in("ecx") msr,
        in("eax") value as u32,
        in("edx") (value >> 32) as u32,
        options(nostack, nomem),
    );
}

#[inline]
unsafe fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    core::arch::asm!(
        "rdmsr",
        in("ecx") msr,
        out("eax") lo,
        out("edx") hi,
        options(nostack, nomem),
    );
    lo as u64 | ((hi as u64) << 32)
}
