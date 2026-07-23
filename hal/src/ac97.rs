//! AC97 (Intel 82801AA ICH) register protocol + ring-buffer state machine —
//! pure logic, generic over the `PortIo` seam so it can be unit tested on
//! the host with `cargo test`, no QEMU required.
//!
//! This is the second driver migrated onto the `hal` seam pattern (after
//! ACPI, which only exercised `PhysMem`) — see `hal/src/acpi.rs` for the
//! worked reference and `.claude/skills/kernel-drivers/SKILL.md` for the
//! general playbook. Everything below reads/writes only through the
//! injected `PortIo`, allocates nothing, logs nothing, and touches no
//! global — the kernel adapter (`kernel/src/ac97.rs`) owns PCI discovery,
//! physical-memory allocation, the raw DMA buffer pointers, and the
//! `spin::Mutex` global.
//!
//! Two independent pieces of logic live here:
//! - The **register protocol** (`Ac97Regs`): cold reset, PCM-out stream
//!   reset, mixer unmute, BDL programming, CIV/LVI access. Bounded polling
//!   throughout (`TIMEOUT_POLLS`), matching every other "never hang boot on
//!   missing hardware" driver in this kernel (mouse, rtc, acpi).
//! - The **ring state machine** (`plan_fill`): given the software-side
//!   `next_fill` cursor and a freshly-read hardware `CIV`, decides whether
//!   there's a free slot to fill and, if so, which physical slot, which
//!   `LVI` to program, and the next `next_fill`. Pure arithmetic — no IO, no
//!   pointers — which is exactly why it's the highest-value thing to test:
//!   the mod-8/mod-32 aliasing here is subtle enough that a host test is
//!   much cheaper than a QEMU audio-corruption hunt.

use crate::PortIo;

// ── Register offsets/bits (unchanged from the original inline driver) ──────

// NAM (Native Audio Mixer) register offsets, relative to BAR0.
const NAM_MASTER_VOLUME: u16 = 0x02;
const NAM_PCM_OUT_VOLUME: u16 = 0x18;

// NABM (Native Audio Bus Master) register offsets, relative to BAR1.
// PCM OUT (PO) per-stream block.
const NABM_PO_BDBAR: u16 = 0x10; // u32: physical address of the Buffer Descriptor List
const NABM_PO_CIV: u16 = 0x14; // u8:  current index value (read-only)
const NABM_PO_LVI: u16 = 0x15; // u8:  last valid index
const NABM_PO_CR: u16 = 0x1B; // u8:  control register
const NABM_GLOB_CNT: u16 = 0x2C; // u32: global control
const NABM_GLOB_STA: u16 = 0x30; // u32: global status

const CR_RPBM: u8 = 1 << 0; // run/pause bus master
const CR_RR: u8 = 1 << 1; // reset registers (self-clears)

const GLOB_CNT_COLD_RESET: u32 = 1 << 1;
const GLOB_STA_CODEC_READY: u32 = 1 << 8;

/// Bounded polling, same "never hang boot" convention as every other
/// optional-hardware probe in this kernel (mouse, rtc, acpi).
pub const TIMEOUT_POLLS: u32 = 1_000_000;

/// Real ring capacity: only this many distinct physical buffers actually
/// exist. `BDL_ENTRIES` (the hardware-visible descriptor count) is a
/// multiple of it — see `build_bdl`'s doc comment for why.
pub const RING_SLOTS: usize = 8;
pub const SLOT_ORDER: usize = 13; // 8 KiB per slot — 2048 stereo s16 frames
pub const SLOT_BYTES: usize = 1 << SLOT_ORDER;
pub const SLOT_FRAMES: usize = SLOT_BYTES / 4; // 4 bytes per stereo s16 frame

/// AC97's CIV/LVI index registers are 5-bit hardware counters (0-31) that
/// wrap at 32 *in hardware* — not at whatever ring size software happens to
/// use. See `build_bdl` for how the aliasing across the real `RING_SLOTS`
/// buffers is constructed.
pub const BDL_ENTRIES: usize = 32;

/// One Buffer Descriptor List entry, exactly as the AC97 bus master reads
/// it from DMA memory. Layout only — writing this into physical memory is
/// the kernel adapter's job (real DMA memory, raw pointers), not this
/// module's.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BdlEntry {
    pub addr: u32,
    pub samples: u16, // count of 16-bit words (stereo frame = 2 samples)
    pub flags: u16,
}

/// Reasons the register protocol can fail — the kernel adapter logs which
/// one and gives up (best-effort, same as every other hardware probe here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ac97Error {
    /// `GLOB_STA.CODEC_READY` never came up after a cold reset.
    CodecNotReady,
    /// `PO_CR.RR` never self-cleared after being set.
    PcmResetTimeout,
}

