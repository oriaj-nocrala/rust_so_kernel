// kernel/src/ac97.rs
//
// AC97 (Intel 82801AA ICH) PCI audio codec driver — thin kernel-side
// adapter around `hal::ac97`'s pure register protocol + ring state
// machine. This module owns everything that's genuinely hardware access:
// PCI discovery (`crate::pci`), physical-memory allocation
// (`crate::allocator::phys_alloc`), the raw DMA buffer pointers, and the
// `spin::Mutex` global. All the register-offset/bit-level protocol and the
// ring-buffer refill arithmetic now live in `hal::ac97`, where they're unit
// tested on the host with `cargo test` (see `hal/src/ac97.rs`).
//
// Parallel to mouse.rs's role for the PS/2 auxiliary device (protocol/
// hardware here, FileHandle wrapper in drivers/dev_dsp.rs). Backs
// /dev/dsp, in turn used by DOOM/Quake's sound modules.
//
// Fixed format, no negotiation: without the VRA (Variable Rate Audio)
// extension, AC97 always runs at 48000 Hz stereo 16-bit signed PCM — this
// driver never touches VRA, so that's simply the only format /dev/dsp
// accepts. One client writing one hardware format needs no ioctl
// negotiation, same simplification already used for /dev/input/event0's
// and event1's fixed record layout.
//
// Polling, not interrupt-driven — see the original module doc (preserved
// below in spirit): the IDT is a spin::Once, populated once at the very
// start of boot() before memory::init_core runs, so wiring up a PCI IRQ
// whose vector is only known after enumeration doesn't fit without either
// an early pre-memory PCI scan or a bigger IDT refactor. `write_pcm`
// instead polls the hardware's CIV register directly and blocks (spinning,
// no lock held across the spin) until a buffer-descriptor slot frees.

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use spin::Mutex;

pub use hal::ac97::{Ac97Regs, BDL_ENTRIES, BdlEntry, RING_SLOTS, SLOT_BYTES, SLOT_ORDER};

use crate::hal::{Driver, DriverError, X86PortIo};

const VENDOR_INTEL: u16 = 0x8086;
const DEVICE_AC97: u16 = 0x2415; // Intel 82801AA AC'97 Audio — what QEMU's `-device AC97` emulates

struct Ac97 {
    regs: Ac97Regs<X86PortIo>,
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

/// `crate::hal::Driver` adapter around the AC97 register protocol + DMA
/// setup — same shape as `acpi::AcpiDriver`, the pilot for this pattern.
/// Best-effort: finds the AC97 PCI function, resets and unmutes the codec,
/// allocates the BDL + ring buffers, and starts the bus master running (on
/// silence, until real PCM arrives via `write_pcm`). Returns `Err` and logs
/// on any failure — no AC97 device (e.g. a real-hardware boot with no
/// sound card) just means /dev/dsp silently discards writes.
pub struct Ac97Driver;

impl Ac97Driver {
    pub fn new() -> Self {
        Ac97Driver
    }
}

impl Default for Ac97Driver {
    fn default() -> Self {
        Self::new()
    }
}

impl Driver for Ac97Driver {
    fn name(&self) -> &str {
        "ac97"
    }

