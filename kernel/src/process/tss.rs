// kernel/src/process/tss.rs

use x86_64::VirtAddr;
use x86_64::structures::tss::TaskStateSegment;
use x86_64::structures::gdt::{GlobalDescriptorTable, Descriptor, SegmentSelector};
use spin::{Mutex, Once};

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

struct Selectors {
    code_selector: SegmentSelector,
    data_selector: SegmentSelector,
    user_code_selector: SegmentSelector,
    user_data_selector: SegmentSelector,
    tss_selector: SegmentSelector,
}

// ✅ TSS mutable con Mutex
static TSS: Mutex<TaskStateSegment> = Mutex::new(TaskStateSegment::new());

// ✅ Puntero estático al TSS (para la GDT)
static TSS_PTR: Once<usize> = Once::new();

// GDT se inicializa una vez
static GDT: Once<(GlobalDescriptorTable, Selectors)> = Once::new();

/// Inicializa el TSS y GDT
pub fn init() {
    // 1. Inicializar TSS con stacks
    {
        let mut tss = TSS.lock();
        
        // Stack para double fault (IST)
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = {
            const STACK_SIZE: usize = 4096 * 5;
            static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];
            
            let stack_start = VirtAddr::from_ptr(unsafe { &raw const STACK });
            let stack_end = stack_start + STACK_SIZE as u64;
            stack_end
        };
        
        // Stack de kernel inicial para syscalls (RSP0)
        tss.privilege_stack_table[0] = {
            const STACK_SIZE: usize = 4096 * 5;
            static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];
            
            let stack_start = VirtAddr::from_ptr(unsafe { &raw const STACK });
            let stack_end = stack_start + STACK_SIZE as u64;
            stack_end
        };
        
        // ✅ Guardar puntero estático al TSS
        TSS_PTR.call_once(|| &*tss as *const TaskStateSegment as usize);
    }
    
    // 2. Crear GDT
    GDT.call_once(|| {
        let mut gdt = GlobalDescriptorTable::new();
        
        // Segmentos de kernel (Ring 0)
        let code_selector = gdt.append(Descriptor::kernel_code_segment());
        let data_selector = gdt.append(Descriptor::kernel_data_segment());
        
        // Segmentos de user (Ring 3)
        let user_data_selector = gdt.append(Descriptor::user_data_segment());
        let user_code_selector = gdt.append(Descriptor::user_code_segment());
        
        // ✅ TSS usando el puntero estático
        let tss_selector = {
            let tss_ref = unsafe { 
                let ptr = *TSS_PTR.get().unwrap() as *const TaskStateSegment;
                &*ptr
            };
            gdt.append(Descriptor::tss_segment(tss_ref))
        };
        
        (gdt, Selectors {
            code_selector,
            data_selector,
            user_code_selector,
            user_data_selector,
            tss_selector,
        })
    });
    
    // 3. Cargar GDT
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

/// ✅ Actualiza el kernel stack del proceso actual en el TSS
pub fn set_kernel_stack(stack_top: VirtAddr) {
    let mut tss = TSS.lock();
    tss.privilege_stack_table[0] = stack_top;
    
    crate::serial_println!("TSS: Updated kernel stack to {:#x}", stack_top.as_u64());
}