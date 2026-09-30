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

    pub fn vram_used(&self) -> u64 {
        self.vram.used()
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
                let off = self.arena.alloc(size, PAGE).ok_or(Error::NoMem)?;
                (Backing::System { arena_off: off }, off)
            }
            uapi::BO_VRAM => {
                let off = self.vram.alloc(size, PAGE).ok_or(Error::NoMem)?;
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
        match backing {
            Backing::System { arena_off } => self.arena.free(arena_off, size),
            Backing::Vram { vram_off } => self.vram.free(vram_off, size),
        }
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
        self.syncs.insert(h, Timeline { value: initial, pending: initial });
        self.next_sync = h.wrapping_add(1);
        Ok(h)
    }

    pub fn sync_destroy(&mut self, handle: u32) -> Result<(), Error> {
        self.syncs.remove(&handle).map(|_| ()).ok_or(Error::NoEnt)
    }

    /// CPU signal: the value must not go backwards (Vulkan timeline semantics).
    pub fn sync_signal(&mut self, handle: u32, value: u64) -> Result<(), Error> {
        let t = self.syncs.get_mut(&handle).ok_or(Error::NoEnt)?;
        if value < t.value {
            return Err(Error::Inval);
        }
        t.value = value;
        t.pending = t.pending.max(value);
        Ok(())
    }

    /// `(value, pending)`.
    pub fn sync_query(&mut self, handle: u32) -> Result<(u64, u64), Error> {
        self.poll();
        self.syncs.get(&handle).map(|t| (t.value, t.pending)).ok_or(Error::NoEnt)
    }

    /// The index of a ready reference (`any`), or of the first one when all are (`!any`, `Some(0)` for none); `None` while not ready.
    /// `pending` compares with each timeline's pending value instead of its completed one. A reference to a destroyed timeline is
    /// an error.
    pub fn waits_ready(&mut self, refs: &[SyncRef], any: bool, pending: bool) -> Result<Option<usize>, Error> {
        self.poll();
        let mut first = None;
        for (i, r) in refs.iter().enumerate() {
            let t = *self.syncs.get(&r.handle).ok_or(Error::NoEnt)?;
            let v = if pending { t.pending } else { t.value };
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
            // `exec` raised `pending` to this value when the work was queued, so `pending >= value` still holds.
            t.value = t.value.max(s.value);
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
        if !signals.is_empty() {
            for s in signals {
                if let Some(t) = self.syncs.get_mut(&s.handle) {
                    t.pending = t.pending.max(s.value);
                }
            }
            self.pending.push(Pending { ctx, seq, signals: signals.to_vec() });
        }
        self.poll();
        Ok(())
    }

    /// Drop everything (the device was closed): unbind, complete, release. Leaves the model empty and reusable.
    pub fn teardown(&mut self) {
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
        self.syncs.clear();
        self.pending.clear();
        let allocs: Vec<(u64, u64)> = self.va_allocs.iter().map(|(&s, &l)| (s, l)).collect();
        for (s, l) in allocs {
            self.va_allocs.remove(&s);
            self.va.free(s, l);
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
}