    fn init(&mut self) -> Result<(), DriverError> {
        let Some(dev) = crate::pci::find_device(VENDOR_INTEL, DEVICE_AC97) else {
            crate::serial_println!("ac97: no AC97 PCI device found — /dev/dsp will discard writes");
            return Err(DriverError::NotFound);
        };
        crate::pci::enable_bus_master_and_io(&dev);

        let nam_base = dev.bar0 as u16;
        let nabm_base = dev.bar1 as u16;
        crate::serial_println!(
            "ac97: found at {:02x}:{:02x}.{} (NAM={:#x} NABM={:#x} irq={})",
            dev.bus, dev.device, dev.function, nam_base, nabm_base, dev.interrupt_line
        );

        let regs = Ac97Regs::new(X86PortIo, nam_base, nabm_base);

        // Cold reset, then wait for the codec-ready bit.
        if regs.cold_reset().is_err() {
            crate::serial_println!("ac97: codec never became ready — giving up");
            return Err(DriverError::NotFound);
        }

        // Reset the PCM-out stream's registers, wait for RR to self-clear.
        if regs.reset_pcm_stream().is_err() {
            crate::serial_println!("ac97: PCM-out register reset never completed — giving up");
            return Err(DriverError::NotFound);
        }

        regs.unmute();

        // BDL: BDL_ENTRIES (32) descriptors, 8 bytes each — 256 bytes, one
        // 4KiB frame is generous but simplest (matches the alignment the
        // hardware wants for BDBAR anyway).
        let Some(bdl_phys) = (unsafe { crate::allocator::phys_alloc(12) }) else {
            crate::serial_println!("ac97: BDL allocation failed — giving up");
            return Err(DriverError::NotFound);
        };
        let bdl_virt = (crate::memory::physical_memory_offset() + bdl_phys.as_u64()).as_mut_ptr::<BdlEntry>();

        let mut slot_virt = [core::ptr::null_mut::<u8>(); RING_SLOTS];
        let mut slot_phys = [0u64; RING_SLOTS];
        for i in 0..RING_SLOTS {
            let Some(phys) = (unsafe { crate::allocator::phys_alloc(SLOT_ORDER) }) else {
                crate::serial_println!("ac97: ring buffer allocation failed — giving up");
                return Err(DriverError::NotFound);
            };
            let virt = (crate::memory::physical_memory_offset() + phys.as_u64()).as_mut_ptr::<u8>();
            unsafe {
                core::ptr::write_bytes(virt, 0, SLOT_BYTES);
            } // start on silence
            slot_virt[i] = virt;
            slot_phys[i] = phys.as_u64();
        }

        // Every physical buffer is aliased across BDL_ENTRIES/RING_SLOTS (4)
        // descriptor entries — see `hal::ac97::build_bdl`'s doc comment.
        // addr/length never change again after this; only LVI bookkeeping does.
        let entries = hal::ac97::build_bdl(slot_phys);
        for (i, entry) in entries.iter().enumerate() {
            unsafe {
                bdl_virt.add(i).write(*entry);
            }
        }

        regs.program_bdl(bdl_phys.as_u64() as u32, (BDL_ENTRIES - 1) as u8); // all 32 valid (silence) to start
        regs.start();

        *AC97.lock() = Some(Ac97 { regs, slot_virt, next_fill: AtomicUsize::new(0) });
        READY.store(1, Ordering::Release);
        crate::serial_println!(
            "ac97: PCM-out running (48000 Hz stereo s16le, {} physical buffers x {}B, {} BDL entries)",
            RING_SLOTS,
            SLOT_BYTES,
            BDL_ENTRIES
        );
        Ok(())
    }
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
        // the lock longer than necessary). `regs` is a cheap Copy (a ZST
        // `X86PortIo` + two u16 bases), so snapshotting it out is free.
        let (regs, slot_ptr, fill_idx) = {
            let guard = AC97.lock();
            let Some(state) = guard.as_ref() else { return 0 };
            let idx = state.next_fill.load(Ordering::Relaxed);
            (state.regs, state.slot_virt[idx % RING_SLOTS], idx)
        };

        let civ = regs.read_civ();
        // The BDL index we're about to (re)fill must not be the one
        // currently playing (CIV) — everything else is fair game.
        match hal::ac97::plan_fill(fill_idx, civ, RING_SLOTS, BDL_ENTRIES) {
            Some(plan) => {
                unsafe {
                    core::ptr::copy_nonoverlapping(bytes.as_ptr(), slot_ptr, n);
                }
                if n < SLOT_BYTES {
                    unsafe {
                        core::ptr::write_bytes(slot_ptr.add(n), 0, SLOT_BYTES - n);
                    }
                }

                let guard = AC97.lock();
                if let Some(state) = guard.as_ref() {
                    state.next_fill.store(plan.next_fill, Ordering::Relaxed);
                }
                drop(guard);

                // Extend the valid range to include the entry we just filled.
                regs.set_lvi(plan.lvi);
                return n;
            }
            None => core::hint::spin_loop(),
        }
    }
}
