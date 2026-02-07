// kernel/src/allocator/mod.rs

// pub mod bump;  // ❌ Comentar o borrar
pub mod buddy_allocator;
pub mod slab;  // ✅ Agregar

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

// ❌ expand_heap ya no se necesita con Slab