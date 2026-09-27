//! The VBIOS: read from the PROM through BAR0, split into images, BIT table.
//!
//! Nouveau reads the VBIOS from several sources and scores them
//! (`nvkm/subdev/bios/shadow.c:173`); on the target board it uses the PROM
//! (`trace-nogsp` dmesg: "bios: using image from PROM"), so that is the only
//! source here. Paths are relative to `drivers/gpu/drm/nouveau/` in the
//! pinned Linux v7.2.2.

use alloc::vec::Vec;

use crate::Mmio;

/// The PROM window in BAR0: 1 MiB at `0x300000` (`nvkm/subdev/bios/shadowrom.c:48,51`).
pub const PROM: u32 = 0x30_0000;
pub const PROM_WINDOW: u32 = 0x10_0000;

/// PCI configuration space mirrored in BAR0 at `0x088000`
/// (`nvkm/subdev/pci/gp100.c:34`); offset `0x50` bit 0 is the "ROM shadow"
/// that must be off to read the PROM (`nvkm/subdev/pci/base.c:66-74`,
/// called from `shadowrom.c:84`).
pub const PCI_ROM_SHADOW: u32 = 0x08_8050;

/// `NV_PBUS_IFR_FMT_FIXED0_SIGNATURE_VALUE` (`shadowrom.c:27`): an IFR header
/// in front of the PCI ROM. Not on the target board (its first PROM word is
/// `0xeb7faa55`, `trace-nogsp`), so not supported.
const IFR_SIGNATURE: u32 = 0x4947_564e;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiosError {
    /// The PROM starts with an IFR header (see [`IFR_SIGNATURE`]).
    IfrHeader,
    /// The first image has no valid ROM/PCIR header.
    NoImage,
}

/// One image of the ROM chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Image {
    pub base: u32,
    /// PCIR code type (`pcir.c:65`): 0x00 x86 (the one with BIT/DCB), 0x03
    /// EFI, 0xe0 NVIDIA firmware (FWSEC & co.), 0x70 NBSI.
    pub kind: u8,
    pub size: u32,
    pub last: bool,
}

fn rd16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(at..at + 2)?.try_into().ok()?))
}
fn rd32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

/// `nvbios_imagen` (`image.c:30-66`) for the image at `base`: `None` if the
/// bytes there are not a valid image header (or are not all in `data`).
pub fn parse_image(data: &[u8], base: u32) -> Option<Image> {
    let b = base as usize;
    // ROM signatures accepted (`image.c:40-42`).
    match rd16(data, b)? {
        0xaa55 | 0xbb77 | 0x4e56 => {}
        _ => return None,
    }
    // PCIR pointer at +0x18 and its signatures (`pcir.c:30-38`).
    let pcir = b + rd16(data, b + 0x18)? as usize;
    if pcir == b {
        return None;
    }
    match rd32(data, pcir)? {
        0x5249_4350 | 0x5349_4752 | 0x5344_504e => {}
        _ => return None,
    }
    let hdr = rd16(data, pcir + 0x0a)? as usize; // `pcir.c:36`
    let mut size = rd16(data, pcir + 0x10)? as u32 * 512; // `pcir.c:63`
    let kind = *data.get(pcir + 0x14)?; // `pcir.c:65`
    let mut last = data.get(pcir + 0x15)? & 0x80 != 0; // `pcir.c:66`
    if kind != 0x70 {
        // An NPDE after the PCIR, 16-byte aligned, overrides size and last
        // (`npde.c:32-36,59-60`, `image.c:54-60`).
        let npde = (pcir + hdr + 0x0f) & !0x0f;
        if rd32(data, npde) == Some(0x4544_504e) {
            size = rd16(data, npde + 0x08)? as u32 * 512;
            last = data.get(npde + 0x0a)? & 0x80 != 0;
        }
    } else {
        last = true; // `image.c:62`
    }
    if size == 0 {
        return None; // would loop forever on the same base
    }
    Some(Image { base, kind, size, last })
}

/// The image chain over `data`, stopping at the first invalid header or at
/// the end of the data (`image.c:69-82`, `shadow.c:52-106`).
pub fn images(data: &[u8]) -> Vec<Image> {
    let mut out = Vec::new();
    let mut base = 0u32;
    while let Some(img) = parse_image(data, base) {
        if (img.base + img.size) as usize > data.len() {
            break;
        }
        out.push(img);
        if img.last {
            break;
        }
        base += img.size;
    }
    out
}

