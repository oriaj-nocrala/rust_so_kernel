// kernel/src/process/tss.rs

use x86_64::VirtAddr;
use x86_64::structures::tss::TaskStateSegment;
use x86_64::structures::gdt::{GlobalDescriptorTable, Descriptor, SegmentSelector};
use lazy_static::lazy_static;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

struct Selectors {
    code_selector: SegmentSelector,
    data_selector: SegmentSelector,
    user_code_selector: SegmentSelector,
    user_data_selector: SegmentSelector,
    tss_selector: SegmentSelector,
}

lazy_static! {
    static ref TSS: TaskStateSegment = {
        let mut tss = TaskStateSegment::new();
        
        // Stack para double fault (IST)
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = {
            const STACK_SIZE: usize = 4096 * 5;
            static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];
            
            let stack_start = VirtAddr::from_ptr({ &raw const STACK });
            let stack_end = stack_start + STACK_SIZE as u64;  // ✅ Fix: cast a u64
            stack_end
        };
        
        // Stack de kernel para syscalls (RSP0)
        tss.privilege_stack_table[0] = {
            const STACK_SIZE: usize = 4096 * 5;
            static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];
            
            let stack_start = VirtAddr::from_ptr({ &raw const STACK });
            let stack_end = stack_start + STACK_SIZE as u64;  // ✅ Fix: cast a u64
            stack_end
        };
        
        tss
    };
    
    static ref GDT: (GlobalDescriptorTable, Selectors) = {
        let mut gdt = GlobalDescriptorTable::new();
        
        // Segmentos de kernel (Ring 0)
        let code_selector = gdt.append(Descriptor::kernel_code_segment());  // ✅ Fix: append
        let data_selector = gdt.append(Descriptor::kernel_data_segment());  // ✅ Fix: append
        
        // Segmentos de user (Ring 3)
        let user_data_selector = gdt.append(Descriptor::user_data_segment());  // ✅ Fix: append
        let user_code_selector = gdt.append(Descriptor::user_code_segment());  // ✅ Fix: append
        
        // TSS
        let tss_selector = gdt.append(Descriptor::tss_segment(&TSS));  // ✅ Fix: append + referencia
        
        (gdt, Selectors {
            code_selector,
            data_selector,
            user_code_selector,
            user_data_selector,
            tss_selector,
        })
    };
}

/// Inicializa el TSS y GDT
pub fn init() {
    use x86_64::instructions::tables::load_tss;
    use x86_64::instructions::segmentation::{CS, DS, Segment};

    // Cargar GDT
    GDT.0.load();
    
    unsafe {
        // Cargar segmentos de kernel
        CS::set_reg(GDT.1.code_selector);
        DS::set_reg(GDT.1.data_selector);
        
        // Cargar TSS
        load_tss(GDT.1.tss_selector);
    }

    crate::serial_println!("TSS and GDT initialized");
}

/// Obtiene los selectores de segmento para user space
pub fn get_user_selectors() -> (SegmentSelector, SegmentSelector) {
    (GDT.1.user_code_selector, GDT.1.user_data_selector)
}

// ✅ REMOVIDO: set_kernel_stack() - no podemos mutar TSS después de crearlo
// En su lugar, cada proceso tendrá su propio TSS (implementación futura)