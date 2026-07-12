// kernel/src/init/processes.rs
//
// Process creation (idle, user, shell) and entry points.
//
// HISTORY:
//   - Now uses ELF loader (memory/elf_loader.rs) for Elf program sources.
//   - Falls back to legacy manual copy for RawCode sources (inline asm tests).
//   - Process creation is unified: both paths produce an AddressSpace + entry
//     point + stack, which feed into Process::new_user().

use alloc::boxed::Box;
use x86_64::VirtAddr;

use crate::{
    memory::{
        address_space::AddressSpace,
        vma::{Vma, VmaKind},
    },
    process::{
        self,
        Pid, Process,
        user_programs::ProgramSource,
    },
    serial_println,
};

// ============================================================================
// PUBLIC API
// ============================================================================

/// Create all processes: idle, user programs.
pub fn init_all() {
    serial_println!("\n🔧 Creating processes with isolated address spaces...");

    create_idle_process();
    create_user_processes();

    serial_println!("✅ All processes created!\n");
}

/// Print open file descriptors for every process (debug).
pub fn debug_file_descriptors() {
    let scheduler = crate::process::scheduler::local_scheduler();
    for proc in scheduler.iter_all() {
        serial_println!("Process {}: open files:", proc.pid.0);
        proc.files.lock().debug_list();
    }
}

// ============================================================================
// HELPERS
// ============================================================================

/// Kernel stack size order. 16 = 64 KiB.
///
/// Was order 14 (16 KiB) — too small for debug builds: `sys_exec`'s call
/// chain (syscall_handler_asm -> syscall_handler -> sys_exec -> load_elf ->
/// load_segment) has large unoptimized stack frames, and kernel stacks are
/// plain physical-offset-mapped regions with NO guard page, so an overflow
/// doesn't fault — it silently corrupts whatever physical memory sits just
/// below, which later crashes as an unrelated-looking kernel page fault
/// once a clobbered return address gets used.
pub const KERNEL_STACK_ORDER: usize = 16;

/// Allocate a kernel stack from the Buddy.
pub fn allocate_kernel_stack() -> VirtAddr {
    let phys_addr = unsafe {
        crate::allocator::phys_alloc(KERNEL_STACK_ORDER)
            .expect("Failed to allocate kernel stack from buddy")
    };

    let virt_addr = crate::memory::physical_memory_offset() + phys_addr.as_u64();

    // Stack top (grows downward)
    VirtAddr::new(virt_addr.as_u64() + (1 << KERNEL_STACK_ORDER))
}

/// `stack_top` (what `allocate_kernel_stack` returned) back to the
/// physical base Buddy actually allocated.
fn kernel_stack_phys_base(stack_top: VirtAddr) -> x86_64::PhysAddr {
    let virt_base = stack_top - (1u64 << KERNEL_STACK_ORDER);
    x86_64::PhysAddr::new(virt_base.as_u64() - crate::memory::physical_memory_offset().as_u64())
}

/// Return a kernel stack (as returned by `allocate_kernel_stack`) to the Buddy.
///
/// Callers must make sure the CPU isn't still executing on this stack —
/// see `Scheduler::pending_stack_frees` for the one place that matters.
pub fn free_kernel_stack(stack_top: VirtAddr) {
    unsafe {
        crate::allocator::phys_free(kernel_stack_phys_base(stack_top), KERNEL_STACK_ORDER);
    }
}

/// Like `free_kernel_stack`, but never blocks — returns `false` instead of
/// waiting if the Buddy lock is currently held elsewhere.
///
/// Needed from timer-interrupt context (`Scheduler::tick`'s
/// `pending_stack_frees` drain): that ISR can interrupt *any* kernel code,
/// including a heap allocation that's mid-way through a slab→Buddy refill
/// with the Buddy lock already held and interrupts still enabled (nothing
/// before this ever called `BUDDY.lock()` from an ISR, so ordinary heap
/// allocations were never written to guard against that reentrancy). A
/// blocking `.lock()` there spins forever: the interrupted code can't run
/// again to release the lock until this same ISR returns, which it never
/// does. Confirmed live — the very first version of this code (calling
/// `free_kernel_stack` unconditionally from `tick()`) froze the kernel
/// solid (idle task never reached its `hlt`, vCPU pegged at ~25% CPU)
/// within a second or two of boot.
pub fn try_free_kernel_stack(stack_top: VirtAddr) -> bool {
    match crate::allocator::buddy_allocator::BUDDY.try_lock() {
        Some(mut buddy) => {
            unsafe { buddy.deallocate(kernel_stack_phys_base(stack_top), KERNEL_STACK_ORDER); }
            true
        }
        None => false,
    }
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
        let mut scheduler = crate::process::scheduler::local_scheduler();
        scheduler.add_process(idle_proc);
    }

    serial_println!("✅ Created idle process (PID 0)");
}

