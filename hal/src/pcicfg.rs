//! PCI configuration space beyond the header: the capability list, BARs
//! (address, type and size) and MSI.
//!
//! Phase 1 of `docs/gpu/gpu-plan.md`. Everything here is pure: the kernel
//! (`kernel/src/pci.rs`) reads the 256 bytes and the BAR sizing readbacks,
//! and performs the configuration writes this module returns as data. The
//! register layout is Linux's `include/uapi/linux/pci_regs.h` (v7.2.2, the
//! pinned reference in `~/src/gpu-ref/linux`); each constant cites its line.
//! Tested against the RTX 3050's real configuration space
//! (`hal/fixtures/ga106-config.txt`).

/// `PCI_COMMAND` (`pci_regs.h:40`).
pub const COMMAND: u8 = 0x04;
/// `PCI_COMMAND_IO` (`pci_regs.h:41`).
pub const COMMAND_IO: u16 = 0x1;
/// `PCI_COMMAND_MEMORY` (`pci_regs.h:42`).
pub const COMMAND_MEMORY: u16 = 0x2;
/// `PCI_COMMAND_MASTER` (`pci_regs.h:43`).
pub const COMMAND_MASTER: u16 = 0x4;
/// `PCI_COMMAND_INTX_DISABLE` (`pci_regs.h:51`).
pub const COMMAND_INTX_DISABLE: u16 = 0x400;
/// `PCI_STATUS` (`pci_regs.h:53`).
const STATUS: usize = 0x06;
/// `PCI_STATUS_CAP_LIST` (`pci_regs.h:56`).
const STATUS_CAP_LIST: u16 = 0x10;
/// `PCI_BASE_ADDRESS_0` (`pci_regs.h:96`); BARn is at `0x10 + 4n`.
pub const BAR0: u8 = 0x10;
/// `PCI_CAPABILITY_LIST` (`pci_regs.h:122`).
const CAPABILITY_LIST: usize = 0x34;
/// `PCI_CAP_LIST_NEXT` (`pci_regs.h:240`).
const CAP_LIST_NEXT: usize = 1;
/// `PCI_CAP_ID_MSI` (`pci_regs.h:223`).
pub const CAP_ID_MSI: u8 = 0x05;
/// `PCI_CAP_ID_MSIX` (`pci_regs.h:235`).
pub const CAP_ID_MSIX: u8 = 0x11;

/// A capability list in 256 bytes has at most (256 - 64) / 4 entries; a
/// longer walk is a loop in the list, not a list.
const MAX_CAPS: usize = 48;

fn u16_at(cfg: &[u8; 256], off: usize) -> u16 {
    u16::from_le_bytes([cfg[off], cfg[off + 1]])
}

fn u32_at(cfg: &[u8; 256], off: usize) -> u32 {
    u32::from_le_bytes([cfg[off], cfg[off + 1], cfg[off + 2], cfg[off + 3]])
}

/// The standard capability list: `(id, offset)` of each entry, in list
/// order. Empty when the status register says there is no list. Stops at a
/// pointer into the header (< 0x40, which includes the 0 terminator) and
/// after [`MAX_CAPS`] entries, so a malformed list cannot loop.
pub fn capabilities(cfg: &[u8; 256]) -> impl Iterator<Item = (u8, u8)> + '_ {
    let has_list = u16_at(cfg, STATUS) & STATUS_CAP_LIST != 0;
    // The two low bits of every pointer are reserved (PCI 3.0 §6.7).
    let mut next = if has_list { cfg[CAPABILITY_LIST] & !3 } else { 0 };
    let mut seen = 0;
    core::iter::from_fn(move || {
        if next < 0x40 || seen >= MAX_CAPS {
            return None;
        }
        seen += 1;
        let off = next;
        next = cfg[off as usize + CAP_LIST_NEXT] & !3;
        Some((cfg[off as usize], off))
    })
}

/// Offset of the first capability with this ID.
pub fn find_capability(cfg: &[u8; 256], id: u8) -> Option<u8> {
    capabilities(cfg).find(|&(cid, _)| cid == id).map(|(_, off)| off)
}

// ── BARs ─────────────────────────────────────────────────────────────────────

