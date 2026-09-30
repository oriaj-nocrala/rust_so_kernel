//! `nvgpu::devmodel` — the bookkeeping behind `/dev/nvgpu` (interface: `nvgpu/uapi/nvgpu.h`, plan: `docs/gpu/g4-nvkmd-plan.md`).
//!
//! Everything a caller can get wrong is checked here, on plain data, so it runs under `cargo test`: buffer objects, the one GPU
//! virtual address space (allocated ranges and bindings, partial unbinds), contexts, timeline synchronisation objects and the
//! validation of an `EXEC`. The effects (pages of memory, GPU page tables, queuing pushes, reading a fence) go through the
//! [`Backend`] seam: [`SoftBackend`] for tests and for the software device, the hardware one in `kernel/src/gpu/`.
//!
//! Nothing here blocks: waiting on a timeline is the adapter's loop around [`Device::waits_ready`] and [`Device::poll`].

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::uapi::{self, Push, SyncRef};

pub const PAGE: u64 = 4096;
/// At most this many contexts at once (each will be a GPU channel).
pub const MAX_CTX: usize = 16;
/// At most this many pushes and sync references in one `EXEC`.
pub const MAX_PUSHES: usize = 256;
pub const MAX_SYNC_REFS: usize = 64;

/// The failures an ioctl reports; the adapter maps them to negative errno values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// `EINVAL`: a malformed request.
    Inval,
    /// `ENOENT`: no such handle.
    NoEnt,
    /// `ENOMEM`: the backing memory ran out.
    NoMem,
    /// `ENOSPC`: no GPU virtual address range or context slot left.
    NoSpc,
    /// `EBUSY`: still in use (a VA range with bindings).
    Busy,
    /// `EEXIST`: the range is already bound.
    Exist,
    /// `EFAULT`: a push that is not entirely inside bound memory.
    Fault,
    /// `EAGAIN`: the device has no room for this right now (a full ring); nothing changed, try again after a fence completes.
    Again,
    /// `EIO`: the GPU stopped answering (a channel that faulted or hung); the session cannot run more work.
    Io,
}

// ---- range allocator ------------------------------------------------------------------------------------------------------

/// First-fit allocator of ranges of a `u64` space, with coalescing on free. Used for the GPU virtual addresses, the arena offsets of
/// system BOs and the VRAM heap.
#[derive(Debug, Clone)]
pub struct RangeAlloc {
    /// free ranges: start -> length, never adjacent, never overlapping
    free: BTreeMap<u64, u64>,
    total: u64,
    used: u64,
}

impl RangeAlloc {
    pub fn new(start: u64, end: u64) -> Self {
        let mut free = BTreeMap::new();
        if end > start {
            free.insert(start, end - start);
        }
        RangeAlloc { free, total: end.saturating_sub(start), used: 0 }
    }

    /// `align` must be a power of two.
    pub fn alloc(&mut self, size: u64, align: u64) -> Option<u64> {
        if size == 0 || !align.is_power_of_two() {
            return None;
        }
        let found = self.free.iter().find_map(|(&s, &l)| {
            let a = s.checked_add(align - 1)? & !(align - 1);
            let end = a.checked_add(size)?;
            (end <= s + l).then_some((s, l, a))
        })?;
        let (s, l, a) = found;
        self.free.remove(&s);
        if a > s {
            self.free.insert(s, a - s);
        }
        let end = a + size;
        if end < s + l {
            self.free.insert(end, s + l - end);
        }
        self.used += size;
        Some(a)
    }

    /// Return `[start, start + size)`, which must have been allocated (a double free panics: it is a bookkeeping bug).
    pub fn free(&mut self, start: u64, size: u64) {
        let end = start + size;
        if let Some((&s, &l)) = self.free.range(..end).next_back() {
            assert!(s + l <= start, "RangeAlloc: freeing memory that is already free");
        }
        if let Some((&s, _)) = self.free.range(start..).next() {
            assert!(s >= end, "RangeAlloc: freeing memory that is already free");
        }
        let mut new_start = start;
        let mut new_len = size;
        if let Some((&s, &l)) = self.free.range(..start).next_back() {
            if s + l == start {
                self.free.remove(&s);
                new_start = s;
                new_len += l;
            }
        }
        if let Some(&l) = self.free.get(&end) {
            self.free.remove(&end);
            new_len += l;
        }
        self.free.insert(new_start, new_len);
        self.used -= size;
    }

    pub fn used(&self) -> u64 {
        self.used
    }

    pub fn total(&self) -> u64 {
        self.total
    }
}

// ---- backend seam ---------------------------------------------------------------------------------------------------------

/// The two kinds of storage a buffer object can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Heap {
    System,
    Vram,
}

/// What a buffer object's storage is, as the backend needs to find it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backing {
    /// Pages of system RAM at this offset (in bytes) of the device's arena object.
    System { arena_off: u64 },
    /// Device memory at this offset (in bytes) of the VRAM heap.
    Vram { vram_off: u64 },
}

/// The effects of the device. Every method runs with the model's state already validated.
pub trait Backend {
    /// Reserve the storage of a BO of `size` bytes; the model has already chosen where (`backing`).
    fn bo_create(&mut self, backing: Backing, size: u64) -> Result<(), Error>;
    /// The BO is gone and unbound everywhere: give the storage back.
    fn bo_release(&mut self, backing: Backing, size: u64);
    /// Write GPU page-table entries: `size` bytes at `va` map `bo_off..` of `backing`.
    fn bind(&mut self, va: u64, size: u64, backing: Backing, bo_off: u64, pte_kind: u32) -> Result<(), Error>;
    fn unbind(&mut self, va: u64, size: u64);
    fn ctx_create(&mut self, ctx: u32, engines: u32) -> Result<(), Error>;
    fn ctx_destroy(&mut self, ctx: u32);
    /// Queue `pushes` on `ctx`; returns the fence sequence number that completes when they have run.
    fn submit(&mut self, ctx: u32, pushes: &[Push]) -> Result<u64, Error>;
    /// Whether fence `seq` of `ctx` has completed.
    fn fence_done(&mut self, ctx: u32, seq: u64) -> bool;
    /// The device is closing: wait for the work in flight to finish. `false` when it did not (a hung GPU), which means the memory
    /// it may still touch must not be reused. The software device has nothing in flight.
    fn quiesce(&mut self) -> bool {
        true
    }
    /// Whether the two heaps (system memory arena offsets, VRAM offsets) are shared with other devices (several sessions on one GPU):
    /// then [`heap_alloc`](Self::heap_alloc), [`heap_hold`](Self::heap_hold) and [`heap_free`](Self::heap_free) decide where a BO's storage
    /// is and how long it lives, and the model's own heaps (`Layout::arena_bytes`, `vram_bytes`) are not used. Buffer sharing
    /// ([`Device::bo_import`]) needs it.
    fn shared_heaps(&self) -> bool {
        false
    }
    /// `size` bytes of `heap` (page aligned), as an offset into it, with one reference held by the caller; `None` when it is full.
    fn heap_alloc(&mut self, _heap: Heap, _size: u64) -> Option<u64> {
        None
    }
    /// Another holder of the storage that starts at `off` (an import, an export): it lives until every holder has called
    /// [`heap_free`](Self::heap_free).
    fn heap_hold(&mut self, _heap: Heap, _off: u64) {}
    /// One holder lets go of the storage `[off, off + size)`; the last one gives it back.
    fn heap_free(&mut self, _heap: Heap, _off: u64, _size: u64) {}

    // ---- timelines shared between devices ([`Device::sync_share`]). A shared timeline lives in the backend, not in one device: its value
    // moves as the fences of the work that signals it complete, and whichever device reads it finds out, so the device that queued the
    // work need not be calling in. The defaults refuse (no sharing).

    /// A shared timeline with this `value` and `pending` (the highest value queued work will signal), one holder (the creator's). `None`: the
    /// backend cannot share timelines.
    fn shared_sync_create(&mut self, _value: u64, _pending: u64) -> Option<u64> {
        None
    }
    /// Another holder of shared timeline `id`; `false` if there is no such timeline.
    fn shared_sync_hold(&mut self, _id: u64) -> bool {
        false
    }
    /// One holder lets go; the last one frees the timeline.
    fn shared_sync_release(&mut self, _id: u64) {}
    /// `(value, pending)` of shared timeline `id`, after applying every queued signal whose fence has completed.
    fn shared_sync_value(&mut self, _id: u64) -> (u64, u64) {
        (0, 0)
    }
    /// A CPU signal: `value` and `pending` rise to at least `value`.
    fn shared_sync_signal(&mut self, _id: u64, _value: u64) {}
    /// Work submitted on `ctx` (fence `seq`, as [`submit`](Self::submit) returned it) will set shared timeline `id` to `value` when it has run.
    fn shared_sync_queue(&mut self, _id: u64, _ctx: u32, _seq: u64, _value: u64) {}
}

/// A backend with no hardware behind it: it accepts everything the model validated and completes work when told to.
#[derive(Debug, Default)]
pub struct SoftBackend {
    /// Every submission, in order: (ctx, its pushes).
    pub log: Vec<(u32, Vec<Push>)>,
    seq: BTreeMap<u32, u64>,
    done: BTreeMap<u32, u64>,
    /// When true, submissions stay pending until [`complete_all`](Self::complete_all) (tests of the asynchronous path).
    pub hold: bool,
    /// Live page-table entries: (va, size).
    pub bound: Vec<(u64, u64)>,
    /// The same entries with what they map: (va, size, backing, offset into it).
    pub mapped: Vec<(u64, u64, Backing, u64)>,
    /// How many page-table writes were asked for (binds plus unbinds): a cut should not rewrite what it does not touch.
    pub table_ops: usize,
    pub live_bos: usize,
}

impl SoftBackend {
    pub fn complete_all(&mut self) {
        for (ctx, s) in &self.seq {
            self.done.insert(*ctx, *s);
        }
    }
}

impl Backend for SoftBackend {
    fn bo_create(&mut self, _backing: Backing, _size: u64) -> Result<(), Error> {
        self.live_bos += 1;
        Ok(())
    }

    fn bo_release(&mut self, _backing: Backing, _size: u64) {
        self.live_bos -= 1;
    }

    fn bind(&mut self, va: u64, size: u64, backing: Backing, bo_off: u64, _pte_kind: u32) -> Result<(), Error> {
        self.bound.push((va, size));
        self.mapped.push((va, size, backing, bo_off));
        self.table_ops += 1;
        Ok(())
    }

    fn unbind(&mut self, va: u64, size: u64) {
        let i = self.bound.iter().position(|&b| b == (va, size)).expect("unbind of a range that was bound as such");
        self.bound.swap_remove(i);
        let j = self.mapped.iter().position(|m| (m.0, m.1) == (va, size)).expect("mapped and bound agree");
        self.mapped.swap_remove(j);
        self.table_ops += 1;
    }

    fn ctx_create(&mut self, ctx: u32, _engines: u32) -> Result<(), Error> {
        self.seq.insert(ctx, 0);
        self.done.insert(ctx, 0);
        Ok(())
    }

    fn ctx_destroy(&mut self, ctx: u32) {
        self.seq.remove(&ctx);
        self.done.remove(&ctx);
    }

    fn submit(&mut self, ctx: u32, pushes: &[Push]) -> Result<u64, Error> {
        let s = self.seq.get_mut(&ctx).expect("submit to a context the model should have refused");
        *s += 1;
        let seq = *s;
        self.log.push((ctx, pushes.to_vec()));
        if !self.hold {
            self.done.insert(ctx, seq);
        }
        Ok(seq)
    }

    fn fence_done(&mut self, ctx: u32, seq: u64) -> bool {
        self.done.get(&ctx).is_some_and(|&d| d >= seq)
    }
}

// ---- the device -----------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Bo {
    size: u64,
    backing: Backing,
    /// Bindings that reference this BO.
    binds: u32,
    /// `BO_FREE` was called: the handle is dead, the storage is released when the last binding goes.
    closed: bool,
}

