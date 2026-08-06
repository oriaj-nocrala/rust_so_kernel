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
// Redzone (debug-only, 2026-08-05 bug-2 hunt)
// ============================================================================
//
// Each slab object's SLOT is 2*object_size wide in debug builds: the
// caller-visible data region [slot, slot+object_size) followed by an
// object_size-byte trailing redzone [slot+object_size, slot+2*object_size).
//
// Why doubling, not a fixed-size redzone: the caches are power-of-2 object
// sizes and serve layouts with align up to object_size, so the object data
// pointers must stay object_size-aligned across the page. A fixed redzone
// would make the slot non-multiple of object_size and drift the alignment.
// A full object_size of redzone keeps every slot a multiple of object_size.
//
// Checked at `allocate` (the object must still be intact while it was free)
// and at `deallocate` (the caller must not have written past its data
// region) — the overflow-into-neighbor corruption mode this hunt suspects
// would trip it at the overflower's own free, before the neighbor is used.
//
// Pattern 0xC5: deliberately distinct from 0xAA (allocated poison) and 0xDD
// (freed poison) — 0xAA caused historical false positives in the UAF check,
// and 0xDD is the free-list invariant. 0xC5 collides with neither.
#[cfg(all(debug_assertions, feature = "slab-debug"))]
const REDZONE_PATTERN: u8 = 0xC5;

#[cfg(all(debug_assertions, feature = "slab-debug"))]
#[inline]
unsafe fn redzone_check(ptr: *mut u8, object_size: usize) {
    let rz = ptr.add(object_size);
    for i in 0..object_size {
        let found = rz.add(i).read();
        if found != REDZONE_PATTERN {
            panic!(
                "SLAB REDZONE BROKEN: obj={:#x} cache={} side=trailing offset={} found=0x{:02x} expected=0x{:02x}",
                ptr as u64, object_size, i, found, REDZONE_PATTERN
            );
        }
    }
}

#[cfg(all(debug_assertions, feature = "slab-debug"))]
#[inline]
unsafe fn redzone_set(ptr: *mut u8, object_size: usize) {
    core::ptr::write_bytes(ptr.add(object_size), REDZONE_PATTERN, object_size);
}

// ============================================================================
// Quarantine (debug-only, 2026-08-05 bug-2 hunt)
// ============================================================================
//
// Delayed reuse: a freed object does NOT go straight back onto the free list.
// It enters a per-cache FIFO quarantine and only becomes reusable after
// QUARANTINE_CAP more frees have pushed through (or the free list runs dry).
// While quarantined it keeps its full 0xDD poison, and on the way out it is
// verified WHOLE — any byte that is no longer 0xDD means a stale owner wrote
// into a freed object (use-after-free with recycling in between), which the
// redzone cannot catch (the write lands inside the recycled object's own data
// region) and the allocate-time UAF check cannot either (it only inspects at
// allocate, and here the write happened after reallocation).
//
// Discriminates the UAF-by-recycling hypothesis: if a stale owner is writing
// into recycled memory, the quarantine either (a) catches the poison break at
// drain, or (b) changes which object gets recycled, which moves the flake rate
// (the bug is layout-sensitive — the redzone already moved it ~8%→58%).
// Release builds reuse immediately (no quarantine struct, no code path change).
#[cfg(all(debug_assertions, feature = "slab-debug"))]
const QUARANTINE_CAP: usize = 128;

#[cfg(all(debug_assertions, feature = "slab-debug"))]
struct Quarantine {
    buf: [Option<NonNull<FreeObject>>; QUARANTINE_CAP],
    head: usize,
    len: usize,
}

