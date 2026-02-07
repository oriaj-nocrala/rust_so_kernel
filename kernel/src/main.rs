#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

mod allocator;
mod framebuffer;
mod interrupts;
mod keyboard;
mod keyboard_buffer;
mod memory;
mod panic;
mod process;
mod pit;
mod repl;
mod serial;

use alloc::{boxed::Box, format, vec::Vec};
// use alloc::{string::ToString, vec::Vec};
use bootloader_api::{BootInfo, BootloaderConfig, config::Mapping, entry_point, info::{MemoryRegion, MemoryRegionKind}};
use framebuffer::Framebuffer;
use interrupts::idt::InterruptDescriptorTable;
use spin::Once;
use x86_64::{VirtAddr, structures::paging::FrameAllocator};
use process::{Process, Pid, scheduler::SCHEDULER};
use crate::allocator::FRAME_ALLOCATOR;

use crate::{
    framebuffer::{Color, init_global_framebuffer}, interrupts::exception::ExceptionStackFrame, memory::{frame_allocator::{self, BootInfoFrameAllocator}, paging::ActivePageTable}, repl::Repl
};

static IDT: Once<InterruptDescriptorTable> = Once::new();

extern "C" {
    fn syscall_entry();
}

fn init_idt() {
    IDT.call_once(|| {
        let mut idt = InterruptDescriptorTable::new();
        idt.add_handler(0, divide_by_zero_handler);
        idt.add_handler(6, invalid_opcode_handler);
        idt.add_double_fault_handler(8, double_fault_handler);
        idt.add_handler_with_error(13, general_protection_fault_handler);
        idt.add_handler_with_error(14, page_fault_handler);
        idt.add_handler(32, timer_handler);
        idt.add_handler(33, keyboard_interrupt_handler);
        idt.entries[0x80].set_handler_addr(syscall_entry as u64);
        idt
    });
}

fn load_idt() {
    IDT.get().unwrap().load();
}

extern "x86-interrupt" fn keyboard_interrupt_handler(_: &mut ExceptionStackFrame) {
    let scancode = unsafe {
        x86_64::instructions::port::PortReadOnly::<u8>::new(0x60).read()
    };
    keyboard::process_scancode(scancode);
    interrupts::pic::end_of_interrupt(interrupts::pic::Irq::Keyboard.as_u8());
}

// Helper para debug serial
fn debug_log(s: &str) {
    unsafe {
        let mut port = x86_64::instructions::port::PortWriteOnly::<u8>::new(0x3F8);
        for byte in s.bytes() {
            port.write(byte);
        }
    }
}

extern "x86-interrupt" fn divide_by_zero_handler(sf: &mut ExceptionStackFrame) {
    panic!("DIVIDE BY ZERO at {:#x}", sf.instruction_pointer);
}

extern "x86-interrupt" fn invalid_opcode_handler(sf: &mut ExceptionStackFrame) {
    panic!("INVALID OPCODE at {:#x}", sf.instruction_pointer);
}

extern "x86-interrupt" fn double_fault_handler(
    sf: &mut ExceptionStackFrame,
    error_code: u64
) -> ! {
    panic!("DOUBLE FAULT (error: {}) at {:#x}", error_code, sf.instruction_pointer);
}

extern "x86-interrupt" fn general_protection_fault_handler(
    sf: &mut ExceptionStackFrame,
    error_code: u64
) {
    panic!("GENERAL PROTECTION FAULT (error: {}) at {:#x}", error_code, sf.instruction_pointer);
}

extern "x86-interrupt" fn page_fault_handler(
    sf: &mut ExceptionStackFrame,
    error_code: u64
) {
    // Leer CR2 para ver qué dirección causó el fault
    let fault_address: u64;
    unsafe {
        core::arch::asm!("mov {}, cr2", out(reg) fault_address);
    }
    
    panic!(
        "PAGE FAULT\nAddress: {:#x}\nError code: {:b}\nRIP: {:#x}",
        fault_address,
        error_code,
        sf.instruction_pointer
    );
}

extern "x86-interrupt" fn timer_handler(_sf: &mut ExceptionStackFrame) {
    // Por ahora, solo reconocer que ocurrió
    
    // Enviar EOI (End of Interrupt) al PIC
    unsafe {
        use x86_64::instructions::port::PortWriteOnly;
        PortWriteOnly::<u8>::new(0x20).write(0x20);
    }
}