/// Reads the ROM chain from the PROM the way nouveau's PROM source does
/// (`shadowrom.c:36-53`, fetched in `shadow.c:39-49`: each image's first
/// 4 KiB to parse the header, then the rest). Stops at the first invalid
/// header after the first image and returns the data up to the end of the
/// last valid one.
///
/// The ROM shadow bit is cleared only if set and then restored to what it
/// was. Nouveau always writes it and leaves it **set** afterwards
/// (`shadowrom.c:59`); here the GPU is left as found (on the target board
/// the bit is already clear, `trace-nogsp`: `0x088050` reads 0), so on
/// this board this reads without writing anything.
pub fn read_prom(m: &impl Mmio) -> Result<Vec<u8>, BiosError> {
    let shadow = m.rd32(PCI_ROM_SHADOW);
    if shadow & 1 != 0 {
        m.wr32(PCI_ROM_SHADOW, shadow & !1);
    }
    let r = read_chain(m);
    if shadow & 1 != 0 {
        m.wr32(PCI_ROM_SHADOW, shadow);
    }
    r
}

fn read_chain(m: &impl Mmio) -> Result<Vec<u8>, BiosError> {
    if m.rd32(PROM) == IFR_SIGNATURE {
        return Err(BiosError::IfrHeader);
    }
    let mut data: Vec<u8> = Vec::new();
    let fetch = |data: &mut Vec<u8>, upto: u32| {
        let limit = ((upto + 3) & !3).min(PROM_WINDOW);
        while (data.len() as u32) < limit {
            let w = m.rd32(PROM + data.len() as u32);
            data.extend_from_slice(&w.to_le_bytes());
        }
    };
    let mut base = 0u32;
    let mut end = 0u32;
    loop {
        fetch(&mut data, base + 0x1000);
        let Some(img) = parse_image(&data, base) else { break };
        if img.base + img.size > PROM_WINDOW {
            break;
        }
        fetch(&mut data, img.base + img.size);
        end = img.base + img.size;
        if img.last {
            break;
        }
        base = end;
    }
    if end == 0 {
        return Err(BiosError::NoImage);
    }
    data.truncate(end as usize);
    Ok(data)
}

/// The parsed VBIOS: the bytes plus what `nvkm_bios_new` derives from them
/// (`base.c:151-207`).
pub struct Bios {
    pub data: Vec<u8>,
    pub images: Vec<Image>,
    image0_size: u32,
    /// Base of the first 0xe0 image after image 0: pointers past image 0
    /// point into it (`base.c:165-175`, applied in `base.c:31-44`).
    imaged_addr: u32,
    /// Offset of the `\xff\xb8BIT` signature (`base.c:185-189`), 0 if none.
    pub bit_offset: u32,
}

/// A BIT table entry (`bit.c:28-50`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitEntry {
    pub id: u8,
    pub version: u8,
    pub length: u16,
    pub offset: u16,
}

impl Bios {
    pub fn new(data: Vec<u8>) -> Result<Bios, BiosError> {
        let images = images(&data);
        let Some(first) = images.first() else { return Err(BiosError::NoImage) };
        let image0_size = first.size;
        let imaged_addr = images.iter().skip(1).find(|i| i.kind == 0xe0).map_or(0, |i| i.base);
        let bit_offset = find(&data, b"\xff\xb8BIT").unwrap_or(0) as u32;
        Ok(Bios { data, images, image0_size, imaged_addr, bit_offset })
    }

    /// `nvbios_addr` (`base.c:31-44`): out of range reads as 0, like
    /// nouveau's `nvbios_rd*`.
    fn addr(&self, addr: u32, size: u32) -> Option<usize> {
        let mut a = addr;
        if a >= self.image0_size && self.imaged_addr != 0 {
            a = a - self.image0_size + self.imaged_addr;
        }
        ((a as u64 + size as u64) <= self.data.len() as u64).then_some(a as usize)
    }
    pub fn rd08(&self, addr: u32) -> u8 {
        self.addr(addr, 1).map_or(0, |a| self.data[a])
    }
    pub fn rd16(&self, addr: u32) -> u16 {
        self.addr(addr, 2).and_then(|a| rd16(&self.data, a)).unwrap_or(0)
    }
    pub fn rd32(&self, addr: u32) -> u32 {
        self.addr(addr, 4).and_then(|a| rd32(&self.data, a)).unwrap_or(0)
    }