/// `PCI_BASE_ADDRESS_SPACE_IO` (`pci_regs.h:103`).
const BAR_SPACE_IO: u32 = 0x01;
/// `PCI_BASE_ADDRESS_MEM_TYPE_MASK` (`pci_regs.h:105`).
const BAR_MEM_TYPE_MASK: u32 = 0x06;
/// `PCI_BASE_ADDRESS_MEM_TYPE_64` (`pci_regs.h:108`).
const BAR_MEM_TYPE_64: u32 = 0x04;
/// `PCI_BASE_ADDRESS_MEM_PREFETCH` (`pci_regs.h:109`).
const BAR_MEM_PREFETCH: u32 = 0x08;
/// `PCI_BASE_ADDRESS_MEM_MASK` (`pci_regs.h:110`).
const BAR_MEM_MASK: u32 = !0x0f;
/// `PCI_BASE_ADDRESS_IO_MASK` (`pci_regs.h:111`).
const BAR_IO_MASK: u32 = !0x03;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarKind {
    Io,
    Mem32 { prefetchable: bool },
    Mem64 { prefetchable: bool },
}

/// One decoded BAR: where the firmware put it and how big it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bar {
    /// 0..=5, the register it starts at (a 64-bit BAR also uses `index+1`).
    pub index: u8,
    pub kind: BarKind,
    pub addr: u64,
    pub size: u64,
}

/// Whether BAR register `index` starts a 64-bit memory BAR, from its
/// current value — the caller then reads (and sizes) `index + 1` with it.
pub fn bar_is_64(raw: u32) -> bool {
    raw & BAR_SPACE_IO == 0 && raw & BAR_MEM_TYPE_MASK == BAR_MEM_TYPE_64
}

/// Decodes a BAR from its original value(s) and the value(s) read back
/// after writing all ones — the sizing protocol of Linux's
/// `__pci_read_base` (`drivers/pci/probe.c:201`). `raw_hi`/`sized_hi` are
/// the next register's, used only when `raw_lo` is a 64-bit memory BAR.
/// `None` for an unimplemented BAR (reads back zero).
pub fn decode_bar(index: u8, raw_lo: u32, raw_hi: u32, sized_lo: u32, sized_hi: u32) -> Option<Bar> {
    if raw_lo & BAR_SPACE_IO != 0 {
        // I/O BARs: only the low 16 bits are decoded on x86; the upper ones
        // may read back as zero, which `| 0xFFFF_0000` treats as set.
        let mask = (sized_lo & BAR_IO_MASK) | 0xFFFF_0000;
        if sized_lo & BAR_IO_MASK == 0 {
            return None;
        }
        return Some(Bar {
            index,
            kind: BarKind::Io,
            addr: (raw_lo & BAR_IO_MASK & 0xFFFF) as u64,
            size: (!mask).wrapping_add(1) as u64,
        });
    }
    let prefetchable = raw_lo & BAR_MEM_PREFETCH != 0;
    if bar_is_64(raw_lo) {
        let mask = ((sized_hi as u64) << 32) | (sized_lo & BAR_MEM_MASK) as u64;
        if mask == 0 {
            return None;
        }
        Some(Bar {
            index,
            kind: BarKind::Mem64 { prefetchable },
            addr: ((raw_hi as u64) << 32) | (raw_lo & BAR_MEM_MASK) as u64,
            size: (!mask).wrapping_add(1),
        })
    } else {
        let mask = sized_lo & BAR_MEM_MASK;
        if mask == 0 {
            return None;
        }
        Some(Bar {
            index,
            kind: BarKind::Mem32 { prefetchable },
            addr: (raw_lo & BAR_MEM_MASK) as u64,
            size: (!mask).wrapping_add(1) as u64,
        })
    }
}

// ── MSI ──────────────────────────────────────────────────────────────────────

/// `PCI_MSI_FLAGS` (`pci_regs.h:315`).
const MSI_FLAGS: u8 = 0x02;
/// `PCI_MSI_FLAGS_ENABLE` (`pci_regs.h:316`).
const MSI_FLAGS_ENABLE: u16 = 0x0001;
/// `PCI_MSI_FLAGS_QMASK` (`pci_regs.h:317`).
const MSI_FLAGS_QMASK: u16 = 0x000e;
/// `PCI_MSI_FLAGS_QSIZE` (`pci_regs.h:318`).
const MSI_FLAGS_QSIZE: u16 = 0x0070;
/// `PCI_MSI_FLAGS_64BIT` (`pci_regs.h:319`).
const MSI_FLAGS_64BIT: u16 = 0x0080;
/// `PCI_MSI_FLAGS_MASKBIT` (`pci_regs.h:320`).
const MSI_FLAGS_MASKBIT: u16 = 0x0100;
/// `PCI_MSI_ADDRESS_LO` (`pci_regs.h:322`).
const MSI_ADDRESS_LO: u8 = 0x04;
/// `PCI_MSI_ADDRESS_HI` (`pci_regs.h:323`).
const MSI_ADDRESS_HI: u8 = 0x08;
/// `PCI_MSI_DATA_32` (`pci_regs.h:324`).
const MSI_DATA_32: u8 = 0x08;
/// `PCI_MSI_MASK_32` (`pci_regs.h:325`).
const MSI_MASK_32: u8 = 0x0c;
/// `PCI_MSI_DATA_64` (`pci_regs.h:327`).
const MSI_DATA_64: u8 = 0x0c;
/// `PCI_MSI_MASK_64` (`pci_regs.h:328`).
const MSI_MASK_64: u8 = 0x10;

