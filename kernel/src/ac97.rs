// kernel/src/ac97.rs
//
// AC97 (Intel 82801AA ICH) PCI audio codec driver — hardware protocol +
// bus-master DMA, parallel to mouse.rs's role for the PS/2 auxiliary
// device (protocol/hardware here, FileHandle wrapper in
// drivers/dev_dsp.rs). Backs /dev/dsp, in turn used by DOOM's sound
// effects (doom-port/doomgeneric_sound_constanos.c).
//
// Fixed format, no negotiation: without the VRA (Variable Rate Audio)
// extension, AC97 always runs at 48000 Hz stereo 16-bit signed PCM — this
// driver never touches VRA, so that's simply the only format /dev/dsp
// accepts. One client (DOOM) writing one hardware format needs no ioctl
// negotiation, same simplification already used for /dev/input/event0's
// and event1's fixed record layout.
//
// Polling, not interrupt-driven. The IDT is a spin::Once, populated once
// at the very start of boot() before memory::init_core runs — registering
// a handler for a PCI IRQ line that's only known after enumeration would
// need either an early (pre-memory-init) PCI scan just for that, or a
// bigger refactor of IDT to something re-mutable. Not worth it for a first
// cut: the AC97 bus master keeps consuming queued buffer-descriptor
// entries on its own once started, so software only needs to periodically
// refill consumed slots before the ring runs dry — DOOM's sound module
// Update() callback already fires ~35x/sec, comfortably enough to keep an
// 8-slot, ~42ms-per-slot ring fed. See the module doc on IRQ vs polling in
// the doom-port-status memory / plan for the full reasoning.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use spin::Mutex;
use x86_64::instructions::port::{Port, PortWriteOnly};

const VENDOR_INTEL: u16 = 0x8086;
const DEVICE_AC97: u16 = 0x2415; // Intel 82801AA AC'97 Audio — what QEMU's `-device AC97` emulates

// ── NAM (Native Audio Mixer) register offsets, relative to BAR0 ────────────
const NAM_MASTER_VOLUME: u16 = 0x02;
const NAM_PCM_OUT_VOLUME: u16 = 0x18;

// ── NABM (Native Audio Bus Master) register offsets, relative to BAR1 ──────
// PCM OUT (PO) per-stream block.
const NABM_PO_BDBAR: u16 = 0x10; // u32: physical address of the Buffer Descriptor List
const NABM_PO_CIV: u16 = 0x14;   // u8:  current index value (read-only)
const NABM_PO_LVI: u16 = 0x15;   // u8:  last valid index
const NABM_PO_SR: u16 = 0x16;    // u16: status register
const NABM_PO_CR: u16 = 0x1B;    // u8:  control register
const NABM_GLOB_CNT: u16 = 0x2C; // u32: global control
const NABM_GLOB_STA: u16 = 0x30; // u32: global status

const CR_RPBM: u8 = 1 << 0; // run/pause bus master
const CR_RR: u8 = 1 << 1;   // reset registers (self-clears)

const GLOB_CNT_COLD_RESET: u32 = 1 << 1;
const GLOB_STA_CODEC_READY: u32 = 1 << 8;

// Bounded polling, same "never hang boot" convention as mouse.rs's
// TIMEOUT_POLLS — a machine with no AC97 codec (or a QEMU build that
// doesn't ACK in the expected way) must not wedge boot.
const TIMEOUT_POLLS: u32 = 1_000_000;

// Real ring capacity: only this many distinct physical buffers actually
// exist. `BDL_ENTRIES` (the hardware-visible descriptor count) is a
// multiple of it — see the module doc below on why they differ.
const RING_SLOTS: usize = 8;
const SLOT_ORDER: usize = 13; // 8 KiB per slot — 2048 stereo s16 frames
const SLOT_BYTES: usize = 1 << SLOT_ORDER;
const SLOT_FRAMES: usize = SLOT_BYTES / 4; // 4 bytes per stereo s16 frame

// AC97's CIV/LVI index registers are 5-bit hardware counters (0-31) that
// wrap at 32 *in hardware* — not at whatever ring size software happens to
// use. Only populating entries 0..RING_SLOTS and expecting the index
// registers to wrap back to 0 at 8 doesn't match the real counter width,
// so the full 32-entry descriptor table is programmed, with each of the
// RING_SLOTS physical buffers aliased across 4 descriptor entries
// (`entry[i].addr = slot_phys[i % RING_SLOTS]`). This makes the hardware's
// natural mod-32 wraparound cycle through the real buffers in order with
// no special-casing, while giving software the same refill timing margin
// a literal 8-entry table would (each physical buffer's next reuse is
// still exactly `RING_SLOTS` buffer-plays away either way).
const BDL_ENTRIES: usize = 32;

