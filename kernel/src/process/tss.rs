// kernel/src/process/tss.rs
// Production version - Minimal logging

use x86_64::VirtAddr;
use x86_64::structures::tss::TaskStateSegment;
use x86_64::structures::gdt::{GlobalDescriptorTable, Descriptor, SegmentSelector};
use spin::Once;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

struct Selectors {
    code_selector: SegmentSelector,
    data_selector: SegmentSelector,
    user_code_selector: SegmentSelector,
    user_data_selector: SegmentSelector,
    tss_selector: SegmentSelector,
}

// TSS estático - ubicación fija en memoria
static mut TSS: TaskStateSegment = TaskStateSegment::new();

/// Top of the current process's kernel stack.
/// Mirrored from TSS.privilege_stack_table[0] so that syscall_entry_fast
/// can load it without parsing the TSS structure.
/// Single-CPU only — safe because syscall entry always runs with IF=0.
#[no_mangle]
pub static mut KERNEL_RSP0: u64 = 0;

// GDT se inicializa una vez
static GDT: Once<(GlobalDescriptorTable, Selectors)> = Once::new();

/// Inicializa el TSS y GDT
pub fn init() {
    // Inicializar TSS con stacks
    unsafe {
        // Stack para double fault (IST)
        TSS.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = {
            const STACK_SIZE: usize = 4096 * 5;
            static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];
            
            let stack_start = VirtAddr::from_ptr(&raw const STACK);
            let stack_end = stack_start + STACK_SIZE as u64;
            stack_end
        };
        
        // Stack de kernel inicial para syscalls (RSP0)
        TSS.privilege_stack_table[0] = {
            const STACK_SIZE: usize = 4096 * 5;
            static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];
            
            let stack_start = VirtAddr::from_ptr(&raw const STACK);
            let stack_end = stack_start + STACK_SIZE as u64;
            stack_end
        };
    }
    
    // Crear GDT
    GDT.call_once(|| {
        let mut gdt = GlobalDescriptorTable::new();
        
        // Segmentos de kernel (Ring 0)
        let code_selector = gdt.append(Descriptor::kernel_code_segment());
        let data_selector = gdt.append(Descriptor::kernel_data_segment());
        
        // Segmentos de user (Ring 3)
        let user_data_selector = gdt.append(Descriptor::user_data_segment());
        let user_code_selector = gdt.append(Descriptor::user_code_segment());
        
        // TSS - la GDT apunta directamente a la ubicación estática
        let tss_selector = unsafe {
            gdt.append(Descriptor::tss_segment(&TSS))
        };
        
        (gdt, Selectors {
            code_selector,
            data_selector,
            user_code_selector,
            user_data_selector,
            tss_selector,
        })
    });
    
    // Cargar GDT
    GDT.get().unwrap().0.load();
    
    unsafe {
        use x86_64::instructions::tables::load_tss;
        use x86_64::instructions::segmentation::{CS, DS, Segment};
        
        // Cargar segmentos de kernel
        CS::set_reg(GDT.get().unwrap().1.code_selector);
        DS::set_reg(GDT.get().unwrap().1.data_selector);
        
        // Cargar TSS
        load_tss(GDT.get().unwrap().1.tss_selector);
    }

    crate::serial_println!("TSS and GDT initialized");
}

/// Obtiene los selectores de segmento para user space
pub fn get_user_selectors() -> (SegmentSelector, SegmentSelector) {
    let selectors = &GDT.get().unwrap().1;
    (selectors.user_code_selector, selectors.user_data_selector)
}

/// Actualiza el kernel stack del proceso actual en el TSS
/// 
/// SAFETY: Solo debe ser llamado con interrupciones deshabilitadas
/// o desde un contexto donde sabemos que el TSS no está siendo usado
pub fn set_kernel_stack(stack_top: VirtAddr) {
    unsafe {
        TSS.privilege_stack_table[0] = stack_top;
        KERNEL_RSP0 = stack_top.as_u64();
    }
}

/// Configure MSRs so that the `syscall` instruction enters the kernel via
/// `syscall_entry_fast` (defined in syscall.rs).
///
/// GDT layout assumed (matches the append order in `init()`):
///   0x08 = kernel CS,  0x10 = kernel SS
///   0x1b = user SS,    0x23 = user CS
///
/// STAR[47:32] = 0x0008 → syscall sets CS=0x08, SS=0x10
/// STAR[63:48] = 0x0010 → sysretq would set CS=0x23, SS=0x1b  (we use iretq)
/// LSTAR       = address of syscall_entry_fast
/// SFMASK      = clear IF (bit 9) so we enter with interrupts disabled
pub fn init_syscall_msrs() {
    extern "C" { fn syscall_entry_fast(); }

    const IA32_EFER:  u32 = 0xC000_0080;
    const IA32_STAR:  u32 = 0xC000_0081;
    const IA32_LSTAR: u32 = 0xC000_0082;
    const IA32_FMASK: u32 = 0xC000_0084;

    unsafe {
        // Enable SCE (System Call Extensions) in EFER
        let efer = rdmsr(IA32_EFER);
        wrmsr(IA32_EFER, efer | 1);

        // STAR: kernel selectors for syscall, user selectors for sysretq
        wrmsr(IA32_STAR, (0x0010u64 << 48) | (0x0008u64 << 32));

        // LSTAR: 64-bit kernel entry point
        wrmsr(IA32_LSTAR, syscall_entry_fast as u64);

        // SFMASK: clear IF on entry (bit 9)
        wrmsr(IA32_FMASK, 1 << 9);
    }

    crate::serial_println!("syscall MSRs configured (LSTAR={:#x})", syscall_entry_fast as u64);
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