/// `PCI_MSIX_FLAGS` (`pci_regs.h:332`).
const MSIX_FLAGS: u8 = 2;
/// `PCI_MSIX_FLAGS_QSIZE` (`pci_regs.h:333`).
const MSIX_FLAGS_QSIZE: u16 = 0x07FF;
/// `PCI_MSIX_FLAGS_ENABLE` (`pci_regs.h:335`).
const MSIX_FLAGS_ENABLE: u16 = 0x8000;
/// `PCI_MSIX_TABLE` (`pci_regs.h:336`).
const MSIX_TABLE: u8 = 4;
/// `PCI_MSIX_TABLE_BIR` (`pci_regs.h:337`).
const MSIX_TABLE_BIR: u32 = 0x0000_0007;
/// `PCI_MSIX_TABLE_OFFSET` (`pci_regs.h:338`).
const MSIX_TABLE_OFFSET: u32 = 0xffff_fff8;

/// A function's MSI capability, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsiCap {
    /// Offset of the capability in configuration space.
    pub offset: u8,
    pub enabled: bool,
    pub is_64: bool,
    pub per_vector_mask: bool,
    /// Vectors the function can request (Multiple Message Capable, 1..=32).
    pub vectors_capable: u8,
    /// The message the function currently sends (whatever firmware or a
    /// previous driver left there).
    pub address: u64,
    pub data: u16,
}

impl MsiCap {
    pub fn decode(cfg: &[u8; 256]) -> Option<MsiCap> {
        let offset = find_capability(cfg, CAP_ID_MSI)?;
        let o = offset as usize;
        // The capability's longest form (64-bit + mask + pending) is 0x18
        // bytes; one that runs off the end of the space is malformed.
        if o + 0x18 > 256 {
            return None;
        }
        let flags = u16_at(cfg, o + MSI_FLAGS as usize);
        let is_64 = flags & MSI_FLAGS_64BIT != 0;
        let lo = u32_at(cfg, o + MSI_ADDRESS_LO as usize) as u64;
        let (address, data) = if is_64 {
            let hi = u32_at(cfg, o + MSI_ADDRESS_HI as usize) as u64;
            ((hi << 32) | lo, u16_at(cfg, o + MSI_DATA_64 as usize))
        } else {
            (lo, u16_at(cfg, o + MSI_DATA_32 as usize))
        };
        Some(MsiCap {
            offset,
            enabled: flags & MSI_FLAGS_ENABLE != 0,
            is_64,
            per_vector_mask: flags & MSI_FLAGS_MASKBIT != 0,
            vectors_capable: 1 << ((flags & MSI_FLAGS_QMASK) >> 1).min(5),
            address,
            data,
        })
    }

    /// The configuration writes that make the function send one message,
    /// `(address, data)`, in the order they must happen: disable, message,
    /// unmask vector 0 (if maskable), enable with a single vector. The
    /// current control word is needed so that bits this code does not own
    /// are written back unchanged.
    pub fn enable_writes(&self, flags_now: u16, address: u64, data: u16) -> ConfigWrites {
        let o = self.offset;
        let mut w = ConfigWrites::default();
        let off = flags_now & !(MSI_FLAGS_ENABLE | MSI_FLAGS_QSIZE);
        w.push(o + MSI_FLAGS, Width::W16, off as u32);
        w.push(o + MSI_ADDRESS_LO, Width::W32, address as u32);
        if self.is_64 {
            w.push(o + MSI_ADDRESS_HI, Width::W32, (address >> 32) as u32);
            w.push(o + MSI_DATA_64, Width::W16, data as u32);
        } else {
            w.push(o + MSI_DATA_32, Width::W16, data as u32);
        }
        if self.per_vector_mask {
            let mask = if self.is_64 { MSI_MASK_64 } else { MSI_MASK_32 };
            w.push(o + mask, Width::W32, 0);
        }
        // QSIZE = 0: one vector.
        w.push(o + MSI_FLAGS, Width::W16, (off | MSI_FLAGS_ENABLE) as u32);
        w
    }
}

