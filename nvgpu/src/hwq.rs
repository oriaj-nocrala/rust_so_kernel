//! `nvgpu::hwq` — the pure half of the hardware backend of `/dev/nvgpu` (G4c of `docs/gpu/g4-nvkmd-plan.md`).
//!
//! The device model ([`crate::devmodel`]) decides *what* is allowed; the [`Backend`](crate::devmodel::Backend) does it. On the
//! hardware that means three things, each of which is data here and I/O in `kernel/src/gpu/uapi.rs`:
//!
//! - **Page tables at run time** ([`bind_range`], [`unbind_range`], [`PageTables::take_dirty`](crate::mmu::PageTables::take_dirty)):
//!   a bind or unbind changes a few tables; the adapter writes those images into VRAM and flushes the GPU's TLB
//!   ([`tlb_flush_regs`]).
//! - **The channel's ring** ([`Queue`]): one GPFIFO shared by every context. An `EXEC` becomes one GPFIFO entry per push segment
//!   and one more that runs a kernel-owned *fence push* (wait for idle, release a semaphore); the semaphore lives in host memory, so
//!   the CPU learns of completion, and of how many ring entries the GPU has consumed, without reading VRAM (BAR1 cannot read it
//!   after GSP-RM boots).
//! - **Fences** ([`Queue::observe`]): the semaphore is 32 bits, the sequence numbers 64; the queue extends the former.
//!
//! Nothing here blocks: a full ring is [`Error::Again`] and the caller retries.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::chan::gp_entry;
use crate::devmodel::Error;
use crate::mmu::{Flags, MapError, PageTables, Target};
use crate::uapi::{Push, PUSH_NO_PREFETCH};

/// `NVC56F_GP_ENTRY1_SYNC_WAIT` (`clc56f.h:276-278`): the GPU finishes everything before this entry's push before it fetches it.
/// It is bit 31 of the entry's second word.
const GP_ENTRY_SYNC_WAIT: u64 = 1 << 63;

// ---- VRAM for user buffers --------------------------------------------------------------------------------------------------

/// The part of VRAM user space's buffer objects come from: `[1 GiB, 7.5 GiB)`. Everything the kernel puts in VRAM is outside it: the
/// GOP framebuffer, scanout buffers, page tables, the channels and the copy/compute test areas are below 256 MiB, the GR
/// context buffers start at `0x1_f000_0000` (7.75 GiB), and GSP-RM's heap and WPR2 are above that.
pub const USER_VRAM_BASE: u64 = 1 << 30;
pub const USER_VRAM_END: u64 = 0x1_e000_0000;
pub const USER_VRAM_BYTES: u64 = USER_VRAM_END - USER_VRAM_BASE;

/// The physical VRAM address of byte `vram_off` of the user heap.
pub fn user_vram_pa(vram_off: u64) -> u64 {
    USER_VRAM_BASE + vram_off
}

// ---- page tables at run time ------------------------------------------------------------------------------------------------

/// Where the pages of a bind come from.
pub enum Source<'a> {
    /// VRAM: the heap offset `off` is at `USER_VRAM_BASE + off`, contiguous.
    Vram { vram_off: u64 },
    /// System memory: `frame(byte_offset_in_bo)` is the bus address of the page holding that byte (`None`: no memory for it).
    System(&'a mut dyn FnMut(u64) -> Option<u64>),
}

fn map_error(e: MapError) -> Error {
    match e {
        MapError::Unaligned | MapError::OutOfRange => Error::Inval,
        MapError::NoTables => Error::NoMem,
        MapError::AlreadyMapped | MapError::Overlap => Error::Exist,
    }
}

/// Map `size` bytes at `va` to the pages of `src` starting `bo_off` bytes into the BO, with 4 KiB pages (64 KiB and 2 MiB pages
/// are for later: the tables hold whatever the model asked for and unbinding needs no page size). All or nothing: when a page
/// cannot be mapped, the ones already mapped by this call are unmapped again and the error says why (tables the failed attempt
/// allocated stay in the pool, ready for the next bind in the same 2 MiB).
pub fn bind_range(pt: &mut PageTables, va: u64, size: u64, bo_off: u64, src: Source, kind: u8) -> Result<(), Error> {
    if va & 0xfff != 0 || size & 0xfff != 0 || bo_off & 0xfff != 0 || size == 0 {
        return Err(Error::Inval);
    }
    let flags = Flags { read_only: false, privileged: false, kind };
    let mut src = src;
    let mut done = 0;
    let mut fail = None;
    while done < size {
        let (pa, target) = match &mut src {
            Source::Vram { vram_off } => (user_vram_pa(*vram_off + bo_off + done), Target::Vram),
            Source::System(frame) => match frame(bo_off + done) {
                Some(pa) => (pa, Target::Host),
                None => {
                    fail = Some(Error::NoMem);
                    break;
                }
            },
        };
        if let Err(e) = pt.map(va + done, pa, target, flags) {
            fail = Some(map_error(e));
            break;
        }
        done += 0x1000;
    }
    match fail {
        None => Ok(()),
        Some(e) => {
            unbind_range(pt, va, done);
            Err(e)
        }
    }
}

