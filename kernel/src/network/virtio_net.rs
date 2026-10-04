// kernel/src/net/virtio_net.rs
//
// virtio-net over the modern PCI transport, polled (no interrupts yet).
// The decisions (capability decoding, feature negotiation, ring layout and
// descriptor bookkeeping) are `hal::virtio`; this file owns what only the
// kernel can do: the MMIO windows, the DMA blocks, the doorbell, the waits.
//
// Frame path: one descriptor per buffer. A buffer is `HDR + MAX_FRAME` bytes
// at a fixed slot of one DMA block; the 12-byte virtio-net header sits right
// before the frame, so a single device-readable (TX) or device-writable (RX)
// descriptor covers both. No offloads and no MRG_RXBUF are negotiated.

use alloc::vec::Vec;
use hal::virtio as v;
use x86_64::PhysAddr;

use crate::memory::dma::DmaBuf;

/// Descriptors per queue (the device's maximum is clamped to this).
const QUEUE_SIZE: u16 = 64;
/// Bytes per buffer slot: header + a full Ethernet frame, rounded up.
const SLOT: usize = 2048;
/// QEMU's virtio devices address all of guest RAM.
const DMA_MASK: u64 = u64::MAX >> 16;

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

struct Mmio {
    base: *mut u8,
}

impl Mmio {
    fn r8(&self, off: usize) -> u8 {
        // SAFETY: `off` lies inside a window `map` returned for this region.
        unsafe { core::ptr::read_volatile(self.base.add(off)) }
    }
    fn r16(&self, off: usize) -> u16 {
        // SAFETY: as above; the offsets in `hal::virtio` are 2-aligned.
        unsafe { core::ptr::read_volatile(self.base.add(off) as *const u16) }
    }
    fn r32(&self, off: usize) -> u32 {
        // SAFETY: as above, 4-aligned offsets.
        unsafe { core::ptr::read_volatile(self.base.add(off) as *const u32) }
    }
    fn w8(&self, off: usize, val: u8) {
        // SAFETY: as `r8`.
        unsafe { core::ptr::write_volatile(self.base.add(off), val) }
    }
    fn w16(&self, off: usize, val: u16) {
        // SAFETY: as `r16`.
        unsafe { core::ptr::write_volatile(self.base.add(off) as *mut u16, val) }
    }
    fn w32(&self, off: usize, val: u32) {
        // SAFETY: as `r32`.
        unsafe { core::ptr::write_volatile(self.base.add(off) as *mut u32, val) }
    }
    fn w64(&self, off: usize, val: u64) {
        self.w32(off, val as u32);
        self.w32(off + 4, (val >> 32) as u32);
    }
}

// SAFETY: the pointers are device registers, only used under the driver's lock.
unsafe impl Send for Mmio {}

struct Queue {
    dma: DmaBuf,
    sq: v::SplitQueue,
    /// Doorbell register for this queue.
    doorbell: Mmio,
    /// Buffer slot owned by each in-flight head descriptor.
    slot_of_head: Vec<u16>,
}

impl Queue {
    fn mem(&self) -> &'static mut [u8] {
        // SAFETY: the block is ours until the driver is dropped (never); the
        // device writes into it, which `hal::virtio` accounts for with fences.
        unsafe { core::slice::from_raw_parts_mut(self.dma.virt(), self.dma.len()) }
    }

    fn notify(&self, index: u16) {
        // The notify area is 2 bytes wide per queue; the value is the index.
        self.doorbell.w16(0, index);
    }
}

pub struct VirtioNet {
    mac: [u8; 6],
    rx: Queue,
    tx: Queue,
    rx_bufs: DmaBuf,
    tx_bufs: DmaBuf,
    /// Free TX slots (a slot is busy from `send` until its used entry).
    tx_free: Vec<u16>,
    common: Mmio,
    device_cfg: Option<Mmio>,
    pub rx_frames: u64,
    pub tx_frames: u64,
    pub tx_dropped: u64,
}

// SAFETY: see `Mmio`; all access goes through the owner of the VirtioNet.
unsafe impl Send for VirtioNet {}

fn map_region(bars: &[Option<hal::pcicfg::Bar>; 6], r: v::CfgRegion) -> Result<Mmio, InitError> {
    let bar = bars[r.bar as usize].ok_or(InitError::NoBar(r.bar))?;
    if (r.offset as u64).checked_add(r.length as u64).map_or(true, |e| e > bar.size) {
        return Err(InitError::NoBar(r.bar));
    }
    // SAFETY: a device register window of a PCI BAR; boot-only, like xhci.
    let virt = unsafe { crate::memory::mmio::map(PhysAddr::new(bar.addr + r.offset as u64), r.length as usize) }
        .ok_or(InitError::MapFailed)?;
    Ok(Mmio { base: virt.as_mut_ptr() })
}