#[repr(C)]
#[derive(Clone, Copy)]
struct BdlEntry {
    addr: u32,
    samples: u16, // count of 16-bit words (stereo frame = 2 samples)
    flags: u16,
}

struct Ac97 {
    nam_base: u16,
    nabm_base: u16,
    slot_virt: [*mut u8; RING_SLOTS],
    /// Next BDL index (0..BDL_ENTRIES) software will fill on the next
    /// `write_pcm` call — the physical buffer it maps to is `idx % RING_SLOTS`.
    next_fill: AtomicUsize,
}

// SAFETY: only ever touched through AC97's Mutex or the atomics inside;
// the raw pointers are fixed physically-backed kernel buffers that live
// for the kernel's lifetime (never freed).
unsafe impl Send for Ac97 {}

static AC97: Mutex<Option<Ac97>> = Mutex::new(None);
static READY: AtomicU32 = AtomicU32::new(0);

fn port_out8(base: u16, offset: u16, value: u8) {
    unsafe { PortWriteOnly::<u8>::new(base + offset).write(value); }
}
fn port_out16(base: u16, offset: u16, value: u16) {
    unsafe { PortWriteOnly::<u16>::new(base + offset).write(value); }
}
fn port_out32(base: u16, offset: u16, value: u32) {
    unsafe { PortWriteOnly::<u32>::new(base + offset).write(value); }
}
fn port_in8(base: u16, offset: u16) -> u8 {
    unsafe { Port::<u8>::new(base + offset).read() }
}
fn port_in32(base: u16, offset: u16) -> u32 {
    unsafe { Port::<u32>::new(base + offset).read() }
}

/// Best-effort init: finds the AC97 PCI function, resets and unmutes the
/// codec, allocates the BDL + ring buffers, and starts the bus master
/// running (on silence, until real PCM arrives via `write_pcm`). Logs and
/// returns on any failure — no AC97 device (e.g. a real-hardware boot with
/// no sound card) just means /dev/dsp silently discards writes.
pub fn init() {
    let Some(dev) = crate::pci::find_device(VENDOR_INTEL, DEVICE_AC97) else {
        crate::serial_println!("ac97: no AC97 PCI device found — /dev/dsp will discard writes");
        return;
    };
    crate::pci::enable_bus_master_and_io(&dev);

    let nam_base = dev.bar0 as u16;
    let nabm_base = dev.bar1 as u16;
    crate::serial_println!(
        "ac97: found at {:02x}:{:02x}.{} (NAM={:#x} NABM={:#x} irq={})",
        dev.bus, dev.device, dev.function, nam_base, nabm_base, dev.interrupt_line
    );

    // Cold reset, then wait for the codec-ready bit.
    port_out32(nabm_base, NABM_GLOB_CNT, GLOB_CNT_COLD_RESET);
    let mut ready = false;
    for _ in 0..TIMEOUT_POLLS {
        if port_in32(nabm_base, NABM_GLOB_STA) & GLOB_STA_CODEC_READY != 0 {
            ready = true;
            break;
        }
    }
    if !ready {
        crate::serial_println!("ac97: codec never became ready — giving up");
        return;
    }

    // Reset the PCM-out stream's registers, wait for RR to self-clear.
    port_out8(nabm_base, NABM_PO_CR, CR_RR);
    let mut reset_done = false;
    for _ in 0..TIMEOUT_POLLS {
        if port_in8(nabm_base, NABM_PO_CR) & CR_RR == 0 {
            reset_done = true;
            break;
        }
    }
    if !reset_done {
        crate::serial_println!("ac97: PCM-out register reset never completed — giving up");
        return;
    }

    // Unmute master + PCM-out volume (0x0000 = 0dB attenuation on both
    // channels, i.e. max volume, mute bit clear).
    port_out16(nam_base, NAM_MASTER_VOLUME, 0x0000);
    port_out16(nam_base, NAM_PCM_OUT_VOLUME, 0x0000);

    // BDL: BDL_ENTRIES (32) descriptors, 8 bytes each — 256 bytes, one
    // 4KiB frame is generous but simplest (matches the alignment the
    // hardware wants for BDBAR anyway).
    let Some(bdl_phys) = (unsafe { crate::allocator::phys_alloc(12) }) else {
        crate::serial_println!("ac97: BDL allocation failed — giving up");
        return;
    };
    let bdl_virt = (crate::memory::physical_memory_offset() + bdl_phys.as_u64()).as_mut_ptr::<BdlEntry>();

    let mut slot_virt = [core::ptr::null_mut::<u8>(); RING_SLOTS];
    let mut slot_phys = [0u64; RING_SLOTS];
    for i in 0..RING_SLOTS {
        let Some(phys) = (unsafe { crate::allocator::phys_alloc(SLOT_ORDER) }) else {
            crate::serial_println!("ac97: ring buffer allocation failed — giving up");
            return;
        };
        let virt = (crate::memory::physical_memory_offset() + phys.as_u64()).as_mut_ptr::<u8>();
        unsafe { core::ptr::write_bytes(virt, 0, SLOT_BYTES); } // start on silence
        slot_virt[i] = virt;
        slot_phys[i] = phys.as_u64();
    }

    // Every physical buffer is aliased across BDL_ENTRIES/RING_SLOTS (4)
    // descriptor entries — see the BDL_ENTRIES doc comment above for why.
    // addr/length never change again after this; only LVI bookkeeping does.
    for i in 0..BDL_ENTRIES {
        unsafe {
            let entry = bdl_virt.add(i);
            (*entry).addr = slot_phys[i % RING_SLOTS] as u32;
            (*entry).samples = (SLOT_FRAMES * 2) as u16; // 2 samples (L+R) per frame
            (*entry).flags = 0;
        }
    }

    port_out32(nabm_base, NABM_PO_BDBAR, bdl_phys.as_u64() as u32);
    port_out8(nabm_base, NABM_PO_LVI, (BDL_ENTRIES - 1) as u8); // all 32 valid (silence) to start
    port_out8(nabm_base, NABM_PO_CR, CR_RPBM);

    *AC97.lock() = Some(Ac97 { nam_base, nabm_base, slot_virt, next_fill: AtomicUsize::new(0) });
    READY.store(1, Ordering::Release);
    crate::serial_println!(
        "ac97: PCM-out running (48000 Hz stereo s16le, {} physical buffers x {}B, {} BDL entries)",
        RING_SLOTS, SLOT_BYTES, BDL_ENTRIES
    );
}