#[derive(Debug, Clone, Copy)]
struct Binding {
    len: u64,
    bo: u32,
    bo_off: u64,
    pte_kind: u32,
}

/// A timeline: `value` has completed, `pending` is the highest value work already queued will signal (`pending >= value`).
#[derive(Debug, Clone, Copy)]
struct Timeline {
    value: u64,
    pending: u64,
    /// The backend's shared timeline this one stands for (`value` and `pending` are then the backend's, not these).
    shared: Option<u64>,
}

#[derive(Debug)]
struct Pending {
    ctx: u32,
    seq: u64,
    signals: Vec<SyncRef>,
}

/// Where the model puts things.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    /// Bytes of arena offset space for system BOs.
    pub arena_bytes: u64,
    /// Bytes of VRAM the heap hands out to VRAM BOs (0: none).
    pub vram_bytes: u64,
    pub va_start: u64,
    pub va_end: u64,
}

pub struct Device<B: Backend> {
    pub backend: B,
    layout: Layout,
    bos: BTreeMap<u32, Bo>,
    next_bo: u32,
    arena: RangeAlloc,
    vram: RangeAlloc,
    /// Bytes of each of the backend's shared heaps this device holds, indexed by [`Heap`] (unused when the heaps are the model's own).
    held: [u64; 2],
    va: RangeAlloc,
    /// allocated VA ranges: start -> length
    va_allocs: BTreeMap<u64, u64>,
    /// bound ranges: start -> binding (never overlapping)
    bound: BTreeMap<u64, Binding>,
    ctxs: BTreeMap<u32, u32>,
    next_ctx: u32,
    syncs: BTreeMap<u32, Timeline>,
    next_sync: u32,
    pending: Vec<Pending>,
}

fn round_up(v: u64) -> Option<u64> {
    v.checked_add(PAGE - 1).map(|x| x & !(PAGE - 1))
}

impl<B: Backend> Device<B> {
    pub fn new(backend: B, layout: Layout) -> Self {
        Device {
            backend,
            layout,
            bos: BTreeMap::new(),
            next_bo: 1,
            arena: RangeAlloc::new(0, layout.arena_bytes),
            vram: RangeAlloc::new(0, layout.vram_bytes),
            held: [0; 2],
            va: RangeAlloc::new(layout.va_start, layout.va_end),
            va_allocs: BTreeMap::new(),
            bound: BTreeMap::new(),
            ctxs: BTreeMap::new(),
            next_ctx: 1,
            syncs: BTreeMap::new(),
            next_sync: 1,
            pending: Vec::new(),
        }
    }

    /// VRAM this device holds in BOs.
    pub fn vram_used(&self) -> u64 {
        if self.backend.shared_heaps() {
            self.held[Heap::Vram as usize]
        } else {
            self.vram.used()
        }
    }

    // ---- buffer objects ---------------------------------------------------------------------------------------------------

    /// Returns `(handle, mmap offset or u64::MAX for VRAM, size reserved)`.
    pub fn bo_create(&mut self, size: u64, flags: u32) -> Result<(u32, u64, u64), Error> {
        if size == 0 {
            return Err(Error::Inval);
        }
        let size = round_up(size).ok_or(Error::Inval)?;
        let (backing, mmap) = match flags {
            uapi::BO_SYSTEM => {
                let off = if self.backend.shared_heaps() {
                    let off = self.backend.heap_alloc(Heap::System, size).ok_or(Error::NoMem)?;
                    self.held[Heap::System as usize] += size;
                    off
                } else {
                    self.arena.alloc(size, PAGE).ok_or(Error::NoMem)?
                };
                (Backing::System { arena_off: off }, off)
            }
            uapi::BO_VRAM => {
                let off = if self.backend.shared_heaps() {
                    let off = self.backend.heap_alloc(Heap::Vram, size).ok_or(Error::NoMem)?;
                    self.held[Heap::Vram as usize] += size;
                    off
                } else {
                    self.vram.alloc(size, PAGE).ok_or(Error::NoMem)?
                };
                (Backing::Vram { vram_off: off }, u64::MAX)
            }
            _ => return Err(Error::Inval),
        };
        if let Err(e) = self.backend.bo_create(backing, size) {
            self.release_range(backing, size);
            return Err(e);
        }
        let mut handle = self.next_bo;
        // closed-but-still-bound BOs stay in the table, so this also keeps handles unique while the GPU may still use them
        while handle == 0 || self.bos.contains_key(&handle) {
            handle = handle.wrapping_add(1);
        }
        self.next_bo = handle.wrapping_add(1);
        self.bos.insert(handle, Bo { size, backing, binds: 0, closed: false });
        Ok((handle, mmap, size))
    }

    fn release_range(&mut self, backing: Backing, size: u64) {
        let (heap, off) = match backing {
            Backing::System { arena_off } => (Heap::System, arena_off),
            Backing::Vram { vram_off } => (Heap::Vram, vram_off),
        };
        if self.backend.shared_heaps() {
            self.backend.heap_free(heap, off, size);
            self.held[heap as usize] -= size;
        } else if heap == Heap::System {
            self.arena.free(off, size);
        } else {
            self.vram.free(off, size);
        }
    }

    /// Take a BO another device made (its `backing` and `size`, as [`bo_backing`](Self::bo_backing) said) into this one, under a handle of
    /// this device's own. Only on shared heaps. The storage stays until every device that holds it has released it, so the exporter may
    /// close its handle, or its whole session, while this device still uses the memory. Returns what `bo_create` does.
    pub fn bo_import(&mut self, backing: Backing, size: u64) -> Result<(u32, u64, u64), Error> {
        if !self.backend.shared_heaps() || size == 0 || size % PAGE != 0 {
            return Err(Error::Inval);
        }
        let (heap, off, mmap) = match backing {
            Backing::System { arena_off } => (Heap::System, arena_off, arena_off),
            Backing::Vram { vram_off } => (Heap::Vram, vram_off, u64::MAX),
        };
        self.backend.heap_hold(heap, off);
        if let Err(e) = self.backend.bo_create(backing, size) {
            self.backend.heap_free(heap, off, size);
            return Err(e);
        }
        self.held[heap as usize] += size;
        let mut handle = self.next_bo;
        while handle == 0 || self.bos.contains_key(&handle) {
            handle = handle.wrapping_add(1);
        }
        self.next_bo = handle.wrapping_add(1);
        self.bos.insert(handle, Bo { size, backing, binds: 0, closed: false });
        Ok((handle, mmap, size))
    }

    fn bo(&self, handle: u32) -> Result<&Bo, Error> {
        match self.bos.get(&handle) {
            Some(b) if !b.closed => Ok(b),
            _ => Err(Error::NoEnt),
        }
    }

    /// Close a handle. If the BO is still bound somewhere the storage stays until the last binding is removed (the GPU may still be
    /// reading it), but the handle is dead at once.
    pub fn bo_free(&mut self, handle: u32) -> Result<(), Error> {
        let b = self.bos.get_mut(&handle).filter(|b| !b.closed).ok_or(Error::NoEnt)?;
        b.closed = true;
        if b.binds == 0 {
            self.drop_bo(handle);
        }
        Ok(())
    }

    fn drop_bo(&mut self, handle: u32) {
        let b = self.bos.remove(&handle).expect("drop of a BO in the table");
        self.backend.bo_release(b.backing, b.size);
        self.release_range(b.backing, b.size);
    }

    /// Where a live BO's storage is (for the adapter: pages to map, VRAM offset).
    pub fn bo_backing(&self, handle: u32) -> Result<(Backing, u64), Error> {
        let b = self.bo(handle)?;
        Ok((b.backing, b.size))
    }

    // ---- GPU virtual address space ---------------------------------------------------------------------------------------

    pub fn va_alloc(&mut self, size: u64, align: u64) -> Result<u64, Error> {
        if size == 0 || align < PAGE || !align.is_power_of_two() {
            return Err(Error::Inval);
        }
        let size = round_up(size).ok_or(Error::Inval)?;
        let va = self.va.alloc(size, align).ok_or(Error::NoSpc)?;
        self.va_allocs.insert(va, size);
        Ok(va)
    }

    /// Free an allocated range as a whole: `va` and `size` (rounded up) must be what `va_alloc` returned/reserved.
    pub fn va_free(&mut self, va: u64, size: u64) -> Result<(), Error> {
        let size = round_up(size).ok_or(Error::Inval)?;
        match self.va_allocs.get(&va) {
            Some(&l) if l == size => {}
            Some(_) | None => return Err(Error::NoEnt),
        }
        // a binding lies inside one allocation, so none can start before this range and reach into it
        if self.bound.range(va..va + size).next().is_some() {
            return Err(Error::Busy);
        }
        self.va_allocs.remove(&va);
        self.va.free(va, size);
        Ok(())
    }

    fn bound_before(&self, va: u64) -> Option<(u64, Binding)> {
        self.bound.range(..va).next_back().map(|(&s, &b)| (s, b))
    }

    fn inside_allocation(&self, va: u64, size: u64) -> bool {
        match self.va_allocs.range(..=va).next_back() {
            Some((&s, &l)) => va.checked_add(size).is_some_and(|e| e <= s + l),
            None => false,
        }
    }

    pub fn va_bind(&mut self, b: &uapi::VaBind) -> Result<(), Error> {
        let (va, size, bo_off) = (b.va, b.size, b.bo_offset);
        if size == 0 || va % PAGE != 0 || size % PAGE != 0 || bo_off % PAGE != 0 {
            return Err(Error::Inval);
        }
        let bo = *self.bo(b.handle)?;
        if bo_off.checked_add(size).is_none_or(|e| e > bo.size) {
            return Err(Error::Inval);
        }
        if !self.inside_allocation(va, size) {
            return Err(Error::Inval);
        }
        let end = va + size;
        if self.bound.range(va..end).next().is_some() || self.bound_before(va).is_some_and(|(s, x)| s + x.len > va) {
            return Err(Error::Exist);
        }
        let backing = match bo.backing {
            Backing::System { arena_off } => Backing::System { arena_off },
            Backing::Vram { vram_off } => Backing::Vram { vram_off },
        };
        self.backend.bind(va, size, backing, bo_off, b.pte_kind)?;
        self.bound.insert(va, Binding { len: size, bo: b.handle, bo_off, pte_kind: b.pte_kind });
        self.bos.get_mut(&b.handle).unwrap().binds += 1;
        Ok(())
    }

    /// Unmap `[va, va + size)`; bindings that overlap it are cut, gaps are ignored.
    pub fn va_unbind(&mut self, va: u64, size: u64) -> Result<(), Error> {
        if size == 0 || va % PAGE != 0 || size % PAGE != 0 || va.checked_add(size).is_none() {
            return Err(Error::Inval);
        }
        let end = va + size;
        let mut hits: Vec<u64> = self.bound.range(va..end).map(|(&s, _)| s).collect();
        if let Some((s, b)) = self.bound_before(va) {
            if s + b.len > va {
                hits.push(s);
            }
        }
        for s in hits {
            let b = self.bound.remove(&s).expect("a binding found a moment ago");
            let (b_start, b_end) = (s, s + b.len);
            let cut_start = b_start.max(va);
            let cut_end = b_end.min(end);
            self.backend.unbind(b_start, b.len);
            // the pieces that survive are bound again, as new page-table writes
            let mut survivors = 0;
            if b_start < cut_start {
                let len = cut_start - b_start;
                self.rebind(b_start, len, b.bo, b.bo_off, b.pte_kind);
                survivors += 1;
            }
            if cut_end < b_end {
                let len = b_end - cut_end;
                self.rebind(cut_end, len, b.bo, b.bo_off + (cut_end - b_start), b.pte_kind);
                survivors += 1;
            }
            let bo = self.bos.get_mut(&b.bo).expect("a bound BO is in the table");
            bo.binds = bo.binds - 1 + survivors;
            if bo.closed && bo.binds == 0 {
                self.drop_bo(b.bo);
            }
        }
        Ok(())
    }

