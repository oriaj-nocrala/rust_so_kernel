// kernel/src/init/processes.rs
//
// Process creation (idle, user, shell) and entry points.
// Code moved verbatim from kernel_main + helper functions.

use alloc::{boxed::Box, format};
use x86_64::VirtAddr;

use crate::{
    memory::{
        address_space::AddressSpace,
        vma::{Vma, VmaKind},
    },
    process::{
        self,
        Pid, Process,
        scheduler::SCHEDULER,
        user_test_fileio,
    },
    serial_println,
};

// ============================================================================
// PUBLIC API
// ============================================================================

/// Create all processes: idle, user×2, shell.
pub fn init_all() {
    serial_println!("\n🔧 Creating processes with isolated address spaces...");
    
    create_idle_process();
    create_user_processes(2);
    create_shell_process();
    
    serial_println!("✅ All processes created!\n");
}

/// Print open file descriptors for every process (debug).
pub fn debug_file_descriptors() {
    let scheduler = SCHEDULER.lock();
    for proc in scheduler.iter_all() {
        serial_println!("Process {}: open files:", proc.pid.0);
        proc.files.debug_list();
    }
}

// ============================================================================
// HELPERS
// ============================================================================

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

// ============================================================================
// PROCESS CREATORS
// ============================================================================

/// Idle process — uses kernel address space.
fn create_idle_process() {
    let kernel_stack = allocate_kernel_stack();
    let address_space = AddressSpace::kernel();
    
    let mut idle_proc = Box::new(Process::new_kernel(
        Pid(0),
        VirtAddr::new(idle_task as *const () as u64),
        kernel_stack,
        address_space,
    ));
    
    idle_proc.set_name("idle");
    idle_proc.set_priority(0);
    
    {
        let mut scheduler = SCHEDULER.lock();
        scheduler.add_process(idle_proc);
    }
    
    serial_println!("✅ Created idle process (PID 0)");
}

/// User processes — each gets its own AddressSpace with DEMAND-PAGED stack.
fn create_user_processes(num_processes: usize) {
    let test_name = "write";
    
    user_test_fileio::print_available_tests();
    serial_println!("\n📝 Using test: '{}'", test_name);
    
    for i in 0..num_processes {
        let kernel_stack = allocate_kernel_stack();
        
        // ============ 1. CREATE ADDRESS SPACE ============
        let mut address_space = unsafe {
            AddressSpace::new_user()
                .expect("Failed to create user address space")
        };
        
        serial_println!(
            "Created address space for process {}: PML4 at {:#x}",
            i,
            address_space.root_frame().start_address().as_u64()
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
                
                let frame = address_space.map_user_page(page, flags)
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
        
        // ============ 3. REGISTER VMAs (into the AddressSpace, not a global table) ============
        let code_start = 0x0000_0000_0040_0000_u64;
        let code_pages = 1usize;
        
        let user_stack_base = 0x0000_7100_0000_0000_u64 + (i as u64 * 0x10000);
        let stack_pages: usize = 16; // 64 KB virtual stack, demand-paged!
        
        let stack_flags = x86_64::structures::paging::PageTableFlags::PRESENT
                        | x86_64::structures::paging::PageTableFlags::WRITABLE
                        | x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE;
        
        // Register code VMA (for validation — already mapped eagerly)
        address_space.add_vma(Vma {
            start: code_start,
            size_pages: code_pages,
            flags: (x86_64::structures::paging::PageTableFlags::PRESENT
                  | x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE).bits(),
            kind: VmaKind::Code,
        }).expect("Failed to register code VMA");
        
        // ✅ Register stack VMA — NO physical pages allocated yet!
        // Pages will be allocated on-demand when the process touches the stack.
        address_space.add_vma(Vma {
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
        
        // ============ 4. ALLOCATE PID ============
        let pid = {
            let mut scheduler = SCHEDULER.lock();
            scheduler.allocate_pid()
        };
        
        // Debug: show all VMAs for this address space
        address_space.dump_vmas(pid.0);
        
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
                address_space,
            ));
            
            user_proc.set_name(&format!("user_{}", i));
            user_proc.set_priority(5);
            
            let mut scheduler = SCHEDULER.lock();
            scheduler.add_process(user_proc);
        }
        
        serial_println!("✅ Created user process {} (PID {})", i, pid.0);
    }
}

/// Shell process — kernel, uses kernel address space.
fn create_shell_process() {
    let kernel_stack = allocate_kernel_stack();
    let address_space = AddressSpace::kernel();
    
    let pid = {
        let mut scheduler = SCHEDULER.lock();
        let pid = scheduler.allocate_pid();
        
        let mut shell = Box::new(Process::new_kernel(
            pid,
            VirtAddr::new(shell_process as *const () as u64),
            kernel_stack,
            address_space,
        ));
        
        shell.set_name("shell");
        shell.set_priority(8);
        
        scheduler.add_process(shell);
        pid
    };
    
    serial_println!("✅ Created shell process (PID {})", pid.0);
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