/// Writes raw 48000 Hz stereo s16le PCM, blocking (spinning, no lock held
/// across the spin — the timer ISR/scheduler still preempts normally)
/// until the next BDL slot is free. Copies at most one slot's worth per
/// call; returns the number of bytes actually consumed (0 if the device
/// never initialized).
pub fn write_pcm(bytes: &[u8]) -> usize {
    if READY.load(Ordering::Acquire) == 0 {
        return 0;
    }

    let n = bytes.len().min(SLOT_BYTES);
    if n == 0 {
        return 0;
    }

    loop {
        // Snapshot what we need under the lock, do the hardware poll
        // outside it so a slow spin never blocks other /dev/dsp state
        // changes (there's only one writer today, but no reason to hold
        // the lock longer than necessary).
        let (nabm_base, slot_ptr, fill_idx) = {
            let guard = AC97.lock();
            let Some(state) = guard.as_ref() else { return 0; };
            let idx = state.next_fill.load(Ordering::Relaxed);
            (state.nabm_base, state.slot_virt[idx % RING_SLOTS], idx)
        };

        let civ = (port_in8(nabm_base, NABM_PO_CIV) & 0x1F) as usize;
        // The BDL index we're about to (re)fill must not be the one
        // currently playing (CIV) — everything else is fair game.
        if fill_idx != civ {
            unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), slot_ptr, n); }
            if n < SLOT_BYTES {
                unsafe { core::ptr::write_bytes(slot_ptr.add(n), 0, SLOT_BYTES - n); }
            }

            let next_idx = (fill_idx + 1) % BDL_ENTRIES;
            let guard = AC97.lock();
            if let Some(state) = guard.as_ref() {
                state.next_fill.store(next_idx, Ordering::Relaxed);
            }
            drop(guard);

            // Extend the valid range to include the entry we just filled.
            port_out8(nabm_base, NABM_PO_LVI, fill_idx as u8);
            return n;
        }
        core::hint::spin_loop();
    }
}
