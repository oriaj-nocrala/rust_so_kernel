use core::{
    alloc::{GlobalAlloc, Layout}, 
    ptr::null_mut, 
    sync::atomic::{AtomicUsize, Ordering}
};

use crate::serial_println;

/// Alinea `addr` hacia arriba al multiplo mas cercano de `align`.
/// `align` debe ser una potencia de 2.
const fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

pub struct BumpAllocator {
    pub heap_start: AtomicUsize,
    pub heap_end: AtomicUsize,
    next: AtomicUsize,
}

impl BumpAllocator {
    /// Crea un nuevo BumpAllocator (sin inicializar)
    pub const fn new() -> Self {
        Self {
            heap_start: AtomicUsize::new(0),
            heap_end: AtomicUsize::new(0),
            next: AtomicUsize::new(0),
        }
    }

    /// Inicializa el heap con un rango de memoria
    pub unsafe fn init(&self, heap_start: usize, heap_size: usize) {
        self.heap_start.store(heap_start, Ordering::Release);
        self.heap_end.store(heap_start + heap_size, Ordering::Release);
        self.next.store(heap_start, Ordering::Release);
        // Actualizariamos heap_start y heap_end aqui si fueran mutables
        // Por ahora, se configuran en la definicion estatica
    }

    fn used_internal(&self) -> usize {
        self.next.load(Ordering::Relaxed) - self.heap_start.load(Ordering::Relaxed)
    }

    fn size_internal(&self) -> usize {
        self.heap_end.load(Ordering::Relaxed) - self.heap_start.load(Ordering::Relaxed)
    }
    
    fn heap_end_internal(&self) -> usize {
        self.heap_end.load(Ordering::Relaxed)
    }
    
    fn expand_internal(&self, new_end: usize) {
        self.heap_end.store(new_end, Ordering::Release);
    }
}

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size();
        let align = layout.align();

        let heap_start = self.heap_start.load(Ordering::Acquire);
        let heap_end = self.heap_end.load(Ordering::Acquire);
        
        // Imprimir por serial (puerto COM1)
        serial_println!("ALLOC: size={}, align={}, heap_start={:#x}, heap_end={:#x}", 
            size, align, heap_start, heap_end);

        if heap_start == 0 || heap_end == 0 {
            serial_println!("ERROR: Heap not initialized!");
            return null_mut();
        }

        loop {
            // Leer la posicion actual
            let current = self.next.load(Ordering::Relaxed);

            // Alinear al requerimiento
            let aligned = align_up(current, align);

            // Calcular nueva posicion
            let new_next = aligned.saturating_add(size);

            // Verificar overflow y bounds
            if new_next > heap_end {
                serial_println!("  OOM!");
                return null_mut(); // OOM
            }

            // CAS para thread-safety
            // Intentar actualizar atomicamente (thread-safe)
            if self.next.compare_exchange(
                current,
                new_next as usize,
                Ordering::Release,
                Ordering::Relaxed,
            ).is_ok() {
                serial_println!("  SUCCESS: allocated at {:#x}", aligned);
                return aligned as *mut u8;
            }
            // Si fallo el CAS, otro thread gano, reintentamos
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Bump allocator no hace nada en dealloc
        // (memoria se libera al resetear todo)
        serial_println!("DEALLOC: ptr={:#x}, size={}", ptr as usize, layout.size());
    }
}

// ========== Global Allocator ==========

// ⭐ 100 KB de heap estático
pub static mut HEAP_MEMORY: [u8; 100 * 1024] = [0; 100 * 1024];

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator::new();

// ========== Funciones públicas ==========

/// Inicializa el heap del kernel
pub fn init_heap() {
    unsafe {
        let heap_start = HEAP_MEMORY.as_ptr() as usize;
        let heap_size = HEAP_MEMORY.len();

        serial_println!("Initializing heap:");
        serial_println!("  start: {:#x}", heap_start);
        serial_println!("  size:  {} bytes", heap_size);
        serial_println!("  end:   {:#x}", heap_start + heap_size);

        ALLOCATOR.init(heap_start, heap_size);
    }
}

/// Retorna estadisticas del heap
pub fn heap_stats() -> (usize, usize) {
    (ALLOCATOR.used_internal(), ALLOCATOR.size_internal())
}

pub fn heap_end() -> usize {
    ALLOCATOR.heap_end_internal()
}

pub fn expand_heap_size(new_end: usize) {
    ALLOCATOR.expand_internal(new_end);
}