//! virtio 1.x over PCI ("modern" transport) and the split virtqueue: the
//! pure half of a virtio-net driver — host-tested.
//!
//! Same split as `xhci`/`ac97`: this module decides *what bytes go where*
//! (capability decoding, ring layout, descriptor chains, the used-ring
//! walk); the kernel adapter owns the MMIO window, the DMA allocation and
//! the doorbell. Queue memory is passed in as a plain `&mut [u8]` covering
//! the whole region, so tests back it with a `Vec`.
//!
//! References: virtio 1.1 spec §2.6 (split virtqueues), §4.1 (PCI),
//! §5.1 (network device).

use crate::pcicfg;
use alloc::vec::Vec;

// ── PCI identification (§4.1.2) ─────────────────────────────────────────────

pub const PCI_VENDOR: u16 = 0x1AF4;
/// Transitional (legacy-capable) network device. Also speaks the modern
/// interface when the vendor capabilities are present.
pub const PCI_DEVICE_NET_TRANSITIONAL: u16 = 0x1000;
/// Modern-only network device (`0x1040 + device type 1`).
pub const PCI_DEVICE_NET_MODERN: u16 = 0x1041;

pub fn is_net(vendor: u16, device: u16) -> bool {
    vendor == PCI_VENDOR && (device == PCI_DEVICE_NET_TRANSITIONAL || device == PCI_DEVICE_NET_MODERN)
}

// ── Vendor capabilities (§4.1.4) ────────────────────────────────────────────

/// `PCI_CAP_ID_VNDR`.
const CAP_ID_VENDOR: u8 = 0x09;

pub const CAP_COMMON_CFG: u8 = 1;
pub const CAP_NOTIFY_CFG: u8 = 2;
pub const CAP_ISR_CFG: u8 = 3;
pub const CAP_DEVICE_CFG: u8 = 4;

/// Where one configuration structure lives: a BAR, an offset into it and a
/// length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CfgRegion {
    pub bar: u8,
    pub offset: u32,
    pub length: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtioCaps {
    pub common: CfgRegion,
    pub notify: CfgRegion,
    /// `queue_notify_off * notify_off_multiplier` is the doorbell's byte
    /// offset inside `notify`.
    pub notify_off_multiplier: u32,
    pub isr: CfgRegion,
    pub device: Option<CfgRegion>,
}

fn u32_at(cfg: &[u8; 256], off: usize) -> u32 {
    u32::from_le_bytes([cfg[off], cfg[off + 1], cfg[off + 2], cfg[off + 3]])
}

/// Walks the vendor capabilities of a function's configuration space and
/// collects the four structures a driver needs. `None` unless common,
/// notify and ISR are all there (a legacy-only device has none of them).
/// The first capability of each type wins, as the spec says to prefer.
pub fn parse_caps(cfg: &[u8; 256]) -> Option<VirtioCaps> {
    let mut common = None;
    let mut notify = None;
    let mut mult = 0;
    let mut isr = None;
    let mut device = None;
    for (id, off) in pcicfg::capabilities(cfg) {
        if id != CAP_ID_VENDOR {
            continue;
        }
        let off = off as usize;
        // cap_len (off+2) must cover the 16-byte virtio_pci_cap; the notify
        // capability adds a 4-byte multiplier, so keep reads inside cfg.
        let cap_len = cfg[off + 2] as usize;
        if cap_len < 16 || off + cap_len > 256 {
            continue;
        }
        let region = CfgRegion { bar: cfg[off + 4], offset: u32_at(cfg, off + 8), length: u32_at(cfg, off + 12) };
        if region.bar > 5 {
            continue;
        }
        match cfg[off + 3] {
            CAP_COMMON_CFG if common.is_none() => common = Some(region),
            CAP_NOTIFY_CFG if notify.is_none() && cap_len >= 20 => {
                notify = Some(region);
                mult = u32_at(cfg, off + 16);
            }
            CAP_ISR_CFG if isr.is_none() => isr = Some(region),
            CAP_DEVICE_CFG if device.is_none() => device = Some(region),
            _ => {}
        }
    }
    Some(VirtioCaps { common: common?, notify: notify?, notify_off_multiplier: mult, isr: isr?, device })
}