/// Unmap `[va, va + size)`: every page that is mapped goes, gaps are fine. Returns how many pages were unmapped.
pub fn unbind_range(pt: &mut PageTables, va: u64, size: u64) -> u64 {
    let mut n = 0;
    let mut off = 0;
    while off < size {
        if pt.unmap(va + off) {
            n += 1;
        }
        off += 0x1000;
    }
    n
}

/// The bus address of the page a bind mapped at `va`, and the aperture, through the images: what the GPU will walk. For tests and
/// for the adapter's self-check.
pub fn resolve(pt: &PageTables, va: u64) -> Option<(u64, Target)> {
    let (pa, pte) = pt.translate(va)?;
    let target = match (pte >> 1) & 3 {
        0 => Target::Vram,
        2 => Target::Host,
        _ => Target::NonCoherent,
    };
    Some((pa, target))
}

/// The GPU's TLB invalidation after a page-table change: the registers to write, in order, and the bit to poll. `root` is the
/// physical address of the page directory (VRAM). Turing and later (`tu102_vmm_flush`, `vmmtu102.c:26-51`; fields in NVIDIA's
/// `dev_vm.ref` `NV_VIRTUAL_FUNCTION_PRIV_MMU_INVALIDATE*`): the PDB (address bits 31:4 = `root >> 8`, aperture VID_MEM), its upper
/// half, then the trigger with ALL_VA and ALL_PDB (no address to name, no PDB to match), which the hardware clears when it is done.
pub fn tlb_flush_regs(root: u64) -> ([(u32, u32); 3], u32) {
    const VF: u32 = 0xb8_0000;
    let pdb = VF + 0x30a0;
    let upper = VF + 0x30a4;
    let trigger = VF + 0x30b0;
    const ALL_VA: u32 = 1 << 0;
    const ALL_PDB: u32 = 1 << 1;
    const TRIGGER: u32 = 1 << 31;
    ([(pdb, (root >> 8) as u32), (upper, (root >> 40) as u32), (trigger, TRIGGER | ALL_VA | ALL_PDB)], trigger)
}

// ---- the channel's ring and its fences --------------------------------------------------------------------------------------

/// What [`Queue::plan`] decided for one `EXEC`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The sequence number this submission completes as (`fence_done(seq)` becomes true when it has run).
    pub seq: u64,
    /// What the fence push writes into the semaphore: `seq` truncated to 32 bits.
    pub payload: u32,
    /// The GPFIFO entries to write: (ring index, entry), the prelude (if the queue has one), the push segments in order and the fence
    /// entry last.
    pub entries: Vec<(u32, u64)>,
    /// Which of the kernel's fence-push slots holds this submission's fence push (`slot * slot_bytes` from the slot area).
    pub fence_slot: u32,
    /// The value to write to `GP_PUT` (the ring index after the last entry).
    pub gp_put: u32,
}

#[derive(Debug, Clone, Copy)]
struct Flight {
    seq: u64,
    /// `Queue::put` after this submission's entries: once its fence is seen, the GPU has consumed everything below it.
    put_end: u64,
}

/// The ring and the fence bookkeeping of one channel.
#[derive(Debug)]
pub struct Queue {
    entries: u32,
    fence_slots: u32,
    /// The fence push's size in bytes, and the VA of its slot area (slot `i` at `+ i * slot_bytes`).
    fence_bytes: u32,
    slot_bytes: u32,
    slot_va: u64,
    /// A kernel push run before the caller's pushes (0 = none), in the same slot after the fence push: it binds the engine's class
    /// to its subchannel, which the copy-engine contexts of NVK never do themselves.
    prelude_bytes: u32,
    /// GPFIFO entries written so far (free running; the ring index is `put % entries`).
    put: u64,
    /// Entries the GPU is known to have consumed: those before the last completed submission.
    consumed: u64,
    /// The sequence number the next submission gets (the first is 1).
    next_seq: u64,
    /// The highest sequence number whose fence has been seen.
    done: u64,
    inflight: VecDeque<Flight>,
}

