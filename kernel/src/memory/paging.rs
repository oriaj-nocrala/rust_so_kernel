// kernel/src/memory/paging.rs

use x86_64::{
    PhysAddr, VirtAddr, structures::paging::{
        FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB, Translate, mapper::{MapToError, UnmapError as X86UnmapError}
    }
};

/// Mapper activo (usa el CR3 actual)
pub struct ActivePageTable {
    pub mapper: OffsetPageTable<'static>,
}

impl ActivePageTable {
    /// Crea desde el CR3 actual
    pub unsafe fn new(physical_memory_offset: VirtAddr) -> Self {
        let (level_4_table_frame, _) = x86_64::registers::control::Cr3::read();
        
        let phys = level_4_table_frame.start_address();
        let virt = physical_memory_offset + phys.as_u64();
        let page_table_ptr: *mut PageTable = virt.as_mut_ptr();
        let level_4_table = &mut *page_table_ptr;
        
        Self {
            mapper: OffsetPageTable::new(level_4_table, physical_memory_offset),
        }
    }
    
    /// Traduce virtual → física
    pub fn translate(&self, addr: VirtAddr) -> Option<PhysAddr> {
        self.mapper.translate_addr(addr)
    }
    
    /// Mapea una página a un frame
    pub fn map_page(
        &mut self,
        page: Page<Size4KiB>,
        frame: PhysFrame<Size4KiB>,
        flags: PageTableFlags,
        frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    ) -> Result<(), MapError> {
        unsafe {
            self.mapper
                .map_to(page, frame, flags, frame_allocator)?
                .flush();
        }
        Ok(())
    }
    
    /// Unmapea una página
    pub fn unmap_page(&mut self, page: Page<Size4KiB>) -> Result<(), UnmapError> {
        let (_, flush) = self.mapper.unmap(page)?;
        flush.flush();
        Ok(())
    }
}

#[derive(Debug)]
pub enum MapError {
    FrameAllocationFailed,
    ParentEntryHugePage,
    PageAlreadyMapped,
}

#[derive(Debug)]
pub enum UnmapError {
    PageNotMapped,
    ParentEntryHugePage,
}

// Implementar From para MapError
impl From<MapToError<Size4KiB>> for MapError {
    fn from(err: MapToError<Size4KiB>) -> Self {
        match err {
            MapToError::FrameAllocationFailed => MapError::FrameAllocationFailed,
            MapToError::ParentEntryHugePage => MapError::ParentEntryHugePage,
            MapToError::PageAlreadyMapped(_) => MapError::PageAlreadyMapped,
        }
    }
}

// Implementar From para UnmapError
impl From<X86UnmapError> for UnmapError {
    fn from(err: X86UnmapError) -> Self {
        match err {
            X86UnmapError::PageNotMapped => UnmapError::PageNotMapped,
            X86UnmapError::ParentEntryHugePage => UnmapError::ParentEntryHugePage,
            _ => UnmapError::PageNotMapped, // catch-all
        }
    }
}