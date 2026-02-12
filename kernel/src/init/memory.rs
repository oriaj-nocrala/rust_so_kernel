// kernel/src/init/memory.rs
//
// Physical memory offset, frame allocator, page table, buddy, slab.
// Code moved verbatim from kernel_main.

use bootloader_api::info::{MemoryRegions, MemoryRegionKind};
use x86_64::VirtAddr;

use crate::{
    allocator,
    memory::{
        self,
        frame_allocator::BootInfoFrameAllocator,
        paging::ActivePageTable,
    },
    serial_println,
};

/// Initialize all memory subsystems in order:
/// phys offset → frame allocator → page table → allocators → buddy.
pub fn init_core(phys_mem_offset: VirtAddr, memory_regions: &'static MemoryRegions) {
    // ✅ Print the physical memory offset so we can verify PML4 entry
    serial_println!("Physical memory offset: {:#x} (PML4 entry {})",
        phys_mem_offset.as_u64(),
        phys_mem_offset.as_u64() >> 39
    );

    memory::init(phys_mem_offset);
    
    // --- Inicialización de Memoria ---
    let frame_allocator = unsafe {
        BootInfoFrameAllocator::init(memory_regions)
    };
    
    let page_table = unsafe {
        ActivePageTable::new(phys_mem_offset)
    };
    
    allocator::init_allocators(page_table, frame_allocator);

    // --- Inicializar Buddy Allocator ---
    {
        let mut buddy = allocator::buddy_allocator::BUDDY.lock();
        
        for region in memory_regions.iter() {
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
}

/// Run allocator smoke tests (slab, Vec, String).
pub fn test_allocators() {
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
}