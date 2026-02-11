// kernel/src/allocator/slab.rs

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{self, null_mut, NonNull};
use spin::Mutex;
use x86_64::{PhysAddr, VirtAddr};

use super::buddy_allocator::BUDDY;

// Tamaños de slab: 8, 16, 32, 64, 128, 256, 512, 1024, 2048 bytes
const SLAB_SIZES: &[usize] = &[8, 16, 32, 64, 128, 256, 512, 1024, 2048];
const NUM_SLABS: usize = SLAB_SIZES.len();
const MAX_SLAB_SIZE: usize = 2048;

// ✅ Constantes para cálculo de order
const PAGE_SIZE: usize = 4096;
const PAGE_ORDER: usize = 12; // log2(4096)

// ✅ FUNCIÓN CENTRALIZADA para calcular order (usada en alloc Y free)
fn size_to_buddy_order(size: usize) -> usize {
    // Calcular cuántas páginas necesitamos
    let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
    
    // Order = log2(páginas) + PAGE_ORDER
    if pages == 0 {
        PAGE_ORDER
    } else if pages == 1 {
        PAGE_ORDER
    } else {
        let order_offset = pages.next_power_of_two().trailing_zeros() as usize;
        PAGE_ORDER + order_offset
    }
}

// Compile-time checks
const _: () = {
    // ✅ Todas las size classes deben ser potencia de 2
    let mut i = 0;
    while i < SLAB_SIZES.len() {
        assert!(SLAB_SIZES[i].is_power_of_two());
        i += 1;
    }
    
    // ✅ Todas deben caber en una página
    let mut i = 0;
    while i < SLAB_SIZES.len() {
        assert!(SLAB_SIZES[i] <= 4096);
        i += 1;
    }
    
    // ✅ MAX_SLAB_SIZE debe ser menor que PAGE_SIZE
    assert!(MAX_SLAB_SIZE <= 4096);
};

pub struct SlabAllocator {
    caches: [SlabCache; NUM_SLABS],
}

unsafe impl Send for SlabAllocator {}

impl SlabAllocator {
    pub const fn new() -> Self {
        const CACHE_INIT: SlabCache = SlabCache::new();
        Self {
            caches: [CACHE_INIT; NUM_SLABS],
        }
    }

    /// Encuentra el índice del slab apropiado para un tamaño
    fn slab_index(size: usize) -> Option<usize> {
        SLAB_SIZES.iter().position(|&s| s >= size)
    }

    /// Allocate usando slab o buddy
    pub unsafe fn allocate(&mut self, layout: Layout) -> *mut u8 {
        let size = layout.size().max(layout.align());

        if size > MAX_SLAB_SIZE {
            // Usar Buddy directamente para allocaciones grandes
            return self.allocate_large(size, layout.align());
        }

        // Usar slab cache
        if let Some(idx) = Self::slab_index(size) {
            // ✅ VALIDAR que el size class es potencia de 2
            debug_assert!(SLAB_SIZES[idx].is_power_of_two(), 
                "Slab size must be power of 2");
            debug_assert!(SLAB_SIZES[idx] <= PAGE_SIZE,
                "Slab size must fit in page");
            self.caches[idx].allocate(SLAB_SIZES[idx])
        } else {
            null_mut()
        }
    }

    /// Deallocate
    pub unsafe fn deallocate(&mut self, ptr: *mut u8, layout: Layout) {
        if ptr.is_null() {
            return;
        }

        let size = layout.size().max(layout.align());

        if size > MAX_SLAB_SIZE {
            crate::serial_print_raw!(">>> Slab: Large dealloc\n");
            self.deallocate_large(ptr, size);
            return;
        }

        if let Some(idx) = Self::slab_index(size) {
            // ✅ VALIDAR simetría: mismo idx en alloc y free
            debug_assert_eq!(
                Some(idx), 
                Self::slab_index(size),
                "Free must use same size class as alloc"
            );

            self.caches[idx].deallocate(ptr, SLAB_SIZES[idx]);
        }
    }

    /// Allocación grande usando Buddy
    unsafe fn allocate_large(&mut self, size: usize, align: usize) -> *mut u8 {
        crate::serial_print_raw!(">>> allocate_large: start\n");
        
        // ✅ Considerar alineación
        let total_size = size.max(align);
        
        // ✅ USAR FUNCIÓN CENTRALIZADA
        let order = size_to_buddy_order(total_size);
        
        crate::serial_print_raw!(">>> allocate_large: order=");
        print_usize(order);
        crate::serial_print_raw!("\n");

        let result = BUDDY.lock()
            .allocate(order)
            .map(|phys_addr| {
                let phys_offset = crate::memory::physical_memory_offset();
                let virt = phys_offset + phys_addr.as_u64();
                virt.as_mut_ptr::<u8>()
            })
            .unwrap_or(null_mut());
        
        if result.is_null() {
            crate::serial_print_raw!(">>> allocate_large: FAILED\n");
        } else {
            crate::serial_print_raw!(">>> allocate_large: OK\n");
        }
        
        result
    }

    unsafe fn deallocate_large(&mut self, ptr: *mut u8, size: usize) {
        crate::serial_print_raw!(">>> deallocate_large\n");
        
        // ✅ MISMA FUNCIÓN que allocate_large (simetría crítica)
        let order = size_to_buddy_order(size);

        let phys_offset = crate::memory::physical_memory_offset();
        let virt = VirtAddr::new(ptr as u64);
        let phys = PhysAddr::new(virt.as_u64() - phys_offset.as_u64());

        BUDDY.lock().deallocate(phys, order);
    }

