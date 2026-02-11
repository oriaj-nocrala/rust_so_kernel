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
use bootloader_api::{BootInfo, BootloaderConfig, config::Mapping, entry_point, info::{MemoryRegion, MemoryRegionKind}};
use framebuffer::Framebuffer;
use interrupts::idt::InterruptDescriptorTable;
use spin::Once;
use x86_64::{VirtAddr, structures::paging::FrameAllocator};
use process::{Process, Pid, scheduler::SCHEDULER};
use crate::{
    allocator::FRAME_ALLOCATOR,
    memory::page_table_manager::OwnedPageTable,
    process::{ProcessState, scheduler, user_test_minimal},
};

use process::user_test_fileio;

use crate::{
    framebuffer::{Color, init_global_framebuffer},
    interrupts::exception::ExceptionStackFrame,
    memory::{
        frame_allocator::BootInfoFrameAllocator,
        paging::ActivePageTable,
    },
    repl::Repl,
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
        idt.entries[0x80]
            .set_handler_addr(syscall_entry as u64)
            .set_privilege_level(3);
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

// ✅ Page fault handler — tries demand paging before panicking
extern "x86-interrupt" fn page_fault_handler(
    sf: &mut ExceptionStackFrame,
    error_code: u64
) {
    use crate::memory::demand_paging;

    // Try demand paging first.
    // If the fault is in a valid VMA (e.g. lazy stack), a page will be
    // allocated, mapped, and zeroed.  The CPU retries the instruction on iret.
    match demand_paging::handle_page_fault(error_code) {
        Ok(()) => {
            // Page was mapped successfully — resume execution.
            return;
        }
        Err(reason) => {
            // Not a demand-pageable fault → unrecoverable
            let fault_address: u64;
            unsafe {
                core::arch::asm!("mov {}, cr2", out(reg) fault_address);
            }

            panic!(
                "PAGE FAULT (unhandled)\n  Address: {:#x}\n  Error code: {:#b}\n  Reason: {}\n  RIP: {:#x}",
                fault_address,
                error_code,
                reason,
                sf.instruction_pointer
            );
        }
    }
}

extern "x86-interrupt" fn timer_handler(_sf: &mut ExceptionStackFrame) {
    unsafe {
        use x86_64::instructions::port::PortWriteOnly;
        PortWriteOnly::<u8>::new(0x20).write(0x20);
    }
}

pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
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

    let phys_mem_offset = VirtAddr::new(
        boot_info.physical_memory_offset.into_option().unwrap()
    );

    // ✅ Print the physical memory offset so we can verify PML4 entry
    serial_println!("Physical memory offset: {:#x} (PML4 entry {})",
        phys_mem_offset.as_u64(),
        phys_mem_offset.as_u64() >> 39
    );

    memory::init(phys_mem_offset);
    
    // --- Inicialización de Memoria ---
    let frame_allocator = unsafe {
        BootInfoFrameAllocator::init(&boot_info.memory_regions)
    };
    
    let page_table = unsafe {
        ActivePageTable::new(phys_mem_offset)
    };
    
    allocator::init_allocators(page_table, frame_allocator);

    // --- Inicializar Buddy Allocator ---
    {
        let mut buddy = allocator::buddy_allocator::BUDDY.lock();
        
        for region in boot_info.memory_regions.iter() {
            if region.kind == MemoryRegionKind::Usable {
                unsafe {
                    buddy.add_region(region.start, region.end);
                }
            }
        }
    }

    serial_println!("Step 8: Printing Buddy stats (lock released)");
    {
        let buddy = allocator::buddy_allocator::BUDDY.lock();
        buddy.debug_print_stats();
    }

    // --- Test Slab ---
    {
        use core::alloc::{GlobalAlloc, Layout};

        let layout = Layout::from_size_align(8, 8).unwrap();
        let ptr = unsafe { alloc::alloc::alloc(layout) };

        if ptr.is_null() {
            serial_println!("  FAILED: Got null pointer");
            panic!("Slab allocation failed");
        } else {
            serial_println!("  SUCCESS: Got pointer {:#x}", ptr as u64);
            unsafe {
                *(ptr as *mut u64) = 0xDEADBEEF;
                let val = *(ptr as *const u64);
                serial_println!("  Write/read test: {:#x}", val);
                assert_eq!(val, 0xDEADBEEF);
                alloc::alloc::dealloc(ptr, layout);
            }
            serial_println!("  SUCCESS: Deallocation complete");
        }
    }

    {
        use alloc::vec::Vec;
        serial_println!("  Creating Vec...");
        let mut v: Vec<u8> = Vec::new();
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
    interrupts::pic::enable_irq(0);
    interrupts::pic::enable_irq(1);
    load_idt();

    pit::init(100);

    let mut repl = Repl::new(10, 50);
    repl.show_prompt();

    serial_println!("Step 9: Initializing TSS and GDT");
    process::tss::init();

    serial_println!("\nStep 10: Creating processes");
    
    init_processes();

    // Debug de file descriptors
    {
        let scheduler = SCHEDULER.lock();
        for proc in scheduler.processes.iter() {
            serial_println!("Process {}: open files:", proc.pid.0);
            proc.files.debug_list();
        }
    }

    serial_println!("DEBUG: About to start first process");

    process::start_first_process();
}

/// Allocar un kernel stack desde el Buddy (4 KiB).
fn allocate_kernel_stack() -> VirtAddr {
    let phys_addr = unsafe {
        crate::allocator::buddy_allocator::BUDDY.lock()
            .allocate(14)
            .expect("Failed to allocate kernel stack from buddy")
    };
    
    let virt_addr = crate::memory::physical_memory_offset() + phys_addr.as_u64();
    
    // Stack top (grows downward)
    VirtAddr::new(virt_addr.as_u64() + 4096)
}

/// Idle process — uses kernel page table (from_current).
fn create_idle_process() {
    let kernel_stack = allocate_kernel_stack();
    let page_table = OwnedPageTable::from_current();
    
    let mut idle_proc = Box::new(Process::new_kernel(
        Pid(0),
        VirtAddr::new(idle_task as *const () as u64),
        kernel_stack,
        page_table,
    ));
    
    idle_proc.set_name("idle");
    idle_proc.set_priority(0);
    
    {
        let mut scheduler = SCHEDULER.lock();
        scheduler.add_process(idle_proc);
    }
    
    serial_println!("✅ Created idle process (PID 0)");
}

/// User processes — each gets its own page table with DEMAND-PAGED stack.
fn create_user_processes(num_processes: usize) {
    use crate::memory::vma::{self, Vma, VmaKind};

    let test_name = "write";
    
    user_test_fileio::print_available_tests();
    serial_println!("\n📝 Using test: '{}'", test_name);
    
    for i in 0..num_processes {
        let kernel_stack = allocate_kernel_stack();
        
        // ============ 1. CREATE PAGE TABLE (copies kernel entries, skips user PML4s) ============
        let page_table = unsafe {
            OwnedPageTable::new_user()
                .expect("Failed to create user page table")
        };
        
        serial_println!(
            "Created page table for process {}: PML4 at {:#x}",
            i,
            page_table.root_frame().start_address().as_u64()
        );
        
        // ============ 2. MAP USER CODE (eagerly — instructions must be present) ============
        unsafe {
            let code_start = 0x0000_0000_0040_0000_u64;
            let code_size = 4096usize;
            let num_code_pages = (code_size + 4095) / 4096;
            
            let flags = x86_64::structures::paging::PageTableFlags::PRESENT
                      | x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE;
            
            serial_println!("  Mapping {} pages of user code at {:#x}", 
                num_code_pages, code_start);
            
            let code_ptr = user_test_fileio::get_test_ptr(test_name);
            
            for page_idx in 0..num_code_pages {
                let page_addr = VirtAddr::new(code_start + (page_idx as u64 * 4096));
                let page = x86_64::structures::paging::Page::containing_address(page_addr);
                
                let frame = page_table.map_user_page(page, flags)
                    .expect("Failed to map code page");
                
                let src = code_ptr.add(page_idx * 4096);
                let copy_size = code_size.saturating_sub(page_idx * 4096).min(4096);
                
                let phys_offset = crate::memory::physical_memory_offset();
                let dst = (phys_offset + frame.start_address().as_u64()).as_mut_ptr::<u8>();
                
                core::ptr::copy_nonoverlapping(src, dst, copy_size);
                
                if copy_size < 4096 {
                    core::ptr::write_bytes(dst.add(copy_size), 0, 4096 - copy_size);
                }
                
                serial_println!("    Page {}: {:#x} -> phys {:#x}", 
                    page_idx, page_addr.as_u64(), frame.start_address().as_u64());
            }
        }
        
        // ============ 3. ALLOCATE PID (need it for VMA registration) ============
        let pid = {
            let mut scheduler = SCHEDULER.lock();
            scheduler.allocate_pid()
        };
        
        // ============ 4. REGISTER VMAs ============
        let code_start = 0x0000_0000_0040_0000_u64;
        let code_pages = 1usize;
        
        let user_stack_base = 0x0000_7100_0000_0000_u64 + (i as u64 * 0x10000);
        let stack_pages: usize = 16; // 64 KB virtual stack, demand-paged!
        
        let stack_flags = x86_64::structures::paging::PageTableFlags::PRESENT
                        | x86_64::structures::paging::PageTableFlags::WRITABLE
                        | x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE;
        
        // Register code VMA (for validation — already mapped eagerly)
        vma::register_vma(pid.0, Vma {
            start: code_start,
            size_pages: code_pages,
            flags: (x86_64::structures::paging::PageTableFlags::PRESENT
                  | x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE).bits(),
            kind: VmaKind::Code,
        }).expect("Failed to register code VMA");
        
        // ✅ Register stack VMA — NO physical pages allocated yet!
        // Pages will be allocated on-demand when the process touches the stack.
        vma::register_vma(pid.0, Vma {
            start: user_stack_base,
            size_pages: stack_pages,
            flags: stack_flags.bits(),
            kind: VmaKind::Anonymous,
        }).expect("Failed to register stack VMA");
        
        serial_println!(
            "  Stack VMA: {:#x}..{:#x} ({} pages, demand-paged)",
            user_stack_base,
            user_stack_base + (stack_pages as u64 * 4096),
            stack_pages,
        );
        
        // Debug: show all VMAs for this process
        vma::dump_vmas(pid.0);
        
        // ============ 5. CREATE PROCESS ============
        {
            // RSP points to the TOP of the VMA (grows downward)
            let user_stack_top = VirtAddr::new(
                user_stack_base + (stack_pages as u64 * 4096) - 8
            );
            
            let mut user_proc = Box::new(Process::new_user(
                pid,
                VirtAddr::new(0x0000_0000_0040_0000),
                user_stack_top,
                kernel_stack,
                page_table,
            ));
            
            user_proc.set_name(&format!("user_{}", i));
            user_proc.set_priority(5);
            
            let mut scheduler = SCHEDULER.lock();
            scheduler.add_process(user_proc);
        }
        
        serial_println!("✅ Created user process {} (PID {})", i, pid.0);
    }
}

/// Shell process — kernel, uses kernel page table.
fn create_shell_process() {
    let kernel_stack = allocate_kernel_stack();
    let page_table = OwnedPageTable::from_current();
    
    let pid = {
        let mut scheduler = SCHEDULER.lock();
        let pid = scheduler.allocate_pid();
        
        let mut shell = Box::new(Process::new_kernel(
            pid,
            VirtAddr::new(shell_process as *const () as u64),
            kernel_stack,
            page_table,
        ));
        
        shell.set_name("shell");
        shell.set_priority(8);
        
        scheduler.add_process(shell);
        pid
    };
    
    serial_println!("✅ Created shell process (PID {})", pid.0);
}

fn init_processes() {
    serial_println!("\n🔧 Creating processes with isolated page tables...");
    
    create_idle_process();
    create_user_processes(2);
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
    let mut repl = crate::repl::Repl::new(10, 50);
    repl.show_prompt();
    
    loop {
        if let Some(character) = crate::keyboard::read_key() {
            repl.handle_char(character);
        }
        unsafe { core::arch::asm!("pause"); }
    }
}