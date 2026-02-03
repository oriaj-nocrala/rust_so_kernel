// kernel/src/allocator/mod.rs

pub mod bump;
pub mod buddy_allocator;

use spin::Mutex;
use x86_64::{
    VirtAddr, structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB}
};
use crate::memory::{paging::ActivePageTable, frame_allocator::BootInfoFrameAllocator};

pub static FRAME_ALLOCATOR: Mutex<Option<BootInfoFrameAllocator>> = Mutex::new(None);
pub static PAGE_TABLE: Mutex<Option<ActivePageTable>> = Mutex::new(None);

pub fn init_allocators(
    page_table: ActivePageTable,
    frame_allocator: BootInfoFrameAllocator
) {
    *PAGE_TABLE.lock() = Some(page_table);
    *FRAME_ALLOCATOR.lock() = Some(frame_allocator);
}

pub fn with_allocators<F, R>(f: F) -> R
where
    F: FnOnce(&mut ActivePageTable, &mut BootInfoFrameAllocator) -> R,
{
    let mut pt = PAGE_TABLE.lock();
    let mut fa = FRAME_ALLOCATOR.lock();

    f(pt.as_mut().unwrap(), fa.as_mut().unwrap())
}

/// Expande el heap mapeando más páginas
// pub fn expand_heap(
//     page_table: &mut ActivePageTable,
//     frame_allocator: &mut BootInfoFrameAllocator,
//     additional_pages: usize,
// ) -> Result<(), &'static str> {
//     let heap_end = bump::heap_end(); // Necesitas exponer esto desde bump.rs
    
//     for i in 0..additional_pages {
//         let page = Page::<Size4KiB>::containing_address(
//             VirtAddr::new(heap_end as u64 + (i * 4096) as u64)
//         );
        
//         let frame = frame_allocator
//             .allocate_frame()
//             .ok_or("Out of physical memory")?;
        
//         let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        
//         unsafe {
//             page_table
//                 .map_page(page, frame, flags, frame_allocator)
//                 .map_err(|_| "Failed to map page")?;
//         }
//     }
    
//     Ok(())
// }

pub fn expand_heap(
    pages: usize
) -> Result<(), &'static str> {
    with_allocators(|pt, fa| {    
        let heap_end = bump::heap_end(); // Necesitas exponer esto desde bump.rs
        for i in 0..pages {
            let page = Page::<Size4KiB>::containing_address(
                VirtAddr::new(heap_end as u64 + (i * 4096) as u64)
            );
            
            let frame = fa
                .allocate_frame()
                .ok_or("Out of physical memory")?;
            
            let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
            
            pt
                .map_page(page, frame, flags, fa)
                .map_err(|_| "Failed to map page")?;
        }

        Ok(())
    })
}