// kernel/src/memory/user_code.rs
// Manejo de código de usuario en páginas dedicadas

use x86_64::{
    VirtAddr, PhysAddr,
    structures::paging::{
        Page, PageTableFlags, Size4KiB,
        Mapper, FrameAllocator,
    },
};

/// Dirección base para código de usuario (como /bin en Linux: 0x400000)
pub const USER_CODE_BASE: u64 = 0x0000_0000_0040_0000;

/// Tamaño máximo de código por proceso (16 páginas = 64KB)
pub const USER_CODE_SIZE: usize = 16 * 4096;

/// Mapea páginas para código de usuario y copia el código
/// 
/// # Safety
/// - `code_ptr` debe apuntar a código ejecutable válido
/// - `code_size` debe ser el tamaño real del código
pub unsafe fn setup_user_code<A>(
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut A,
    code_ptr: *const u8,
    code_size: usize,
) -> Result<VirtAddr, &'static str>
where
    A: FrameAllocator<Size4KiB>,
{
    if code_size > USER_CODE_SIZE {
        return Err("Code too large");
    }

    let num_pages = (code_size + 4095) / 4096;
    
    crate::serial_println!(
        "Setting up user code: {} bytes ({} pages)",
        code_size,
        num_pages
    );

    // Flags para código de usuario: PRESENT + USER + eXecutable (sin WRITABLE)
    let flags = PageTableFlags::PRESENT
              | PageTableFlags::USER_ACCESSIBLE;
    // Nota: NO incluimos WRITABLE para que sea read-only + executable

    // Obtener el offset de memoria física para traducir direcciones
    let phys_offset = crate::memory::physical_memory_offset();

    // Mapear y copiar página por página
    for i in 0..num_pages {
        let page_addr = VirtAddr::new(USER_CODE_BASE + (i as u64 * 4096));
        let page: Page<Size4KiB> = Page::containing_address(page_addr);

        // Allocar frame físico
        let frame = frame_allocator
            .allocate_frame()
            .ok_or("Failed to allocate frame for user code")?;

        // Mapear con flags USER
        mapper
            .map_to(page, frame, flags, frame_allocator)
            .map_err(|_| "Failed to map user code page")?
            .flush();

        // Copiar el código a esta página
        let dst = (phys_offset + frame.start_address().as_u64()).as_mut_ptr::<u8>();
        let src = code_ptr.add(i * 4096);
        let copy_size = code_size.saturating_sub(i * 4096).min(4096);

        core::ptr::copy_nonoverlapping(src, dst, copy_size);

        // Limpiar el resto de la página
        if copy_size < 4096 {
            core::ptr::write_bytes(dst.add(copy_size), 0, 4096 - copy_size);
        }

        crate::serial_println!(
            "  Page {}: virt={:#x} -> phys={:#x}, copied {} bytes",
            i,
            page_addr.as_u64(),
            frame.start_address().as_u64(),
            copy_size
        );
    }

    crate::serial_println!("User code setup complete");
    Ok(VirtAddr::new(USER_CODE_BASE))
}

/// Obtiene el tamaño del código de una función
/// 
/// HACK: Asumimos que la siguiente función está después de esta.
/// Para producción, necesitarías símbolos del linker.
/// 
/// # Safety
/// Esto es extremadamente unsafe y solo funciona como heurística
pub unsafe fn estimate_code_size(func_ptr: *const u8, next_func_ptr: Option<*const u8>) -> usize {
    let start = func_ptr as usize;
    
    if let Some(next) = next_func_ptr {
        let end = next as usize;
        if end > start {
            return end - start;
        }
    }
    
    // Fallback: asumir 4KB (1 página)
    4096
}