    fn rebind(&mut self, va: u64, len: u64, bo: u32, bo_off: u64, pte_kind: u32) {
        let backing = self.bos[&bo].backing;
        // Re-writing entries that were valid a moment ago: the backend cannot refuse what it accepted.
        self.backend.bind(va, len, backing, bo_off, pte_kind).expect("rebinding a piece of a range that was bound");
        self.bound.insert(va, Binding { len, bo, bo_off, pte_kind });
    }

    /// Whether `[va, va + size)` is entirely covered by bound memory (possibly by several adjacent bindings).
    fn covered(&self, va: u64, size: u64) -> bool {
        let Some(end) = va.checked_add(size) else { return false };
        let mut at = va;
        while at < end {
            let Some((s, b)) = self.bound.range(..=at).next_back().map(|(&s, &b)| (s, b)) else { return false };
            let e = s + b.len;
            if e <= at {
                return false;
            }
            at = e;
        }
        true
    }

    // ---- contexts ---------------------------------------------------------------------------------------------------------

    pub fn ctx_create(&mut self, engines: u32) -> Result<u32, Error> {
        if engines == 0 || engines & !uapi::ENGINE_ALL != 0 {
            return Err(Error::Inval);
        }
        if self.ctxs.len() >= MAX_CTX {
            return Err(Error::NoSpc);
        }
        let mut id = self.next_ctx;
        while self.ctxs.contains_key(&id) || id == 0 {
            id = id.wrapping_add(1);
        }
        self.backend.ctx_create(id, engines)?;
        self.ctxs.insert(id, engines);
        self.next_ctx = id.wrapping_add(1);
        Ok(id)
    }