/// A function's MSI-X capability, decoded (reported only: nothing here
/// programs MSI-X yet; the GA106 has none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsixCap {
    pub offset: u8,
    pub enabled: bool,
    pub table_size: u16,
    pub table_bar: u8,
    pub table_offset: u32,
}

impl MsixCap {
    pub fn decode(cfg: &[u8; 256]) -> Option<MsixCap> {
        let offset = find_capability(cfg, CAP_ID_MSIX)?;
        let o = offset as usize;
        if o + 12 > 256 {
            return None;
        }
        let flags = u16_at(cfg, o + MSIX_FLAGS as usize);
        let table = u32_at(cfg, o + MSIX_TABLE as usize);
        Some(MsixCap {
            offset,
            enabled: flags & MSIX_FLAGS_ENABLE != 0,
            table_size: (flags & MSIX_FLAGS_QSIZE) + 1,
            table_bar: (table & MSIX_TABLE_BIR) as u8,
            table_offset: table & MSIX_TABLE_OFFSET,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    W16,
    W32,
}

/// Configuration-space writes to perform in order ("decide, don't do").
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConfigWrites {
    items: [(u8, Width, u32); 8],
    len: usize,
}

impl ConfigWrites {
    fn push(&mut self, offset: u8, width: Width, value: u32) {
        self.items[self.len] = (offset, width, value);
        self.len += 1;
    }

    pub fn as_slice(&self) -> &[(u8, Width, u32)] {
        &self.items[..self.len]
    }
}

impl Default for Width {
    fn default() -> Self {
        Width::W32
    }
}

/// `X86_MSI_BASE_ADDRESS_LOW` (`arch/x86/include/asm/msi.h:52`), shifted
/// back into place: the LAPIC's MSI window.
const X86_MSI_BASE: u32 = 0xfee0_0000;

/// The x86 MSI message that delivers `vector` to the local APIC
/// `dest_apic_id`: physical destination, fixed delivery, edge — every
/// other field of `x86_msi_addr_lo`/`x86_msi_data`
/// (`arch/x86/include/asm/msi.h:14-49`) zero. `destid_0_7` is bits 19:12
/// of the address and `vector` bits 7:0 of the data. `None` for an APIC ID
/// that does not fit in 8 bits (that needs remapping or the extended
/// destination ID, neither of which this kernel does) or a vector below
/// 32 (exceptions).
pub fn x86_msi_message(dest_apic_id: u32, vector: u8) -> Option<(u64, u16)> {
    if dest_apic_id > 0xff || vector < 32 {
        return None;
    }
    Some(((X86_MSI_BASE | (dest_apic_id << 12)) as u64, vector as u16))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// The GA106's first 256 bytes, parsed from the `lspci -xxxx` rows.
    fn ga106() -> [u8; 256] {
        let mut cfg = [0u8; 256];
        let mut n = 0;
        for line in include_str!("../fixtures/ga106-config.txt").lines() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let (off, bytes) = line.split_once(": ").unwrap();
            let base = usize::from_str_radix(off, 16).unwrap();
            for (i, b) in bytes.split_whitespace().enumerate() {
                cfg[base + i] = u8::from_str_radix(b, 16).unwrap();
                n += 1;
            }
        }
        assert_eq!(n, 256);
        cfg
    }

    /// Same list `lspci -vvv` prints for 09:00.0 (below 0x100: the
    /// extended ones need ECAM).
    #[test]
    fn ga106_capability_list_matches_lspci() {
        let caps: Vec<(u8, u8)> = capabilities(&ga106()).collect();
        assert_eq!(caps, [(0x01, 0x60), (0x05, 0x68), (0x10, 0x78), (0x09, 0xb4)]);
    }

    /// lspci: `[68] MSI: Enable+ Count=1/1 Maskable- 64bit+`,
    /// `Address: 00000000fee00000  Data: 0000`; no MSI-X.
    #[test]
    fn ga106_msi_matches_lspci() {
        let cfg = ga106();
        let msi = MsiCap::decode(&cfg).unwrap();
        assert_eq!(
            msi,
            MsiCap {
                offset: 0x68,
                enabled: true,
                is_64: true,
                per_vector_mask: false,
                vectors_capable: 1,
                address: 0xfee0_0000,
                data: 0,
            }
        );
        assert_eq!(MsixCap::decode(&cfg), None);
    }

    #[test]
    fn enable_writes_64bit_unmaskable() {
        let msi = MsiCap::decode(&ga106()).unwrap();
        let w = msi.enable_writes(0x0081, 0xfee0_1000, 0x51);
        assert_eq!(
            w.as_slice(),
            [
                (0x6a, Width::W16, 0x0080),
                (0x6c, Width::W32, 0xfee0_1000),
                (0x70, Width::W32, 0),
                (0x74, Width::W16, 0x51),
                (0x6a, Width::W16, 0x0081),
            ]
        );
    }

    /// A 32-bit maskable capability that asks for 4 vectors: data at +8,
    /// mask at +0xc, and QSIZE forced back to one vector.
    #[test]
    fn enable_writes_32bit_maskable() {
        let mut cfg = [0u8; 256];
        cfg[STATUS] = STATUS_CAP_LIST as u8;
        cfg[CAPABILITY_LIST] = 0x50;
        cfg[0x50] = CAP_ID_MSI;
        cfg[0x52] = 0x24; // QMASK=2 (4 vectors), QSIZE=2
        cfg[0x53] = 0x01; // MASKBIT
        let msi = MsiCap::decode(&cfg).unwrap();
        assert!(!msi.is_64 && msi.per_vector_mask);
        assert_eq!(msi.vectors_capable, 4);
        let w = msi.enable_writes(0x0124, 0xfee0_0000, 0x50);
        assert_eq!(
            w.as_slice(),
            [
                (0x52, Width::W16, 0x0104),
                (0x54, Width::W32, 0xfee0_0000),
                (0x58, Width::W16, 0x50),
                (0x5c, Width::W32, 0),
                (0x52, Width::W16, 0x0105),
            ]
        );
    }

    #[test]
    fn capability_walk_is_bounded() {
        // No list bit: nothing, whatever the pointer says.
        let mut cfg = [0u8; 256];
        cfg[CAPABILITY_LIST] = 0x40;
        assert_eq!(capabilities(&cfg).count(), 0);
        // A capability pointing at itself.
        cfg[STATUS] = STATUS_CAP_LIST as u8;
        cfg[0x40] = 0x09;
        cfg[0x41] = 0x40;
        assert_eq!(capabilities(&cfg).count(), MAX_CAPS);
        // A pointer into the header ends the list.
        cfg[0x41] = 0x3c;
        assert_eq!(capabilities(&cfg).count(), 1);
        // An MSI capability whose body would run off the end.
        cfg[CAPABILITY_LIST] = 0xf0;
        cfg[0xf0] = CAP_ID_MSI;
        assert_eq!(MsiCap::decode(&cfg), None);
    }

    /// Sizes lspci reports for 09:00.0 — `Region 0: Memory at f5000000
    /// (32-bit, non-prefetchable) [size=16M]`, `Region 1: Memory at
    /// 7c00000000 (64-bit, prefetchable) [size=8G]`, `Region 3: ... at
    /// 7e00000000 ... [size=32M]`, `Region 5: I/O ports at e000
    /// [size=128]` — from the raw BARs in the fixture and the readbacks
    /// those sizes imply.
    #[test]
    fn ga106_bars() {
        let cfg = ga106();
        let raw = |i: usize| u32_at(&cfg, 0x10 + 4 * i);
        assert!(!bar_is_64(raw(0)) && bar_is_64(raw(1)) && bar_is_64(raw(3)));
        assert_eq!(
            decode_bar(0, raw(0), 0, 0xff00_0000, 0),
            Some(Bar { index: 0, kind: BarKind::Mem32 { prefetchable: false }, addr: 0xf500_0000, size: 16 << 20 })
        );
        assert_eq!(
            decode_bar(1, raw(1), raw(2), 0x0000_000c, 0xffff_fffe),
            Some(Bar { index: 1, kind: BarKind::Mem64 { prefetchable: true }, addr: 0x7c_0000_0000, size: 8 << 30 })
        );
        assert_eq!(
            decode_bar(3, raw(3), raw(4), 0xfe00_000c, 0xffff_ffff),
            Some(Bar { index: 3, kind: BarKind::Mem64 { prefetchable: true }, addr: 0x7e_0000_0000, size: 32 << 20 })
        );
        assert_eq!(
            decode_bar(5, raw(5), 0, 0x0000_ff81, 0),
            Some(Bar { index: 5, kind: BarKind::Io, addr: 0xe000, size: 128 })
        );
        // Unimplemented BAR.
        assert_eq!(decode_bar(2, 0, 0, 0, 0), None);
    }

    #[test]
    fn msi_message() {
        assert_eq!(x86_msi_message(0, 0x50), Some((0xfee0_0000, 0x50)));
        assert_eq!(x86_msi_message(3, 0x5f), Some((0xfee0_3000, 0x5f)));
        assert_eq!(x86_msi_message(0x100, 0x50), None);
        assert_eq!(x86_msi_message(0, 14), None);
    }
}
