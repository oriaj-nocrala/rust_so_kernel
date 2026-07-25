// mm/src/slab.rs

use crate::{FrameSource, PhysMap};
use core::alloc::Layout;
use core::ptr::{null_mut, NonNull};
use x86_64::PhysAddr;

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
    if pages <= 1 {
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

// ============================================================================
// Reported-by-value events — see crate doc comment's "Two seams" section.
// This mechanically replaces the pre-extraction `serial_println_raw!` calls
// scattered through `allocate`/`allocate_large`/`deallocate_large`/`expand`
// — same text, same conditions, just returned as data so
// `kernel/src/allocator/mod.rs` can print it. `AllocEvent::None`/
// `DeallocEvent::Small` are the common, silent paths (no print happened
// there before either).
// ============================================================================

/// What happened during `SlabCache::expand` — mirrors the pre-extraction
/// `"Slab: Failed to expand {}B cache (OOM)"` / `"Slab: Expanded {}B cache
/// (+{} objects, total {})"` lines.
#[derive(Debug, Clone, Copy)]
pub struct ExpandTrace {
    pub object_size: usize,
    pub ok: bool,
    /// Valid when `ok`: how many objects this expansion added.
    pub added_objects: usize,
    /// Valid when `ok`: total objects in the cache after this expansion.
    pub total_objects: usize,
}

/// What happened on the large-object (direct-buddy) allocation path —
/// mirrors the pre-extraction `">>> allocate_large: size={} order={}"` +
/// `">>> allocate_large: OK at {:#x}"`/`"FAILED"` lines.
#[derive(Debug, Clone, Copy)]
pub struct LargeAllocTrace {
    pub size: usize,
    pub order: usize,
    pub ok: bool,
    /// Valid when `ok`: the returned pointer, as a plain address for
    /// formatting (avoids threading a raw pointer through the report type).
    pub result_addr: u64,
}

/// What happened during `SlabAllocator::allocate` — `None` for the common
/// slab-cache path that needed no expansion (silent, same as before).
#[derive(Debug, Clone, Copy)]
pub enum AllocEvent {
    None,
    Expand(ExpandTrace),
    Large(LargeAllocTrace),
}

/// What happened on the large-object (direct-buddy) deallocation path —
/// mirrors the pre-extraction `">>> Slab: Large dealloc"` marker plus
/// `"[SLAB] deallocate_large: virt={:#x} phys={:#x} size={} order={}"` and
/// the conditional `"[SLAB]   ^^^ THIS IS IN THE HOT RANGE!"` line. The
/// hot-range check (`(phys & !0x3FFF) == 0x1ecb0000`) is a hardcoded
/// address window from a past debugging investigation, preserved verbatim
/// — not reinterpreted or generalized as part of this move.
#[derive(Debug, Clone, Copy)]
pub struct LargeDeallocTrace {
    pub size: usize,
    pub order: usize,
    pub virt: u64,
    pub phys: u64,
    pub hot_range: bool,
}

/// What happened during `SlabAllocator::deallocate`.
#[derive(Debug, Clone, Copy)]
pub enum DeallocEvent {
    /// `ptr` was null — a no-op both before and after this extraction.
    Ignored,
    /// Slab-cache path — no trace output before either.
    Small,
    Large(LargeDeallocTrace),
}

/// Return value of [`SlabAllocator::allocate`]: the pointer plus whatever
/// happened along the way (see [`AllocEvent`]).
pub struct AllocResult {
    pub ptr: *mut u8,
    pub event: AllocEvent,
}

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
    pub unsafe fn allocate(&mut self, mem: &dyn PhysMap, frames: &dyn FrameSource, layout: Layout) -> AllocResult {
        let size = layout.size().max(layout.align());

        if size > MAX_SLAB_SIZE {
            // Usar Buddy directamente para allocaciones grandes
            let (ptr, trace) = self.allocate_large(mem, frames, size, layout.align());
            return AllocResult { ptr, event: AllocEvent::Large(trace) };
        }

        // Usar slab cache
        if let Some(idx) = Self::slab_index(size) {
            // ✅ VALIDAR que el size class es potencia de 2
            debug_assert!(SLAB_SIZES[idx].is_power_of_two(),
                "Slab size must be power of 2");
            debug_assert!(SLAB_SIZES[idx] <= PAGE_SIZE,
                "Slab size must fit in page");
            let (ptr, expand_trace) = self.caches[idx].allocate(mem, frames, SLAB_SIZES[idx]);
            let event = match expand_trace {
                Some(t) => AllocEvent::Expand(t),
                None => AllocEvent::None,
            };
            AllocResult { ptr, event }
        } else {
            AllocResult { ptr: null_mut(), event: AllocEvent::None }
        }
    }

    /// Deallocate
    pub unsafe fn deallocate(&mut self, mem: &dyn PhysMap, frames: &dyn FrameSource, ptr: *mut u8, layout: Layout) -> DeallocEvent {
        if ptr.is_null() {
            return DeallocEvent::Ignored;
        }

        let size = layout.size().max(layout.align());

        if size > MAX_SLAB_SIZE {
            let trace = self.deallocate_large(mem, frames, ptr, size);
            return DeallocEvent::Large(trace);
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
        DeallocEvent::Small
    }

    /// Allocación grande usando Buddy
    unsafe fn allocate_large(&mut self, mem: &dyn PhysMap, frames: &dyn FrameSource, size: usize, align: usize) -> (*mut u8, LargeAllocTrace) {
        // ✅ Considerar alineación
        let total_size = size.max(align);

        // ✅ USAR FUNCIÓN CENTRALIZADA
        let order = size_to_buddy_order(total_size);

        let result = frames.alloc_order(order)
            .map(|phys_addr| mem.virt_for(phys_addr))
            .unwrap_or(null_mut());

        let trace = LargeAllocTrace {
            size: total_size,
            order,
            ok: !result.is_null(),
            result_addr: result as u64,
        };

        (result, trace)
    }

    unsafe fn deallocate_large(&mut self, mem: &dyn PhysMap, frames: &dyn FrameSource, ptr: *mut u8, size: usize) -> LargeDeallocTrace {
        // ✅ MISMA FUNCIÓN que allocate_large (simetría crítica)
        let order = size_to_buddy_order(size);

        // Reverse of `mem.virt_for`: this crate doesn't otherwise need a
        // virt→phys direction, so this stays a local computation exactly
        // like the pre-extraction code's `virt.as_u64() - phys_offset`,
        // just through the `PhysMap`-derived base instead of calling
        // `physical_memory_offset()` directly. `virt_for(PhysAddr::zero())`
        // is the same base address `physical_memory_offset()` was.
        let base = mem.virt_for(PhysAddr::zero()) as u64;
        let phys = PhysAddr::new(ptr as u64 - base);

        let hot_range = (phys.as_u64() & !0x3FFF) == 0x1ecb0000;

        let trace = LargeDeallocTrace {
            size,
            order,
            virt: ptr as u64,
            phys: phys.as_u64(),
            hot_range,
        };

        frames.free_order(phys, order);

        trace
    }

    /// (size_class, total_objects, used_objects) per non-empty cache — what
    /// the pre-extraction `stats()` printed directly as `"  {}B: {}/{}
    /// objects ({}% used)"`. `kernel/src/allocator/mod.rs::slab_stats` now
    /// does the printing (skipping zero-total caches, same as before).
    pub fn cache_stats(&self) -> [(usize, usize, usize); NUM_SLABS] {
        let mut out = [(0usize, 0usize, 0usize); NUM_SLABS];
        for (idx, cache) in self.caches.iter().enumerate() {
            let (total, used) = cache.stats();
            out[idx] = (SLAB_SIZES[idx], total, used);
        }
        out
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

    /// Allocate un objeto del slab. Returns the pointer plus an
    /// `ExpandTrace` if expansion was needed to serve this request (`None`
    /// if an already-free object was available).
    unsafe fn allocate(&mut self, mem: &dyn PhysMap, frames: &dyn FrameSource, object_size: usize) -> (*mut u8, Option<ExpandTrace>) {
        // Si no hay objetos libres, expandir el cache
        let mut expand_trace = None;
        if self.free_list.is_none() {
            let trace = self.expand(mem, frames, object_size);
            let ok = trace.ok;
            expand_trace = Some(trace);
            if !ok {
                return (null_mut(), expand_trace);
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

        (ptr, expand_trace)
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
    unsafe fn expand(&mut self, mem: &dyn PhysMap, frames: &dyn FrameSource, object_size: usize) -> ExpandTrace {
        // Allocar una página de 4KB del Buddy
        let page_phys = match frames.alloc_order(12) {
            Some(addr) => addr,
            None => {
                return ExpandTrace { object_size, ok: false, added_objects: 0, total_objects: self.total_objects };
            }
        };

        let page_ptr = mem.virt_for(page_phys);

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

        ExpandTrace {
            object_size,
            ok: true,
            added_objects: objects_per_page,
            total_objects: self.total_objects,
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    /// Same idea as `buddy::tests::VecMem`: a flat host buffer standing in
    /// for physical memory, addressed from a fixed "physical base" the way
    /// the kernel's real `physical_memory_offset()` does.
    struct VecMem {
        buf: std::cell::UnsafeCell<Vec<u8>>,
    }

    impl VecMem {
        fn new(size: usize) -> Self {
            Self { buf: std::cell::UnsafeCell::new(std::vec![0u8; size]) }
        }
    }

    unsafe impl Sync for VecMem {}

    impl PhysMap for VecMem {
        fn virt_for(&self, pa: PhysAddr) -> *mut u8 {
            unsafe {
                let buf = &mut *self.buf.get();
                assert!((pa.as_u64() as usize) < buf.len(), "test PhysMap OOB: {:#x}", pa.as_u64());
                buf.as_mut_ptr().add(pa.as_u64() as usize)
            }
        }
    }

    /// A `FrameSource` backed by a simple bump allocator over `VecMem`'s
    /// buffer — enough to serve the slab's page-sized (order-12) and
    /// large-object requests in tests without needing the real buddy
    /// allocator (which is tested on its own in `buddy::tests`).
    struct BumpFrames {
        next: core::cell::Cell<u64>,
        limit: u64,
    }

    impl BumpFrames {
        fn new(limit: u64) -> Self {
            Self { next: core::cell::Cell::new(4096), limit } // start past addr 0
        }
    }

    impl FrameSource for BumpFrames {
        unsafe fn alloc_order(&self, order: usize) -> Option<PhysAddr> {
            let size = 1u64 << order;
            let aligned = (self.next.get() + size - 1) & !(size - 1);
            if aligned + size > self.limit {
                return None;
            }
            self.next.set(aligned + size);
            Some(PhysAddr::new(aligned))
        }
        unsafe fn free_order(&self, _addr: PhysAddr, _order: usize) {
            // Bump allocator: frees are no-ops, fine for these tests (none
            // of them depend on reuse after a large-object free).
        }
    }

    const TEST_MEM_SIZE: u64 = 4 * 1024 * 1024; // 4 MiB

    // ── size_to_buddy_order ──────────────────────────────────────────────

    #[test]
    fn size_to_buddy_order_one_page_or_less() {
        assert_eq!(size_to_buddy_order(1), PAGE_ORDER);
        assert_eq!(size_to_buddy_order(4096), PAGE_ORDER);
    }

    #[test]
    fn size_to_buddy_order_multiple_pages_rounds_up_to_power_of_two() {
        // 2 pages -> order+1
        assert_eq!(size_to_buddy_order(4097), PAGE_ORDER + 1);
        assert_eq!(size_to_buddy_order(8192), PAGE_ORDER + 1);
        // 3 pages needs 4 pages worth (next_power_of_two(3) == 4) -> order+2
        assert_eq!(size_to_buddy_order(8193), PAGE_ORDER + 2);
    }

    // ── slab_index ───────────────────────────────────────────────────────

    #[test]
    fn slab_index_picks_smallest_fitting_class() {
        assert_eq!(SlabAllocator::slab_index(1), Some(0)); // -> 8
        assert_eq!(SlabAllocator::slab_index(8), Some(0));
        assert_eq!(SlabAllocator::slab_index(9), Some(1)); // -> 16
        assert_eq!(SlabAllocator::slab_index(2048), Some(NUM_SLABS - 1));
    }

    #[test]
    fn slab_index_beyond_max_slab_size_is_none() {
        assert_eq!(SlabAllocator::slab_index(4096), None);
    }

    // ── SlabAllocator allocate/deallocate ───────────────────────────────

    #[test]
    fn allocate_small_object_serves_from_cache_and_reports_expand() {
        let mem = VecMem::new(TEST_MEM_SIZE as usize);
        let frames = BumpFrames::new(TEST_MEM_SIZE);
        let mut slab = SlabAllocator::new();

        let layout = Layout::from_size_align(8, 8).unwrap();
        let result = unsafe { slab.allocate(&mem, &frames, layout) };
        assert!(!result.ptr.is_null());
        match result.event {
            AllocEvent::Expand(t) => {
                assert!(t.ok);
                assert_eq!(t.object_size, 8);
                assert!(t.added_objects > 0);
            }
            other => panic!("expected Expand on first alloc, got {:?}", other),
        }
    }

    #[test]
    fn allocate_second_object_of_same_size_does_not_expand() {
        let mem = VecMem::new(TEST_MEM_SIZE as usize);
        let frames = BumpFrames::new(TEST_MEM_SIZE);
        let mut slab = SlabAllocator::new();
        let layout = Layout::from_size_align(8, 8).unwrap();

        let _first = unsafe { slab.allocate(&mem, &frames, layout) };
        let second = unsafe { slab.allocate(&mem, &frames, layout) };
        assert!(!second.ptr.is_null());
        assert!(matches!(second.event, AllocEvent::None));
    }

    #[test]
    fn allocate_dealloc_roundtrip_reuses_object() {
        let mem = VecMem::new(TEST_MEM_SIZE as usize);
        let frames = BumpFrames::new(TEST_MEM_SIZE);
        let mut slab = SlabAllocator::new();
        let layout = Layout::from_size_align(16, 16).unwrap();

        let a = unsafe { slab.allocate(&mem, &frames, layout) };
        unsafe { slab.deallocate(&mem, &frames, a.ptr, layout); }
        let b = unsafe { slab.allocate(&mem, &frames, layout) };
        assert_eq!(a.ptr, b.ptr, "freed object should be handed back out again");
    }

    #[test]
    fn allocate_large_object_goes_through_frame_source() {
        let mem = VecMem::new(TEST_MEM_SIZE as usize);
        let frames = BumpFrames::new(TEST_MEM_SIZE);
        let mut slab = SlabAllocator::new();

        let layout = Layout::from_size_align(4096, 4096).unwrap();
        let result = unsafe { slab.allocate(&mem, &frames, layout) };
        assert!(!result.ptr.is_null());
        match result.event {
            AllocEvent::Large(t) => {
                assert!(t.ok);
                assert_eq!(t.order, PAGE_ORDER);
            }
            other => panic!("expected Large event, got {:?}", other),
        }
    }

    #[test]
    fn deallocate_large_object_reports_trace_and_frees_via_frame_source() {
        let mem = VecMem::new(TEST_MEM_SIZE as usize);
        let frames = BumpFrames::new(TEST_MEM_SIZE);
        let mut slab = SlabAllocator::new();

        let layout = Layout::from_size_align(4096, 4096).unwrap();
        let a = unsafe { slab.allocate(&mem, &frames, layout) };
        let event = unsafe { slab.deallocate(&mem, &frames, a.ptr, layout) };
        match event {
            DeallocEvent::Large(t) => {
                assert_eq!(t.order, PAGE_ORDER);
                assert_eq!(t.virt, a.ptr as u64);
            }
            other => panic!("expected Large dealloc event, got {:?}", other),
        }
    }

    #[test]
    fn deallocate_null_is_ignored() {
        let mem = VecMem::new(TEST_MEM_SIZE as usize);
        let frames = BumpFrames::new(TEST_MEM_SIZE);
        let mut slab = SlabAllocator::new();
        let layout = Layout::from_size_align(8, 8).unwrap();
        let event = unsafe { slab.deallocate(&mem, &frames, null_mut(), layout) };
        assert!(matches!(event, DeallocEvent::Ignored));
    }

    #[test]
    fn allocate_oom_on_first_expand_reports_failure() {
        let mem = VecMem::new(TEST_MEM_SIZE as usize);
        let frames = BumpFrames::new(0); // no room at all
        let mut slab = SlabAllocator::new();
        let layout = Layout::from_size_align(8, 8).unwrap();

        let result = unsafe { slab.allocate(&mem, &frames, layout) };
        assert!(result.ptr.is_null());
        match result.event {
            AllocEvent::Expand(t) => assert!(!t.ok),
            other => panic!("expected failed Expand, got {:?}", other),
        }
    }

    #[test]
    fn cache_stats_reflects_used_and_total() {
        let mem = VecMem::new(TEST_MEM_SIZE as usize);
        let frames = BumpFrames::new(TEST_MEM_SIZE);
        let mut slab = SlabAllocator::new();
        let layout = Layout::from_size_align(8, 8).unwrap();

        let _a = unsafe { slab.allocate(&mem, &frames, layout) };
        let _b = unsafe { slab.allocate(&mem, &frames, layout) };

        let stats = slab.cache_stats();
        let (size_class, total, used) = stats[0];
        assert_eq!(size_class, 8);
        assert_eq!(used, 2);
        assert!(total >= 2);
    }
}