// ── Common configuration layout (§4.1.4.3) ──────────────────────────────────

pub const COMMON_DEVICE_FEATURE_SELECT: usize = 0x00;
pub const COMMON_DEVICE_FEATURE: usize = 0x04;
pub const COMMON_DRIVER_FEATURE_SELECT: usize = 0x08;
pub const COMMON_DRIVER_FEATURE: usize = 0x0C;
pub const COMMON_MSIX_CONFIG: usize = 0x10;
pub const COMMON_NUM_QUEUES: usize = 0x12;
pub const COMMON_DEVICE_STATUS: usize = 0x14;
pub const COMMON_CONFIG_GENERATION: usize = 0x15;
pub const COMMON_QUEUE_SELECT: usize = 0x16;
pub const COMMON_QUEUE_SIZE: usize = 0x18;
pub const COMMON_QUEUE_MSIX_VECTOR: usize = 0x1A;
pub const COMMON_QUEUE_ENABLE: usize = 0x1C;
pub const COMMON_QUEUE_NOTIFY_OFF: usize = 0x1E;
pub const COMMON_QUEUE_DESC: usize = 0x20;
pub const COMMON_QUEUE_DRIVER: usize = 0x28;
pub const COMMON_QUEUE_DEVICE: usize = 0x30;

/// `VIRTIO_MSI_NO_VECTOR`: written to `msix_config` / `queue_msix_vector` for "no
/// interrupt" (and what the device answers when it refuses a vector).
pub const MSIX_NO_VECTOR: u16 = 0xFFFF;

// ── Device status (§2.1) ────────────────────────────────────────────────────

pub const STATUS_ACKNOWLEDGE: u8 = 1;
pub const STATUS_DRIVER: u8 = 2;
pub const STATUS_DRIVER_OK: u8 = 4;
pub const STATUS_FEATURES_OK: u8 = 8;
pub const STATUS_NEEDS_RESET: u8 = 64;
pub const STATUS_FAILED: u8 = 128;

// ── Features ────────────────────────────────────────────────────────────────

/// `VIRTIO_F_VERSION_1` (bit 32): the device follows the 1.x interface.
pub const F_VERSION_1: u64 = 1 << 32;
/// `VIRTIO_NET_F_MAC`: the device config holds a MAC address.
pub const NET_F_MAC: u64 = 1 << 5;
/// `VIRTIO_NET_F_STATUS`: the device config holds a link status.
pub const NET_F_STATUS: u64 = 1 << 16;

/// What this driver asks for. No offloads: every frame is a plain Ethernet
/// frame behind a 12-byte header, so smoltcp computes the checksums.
pub const WANTED_FEATURES: u64 = F_VERSION_1 | NET_F_MAC | NET_F_STATUS;

/// The feature set to write back: what the device offers, restricted to
/// what we want. `None` when the device lacks `VERSION_1` or `MAC` (we need
/// both: legacy framing and a random MAC are not implemented).
pub fn negotiate(device_features: u64) -> Option<u64> {
    let chosen = device_features & WANTED_FEATURES;
    if chosen & F_VERSION_1 == 0 || chosen & NET_F_MAC == 0 {
        return None;
    }
    Some(chosen)
}

// ── virtio-net device configuration and header (§5.1) ───────────────────────

/// Device config: `mac[6]` at 0, `status` (u16) at 6.
pub const NET_CFG_MAC: usize = 0;
pub const NET_CFG_STATUS: usize = 6;
pub const NET_S_LINK_UP: u16 = 1;

/// Queue numbers: 0 is receiveq1, 1 is transmitq1 (no multiqueue/control).
pub const NET_QUEUE_RX: u16 = 0;
pub const NET_QUEUE_TX: u16 = 1;

/// `struct virtio_net_hdr` with `VERSION_1` (it always carries
/// `num_buffers`). Sent all-zero (no offload) and received with
/// `num_buffers == 1` since we do not negotiate `MRG_RXBUF`.
pub const NET_HDR_LEN: usize = 12;

pub fn net_status_link_up(status: u16) -> bool {
    status & NET_S_LINK_UP != 0
}

// ── Split virtqueue (§2.6) ──────────────────────────────────────────────────

