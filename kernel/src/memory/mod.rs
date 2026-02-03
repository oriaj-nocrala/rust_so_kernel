// kernel/src/memory/mod.rs

use x86_64::VirtAddr;
use core::sync::atomic::{AtomicU64, Ordering};

pub mod paging;
pub mod frame_allocator;

static PHYSICAL_MEMORY_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Inicializa el offset de memoria física (llamar desde kernel_main)
pub fn init(physical_memory_offset: VirtAddr) {
    PHYSICAL_MEMORY_OFFSET.store(physical_memory_offset.as_u64(), Ordering::Relaxed);
}

/// Obtiene el offset de memoria física
pub fn physical_memory_offset() -> VirtAddr {
    VirtAddr::new(PHYSICAL_MEMORY_OFFSET.load(Ordering::Relaxed))
}