    pub fn ctx_destroy(&mut self, ctx: u32) -> Result<(), Error> {
        if self.ctxs.remove(&ctx).is_none() {
            return Err(Error::NoEnt);
        }
        // Work already queued still signals its timelines: complete it first, then forget the context.
        self.backend.ctx_destroy(ctx);
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].ctx == ctx {
                let p = self.pending.remove(i);
                for s in p.signals {
                    self.advance(s);
                }
            } else {
                i += 1;
            }
        }
        Ok(())
    }

    // ---- timelines --------------------------------------------------------------------------------------------------------

    pub fn sync_create(&mut self, initial: u64) -> Result<u32, Error> {
        if self.syncs.len() >= 1 << 16 {
            return Err(Error::NoSpc);
        }
        let mut h = self.next_sync;
        while self.syncs.contains_key(&h) || h == 0 {
            h = h.wrapping_add(1);
        }
        self.syncs.insert(h, Timeline { value: initial, pending: initial, shared: None });
        self.next_sync = h.wrapping_add(1);
        Ok(h)
    }

    pub fn sync_destroy(&mut self, handle: u32) -> Result<(), Error> {
        let t = self.syncs.remove(&handle).ok_or(Error::NoEnt)?;
        if let Some(id) = t.shared {
            self.backend.shared_sync_release(id);
        }
        Ok(())
    }

    /// The timeline's `(value, pending)`: its own, or the backend's when it is shared.
    fn tl(&mut self, handle: u32) -> Result<(u64, u64), Error> {
        let t = *self.syncs.get(&handle).ok_or(Error::NoEnt)?;
        Ok(match t.shared {
            Some(id) => self.backend.shared_sync_value(id),
            None => (t.value, t.pending),
        })
    }

    /// CPU signal: the value must not go backwards (Vulkan timeline semantics).
    pub fn sync_signal(&mut self, handle: u32, value: u64) -> Result<(), Error> {
        let (cur, _) = self.tl(handle)?;
        if value < cur {
            return Err(Error::Inval);
        }
        let t = self.syncs.get_mut(&handle).ok_or(Error::NoEnt)?;
        match t.shared {
            Some(id) => self.backend.shared_sync_signal(id, value),
            None => {
                t.value = value;
                t.pending = t.pending.max(value);
            }
        }
        Ok(())
    }

    /// `(value, pending)`.
    pub fn sync_query(&mut self, handle: u32) -> Result<(u64, u64), Error> {
        self.poll();
        self.tl(handle)
    }

    /// Make the timeline shareable (it stays this device's handle, now backed by a timeline the backend keeps) and take one more hold on it
    /// for the caller, who turns it into a descriptor. Returns the shared id; asking again for the same timeline returns the same id.
    /// `Inval` when the backend cannot share timelines.
    pub fn sync_share(&mut self, handle: u32) -> Result<u64, Error> {
        let t = *self.syncs.get(&handle).ok_or(Error::NoEnt)?;
        let id = match t.shared {
            Some(id) => id,
            None => {
                let id = self.backend.shared_sync_create(t.value, t.pending).ok_or(Error::Inval)?;
                self.syncs.get_mut(&handle).expect("the timeline was just read").shared = Some(id);
                // Work already queued against it (the usual order: submit, then export) is the backend's to resolve from now on, for whoever
                // reads the timeline. Left here it would be applied only when this device next calls in, and a device that queued its
                // work and went idle would never complete it for anyone else. The entry stays (`busy` counts it) without the signal.
                for p in self.pending.iter_mut() {
                    let (ctx, seq) = (p.ctx, p.seq);
                    let backend = &mut self.backend;
                    p.signals.retain(|sig| {
                        if sig.handle != handle {
                            return true;
                        }
                        backend.shared_sync_queue(id, ctx, seq, sig.value);
                        false
                    });
                }
                id
            }
        };
        if !self.backend.shared_sync_hold(id) {
            return Err(Error::NoEnt);
        }
        Ok(id)
    }

    /// Take shared timeline `id` into this device under a handle of its own (one more hold on it).
    pub fn sync_import(&mut self, id: u64) -> Result<u32, Error> {
        if self.syncs.len() >= 1 << 16 {
            return Err(Error::NoSpc);
        }
        if !self.backend.shared_sync_hold(id) {
            return Err(Error::NoEnt);
        }
        let mut h = self.next_sync;
        while self.syncs.contains_key(&h) || h == 0 {
            h = h.wrapping_add(1);
        }
        self.syncs.insert(h, Timeline { value: 0, pending: 0, shared: Some(id) });
        self.next_sync = h.wrapping_add(1);
        Ok(h)
    }

    /// The index of a ready reference (`any`), or of the first one when all are (`!any`, `Some(0)` for none); `None` while not ready.
    /// `pending` compares with each timeline's pending value instead of its completed one. A reference to a destroyed timeline is
    /// an error.
    pub fn waits_ready(&mut self, refs: &[SyncRef], any: bool, pending: bool) -> Result<Option<usize>, Error> {
        self.poll();
        let mut first = None;
        for (i, r) in refs.iter().enumerate() {
            let (value, pend) = self.tl(r.handle)?;
            let v = if pending { pend } else { value };
            let ready = v >= r.value;
            if any && ready {
                return Ok(Some(i));
            }
            if !any && !ready {
                return Ok(None);
            }
            if ready {
                first.get_or_insert(i);
            }
        }
        Ok(if any { None } else { Some(first.unwrap_or(0)) })
    }

    fn advance(&mut self, s: SyncRef) {
        // a timeline destroyed while work was in flight simply loses the signal
        if let Some(t) = self.syncs.get_mut(&s.handle) {
            match t.shared {
                // shared after the work was queued: the backend's timeline takes the signal
                Some(id) => self.backend.shared_sync_signal(id, s.value),
                // `exec` raised `pending` to this value when the work was queued, so `pending >= value` still holds.
                None => t.value = t.value.max(s.value),
            }
        }
    }

    /// Apply the signals of every submission whose fence completed.
    pub fn poll(&mut self) {
        let mut i = 0;
        while i < self.pending.len() {
            let (ctx, seq) = (self.pending[i].ctx, self.pending[i].seq);
            if self.backend.fence_done(ctx, seq) {
                let p = self.pending.remove(i);
                for s in p.signals {
                    self.advance(s);
                }
            } else {
                i += 1;
            }
        }
    }

    /// Whether any submission is still running.
    pub fn busy(&self) -> bool {
        !self.pending.is_empty()
    }

    // ---- execution --------------------------------------------------------------------------------------------------------

    /// Validate and queue `pushes` on `ctx`, then arrange for `signals` to be set when they have run. The `waits` are the caller's
    /// business (it blocked on them before calling); they are only checked to name live timelines.
    pub fn exec(&mut self, ctx: u32, pushes: &[Push], waits: &[SyncRef], signals: &[SyncRef]) -> Result<(), Error> {
        if !self.ctxs.contains_key(&ctx) {
            return Err(Error::NoEnt);
        }
        if pushes.len() > MAX_PUSHES || waits.len() > MAX_SYNC_REFS || signals.len() > MAX_SYNC_REFS {
            return Err(Error::Inval);
        }
        for p in pushes {
            if p.bytes == 0 || p.bytes % 4 != 0 || p.bytes > uapi::PUSH_MAX_BYTES || p.va % 4 != 0 {
                return Err(Error::Inval);
            }
            if p.flags & !uapi::PUSH_NO_PREFETCH != 0 {
                return Err(Error::Inval);
            }
            if !self.covered(p.va, p.bytes as u64) {
                return Err(Error::Fault);
            }
        }
        for r in waits.iter().chain(signals) {
            if !self.syncs.contains_key(&r.handle) {
                return Err(Error::NoEnt);
            }
        }
        // An EXEC with nothing to run still orders after earlier work on the context: signals wait for its fence too.
        let seq = self.backend.submit(ctx, pushes)?;
        // A signal on a shared timeline goes to the backend, which resolves it from the fence whoever asks; the others wait here for `poll`.
        let mut local: Vec<SyncRef> = Vec::new();
        for s in signals {
            match self.syncs.get_mut(&s.handle) {
                Some(Timeline { shared: Some(id), .. }) => {
                    let id = *id;
                    self.backend.shared_sync_queue(id, ctx, seq, s.value);
                }
                Some(t) => {
                    t.pending = t.pending.max(s.value);
                    local.push(*s);
                }
                None => {}
            }
        }
        if !local.is_empty() {
            self.pending.push(Pending { ctx, seq, signals: local });
        }
        self.poll();
        Ok(())
    }

    /// Drop everything (the device was closed): let the GPU finish, then unbind, complete, release. Leaves the model empty and
    /// reusable. Returns `false` when the GPU did not finish ([`Backend::quiesce`]): the caller must then keep the memory the
    /// buffers were made of out of circulation.
    pub fn teardown(&mut self) -> bool {
        let quiet = self.backend.quiesce();
        let starts: Vec<(u64, u64)> = self.bound.iter().map(|(&s, b)| (s, b.len)).collect();
        for (s, l) in starts {
            let _ = self.va_unbind(s, l);
        }
        let handles: Vec<u32> = self.bos.keys().copied().collect();
        for h in handles {
            if let Some(b) = self.bos.get_mut(&h) {
                b.closed = true;
            }
            if self.bos.get(&h).is_some_and(|b| b.binds == 0) {
                self.drop_bo(h);
            }
        }
        let ctxs: Vec<u32> = self.ctxs.keys().copied().collect();
        for c in ctxs {
            let _ = self.ctx_destroy(c);
        }
        for t in core::mem::take(&mut self.syncs).into_values() {
            if let Some(id) = t.shared {
                self.backend.shared_sync_release(id);
            }
        }
        self.pending.clear();
        let allocs: Vec<(u64, u64)> = self.va_allocs.iter().map(|(&s, &l)| (s, l)).collect();
        for (s, l) in allocs {
            self.va_allocs.remove(&s);
            self.va.free(s, l);
        }
        quiet
    }

    /// The VRAM heap offset and size of a live BO that lives in VRAM (what `PRESENT` scans out); `None` for a system BO, a freed or unknown handle.
    pub fn vram_bo(&self, handle: u32) -> Option<(u64, u64)> {
        match self.bos.get(&handle) {
            Some(b) if !b.closed => match b.backing {
                Backing::Vram { vram_off } => Some((vram_off, b.size)),
                Backing::System { .. } => None,
            },
            _ => None,
        }
    }

    pub fn layout(&self) -> Layout {
        self.layout
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uapi::{VaBind, BO_SYSTEM, BO_VRAM};

    const VA0: u64 = 0x100_0000_0000;

    fn layout() -> Layout {
        Layout { arena_bytes: 1 << 24, vram_bytes: 1 << 24, va_start: VA0, va_end: VA0 + (1 << 30) }
    }

    fn dev() -> Device<SoftBackend> {
        Device::new(SoftBackend::default(), layout())
    }

    fn bind(d: &mut Device<SoftBackend>, va: u64, size: u64, handle: u32, bo_offset: u64) -> Result<(), Error> {
        d.va_bind(&VaBind { va, size, bo_offset, handle, pte_kind: 0 })
    }

    fn push(va: u64, bytes: u32) -> Push {
        Push { va, bytes, flags: 0 }
    }

    fn sr(handle: u32, value: u64) -> SyncRef {
        SyncRef { handle, _pad: 0, value }
    }

    // ---- RangeAlloc ------------------------------------------------------------------------------------------------------

    #[test]
    fn range_alloc_respects_alignment_and_exhausts() {
        let mut r = RangeAlloc::new(0x1000, 0x10000);
        assert_eq!(r.alloc(0x1000, 0x1000), Some(0x1000));
        assert_eq!(r.alloc(0x2000, 0x4000), Some(0x4000), "aligned up; the gap before it stays free");
        assert_eq!(r.alloc(0x1000, 0x1000), Some(0x2000), "the gap is reused");
        assert_eq!(r.alloc(0x10000, 0x1000), None, "bigger than what is left");
        assert_eq!(r.alloc(0, 0x1000), None);
        assert_eq!(r.alloc(0x1000, 0x3000), None, "align must be a power of two");
    }

    #[test]
    fn range_alloc_coalesces_on_free() {
        let mut r = RangeAlloc::new(0, 0x10000);
        let a = r.alloc(0x4000, 0x1000).unwrap();
        let b = r.alloc(0x4000, 0x1000).unwrap();
        let c = r.alloc(0x4000, 0x1000).unwrap();
        r.free(b, 0x4000);
        r.free(a, 0x4000);
        r.free(c, 0x4000);
        assert_eq!(r.used(), 0);
        assert_eq!(r.alloc(0x10000, 0x1000), Some(0), "one free range again");
    }

    #[test]
    #[should_panic(expected = "already free")]
    fn range_alloc_double_free_panics() {
        let mut r = RangeAlloc::new(0, 0x10000);
        let a = r.alloc(0x2000, 0x1000).unwrap();
        r.free(a, 0x2000);
        r.free(a, 0x1000);
    }

    #[test]
    fn range_alloc_matches_a_bitmap_model() {
        let mut rng = 0x9e3779b97f4a7c15u64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let pages = 256u64;
        let mut r = RangeAlloc::new(0, pages * PAGE);
        let mut used = vec![false; pages as usize];
        let mut live: Vec<(u64, u64)> = Vec::new();
        for _ in 0..4000 {
            if next() % 3 != 0 || live.is_empty() {
                let n = next() % 8 + 1;
                let align = 1u64 << (next() % 4);
                if let Some(at) = r.alloc(n * PAGE, align * PAGE) {
                    assert_eq!(at % (align * PAGE), 0);
                    for p in at / PAGE..at / PAGE + n {
                        assert!(!used[p as usize], "handed out a page twice");
                        used[p as usize] = true;
                    }
                    live.push((at, n * PAGE));
                }
            } else {
                let (at, len) = live.swap_remove((next() % live.len() as u64) as usize);
                for p in at / PAGE..(at + len) / PAGE {
                    used[p as usize] = false;
                }
                r.free(at, len);
            }
            assert_eq!(r.used(), used.iter().filter(|u| **u).count() as u64 * PAGE);
        }
    }

    // ---- buffer objects --------------------------------------------------------------------------------------------------

    #[test]
    fn bo_create_rounds_up_and_returns_distinct_arena_offsets() {
        let mut d = dev();
        let (h1, off1, size1) = d.bo_create(1, BO_SYSTEM).unwrap();
        let (h2, off2, _) = d.bo_create(5000, BO_SYSTEM).unwrap();
        assert_ne!(h1, h2);
        assert_eq!(size1, 4096);
        assert_ne!(off1, off2);
        assert_eq!(off1 % 4096, 0);
        let (_, off_v, _) = d.bo_create(4096, BO_VRAM).unwrap();
        assert_eq!(off_v, u64::MAX, "VRAM has no CPU mapping");
        assert_eq!(d.vram_used(), 4096);
    }

    #[test]
    fn bo_create_rejects_bad_requests() {
        let mut d = dev();
        assert_eq!(d.bo_create(0, BO_SYSTEM), Err(Error::Inval));
        assert_eq!(d.bo_create(4096, 7), Err(Error::Inval));
        assert_eq!(d.bo_create(u64::MAX, BO_SYSTEM), Err(Error::Inval));
        assert_eq!(d.bo_create(1 << 30, BO_SYSTEM), Err(Error::NoMem), "bigger than the arena");
        assert_eq!(d.bo_create(1 << 30, BO_VRAM), Err(Error::NoMem));
    }

    #[test]
    fn bo_free_releases_and_rejects_a_second_free() {
        let mut d = dev();
        let (h, _, _) = d.bo_create(8192, BO_SYSTEM).unwrap();
        assert_eq!(d.backend.live_bos, 1);
        d.bo_free(h).unwrap();
        assert_eq!(d.backend.live_bos, 0);
        assert_eq!(d.bo_free(h), Err(Error::NoEnt));
        assert_eq!(d.bo_backing(h), Err(Error::NoEnt));
    }

    #[test]
    fn a_freed_bo_that_is_still_bound_keeps_its_storage_until_unbound() {
        let mut d = dev();
        let (h, _, _) = d.bo_create(8192, BO_SYSTEM).unwrap();
        let va = d.va_alloc(8192, 4096).unwrap();
        bind(&mut d, va, 8192, h, 0).unwrap();
        d.bo_free(h).unwrap();
        assert_eq!(d.backend.live_bos, 1, "the GPU may still be reading it");
        assert_eq!(d.bo_backing(h), Err(Error::NoEnt), "but the handle is dead");
        let (h2, _, _) = d.bo_create(4096, BO_SYSTEM).unwrap();
        assert_ne!(h2, h, "the handle is not reused while the storage lives");
        assert_eq!(bind(&mut d, va, 4096, h, 0), Err(Error::NoEnt));
        d.va_unbind(va, 8192).unwrap();
        assert_eq!(d.backend.live_bos, 1, "only the second BO is left");
    }

    #[test]
    fn released_arena_space_is_reused() {
        let mut d = dev();
        let (h, off, _) = d.bo_create(4096, BO_SYSTEM).unwrap();
        d.bo_free(h).unwrap();
        let (_, off2, _) = d.bo_create(4096, BO_SYSTEM).unwrap();
        assert_eq!(off, off2);
    }

    // ---- VA space ---------------------------------------------------------------------------------------------------------

    #[test]
    fn va_alloc_is_aligned_inside_the_range_and_disjoint() {
        let mut d = dev();
        let a = d.va_alloc(0x3000, 0x1000).unwrap();
        let b = d.va_alloc(0x1000, 0x10000).unwrap();
        assert!(a >= VA0 && b >= VA0);
        assert_eq!(b % 0x10000, 0);
        assert!(b >= a + 0x3000 || b + 0x1000 <= a);
        assert_eq!(d.va_alloc(0, 4096), Err(Error::Inval));
        assert_eq!(d.va_alloc(4096, 100), Err(Error::Inval), "align below a page");
        assert_eq!(d.va_alloc(4096, 6000), Err(Error::Inval), "align not a power of two");
        assert_eq!(d.va_alloc(1 << 40, 4096), Err(Error::NoSpc));
    }

    #[test]
    fn va_free_needs_the_exact_range_and_no_bindings() {
        let mut d = dev();
        let (h, _, _) = d.bo_create(4096, BO_SYSTEM).unwrap();
        let va = d.va_alloc(0x4000, 4096).unwrap();
        assert_eq!(d.va_free(va, 0x2000), Err(Error::NoEnt), "not the whole range");
        assert_eq!(d.va_free(va + 0x1000, 0x3000), Err(Error::NoEnt), "not a start");
        bind(&mut d, va + 0x2000, 4096, h, 0).unwrap();
        assert_eq!(d.va_free(va, 0x4000), Err(Error::Busy));
        d.va_unbind(va, 0x4000).unwrap();
        d.va_free(va, 0x4000).unwrap();
        assert_eq!(d.va_free(va, 0x4000), Err(Error::NoEnt));
    }

    #[test]
    fn bind_validation() {
        let mut d = dev();
        let (h, _, _) = d.bo_create(0x4000, BO_SYSTEM).unwrap();
        let va = d.va_alloc(0x8000, 4096).unwrap();
        assert_eq!(bind(&mut d, va + 1, 4096, h, 0), Err(Error::Inval), "va not page aligned");
        assert_eq!(bind(&mut d, va, 4095, h, 0), Err(Error::Inval), "size not a page multiple");
        assert_eq!(bind(&mut d, va, 4096, h, 100), Err(Error::Inval), "bo offset not aligned");
        assert_eq!(bind(&mut d, va, 0, h, 0), Err(Error::Inval), "empty");
        assert_eq!(bind(&mut d, va, 0x5000, h, 0), Err(Error::Inval), "past the end of the BO");
        assert_eq!(bind(&mut d, va, 0x2000, h, 0x3000), Err(Error::Inval), "offset + size past the end of the BO");
        assert_eq!(bind(&mut d, va, u64::MAX & !4095, h, 0), Err(Error::Inval), "overflowing size");
        assert_eq!(bind(&mut d, va, 4096, 999, 0), Err(Error::NoEnt), "unknown BO");
        assert_eq!(bind(&mut d, va + 0x7000, 0x2000, h, 0), Err(Error::Inval), "leaves the allocated range");
        assert_eq!(bind(&mut d, VA0 - 0x1000, 4096, h, 0), Err(Error::Inval), "outside any allocation");
        bind(&mut d, va, 0x4000, h, 0).unwrap();
        assert_eq!(bind(&mut d, va + 0x2000, 0x2000, h, 0), Err(Error::Exist), "overlaps");
        assert_eq!(bind(&mut d, va - 0x1000, 0x2000, h, 0), Err(Error::Inval), "starts outside the allocation");
        bind(&mut d, va + 0x4000, 0x2000, h, 0x2000).unwrap();
        assert_eq!(d.backend.bound.len(), 2);
    }

    #[test]
    fn an_overlap_from_the_left_is_refused_too() {
        let mut d = dev();
        let (h, _, _) = d.bo_create(0x8000, BO_SYSTEM).unwrap();
        let va = d.va_alloc(0x8000, 4096).unwrap();
        bind(&mut d, va + 0x2000, 0x4000, h, 0).unwrap();
        assert_eq!(bind(&mut d, va, 0x3000, h, 0x4000), Err(Error::Exist), "ends inside the bound range");
        assert_eq!(bind(&mut d, va + 0x3000, 0x1000, h, 0), Err(Error::Exist), "inside it");
        assert_eq!(bind(&mut d, va, 0x2000, h, 0x4000), Ok(()), "adjacent on the left is fine");
    }

    #[test]
    fn unbind_cuts_the_middle_out_of_a_binding() {
        let mut d = dev();
        let (h, _, _) = d.bo_create(0x6000, BO_SYSTEM).unwrap();
        let va = d.va_alloc(0x6000, 4096).unwrap();
        bind(&mut d, va, 0x6000, h, 0).unwrap();
        d.va_unbind(va + 0x2000, 0x2000).unwrap();
        let mut b = d.backend.bound.clone();
        b.sort();
        assert_eq!(b, vec![(va, 0x2000), (va + 0x4000, 0x2000)]);
        // the BO stays referenced by both pieces, and its second piece maps the right part of it
        d.bo_free(h).unwrap();
        assert_eq!(d.backend.live_bos, 1);
        d.va_unbind(va, 0x2000).unwrap();
        assert_eq!(d.backend.live_bos, 1);
        d.va_unbind(va + 0x4000, 0x2000).unwrap();
        assert_eq!(d.backend.live_bos, 0);
    }

    #[test]
    fn unbind_across_several_bindings_and_gaps() {
        let mut d = dev();
        let (h, _, _) = d.bo_create(0x4000, BO_SYSTEM).unwrap();
        let va = d.va_alloc(0x10000, 4096).unwrap();
        bind(&mut d, va + 0x1000, 0x2000, h, 0).unwrap();
        bind(&mut d, va + 0x5000, 0x2000, h, 0x2000).unwrap();
        d.va_unbind(va, 0x6000).unwrap();
        let mut b = d.backend.bound.clone();
        b.sort();
        assert_eq!(b, vec![(va + 0x6000, 0x1000)], "the first binding is gone, the second cut on the left");
        d.va_unbind(va, 0x10000).unwrap();
        assert!(d.backend.bound.is_empty());
        assert_eq!(d.va_unbind(va, 0), Err(Error::Inval));
        assert_eq!(d.va_unbind(va + 1, 4096), Err(Error::Inval));
    }

    #[test]
    fn unbind_of_nothing_is_fine() {
        let mut d = dev();
        d.va_unbind(VA0, 0x10000).unwrap();
    }

    // ---- contexts and timelines -------------------------------------------------------------------------------------------

    #[test]
    fn ctx_limits_and_engines() {
        let mut d = dev();
        assert_eq!(d.ctx_create(0), Err(Error::Inval));
        assert_eq!(d.ctx_create(1 << 9), Err(Error::Inval));
        let ids: Vec<u32> = (0..MAX_CTX).map(|_| d.ctx_create(uapi::ENGINE_COMPUTE).unwrap()).collect();
        assert_eq!(d.ctx_create(uapi::ENGINE_COMPUTE), Err(Error::NoSpc));
        d.ctx_destroy(ids[3]).unwrap();
        assert_eq!(d.ctx_destroy(ids[3]), Err(Error::NoEnt));
        d.ctx_create(uapi::ENGINE_3D | uapi::ENGINE_COPY).unwrap();
    }

    #[test]
    fn sync_timelines_only_move_forward() {
        let mut d = dev();
        let s = d.sync_create(5).unwrap();
        assert_eq!(d.sync_query(s).map(|t| t.0), Ok(5));
        d.sync_signal(s, 5).unwrap();
        d.sync_signal(s, 9).unwrap();
        assert_eq!(d.sync_signal(s, 8), Err(Error::Inval));
        assert_eq!(d.sync_query(s).map(|t| t.0), Ok(9));
        d.sync_destroy(s).unwrap();
        assert_eq!(d.sync_query(s), Err(Error::NoEnt));
        assert_eq!(d.sync_signal(s, 10), Err(Error::NoEnt));
    }

    #[test]
    fn waits_ready_all_and_any() {
        let mut d = dev();
        let a = d.sync_create(3).unwrap();
        let b = d.sync_create(0).unwrap();
        let refs = [sr(a, 3), sr(b, 1)];
        assert_eq!(d.waits_ready(&refs, false, false), Ok(None), "b has not reached 1");
        assert_eq!(d.waits_ready(&refs, true, false), Ok(Some(0)), "a is ready");
        assert_eq!(d.waits_ready(&[sr(b, 1), sr(a, 4)], true, false), Ok(None));
        d.sync_signal(b, 1).unwrap();
        assert_eq!(d.waits_ready(&refs, false, false), Ok(Some(0)));
        assert_eq!(d.waits_ready(&[], false, false), Ok(Some(0)), "waiting for nothing is ready");
        assert_eq!(d.waits_ready(&[], true, false), Ok(None), "any of nothing is not");
        assert_eq!(d.waits_ready(&[sr(77, 0)], false, false), Err(Error::NoEnt));
        assert_eq!(d.waits_ready(&[sr(a, 0), sr(77, 0)], true, false), Ok(Some(0)), "any stops at the first ready one");
    }

    // ---- exec -------------------------------------------------------------------------------------------------------------

    fn ready_ctx(d: &mut Device<SoftBackend>) -> (u32, u64) {
        let (h, _, _) = d.bo_create(0x4000, BO_SYSTEM).unwrap();
        let va = d.va_alloc(0x8000, 4096).unwrap();
        bind(d, va, 0x2000, h, 0).unwrap();
        bind(d, va + 0x2000, 0x2000, h, 0x2000).unwrap();
        (d.ctx_create(uapi::ENGINE_COMPUTE).unwrap(), va)
    }

    #[test]
    fn exec_accepts_pushes_inside_bound_memory_even_across_adjacent_bindings() {
        let mut d = dev();
        let (ctx, va) = ready_ctx(&mut d);
        d.exec(ctx, &[push(va, 64), push(va + 0x1ff0, 0x40)], &[], &[]).unwrap();
        assert_eq!(d.backend.log.len(), 1);
        assert_eq!(d.backend.log[0].1.len(), 2);
    }

    #[test]
    fn exec_rejects_bad_pushes() {
        let mut d = dev();
        let (ctx, va) = ready_ctx(&mut d);
        assert_eq!(d.exec(99, &[push(va, 64)], &[], &[]), Err(Error::NoEnt));
        assert_eq!(d.exec(ctx, &[push(va, 0)], &[], &[]), Err(Error::Inval), "empty");
        assert_eq!(d.exec(ctx, &[push(va, 6)], &[], &[]), Err(Error::Inval), "size not a multiple of 4");
        assert_eq!(d.exec(ctx, &[push(va + 2, 8)], &[], &[]), Err(Error::Inval), "address not a multiple of 4");
        assert_eq!(d.exec(ctx, &[push(va, 0x80_0000)], &[], &[]), Err(Error::Inval), "too long for a GPFIFO entry");
        assert_eq!(d.exec(ctx, &[Push { va, bytes: 8, flags: 4 }], &[], &[]), Err(Error::Inval), "unknown flag");
        assert_eq!(d.exec(ctx, &[push(va + 0x3ff8, 16)], &[], &[]), Err(Error::Fault), "runs past the bound memory");
        assert_eq!(d.exec(ctx, &[push(va + 0x4000, 16)], &[], &[]), Err(Error::Fault), "unbound");
        assert_eq!(d.exec(ctx, &[push(u64::MAX - 3, 16)], &[], &[]), Err(Error::Fault), "wraps around the address space");
        assert_eq!(d.exec(ctx, &vec![push(va, 8); MAX_PUSHES + 1], &[], &[]), Err(Error::Inval), "too many pushes");
        assert!(d.backend.log.is_empty(), "nothing reached the backend");
        d.va_unbind(va + 0x1000, 0x1000).unwrap();
        assert_eq!(d.exec(ctx, &[push(va + 0xff8, 16)], &[], &[]), Err(Error::Fault), "a hole in the middle");
    }

    #[test]
    fn exec_names_live_timelines() {
        let mut d = dev();
        let (ctx, va) = ready_ctx(&mut d);
        assert_eq!(d.exec(ctx, &[push(va, 8)], &[sr(42, 1)], &[]), Err(Error::NoEnt));
        assert_eq!(d.exec(ctx, &[push(va, 8)], &[], &[sr(42, 1)]), Err(Error::NoEnt));
        assert!(d.backend.log.is_empty());
    }

    #[test]
    fn signals_are_set_when_the_work_completes() {
        let mut d = dev();
        d.backend.hold = true;
        let (ctx, va) = ready_ctx(&mut d);
        let s = d.sync_create(0).unwrap();
        d.exec(ctx, &[push(va, 8)], &[], &[sr(s, 7)]).unwrap();
        assert_eq!(d.sync_query(s).map(|t| t.0), Ok(0), "still running");
        assert!(d.busy());
        d.backend.complete_all();
        assert_eq!(d.sync_query(s).map(|t| t.0), Ok(7));
        assert!(!d.busy());
    }

    #[test]
    fn signals_complete_at_once_on_a_backend_that_finishes_synchronously() {
        let mut d = dev();
        let (ctx, va) = ready_ctx(&mut d);
        let s = d.sync_create(0).unwrap();
        d.exec(ctx, &[push(va, 8)], &[], &[sr(s, 3)]).unwrap();
        assert_eq!(d.sync_query(s).map(|t| t.0), Ok(3));
    }

    #[test]
    fn a_signal_never_moves_a_timeline_backwards() {
        let mut d = dev();
        let (ctx, va) = ready_ctx(&mut d);
        let s = d.sync_create(10).unwrap();
        d.exec(ctx, &[push(va, 8)], &[], &[sr(s, 4)]).unwrap();
        assert_eq!(d.sync_query(s).map(|t| t.0), Ok(10));
    }

    #[test]
    fn an_exec_with_only_signals_orders_after_earlier_work() {
        let mut d = dev();
        d.backend.hold = true;
        let (ctx, va) = ready_ctx(&mut d);
        let s = d.sync_create(0).unwrap();
        d.exec(ctx, &[push(va, 8)], &[], &[]).unwrap();
        d.exec(ctx, &[], &[], &[sr(s, 1)]).unwrap();
        assert_eq!(d.sync_query(s).map(|t| t.0), Ok(0), "the earlier push has not finished");
        d.backend.complete_all();
        assert_eq!(d.sync_query(s).map(|t| t.0), Ok(1));
    }

    #[test]
    fn destroying_a_context_settles_its_pending_signals() {
        let mut d = dev();
        d.backend.hold = true;
        let (ctx, va) = ready_ctx(&mut d);
        let s = d.sync_create(0).unwrap();
        d.exec(ctx, &[push(va, 8)], &[], &[sr(s, 5)]).unwrap();
        d.ctx_destroy(ctx).unwrap();
        assert_eq!(d.sync_query(s).map(|t| t.0), Ok(5));
        assert!(!d.busy());
    }

    #[test]
    fn a_timeline_destroyed_with_work_in_flight_loses_only_the_signal() {
        let mut d = dev();
        d.backend.hold = true;
        let (ctx, va) = ready_ctx(&mut d);
        let s = d.sync_create(0).unwrap();
        d.exec(ctx, &[push(va, 8)], &[], &[sr(s, 5)]).unwrap();
        d.sync_destroy(s).unwrap();
        d.backend.complete_all();
        d.poll();
        assert!(!d.busy());
    }


    #[test]
    fn bo_handles_stay_unique_when_the_counter_wraps() {
        let mut d = dev();
        let (first, _, _) = d.bo_create(4096, BO_SYSTEM).unwrap();
        assert_eq!(first, 1);
        d.next_bo = u32::MAX;
        let (a, _, _) = d.bo_create(4096, BO_SYSTEM).unwrap();
        let (b, _, _) = d.bo_create(4096, BO_SYSTEM).unwrap();
        assert_eq!(a, u32::MAX);
        assert_eq!(b, 2, "0 is never a handle and 1 is taken");
    }

    #[test]
    fn va_alloc_align_below_a_page_is_refused() {
        let mut d = dev();
        assert_eq!(d.va_alloc(4096, 2048), Err(Error::Inval));
        assert_eq!(d.va_alloc(4096, 4096).map(|_| ()), Ok(()));
    }

    #[test]
    fn unbind_leaves_untouched_bindings_alone() {
        let mut d = dev();
        let (h, _, _) = d.bo_create(0x4000, BO_SYSTEM).unwrap();
        let va = d.va_alloc(0x8000, 4096).unwrap();
        bind(&mut d, va, 0x2000, h, 0).unwrap();
        bind(&mut d, va + 0x4000, 0x2000, h, 0x2000).unwrap();
        let ops = d.backend.table_ops;
        d.va_unbind(va + 0x2000, 0x2000).unwrap();
        assert_eq!(d.backend.table_ops, ops, "a range that touches no binding writes no entries, not even one that ends where it starts");
        d.va_unbind(va + 0x6000, 0x2000).unwrap();
        assert_eq!(d.backend.table_ops, ops, "nor one that starts where a binding ends");
    }

    #[test]
    fn the_pieces_left_by_a_cut_map_the_right_part_of_the_bo() {
        let mut d = dev();
        let (h, _, _) = d.bo_create(0x8000, BO_SYSTEM).unwrap();
        let base = match d.bo_backing(h).unwrap().0 {
            Backing::System { arena_off } => arena_off,
            _ => unreachable!(),
        };
        let va = d.va_alloc(0x8000, 4096).unwrap();
        bind(&mut d, va, 0x8000, h, 0).unwrap();
        d.va_unbind(va + 0x2000, 0x3000).unwrap();
        let mut m = d.backend.mapped.clone();
        m.sort_by_key(|m| m.0);
        assert_eq!(m, vec![
            (va, 0x2000, Backing::System { arena_off: base }, 0),
            (va + 0x5000, 0x3000, Backing::System { arena_off: base }, 0x5000),
        ]);
        // and again from inside the right piece, with the cut starting before it
        d.va_unbind(va + 0x4000, 0x2000).unwrap();
        let mut m = d.backend.mapped.clone();
        m.sort_by_key(|m| m.0);
        assert_eq!(m[1], (va + 0x6000, 0x2000, Backing::System { arena_off: base }, 0x6000));
    }

    #[test]
    fn exec_array_limits_are_exact() {
        let mut d = dev();
        let (ctx, va) = ready_ctx(&mut d);
        let s = d.sync_create(0).unwrap();
        d.exec(ctx, &vec![push(va, 8); MAX_PUSHES], &[], &[]).unwrap();
        assert_eq!(d.exec(ctx, &[], &vec![sr(s, 0); MAX_SYNC_REFS + 1], &[]), Err(Error::Inval));
        d.exec(ctx, &[], &vec![sr(s, 0); MAX_SYNC_REFS], &[]).unwrap();
        assert_eq!(d.exec(ctx, &[], &[], &vec![sr(s, 0); MAX_SYNC_REFS + 1]), Err(Error::Inval));
        d.exec(ctx, &[], &[], &vec![sr(s, 0); MAX_SYNC_REFS]).unwrap();
    }

    #[test]
    #[should_panic(expected = "the model should have refused")]
    fn a_backend_that_does_not_check_contexts_is_never_reached_with_a_stale_one() {
        // if the model let an unknown context through, the soft backend would panic here
        let mut d = dev();
        let (ctx, va) = ready_ctx(&mut d);
        d.ctx_destroy(ctx).unwrap();
        let r = d.exec(ctx, &[push(va, 8)], &[], &[]);
        assert_eq!(r, Err(Error::NoEnt), "refused by the model");
        // prove the guard above is what stops it: call the backend directly
        d.backend.submit(ctx, &[]).unwrap();
    }


    #[test]
    fn pending_runs_ahead_of_the_completed_value_until_the_work_finishes() {
        let mut d = dev();
        d.backend.hold = true;
        let (ctx, va) = ready_ctx(&mut d);
        let s = d.sync_create(2).unwrap();
        assert_eq!(d.sync_query(s), Ok((2, 2)), "a fresh timeline: nothing pending beyond its value");
        d.exec(ctx, &[push(va, 8)], &[], &[sr(s, 7)]).unwrap();
        assert_eq!(d.sync_query(s), Ok((2, 7)), "queued work will take it to 7");
        assert_eq!(d.waits_ready(&[sr(s, 7)], false, false), Ok(None), "not completed");
        assert_eq!(d.waits_ready(&[sr(s, 7)], false, true), Ok(Some(0)), "but pending");
        assert_eq!(d.waits_ready(&[sr(s, 8)], false, true), Ok(None), "beyond what anyone will signal");
        d.exec(ctx, &[push(va, 8)], &[], &[sr(s, 4)]).unwrap();
        assert_eq!(d.sync_query(s), Ok((2, 7)), "a lower signal never lowers pending");
        d.backend.complete_all();
        assert_eq!(d.sync_query(s), Ok((7, 7)));
    }

    #[test]
    fn a_cpu_signal_raises_both_values() {
        let mut d = dev();
        let s = d.sync_create(0).unwrap();
        d.sync_signal(s, 3).unwrap();
        assert_eq!(d.sync_query(s), Ok((3, 3)));
    }

    #[test]
    fn a_cpu_signal_below_the_completed_value_is_refused_but_below_pending_is_not() {
        let mut d = dev();
        d.backend.hold = true;
        let (ctx, va) = ready_ctx(&mut d);
        let s = d.sync_create(0).unwrap();
        d.exec(ctx, &[push(va, 8)], &[], &[sr(s, 9)]).unwrap();
        d.sync_signal(s, 4).unwrap();
        assert_eq!(d.sync_query(s), Ok((4, 9)), "the CPU moved the completed value, pending stays ahead");
        assert_eq!(d.sync_signal(s, 3), Err(Error::Inval));
    }

    #[test]
    fn a_refused_exec_leaves_pending_alone() {
        let mut d = dev();
        let (ctx, va) = ready_ctx(&mut d);
        let s = d.sync_create(0).unwrap();
        assert_eq!(d.exec(ctx, &[push(va + 0x100000, 8)], &[], &[sr(s, 5)]), Err(Error::Fault));
        assert_eq!(d.sync_query(s), Ok((0, 0)));
    }

    #[test]
    fn pending_wait_any_and_all() {
        let mut d = dev();
        d.backend.hold = true;
        let (ctx, va) = ready_ctx(&mut d);
        let a = d.sync_create(0).unwrap();
        let b = d.sync_create(0).unwrap();
        d.exec(ctx, &[push(va, 8)], &[], &[sr(a, 1)]).unwrap();
        assert_eq!(d.waits_ready(&[sr(a, 1), sr(b, 1)], true, true), Ok(Some(0)));
        assert_eq!(d.waits_ready(&[sr(a, 1), sr(b, 1)], false, true), Ok(None));
    }

    // ---- teardown and a randomised cross-check ----------------------------------------------------------------------------

    #[test]
    fn teardown_leaves_nothing_behind() {
        let mut d = dev();
        let (ctx, va) = ready_ctx(&mut d);
        let s = d.sync_create(0).unwrap();
        d.backend.hold = true;
        d.exec(ctx, &[push(va, 8)], &[], &[sr(s, 1)]).unwrap();
        let (v, _, _) = d.bo_create(4096, BO_VRAM).unwrap();
        let vva = d.va_alloc(4096, 4096).unwrap();
        bind(&mut d, vva, 4096, v, 0).unwrap();
        d.teardown();
        assert!(d.backend.bound.is_empty());
        assert_eq!(d.backend.live_bos, 0);
        assert_eq!(d.vram_used(), 0);
        assert!(!d.busy());
        // reusable
        let (_, off, _) = d.bo_create(4096, BO_SYSTEM).unwrap();
        assert_eq!(off, 0, "the arena is empty again");
        assert_eq!(d.va_alloc(4096, 4096), Ok(VA0), "and so is the VA space");
    }

    #[test]
    fn random_binds_and_unbinds_match_a_page_table_model() {
        let mut rng = 0x243f6a8885a308d3u64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut d = dev();
        let pages = 64u64;
        let (h1, _, _) = d.bo_create(pages * PAGE, BO_SYSTEM).unwrap();
        let (h2, _, _) = d.bo_create(pages * PAGE, BO_VRAM).unwrap();
        let va = d.va_alloc(pages * PAGE, PAGE).unwrap();
        // model: for every page of the VA range, what it maps (handle, page of the BO)
        let mut model: Vec<Option<(u32, u64)>> = vec![None; pages as usize];
        for _ in 0..3000 {
            let start = next() % pages;
            let n = next() % 8 + 1;
            let n = n.min(pages - start);
            if next() % 2 == 0 {
                let (h, off) = if next() % 2 == 0 { (h1, next() % (pages - n + 1)) } else { (h2, next() % (pages - n + 1)) };
                let free = model[start as usize..(start + n) as usize].iter().all(|m| m.is_none());
                let r = bind(&mut d, va + start * PAGE, n * PAGE, h, off * PAGE);
                if free {
                    assert_eq!(r, Ok(()));
                    for i in 0..n {
                        model[(start + i) as usize] = Some((h, off + i));
                    }
                } else {
                    assert_eq!(r, Err(Error::Exist));
                }
            } else {
                d.va_unbind(va + start * PAGE, n * PAGE).unwrap();
                for i in 0..n {
                    model[(start + i) as usize] = None;
                }
            }
            // the backend's page tables cover exactly the model's mapped pages
            let mut covered = vec![false; pages as usize];
            for &(bva, blen) in &d.backend.bound {
                for p in (bva - va) / PAGE..(bva - va + blen) / PAGE {
                    assert!(!covered[p as usize], "a page is mapped twice");
                    covered[p as usize] = true;
                }
            }
            for (i, m) in model.iter().enumerate() {
                assert_eq!(covered[i], m.is_some(), "page {i}");
            }
            // every mapped page of the model is reachable by exec, every other one is a fault
            let probe = next() % pages;
            let r = d.ctx_create(uapi::ENGINE_COMPUTE).and_then(|c| {
                let r = d.exec(c, &[push(va + probe * PAGE, 4)], &[], &[]);
                d.ctx_destroy(c).unwrap();
                r
            });
            assert_eq!(r.is_ok(), model[probe as usize].is_some());
        }
        d.va_unbind(va, pages * PAGE).unwrap();
        assert!(d.backend.bound.is_empty());
    }

    // ---- several devices on one VRAM heap (G5 layer 1) ---------------------------------------------------------------------

    /// A soft backend whose heaps are shared with other instances (what the kernel does across `/dev/nvgpu` sessions): one `RangeAlloc`
    /// per heap and a reference count per allocation.
    struct SharedHeaps {
        heaps: [RangeAlloc; 2],
        refs: alloc::collections::BTreeMap<(Heap, u64), u32>,
        syncs: alloc::collections::BTreeMap<u64, TSync>,
        next_sync: u64,
        /// Fence state the shared timelines are resolved against: (device tag, ctx) -> the highest completed `seq`.
        done: alloc::collections::BTreeMap<(u32, u32), u64>,
    }

    /// A shared timeline in the test backend: what the kernel adapter keeps in its registry.
    struct TSync {
        value: u64,
        pending: u64,
        refs: u32,
        /// Queued signals: (device tag, ctx, seq, value).
        pend: Vec<(u32, u32, u64, u64)>,
    }

    type Pool = alloc::rc::Rc<core::cell::RefCell<SharedHeaps>>;

    fn pool(system: u64, vram: u64) -> Pool {
        alloc::rc::Rc::new(core::cell::RefCell::new(SharedHeaps {
            heaps: [RangeAlloc::new(0, system), RangeAlloc::new(0, vram)],
            refs: Default::default(),
            syncs: Default::default(),
            next_sync: 1,
            done: Default::default(),
        }))
    }

    struct Shared {
        soft: SoftBackend,
        pool: Pool,
        /// Which device this backend is: the fences it resolves are its own.
        tag: u32,
    }

    impl Backend for Shared {
        fn bo_create(&mut self, b: Backing, s: u64) -> Result<(), Error> {
            self.soft.bo_create(b, s)
        }
        fn bo_release(&mut self, b: Backing, s: u64) {
            self.soft.bo_release(b, s)
        }
        fn bind(&mut self, va: u64, size: u64, b: Backing, o: u64, k: u32) -> Result<(), Error> {
            self.soft.bind(va, size, b, o, k)
        }
        fn unbind(&mut self, va: u64, size: u64) {
            self.soft.unbind(va, size)
        }
        fn ctx_create(&mut self, c: u32, e: u32) -> Result<(), Error> {
            self.soft.ctx_create(c, e)
        }
        fn ctx_destroy(&mut self, c: u32) {
            self.soft.ctx_destroy(c)
        }
        fn submit(&mut self, c: u32, p: &[Push]) -> Result<u64, Error> {
            self.soft.submit(c, p)
        }
        fn fence_done(&mut self, c: u32, s: u64) -> bool {
            self.soft.fence_done(c, s)
        }
        fn shared_heaps(&self) -> bool {
            true
        }
        fn heap_alloc(&mut self, heap: Heap, size: u64) -> Option<u64> {
            let mut p = self.pool.borrow_mut();
            let off = p.heaps[heap as usize].alloc(size, PAGE)?;
            p.refs.insert((heap, off), 1);
            Some(off)
        }
        fn heap_hold(&mut self, heap: Heap, off: u64) {
            *self.pool.borrow_mut().refs.get_mut(&(heap, off)).expect("holding storage that is not allocated") += 1;
        }
        fn heap_free(&mut self, heap: Heap, off: u64, size: u64) {
            let mut p = self.pool.borrow_mut();
            let n = p.refs.get_mut(&(heap, off)).expect("freeing storage that is not allocated");
            *n -= 1;
            if *n == 0 {
                p.refs.remove(&(heap, off));
                p.heaps[heap as usize].free(off, size);
            }
        }
        fn shared_sync_create(&mut self, value: u64, pending: u64) -> Option<u64> {
            let mut p = self.pool.borrow_mut();
            let id = p.next_sync;
            p.next_sync += 1;
            p.syncs.insert(id, TSync { value, pending, refs: 1, pend: Vec::new() });
            Some(id)
        }
        fn shared_sync_hold(&mut self, id: u64) -> bool {
            match self.pool.borrow_mut().syncs.get_mut(&id) {
                Some(t) => {
                    t.refs += 1;
                    true
                }
                None => false,
            }
        }
        fn shared_sync_release(&mut self, id: u64) {
            let mut p = self.pool.borrow_mut();
            let t = p.syncs.get_mut(&id).expect("releasing a timeline that does not exist");
            t.refs -= 1;
            if t.refs == 0 {
                p.syncs.remove(&id);
            }
        }
        fn shared_sync_value(&mut self, id: u64) -> (u64, u64) {
            let mut p = self.pool.borrow_mut();
            let p = &mut *p;
            let t = p.syncs.get_mut(&id).expect("reading a timeline that does not exist");
            let done = &p.done;
            t.pend.retain(|&(tag, ctx, seq, v)| {
                if done.get(&(tag, ctx)).is_some_and(|&d| d >= seq) {
                    t.value = t.value.max(v);
                    false
                } else {
                    true
                }
            });
            (t.value, t.pending)
        }
        fn shared_sync_signal(&mut self, id: u64, value: u64) {
            let mut p = self.pool.borrow_mut();
            let t = p.syncs.get_mut(&id).expect("signalling a timeline that does not exist");
            t.value = t.value.max(value);
            t.pending = t.pending.max(value);
        }
        fn shared_sync_queue(&mut self, id: u64, ctx: u32, seq: u64, value: u64) {
            let tag = self.tag;
            let mut p = self.pool.borrow_mut();
            let t = p.syncs.get_mut(&id).expect("queueing on a timeline that does not exist");
            t.pending = t.pending.max(value);
            t.pend.push((tag, ctx, seq, value));
        }
    }

    fn slot_layout(slot: u64) -> Layout {
        // each device's own heap sizes are 0: they must not matter
        Layout { arena_bytes: 0, vram_bytes: 0, va_start: VA0 + slot * (1 << 30), va_end: VA0 + (slot + 1) * (1 << 30) }
    }

    fn shared_pair(system: u64, vram: u64) -> (Device<Shared>, Device<Shared>, Pool) {
        let p = pool(system, vram);
        let mk = |slot| Device::new(Shared { soft: SoftBackend::default(), pool: p.clone(), tag: slot as u32 }, slot_layout(slot));
        (mk(0), mk(1), p)
    }

    fn used(p: &Pool, h: Heap) -> u64 {
        p.borrow().heaps[h as usize].used()
    }

    #[test]
    fn devices_on_shared_heaps_get_disjoint_storage() {
        let (mut a, mut b, _) = shared_pair(1 << 20, 1 << 20);
        let (ha, _, _) = a.bo_create(0x40000, BO_VRAM).unwrap();
        let (hb, _, _) = b.bo_create(0x40000, BO_VRAM).unwrap();
        let (oa, sa) = a.vram_bo(ha).unwrap();
        let (ob, sb) = b.vram_bo(hb).unwrap();
        assert!(oa + sa <= ob || ob + sb <= oa, "{oa:#x}+{sa:#x} overlaps {ob:#x}+{sb:#x}");
        assert_eq!((a.vram_used(), b.vram_used()), (0x40000, 0x40000), "each device counts its own");
        let (_, ma, _) = a.bo_create(0x10000, BO_SYSTEM).unwrap();
        let (_, mb, _) = b.bo_create(0x10000, BO_SYSTEM).unwrap();
        assert!(ma + 0x10000 <= mb || mb + 0x10000 <= ma, "system BOs of two devices share an arena offset: {ma:#x} {mb:#x}");
    }

    #[test]
    fn a_shared_heap_is_one_pool_and_gives_back_on_free_and_teardown() {
        let (mut a, mut b, p) = shared_pair(1 << 20, 1 << 20);
        let (ha, _, _) = a.bo_create(0xc0000, BO_VRAM).unwrap();
        assert_eq!(b.bo_create(0x80000, BO_VRAM), Err(Error::NoMem), "what a holds is not b's to take");
        a.bo_free(ha).unwrap();
        let (hb, _, _) = b.bo_create(0x80000, BO_VRAM).unwrap();
        assert_eq!(used(&p, Heap::Vram), 0x80000);
        b.bo_create(0x40000, BO_VRAM).unwrap();
        b.bo_create(0x40000, BO_SYSTEM).unwrap();
        assert!(b.teardown());
        assert_eq!((used(&p, Heap::Vram), used(&p, Heap::System)), (0, 0), "teardown returns every byte");
        assert_eq!((a.vram_used(), b.vram_used()), (0, 0));
        assert_eq!(b.vram_bo(hb), None);
    }

    /// A backend that refuses every `bo_create`, over a `Shared` one.
    struct Refuses(Shared);

    impl Backend for Refuses {
        fn bo_create(&mut self, _: Backing, _: u64) -> Result<(), Error> {
            Err(Error::NoMem)
        }
        fn bo_release(&mut self, b: Backing, s: u64) {
            self.0.bo_release(b, s)
        }
        fn bind(&mut self, va: u64, size: u64, b: Backing, o: u64, k: u32) -> Result<(), Error> {
            self.0.bind(va, size, b, o, k)
        }
        fn unbind(&mut self, va: u64, size: u64) {
            self.0.unbind(va, size)
        }
        fn ctx_create(&mut self, c: u32, e: u32) -> Result<(), Error> {
            self.0.ctx_create(c, e)
        }
        fn ctx_destroy(&mut self, c: u32) {
            self.0.ctx_destroy(c)
        }
        fn submit(&mut self, c: u32, p: &[Push]) -> Result<u64, Error> {
            self.0.submit(c, p)
        }
        fn fence_done(&mut self, c: u32, s: u64) -> bool {
            self.0.fence_done(c, s)
        }
        fn shared_heaps(&self) -> bool {
            true
        }
        fn heap_alloc(&mut self, h: Heap, size: u64) -> Option<u64> {
            self.0.heap_alloc(h, size)
        }
        fn heap_hold(&mut self, h: Heap, o: u64) {
            self.0.heap_hold(h, o)
        }
        fn heap_free(&mut self, h: Heap, o: u64, s: u64) {
            self.0.heap_free(h, o, s)
        }
    }

    #[test]
    fn a_backend_failure_gives_shared_storage_back() {
        let p = pool(1 << 20, 1 << 20);
        let mut d = Device::new(Refuses(Shared { soft: SoftBackend::default(), pool: p.clone(), tag: 0 }), layout());
        assert_eq!(d.bo_create(0x1000, BO_VRAM), Err(Error::NoMem));
        assert_eq!(d.bo_create(0x1000, BO_SYSTEM), Err(Error::NoMem));
        assert_eq!((used(&p, Heap::Vram), used(&p, Heap::System)), (0, 0));
        assert_eq!(d.vram_used(), 0);
    }

    // ---- sharing buffers between devices (G5 layer 2) ----------------------------------------------------------------------

    fn export(d: &Device<Shared>, h: u32) -> (Backing, u64) {
        d.bo_backing(h).unwrap()
    }

    #[test]
    fn an_imported_bo_is_the_same_storage_under_a_handle_of_its_own() {
        let (mut a, mut b, p) = shared_pair(1 << 20, 1 << 20);
        let (ha, ma, sa) = a.bo_create(0x8000, BO_SYSTEM).unwrap();
        let (backing, size) = export(&a, ha);
        let (hb, mb, sb) = b.bo_import(backing, size).unwrap();
        assert_eq!((mb, sb), (ma, sa), "the importer maps the same arena offset");
        assert_eq!(b.bo_backing(hb).unwrap(), (backing, size));
        // both devices can bind it, each at its own addresses
        let va_a = a.va_alloc(0x8000, 0x1000).unwrap();
        let va_b = b.va_alloc(0x8000, 0x1000).unwrap();
        assert_ne!(va_a, va_b);
        a.va_bind(&VaBind { va: va_a, size: 0x8000, bo_offset: 0, handle: ha, pte_kind: 0 }).unwrap();
        b.va_bind(&VaBind { va: va_b, size: 0x8000, bo_offset: 0, handle: hb, pte_kind: 0 }).unwrap();
        assert_eq!(a.backend.soft.mapped[0].2, b.backend.soft.mapped[0].2, "same backing in both page-table writes");
        // one allocation in the pool, however many holders
        assert_eq!(used(&p, Heap::System), 0x8000);
    }

    #[test]
    fn the_storage_lives_until_the_last_holder_lets_go() {
        let (mut a, mut b, p) = shared_pair(1 << 20, 1 << 20);
        let (ha, _, _) = a.bo_create(0x4000, BO_VRAM).unwrap();
        let (backing, size) = export(&a, ha);
        let (hb, _, _) = b.bo_import(backing, size).unwrap();
        a.bo_free(ha).unwrap();
        assert_eq!(used(&p, Heap::Vram), 0x4000, "the exporter's handle is gone, the importer still holds the memory");
        assert_eq!(b.vram_bo(hb), Some((match backing { Backing::Vram { vram_off } => vram_off, _ => unreachable!() }, 0x4000)));
        assert_eq!((a.vram_used(), b.vram_used()), (0, 0x4000));
        // a fresh BO cannot land on the memory b still holds
        let (hc, _, _) = a.bo_create(0x4000, BO_VRAM).unwrap();
        assert_ne!(a.vram_bo(hc).unwrap().0, b.vram_bo(hb).unwrap().0);
        b.bo_free(hb).unwrap();
        assert_eq!(used(&p, Heap::Vram), 0x4000, "only hc is left");
        a.bo_free(hc).unwrap();
        assert_eq!(used(&p, Heap::Vram), 0);
    }

    #[test]
    fn the_exporters_whole_session_can_end_first() {
        let (mut a, mut b, p) = shared_pair(1 << 20, 1 << 20);
        let (ha, _, _) = a.bo_create(0x4000, BO_SYSTEM).unwrap();
        let (backing, size) = export(&a, ha);
        let (hb, mb, _) = b.bo_import(backing, size).unwrap();
        let va = a.va_alloc(0x4000, 0x1000).unwrap();
        a.va_bind(&VaBind { va, size: 0x4000, bo_offset: 0, handle: ha, pte_kind: 0 }).unwrap();
        assert!(a.teardown());
        assert_eq!(used(&p, Heap::System), 0x4000, "b's hold keeps it");
        assert_eq!(b.bo_backing(hb).unwrap().0, backing);
        assert_eq!(Backing::System { arena_off: mb }, backing);
        assert!(b.teardown());
        assert_eq!(used(&p, Heap::System), 0, "and its teardown frees it");
    }

    #[test]
    fn a_bo_can_be_imported_twice_into_one_device() {
        let (mut a, mut b, p) = shared_pair(1 << 20, 1 << 20);
        let (ha, _, _) = a.bo_create(0x2000, BO_VRAM).unwrap();
        let (backing, size) = export(&a, ha);
        let (h1, _, _) = b.bo_import(backing, size).unwrap();
        let (h2, _, _) = b.bo_import(backing, size).unwrap();
        assert_ne!(h1, h2);
        assert_eq!(b.vram_used(), 0x4000, "each import is a holder");
        b.bo_free(h1).unwrap();
        a.bo_free(ha).unwrap();
        assert_eq!(used(&p, Heap::Vram), 0x2000, "h2 still holds it");
        b.bo_free(h2).unwrap();
        assert_eq!(used(&p, Heap::Vram), 0);
    }

    #[test]
    fn an_import_is_refused_off_shared_heaps_and_for_bad_sizes() {
        let mut solo = dev();
        let (h, _, _) = solo.bo_create(0x1000, BO_VRAM).unwrap();
        let (backing, size) = solo.bo_backing(h).unwrap();
        assert_eq!(solo.bo_import(backing, size), Err(Error::Inval), "a device with heaps of its own has nobody to share with");
        let (mut a, mut b, p) = shared_pair(1 << 20, 1 << 20);
        let (ha, _, _) = a.bo_create(0x2000, BO_VRAM).unwrap();
        let (backing, _) = export(&a, ha);
        assert_eq!(b.bo_import(backing, 0), Err(Error::Inval));
        assert_eq!(b.bo_import(backing, 0x1800), Err(Error::Inval));
        // nothing was held by the refused attempts
        a.bo_free(ha).unwrap();
        assert_eq!(used(&p, Heap::Vram), 0);
    }

    #[test]
    fn a_failed_import_lets_go_of_its_hold() {
        let p = pool(1 << 20, 1 << 20);
        let mut a = Device::new(Shared { soft: SoftBackend::default(), pool: p.clone(), tag: 0 }, slot_layout(0));
        let mut r = Device::new(Refuses(Shared { soft: SoftBackend::default(), pool: p.clone(), tag: 0 }), slot_layout(1));
        let (ha, _, _) = a.bo_create(0x2000, BO_VRAM).unwrap();
        let (backing, size) = a.bo_backing(ha).unwrap();
        assert_eq!(r.bo_import(backing, size), Err(Error::NoMem));
        assert_eq!(r.vram_used(), 0);
        a.bo_free(ha).unwrap();
        assert_eq!(used(&p, Heap::Vram), 0, "the hold taken for the failed import did not leak the memory");
    }

    // ---- sharing timelines between devices (G5 layer 2) ----------------------------------------------------------------------

    /// The GPU finished everything `d` had submitted: its soft fences complete and the pool's fence state says so.
    fn complete(d: &mut Device<Shared>) {
        d.backend.soft.complete_all();
        let tag = d.backend.tag;
        let done: Vec<((u32, u32), u64)> = d.backend.soft.done.iter().map(|(&c, &s)| ((tag, c), s)).collect();
        d.backend.pool.borrow_mut().done.extend(done);
    }

    fn holding_device(slot: u64, p: &Pool) -> Device<Shared> {
        let mut soft = SoftBackend::default();
        soft.hold = true;
        Device::new(Shared { soft, pool: p.clone(), tag: slot as u32 }, slot_layout(slot))
    }

    /// A device with a context and a bound page to push from.
    fn ready(d: &mut Device<Shared>) -> u32 {
        let (h, _, _) = d.bo_create(0x1000, BO_SYSTEM).unwrap();
        let va = d.va_alloc(0x1000, 0x1000).unwrap();
        d.va_bind(&VaBind { va, size: 0x1000, bo_offset: 0, handle: h, pte_kind: 0 }).unwrap();
        d.backend.soft.table_ops = va as usize; // remembered for `push_at`
        d.ctx_create(uapi::ENGINE_COMPUTE).unwrap()
    }

    fn push_at(d: &Device<Shared>) -> Push {
        push(d.backend.soft.table_ops as u64, 16)
    }

    #[test]
    fn a_shared_timeline_is_resolved_for_a_reader_while_the_signaller_is_idle() {
        let p = pool(1 << 20, 1 << 20);
        let (mut a, mut b) = (holding_device(0, &p), holding_device(1, &p));
        let ca = ready(&mut a);
        let ta = a.sync_create(0).unwrap();
        let id = a.sync_share(ta).unwrap();
        let tb = b.sync_import(id).unwrap();
        // a queues work that signals 5 and then does not call in again
        a.exec(ca, &[push_at(&a)], &[], &[sr(ta, 5)]).unwrap();
        assert_eq!(b.sync_query(tb).unwrap(), (0, 5), "b sees the queued value as pending, not yet as completed");
        assert_eq!(b.waits_ready(&[sr(tb, 5)], false, false).unwrap(), None);
        assert_eq!(b.waits_ready(&[sr(tb, 5)], false, true).unwrap(), Some(0), "WAIT_PENDING is satisfied by the queued value");
        complete(&mut a);
        assert_eq!(b.sync_query(tb).unwrap(), (5, 5), "the work completed and b found out without a doing anything");
        assert_eq!(b.waits_ready(&[sr(tb, 5)], false, false).unwrap(), Some(0));
        assert_eq!(a.sync_query(ta).unwrap(), (5, 5), "and a's own handle says the same");
    }

    #[test]
    fn a_timeline_shared_after_work_was_queued_still_completes() {
        let p = pool(1 << 20, 1 << 20);
        let (mut a, mut b) = (holding_device(0, &p), holding_device(1, &p));
        let ca = ready(&mut a);
        let ta = a.sync_create(0).unwrap();
        a.exec(ca, &[push_at(&a)], &[], &[sr(ta, 7)]).unwrap();
        let id = a.sync_share(ta).unwrap();
        let tb = b.sync_import(id).unwrap();
        assert_eq!(b.sync_query(tb).unwrap().1, 7, "the pending value came along");
        complete(&mut a);
        // no `a.poll()`: a queued the work and went idle, and b still finds it complete
        assert_eq!(b.sync_query(tb).unwrap(), (7, 7));
        assert!(a.busy(), "the device still knows it has a submission outstanding until it polls");
        a.poll();
        assert!(!a.busy());
        assert_eq!(a.sync_query(ta).unwrap(), (7, 7), "and polling later changes nothing");
    }

    #[test]
    fn sharing_one_timeline_moves_only_its_own_queued_signals() {
        let p = pool(1 << 20, 1 << 20);
        let (mut a, mut b) = (holding_device(0, &p), holding_device(1, &p));
        let ca = ready(&mut a);
        let (shared, private) = (a.sync_create(0).unwrap(), a.sync_create(0).unwrap());
        // one submission signals both timelines: 4 on the one that will be shared, 6 on the one that will not
        a.exec(ca, &[push_at(&a)], &[], &[sr(shared, 4), sr(private, 6)]).unwrap();
        let id = a.sync_share(shared).unwrap();
        let tb = b.sync_import(id).unwrap();
        complete(&mut a);
        assert_eq!(b.sync_query(tb).unwrap(), (4, 4), "the shared timeline got its own signal, and not the other's 6");
        assert_eq!(a.sync_query(private).unwrap(), (6, 6), "the private one still completes through the device's own poll");
        assert_eq!(a.sync_query(shared).unwrap(), (4, 4));
    }

    #[test]
    fn a_cpu_signal_on_one_side_is_seen_on_the_other_and_cannot_go_backwards() {
        let (mut a, mut b, _) = shared_pair(1 << 20, 1 << 20);
        let ta = a.sync_create(3).unwrap();
        let id = a.sync_share(ta).unwrap();
        let tb = b.sync_import(id).unwrap();
        assert_eq!(b.sync_query(tb).unwrap(), (3, 3), "the value the timeline had when it was shared");
        b.sync_signal(tb, 9).unwrap();
        assert_eq!(a.sync_query(ta).unwrap(), (9, 9));
        assert_eq!(a.sync_signal(ta, 4), Err(Error::Inval), "a value below the current one is refused on either side");
        assert_eq!(b.sync_signal(tb, 8), Err(Error::Inval));
    }

    #[test]
    fn a_shared_timeline_lives_until_its_last_holder_lets_go() {
        let (mut a, mut b, p) = shared_pair(1 << 20, 1 << 20);
        let ta = a.sync_create(0).unwrap();
        let id = a.sync_share(ta).unwrap();   // the descriptor's hold
        let again = a.sync_share(ta).unwrap();
        assert_eq!(again, id, "sharing twice names the same timeline");
        a.backend.shared_sync_release(again);  // a second descriptor, closed
        let tb = b.sync_import(id).unwrap();
        a.backend.shared_sync_release(id);     // the first descriptor, closed
        assert!(a.teardown());
        assert_eq!(p.borrow().syncs.len(), 1, "a's session is gone; b's handle keeps the timeline");
        b.sync_signal(tb, 2).unwrap();
        b.sync_destroy(tb).unwrap();
        assert_eq!(p.borrow().syncs.len(), 0, "the last holder's destroy frees it");
        let t2 = b.sync_create(0).unwrap();
        let id2 = b.sync_share(t2).unwrap();
        b.backend.shared_sync_release(id2);
        assert!(b.teardown());
        assert_eq!(p.borrow().syncs.len(), 0, "teardown releases what the device holds");
    }

    #[test]
    fn sharing_is_refused_without_a_backend_that_can_and_for_unknown_things() {
        let mut solo = dev();
        let t = solo.sync_create(0).unwrap();
        assert_eq!(solo.sync_share(t), Err(Error::Inval), "a backend with no shared timelines");
        assert_eq!(solo.sync_import(1), Err(Error::NoEnt));
        let (mut a, mut b, _) = shared_pair(1 << 20, 1 << 20);
        assert_eq!(a.sync_share(77), Err(Error::NoEnt));
        assert_eq!(b.sync_import(12345), Err(Error::NoEnt), "a timeline that does not exist");
    }

    #[test]
    fn a_wait_on_an_imported_timeline_is_satisfied_by_the_other_devices_work() {
        let p = pool(1 << 20, 1 << 20);
        let (mut a, mut b) = (holding_device(0, &p), holding_device(1, &p));
        let ca = ready(&mut a);
        let cb = ready(&mut b);
        let ta = a.sync_create(0).unwrap();
        let id = a.sync_share(ta).unwrap();
        let tb = b.sync_import(id).unwrap();
        a.exec(ca, &[push_at(&a)], &[], &[sr(ta, 1)]).unwrap();
        // b's EXEC waits for a's work: not ready, then ready once a's fence completes
        assert_eq!(b.waits_ready(&[sr(tb, 1)], false, false).unwrap(), None);
        complete(&mut a);
        assert_eq!(b.waits_ready(&[sr(tb, 1)], false, false).unwrap(), Some(0));
        b.exec(cb, &[push_at(&b)], &[sr(tb, 1)], &[]).unwrap();
    }

    #[test]
    fn a_vram_bo_says_where_it_is_and_a_system_one_does_not() {
        let mut d = Device::new(SoftBackend::default(), layout());
        let (v, _, vs) = d.bo_create(0x5000, BO_VRAM).unwrap();
        let (s, _, _) = d.bo_create(0x1000, BO_SYSTEM).unwrap();
        let (off, size) = d.vram_bo(v).unwrap();
        assert_eq!(size, vs);
        assert_eq!(off % PAGE, 0);
        assert_eq!(d.vram_bo(s), None);
        assert_eq!(d.vram_bo(0xdead), None);
        d.bo_free(v).unwrap();
        assert_eq!(d.vram_bo(v), None);
    }
}