#[cfg(all(debug_assertions, feature = "slab-debug"))]
impl Quarantine {
    const fn new() -> Self {
        Self {
            buf: [None; QUARANTINE_CAP],
            head: 0,
            len: 0,
        }
    }
    fn is_empty(&self) -> bool {
        self.len == 0
    }
    fn is_full(&self) -> bool {
        self.len == QUARANTINE_CAP
    }
    fn push(&mut self, obj: NonNull<FreeObject>) {
        let idx = (self.head + self.len) % QUARANTINE_CAP;
        self.buf[idx] = Some(obj);
        self.len += 1;
    }
    fn pop(&mut self) -> Option<NonNull<FreeObject>> {
        if self.len == 0 {
            return None;
        }
        let obj = self.buf[self.head].take();
        self.head = (self.head + 1) % QUARANTINE_CAP;
        self.len -= 1;
        obj
    }
}

/// Verify a quarantined object is still fully 0xDD-poisoned before it is
/// allowed back onto the free list. A break means a stale owner wrote into
/// freed memory — freeze the instant, dumping the object's first bytes (a
/// pointer / filename / counter in there usually identifies the writer).
#[cfg(all(debug_assertions, feature = "slab-debug"))]
unsafe fn verify_quarantined(obj: *mut u8, object_size: usize) {
    let region = object_size.min(256);
    for i in 0..region {
        let found = obj.add(i).read();
        if found != 0xDD {
            let n = region.min(32);
            let mut dump = [0u8; 32];
            for j in 0..n {
                dump[j] = obj.add(j).read();
            }
            panic!(
                "SLAB QUARANTINE VIOLATION: obj={:#x} cache={} offset={} found=0x{:02x} expected=0xDD bytes={:02x?}",
                obj as u64, object_size, i, found, &dump[..n]
            );
        }
    }
}


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
    #[cfg(all(debug_assertions, feature = "slab-debug"))]
    quarantine: Quarantine,
}

impl SlabCache {
    const fn new() -> Self {
        Self {
            free_list: None,
            total_objects: 0,
            used_objects: 0,
            #[cfg(all(debug_assertions, feature = "slab-debug"))]
            quarantine: Quarantine::new(),
        }
    }

