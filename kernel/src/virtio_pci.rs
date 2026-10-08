// kernel/src/virtio_pci.rs
//
// The virtio 1.x modern PCI transport, shared by every virtio driver
// (`network::virtio_net`, `block::virtio_blk`): finding the function,
// mapping its configuration windows, the §3.1.1 status/feature handshake
// and setting up a split queue. The decisions (capability decoding, ring
// layout, feature choice) are `hal::virtio`; this file does the MMIO, the
// DMA allocation and the busy-waits.

use hal::virtio as v;
use x86_64::PhysAddr;

use crate::memory::dma::DmaBuf;

/// QEMU's virtio devices address all of guest RAM.
pub const DMA_MASK: u64 = u64::MAX >> 16;

#[derive(Debug)]
pub enum InitError {
    NoDevice,
    NoCaps,
    NoBar(u8),
    MapFailed,
    Features(u64),
    Rejected,
    Queue(u16),
    Dma,
}

pub struct Mmio {
    base: *mut u8,
}

impl Mmio {
    pub fn r8(&self, off: usize) -> u8 {
        // SAFETY: `off` lies inside a window `map_region` returned for this region.
        unsafe { core::ptr::read_volatile(self.base.add(off)) }
    }
    pub fn r16(&self, off: usize) -> u16 {
        // SAFETY: as above; the offsets in `hal::virtio` are 2-aligned.
        unsafe { core::ptr::read_volatile(self.base.add(off) as *const u16) }
    }
    pub fn r32(&self, off: usize) -> u32 {
        // SAFETY: as above, 4-aligned offsets.
        unsafe { core::ptr::read_volatile(self.base.add(off) as *const u32) }
    }
    pub fn w8(&self, off: usize, val: u8) {
        // SAFETY: as `r8`.
        unsafe { core::ptr::write_volatile(self.base.add(off), val) }
    }
    pub fn w16(&self, off: usize, val: u16) {
        // SAFETY: as `r16`.
        unsafe { core::ptr::write_volatile(self.base.add(off) as *mut u16, val) }
    }
    pub fn w32(&self, off: usize, val: u32) {
        // SAFETY: as `r32`.
        unsafe { core::ptr::write_volatile(self.base.add(off) as *mut u32, val) }
    }
    pub fn w64(&self, off: usize, val: u64) {
        self.w32(off, val as u32);
        self.w32(off + 4, (val >> 32) as u32);
    }
}

// SAFETY: the pointers are device registers, only used under the driver's lock.
unsafe impl Send for Mmio {}

/// One split queue: its ring memory, the driver-side state and its doorbell.
pub struct Queue {
    pub dma: DmaBuf,
    pub sq: v::SplitQueue,
    doorbell: Mmio,
}

impl Queue {
    pub fn mem(&self) -> &'static mut [u8] {
        // SAFETY: the block is ours until the driver is dropped (never); the
        // device writes into it, which `hal::virtio` accounts for with fences.
        unsafe { core::slice::from_raw_parts_mut(self.dma.virt(), self.dma.len()) }
    }

    pub fn notify(&self, index: u16) {
        // The notify area is 2 bytes wide per queue; the value is the index.
        self.doorbell.w16(0, index);
    }
}

/// A function found and mapped, its handshake done up to FEATURES_OK.
pub struct Device {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub common: Mmio,
    pub notify: Mmio,
    pub notify_off_multiplier: u32,
    pub device_cfg: Option<Mmio>,
    /// The features written back (`negotiate`'s choice).
    pub features: u64,
}

pub fn map_region(bars: &[Option<hal::pcicfg::Bar>; 6], r: v::CfgRegion) -> Result<Mmio, InitError> {
    let bar = bars[r.bar as usize].ok_or(InitError::NoBar(r.bar))?;
    if (r.offset as u64).checked_add(r.length as u64).map_or(true, |e| e > bar.size) {
        return Err(InitError::NoBar(r.bar));
    }
    // SAFETY: a device register window of a PCI BAR; boot-only, like xhci.
    let virt = unsafe { crate::memory::mmio::map(PhysAddr::new(bar.addr + r.offset as u64), r.length as usize) }
        .ok_or(InitError::MapFailed)?;
    Ok(Mmio { base: virt.as_mut_ptr() })
}

