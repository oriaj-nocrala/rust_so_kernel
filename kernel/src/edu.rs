// kernel/src/edu.rs
//
// Driver for QEMU's `edu` teaching device, test builds only: it is how
// phase 1 of docs/gpu/gpu-plan.md proves MSI, DMA and BAR handling in QEMU
// before any of it touches the GPU (`hw_tests::edu_mmio_dma_msi`).
//
// Register map: QEMU v11.1.1 `docs/specs/edu.rst:44-95` and `hw/misc/edu.c`
// (a copy is in `~/src/gpu-ref/qemu-v11.1.1/`). Addresses below 0x80 take
// only 4-byte accesses; from 0x80 on, 4 or 8 (`edu.rst:41-42`), and the DMA
// registers are 64-bit, so they are written with 8-byte accesses.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::memory::dma::{DmaBuf, DmaError};

/// `edu.rst:32`.
pub const VENDOR: u16 = 0x1234;
pub const DEVICE: u16 = 0x11e8;

/// `edu.rst:44-95`.
const IDENT: u32 = 0x00;
const LIVENESS: u32 = 0x04;
const FACTORIAL: u32 = 0x08;
const STATUS: u32 = 0x20;
const IRQ_STATUS: u32 = 0x24;
const IRQ_RAISE: u32 = 0x60;
const IRQ_ACK: u32 = 0x64;
const DMA_SRC: u32 = 0x80;
const DMA_DST: u32 = 0x88;
const DMA_COUNT: u32 = 0x90;
const DMA_CMD: u32 = 0x98;
/// Status bit 0: computing factorial (`edu.rst:60-61`).
const STATUS_COMPUTING: u32 = 0x01;
/// DMA command bits (`edu.rst:90-95`, `edu.c:64-68`).
const DMA_RUN: u64 = 0x1;
const DMA_TO_RAM: u64 = 0x2;
const DMA_IRQ: u64 = 0x4;
/// The value a finished DMA raises (`edu.c:42`).
pub const DMA_DONE_IRQ: u32 = 0x100;
/// The device's DMA buffer: 4096 bytes at 0x40000 (`edu.rst:113`, `edu.c:44-45`).
pub const DMA_BUF_ADDR: u64 = 0x40000;
pub const DMA_BUF_SIZE: usize = 4096;

/// Bus-address mask the device accepts. Its default is 28 bits
/// (`edu.rst:22-26`), which the buddy allocator cannot target (it has no
/// "below N" request); the test runner starts the device with
/// `dma_mask=0xffffffffffff`, and this is that value.
pub const DMA_MASK: u64 = 0xffff_ffff_ffff;

/// BAR0's virtual address, for the ISR (0 = no device).
static BAR0: AtomicU64 = AtomicU64::new(0);
/// IRQ status values the ISR has seen and acknowledged, OR-ed together.
pub static IRQ_SEEN: AtomicU64 = AtomicU64::new(0);
/// CPU the last interrupt ran on (`usize::MAX` = none yet).
pub static IRQ_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);

pub struct Edu {
    pub bdf: (u8, u8, u8),
    base: u64,
}

fn rd32(base: u64, off: u32) -> u32 {
    // SAFETY: `base` is the UC mapping of BAR0 (1 MiB), `off` < 0x80.
    unsafe { core::ptr::read_volatile((base + off as u64) as *const u32) }
}

fn wr32(base: u64, off: u32, v: u32) {
    // SAFETY: as `rd32`.
    unsafe { core::ptr::write_volatile((base + off as u64) as *mut u32, v) }
}

fn rd64(base: u64, off: u32) -> u64 {
    // SAFETY: as `rd32`; 8-byte access is allowed at and above 0x80.
    unsafe { core::ptr::read_volatile((base + off as u64) as *const u64) }
}

fn wr64(base: u64, off: u32, v: u64) {
    // SAFETY: as `rd64`.
    unsafe { core::ptr::write_volatile((base + off as u64) as *mut u64, v) }
}

/// The MSI handler (ISR context, global work: it only acknowledges the
/// device and records what it saw).
fn on_msi(_vector: u8) {
    let base = BAR0.load(Ordering::Acquire);
    if base == 0 {
        return;
    }
    let status = rd32(base, IRQ_STATUS);
    // `edu.rst:104-107`: even with MSI, the ISR must acknowledge.
    wr32(base, IRQ_ACK, status);
    // CPU first: a waiter that sees the status must also see where it ran.
    IRQ_CPU.store(crate::cpu::percpu::cpu_id(), Ordering::Release);
    IRQ_SEEN.fetch_or(status as u64, Ordering::AcqRel);
}