/// Builds the full 32-entry Buffer Descriptor List, aliasing each of the
/// `RING_SLOTS` (8) real physical buffers across `BDL_ENTRIES / RING_SLOTS`
/// (4) descriptor entries (`entries[i].addr = slot_phys[i % RING_SLOTS]`).
///
/// Only populating entries `0..RING_SLOTS` and expecting the hardware's
/// 5-bit CIV/LVI counters to wrap back to 0 at 8 doesn't match their real
/// width (they wrap at 32) — so the full 32-entry table is programmed
/// instead, giving the hardware's natural mod-32 wraparound a real buffer
/// to land on at every index while preserving the same per-buffer reuse
/// spacing (`RING_SLOTS` plays away) a literal 8-entry table would have.
///
/// Pure — no allocation (fixed-size array), no IO, no pointers. `addr`/
/// `samples`/`flags` never change again after the caller writes this array
/// into DMA memory; only `LVI` bookkeeping does, via `Ac97Regs::set_lvi`.
pub fn build_bdl(slot_phys: [u64; RING_SLOTS]) -> [BdlEntry; BDL_ENTRIES] {
    let mut entries = [BdlEntry { addr: 0, samples: 0, flags: 0 }; BDL_ENTRIES];
    for (i, entry) in entries.iter_mut().enumerate() {
        entry.addr = slot_phys[i % RING_SLOTS] as u32;
        entry.samples = (SLOT_FRAMES * 2) as u16; // 2 samples (L+R) per frame
        entry.flags = 0;
    }
    entries
}

/// The AC97 register protocol, generic over the `PortIo` seam. Owns only
/// the two I/O-port base addresses (BAR0/BAR1) — no hardware state, no
/// DMA memory. `Copy`/`Clone` when `IO` is, so the kernel adapter can
/// snapshot a cheap copy out from behind its `Mutex` before a blocking
/// poll (mirroring the original driver's "poll CIV outside the lock"
/// discipline) — see the module doc.
#[derive(Clone, Copy)]
pub struct Ac97Regs<IO: PortIo> {
    io: IO,
    nam_base: u16,
    nabm_base: u16,
}

impl<IO: PortIo> Ac97Regs<IO> {
    pub fn new(io: IO, nam_base: u16, nabm_base: u16) -> Self {
        Ac97Regs { io, nam_base, nabm_base }
    }

    /// Triggers a cold reset and polls (bounded, `TIMEOUT_POLLS`) for
    /// `GLOB_STA.CODEC_READY`.
    pub fn cold_reset(&self) -> Result<(), Ac97Error> {
        self.io.outl(self.nabm_base + NABM_GLOB_CNT, GLOB_CNT_COLD_RESET);
        for _ in 0..TIMEOUT_POLLS {
            if self.io.inl(self.nabm_base + NABM_GLOB_STA) & GLOB_STA_CODEC_READY != 0 {
                return Ok(());
            }
        }
        Err(Ac97Error::CodecNotReady)
    }

    /// Resets the PCM-out stream's registers and polls (bounded) for
    /// `PO_CR.RR` to self-clear.
    pub fn reset_pcm_stream(&self) -> Result<(), Ac97Error> {
        self.io.outb(self.nabm_base + NABM_PO_CR, CR_RR);
        for _ in 0..TIMEOUT_POLLS {
            if self.io.inb(self.nabm_base + NABM_PO_CR) & CR_RR == 0 {
                return Ok(());
            }
        }
        Err(Ac97Error::PcmResetTimeout)
    }

    /// Unmutes master + PCM-out volume (0x0000 = 0dB attenuation on both
    /// channels, i.e. max volume, mute bit clear).
    pub fn unmute(&self) {
        self.io.outw(self.nam_base + NAM_MASTER_VOLUME, 0x0000);
        self.io.outw(self.nam_base + NAM_PCM_OUT_VOLUME, 0x0000);
    }

    /// Programs the BDL's physical base address and the initial LVI.
    pub fn program_bdl(&self, bdl_phys: u32, lvi: u8) {
        self.io.outl(self.nabm_base + NABM_PO_BDBAR, bdl_phys);
        self.io.outb(self.nabm_base + NABM_PO_LVI, lvi);
    }

    /// Sets `PO_CR.RPBM` (run/pause bus master) to start playback.
    pub fn start(&self) {
        self.io.outb(self.nabm_base + NABM_PO_CR, CR_RPBM);
    }

    /// Reads the current index value, already masked to the real 5-bit
    /// counter width.
    pub fn read_civ(&self) -> u8 {
        self.io.inb(self.nabm_base + NABM_PO_CIV) & 0x1F
    }