    /// Debug: estadísticas SIN allocaciones
    pub fn stats(&self) {
        crate::serial_print_raw!("Slab Allocator Stats:\n");
        for (idx, cache) in self.caches.iter().enumerate() {
            let (total, used) = cache.stats();
            if total > 0 {
                crate::serial_print_raw!("  ");
                print_usize(SLAB_SIZES[idx]);
                crate::serial_print_raw!("B: ");
                print_usize(used);
                crate::serial_print_raw!("/");
                print_usize(total);
                crate::serial_print_raw!(" objects (");
                print_usize((used * 100) / total.max(1));
                crate::serial_print_raw!("% used)\n");
            }
        }
    }
}

//HELPER FUNCTION

fn print_usize(n: usize) {
    if n == 0 {
        crate::serial_print_raw!("0");
        return;
    }
    
    let mut buf = [0u8; 20];
    let mut i = 0;
    let mut num = n;
    
    while num > 0 {
        buf[i] = b'0' + (num % 10) as u8;
        num /= 10;
        i += 1;
    }
    
    while i > 0 {
        i -= 1;
        unsafe {
            let mut port = x86_64::instructions::port::Port::<u8>::new(0x3F8);
            port.write(buf[i]);
        }
    }
}

/// Un slab cache para objetos de un tamaño fijo
struct SlabCache {
    free_list: Option<NonNull<FreeObject>>,
    total_objects: usize,
    used_objects: usize,
}

impl SlabCache {
    const fn new() -> Self {
        Self {
            free_list: None,
            total_objects: 0,
            used_objects: 0,
        }
    }

    /// Allocate un objeto del slab
    unsafe fn allocate(&mut self, object_size: usize) -> *mut u8 {
        // Si no hay objetos libres, expandir el cache
        if self.free_list.is_none() {
            if !self.expand(object_size) {
                return null_mut();
            }
        }

        // Tomar el primer objeto libre
        let free_obj = self.free_list.unwrap();

        #[cfg(debug_assertions)]
        {
            // ✅ Verificar que no está corrupto
            let ptr = free_obj.as_ptr() as *mut u8;
            for i in 0..object_size.min(8) {
                let val = ptr.add(i).read();
                // Si no es 0xDD (free poison), está OK o es primera vez
                if val == 0xAA {
                    panic!("Use-after-free detected at {:#x}", ptr as u64);
                }
            }
        }
        
        self.free_list = free_obj.as_ref().next;
        self.used_objects += 1;

        let ptr = free_obj.as_ptr() as *mut u8;

        #[cfg(debug_assertions)]
        {
            // ✅ Poison con patrón de "allocated"
            core::ptr::write_bytes(ptr, 0xAA, object_size.min(256));
        }

        ptr
    }

    /// Deallocate un objeto
    unsafe fn deallocate(&mut self, ptr: *mut u8, object_size: usize) {

        #[cfg(debug_assertions)]
        {
            // ✅ Poison con patrón de "freed"
            core::ptr::write_bytes(ptr, 0xDD, object_size.min(256));
        }

        let free_obj = NonNull::new_unchecked(ptr as *mut FreeObject);
        
        // Agregar al inicio de la free list
        let old_head = self.free_list;
        free_obj.as_ptr().write(FreeObject { next: old_head });
        self.free_list = Some(free_obj);
        
        self.used_objects = self.used_objects.saturating_sub(1);
    }

    /// Expandir el cache allocando una nueva página del Buddy
    unsafe fn expand(&mut self, object_size: usize) -> bool {
        // Allocar una página de 4KB del Buddy
        let page_phys = match BUDDY.lock().allocate(12) {
            Some(addr) => addr,
            None => {
                crate::serial_println!("Slab: Failed to expand (OOM)");
                return false;
            }
        };

        let phys_offset = crate::memory::physical_memory_offset();
        let page_virt = phys_offset + page_phys.as_u64();
        let page_ptr = page_virt.as_mut_ptr::<u8>();

        // Dividir la página en objetos
        const PAGE_SIZE: usize = 4096;
        let objects_per_page = PAGE_SIZE / object_size;

        for i in 0..objects_per_page {
            let obj_ptr = page_ptr.add(i * object_size) as *mut FreeObject;
            let free_obj = NonNull::new_unchecked(obj_ptr);

            // Link a la free list
            obj_ptr.write(FreeObject {
                next: self.free_list,
            });
            self.free_list = Some(free_obj);
        }

        self.total_objects += objects_per_page;

        crate::serial_println!(
            "Slab: Expanded {}B cache (+{} objects)",
            object_size,
            objects_per_page
        );

        true
    }

    fn stats(&self) -> (usize, usize) {
        (self.total_objects, self.used_objects)
    }
}

/// Nodo en la free list
#[repr(C)]
struct FreeObject {
    next: Option<NonNull<FreeObject>>,
}

// Global slab allocator
static SLAB_ALLOCATOR: Mutex<SlabAllocator> = Mutex::new(SlabAllocator::new());

// GlobalAlloc implementation
pub struct SlabGlobalAlloc;

unsafe impl GlobalAlloc for SlabGlobalAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        SLAB_ALLOCATOR.lock().allocate(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        SLAB_ALLOCATOR.lock().deallocate(ptr, layout);
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: SlabGlobalAlloc = SlabGlobalAlloc;

// Función pública para stats
pub fn slab_stats() {
    SLAB_ALLOCATOR.lock().stats();
}