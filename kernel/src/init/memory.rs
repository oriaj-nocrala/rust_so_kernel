// kernel/src/init/memory.rs
//
// Physical memory offset → buddy → slab.
//
// CORRECTED: Removed BootInfoFrameAllocator + ActivePageTable.
// Previously both the BootInfoFrameAllocator AND the Buddy were initialized
// over the same usable memory regions.  Both could hand out the same frame.
// Now the Buddy is the SOLE physical memory allocator after init.

use bootloader_api::info::{MemoryRegions, MemoryRegionKind};
use x86_64::VirtAddr;

use crate::{
    allocator,
    memory,
    serial_println,
};

/// Initialize all memory subsystems in order:
/// phys offset → buddy → slab (slab uses buddy internally).
pub fn init_core(phys_mem_offset: VirtAddr, memory_regions: &'static MemoryRegions) {
    serial_println!("Physical memory offset: {:#x} (PML4 entry {})",
        phys_mem_offset.as_u64(),
        phys_mem_offset.as_u64() >> 39
    );

    memory::init(phys_mem_offset);

    // The AP trampoline's pages (stage 4 of docs/smp/smp-plan.md) must sit
    // below 640 KiB and must never be handed out: an AP runs from them while
    // the rest of the kernel is already allocating. Carved out here, before
    // the Buddy allocator ever sees them.
    let usable = || memory_regions.iter()
        .filter(|r| r.kind == MemoryRegionKind::Usable)
        .map(|r| (r.start, r.end));
    let trampoline = hal::smp::pick_trampoline(usable());
    let reserved = trampoline.map(|b| (b, b + hal::smp::TRAMPOLINE_PAGES * hal::smp::PAGE));
    match trampoline {
        Some(base) => {
            crate::smp::set_trampoline(base);
            serial_println!("smp: AP trampoline reserved at {:#x}", base);
        }
        None => serial_println!("smp: no usable window below 640 KiB for the AP trampoline"),
    }

    // Initialize Buddy allocator — sole owner of all usable physical memory.
    let mut max_usable_end: u64 = 0;
    allocator::BUDDY.with(|buddy| {
        for (start, end) in usable() {
            if end > max_usable_end {
                max_usable_end = end;
            }
            let add = |buddy: &mut mm::buddy::BuddyAllocator, s: u64, e: u64| {
                if e > s {
                    unsafe { buddy.add_region(&allocator::KernelPhysMap, s, e) };
                }
            };
            match reserved {
                Some((rs, re)) if rs >= start && re <= end => {
                    add(buddy, start, rs);
                    add(buddy, re, end);
                }
                _ => add(buddy, start, end),
            }
        }
    });

    serial_println!("Buddy stats:");
    allocator::debug_print_buddy_stats();

    // COW frame refcount table — sized to the highest usable physical
    // address just measured, not to a constant. Must happen here, after
    // the Buddy allocator can serve the table's own backing frames and
    // before anything calls `fork()`; see `memory::cow`'s module comment
    // for what a table that fails to cover all of RAM does instead of
    // merely degrading.
    unsafe { memory::cow::init_refcount_table(max_usable_end); }
}

/// Run allocator smoke tests (slab, Vec, String).
pub fn test_allocators() {
    {
        use core::alloc::Layout;

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

    allocator::slab_stats();
}