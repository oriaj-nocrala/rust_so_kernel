// memory/frame_allocator.rs

use bootloader_api::info::{MemoryRegions, MemoryRegionKind};
use x86_64::{
    structures::paging::{FrameAllocator, PhysFrame, Size4KiB},
    PhysAddr,
};

pub struct BootInfoFrameAllocator {
    memory_regions: &'static MemoryRegions,
    next: usize, // Índice de la siguiente región
    current_region_start: PhysFrame,
    current_region_end: PhysFrame,
}

unsafe impl Send for BootInfoFrameAllocator {}
unsafe impl Sync for BootInfoFrameAllocator {}

impl BootInfoFrameAllocator {
    pub unsafe fn init(memory_regions: &'static MemoryRegions) -> Self {
        let mut allocator = BootInfoFrameAllocator {
            memory_regions,
            next: 0,
            current_region_start: PhysFrame::containing_address(PhysAddr::new(0)),
            current_region_end: PhysFrame::containing_address(PhysAddr::new(0)),
        };
        allocator.advance_to_next_usable_region();
        allocator
    }
    
    fn advance_to_next_usable_region(&mut self) {
        // Buscar la siguiente región usable
        while self.next < self.memory_regions.len() {
            let region = &self.memory_regions[self.next];
            
            if region.kind == MemoryRegionKind::Usable {
                self.current_region_start = PhysFrame::containing_address(
                    PhysAddr::new(region.start)
                );
                self.current_region_end = PhysFrame::containing_address(
                    PhysAddr::new(region.end - 1)
                );
                return;
            }
            
            self.next += 1;
        }
        
        // No hay más regiones
        self.current_region_start = PhysFrame::containing_address(PhysAddr::new(0));
        self.current_region_end = PhysFrame::containing_address(PhysAddr::new(0));
    }
}

unsafe impl FrameAllocator<Size4KiB> for BootInfoFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        if self.current_region_start > self.current_region_end {
            // Región actual agotada, avanzar a la siguiente
            self.next += 1;
            self.advance_to_next_usable_region();
        }
        
        if self.current_region_start > self.current_region_end {
            // No hay más frames
            return None;
        }
        
        let frame = self.current_region_start;
        self.current_region_start += 1;
        Some(frame)
    }
}