    /// Extends the valid descriptor range to include `idx` (i.e. "I just
    /// filled entry `idx`, it's playable now").
    pub fn set_lvi(&self, idx: u8) {
        self.io.outb(self.nabm_base + NABM_PO_LVI, idx);
    }
}

// ── Ring state machine (pure — the crown jewel) ─────────────────────────────

/// What to do with the next `write_pcm` call, decided purely from the
/// software cursor and a freshly-read hardware CIV.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillPlan {
    /// Physical buffer index (`0..RING_SLOTS`) to copy PCM into.
    pub slot: usize,
    /// BDL index to program into `PO_LVI` once the copy is done.
    pub lvi: u8,
    /// The `next_fill` cursor value to store for the *next* call.
    pub next_fill: usize,
}

/// Decides whether BDL entry `next_fill` is safe to (re)fill given the
/// hardware's current `civ`, and if so, what to do about it.
///
/// Returns `None` when `next_fill == civ` — that descriptor is the one
/// currently playing, so the caller must wait (not overwrite it) rather
/// than fill it. Otherwise returns a `FillPlan` naming the physical slot
/// (`next_fill % ring_slots`), the `LVI` to program (`next_fill` itself —
/// extending the valid range to include the entry just filled), and the
/// advanced `next_fill` (mod `bdl_entries`) for the next call.
///
/// Pure arithmetic, no IO — this is the exact ring semantics `write_pcm`
/// must reproduce byte-for-byte; see the module doc.
pub fn plan_fill(next_fill: usize, civ: u8, ring_slots: usize, bdl_entries: usize) -> Option<FillPlan> {
    if next_fill == civ as usize {
        return None;
    }
    Some(FillPlan {
        slot: next_fill % ring_slots,
        lvi: next_fill as u8,
        next_fill: (next_fill + 1) % bdl_entries,
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ScriptedIo;

    // ── Ring state machine ──────────────────────────────────────────────

    #[test]
    fn plan_fill_none_when_next_fill_equals_civ() {
        assert_eq!(plan_fill(5, 5, RING_SLOTS, BDL_ENTRIES), None);
        assert_eq!(plan_fill(0, 0, RING_SLOTS, BDL_ENTRIES), None);
        assert_eq!(plan_fill(31, 31, RING_SLOTS, BDL_ENTRIES), None);
    }

    #[test]
    fn plan_fill_computes_slot_and_lvi_and_advances() {
        let plan = plan_fill(3, 7, RING_SLOTS, BDL_ENTRIES).expect("should fill");
        assert_eq!(plan.slot, 3 % RING_SLOTS);
        assert_eq!(plan.lvi, 3);
        assert_eq!(plan.next_fill, 4);
    }

    #[test]
    fn plan_fill_wraps_next_fill_mod_bdl_entries() {
        let plan = plan_fill(31, 0, RING_SLOTS, BDL_ENTRIES).expect("should fill");
        assert_eq!(plan.slot, 31 % RING_SLOTS);
        assert_eq!(plan.lvi, 31);
        assert_eq!(plan.next_fill, 0); // wraps 31 -> 0
    }

    #[test]
    fn plan_fill_full_cycle_maps_to_physical_buffers_in_order_four_times_each() {
        // A full walk of next_fill = 0..32 (never colliding with civ) must
        // cycle the 8 physical slots in order, exactly 4 times each —
        // BDL_ENTRIES / RING_SLOTS = 4, matching the aliasing build_bdl
        // sets up in hardware.
        let mut next_fill = 0usize;
        let mut slots_seen = alloc::vec::Vec::new();
        for _ in 0..BDL_ENTRIES {
            // civ deliberately never equals next_fill here (fixed far away)
            // so every step actually produces a plan.
            let civ = ((next_fill + 16) % BDL_ENTRIES) as u8;
            let plan = plan_fill(next_fill, civ, RING_SLOTS, BDL_ENTRIES).expect("should fill");
            slots_seen.push(plan.slot);
            next_fill = plan.next_fill;
        }
        assert_eq!(slots_seen.len(), BDL_ENTRIES);
        for (i, slot) in slots_seen.iter().enumerate() {
            assert_eq!(*slot, i % RING_SLOTS);
        }
        // Each physical slot appears exactly BDL_ENTRIES / RING_SLOTS times.
        for slot in 0..RING_SLOTS {
            let count = slots_seen.iter().filter(|&&s| s == slot).count();
            assert_eq!(count, BDL_ENTRIES / RING_SLOTS);
        }
        // Cursor should have wrapped all the way back to 0.
        assert_eq!(next_fill, 0);
    }

    #[test]
    fn build_bdl_aliases_each_slot_across_four_entries() {
        let slot_phys: [u64; RING_SLOTS] = [10, 20, 30, 40, 50, 60, 70, 80];
        let entries = build_bdl(slot_phys);
        assert_eq!(entries.len(), BDL_ENTRIES);
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(entry.addr, slot_phys[i % RING_SLOTS] as u32);
            assert_eq!(entry.samples, (SLOT_FRAMES * 2) as u16);
            assert_eq!(entry.flags, 0);
        }
    }

    // ── Register protocol ───────────────────────────────────────────────

    const NAM_BASE: u16 = 0x1000;
    const NABM_BASE: u16 = 0x1400;

    #[test]
    fn cold_reset_waits_for_codec_ready_then_succeeds() {
        let io = ScriptedIo::new();
        // Not ready for the first 3 reads of GLOB_STA, ready on the 4th.
        io.queue_reads(NABM_BASE + NABM_GLOB_STA, &[0, 0, 0, GLOB_STA_CODEC_READY]);

        let regs = Ac97Regs::new(&io, NAM_BASE, NABM_BASE);
        assert_eq!(regs.cold_reset(), Ok(()));

        // Exactly one write: GLOB_CNT = COLD_RESET, before any status read.
        let writes = io.writes();
        assert_eq!(writes, alloc::vec![(NABM_BASE + NABM_GLOB_CNT, GLOB_CNT_COLD_RESET)]);
    }

    #[test]
    fn cold_reset_never_ready_fails_within_timeout() {
        let io = ScriptedIo::new();
        // Never queue a ready value — reads sit on the sticky default (0)
        // forever. Must return Err, not hang.
        io.queue_read(NABM_BASE + NABM_GLOB_STA, 0);

        let regs = Ac97Regs::new(&io, NAM_BASE, NABM_BASE);
        assert_eq!(regs.cold_reset(), Err(Ac97Error::CodecNotReady));
    }

    #[test]
    fn reset_pcm_stream_waits_for_self_clear_then_succeeds() {
        let io = ScriptedIo::new();
        io.queue_reads(NABM_BASE + NABM_PO_CR, &[CR_RR as u32, CR_RR as u32, 0]);

        let regs = Ac97Regs::new(&io, NAM_BASE, NABM_BASE);
        assert_eq!(regs.reset_pcm_stream(), Ok(()));

        let writes = io.writes();
        assert_eq!(writes, alloc::vec![(NABM_BASE + NABM_PO_CR, CR_RR as u32)]);
    }

    #[test]
    fn reset_pcm_stream_never_clears_fails_within_timeout() {
        let io = ScriptedIo::new();
        io.queue_read(NABM_BASE + NABM_PO_CR, CR_RR as u32);

        let regs = Ac97Regs::new(&io, NAM_BASE, NABM_BASE);
        assert_eq!(regs.reset_pcm_stream(), Err(Ac97Error::PcmResetTimeout));
    }

    #[test]
    fn unmute_writes_both_volumes_zero() {
        let io = ScriptedIo::new();
        let regs = Ac97Regs::new(&io, NAM_BASE, NABM_BASE);
        regs.unmute();
        assert_eq!(
            io.writes(),
            alloc::vec![(NAM_BASE + NAM_MASTER_VOLUME, 0), (NAM_BASE + NAM_PCM_OUT_VOLUME, 0)]
        );
    }

    #[test]
    fn program_bdl_and_start_write_expected_offsets_in_order() {
        let io = ScriptedIo::new();
        let regs = Ac97Regs::new(&io, NAM_BASE, NABM_BASE);
        regs.program_bdl(0xDEAD_BEEF, 31);
        regs.start();
        assert_eq!(
            io.writes(),
            alloc::vec![
                (NABM_BASE + NABM_PO_BDBAR, 0xDEAD_BEEFu32),
                (NABM_BASE + NABM_PO_LVI, 31),
                (NABM_BASE + NABM_PO_CR, CR_RPBM as u32),
            ]
        );
    }

    #[test]
    fn read_civ_masks_to_five_bits() {
        let io = ScriptedIo::new();
        io.queue_read(NABM_BASE + NABM_PO_CIV, 0xFF);
        let regs = Ac97Regs::new(&io, NAM_BASE, NABM_BASE);
        assert_eq!(regs.read_civ(), 0x1F);
    }

    #[test]
    fn set_lvi_writes_expected_offset() {
        let io = ScriptedIo::new();
        let regs = Ac97Regs::new(&io, NAM_BASE, NABM_BASE);
        regs.set_lvi(17);
        assert_eq!(io.writes(), alloc::vec![(NABM_BASE + NABM_PO_LVI, 17)]);
    }
}
