#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;

mod allocator;
mod framebuffer;
mod interrupts;
mod keyboard;
mod memory;
mod panic;
mod pit;
mod repl;
mod serial;

use alloc::{format, vec::Vec};
// use alloc::{string::ToString, vec::Vec};
use bootloader_api::{BootInfo, BootloaderConfig, config::Mapping, entry_point, info::{MemoryRegion, MemoryRegionKind}};
use framebuffer::Framebuffer;
use interrupts::idt::InterruptDescriptorTable;
use spin::Once;
use x86_64::{VirtAddr, structures::paging::FrameAllocator};

use crate::{
    allocator::buddy_allocator::Buddy, framebuffer::{Color, init_global_framebuffer}, interrupts::exception::ExceptionStackFrame, memory::{frame_allocator::{self, BootInfoFrameAllocator}, paging::ActivePageTable}, repl::Repl
};

static IDT: Once<InterruptDescriptorTable> = Once::new();

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
        idt
    });
}

fn load_idt() {
    IDT.get().unwrap().load();
}

extern "x86-interrupt" fn keyboard_interrupt_handler(_: &mut ExceptionStackFrame) {
    // Log serial para debug (no usa framebuffer, no puede causar problemas)
    debug_log("Keyboard IRQ\n");
    
    // Leer scancode
    let scancode = unsafe {
        debug_log("Reading port...\n");
        x86_64::instructions::port::PortReadOnly::<u8>::new(0x60).read()
    };
    
    debug_log("Processing...\n");
    keyboard::process_scancode(scancode);
    
    debug_log("Sending EOI...\n");
    interrupts::pic::end_of_interrupt(interrupts::pic::Irq::Keyboard.as_u8());
    
    debug_log("Done\n");
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

    // --- 3. Inicialización del Heap ---
    // POR AHORA: Sigue usando tu heap estático (array de 100KB)
    // Esto te permite seguir usando Box, Vec, etc. mientras aprendes paging.
    allocator::bump::init_heap();

    // ⭐ Expandir heap dinámicamente (ej: +1MB)
    // match allocator::expand_heap(&mut page_table, &mut frame_allocator, 256) {
    //     Ok(_) => {
    //         // Actualizar el heap_end del bump allocator
    //         let new_end = allocator::bump::heap_end() + (256 * 4096);
    //         allocator::bump::expand_heap_size(new_end);
    //     }
    //     Err(e) => {
    //         serial_println!("Failed to expand heap: {}", e);
    //     }
    // }
    

    // panic!("Testing panic handler!");

    let mut total: usize = 0;
    let mut total_mem: u64 = 0;
    let mut reg_st: u64 = 0;
    let mut reg_end: u64 = 0;
    let mut i = 0;
    let mut total_regions = 0;

    let mut buddy = Buddy::new();
    while total < boot_info.memory_regions.len() {
        let region = &boot_info.memory_regions[total];
        
        if region.kind == MemoryRegionKind::Usable {
            total_mem += region.end - region.start;
            total_regions += 1;
            if i == 0 {
                // PRIMERA MEMORY REGION.
                unsafe{
                    buddy.add_region(region.start, region.end);

                    let test_frame = buddy.allocate(18).unwrap();
                    serial_println!("Allocated: {:#x}", test_frame.as_u64());

                    let virt = phys_mem_offset + test_frame.as_u64();
                    let ptr = virt.as_mut_ptr::<u64>();
                    unsafe {
                        *ptr = 0xDEAD;
                        assert_eq!(*ptr, 0xDEAD);
                    }
                    serial_println!("Write test: OK");
                }
            }
        }
        i += 1;
        
        total += 1;
    }

    let text = format!("Regiones totales: {} Regiones usables: {} Memoria total: {}", total, total_regions, total_mem);
    
     // Limpiar pantalla
    {
        let mut fb = framebuffer::FRAMEBUFFER.lock();
        if let Some(fb) = fb.as_mut() {
            fb.clear(Color::rgb(0, 0, 0));
            fb.draw_text(10, 10, "NeoOS v0.1", Color::rgb(0, 200, 255), Color::rgb(0, 0, 0), 2);
            fb.draw_text(10,770, text.as_str(), Color::rgb(0, 200, 255), Color::rgb(0, 0, 0), 2);
            // fb.draw_text(10,420, example_region.as_str(), Color::rgb(0, 200, 255), Color::rgb(0, 0, 0), 2);
        }
    }

    // Inicializar interrupciones
    interrupts::pic::initialize();
    interrupts::pic::enable_irq(1); // Habilitar IRQ1 (teclado)
    load_idt();
    
    unsafe {
        core::arch::asm!("sti");
    }

    // Inicializar el PIT
    pit::init(100); // 100 Hz

    let mut repl = Repl::new(10, 50);
    repl.show_prompt();

    loop {
        if let Some(character) = keyboard::read_from_buffer() {
            repl.handle_char(character);
        } else {
            unsafe { core::arch::asm!("hlt"); }
        }
    }
}