fn setup_queue(
    common: &Mmio,
    notify: &Mmio,
    mult: u32,
    index: u16,
    msix_vector: u16,
) -> Result<(DmaBuf, v::SplitQueue, Mmio), InitError> {
    common.w16(v::COMMON_QUEUE_SELECT, index);
    let max = common.r16(v::COMMON_QUEUE_SIZE);
    if max == 0 {
        return Err(InitError::Queue(index));
    }
    let size = QUEUE_SIZE.min(max);
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
    let off = common.r16(v::COMMON_QUEUE_NOTIFY_OFF) as u64 * mult as u64;
    common.w16(v::COMMON_QUEUE_ENABLE, 1);
    // One doorbell register per queue inside the notify window.
    let doorbell = Mmio { base: notify.base.wrapping_add(off as usize) };
    Ok((dma, sq, doorbell))
}

impl VirtioNet {
    /// Finds the first virtio-net function and brings it to DRIVER_OK.
    /// Boot-only: it maps MMIO and busy-waits on the device. With `irq_apic`
    /// (a local APIC id) it also sets up one MSI-X vector, shared by the
    /// config change and both queues, delivered there and handled by
    /// `network::irq`; without it, or when anything about MSI-X is missing,
    /// the device stays polled (`network::tick`).
    pub fn probe(irq_apic: Option<u32>) -> Result<VirtioNet, InitError> {
        let mut found = None;
        crate::pci::for_each_function(|f| {
            if found.is_none() && v::is_net(f.vendor, f.device_id) {
                found = Some(f);
            }
        });
        let f = found.ok_or(InitError::NoDevice)?;
        let (bus, dev, func) = (f.bus, f.device, f.function);
        let cfg = crate::pci::config_space(bus, dev, func);
        let caps = v::parse_caps(&cfg).ok_or(InitError::NoCaps)?;
        let bars = crate::pci::size_bars(bus, dev, func);
        crate::pci::claim(bus, dev, func, "virtio-net");
        crate::pci::enable_mem_and_bus_master(bus, dev, func);

        let common = map_region(&bars, caps.common)?;
        let irq_vector = irq_apic.and_then(|apic| Self::setup_msix(bus, dev, func, &bars, apic));
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
        let chosen = v::negotiate(offered).ok_or(InitError::Features(offered))?;
        common.w32(v::COMMON_DRIVER_FEATURE_SELECT, 0);
        common.w32(v::COMMON_DRIVER_FEATURE, chosen as u32);
        common.w32(v::COMMON_DRIVER_FEATURE_SELECT, 1);
        common.w32(v::COMMON_DRIVER_FEATURE, (chosen >> 32) as u32);
        let s = v::STATUS_ACKNOWLEDGE | v::STATUS_DRIVER | v::STATUS_FEATURES_OK;
        common.w8(v::COMMON_DEVICE_STATUS, s);
        if common.r8(v::COMMON_DEVICE_STATUS) & v::STATUS_FEATURES_OK == 0 {
            common.w8(v::COMMON_DEVICE_STATUS, v::STATUS_FAILED);
            return Err(InitError::Rejected);
        }

        // MSI-X table entry 0 for the config change and for both queues.
        let msix = if irq_vector.is_some() { 0 } else { v::MSIX_NO_VECTOR };
        common.w16(v::COMMON_MSIX_CONFIG, msix);
        let (rx_dma, rx_sq, rx_bell) = setup_queue(&common, &notify, caps.notify_off_multiplier, v::NET_QUEUE_RX, msix)?;
        let (tx_dma, tx_sq, tx_bell) = setup_queue(&common, &notify, caps.notify_off_multiplier, v::NET_QUEUE_TX, msix)?;
        let rx_n = rx_sq.layout().size as usize;
        let tx_n = tx_sq.layout().size as usize;
        let rx_bufs = DmaBuf::alloc(rx_n * SLOT, DMA_MASK).map_err(|_| InitError::Dma)?;
        let tx_bufs = DmaBuf::alloc(tx_n * SLOT, DMA_MASK).map_err(|_| InitError::Dma)?;

        let mut mac = [0u8; 6];
        if let Some(dc) = &device_cfg {
            for (i, b) in mac.iter_mut().enumerate() {
                *b = dc.r8(v::NET_CFG_MAC + i);
            }
        }

        let mut rx = Queue { dma: rx_dma, sq: rx_sq, doorbell: rx_bell, slot_of_head: alloc::vec![0; rx_n] };
        let tx = Queue { dma: tx_dma, sq: tx_sq, doorbell: tx_bell, slot_of_head: alloc::vec![0; tx_n] };
        // Hand every RX slot to the device before DRIVER_OK.
        for slot in 0..rx_n as u16 {
            Self::post_rx(&mut rx, &rx_bufs, slot);
        }
        common.w8(v::COMMON_DEVICE_STATUS, s | v::STATUS_DRIVER_OK);
        rx.notify(v::NET_QUEUE_RX);

        crate::serial_println!(
            "virtio-net: {:02x}:{:02x}.{} mac {:02x?} queues rx={} tx={} features {:#x} irq {}",
            bus, dev, func, mac, rx_n, tx_n, chosen,
            match irq_vector {
                Some(vec) => alloc::format!("MSI-X vector {:#x}", vec),
                None => alloc::string::String::from("polled"),
            }
        );
        Ok(VirtioNet {
            mac,
            rx,
            tx,
            rx_bufs,
            tx_bufs,
            tx_free: (0..tx_n as u16).rev().collect(),
            common,
            device_cfg,
            rx_frames: 0,
            tx_frames: 0,
            tx_dropped: 0,
        })
    }

