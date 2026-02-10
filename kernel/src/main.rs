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
use crate::{allocator::FRAME_ALLOCATOR, process::{ProcessState, scheduler, user_test_minimal}};

use process::user_test_fileio;

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
        idt.entries[32].set_handler_addr(process::timer_preempt::timer_interrupt_entry as u64);
        idt.add_handler(33, keyboard_interrupt_handler);
        // ✅ FIX: INT 0x80 necesita DPL=3 para que Ring 3 pueda llamarla
        idt.entries[0x80]
            .set_handler_addr(syscall_entry as u64)
            .set_privilege_level(3);  // ← AGREGAR ESTA LÍNEA
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
    // ❌ NO hacer yield_cpu aquí
    
    // Solo EOI
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
            fb.draw_text(10, 10, "ConstanOS v0.1", Color::rgb(0, 200, 255), Color::rgb(0, 0, 0), 2);
            fb.draw_text(10, 770, "Allocator: Ready", Color::rgb(0, 255, 0), Color::rgb(0, 0, 0), 2);
        }
    }

    // Inicializar interrupciones
    interrupts::pic::initialize();
    interrupts::pic::enable_irq(0); // ⏰ TIMER
    interrupts::pic::enable_irq(1); // ⌨ KEYBOARD
    load_idt();

    // Inicializar el PIT
    pit::init(100); // 100 Hz

    let mut repl = Repl::new(10, 50);
    repl.show_prompt();

    serial_println!("Step 9: Initializing TSS and GDT");
    process::tss::init();

    serial_println!("Step 7.5: Setting up user space memory");

    // ✅ Declarar code_entry AQUÍ (fuera del unsafe block)
    let code_entry: VirtAddr;

    unsafe {
        let phys_offset = memory::physical_memory_offset();
        let mut page_table = memory::paging::ActivePageTable::new(phys_offset);
        let mut frame_allocator_lock = FRAME_ALLOCATOR.lock();
        
        let frame_allocator = frame_allocator_lock.as_mut()
            .expect("Frame allocator not initialized");
        
        // ============ 1. Mapear USER CODE (compartido) ============
        use memory::user_code;
        use process::user_test_minimal;

        user_test_minimal::print_available_tests();
        
        let test_name = "write";
        let test_ptr = user_test_fileio::get_test_ptr(test_name);
        
        serial_println!("\n📝 Using test: '{}'", test_name);
        serial_println!("   Test address: {:#x}", test_ptr as u64);
        
        // ✅ Asignar a la variable declarada arriba
        code_entry = memory::user_code::setup_user_code(
            &mut page_table.mapper,
            frame_allocator,
            test_ptr,
            4096,
        ).expect("Failed to setup user code");
        
        serial_println!("  ✅ User code copied to: {:#x}\n", code_entry.as_u64());
        
        // ============ 2. Mapear MÚLTIPLES USER STACKS (separados) ============
        // ⚠️ IMPORTANTE: Cada proceso necesita su PROPIO stack
        
        const NUM_USER_PROCESSES: usize = 2;
        
        for i in 0..NUM_USER_PROCESSES {
            // Calcular base del stack para este proceso
            // Separar stacks por 64KB para evitar overlap
            let stack_base = 0x0000_7100_0000_0000_u64 + (i as u64 * 0x10000);
            let stack_size = 8192;  // 2 páginas (8KB)
            
            serial_println!("Mapping user stack {} at {:#x}", i, stack_base);
            
            memory::user_pages::map_user_pages(
                &mut page_table.mapper,
                frame_allocator,
                VirtAddr::new(stack_base),
                (stack_size / 4096) as usize,
            ).expect(&format!("Failed to map user stack {}", i));
            
            serial_println!(
                "  ✅ Stack {}: {:#x} - {:#x}",
                i,
                stack_base,
                stack_base + stack_size
            );
        }
    }

    // ============ CREAR PROCESOS (SIMPLIFICADO) ============
    
    serial_println!("\nStep 10: Creating processes");
    
    // ✅ Toda la lógica repetida ahora está en 3 funciones
    init_processes(code_entry);

    // Debug de file descriptors
    {
        let scheduler = SCHEDULER.lock();
        for proc in scheduler.processes.iter() {
            serial_println!("Process {}: open files:", proc.pid.0);
            proc.files.debug_list();
        }
    }

    serial_println!("DEBUG: About to start first process");

    // Arrancar primer proceso
    process::start_first_process();
}