pub const DESC_F_NEXT: u16 = 1;
pub const DESC_F_WRITE: u16 = 2;

const DESC_SIZE: usize = 16;

/// Byte offsets of the three areas inside one contiguous queue allocation.
/// Rings need 16/2/4-byte alignment (descriptors, available, used); the
/// used ring is placed on a 4-byte boundary after the available ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueLayout {
    pub size: u16,
    pub desc: usize,
    pub avail: usize,
    pub used: usize,
    /// Total bytes to allocate (not rounded to a page).
    pub total: usize,
}

impl QueueLayout {
    /// `None` unless `size` is a power of two in `1..=32768` (§2.6).
    pub fn new(size: u16) -> Option<QueueLayout> {
        if size == 0 || size > 32768 || !size.is_power_of_two() {
            return None;
        }
        let n = size as usize;
        let desc = 0;
        let avail = desc + DESC_SIZE * n;
        // flags, idx, ring[n], used_event
        let avail_end = avail + 2 + 2 + 2 * n + 2;
        let used = (avail_end + 3) & !3;
        // flags, idx, ring[n] of {id, len}, avail_event
        let total = used + 2 + 2 + 8 * n + 2;
        Some(QueueLayout { size, desc, avail, used, total })
    }
}

fn rd16(m: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([m[o], m[o + 1]])
}
fn wr16(m: &mut [u8], o: usize, v: u16) {
    m[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn rd32(m: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([m[o], m[o + 1], m[o + 2], m[o + 3]])
}

/// One buffer of a descriptor chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Buf {
    pub addr: u64,
    pub len: u32,
    /// Device-writable (a receive buffer); otherwise device-readable.
    pub write: bool,
}

/// Driver-side state of one split queue. Holds no pointers: every method
/// takes the queue memory, so the kernel adapter controls how it is mapped.
#[derive(Debug)]
pub struct SplitQueue {
    layout: QueueLayout,
    free_head: u16,
    free_count: u16,
    /// Our shadow of `avail.idx` (the device never writes it).
    avail_idx: u16,
    /// How far into the used ring we have consumed.
    last_used: u16,
    /// Chain length per head, to free the chain when it completes.
    chain_len: Vec<u16>,
}

impl SplitQueue {
    /// Initialises a fresh queue: zeroes `mem` and threads the free list
    /// through the descriptors' `next` fields. `mem` must hold
    /// `layout.total` bytes.
    pub fn new(layout: QueueLayout, mem: &mut [u8]) -> SplitQueue {
        assert!(mem.len() >= layout.total);
        mem[..layout.total].fill(0);
        let n = layout.size as usize;
        for i in 0..n {
            wr16(mem, layout.desc + i * DESC_SIZE + 14, ((i + 1) % n) as u16);
        }
        SplitQueue {
            layout,
            free_head: 0,
            free_count: layout.size,
            avail_idx: 0,
            last_used: 0,
            chain_len: alloc::vec![0; n],
        }
    }

    pub fn layout(&self) -> &QueueLayout {
        &self.layout
    }

    pub fn free_descriptors(&self) -> u16 {
        self.free_count
    }

    /// Publishes a chain of 1+ buffers to the device. Returns its head
    /// descriptor index (the token `pop_used` hands back), or `None` when
    /// `bufs` is empty or the free list cannot hold it. The caller must
    /// then ring the doorbell.
    pub fn push(&mut self, mem: &mut [u8], bufs: &[Buf]) -> Option<u16> {
        if bufs.is_empty() || bufs.len() > self.free_count as usize {
            return None;
        }
        let n = self.layout.size as usize;
        let head = self.free_head;
        let mut cur = head;
        for (i, b) in bufs.iter().enumerate() {
            let d = self.layout.desc + cur as usize * DESC_SIZE;
            let next = rd16(mem, d + 14);
            let last = i + 1 == bufs.len();
            let mut flags = if b.write { DESC_F_WRITE } else { 0 };
            if !last {
                flags |= DESC_F_NEXT;
            }
            mem[d..d + 8].copy_from_slice(&b.addr.to_le_bytes());
            mem[d + 8..d + 12].copy_from_slice(&b.len.to_le_bytes());
            wr16(mem, d + 12, flags);
            if last {
                self.free_head = next;
            } else {
                cur = next;
            }
        }
        self.free_count -= bufs.len() as u16;
        self.chain_len[head as usize] = bufs.len() as u16;
        let slot = self.layout.avail + 4 + 2 * (self.avail_idx as usize % n);
        wr16(mem, slot, head);
        // The device must see the descriptors and ring entry before the
        // index that makes them visible (§2.6.13.3).
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        wr16(mem, self.layout.avail + 2, self.avail_idx);
        Some(head)
    }

    /// Takes the next completed chain: `(head, bytes the device wrote)`.
    /// Frees its descriptors. `None` when the used ring has nothing new, or
    /// when the device reported a head that is not one of our chains (a
    /// buggy device; the entry is skipped rather than trusted).
    pub fn pop_used(&mut self, mem: &mut [u8]) -> Option<(u16, u32)> {
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        let used_idx = rd16(mem, self.layout.used + 2);
        if used_idx == self.last_used {
            return None;
        }
        let n = self.layout.size as usize;
        let e = self.layout.used + 4 + 8 * (self.last_used as usize % n);
        let id = rd32(mem, e);
        let len = rd32(mem, e + 4);
        self.last_used = self.last_used.wrapping_add(1);
        if id as usize >= n || self.chain_len[id as usize] == 0 {
            return self.pop_used(mem);
        }
        let head = id as u16;
        // Walk the chain to find its tail, then splice it onto the free list.
        let count = self.chain_len[head as usize];
        let mut tail = head;
        for _ in 1..count {
            tail = rd16(mem, self.layout.desc + tail as usize * DESC_SIZE + 14);
        }
        wr16(mem, self.layout.desc + tail as usize * DESC_SIZE + 14, self.free_head);
        self.free_head = head;
        self.free_count += count;
        self.chain_len[head as usize] = 0;
        Some((head, len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(size: u16) -> (SplitQueue, Vec<u8>) {
        let layout = QueueLayout::new(size).unwrap();
        let mut mem = alloc::vec![0xAAu8; layout.total];
        let sq = SplitQueue::new(layout, &mut mem);
        (sq, mem)
    }

    /// Plays the device: completes `head` having written `len` bytes.
    fn device_complete(sq: &SplitQueue, mem: &mut [u8], used_idx: &mut u16, head: u16, len: u32) {
        let n = sq.layout.size as usize;
        let e = sq.layout.used + 4 + 8 * (*used_idx as usize % n);
        mem[e..e + 4].copy_from_slice(&(head as u32).to_le_bytes());
        mem[e + 4..e + 8].copy_from_slice(&len.to_le_bytes());
        *used_idx = used_idx.wrapping_add(1);
        wr16(mem, sq.layout.used + 2, *used_idx);
    }

    #[test]
    fn layout_matches_the_spec_for_256_entries() {
        let l = QueueLayout::new(256).unwrap();
        assert_eq!(l.desc, 0);
        assert_eq!(l.avail, 4096);
        // 4096 + 4 + 512 + 2 = 4614, rounded up to 4 -> 4616
        assert_eq!(l.used, 4616);
        assert_eq!(l.total, 4616 + 4 + 2048 + 2);
    }

    #[test]
    fn layout_rejects_bad_sizes() {
        assert!(QueueLayout::new(0).is_none());
        assert!(QueueLayout::new(3).is_none());
        assert!(QueueLayout::new(65535).is_none());
        assert!(QueueLayout::new(1).is_some());
        assert!(QueueLayout::new(32768).is_some());
    }

    #[test]
    fn new_zeroes_memory_and_links_the_free_list() {
        let (sq, mem) = q(4);
        assert_eq!(sq.free_descriptors(), 4);
        assert_eq!(rd16(&mem, 14), 1);
        assert_eq!(rd16(&mem, 3 * 16 + 14), 0);
        assert_eq!(rd16(&mem, sq.layout.avail + 2), 0);
    }

    #[test]
    fn push_writes_a_chained_descriptor_pair() {
        let (mut sq, mut mem) = q(4);
        let head = sq
            .push(
                &mut mem,
                &[
                    Buf { addr: 0x1000, len: 12, write: false },
                    Buf { addr: 0x2000, len: 60, write: false },
                ],
            )
            .unwrap();
        assert_eq!(head, 0);
        assert_eq!(&mem[0..8], &0x1000u64.to_le_bytes());
        assert_eq!(rd32(&mem, 8), 12);
        assert_eq!(rd16(&mem, 12), DESC_F_NEXT);
        assert_eq!(rd16(&mem, 14), 1);
        assert_eq!(&mem[16..24], &0x2000u64.to_le_bytes());
        assert_eq!(rd16(&mem, 16 + 12), 0, "last buffer: no NEXT");
        assert_eq!(sq.free_descriptors(), 2);
        assert_eq!(rd16(&mem, sq.layout.avail + 2), 1, "avail.idx");
        assert_eq!(rd16(&mem, sq.layout.avail + 4), 0, "ring[0] = head");
    }

    #[test]
    fn write_flag_marks_receive_buffers() {
        let (mut sq, mut mem) = q(2);
        sq.push(&mut mem, &[Buf { addr: 0x3000, len: 1526, write: true }]).unwrap();
        assert_eq!(rd16(&mem, 12), DESC_F_WRITE);
    }

    #[test]
    fn push_fails_when_full_or_empty() {
        let (mut sq, mut mem) = q(2);
        assert!(sq.push(&mut mem, &[]).is_none());
        let b = Buf { addr: 0, len: 1, write: false };
        assert!(sq.push(&mut mem, &[b, b, b]).is_none(), "3 > 2 free");
        assert!(sq.push(&mut mem, &[b, b]).is_some());
        assert!(sq.push(&mut mem, &[b]).is_none(), "exhausted");
        assert_eq!(rd16(&mem, sq.layout.avail + 2), 1, "failed pushes publish nothing");
    }

    #[test]
    fn used_ring_round_trip_recycles_descriptors() {
        let (mut sq, mut mem) = q(4);
        let b = Buf { addr: 0x1000, len: 64, write: true };
        let h0 = sq.push(&mut mem, &[b]).unwrap();
        let h1 = sq.push(&mut mem, &[b, b]).unwrap();
        assert_eq!(sq.free_descriptors(), 1);
        assert!(sq.pop_used(&mut mem).is_none());

        let mut dev = 0;
        device_complete(&sq, &mut mem, &mut dev, h1, 100);
        device_complete(&sq, &mut mem, &mut dev, h0, 42);
        assert_eq!(sq.pop_used(&mut mem), Some((h1, 100)));
        assert_eq!(sq.free_descriptors(), 3);
        assert_eq!(sq.pop_used(&mut mem), Some((h0, 42)));
        assert_eq!(sq.free_descriptors(), 4);
        assert!(sq.pop_used(&mut mem).is_none());
        // All four are usable again, in a single chain.
        assert!(sq.push(&mut mem, &[b, b, b, b]).is_some());
    }

    #[test]
    fn indices_wrap_past_u16() {
        let (mut sq, mut mem) = q(2);
        let b = Buf { addr: 0, len: 1, write: false };
        let mut dev = 0u16;
        for i in 0..70_000u32 {
            let h = sq.push(&mut mem, &[b]).unwrap();
            device_complete(&sq, &mut mem, &mut dev, h, i);
            assert_eq!(sq.pop_used(&mut mem), Some((h, i)));
        }
        assert_eq!(sq.free_descriptors(), 2);
    }

    #[test]
    fn bogus_used_entries_are_skipped() {
        let (mut sq, mut mem) = q(4);
        let b = Buf { addr: 0, len: 1, write: false };
        let h = sq.push(&mut mem, &[b]).unwrap();
        let mut dev = 0;
        device_complete(&sq, &mut mem, &mut dev, 3, 7); // never pushed
        device_complete(&sq, &mut mem, &mut dev, 99, 7); // out of range
        device_complete(&sq, &mut mem, &mut dev, h, 5);
        assert_eq!(sq.pop_used(&mut mem), Some((h, 5)));
        assert_eq!(sq.free_descriptors(), 4);
    }

    #[test]
    fn negotiate_needs_version_1_and_mac() {
        let offered = F_VERSION_1 | NET_F_MAC | NET_F_STATUS | (1 << 0) | (1 << 15);
        assert_eq!(negotiate(offered), Some(F_VERSION_1 | NET_F_MAC | NET_F_STATUS));
        assert_eq!(negotiate(NET_F_MAC), None, "legacy device");
        assert_eq!(negotiate(F_VERSION_1), None, "no MAC");
        assert_eq!(negotiate(F_VERSION_1 | NET_F_MAC), Some(F_VERSION_1 | NET_F_MAC));
    }

    /// Builds config space with a status-register capability list and the
    /// given vendor capabilities `(cfg_type, bar, offset, length, extra)`.
    fn cfg_with(caps: &[(u8, u8, u32, u32, Option<u32>)]) -> [u8; 256] {
        let mut cfg = [0u8; 256];
        cfg[6] = 0x10; // status bit 4: capability list
        cfg[0x34] = 0x40;
        let mut at = 0x40usize;
        for (i, &(ty, bar, off, len, extra)) in caps.iter().enumerate() {
            let cap_len = if extra.is_some() { 20 } else { 16 };
            cfg[at] = CAP_ID_VENDOR;
            cfg[at + 1] = if i + 1 == caps.len() { 0 } else { (at + cap_len) as u8 };
            cfg[at + 2] = cap_len as u8;
            cfg[at + 3] = ty;
            cfg[at + 4] = bar;
            cfg[at + 8..at + 12].copy_from_slice(&off.to_le_bytes());
            cfg[at + 12..at + 16].copy_from_slice(&len.to_le_bytes());
            if let Some(x) = extra {
                cfg[at + 16..at + 20].copy_from_slice(&x.to_le_bytes());
            }
            at += cap_len;
        }
        cfg
    }

    #[test]
    fn parses_the_caps_qemu_exposes() {
        // Layout QEMU's virtio-net-pci uses: everything in BAR 4.
        let cfg = cfg_with(&[
            (CAP_COMMON_CFG, 4, 0x0000, 0x1000, None),
            (CAP_NOTIFY_CFG, 4, 0x3000, 0x1000, Some(4)),
            (CAP_ISR_CFG, 4, 0x1000, 0x1000, None),
            (CAP_DEVICE_CFG, 4, 0x2000, 0x1000, None),
        ]);
        let c = parse_caps(&cfg).unwrap();
        assert_eq!(c.common, CfgRegion { bar: 4, offset: 0, length: 0x1000 });
        assert_eq!(c.notify.offset, 0x3000);
        assert_eq!(c.notify_off_multiplier, 4);
        assert_eq!(c.isr.offset, 0x1000);
        assert_eq!(c.device.unwrap().offset, 0x2000);
    }

    #[test]
    fn missing_or_malformed_caps_are_rejected() {
        let no_isr = cfg_with(&[
            (CAP_COMMON_CFG, 4, 0, 0x1000, None),
            (CAP_NOTIFY_CFG, 4, 0x3000, 0x1000, Some(4)),
        ]);
        assert!(parse_caps(&no_isr).is_none());
        // A notify cap without the multiplier field is unusable.
        let short_notify = cfg_with(&[
            (CAP_COMMON_CFG, 4, 0, 0x1000, None),
            (CAP_NOTIFY_CFG, 4, 0x3000, 0x1000, None),
            (CAP_ISR_CFG, 4, 0x1000, 0x1000, None),
        ]);
        assert!(parse_caps(&short_notify).is_none());
        // BAR index out of range is skipped.
        let bad_bar = cfg_with(&[
            (CAP_COMMON_CFG, 7, 0, 0x1000, None),
            (CAP_NOTIFY_CFG, 4, 0x3000, 0x1000, Some(4)),
            (CAP_ISR_CFG, 4, 0x1000, 0x1000, None),
        ]);
        assert!(parse_caps(&bad_bar).is_none());
        assert!(parse_caps(&[0u8; 256]).is_none());
    }

    #[test]
    fn identifies_net_devices() {
        assert!(is_net(0x1AF4, 0x1041));
        assert!(is_net(0x1AF4, 0x1000));
        assert!(!is_net(0x1AF4, 0x1042));
        assert!(!is_net(0x8086, 0x1041));
    }
}