    /// Reserves a vector, programs MSI-X entry 0 to deliver it to `apic`, and
    /// returns it. Logs and returns `None` (polled) if any step fails.
    fn setup_msix(bus: u8, dev: u8, func: u8, bars: &[Option<hal::pcicfg::Bar>; 6], apic: u32) -> Option<u8> {
        let vector = crate::interrupts::msi::alloc(crate::network::irq)?;
        match crate::pci::enable_msix(bus, dev, func, bars, apic, vector) {
            Ok(()) => Some(vector),
            Err(e) => {
                crate::interrupts::msi::free(vector);
                crate::serial_println!("virtio-net: no MSI-X ({}), polling", e);
                None
            }
        }
    }

    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    pub fn link_up(&self) -> bool {
        match &self.device_cfg {
            Some(dc) => v::net_status_link_up(dc.r16(v::NET_CFG_STATUS)),
            None => true,
        }
    }

    fn post_rx(q: &mut Queue, bufs: &DmaBuf, slot: u16) {
        let addr = bufs.bus_addr() + slot as u64 * SLOT as u64;
        let head = q
            .sq
            .push(q.mem(), &[v::Buf { addr, len: SLOT as u32, write: true }])
            .expect("RX ring holds one descriptor per slot");
        q.slot_of_head[head as usize] = slot;
    }

    /// The device status byte, for diagnostics (NEEDS_RESET shows up here).
    pub fn device_status(&self) -> u8 {
        self.common.r8(v::COMMON_DEVICE_STATUS)
    }

    fn reclaim_tx(&mut self) {
        while let Some((head, _)) = self.tx.sq.pop_used(self.tx.mem()) {
            self.tx_free.push(self.tx.slot_of_head[head as usize]);
        }
    }
}

impl net::Nic for VirtioNet {
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        loop {
            let (head, len) = self.rx.sq.pop_used(self.rx.mem())?;
            let slot = self.rx.slot_of_head[head as usize];
            let total = len as usize;
            let keep = total > v::NET_HDR_LEN && total - v::NET_HDR_LEN <= buf.len() && total <= SLOT;
            let n = if keep { total - v::NET_HDR_LEN } else { 0 };
            if keep {
                self.rx_bufs.read(slot as usize * SLOT + v::NET_HDR_LEN, &mut buf[..n]);
                self.rx_frames += 1;
            }
            Self::post_rx(&mut self.rx, &self.rx_bufs, slot);
            self.rx.notify(v::NET_QUEUE_RX);
            if keep {
                return Some(n);
            }
            // Runt or oversized: dropped, look at the next completion.
        }
    }

    fn send(&mut self, frame: &[u8]) -> bool {
        self.reclaim_tx();
        if frame.is_empty() || frame.len() > SLOT - v::NET_HDR_LEN {
            self.tx_dropped += 1;
            return false;
        }
        let Some(slot) = self.tx_free.pop() else {
            self.tx_dropped += 1;
            return false;
        };
        let base = slot as usize * SLOT;
        self.tx_bufs.write(base, &[0u8; v::NET_HDR_LEN]);
        self.tx_bufs.write(base + v::NET_HDR_LEN, frame);
        let addr = self.tx_bufs.bus_addr() + base as u64;
        let len = (v::NET_HDR_LEN + frame.len()) as u32;
        match self.tx.sq.push(self.tx.mem(), &[v::Buf { addr, len, write: false }]) {
            Some(head) => {
                self.tx.slot_of_head[head as usize] = slot;
                self.tx.notify(v::NET_QUEUE_TX);
                self.tx_frames += 1;
                true
            }
            None => {
                self.tx_free.push(slot);
                self.tx_dropped += 1;
                false
            }
        }
    }
}

/// Bounded busy-wait for a register condition (device reset takes
/// microseconds; the bound only guards a dead device).
fn wait(cond: impl Fn() -> bool) -> Result<(), InitError> {
    for _ in 0..5_000_000u32 {
        if cond() {
            return Ok(());
        }
        crate::memory::tlb::service_pending();
        core::hint::spin_loop();
    }
    Err(InitError::Rejected)
}
