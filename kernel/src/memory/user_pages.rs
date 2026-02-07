// kernel/src/memory/user_pages.rs

use x86_64::{
    VirtAddr,
    structures::paging::{
        Page, PhysFrame, Size4KiB, PageTableFlags,
        Mapper, FrameAllocator,
    },
};

/// Mapea páginas con permisos de usuario (USER_ACCESSIBLE)
/// 
/// # Safety
/// El caller debe asegurar que:
/// - `start` es una dirección virtual válida
/// - `num_pages` no causa overflow
/// - Las páginas no están ya mapeadas
pub unsafe fn map_user_pages<A>(
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut A,
    start: VirtAddr,
    num_pages: usize,
) -> Result<(), &'static str>
where
    A: FrameAllocator<Size4KiB>,
{
    // Flags para páginas de usuario
    let flags = PageTableFlags::PRESENT
              | PageTableFlags::WRITABLE
              | PageTableFlags::USER_ACCESSIBLE; // ← Clave para Ring 3
    
    crate::serial_println!(
        "Mapping {} user pages at {:#x}",
        num_pages,
        start.as_u64()
    );
    
    for i in 0..num_pages {
        let page_addr = start + (i as u64 * 4096);
        let page: Page<Size4KiB> = Page::containing_address(page_addr);
        
        // Alocar frame físico
        let frame = frame_allocator
            .allocate_frame()
            .ok_or("Failed to allocate frame for user page")?;
        
        crate::serial_println!(
            "  Page {}: virt={:#x} -> phys={:#x}",
            i,
            page_addr.as_u64(),
            frame.start_address().as_u64()
        );
        
        // Mapear con flags USER
        mapper
            .map_to(page, frame, flags, frame_allocator)
            .map_err(|_| "Failed to map user page")?
            .flush();
    }
    
    crate::serial_println!("User pages mapped successfully");
    Ok(())
}

/// Desmapea páginas de usuario
pub unsafe fn unmap_user_pages(
    mapper: &mut impl Mapper<Size4KiB>,
    start: VirtAddr,
    num_pages: usize,
) -> Result<(), &'static str> {
    for i in 0..num_pages {
        let page_addr = start + (i as u64 * 4096);
        let page: Page<Size4KiB> = Page::containing_address(page_addr);
        
        mapper
            .unmap(page)
            .map_err(|_| "Failed to unmap user page")?
            .1
            .flush();
    }
    
    Ok(())
}