    /// Allocate un objeto del slab. Returns the pointer plus an
    /// `ExpandTrace` if expansion was needed to serve this request (`None`
    /// if an already-free object was available).
    unsafe fn allocate(&mut self, mem: &dyn PhysMap, frames: &dyn FrameSource, object_size: usize) -> (*mut u8, Option<ExpandTrace>) {
        // Si no hay objetos libres, expandir el cache
        let mut expand_trace = None;
        if self.free_list.is_none() {
            #[cfg(all(debug_assertions, feature = "slab-debug"))]
            {
                // Quarantine: feed one drained (verified) object into the
                // free list before expanding — objects are only reused after
                // they have spent QUARANTINE_CAP frees in quarantine.
                if let Some(head) = self.quarantine.pop() {
                    verify_quarantined(head.as_ptr() as *mut u8, object_size);
                    let old_head = self.free_list;
                    head.as_ptr().write(FreeObject { next: old_head });
                    self.free_list = Some(head);
                }
            }
        }
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
            // ✅ Verificar que nadie escribió en el objeto mientras estaba
            // libre (use-after-free real). Los primeros
            // `size_of::<FreeObject>()` bytes contienen el puntero `next`
            // de la free list, escrito por `deallocate` encima del poison
            // 0xDD — no son poison y no se pueden comprobar contra nada.
            // Todo lo que queda del objeto sí debe seguir siendo 0xDD tal
            // como `deallocate` lo dejó; cualquier otro valor ahí significa
            // que algo escribió en memoria ya liberada. (Antes este chequeo
            // miraba esos mismos primeros bytes del puntero buscando 0xAA,
            // así que en realidad nunca miraba el poison — cualquier objeto
            // libre cuyo sucesor cayera en una dirección con un byte 0xAA
            // disparaba un panic falso, y un UAF real quedaba indetectable
            // porque el propio puntero `next` ya pisaba el poison.)
            let ptr = free_obj.as_ptr() as *mut u8;
            let ptr_size = core::mem::size_of::<FreeObject>();
            let poisoned_len = object_size.min(256);
            if poisoned_len > ptr_size {
                for i in ptr_size..poisoned_len {
                    let val = ptr.add(i).read();
                    if val != 0xDD {
                        panic!("Use-after-free detected at {:#x}", ptr as u64);
                    }
                }
            }
            // The object was free; its trailing redzone must still be intact.
            // A broken one means something wrote past an adjacent object into
            // this one's redzone while it sat on the free list — freeze that
            // instant instead of handing out a neighbour-corrupted object.
            #[cfg(all(debug_assertions, feature = "slab-debug"))]
            {
                redzone_check(ptr, object_size);
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
            // Debug poison for the always-on UAF check.
            core::ptr::write_bytes(ptr, 0xDD, object_size.min(256));
        }
        #[cfg(all(debug_assertions, feature = "slab-debug"))]
        {
            // The caller is done with the object: its trailing redzone must
            // be intact. A broken one means the caller (or a neighbour) wrote
            // past this object's data region — the overflow-into-neighbour
            // mode this hunt suspects. Freeze the instant.
            redzone_check(ptr, object_size);

            // Quarantine (feature-gated): delay reuse. When the quarantine is
            // full, the oldest object is drained first and verified WHOLE — a
            // stale owner's write into freed memory shows up here as a broken
            // 0xDD pattern.
            let obj = NonNull::new_unchecked(ptr as *mut FreeObject);
            if self.quarantine.is_full() {
                if let Some(head) = self.quarantine.pop() {
                    verify_quarantined(head.as_ptr() as *mut u8, object_size);
                    let old_head = self.free_list;
                    head.as_ptr().write(FreeObject { next: old_head });
                    self.free_list = Some(head);
                }
            }
            self.quarantine.push(obj);
            self.used_objects = self.used_objects.saturating_sub(1);
            return;
        }

        // Normal path (release, or debug without the slab-debug feature):
        // immediate free-list reuse.
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

        // Dividir la página en objetos. DEBUG: cada slot es 2*object_size
        // (objeto + redzone del mismo tamaño) — la alineación se preserva
        // porque el slot es múltiplo de object_size. Release: sin redzone.
        const PAGE_SIZE: usize = 4096;
        // DEBUG + feature: slot is 2*object_size (object + redzone). With the
        // feature off the layout is the reference (slot == object_size).
        let slot_size = if cfg!(all(debug_assertions, feature = "slab-debug")) {
            2 * object_size
        } else {
            object_size
        };
        let objects_per_page = PAGE_SIZE / slot_size;

        for i in 0..objects_per_page {
            let obj_ptr = page_ptr.add(i * slot_size) as *mut u8;

            #[cfg(debug_assertions)]
            {
                // ✅ Poison con el mismo patrón "freed" que `deallocate` usa,
                // para que el chequeo de UAF en `allocate` tenga un
                // invariante uniforme que verificar independientemente de
                // si el objeto viene de una página recién expandida o de
                // una liberación real: "todo objeto en la free list, más
                // allá de los bytes del puntero `next`, es 0xDD".
                core::ptr::write_bytes(obj_ptr, 0xDD, object_size.min(256));
                #[cfg(all(debug_assertions, feature = "slab-debug"))]
                {
                    // Redzone: el segundo object_size del slot.
                    redzone_set(obj_ptr, object_size);
                }
            }

            let obj_ptr = obj_ptr as *mut FreeObject;
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
        // With the slab-debug feature the quarantine delays reuse (b != a);
        // without it the freed object is reused immediately (b == a).
        assert_eq!(a.ptr != b.ptr, cfg!(feature = "slab-debug"),
            "quarantine must delay reuse iff the slab-debug feature is on");
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

    // ── Free-list integrity / corruption-mode tests (2026-08-05 bug-2 hunt) ──
    //
    // The buddy allocator has a property-test suite (buddy.rs::tests); the
    // slab does not. These close that gap for the three corruption modes the
    // hunt considers: a caller double-free, the free-list integrity under
    // correct usage, and the overflow-into-neighbor risk (there is no
    // redzone/canary between slab objects).

    #[test]
    #[should_panic(expected = "SLAB QUARANTINE VIOLATION")]
    #[cfg(feature = "slab-debug")]
    fn double_free_is_detected_by_the_uaf_check_in_debug() {        // `deallocate` pushes the object onto the free list unconditionally,
        // with no dedicated double-free detection. With the quarantine active,
        // the double-free's duplicate entry is caught when the second copy of
        // the object is drained from quarantine: the object was allocated once
        // (re-poisoned 0xAA) by the first hand-out, so the drain verification
        // finds non-0xDD bytes → "SLAB QUARANTINE VIOLATION". (The allocation-
        // time UAF check would also catch it were it reached first; either
        // way a double-free is a loud panic in debug — not the observed hang
        // nor the heap-jump page-fault panic.)
        let mem = VecMem::new(TEST_MEM_SIZE as usize);
        let frames = BumpFrames::new(TEST_MEM_SIZE);
        let mut slab = SlabAllocator::new();

        // Fill the cache's first page so the free list is empty and the next
        // allocate drains the quarantine deterministically.
        let mut objs = fill_first_page(&mut slab, &mem, &frames, 16);
        let a = objs.pop().unwrap();

        unsafe { slab.deallocate(&mem, &frames, a, layout16()); }
        unsafe { slab.deallocate(&mem, &frames, a, layout16()); } // double-free
        // First hand-out drains the first copy (still 0xDD)…
        let c = unsafe { slab.allocate(&mem, &frames, layout16()) };
        assert_eq!(c.ptr, a);
        // …second hand-out drains the duplicate: the object is now 0xAA → panic.
        let _ = unsafe { slab.allocate(&mem, &frames, layout16()) };
    }

    fn layout16() -> core::alloc::Layout {
        Layout::from_size_align(16, 16).unwrap()
    }

    #[test]
    #[cfg(feature = "slab-debug")]
    fn double_free_with_reallocation_in_between_does_not_reuse_twice() {
        // Supervisor's degradation scenario tested directly: free A,
        // reallocate (legitimately reusing A's slot, overwriting it with
        // valid data), free it AGAIN. In this free-list + quarantine
        // implementation, the reallocation consumes the quarantine entry, so
        // the second free adds the object exactly ONCE — two later allocates
        // yield different objects, not overlapping ones.
        let mem = VecMem::new(TEST_MEM_SIZE as usize);
        let frames = BumpFrames::new(TEST_MEM_SIZE);
        let mut slab = SlabAllocator::new();
        let layout = layout16();

        let mut objs = fill_first_page(&mut slab, &mem, &frames, 16);
        let a = objs.pop().unwrap();
        unsafe { slab.deallocate(&mem, &frames, a, layout); }

        // Reallocate (reuses a's slot), write valid data.
        let b = unsafe { slab.allocate(&mem, &frames, layout) };
        assert_eq!(b.ptr, a);
        unsafe { b.ptr.write_bytes(0x42, 16); }

        // Double-free: free b (== a) again.
        unsafe { slab.deallocate(&mem, &frames, b.ptr, layout); }

        // No overlap: the reallocation consumed the quarantine entry.
        let c = unsafe { slab.allocate(&mem, &frames, layout) };
        let d = unsafe { slab.allocate(&mem, &frames, layout) };
        assert_eq!(c.ptr, a);
        assert_ne!(c.ptr, d.ptr, "reallocation removed the first free entry — no overlap");
    }

    #[test]
    fn free_list_yields_unique_objects_under_correct_usage() {
        // Under correct (no double-free) usage, cycling a cache through
        // allocate-all / free-all / allocate-all must never hand out the same
        // object twice while it's live.
        let mem = VecMem::new(TEST_MEM_SIZE as usize);
        let frames = BumpFrames::new(TEST_MEM_SIZE);
        let mut slab = SlabAllocator::new();
        let layout = Layout::from_size_align(8, 8).unwrap();
        const N: usize = 256;

        let mut ptrs = std::vec::Vec::with_capacity(N);
        for _ in 0..N {
            let r = unsafe { slab.allocate(&mem, &frames, layout) };
            assert!(!r.ptr.is_null());
            ptrs.push(r.ptr as usize);
        }
        for &p in &ptrs {
            unsafe { slab.deallocate(&mem, &frames, p as *mut u8, layout); }
        }

        let mut seen = std::collections::HashSet::new();
        for _ in 0..N {
            let r = unsafe { slab.allocate(&mem, &frames, layout) };
            assert!(
                seen.insert(r.ptr as usize),
                "free list handed out a duplicate live object"
            );
        }
    }

    #[test]
    #[should_panic(expected = "SLAB REDZONE BROKEN")]
    #[cfg(feature = "slab-debug")]
    fn redzone_detects_overflow_past_object_end() {
        // The canary's core calibration: write one byte past the caller's
        // data region (into the trailing redzone) and verify the slab panics
        // at the object's deallocate — freezing the overflower instead of
        // letting it silently corrupt the neighbour.
        let mem = VecMem::new(TEST_MEM_SIZE as usize);
        let frames = BumpFrames::new(TEST_MEM_SIZE);
        let mut slab = SlabAllocator::new();
        let layout = Layout::from_size_align(16, 16).unwrap();

        let a = unsafe { slab.allocate(&mem, &frames, layout) };
        unsafe {
            // One byte past the 16-byte data region → the trailing redzone.
            a.ptr.add(16).write(0xAB);
        }
        // Deallocate checks the redzone first → "SLAB REDZONE BROKEN".
        unsafe { slab.deallocate(&mem, &frames, a.ptr, layout); }
    }

    #[test]
    #[should_panic(expected = "SLAB REDZONE BROKEN")]
    #[cfg(feature = "slab-debug")]
    fn redzone_detects_overflow_into_free_object_neighbour() {
        // A neighbour overflow lands in the next slot's data region; while
        // that next object is FREE, its trailing redzone is the first thing
        // an even larger overflow from the other side could hit, and the
        // allocate-time check must catch a broken redzone before handing the
        // corrupted object out.
        let mem = VecMem::new(TEST_MEM_SIZE as usize);
        let frames = BumpFrames::new(TEST_MEM_SIZE);
        let mut slab = SlabAllocator::new();
        let layout = layout16();

        // Fill the page so the free list is empty: the next allocate drains
        // the quarantined victim (free list empty), hitting its redzone check.
        let mut objs = fill_first_page(&mut slab, &mem, &frames, 16);
        let a = objs.pop().unwrap();
        unsafe { slab.deallocate(&mem, &frames, a, layout); }
        // a is free: data region [a, a+16), trailing redzone [a+16, a+32).
        // Break the redzone as an external overflow would.
        unsafe { a.add(16).write(0xAB); }
        // Allocating must drain a and trip the allocate-time redzone check.
        let _ = unsafe { slab.allocate(&mem, &frames, layout) };
    }

    // ── Quarantine calibration (2026-08-05 bug-2 hunt) ───────────────────────

    /// Allocate every object in the cache's first page (leaving the free list
    /// empty, so the next allocate drains the quarantine or expands). Returns
    /// the pointers. The per-page count accounts for the debug redzone
    /// (slot = 2*obj_size).
    fn fill_first_page(
        slab: &mut SlabAllocator,
        mem: &VecMem,
        frames: &BumpFrames,
        obj_size: usize,
    ) -> std::vec::Vec<*mut u8> {
        let layout = Layout::from_size_align(obj_size, obj_size).unwrap();
        let slot = if cfg!(feature = "slab-debug") { 2 * obj_size } else { obj_size };
        let per_page = 4096 / slot;
        let mut objs = std::vec::Vec::with_capacity(per_page);
        for _ in 0..per_page {
            let r = unsafe { slab.allocate(mem, frames, layout) };
            assert!(!r.ptr.is_null());
            objs.push(r.ptr);
        }
        objs
    }

    #[test]
    #[cfg(feature = "slab-debug")]
    fn quarantine_delays_reuse_until_drain() {
        // A freed object must NOT be handed back out immediately — it sits in
        // quarantine until it has aged QUARANTINE_CAP frees (or the free list
        // runs dry). This is what makes a stale owner's write into freed
        // memory detectable.
        let mem = VecMem::new(TEST_MEM_SIZE as usize);
        let frames = BumpFrames::new(TEST_MEM_SIZE);
        let mut slab = SlabAllocator::new();
        let layout = Layout::from_size_align(8, 8).unwrap();

        let a = unsafe { slab.allocate(&mem, &frames, layout) };
        unsafe { slab.deallocate(&mem, &frames, a.ptr, layout); }
        // Immediately reallocating must NOT return the just-freed object.
        let b = unsafe { slab.allocate(&mem, &frames, layout) };
        assert!(!b.ptr.is_null());
        assert_ne!(a.ptr, b.ptr, "quarantine must delay reuse");
    }

    #[test]
    #[should_panic(expected = "SLAB QUARANTINE VIOLATION")]
    #[cfg(feature = "slab-debug")]
    fn quarantine_detects_stale_owner_write_into_freed_object() {
        // The UAF discriminator: free an object, then have a stale owner
        // write into it (the write lands inside the recycled object's own
        // data region — invisible to the redzone and to the allocate-time UAF
        // check). When the object ages out of quarantine, the whole-object
        // 0xDD verification finds the foreign byte → panic.
        let mem = VecMem::new(TEST_MEM_SIZE as usize);
        let frames = BumpFrames::new(TEST_MEM_SIZE);
        let mut slab = SlabAllocator::new();
        let layout = Layout::from_size_align(8, 8).unwrap();

        // QUARANTINE_CAP + 1 objects: the victim plus enough churn to push it
        // through the quarantine (the 8-cache page holds 256 slots here).
        let mut objs = std::vec::Vec::new();
        for _ in 0..(QUARANTINE_CAP + 1) {
            let r = unsafe { slab.allocate(&mem, &frames, layout) };
            assert!(!r.ptr.is_null());
            objs.push(r.ptr);
        }

        // Free the victim, then write into it as a stale owner would.
        let victim = objs.pop().unwrap();
        unsafe { slab.deallocate(&mem, &frames, victim, layout); }
        unsafe { victim.add(4).write(0x41); } // 'A' — a stale owner's data

        // Free QUARANTINE_CAP more objects: the last one fills the quarantine
        // and drains the victim (the head) → verification finds 0x41 → panic.
        for o in objs {
            unsafe { slab.deallocate(&mem, &frames, o, layout); }
        }
    }

    #[test]
    #[cfg(feature = "slab-debug")]
    fn quarantine_reuses_drained_object_after_churn() {
        // After enough churn, the freed object ages out of quarantine and IS
        // reused — the quarantine delays, it does not leak. The victim must
        // come back (as some allocate's result) once drained.
        let mem = VecMem::new(TEST_MEM_SIZE as usize);
        let frames = BumpFrames::new(TEST_MEM_SIZE);
        let mut slab = SlabAllocator::new();
        let layout = Layout::from_size_align(8, 8).unwrap();

        let mut objs = std::vec::Vec::new();
        for _ in 0..(QUARANTINE_CAP + 1) {
            let r = unsafe { slab.allocate(&mem, &frames, layout) };
            objs.push(r.ptr);
        }
        let victim = objs.pop().unwrap();
        unsafe { slab.deallocate(&mem, &frames, victim, layout); }
        // Free everything else; then allocate QUARANTINE_CAP more. The victim
        // must appear among the reallocated pointers once it drains.
        for o in objs {
            unsafe { slab.deallocate(&mem, &frames, o, layout); }
        }
        let mut saw_victim = false;
        for _ in 0..QUARANTINE_CAP {
            let r = unsafe { slab.allocate(&mem, &frames, layout) };
            assert!(!r.ptr.is_null());
            if r.ptr == victim {
                saw_victim = true;
            }
        }
        assert!(saw_victim, "drained object must be reused");
    }
}