impl Queue {
    /// A ring of `entries` GPFIFO entries (a power of two) whose write position is already `put` (`GP_PUT`: the boot's own
    /// pushes were all waited for, so the GPU has consumed them), with `fence_slots` slots of `slot_bytes` at VA `slot_va` for
    /// the fence pushes, each `fence_bytes` long, and (when `prelude_bytes` > 0) a prelude push behind each, also in the slot.
    pub fn new(entries: u32, put: u32, fence_slots: u32, slot_va: u64, slot_bytes: u32, fence_bytes: u32, prelude_bytes: u32) -> Self {
        assert!(entries.is_power_of_two() && put < entries && fence_slots >= 1 && fence_bytes + prelude_bytes <= slot_bytes && slot_bytes % 4 == 0);
        assert!(fence_bytes % 4 == 0 && prelude_bytes % 4 == 0);
        Queue { entries, fence_slots, fence_bytes, slot_bytes, slot_va, prelude_bytes, put: put as u64, consumed: put as u64, next_seq: 1, done: 0, inflight: VecDeque::new() }
    }

    /// Submissions queued and not yet seen complete.
    pub fn in_flight(&self) -> usize {
        self.inflight.len()
    }

    /// The last sequence number handed out (0 before the first submission).
    pub fn last_seq(&self) -> u64 {
        self.next_seq - 1
    }

    /// The highest sequence number known complete.
    pub fn done_seq(&self) -> u64 {
        self.done
    }

    /// Free ring entries (the ring is full one entry short of its size: `GP_PUT + 1 == GP_GET` means full).
    pub fn free_entries(&self) -> u32 {
        (self.entries as u64 - 1 - (self.put - self.consumed)) as u32
    }

    /// The ring index `GP_PUT` holds now.
    pub fn gp_put(&self) -> u32 {
        (self.put % self.entries as u64) as u32
    }

    /// Decide where an `EXEC`'s pushes go: one GPFIFO entry per segment, then the fence entry. [`Error::Again`] when the ring or
    /// the fence slots are all in use (nothing changes: the caller retries after a fence completes); the pushes are the model's
    /// validated ones. The caller writes the fence push into its slot, the entries, `GP_PUT`, and rings the doorbell.
    pub fn plan(&mut self, pushes: &[Push]) -> Result<Plan, Error> {
        let need = pushes.len() as u64 + 1 + (self.prelude_bytes > 0) as u64;
        if need > self.entries as u64 - 1 {
            return Err(Error::Inval);
        }
        if self.inflight.len() as u32 >= self.fence_slots || need > self.free_entries() as u64 {
            return Err(Error::Again);
        }
        let seq = self.next_seq;
        let slot = (seq % self.fence_slots as u64) as u32;
        let mut entries = Vec::with_capacity(need as usize);
        if self.prelude_bytes > 0 {
            // behind the fence push in the same slot; it must run before anything of the caller's, and the previous submission's fence
            // entry (SYNC_WAIT) already made everything before it finish
            entries.push(gp_entry(self.slot_va + slot as u64 * self.slot_bytes as u64 + self.fence_bytes as u64, self.prelude_bytes));
        }
        for p in pushes {
            let mut e = gp_entry(p.va, p.bytes);
            if p.flags & PUSH_NO_PREFETCH != 0 {
                e |= GP_ENTRY_SYNC_WAIT;
            }
            entries.push(e);
        }
        // the fence push must not be fetched ahead of the work it follows: SYNC_WAIT, and its own WAIT_FOR_IDLE inside
        entries.push(gp_entry(self.slot_va + slot as u64 * self.slot_bytes as u64, self.fence_bytes) | GP_ENTRY_SYNC_WAIT);
        let placed: Vec<(u32, u64)> = entries.into_iter().enumerate().map(|(i, e)| (((self.put + i as u64) % self.entries as u64) as u32, e)).collect();
        self.put += need;
        self.next_seq += 1;
        self.inflight.push_back(Flight { seq, put_end: self.put });
        Ok(Plan { seq, payload: seq as u32, entries: placed, fence_slot: slot, gp_put: self.gp_put() })
    }

    /// The semaphore now holds `sem`. Extends it to 64 bits (it can only move forward, and never past the last submission),
    /// retires the submissions it completes and returns whether anything completed. A value that cannot be a payload of ours (ahead
    /// of the last submission, or behind the last completed one) is ignored: the page is host memory a wild shader could write.
    pub fn observe(&mut self, sem: u32) -> bool {
        let ahead = sem.wrapping_sub(self.done as u32) as u64;
        if ahead == 0 || ahead > self.last_seq() - self.done {
            return false;
        }
        self.done += ahead;
        while let Some(f) = self.inflight.front() {
            if f.seq > self.done {
                break;
            }
            self.consumed = f.put_end;
            self.inflight.pop_front();
        }
        true
    }