impl Device {
    /// Finds the first function `matches(vendor, device)` accepts, claims it
    /// as `name`, maps its windows, resets it and negotiates features up to
    /// FEATURES_OK. `before_reset` runs with the BARs sized and the common
    /// window mapped (MSI-X setup goes there). Boot-only: it busy-waits on
    /// the device.
    pub fn probe(
        name: &'static str,
        matches: fn(u16, u16) -> bool,
        negotiate: fn(u64) -> Option<u64>,
        before_reset: impl FnOnce(u8, u8, u8, &[Option<hal::pcicfg::Bar>; 6]),
    ) -> Result<Device, InitError> {
        let mut found = None;
        crate::pci::for_each_function(|f| {
            if found.is_none() && matches(f.vendor, f.device_id) {
                found = Some(f);
            }
        });
        let f = found.ok_or(InitError::NoDevice)?;
        let (bus, dev, func) = (f.bus, f.device, f.function);
        let cfg = crate::pci::config_space(bus, dev, func);
        let caps = v::parse_caps(&cfg).ok_or(InitError::NoCaps)?;
        let bars = crate::pci::size_bars(bus, dev, func);
        crate::pci::claim(bus, dev, func, name);
        crate::pci::enable_mem_and_bus_master(bus, dev, func);

        let common = map_region(&bars, caps.common)?;
        before_reset(bus, dev, func, &bars);
        let notify = map_region(&bars, caps.notify)?;
        let device_cfg = match caps.device {
            Some(r) => Some(map_region(&bars, r)?),
            None => None,
        };

        // Reset, then the §3.1.1 handshake.
        common.w8(v::COMMON_DEVICE_STATUS, 0);
        wait(|| common.r8(v::COMMON_DEVICE_STATUS) == 0)?;
        common.w8(v::COMMON_DEVICE_STATUS, v::STATUS_ACKNOWLEDGE);
        common.w8(v::COMMON_DEVICE_STATUS, v::STATUS_ACKNOWLEDGE | v::STATUS_DRIVER);

        common.w32(v::COMMON_DEVICE_FEATURE_SELECT, 0);
        let lo = common.r32(v::COMMON_DEVICE_FEATURE) as u64;
        common.w32(v::COMMON_DEVICE_FEATURE_SELECT, 1);
        let hi = common.r32(v::COMMON_DEVICE_FEATURE) as u64;
        let offered = lo | hi << 32;
        let chosen = negotiate(offered).ok_or(InitError::Features(offered))?;
        common.w32(v::COMMON_DRIVER_FEATURE_SELECT, 0);
        common.w32(v::COMMON_DRIVER_FEATURE, chosen as u32);
        common.w32(v::COMMON_DRIVER_FEATURE_SELECT, 1);
        common.w32(v::COMMON_DRIVER_FEATURE, (chosen >> 32) as u32);
        common.w8(v::COMMON_DEVICE_STATUS, Self::FEATURES_OK);
        if common.r8(v::COMMON_DEVICE_STATUS) & v::STATUS_FEATURES_OK == 0 {
            common.w8(v::COMMON_DEVICE_STATUS, v::STATUS_FAILED);
            return Err(InitError::Rejected);
        }
        Ok(Device {
            bus,
            dev,
            func,
            common,
            notify,
            notify_off_multiplier: caps.notify_off_multiplier,
            device_cfg,
            features: chosen,
        })
    }

    const FEATURES_OK: u8 = v::STATUS_ACKNOWLEDGE | v::STATUS_DRIVER | v::STATUS_FEATURES_OK;

    /// Allocates and enables queue `index` (at most `max_size` entries),
    /// delivering its interrupts to MSI-X entry `msix_vector` (or none).
    pub fn setup_queue(&self, index: u16, max_size: u16, msix_vector: u16) -> Result<Queue, InitError> {
        let common = &self.common;
        common.w16(v::COMMON_QUEUE_SELECT, index);
        let max = common.r16(v::COMMON_QUEUE_SIZE);
        if max == 0 {
            return Err(InitError::Queue(index));
        }
        let size = max_size.min(max);
        let layout = v::QueueLayout::new(size).ok_or(InitError::Queue(index))?;
        let dma = DmaBuf::alloc(layout.total, DMA_MASK).map_err(|_| InitError::Dma)?;
        // SAFETY: freshly allocated, zeroed, not yet visible to the device.
        let mem = unsafe { core::slice::from_raw_parts_mut(dma.virt(), dma.len()) };
        let sq = v::SplitQueue::new(layout, mem);
        common.w16(v::COMMON_QUEUE_SIZE, size);
        common.w16(v::COMMON_QUEUE_MSIX_VECTOR, msix_vector);
        if msix_vector != v::MSIX_NO_VECTOR && common.r16(v::COMMON_QUEUE_MSIX_VECTOR) == v::MSIX_NO_VECTOR {
            // The device refused the vector (out of its MSI-X table): fall back to polling.
            common.w16(v::COMMON_QUEUE_MSIX_VECTOR, v::MSIX_NO_VECTOR);
        }
        common.w64(v::COMMON_QUEUE_DESC, dma.bus_addr() + layout.desc as u64);
        common.w64(v::COMMON_QUEUE_DRIVER, dma.bus_addr() + layout.avail as u64);
        common.w64(v::COMMON_QUEUE_DEVICE, dma.bus_addr() + layout.used as u64);
        let off = common.r16(v::COMMON_QUEUE_NOTIFY_OFF) as u64 * self.notify_off_multiplier as u64;
        common.w16(v::COMMON_QUEUE_ENABLE, 1);
        // One doorbell register per queue inside the notify window.
        let doorbell = Mmio { base: self.notify.base.wrapping_add(off as usize) };
        Ok(Queue { dma, sq, doorbell })
    }

    /// Sets DRIVER_OK: the device is live from here on.
    pub fn driver_ok(&self) {
        self.common.w8(v::COMMON_DEVICE_STATUS, Self::FEATURES_OK | v::STATUS_DRIVER_OK);
    }

    /// The device status byte, for diagnostics (NEEDS_RESET shows up here).
    pub fn status(&self) -> u8 {
        self.common.r8(v::COMMON_DEVICE_STATUS)
    }
}

/// Bounded busy-wait for a register condition (device reset takes
/// microseconds; the bound only guards a dead device).
pub fn wait(cond: impl Fn() -> bool) -> Result<(), InitError> {
    for _ in 0..5_000_000u32 {
        if cond() {
            return Ok(());
        }
        crate::memory::tlb::service_pending();
        core::hint::spin_loop();
    }
    Err(InitError::Rejected)
}