/// Spins up to `ms` milliseconds for `done`, answering TLB shootdowns
/// (the caller may have IF=0: CLAUDE.md, busy-waits).
pub fn wait_ms(ms: u64, mut done: impl FnMut() -> bool) -> bool {
    let end = crate::cpu::tsc::uptime_ms() + ms;
    while crate::cpu::tsc::uptime_ms() < end {
        if done() {
            return true;
        }
        crate::memory::tlb::service_pending();
        core::hint::spin_loop();
    }
    done()
}

impl Edu {
    /// Finds the device, enables memory decoding and bus mastering, sizes
    /// and maps BAR0.
    pub fn find() -> Result<Edu, &'static str> {
        let mut found = None;
        crate::pci::for_each_function(|f| {
            if f.vendor == VENDOR && f.device_id == DEVICE {
                found = Some((f.bus, f.device, f.function));
            }
        });
        let (b, d, f) = found.ok_or("no edu device (QEMU without -device edu?)")?;
        let bar = crate::pci::size_bars(b, d, f)[0].ok_or("edu BAR0 unimplemented")?;
        if bar.size != 1 << 20 {
            return Err("edu BAR0 is not 1 MiB (edu.rst:35)");
        }
        use hal::pcicfg::{COMMAND_MASTER, COMMAND_MEMORY};
        crate::pci::update_command(b, d, f, COMMAND_MEMORY | COMMAND_MASTER, 0);
        // SAFETY: edu's register BAR.
        let v = unsafe { crate::memory::mmio::map(x86_64::PhysAddr::new(bar.addr), bar.size as usize) }
            .ok_or("cannot map edu BAR0")?;
        crate::pci::claim(b, d, f, "edu");
        Ok(Edu { bdf: (b, d, f), base: v.as_u64() })
    }

    pub fn ident(&self) -> u32 {
        rd32(self.base, IDENT)
    }

    pub fn liveness(&self, v: u32) -> u32 {
        wr32(self.base, LIVENESS, v);
        rd32(self.base, LIVENESS)
    }

    pub fn factorial(&self, n: u32) -> Option<u32> {
        wr32(self.base, FACTORIAL, n);
        wait_ms(1000, || rd32(self.base, STATUS) & STATUS_COMPUTING == 0).then(|| rd32(self.base, FACTORIAL))
    }

    /// Routes the device's interrupt to `dest_apic_id` by MSI. Returns the
    /// vector.
    pub fn enable_msi(&self, dest_apic_id: u32) -> Result<u8, &'static str> {
        BAR0.store(self.base, Ordering::Release);
        let vector = crate::interrupts::msi::alloc(on_msi).ok_or("no free MSI vector")?;
        let (b, d, f) = self.bdf;
        if let Err(e) = crate::pci::enable_msi(b, d, f, dest_apic_id, vector) {
            crate::interrupts::msi::free(vector);
            return Err(e);
        }
        Ok(vector)
    }

    pub fn raise_irq(&self, value: u32) {
        wr32(self.base, IRQ_RAISE, value);
    }

    pub fn irq_status(&self) -> u32 {
        rd32(self.base, IRQ_STATUS)
    }

    /// One DMA transfer; `to_ram` = device buffer → RAM. Raises
    /// [`DMA_DONE_IRQ`] when done. Waits for the run bit to clear.
    pub fn dma(&self, src: u64, dst: u64, len: usize, to_ram: bool) -> Result<(), &'static str> {
        wr64(self.base, DMA_SRC, src);
        wr64(self.base, DMA_DST, dst);
        wr64(self.base, DMA_COUNT, len as u64);
        let cmd = DMA_RUN | DMA_IRQ | if to_ram { DMA_TO_RAM } else { 0 };
        wr64(self.base, DMA_CMD, cmd);
        // The device completes on a 100 ms virtual-clock timer (`edu.c:191`).
        if wait_ms(2000, || rd64(self.base, DMA_CMD) & DMA_RUN == 0) {
            Ok(())
        } else {
            Err("edu DMA did not complete in 2 s")
        }
    }

    pub fn alloc_dma() -> Result<DmaBuf, DmaError> {
        DmaBuf::alloc(DMA_BUF_SIZE, DMA_MASK)
    }
}
