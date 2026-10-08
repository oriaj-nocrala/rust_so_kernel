// kernel/src/block/virtio_blk.rs
//
// virtio-blk over the modern PCI transport, polled, one request in flight.
// The disk QEMU runs use for `/mnt` (`-device virtio-blk-pci`): under KVM
// every ATA PIO word is a VM exit (mounting disk.img took 36 s), while a
// virtio request is one doorbell write for up to 128 KiB.
//
// The decisions (feature choice, request header, descriptor chain, range
// check) are `hal::virtio`; this file owns the queue, the DMA blocks and
// the wait. Same locking as `block::ata`: the lock is held with interrupts
// off, since readers arrive with IF=0 from `sys_read` (fd-table lock) and
// a preempted holder would leave them spinning.
//
// Writes are complete when the device says so: the data is in QEMU's host
// page cache, which survives the guest crashing and QEMU being killed.
// `flush` (sync, reboot) asks for it to reach the host's disk.

use core::sync::atomic::{AtomicU64, Ordering};
use hal::virtio as v;

use crate::memory::dma::DmaBuf;
use crate::sync::Mutex;
use crate::virtio_pci::{Device, InitError, Queue, DMA_MASK};

use super::{BlockDevice, SECTOR_SIZE};

/// The largest request: 256 sectors, `count == 0` in the LBA28 convention.
const MAX_BYTES: usize = 256 * SECTOR_SIZE;
/// A request needs 3 descriptors and only one is in flight.
const QUEUE_SIZE: u16 = 4;
/// Status byte offset inside the request block (after the header).
const STATUS_OFF: usize = v::BLK_HDR_LEN;

struct VirtioBlk {
    dev: Device,
    q: Queue,
    /// Header + status byte of the request in flight.
    req: DmaBuf,
    /// Bounce buffer for the data.
    data: DmaBuf,
    capacity: u64,
    read_only: bool,
    has_flush: bool,
}

// SAFETY: see `virtio_pci::Mmio`; only used under `VBLK`.
unsafe impl Send for VirtioBlk {}

static VBLK: Mutex<Option<VirtioBlk>> = Mutex::new(None);

/// Requests completed, the time spent waiting for them, and flushes.
static REQUESTS: AtomicU64 = AtomicU64::new(0);
static WAIT_NS: AtomicU64 = AtomicU64::new(0);
static FLUSHES: AtomicU64 = AtomicU64::new(0);

/// `(requests, nanoseconds waited)` since boot.
pub fn stats() -> (u64, u64) {
    (REQUESTS.load(Ordering::Relaxed), WAIT_NS.load(Ordering::Relaxed))
}

/// `/proc/kdebug` line.
pub fn render_kdebug() -> alloc::string::String {
    alloc::format!(
        "virtio_blk: present={} requests={} wait_us={} flushes={}",
        present() as u32,
        REQUESTS.load(Ordering::Relaxed),
        WAIT_NS.load(Ordering::Relaxed) / 1000,
        FLUSHES.load(Ordering::Relaxed)
    )
}

/// Finds the first virtio-blk function and brings it up. `Err(NoDevice)`
/// is the ordinary answer on a machine (or a QEMU run) without one.
pub fn init() -> Result<(), InitError> {
    let dev = Device::probe("virtio-blk", v::is_blk, v::negotiate_blk, |_, _, _, _| {})?;
    dev.common.w16(v::COMMON_MSIX_CONFIG, v::MSIX_NO_VECTOR);
    let q = dev.setup_queue(v::BLK_QUEUE, QUEUE_SIZE, v::MSIX_NO_VECTOR)?;
    let req = DmaBuf::alloc(v::BLK_HDR_LEN + 1, DMA_MASK).map_err(|_| InitError::Dma)?;
    let data = DmaBuf::alloc(MAX_BYTES, DMA_MASK).map_err(|_| InitError::Dma)?;
    let capacity = match &dev.device_cfg {
        Some(dc) => dc.r32(v::BLK_CFG_CAPACITY) as u64 | (dc.r32(v::BLK_CFG_CAPACITY + 4) as u64) << 32,
        None => return Err(InitError::NoCaps),
    };
    dev.driver_ok();
    let read_only = dev.features & v::BLK_F_RO != 0;
    let has_flush = dev.features & v::BLK_F_FLUSH != 0;
    crate::serial_println!(
        "virtio-blk: {:02x}:{:02x}.{} {} sectors ({} MiB){}{} features {:#x}",
        dev.bus, dev.dev, dev.func, capacity, capacity / 2048,
        if read_only { ", read-only" } else { "" },
        if has_flush { ", write-back cache" } else { "" },
        dev.features
    );
    let blk = VirtioBlk { dev, q, req, data, capacity, read_only, has_flush };
    x86_64::instructions::interrupts::without_interrupts(|| *VBLK.lock() = Some(blk));
    Ok(())
}