/// Allocar un kernel stack para un proceso
fn allocate_kernel_stack() -> VirtAddr {
    let mut frame_alloc = FRAME_ALLOCATOR.lock();
    let frame_alloc = frame_alloc.as_mut()
        .expect("Frame allocator not initialized");
    
    let stack_frame = frame_alloc.allocate_frame()
        .expect("Failed to allocate kernel stack");
    
    let phys_addr = stack_frame.start_address();
    let virt_addr = memory::physical_memory_offset() + phys_addr.as_u64();
    
    // Retornar el tope del stack
    VirtAddr::new(virt_addr.as_u64() + 4096)
}

/// Obtener el page table frame actual (CR3)
fn get_current_page_table() -> x86_64::structures::paging::PhysFrame {
    unsafe {
        let (frame, _) = x86_64::registers::control::Cr3::read();
        frame
    }
}

/// Crear el proceso idle (PID 0)
fn create_idle_process() {
    let page_table = get_current_page_table();
    let kernel_stack = allocate_kernel_stack();
    
    let idle = Box::new(Process::new_kernel(
        Pid(0),
        VirtAddr::new(idle_task as *const () as u64),
        kernel_stack,
        page_table,
    ));
    
    let mut scheduler = SCHEDULER.lock();
    let mut idle_proc = idle;
    idle_proc.set_name("idle");
    idle_proc.set_priority(0);  // Lowest priority
    scheduler.add_process(idle_proc);
    
    serial_println!("✅ Created idle process (PID 0)");
}

/// Crear procesos de usuario
fn create_user_processes(code_entry: VirtAddr, num_processes: usize) {
    let page_table = get_current_page_table();
    
    for i in 0..num_processes {
        let kernel_stack = allocate_kernel_stack();
        
        // Calcular user stack (cada proceso tiene el suyo)
        let user_stack_base = 0x0000_7100_0000_0000_u64 + (i as u64 * 0x10000);
        let user_stack_size = 8192;
        let user_stack_top = VirtAddr::new(user_stack_base + user_stack_size - 8);
        
        // Crear proceso
        let mut user_proc = Box::new(Process::new_user(
            scheduler::SCHEDULER.lock().allocate_pid(),
            code_entry,
            user_stack_top,
            kernel_stack,
            page_table,
        ));
        
        let mut scheduler = SCHEDULER.lock();
        user_proc.set_name(&format!("user_{}", i));
        user_proc.set_priority(5);  // Normal priority
        let pid = user_proc.pid;
        scheduler.add_process(user_proc);
        
        serial_println!("✅ Created user process {} (PID {})", i, pid.0);
    }
}

/// Crear el proceso shell (kernel)
fn create_shell_process() {
    let page_table = get_current_page_table();
    let kernel_stack = allocate_kernel_stack();
    
    let mut shell = Box::new(Process::new_kernel(
        scheduler::SCHEDULER.lock().allocate_pid(),
        VirtAddr::new(shell_process as *const () as u64),
        kernel_stack,
        page_table,
    ));
    
    let mut scheduler = SCHEDULER.lock();
    shell.set_name("shell");
    shell.set_priority(8);  // High priority
    let pid = shell.pid;
    scheduler.add_process(shell);
    
    serial_println!("✅ Created shell process (PID {})", pid.0);
}

/// Inicializar todos los procesos
fn init_processes(code_entry: VirtAddr) {
    serial_println!("\n🔧 Creating processes...");
    
    create_idle_process();
    create_user_processes(code_entry, 2);  // 2 user processes
    create_shell_process();
    
    serial_println!("✅ All processes created!\n");
}

// ============================================================================
// PROCESS ENTRY POINTS
// ============================================================================

fn idle_task() -> ! {
    loop {
        unsafe { core::arch::asm!("hlt"); }
    }
}

fn shell_process() -> ! {
    let mut repl = Repl::new(10, 50);
    repl.show_prompt();
    
    loop {
        if let Some(character) = keyboard::read_key() {
            repl.handle_char(character);
        }
        unsafe { core::arch::asm!("pause"); }
    }
}