/// Create user processes from the embedded program registry.
///
/// For each program in user_programs::list_programs(), spawns one
/// process using either the ELF loader or the legacy raw-code path.
fn create_user_processes() {
    let programs = process::user_programs::list_programs();
    process::user_programs::print_available();

    for (i, (name, source)) in programs.iter().enumerate() {
        // Only auto-start the userspace shell; other programs are exec'd on demand.
        if *name != "shell" { continue; }

        serial_println!("\n📝 Loading program '{}' (index {})", name, i);

        let result = match source {
            ProgramSource::Elf(elf_bytes) => load_elf_process(elf_bytes, i),
            ProgramSource::RawCode { code_ptr, code_size } => {
                load_raw_process(code_ptr(), *code_size, i)
            }
        };

        let (address_space, entry_point, user_stack_top) = match result {
            Ok(v) => v,
            Err(e) => {
                serial_println!("❌ Failed to load '{}': {}", name, e);
                continue;
            }
        };

        // ── Allocate PID and create process ───────────────────────────

        let kernel_stack = allocate_kernel_stack();

        let pid = {
            let mut scheduler = crate::process::scheduler::local_scheduler();
            scheduler.allocate_pid()
        };

        // Debug: show all VMAs
        address_space.dump_vmas(pid.0);

        {
            let mut user_proc = Box::new(Process::new_user(
                pid,
                entry_point,
                user_stack_top,
                kernel_stack,
                address_space,
            ));

            user_proc.set_name(name);
            user_proc.set_priority(5);

            let mut scheduler = crate::process::scheduler::local_scheduler();
            scheduler.add_process(user_proc);
        }

        serial_println!("✅ Created user process '{}' (PID {})", name, pid.0);
    }
}

// ============================================================================
// LOADING STRATEGIES
// ============================================================================

/// Load a program from ELF bytes using the ELF loader.
///
/// Returns (address_space, entry_point, user_stack_top).
fn load_elf_process(
    elf_bytes: &[u8],
    process_index: usize,
) -> Result<(AddressSpace, VirtAddr, VirtAddr), &'static str> {
    let loaded = unsafe {
        crate::memory::elf_loader::load_elf(elf_bytes, process_index)?
    };

    serial_println!(
        "  ELF loaded: entry={:#x} stack_top={:#x}",
        loaded.entry_point.as_u64(),
        loaded.user_stack_top.as_u64(),
    );

    Ok((loaded.address_space, loaded.entry_point, loaded.user_stack_top))
}

/// Legacy loader: manually map raw code bytes (inline assembly tests).
///
/// This replicates the old create_user_processes logic for backward
/// compatibility until all programs are ELF binaries.
fn load_raw_process(
    code_ptr: *const u8,
    code_size: usize,
    process_index: usize,
) -> Result<(AddressSpace, VirtAddr, VirtAddr), &'static str> {
    // ── 1. Create address space ───────────────────────────────────────

    let mut address_space = unsafe {
        AddressSpace::new_user()
            .map_err(|_| "Failed to create user address space")?
    };

    serial_println!(
        "  Legacy: address space PML4 at {:#x}",
        address_space.root_frame().start_address().as_u64(),
    );

    // ── 2. Map user code eagerly ──────────────────────────────────────

    let code_start = 0x0000_0000_0040_0000_u64;
    let num_code_pages = (code_size + 4095) / 4096;

    let flags = x86_64::structures::paging::PageTableFlags::PRESENT
              | x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE;

    unsafe {
        let phys_offset = crate::memory::physical_memory_offset();

        for page_idx in 0..num_code_pages {
            let page_addr = VirtAddr::new(code_start + (page_idx as u64 * 4096));
            let page = x86_64::structures::paging::Page::containing_address(page_addr);

            let frame = address_space.map_user_page(page, flags)
                .map_err(|_| "Failed to map code page")?;

            let src = code_ptr.add(page_idx * 4096);
            let copy_size = code_size.saturating_sub(page_idx * 4096).min(4096);

            let dst = (phys_offset + frame.start_address().as_u64()).as_mut_ptr::<u8>();
            core::ptr::copy_nonoverlapping(src, dst, copy_size);

            if copy_size < 4096 {
                core::ptr::write_bytes(dst.add(copy_size), 0, 4096 - copy_size);
            }
        }
    }

    // ── 3. Register VMAs ──────────────────────────────────────────────

    address_space.add_vma(Vma {
        start: code_start,
        size_pages: num_code_pages,
        flags: flags.bits(),
        kind: VmaKind::Code,
    }).map_err(|_| "Failed to register code VMA")?;

    // Stack VMA (demand-paged)
    let user_stack_base = 0x0000_7100_0000_0000_u64 + (process_index as u64 * 0x10000);
    let stack_pages: usize = 16;

    let stack_flags = x86_64::structures::paging::PageTableFlags::PRESENT
                    | x86_64::structures::paging::PageTableFlags::WRITABLE
                    | x86_64::structures::paging::PageTableFlags::USER_ACCESSIBLE;

    address_space.add_vma(Vma {
        start: user_stack_base,
        size_pages: stack_pages,
        flags: stack_flags.bits(),
        kind: VmaKind::Anonymous,
    }).map_err(|_| "Failed to register stack VMA")?;

    let user_stack_top = VirtAddr::new(
        user_stack_base + (stack_pages as u64 * 4096) - 8
    );

    serial_println!(
        "  Legacy: code={:#x} stack_top={:#x}",
        code_start,
        user_stack_top.as_u64(),
    );

    Ok((
        address_space,
        VirtAddr::new(code_start),
        user_stack_top,
    ))
}

// ============================================================================
// PROCESS ENTRY POINTS
// ============================================================================

fn idle_task() -> ! {
    loop {
        unsafe { core::arch::asm!("hlt"); }
    }
}