pub fn present() -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| VBLK.lock().is_some())
}

impl VirtioBlk {
    /// Runs one request to completion. `len` bytes of `self.data` are the
    /// payload (none for a flush).
    fn request(&mut self, req_type: u32, sector: u64, len: usize) -> Result<(), &'static str> {
        self.req.write(0, &v::blk_header(req_type, sector));
        self.req.write(STATUS_OFF, &[0xFF]);
        let data = (len > 0).then(|| (self.data.bus_addr(), len as u32));
        let (bufs, n) = v::blk_chain(req_type, self.req.bus_addr(), data, self.req.bus_addr() + STATUS_OFF as u64);
        let head = self.q.sq.push(self.q.mem(), &bufs[..n]).ok_or("virtio-blk: queue full")?;
        let t0 = crate::time::clocksource::ktime_get();
        self.q.notify(v::BLK_QUEUE);
        // Bounded: a dead device must not hang the caller (IF=0) forever.
        let mut spins: u64 = 0;
        loop {
            if let Some((done, _)) = self.q.sq.pop_used(self.q.mem()) {
                if done == head {
                    break;
                }
                continue;
            }
            spins += 1;
            if spins > 2_000_000_000 {
                crate::serial_println!("virtio-blk: request timed out, device status {:#x}", self.dev.status());
                return Err("virtio-blk: request timed out");
            }
            crate::memory::tlb::service_pending();
            core::hint::spin_loop();
        }
        REQUESTS.fetch_add(1, Ordering::Relaxed);
        WAIT_NS.fetch_add(crate::time::clocksource::ktime_get().saturating_sub(t0), Ordering::Relaxed);
        let mut status = [0u8];
        self.req.read(STATUS_OFF, &mut status);
        match status[0] {
            v::BLK_S_OK => Ok(()),
            v::BLK_S_UNSUPP => Err("virtio-blk: request not supported"),
            _ => Err("virtio-blk: I/O error"),
        }
    }
}

fn with_dev<R>(f: impl FnOnce(&mut VirtioBlk) -> Result<R, &'static str>) -> Result<R, &'static str> {
    x86_64::instructions::interrupts::without_interrupts(|| match VBLK.lock().as_mut() {
        Some(d) => f(d),
        None => Err("virtio-blk: no device"),
    })
}

pub fn read_sectors(lba: u32, count: u8, buf: &mut [u8]) -> Result<(), &'static str> {
    with_dev(|d| {
        let n = v::blk_range(lba, count, d.capacity).ok_or("virtio-blk: read past the end of the disk")?;
        let len = n * SECTOR_SIZE;
        if buf.len() < len {
            return Err("virtio-blk: buffer too small");
        }
        d.request(v::BLK_T_IN, lba as u64, len)?;
        d.data.copy_out(0, &mut buf[..len]);
        Ok(())
    })
}

pub fn write_sectors(lba: u32, count: u8, buf: &[u8]) -> Result<(), &'static str> {
    with_dev(|d| {
        if d.read_only {
            return Err("virtio-blk: read-only device");
        }
        let n = v::blk_range(lba, count, d.capacity).ok_or("virtio-blk: write past the end of the disk")?;
        let len = n * SECTOR_SIZE;
        if buf.len() < len {
            return Err("virtio-blk: buffer too small");
        }
        d.data.copy_in(0, &buf[..len]);
        d.request(v::BLK_T_OUT, lba as u64, len)
    })
}

/// Asks the device to write its cache out. `Ok(false)`: no device, or one
/// without a write-back cache (nothing to flush).
pub fn flush() -> Result<bool, &'static str> {
    if !present() {
        return Ok(false);
    }
    with_dev(|d| {
        if !d.has_flush {
            return Ok(false);
        }
        d.request(v::BLK_T_FLUSH, 0, 0)?;
        FLUSHES.fetch_add(1, Ordering::Relaxed);
        Ok(true)
    })
}

/// The `BlockDevice` face `fs::ext2` mounts: zero-sized, like
/// `AtaBlockDevice`, over the one device in `VBLK`.
#[derive(Clone, Copy, Default)]
pub struct VirtioBlkDevice;

impl BlockDevice for VirtioBlkDevice {
    fn present(&self) -> bool {
        present()
    }

    fn read_sectors(&self, lba: u32, count: u8, buf: &mut [u8]) -> Result<(), &'static str> {
        read_sectors(lba, count, buf)
    }

    fn write_sectors(&self, lba: u32, count: u8, buf: &[u8]) -> Result<(), &'static str> {
        write_sectors(lba, count, buf)
    }
}