    /// `bit_entry` (`bit.c:28-50`).
    pub fn bit_entry(&self, id: u8) -> Option<BitEntry> {
        if self.bit_offset == 0 {
            return None;
        }
        let entries = self.rd08(self.bit_offset + 10);
        let stride = self.rd08(self.bit_offset + 9) as u32;
        let mut entry = self.bit_offset + 12;
        for _ in 0..entries {
            if self.rd08(entry) == id {
                return Some(BitEntry {
                    id,
                    version: self.rd08(entry + 1),
                    length: self.rd16(entry + 2),
                    offset: self.rd16(entry + 4),
                });
            }
            entry += stride;
        }
        None
    }

    /// The version nouveau prints (`base.c:192-198`): BIT 'i' bytes 3, 2,
    /// 1, 0, 4, e.g. `94.06.37.00.40`.
    pub fn version(&self) -> Option<[u8; 5]> {
        let i = self.bit_entry(b'i').filter(|e| e.length >= 4)?;
        let o = i.offset as u32;
        Some([self.rd08(o + 3), self.rd08(o + 2), self.rd08(o + 1), self.rd08(o), self.rd08(o + 4)])
    }
}

/// `nvbios_findstr` (`base.c:91-104`), except "not found" is `None` rather
/// than 0.
fn find(data: &[u8], s: &[u8]) -> Option<usize> {
    data.windows(s.len()).position(|w| w == s)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::mmio::testing::TableMmio;
    use core::cell::{Cell, RefCell};

    /// The VBIOS dumped in phase 0 (`static/vbios-rom.bin`, 148 992 bytes:
    /// images 0 and 1, what sysfs exposes). Not in git (D3): `None`, and
    /// the test says so, when `$GPU_ORACLE` (default `~/constanos-gpu-oracle`)
    /// is not there.
    pub fn oracle_vbios() -> Option<Vec<u8>> {
        let dir = std::env::var("GPU_ORACLE")
            .unwrap_or_else(|_| std::format!("{}/constanos-gpu-oracle", std::env::var("HOME").unwrap_or_default()));
        match std::fs::read(std::format!("{dir}/static/vbios-rom.bin")) {
            Ok(d) => Some(d),
            Err(e) => {
                std::eprintln!("SKIP: no VBIOS fixture in {dir} ({e}); see docs/gpu/gpu-plan.md D3");
                None
            }
        }
    }

    #[test]
    fn oracle_images_and_version() {
        let Some(rom) = oracle_vbios() else { return };
        let bios = Bios::new(rom).unwrap();
        // nouveau, trace-nogsp dmesg: "00000000: type 00, 65024 bytes",
        // "0000fe00: type 03, 83968 bytes" (the dump ends there; the PROM
        // has two more 0xe0 images).
        assert_eq!(bios.images.len(), 2);
        assert_eq!((bios.images[0].base, bios.images[0].kind, bios.images[0].size), (0, 0x00, 65024));
        assert_eq!((bios.images[1].base, bios.images[1].kind, bios.images[1].size), (0xfe00, 0x03, 83968));
        assert!(!bios.images[1].last, "the NPDE says more images follow");
        assert_ne!(bios.bit_offset, 0);
        // "bios: version 94.06.37.00.40"
        assert_eq!(bios.version(), Some([0x94, 0x06, 0x37, 0x00, 0x40]));
    }

    /// A PROM behind BAR0: `rom` at `PROM`, `0xffffffff` past it (an
    /// erased flash), and the shadow register.
    struct PromMmio {
        rom: Vec<u8>,
        shadow: Cell<u32>,
        max_read: Cell<u32>,
        writes: RefCell<Vec<(u32, u32)>>,
    }
    impl Mmio for PromMmio {
        fn rd32(&self, o: u32) -> u32 {
            if o == PCI_ROM_SHADOW {
                return self.shadow.get();
            }
            assert!((PROM..PROM + PROM_WINDOW).contains(&o), "read outside the PROM: {o:#x}");
            let i = (o - PROM) as usize;
            self.max_read.set(self.max_read.get().max(o - PROM));
            rd32(&self.rom, i).unwrap_or(0xffff_ffff)
        }
        fn wr32(&self, o: u32, v: u32) {
            if o == PCI_ROM_SHADOW {
                self.shadow.set(v);
            }
            self.writes.borrow_mut().push((o, v));
        }
        fn udelay(&self, _: u32) {}
    }

    #[test]
    fn prom_read_matches_the_dump_and_writes_nothing() {
        let Some(rom) = oracle_vbios() else { return };
        let m = PromMmio { rom: rom.clone(), shadow: Cell::new(0), max_read: Cell::new(0), writes: RefCell::new(Vec::new()) };
        let data = read_prom(&m).unwrap();
        assert_eq!(data, rom, "both images, byte for byte, and nothing past them");
        assert!(m.writes.borrow().is_empty(), "shadow bit already clear: no write");
        // Past the dump it reads one 4 KiB header window and stops.
        assert_eq!(m.max_read.get(), rom.len() as u32 + 0x1000 - 4);
    }

    #[test]
    fn prom_shadow_bit_is_cleared_and_restored() {
        let Some(rom) = oracle_vbios() else { return };
        let m = PromMmio { rom, shadow: Cell::new(0x0000_0101), max_read: Cell::new(0), writes: RefCell::new(Vec::new()) };
        read_prom(&m).unwrap();
        assert_eq!(*m.writes.borrow(), [(PCI_ROM_SHADOW, 0x100), (PCI_ROM_SHADOW, 0x101)]);
    }

    #[test]
    fn prom_guards() {
        // IFR header: refused before reading anything else.
        let ifr = TableMmio::new(&[(PCI_ROM_SHADOW, 0), (PROM, IFR_SIGNATURE)]);
        assert_eq!(read_prom(&ifr), Err(BiosError::IfrHeader));
        // Blank flash.
        let m = PromMmio { rom: Vec::new(), shadow: Cell::new(0), max_read: Cell::new(0), writes: RefCell::new(Vec::new()) };
        assert_eq!(read_prom(&m), Err(BiosError::NoImage));
    }

    /// A one-image ROM with a PCIR; `size_blocks` 512-byte blocks.
    fn tiny_rom(size_blocks: u16, last: bool) -> Vec<u8> {
        let mut r = alloc::vec![0u8; 0x400];
        r[0..2].copy_from_slice(&0xaa55u16.to_le_bytes());
        r[0x18..0x1a].copy_from_slice(&0x40u16.to_le_bytes());
        r[0x40..0x44].copy_from_slice(b"PCIR");
        r[0x4a..0x4c].copy_from_slice(&0x18u16.to_le_bytes());
        r[0x50..0x52].copy_from_slice(&size_blocks.to_le_bytes());
        r[0x55] = if last { 0x80 } else { 0 };
        r
    }

    #[test]
    fn malformed_images_terminate() {
        // Size 0 would make the walk loop on the same base.
        assert_eq!(parse_image(&tiny_rom(0, false), 0), None);
        // Size larger than the data: no image.
        assert!(images(&tiny_rom(4, true)).is_empty());
        // Valid, last.
        assert_eq!(images(&tiny_rom(2, true)), [Image { base: 0, kind: 0, size: 1024, last: true }]);
        // Not last, but nothing valid follows: one image.
        let mut two = tiny_rom(1, false);
        two.truncate(512);
        two.extend_from_slice(&[0xff; 512]);
        assert_eq!(images(&two).len(), 1);
        // PCIR pointer out of range, truncated data: no panic.
        let mut bad = tiny_rom(2, true);
        bad[0x18..0x1a].copy_from_slice(&0xfff0u16.to_le_bytes());
        assert!(images(&bad).is_empty());
        assert!(images(&[0x55, 0xaa]).is_empty());
        // An NPDE overrides the PCIR's size and last flag.
        let mut npde = tiny_rom(1, true);
        npde[0x60..0x64].copy_from_slice(b"NPDE");
        npde[0x68..0x6a].copy_from_slice(&2u16.to_le_bytes());
        npde[0x6a] = 0x80;
        assert_eq!(images(&npde), [Image { base: 0, kind: 0, size: 1024, last: true }]);
        // A Bios over garbage reads zeros, never panics.
        assert!(Bios::new(alloc::vec![0; 16]).is_err());
    }
}
