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

use crate::memory::dma::DmaBuf;
use crate::virtio_pci::{Device, Queue, DMA_MASK};
pub use crate::virtio_pci::InitError;

/// Descriptors per queue (the device's maximum is clamped to this).
const QUEUE_SIZE: u16 = 64;
/// Bytes per buffer slot: header + a full Ethernet frame, rounded up.
const SLOT: usize = 2048;

pub struct VirtioNet {
    mac: [u8; 6],
    rx: Queue,
    tx: Queue,
    /// Buffer slot owned by each in-flight head descriptor, per queue.
    rx_slot_of_head: Vec<u16>,
    tx_slot_of_head: Vec<u16>,
    rx_bufs: DmaBuf,
    tx_bufs: DmaBuf,
    /// Free TX slots (a slot is busy from `send` until its used entry).
    tx_free: Vec<u16>,
    dev: Device,
    pub rx_frames: u64,
    pub tx_frames: u64,
    pub tx_dropped: u64,
}

// SAFETY: see `virtio_pci::Mmio`; all access goes through the owner of the VirtioNet.
unsafe impl Send for VirtioNet {}

impl VirtioNet {
    /// Finds the first virtio-net function and brings it to DRIVER_OK.
    /// Boot-only: it maps MMIO and busy-waits on the device. With `irq_apic`
    /// (a local APIC id) it also sets up one MSI-X vector, shared by the
    /// config change and both queues, delivered there and handled by
    /// `network::irq`; without it, or when anything about MSI-X is missing,
    /// the device stays polled (`network::tick`).
    pub fn probe(irq_apic: Option<u32>) -> Result<VirtioNet, InitError> {
        let mut irq_vector = None;
        let dev = Device::probe("virtio-net", v::is_net, v::negotiate, |bus, d, func, bars| {
            irq_vector = irq_apic.and_then(|apic| Self::setup_msix(bus, d, func, bars, apic));
        })?;

        // MSI-X table entry 0 for the config change and for both queues.
        let msix = if irq_vector.is_some() { 0 } else { v::MSIX_NO_VECTOR };
        dev.common.w16(v::COMMON_MSIX_CONFIG, msix);
        let mut rx = dev.setup_queue(v::NET_QUEUE_RX, QUEUE_SIZE, msix)?;
        let tx = dev.setup_queue(v::NET_QUEUE_TX, QUEUE_SIZE, msix)?;
        let rx_n = rx.sq.layout().size as usize;
        let tx_n = tx.sq.layout().size as usize;
        let rx_bufs = DmaBuf::alloc(rx_n * SLOT, DMA_MASK).map_err(|_| InitError::Dma)?;
        let tx_bufs = DmaBuf::alloc(tx_n * SLOT, DMA_MASK).map_err(|_| InitError::Dma)?;

        let mut mac = [0u8; 6];
        if let Some(dc) = &dev.device_cfg {
            for (i, b) in mac.iter_mut().enumerate() {
                *b = dc.r8(v::NET_CFG_MAC + i);
            }
        }

        let mut rx_slot_of_head = alloc::vec![0; rx_n];
        // Hand every RX slot to the device before DRIVER_OK.
        for slot in 0..rx_n as u16 {
            Self::post_rx(&mut rx, &mut rx_slot_of_head, &rx_bufs, slot);
        }
        dev.driver_ok();
        rx.notify(v::NET_QUEUE_RX);

        crate::serial_println!(
            "virtio-net: {:02x}:{:02x}.{} mac {:02x?} queues rx={} tx={} features {:#x} irq {}",
            dev.bus, dev.dev, dev.func, mac, rx_n, tx_n, dev.features,
            match irq_vector {
                Some(vec) => alloc::format!("MSI-X vector {:#x}", vec),
                None => alloc::string::String::from("polled"),
            }
        );
        Ok(VirtioNet {
            mac,
            rx,
            tx,
            rx_slot_of_head,
            tx_slot_of_head: alloc::vec![0; tx_n],
            rx_bufs,
            tx_bufs,
            tx_free: (0..tx_n as u16).rev().collect(),
            dev,
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
        match &self.dev.device_cfg {
            Some(dc) => v::net_status_link_up(dc.r16(v::NET_CFG_STATUS)),
            None => true,
        }
    }

    fn post_rx(q: &mut Queue, slot_of_head: &mut [u16], bufs: &DmaBuf, slot: u16) {
        let addr = bufs.bus_addr() + slot as u64 * SLOT as u64;
        let head = q
            .sq
            .push(q.mem(), &[v::Buf { addr, len: SLOT as u32, write: true }])
            .expect("RX ring holds one descriptor per slot");
        slot_of_head[head as usize] = slot;
    }

    /// The device status byte, for diagnostics (NEEDS_RESET shows up here).
    pub fn device_status(&self) -> u8 {
        self.dev.status()
    }

    fn reclaim_tx(&mut self) {
        while let Some((head, _)) = self.tx.sq.pop_used(self.tx.mem()) {
            self.tx_free.push(self.tx_slot_of_head[head as usize]);
        }
    }
}

impl net::Nic for VirtioNet {
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        loop {
            let (head, len) = self.rx.sq.pop_used(self.rx.mem())?;
            let slot = self.rx_slot_of_head[head as usize];
            let total = len as usize;
            let keep = total > v::NET_HDR_LEN && total - v::NET_HDR_LEN <= buf.len() && total <= SLOT;
            let n = if keep { total - v::NET_HDR_LEN } else { 0 };
            if keep {
                self.rx_bufs.read(slot as usize * SLOT + v::NET_HDR_LEN, &mut buf[..n]);
                self.rx_frames += 1;
            }
            Self::post_rx(&mut self.rx, &mut self.rx_slot_of_head, &self.rx_bufs, slot);
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
                self.tx_slot_of_head[head as usize] = slot;
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