    /// Whether the fence of `seq` has completed (as of the last [`observe`](Self::observe)).
    pub fn is_done(&self, seq: u64) -> bool {
        seq <= self.done
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chan::gp_entry;
    use crate::mmu::PageTables;

    const SLOT_VA: u64 = 0x3_8001_0000;

    fn queue(entries: u32, slots: u32) -> Queue {
        Queue::new(entries, 0, slots, SLOT_VA, 64, crate::gr::FENCE_PUSH_BYTES, 0)
    }

    fn push(va: u64, bytes: u32) -> Push {
        Push { va, bytes, flags: 0 }
    }

    // ---- the ring ----

    #[test]
    fn a_submission_is_its_pushes_then_the_fence() {
        let mut q = queue(16, 4);
        let p = q.plan(&[push(0x1000_0000, 64), push(0x1000_1000, 128)]).unwrap();
        assert_eq!(p.seq, 1);
        assert_eq!(p.payload, 1);
        assert_eq!(p.entries.len(), 3);
        assert_eq!(p.entries[0], (0, gp_entry(0x1000_0000, 64)));
        assert_eq!(p.entries[1], (1, gp_entry(0x1000_1000, 128)));
        // the fence push sits in its slot, is FENCE_PUSH_BYTES long, and waits for the work before it
        assert_eq!(p.entries[2], (2, gp_entry(SLOT_VA + 64, crate::gr::FENCE_PUSH_BYTES) | GP_ENTRY_SYNC_WAIT));
        assert_eq!(p.fence_slot, 1);
        assert_eq!(p.gp_put, 3);
        assert_eq!(q.gp_put(), 3);
    }

    #[test]
    fn a_prelude_runs_first_from_the_slot_behind_the_fence_push() {
        let mut q = Queue::new(16, 0, 4, SLOT_VA, 64, 36, 8);
        let p = q.plan(&[push(0x1000_0000, 64)]).unwrap();
        // prelude, the push, the fence
        assert_eq!(p.entries.len(), 3);
        assert_eq!(p.entries[0], (0, gp_entry(SLOT_VA + 64 + 36, 8)));
        assert_eq!(p.entries[1], (1, gp_entry(0x1000_0000, 64)));
        assert_eq!(p.entries[2], (2, gp_entry(SLOT_VA + 64, 36) | GP_ENTRY_SYNC_WAIT));
        assert_eq!(p.gp_put, 3);
        // one more entry per submission: a 6-segment EXEC (6 + prelude + fence = 8) does not fit a ring of 8 (7 usable)
        let mut q = Queue::new(8, 0, 4, SLOT_VA, 64, 36, 8);
        let six: Vec<Push> = (0..6).map(|i| push(0x1000 * (i + 1), 16)).collect();
        assert_eq!(q.plan(&six), Err(Error::Inval));
        q.plan(&six[..5]).unwrap();
        // and the space it took comes back with the fence
        assert_eq!(q.free_entries(), 0);
        assert!(q.observe(1));
        assert_eq!(q.free_entries(), 7);
    }

    #[test]
    #[should_panic]
    fn a_prelude_that_does_not_fit_its_slot_is_a_bug() {
        Queue::new(16, 0, 4, SLOT_VA, 64, 36, 32);
    }

    #[test]
    fn no_prefetch_sets_the_sync_wait_bit_of_that_entry_only() {
        let mut q = queue(16, 4);
        let p = q.plan(&[Push { va: 0x1000, bytes: 16, flags: PUSH_NO_PREFETCH }, push(0x2000, 16)]).unwrap();
        assert_eq!(p.entries[0].1, gp_entry(0x1000, 16) | (1 << 63));
        assert_eq!(p.entries[1].1, gp_entry(0x2000, 16));
        // bit 31 of the second word is `SYNC` (`clc56f.h:276`)
        assert_eq!((p.entries[0].1 >> 32) as u32 >> 31, 1);
    }

    #[test]
    fn an_empty_exec_is_only_the_fence() {
        let mut q = queue(16, 4);
        let p = q.plan(&[]).unwrap();
        assert_eq!(p.entries.len(), 1);
        assert_eq!(p.gp_put, 1);
    }

    #[test]
    fn the_ring_wraps_and_indexes_follow_put() {
        let mut q = Queue::new(8, 6, 4, SLOT_VA, 64, crate::gr::FENCE_PUSH_BYTES, 0);
        let p = q.plan(&[push(0x1000, 16), push(0x2000, 16)]).unwrap();
        // 6, 7, then back to 0
        assert_eq!(p.entries.iter().map(|e| e.0).collect::<Vec<_>>(), vec![6, 7, 0]);
        assert_eq!(p.gp_put, 1);
    }

    #[test]
    fn a_full_ring_asks_to_retry_and_changes_nothing() {
        let mut q = queue(8, 8);
        // 7 free entries (one is kept empty): a 3-push exec takes 4, a second one does not fit
        q.plan(&[push(0x1000, 16), push(0x2000, 16), push(0x3000, 16)]).unwrap();
        assert_eq!(q.free_entries(), 3);
        let before = (q.gp_put(), q.last_seq(), q.in_flight());
        assert_eq!(q.plan(&[push(0x4000, 16), push(0x5000, 16), push(0x6000, 16)]), Err(Error::Again));
        assert_eq!((q.gp_put(), q.last_seq(), q.in_flight()), before);
        // once the first fence is seen the space comes back
        assert!(q.observe(1));
        assert_eq!(q.free_entries(), 7);
        q.plan(&[push(0x4000, 16), push(0x5000, 16), push(0x6000, 16)]).unwrap();
    }

    #[test]
    fn an_exec_that_can_never_fit_is_invalid_not_retryable() {
        let mut q = queue(8, 8);
        let too_many: Vec<Push> = (0..7).map(|i| push(0x1000 * (i + 1), 16)).collect();
        assert_eq!(q.plan(&too_many), Err(Error::Inval));
        // 6 segments + the fence = 7 = the whole ring
        q.plan(&too_many[..6]).unwrap();
    }

    #[test]
    fn the_fence_slots_bound_what_is_in_flight() {
        let mut q = queue(64, 3);
        for i in 1..=3 {
            let p = q.plan(&[]).unwrap();
            assert_eq!(p.seq, i);
            assert_eq!(p.fence_slot, (i % 3) as u32);
        }
        assert_eq!(q.plan(&[]), Err(Error::Again));
        assert!(q.observe(1));
        // slot of seq 4 is 1: the slot seq 1 used, free now that its fence was seen
        let p = q.plan(&[]).unwrap();
        assert_eq!((p.seq, p.fence_slot), (4, 1));
    }

    #[test]
    fn slots_in_flight_never_collide() {
        let mut q = queue(64, 4);
        let mut live: Vec<(u64, u32)> = Vec::new();
        let mut seen = 0u32;
        for round in 0..200u32 {
            while let Ok(p) = q.plan(&[]) {
                assert!(live.iter().all(|&(_, s)| s != p.fence_slot), "slot {} reused while in flight", p.fence_slot);
                live.push((p.seq, p.fence_slot));
            }
            // the GPU finishes one or two per round
            seen += 1 + round % 2;
            let last = live.last().unwrap().0 as u32;
            seen = seen.min(last);
            q.observe(seen);
            live.retain(|&(s, _)| s > seen as u64);
        }
    }

    // ---- fences ----

    #[test]
    fn a_fence_is_done_when_the_semaphore_reaches_it() {
        let mut q = queue(16, 4);
        let a = q.plan(&[]).unwrap().seq;
        let b = q.plan(&[]).unwrap().seq;
        assert!(!q.is_done(a));
        assert!(q.observe(1));
        assert!(q.is_done(a) && !q.is_done(b));
        // seeing the same value again changes nothing
        assert!(!q.observe(1));
        assert!(q.observe(2));
        assert!(q.is_done(b));
        assert_eq!(q.in_flight(), 0);
    }

    #[test]
    fn one_observation_can_complete_several() {
        let mut q = queue(16, 8);
        for _ in 0..5 {
            q.plan(&[push(0x1000, 16)]).unwrap();
        }
        // 10 entries used, none consumed yet
        assert_eq!(q.free_entries(), 15 - 10);
        assert!(q.observe(4));
        assert_eq!(q.done_seq(), 4);
        assert_eq!(q.in_flight(), 1);
        // 8 entries (4 submissions x 2) consumed
        assert_eq!(q.free_entries(), 15 - 2);
    }

    #[test]
    fn a_semaphore_value_that_cannot_be_ours_is_ignored() {
        let mut q = queue(16, 4);
        q.plan(&[]).unwrap();
        q.plan(&[]).unwrap();
        // ahead of everything submitted
        assert!(!q.observe(3));
        assert!(!q.observe(0xdead_beef));
        assert_eq!(q.done_seq(), 0);
        assert!(q.observe(2));
        // behind what is done
        assert!(!q.observe(1));
        assert_eq!(q.done_seq(), 2);
    }

    #[test]
    fn sequence_numbers_extend_the_32_bit_semaphore_across_the_wrap() {
        let mut q = queue(16, 4);
        // start just below 2^32: pretend 0xffff_fffd submissions were done
        q.next_seq = 0xffff_fffe;
        q.done = 0xffff_fffd;
        let a = q.plan(&[]).unwrap();
        let b = q.plan(&[]).unwrap();
        let c = q.plan(&[]).unwrap();
        assert_eq!((a.payload, b.payload, c.payload), (0xffff_fffe, 0xffff_ffff, 0));
        assert!(q.observe(0xffff_ffff));
        assert!(q.is_done(b.seq) && !q.is_done(c.seq));
        assert!(q.observe(0));
        assert!(q.is_done(c.seq));
        assert_eq!(q.done_seq(), 0x1_0000_0000);
    }

    // ---- binding ----

    fn tables() -> PageTables {
        PageTables::new(64 << 20, 64, Target::Vram)
    }

    const VA: u64 = 0x10_0000_0000;

    #[test]
    fn a_system_bind_maps_each_page_to_its_frame() {
        let mut pt = tables();
        // frames scattered on purpose: page i of the BO is at 0x2000_0000 + (7 - i) * 0x1000
        let mut frame = |off: u64| Some(0x2000_0000 + (7 - off / 0x1000) * 0x1000);
        bind_range(&mut pt, VA, 0x8000, 0, Source::System(&mut frame), 0).unwrap();
        for i in 0..8u64 {
            assert_eq!(resolve(&pt, VA + i * 0x1000), Some((0x2000_0000 + (7 - i) * 0x1000, Target::Host)));
        }
        assert_eq!(resolve(&pt, VA + 0x8000), None);
        // a byte inside a page resolves inside its frame
        assert_eq!(pt.translate(VA + 0x1234).unwrap().0, 0x2000_0000 + 6 * 0x1000 + 0x234);
    }

    #[test]
    fn a_bind_starts_bo_off_bytes_into_the_bo() {
        let mut pt = tables();
        let mut seen = Vec::new();
        let mut frame = |off: u64| {
            seen.push(off);
            Some(0x3000_0000 + off)
        };
        bind_range(&mut pt, VA, 0x2000, 0x5000, Source::System(&mut frame), 0).unwrap();
        assert_eq!(seen, vec![0x5000, 0x6000]);
        assert_eq!(resolve(&pt, VA), Some((0x3000_5000, Target::Host)));
        assert_eq!(resolve(&pt, VA + 0x1000), Some((0x3000_6000, Target::Host)));
    }

    #[test]
    fn a_vram_bind_is_contiguous_from_the_user_heap() {
        let mut pt = tables();
        bind_range(&mut pt, VA, 0x3000, 0x1000, Source::Vram { vram_off: 0x40_0000 }, 0).unwrap();
        assert_eq!(resolve(&pt, VA), Some((USER_VRAM_BASE + 0x40_0000 + 0x1000, Target::Vram)));
        assert_eq!(resolve(&pt, VA + 0x2000), Some((USER_VRAM_BASE + 0x40_0000 + 0x3000, Target::Vram)));
    }

    #[test]
    fn the_kind_reaches_the_pte() {
        let mut pt = tables();
        bind_range(&mut pt, VA, 0x1000, 0, Source::Vram { vram_off: 0 }, 0xfe).unwrap();
        let (_, pte) = pt.translate(VA).unwrap();
        assert_eq!(pte >> 56, 0xfe);
    }

    #[test]
    fn a_failed_bind_leaves_nothing_mapped() {
        let mut pt = tables();
        // the fifth page has no frame
        let mut frame = |off: u64| (off < 0x4000).then_some(0x2000_0000 + off);
        assert_eq!(bind_range(&mut pt, VA, 0x8000, 0, Source::System(&mut frame), 0), Err(Error::NoMem));
        for i in 0..8 {
            assert_eq!(resolve(&pt, VA + i * 0x1000), None, "page {} is still mapped", i);
        }
        // and the same range binds fine afterwards
        let mut frame = |off: u64| Some(0x2000_0000 + off);
        bind_range(&mut pt, VA, 0x8000, 0, Source::System(&mut frame), 0).unwrap();
    }

    #[test]
    fn running_out_of_tables_is_enomem_and_rolls_back() {
        // a pool of 5: the root, PD2, PD1, PD0 and one PT; a second 2 MiB needs a sixth table
        let mut pt = PageTables::new(64 << 20, 5, Target::Vram);
        let r = bind_range(&mut pt, VA + 0x1f_f000, 0x2000, 0, Source::Vram { vram_off: 0 }, 0);
        assert_eq!(r, Err(Error::NoMem));
        // the page before the 2 MiB boundary was mapped by the call and is gone again
        assert_eq!(resolve(&pt, VA + 0x1f_f000), None);
    }

    #[test]
    fn unaligned_or_empty_binds_are_invalid() {
        let mut pt = tables();
        for (va, size, off) in [(VA + 8, 0x1000, 0), (VA, 0x1001, 0), (VA, 0x1000, 8), (VA, 0, 0)] {
            assert_eq!(bind_range(&mut pt, va, size, off, Source::Vram { vram_off: 0 }, 0), Err(Error::Inval));
        }
    }

    #[test]
    fn a_system_bind_needs_an_aligned_bo_offset_too() {
        // the frames are page aligned whatever the offset, so only the check says no: a bind at byte 8 of a BO would map page 0 silently
        let mut pt = tables();
        let mut called = false;
        let mut frame = |off: u64| {
            called = true;
            Some(0x2000_0000 + off)
        };
        assert_eq!(bind_range(&mut pt, VA, 0x1000, 8, Source::System(&mut frame), 0), Err(Error::Inval));
        assert!(!called);
        assert_eq!(resolve(&pt, VA), None);
    }

    #[test]
    fn a_page_that_cannot_be_mapped_for_its_address_is_invalid() {
        let mut pt = tables();
        // an unaligned heap offset makes an unaligned physical address; a VA past 49 bits is out of range
        assert_eq!(bind_range(&mut pt, VA, 0x1000, 0, Source::Vram { vram_off: 8 }, 0), Err(Error::Inval));
        assert_eq!(bind_range(&mut pt, 1 << 49, 0x1000, 0, Source::Vram { vram_off: 0 }, 0), Err(Error::Inval));
        assert_eq!(resolve(&pt, VA), None);
    }

    #[test]
    fn user_mappings_are_valid_writable_and_unprivileged() {
        let mut pt = tables();
        bind_range(&mut pt, VA, 0x1000, 0, Source::Vram { vram_off: 0 }, 0).unwrap();
        let mut frame = |_: u64| Some(0x2000_0000);
        bind_range(&mut pt, VA + 0x1000, 0x1000, 0, Source::System(&mut frame), 0).unwrap();
        for va in [VA, VA + 0x1000] {
            let (_, pte) = pt.translate(va).unwrap();
            // VALID (0), PRIV (5), RO (6): `gp100_vmm_valid` / NVIDIA's dev_mmu.ref
            assert_eq!(pte & 1, 1);
            assert_eq!(pte & (1 << 5), 0, "a user page must not be privileged");
            assert_eq!(pte & (1 << 6), 0, "a user page must be writable");
        }
    }

    #[test]
    fn binding_over_a_mapping_is_eexist_and_keeps_the_first() {
        let mut pt = tables();
        bind_range(&mut pt, VA, 0x2000, 0, Source::Vram { vram_off: 0 }, 0).unwrap();
        let r = bind_range(&mut pt, VA + 0x1000, 0x2000, 0, Source::Vram { vram_off: 0x10_0000 }, 0);
        assert_eq!(r, Err(Error::Exist));
        // the overlapped page is still the first bind's, the page the failed call mapped is not there
        assert_eq!(resolve(&pt, VA + 0x1000), Some((USER_VRAM_BASE + 0x1000, Target::Vram)));
        assert_eq!(resolve(&pt, VA + 0x2000), None);
    }

    #[test]
    fn unbind_removes_what_is_mapped_and_skips_gaps() {
        let mut pt = tables();
        bind_range(&mut pt, VA, 0x2000, 0, Source::Vram { vram_off: 0 }, 0).unwrap();
        bind_range(&mut pt, VA + 0x4000, 0x1000, 0, Source::Vram { vram_off: 0x10_0000 }, 0).unwrap();
        assert_eq!(unbind_range(&mut pt, VA, 0x8000), 3);
        for i in 0..8 {
            assert_eq!(resolve(&pt, VA + i * 0x1000), None);
        }
        // a cut in the middle
        bind_range(&mut pt, VA, 0x4000, 0, Source::Vram { vram_off: 0 }, 0).unwrap();
        assert_eq!(unbind_range(&mut pt, VA + 0x1000, 0x2000), 2);
        assert!(resolve(&pt, VA).is_some() && resolve(&pt, VA + 0x3000).is_some());
        assert!(resolve(&pt, VA + 0x1000).is_none() && resolve(&pt, VA + 0x2000).is_none());
    }

    // ---- dirty tables ----

    #[test]
    fn only_the_tables_a_bind_touched_are_reported() {
        let mut pt = tables();
        // a fresh tree reports its root
        assert_eq!(pt.take_dirty().len(), 1);
        assert!(pt.take_dirty().is_empty());
        bind_range(&mut pt, VA, 0x2000, 0, Source::Vram { vram_off: 0 }, 0).unwrap();
        // root, PD2, PD1, PD0, PT
        let d = pt.take_dirty();
        assert_eq!(d.len(), 5);
        assert_eq!(d[0].0, 64 << 20);
        assert!(pt.take_dirty().is_empty());
        // a second bind in the same 2 MiB writes only the PT
        bind_range(&mut pt, VA + 0x10_0000, 0x1000, 0, Source::Vram { vram_off: 0x10_0000 }, 0).unwrap();
        let d = pt.take_dirty();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].0, (64 << 20) + 4 * 0x1000);
        // an unbind rewrites that PT (the entry is zero now)
        unbind_range(&mut pt, VA + 0x10_0000, 0x1000);
        let d = pt.take_dirty();
        assert_eq!(d.len(), 1);
        // the PT's entry 0 (VA) is still there, its entry 256 (VA + 1 MiB) is gone
        assert_ne!(u64::from_le_bytes(d[0].1[0..8].try_into().unwrap()), 0);
        assert_eq!(u64::from_le_bytes(d[0].1[0x800..0x808].try_into().unwrap()), 0);
    }

    #[test]
    fn a_table_allocated_by_a_failed_bind_is_still_written_out() {
        // VRAM holds garbage where the pool was never written: a directory entry that points at a new, still empty table must see
        // zeros there, so every allocated table is reported, empty or not
        let mut pt = PageTables::new(64 << 20, 4, Target::Vram);
        pt.take_dirty();
        assert_eq!(bind_range(&mut pt, VA, 0x1000, 0, Source::Vram { vram_off: 0 }, 0), Err(Error::NoMem));
        let d = pt.take_dirty();
        // root, PD2, PD1 (each got an entry) and PD0 (new and empty: the PT it needs does not fit)
        assert_eq!(d.iter().map(|t| t.0).collect::<Vec<_>>(), vec![64 << 20, (64 << 20) + 0x1000, (64 << 20) + 0x2000, (64 << 20) + 0x3000]);
        assert!(d[3].1.iter().all(|&b| b == 0));
    }

    #[test]
    fn the_dirty_images_are_the_tables_the_gpu_walks() {
        // writing exactly what take_dirty returned, over an all-zero VRAM, gives tables that translate like the model's
        let mut pt = tables();
        let mut vram: std::collections::BTreeMap<u64, [u8; 4096]> = Default::default();
        let flush = |pt: &mut PageTables, vram: &mut std::collections::BTreeMap<u64, [u8; 4096]>| {
            for (pa, img) in pt.take_dirty() {
                vram.insert(pa, img);
            }
        };
        flush(&mut pt, &mut vram);
        bind_range(&mut pt, VA, 0x3000, 0, Source::Vram { vram_off: 0 }, 0).unwrap();
        flush(&mut pt, &mut vram);
        bind_range(&mut pt, VA + 0x8000_0000, 0x1000, 0, Source::Vram { vram_off: 0x2000 }, 0).unwrap();
        flush(&mut pt, &mut vram);
        unbind_range(&mut pt, VA + 0x1000, 0x1000);
        flush(&mut pt, &mut vram);
        // rebuild a tree from the written images alone
        assert_eq!(vram.len(), pt.len());
        for (i, pa) in vram.keys().enumerate() {
            assert_eq!(*pa, (64 << 20) + (i * 4096) as u64);
        }
        let images: Vec<[u8; 4096]> = vram.values().copied().collect();
        let from_vram = PageTables::from_images(64 << 20, 64, &images);
        for va in [VA, VA + 0x1000, VA + 0x2000, VA + 0x8000_0000] {
            assert_eq!(from_vram.translate(va), pt.translate(va), "VA {:#x}", va);
        }
        assert_eq!(pt.translate(VA + 0x1000), None);
    }

    // ---- TLB flush ----

    #[test]
    fn the_tlb_flush_names_the_root_and_triggers_all() {
        let (regs, poll) = tlb_flush_regs(64 << 20);
        // nouveau writes `pd->pt[0]->addr >> 8` to 0xb830a0 (`vmmtu102.c:39`), 0 to 0xb830a4, then `0x80000000 | type` to 0xb830b0
        assert_eq!(regs[0], (0xb830a0, 0x40000));
        assert_eq!(regs[1], (0xb830a4, 0));
        assert_eq!(regs[2].0, 0xb830b0);
        assert_eq!(poll, 0xb830b0);
        // TRIGGER (31), ALL_PDB (1), ALL_VA (0); nothing else (no replay, no cancel, no HUB-only)
        assert_eq!(regs[2].1, 0x8000_0003);
    }

    #[test]
    fn the_tlb_flush_splits_a_wide_root_between_the_two_registers() {
        // PDB_ADDR is bits 31:4 holding address bits 39:12; UPPER_PDB_ADDR bits 19:0 hold address bits 59:40
        let (regs, _) = tlb_flush_regs(0x1_2345_6000);
        assert_eq!(regs[0].1, 0x0123_4560);
        assert_eq!(regs[1].1, 0);
        let (regs, _) = tlb_flush_regs(0x300_0abc_d000);
        assert_eq!(regs[0].1, 0x0abc_d0);
        assert_eq!(regs[1].1, 3);
    }

    #[test]
    fn the_user_heap_lies_outside_what_the_kernel_uses() {
        // the GR context buffers (`gr::VRAM_CTX`) and the channel blocks are above and below it
        assert!(USER_VRAM_END <= crate::gr::VRAM_CTX);
        assert!(USER_VRAM_BASE > crate::chan::FRAME_VRAM + crate::chan::FRAME_BYTES);
        assert!(USER_VRAM_BASE > 64 << 20);
        assert_eq!(USER_VRAM_BYTES, 0x1_e000_0000 - (1 << 30));
    }
}