// 1. Definimos la configuración
pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    // ESTO es lo que hace que el offset no sea None
    config.mappings.physical_memory = Some(Mapping::Dynamic); 
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    init_idt();

    let fb = boot_info.framebuffer.as_mut().expect("No framebuffer");
    let info = fb.info();
    let buffer = fb.buffer_mut();

    let framebuffer = Framebuffer::new(
        buffer,
        info.width as usize,
        info.height as usize,
        info.stride as usize,
        info.bytes_per_pixel as usize,
    );

    init_global_framebuffer(framebuffer);

    // Obtener offset de memoria física
    let phys_mem_offset = VirtAddr::new(
        boot_info.physical_memory_offset.into_option().unwrap()
    );

    // ⭐ Guardar globalmente
    memory::init(phys_mem_offset);
    
    
    // --- 2. Inicialización de Memoria Avanzada ---
    
    // A. Inicializamos el Frame Allocator con el mapa de memoria REAL del BIOS/UEFI
    // Esto reemplaza cualquier lógica manual de rangos de memoria que tuvieras antes.
    let frame_allocator = unsafe {
        BootInfoFrameAllocator::init(&boot_info.memory_regions)
    };
    
    // Crear page table
    let page_table = unsafe {
        ActivePageTable::new(phys_mem_offset)  // ← Ahora sí recibe parámetro
    };
    
    allocator::init_allocators(page_table, frame_allocator);


    // --- 2. Inicializar Buddy Allocator ---
    {
        let mut buddy = allocator::buddy_allocator::BUDDY.lock();
        
        for region in boot_info.memory_regions.iter() {
            if region.kind == MemoryRegionKind::Usable {
                unsafe {
                    buddy.add_region(region.start, region.end);
                }
            }
        }
    }  // ← LIBERAR LOCK AQUÍ

    serial_println!("Step 8: Printing Buddy stats (lock released)");
    {
        let buddy = allocator::buddy_allocator::BUDDY.lock();
        buddy.debug_print_stats();
    }  // ← Lock se libera aquí también

    // --- 3. Ahora SÍ podemos usar Slab (String, Vec, format!) ---
    {
        use core::alloc::{GlobalAlloc, Layout};

        let layout = Layout::from_size_align(8, 8).unwrap();

        
        // ✅ CORRECTO: Usar el GLOBAL_ALLOCATOR directamente
        let ptr = unsafe {
            alloc::alloc::alloc(layout)  // ← Esto usa el #[global_allocator]
        };

        if ptr.is_null() {
            serial_println!("  FAILED: Got null pointer");
            panic!("Slab allocation failed");
        } else {
            serial_println!("  SUCCESS: Got pointer {:#x}", ptr as u64);
            
            // Escribir/leer para verificar
            unsafe {
                *(ptr as *mut u64) = 0xDEADBEEF;
                let val = *(ptr as *const u64);
                serial_println!("  Write/read test: {:#x}", val);
                assert_eq!(val, 0xDEADBEEF);
            }
            
            unsafe {
                alloc::alloc::dealloc(ptr, layout);
            }
            serial_println!("  SUCCESS: Deallocation complete");
        }
    }

    // Vec Test
    {
        use alloc::vec::Vec;
        serial_println!("  Creating Vec...");
        let mut v: Vec<u8> = Vec::new();
        serial_println!("  Pushing elements...");
        v.push(1);
        v.push(2);
        v.push(3);
        serial_println!("  Vec OK: len={}", v.len());
    }

    {
        use alloc::string::String;
        serial_println!("  Creating String...");
        let s = String::from("Hello Slab!");
        serial_println!("  String test: {}", s);
    }

    allocator::slab::slab_stats();
    
     // Limpiar pantalla
    {
        let mut fb = framebuffer::FRAMEBUFFER.lock();
        if let Some(fb) = fb.as_mut() {
            fb.clear(Color::rgb(0, 0, 0));
            fb.draw_text(10, 10, "NeoOS v0.1", Color::rgb(0, 200, 255), Color::rgb(0, 0, 0), 2);
            fb.draw_text(10, 770, "Allocator: Ready", Color::rgb(0, 255, 0), Color::rgb(0, 0, 0), 2);
        }
    }

    // Inicializar interrupciones
    interrupts::pic::initialize();
    interrupts::pic::enable_irq(1); // Habilitar IRQ1 (teclado)
    load_idt();
    
    unsafe {
        core::arch::asm!("sti");
    }

    // Justo después de:
    // serial_println!("Step 7: Paging initialized");

    serial_println!("Step 7.5: Pre-mapping user stack for Ring 3");

    // Dirección fija para el user stack (usamos una región alta en user space)
    const USER_STACK_TOP: u64 = 0x0000_7000_0000_2000;  // 2 páginas antes de aquí
    const USER_STACK_SIZE: u64 = 8192;  // 2 páginas (8KB)
    const USER_STACK_BASE: u64 = USER_STACK_TOP - USER_STACK_SIZE;

    unsafe {
        let phys_offset = memory::physical_memory_offset();
        let mut page_table = memory::paging::ActivePageTable::new(phys_offset);
        let mut frame_allocator_lock = FRAME_ALLOCATOR.lock();
        
        // ✅ Unwrap el Option
        let frame_allocator = frame_allocator_lock.as_mut()
            .expect("Frame allocator not initialized");
        
        memory::user_pages::map_user_pages(
            &mut page_table.mapper,
            frame_allocator,  // ← Ya no es Option
            VirtAddr::new(USER_STACK_BASE),
            (USER_STACK_SIZE / 4096) as usize,
        ).expect("Failed to map user stack");
        
        serial_println!(
            "User stack mapped: {:#x} - {:#x}",
            USER_STACK_BASE,
            USER_STACK_TOP
        );
    }

    // Inicializar el PIT
    pit::init(100); // 100 Hz

    let mut repl = Repl::new(10, 50);
    repl.show_prompt();

    serial_println!("Step 9: Initializing TSS and GDT");
    process::tss::init();

    serial_println!("Step 10: Creating test processes");

    // Proceso idle (kernel space)
    {
        let mut scheduler = SCHEDULER.lock();
        let pid = Pid(0);
        
        let page_table = unsafe {
            let (frame, _) = x86_64::registers::control::Cr3::read();
            frame
        };
        
        let idle = Box::new(Process::new(
            pid,
            VirtAddr::new(idle_task as *const () as u64),
            page_table,
        ));
        
        scheduler.add_process(idle);
    }

    // Si activamos esto: Kernel panic! por error de proteccion
    // (aparentemente falta USER_ACCESSIBLE)
    // // ✅ NUEVO: Proceso en user space (Ring 3)
    // {
    //     let mut scheduler = SCHEDULER.lock();
    //     let pid = scheduler.allocate_pid();
        
    //     let page_table = unsafe {
    //         let (frame, _) = x86_64::registers::control::Cr3::read();
    //         frame
    //     };
        
    //     let mut proc = Box::new(Process::new_user(
    //         pid,
    //         VirtAddr::new(process::user_test_function as *const () as u64),
    //         page_table,
    //     ));
    //     proc.set_name("user_test")   ;
        
    //     scheduler.add_process(proc);
    // }

    {
        let mut scheduler = SCHEDULER.lock();
        let pid = scheduler.allocate_pid();
        
        let page_table = unsafe {
            let (frame, _) = x86_64::registers::control::Cr3::read();
            frame
        };
        
        let mut proc = Box::new(Process::new(
            pid,
            VirtAddr::new(shell_process as *const () as u64),
            page_table,
        ));
        proc.set_name("shell");
        
        scheduler.add_process(proc);
    }

    serial_println!("Processes created!");

    loop {
        // ✅ Main loop SOLO hace scheduling
        yield_cpu();
    }
}

fn idle_task() -> ! {
    loop {
        // Idle siempre cede inmediatamente
        yield_cpu();
    }
}

fn yield_cpu() {
    use process::context::switch_context;
    
    let switch_info = {
        let mut scheduler = process::scheduler::SCHEDULER.lock();
        scheduler.switch_to_next()
    };
    
    if let Some((old_ctx, new_ctx)) = switch_info {
        unsafe {
            switch_context(old_ctx, new_ctx);
        }
    }
}

fn shell_process() -> ! {
    // Crear REPL local (no global)
    let mut repl = Repl::new(10, 50);
    repl.show_prompt();
    
    loop {
        // Procesar teclado
        if let Some(character) = keyboard::read_key() {
            repl.handle_char(character);
        }
        
        // Ceder control periódicamente
        static mut SHELL_COUNTER: usize = 0;
        unsafe {
            SHELL_COUNTER += 1;
            if SHELL_COUNTER >= 1000 {
                SHELL_COUNTER = 0;
                yield_cpu();
            }
        }
    }
}