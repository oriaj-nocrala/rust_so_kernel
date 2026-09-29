//! The memory the GSP-RM boot needs (phase 4d): the VRAM layout of the protected
//! region (WPR2), the `GspFwWprMeta` the booter reads, the radix3 page table
//! of the firmware, the LibOS init arguments with the three log buffers, the RM
//! arguments and the shared memory of the two message queues. All pure: the
//! functions take the bus addresses of buffers the kernel allocated and return
//! the bytes to put in them.
//!
//! ABI layouts are those of `open-gpu-kernel-modules` 570.144; the offsets
//! asserted in the tests are from a C compiler (`nvgpu/gen/abi.c`, the plan's
//! D2: generated from the real headers, not written by memory). Paths are
//! relative to `drivers/gpu/drm/nouveau/nvkm/subdev/gsp/` in Linux v7.2.2.

use alloc::vec;
use alloc::vec::Vec;

/// `GSP_PAGE_SIZE` (`rm/r535/nvrm/gsp.h`): the GSP only understands 4 KiB pages.
pub const PAGE: usize = 4096;

fn align_up(x: u64, a: u64) -> u64 {
    (x + a - 1) & !(a - 1)
}
fn align_down(x: u64, a: u64) -> u64 {
    x & !(a - 1)
}
fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

// ---- GspFwWprMeta -------------------------------------------------------

pub const WPR_META_MAGIC: u64 = 0xdc3a_ae21_371a_60b3;
pub const WPR_META_REVISION: u64 = 1;
/// `sizeof(GspFwWprMeta)`: "exactly 256 bytes" (`gsp_fw_wpr_meta.h`).
pub const WPR_META_SIZE: usize = 256;

/// The fields `tu102_gsp_wpr_meta_init` sets (`tu102.c:212-259`); the rest
/// (boot count, partition RPC, `verified`, ...) stay 0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WprMeta {
    pub sysmem_addr_of_radix3_elf: u64,
    pub size_of_radix3_elf: u64,
    pub sysmem_addr_of_bootloader: u64,
    pub size_of_bootloader: u64,
    pub bootloader_code_offset: u64,
    pub bootloader_data_offset: u64,
    pub bootloader_manifest_offset: u64,
    pub sysmem_addr_of_signature: u64,
    pub size_of_signature: u64,
    pub gsp_fw_rsvd_start: u64,
    pub non_wpr_heap_offset: u64,
    pub non_wpr_heap_size: u64,
    pub gsp_fw_wpr_start: u64,
    pub gsp_fw_heap_offset: u64,
    pub gsp_fw_heap_size: u64,
    pub gsp_fw_offset: u64,
    pub boot_bin_offset: u64,
    pub frts_offset: u64,
    pub frts_size: u64,
    pub gsp_fw_wpr_end: u64,
    pub fb_size: u64,
    pub vga_workspace_offset: u64,
    pub vga_workspace_size: u64,
}

impl WprMeta {
    /// The 256 bytes at the offsets of the C struct.
    pub fn to_bytes(&self) -> [u8; WPR_META_SIZE] {
        let mut b = [0u8; WPR_META_SIZE];
        put64(&mut b, 0, WPR_META_MAGIC);
        put64(&mut b, 8, WPR_META_REVISION);
        let fields = [
            self.sysmem_addr_of_radix3_elf,
            self.size_of_radix3_elf,
            self.sysmem_addr_of_bootloader,
            self.size_of_bootloader,
            self.bootloader_code_offset,
            self.bootloader_data_offset,
            self.bootloader_manifest_offset,
            self.sysmem_addr_of_signature,
            self.size_of_signature,
            self.gsp_fw_rsvd_start,
            self.non_wpr_heap_offset,
            self.non_wpr_heap_size,
            self.gsp_fw_wpr_start,
            self.gsp_fw_heap_offset,
            self.gsp_fw_heap_size,
            self.gsp_fw_offset,
            self.boot_bin_offset,
            self.frts_offset,
            self.frts_size,
            self.gsp_fw_wpr_end,
            self.fb_size,
            self.vga_workspace_offset,
            self.vga_workspace_size,
        ];
        for (i, v) in fields.into_iter().enumerate() {
            put64(&mut b, 16 + i * 8, v);
        }
        b
    }
}

// ---- VRAM layout --------------------------------------------------------

/// `GSP_FW_HEAP_PARAM_*` of the GA10x with the r570 firmware
/// (`rm/r570/rm.c:r570_wpr_libos3`; `gsp_fw_heap.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WprParams {
    pub os_carveout_size: u64,
    pub base_size: u64,
    /// `heap_size_min`: nouveau stores the header's value in *megabytes*
    /// (170) and compares it with a size in bytes (`max(heap_size, min)`,
    /// `tu102.c:272`), so it never applies. Kept as nouveau has it: that is
    /// what the booter accepted in `trace-gsp`.
    pub heap_size_min: u64,
}

pub const R570_LIBOS3: WprParams = WprParams {
    os_carveout_size: 22 << 20,                    // GSP_FW_HEAP_PARAM_OS_SIZE_LIBOS3_BAREMETAL
    base_size: 8 << 20,                            // GSP_FW_HEAP_PARAM_BASE_RM_SIZE_TU10X
    heap_size_min: 88 + 12 + 70,                   // GSP_FW_HEAP_SIZE_OVERRIDE_LIBOS3_BAREMETAL_MIN_MB
};

/// `GSP_FW_HEAP_PARAM_SIZE_PER_GB_FB` and `..._CLIENT_ALLOC_SIZE`.
const HEAP_PER_GB_FB: u64 = 96 << 10;
const HEAP_CLIENT_ALLOC: u64 = (48 << 10) * 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub addr: u64,
    pub size: u64,
}

/// `tu102_gsp_oneinit`'s VRAM plan (`tu102.c:336-397`), top of VRAM down:
/// VGA workspace, FRTS, the bootloader, the firmware ELF, the WPR heap, the
/// WPR meta, and a 1 MiB heap outside WPR2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FbLayout {
    pub fb_size: u64,
    /// `gsp->fb.bios`: the VGA workspace, at the top.
    pub bios: Region,
    pub frts: Region,
    pub boot: Region,
    pub elf: Region,
    pub heap: Region,
    /// WPR2: from the meta up to the end of FRTS.
    pub wpr2: Region,
    /// `gsp->fb.heap`: 1 MiB just below WPR2, outside it.
    pub nonwpr_heap: Region,
}

/// `tu102_gsp_wpr_heap_size` (`tu102.c:261-273`).
pub fn wpr_heap_size(fb_size: u64, p: &WprParams) -> u64 {
    let fb_gb = fb_size.div_ceil(1 << 30);
    let heap = p.os_carveout_size
        + p.base_size
        + align_up(HEAP_PER_GB_FB * fb_gb, 1 << 20)
        + align_up(HEAP_CLIENT_ALLOC, 1 << 20);
    heap.max(p.heap_size_min)
}

/// The layout for a GA10x (no GA100 MMU lock). `vga_workspace` is
/// [`crate::fwsec::vga_workspace`]; `boot_size` the bootloader image size,
/// `fw_len` the `.fwimage` size.
pub fn fb_layout(fb_size: u64, vga_workspace: u64, boot_size: u64, fw_len: u64, p: &WprParams) -> FbLayout {
    let bios = Region { addr: vga_workspace, size: fb_size - vga_workspace };
    let frts_size = 0x10_0000;
    let frts = Region { addr: align_down(bios.addr, 0x2_0000) - frts_size, size: frts_size };
    let boot_addr = align_down(frts.addr - boot_size, 0x1000);
    let elf_addr = align_down(boot_addr - fw_len, 0x1_0000);
    let mut heap_size = wpr_heap_size(fb_size, p);
    let heap_addr = align_down(elf_addr - heap_size, 0x10_0000);
    heap_size = align_down(elf_addr - heap_addr, 0x10_0000);
    let wpr2_addr = align_down(heap_addr - WPR_META_SIZE as u64, 0x10_0000);
    let nonwpr = Region { addr: wpr2_addr - 0x10_0000, size: 0x10_0000 };
    FbLayout {
        fb_size,
        bios,
        frts,
        boot: Region { addr: boot_addr, size: boot_size },
        elf: Region { addr: elf_addr, size: fw_len },
        heap: Region { addr: heap_addr, size: heap_size },
        wpr2: Region { addr: wpr2_addr, size: frts.addr + frts.size - wpr2_addr },
        nonwpr_heap: nonwpr,
    }
}

/// Bus addresses and sizes of the host buffers the meta points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostBufs {
    /// Level 0 of the firmware's radix3 table.
    pub radix3_lvl0: u64,
    /// The bootloader image (`gsp->boot.fw`).
    pub bootloader: u64,
    /// The signature copy (`gsp->sig`) and its size (256-aligned).
    pub signature: u64,
    pub signature_size: u64,
    /// `RM_RISCV_UCODE_DESC`'s `monitorCodeOffset`, `monitorDataOffset`,
    /// `manifestOffset` (`rm/r535/gsp.c:1834-1836`).
    pub code_offset: u64,
    pub data_offset: u64,
    pub manifest_offset: u64,
}

/// `tu102_gsp_wpr_meta_init` (`tu102.c:212-259`).
pub fn wpr_meta(l: &FbLayout, h: &HostBufs) -> WprMeta {
    WprMeta {
        sysmem_addr_of_radix3_elf: h.radix3_lvl0,
        size_of_radix3_elf: l.elf.size,
        sysmem_addr_of_bootloader: h.bootloader,
        size_of_bootloader: l.boot.size,
        bootloader_code_offset: h.code_offset,
        bootloader_data_offset: h.data_offset,
        bootloader_manifest_offset: h.manifest_offset,
        sysmem_addr_of_signature: h.signature,
        size_of_signature: h.signature_size,
        gsp_fw_rsvd_start: l.nonwpr_heap.addr,
        non_wpr_heap_offset: l.nonwpr_heap.addr,
        non_wpr_heap_size: l.nonwpr_heap.size,
        gsp_fw_wpr_start: l.wpr2.addr,
        gsp_fw_heap_offset: l.heap.addr,
        gsp_fw_heap_size: l.heap.size,
        gsp_fw_offset: l.elf.addr,
        boot_bin_offset: l.boot.addr,
        frts_offset: l.frts.addr,
        frts_size: l.frts.size,
        gsp_fw_wpr_end: align_down(l.bios.addr, 0x2_0000),
        fb_size: l.fb_size,
        vga_workspace_offset: l.bios.addr,
        vga_workspace_size: l.bios.size,
    }
}

// ---- radix3 -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemError {
    /// The image is empty, or has more pages than three levels map (512 * 512).
    BadImage,
    /// The caller passed a different number of pages than the image needs.
    WrongPageCount { want: usize, got: usize },
}

/// How many 4 KiB pages level 2 takes for an image of `image_len` bytes:
/// `ALIGN((size / 4096) * 8, 4096) / 4096` (`nvkm_gsp_radix3_sg`, `rm/r535/gsp.c:1675`).
pub fn radix3_lvl2_pages(image_len: u64) -> usize {
    align_up((image_len / PAGE as u64) * 8, PAGE as u64) as usize / PAGE
}

/// The three levels of `nvkm_gsp_radix3_sg` (`rm/r535/gsp.c:1657-1714`):
/// level 0 = one entry, the bus address of level 1; level 1 = the addresses
/// of level 2's pages; level 2 = the addresses of the image's pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Radix3 {
    pub lvl0: Vec<u8>,
    pub lvl1: Vec<u8>,
    /// `radix3_lvl2_pages` pages back to back.
    pub lvl2: Vec<u8>,
}

/// `image_pages`: the bus address of each 4 KiB page of the image, in order;
/// `lvl1_addr`: where level 1 lives; `lvl2_pages`: the bus address of each page
/// of level 2.
pub fn radix3(image_pages: &[u64], lvl1_addr: u64, lvl2_pages: &[u64]) -> Result<Radix3, MemError> {
    if image_pages.is_empty() || image_pages.len() > 512 * 512 {
        return Err(MemError::BadImage);
    }
    let want = radix3_lvl2_pages((image_pages.len() * PAGE) as u64);
    if lvl2_pages.len() != want {
        return Err(MemError::WrongPageCount { want, got: lvl2_pages.len() });
    }
    let mut lvl0 = vec![0u8; PAGE];
    put64(&mut lvl0, 0, lvl1_addr);
    let mut lvl1 = vec![0u8; PAGE];
    for (i, a) in lvl2_pages.iter().enumerate() {
        put64(&mut lvl1, i * 8, *a);
    }
    let mut lvl2 = vec![0u8; want * PAGE];
    for (i, a) in image_pages.iter().enumerate() {
        put64(&mut lvl2, i * 8, *a);
    }
    Ok(Radix3 { lvl0, lvl1, lvl2 })
}

// ---- LibOS init arguments ----------------------------------------------

/// `LIBOS_MEMORY_REGION_CONTIGUOUS`, `LIBOS_MEMORY_REGION_LOC_SYSMEM`
/// (`libos_init_args.h`).
const REGION_CONTIGUOUS: u8 = 1;
const REGION_LOC_SYSMEM: u8 = 1;
/// `sizeof(LibosMemoryRegionInitArgument)`.
pub const LIBOS_ARG_SIZE: usize = 32;

/// `r535_gsp_libos_id8` (`rm/r535/gsp.c:1437-1445`): up to 8 characters, the
/// first in the most significant byte.
pub fn libos_id8(name: &str) -> u64 {
    name.bytes().take(8).fold(0u64, |id, c| (id << 8) | c as u64)
}

/// The four regions of `r535_gsp_libos_init` (`rm/r535/gsp.c:1508-1560`), in
/// order: `LOGINIT`, `LOGINTR`, `LOGRM`, `RMARGS`: (name, bus address, size).
pub const LIBOS_NAMES: [&str; 4] = ["LOGINIT", "LOGINTR", "LOGRM", "RMARGS"];

/// The 4 KiB page of `LibosMemoryRegionInitArgument`s.
pub fn libos_args(regions: &[(u64, u64); 4]) -> Vec<u8> {
    let mut page = vec![0u8; PAGE];
    for (i, (name, (pa, size))) in LIBOS_NAMES.iter().zip(regions.iter()).enumerate() {
        let at = i * LIBOS_ARG_SIZE;
        put64(&mut page, at, libos_id8(name));
        put64(&mut page, at + 8, *pa);
        put64(&mut page, at + 16, *size);
        page[at + 24] = REGION_CONTIGUOUS;
        page[at + 25] = REGION_LOC_SYSMEM;
    }
    page
}

/// A log buffer's page table (`create_pte_array` at offset 8,
/// `rm/r535/gsp.c:1461-1470,1531`): the bus address of each of its pages, right
/// after the 8-byte put pointer, which starts at 0. `buf` is the whole buffer
/// (a multiple of 4 KiB) at bus address `addr`.
pub fn log_buffer_init(buf: &mut [u8], addr: u64) {
    let pages = buf.len() / PAGE;
    for i in 0..pages {
        put64(buf, 8 + i * 8, addr + (i * PAGE) as u64);
    }
}

// ---- RM arguments and the queues' shared memory --------------------------

/// `sizeof(GSP_ARGUMENTS_CACHED)`.
pub const RM_ARGS_SIZE: usize = 72;

/// Where the queues sit in the shared memory (`r535_gsp_shared_init`,
/// `rm/r535/gsp.c:1135-1183`).
pub const CMDQ_SIZE: usize = 0x40000;
pub const MSGQ_SIZE: usize = 0x40000;
/// `ptes.nr`: one entry per page of the queues plus the entries' own pages.
pub const SHARED_PTES: usize = (CMDQ_SIZE + MSGQ_SIZE) / PAGE + (((CMDQ_SIZE + MSGQ_SIZE) / PAGE * 8).div_ceil(PAGE));
/// Bytes of the PTE array (`ALIGN(nr * 8, 4096)`).
pub const SHARED_PTES_BYTES: usize = (SHARED_PTES * 8).div_ceil(PAGE) * PAGE;
pub const SHARED_SIZE: usize = SHARED_PTES_BYTES + CMDQ_SIZE + MSGQ_SIZE;
pub const CMDQ_OFFSET: usize = SHARED_PTES_BYTES;
pub const MSGQ_OFFSET: usize = SHARED_PTES_BYTES + CMDQ_SIZE;

/// The shared memory before boot: the PTEs (each 4 KiB page of the memory at
/// `addr`), the command queue's TX header, the status queue zeroed.
pub fn shared_memory(addr: u64) -> Vec<u8> {
    let mut m = vec![0u8; SHARED_SIZE];
    for i in 0..SHARED_PTES {
        put64(&mut m, i * 8, addr + (i * PAGE) as u64);
    }
    let q = CMDQ_OFFSET;
    // msgqTxHeader: version, size, msgSize, msgCount, writePtr, flags, rxHdrOff, entryOff
    put32(&mut m, q, 0);
    put32(&mut m, q + 4, CMDQ_SIZE as u32);
    put32(&mut m, q + 8, PAGE as u32);
    put32(&mut m, q + 12, ((CMDQ_SIZE - PAGE) / PAGE) as u32);
    put32(&mut m, q + 16, 0);
    put32(&mut m, q + 20, 1);
    // `offsetof(struct { msgqTxHeader tx; msgqRxHeader rx; }, rx.readPtr)`
    put32(&mut m, q + 24, 32);
    put32(&mut m, q + 28, PAGE as u32);
    m
}

/// `r570_gsp_set_rmargs` for a cold boot (`rm/r570/gsp.c:186-207`): the
/// `GSP_ARGUMENTS_CACHED` in the first bytes of a zeroed page.
pub fn rm_args(shared_addr: u64) -> Vec<u8> {
    let mut p = vec![0u8; PAGE];
    put64(&mut p, 0, shared_addr); // sharedMemPhysAddr
    put32(&mut p, 8, SHARED_PTES as u32); // pageTableEntryCount
    put64(&mut p, 16, CMDQ_OFFSET as u64); // cmdQueueOffset
    put64(&mut p, 24, MSGQ_OFFSET as u64); // statQueueOffset
    // srInitArguments: oldLevel 0, flags 0, bInPMTransition 0
    p[48] = 1; // bDmemStack
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rd64(b: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
    }
    fn rd32(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    // ---- layout: numbers computed apart (Python) from the formulas ----

    const FB: u64 = 0x2_0000_0000;

    #[test]
    fn layout_of_this_board() {
        let vga = crate::fwsec::vga_workspace(FB, 1);
        assert_eq!(vga, 0x1_fff0_0000);
        let l = fb_layout(FB, vga, 0x6000, 0x3c9_9000, &R570_LIBOS3);
        assert_eq!(l.bios, Region { addr: 0x1_fff0_0000, size: 0x10_0000 });
        assert_eq!(l.frts, Region { addr: 0x1_ffe0_0000, size: 0x10_0000 });
        assert_eq!(l.boot, Region { addr: 0x1_ffdf_a000, size: 0x6000 });
        assert_eq!(l.elf, Region { addr: 0x1_fc16_0000, size: 0x3c9_9000 });
        assert_eq!(l.heap, Region { addr: 0x1_f420_0000, size: 0x7f0_0000 });
        assert_eq!(l.wpr2, Region { addr: 0x1_f410_0000, size: 0xbe0_0000 });
        assert_eq!(l.nonwpr_heap, Region { addr: 0x1_f400_0000, size: 0x10_0000 });
        // The FRTS region is the one FWSEC-FRTS was given.
        assert_eq!((l.frts.addr, l.frts.size), crate::fwsec::frts_region(FB, 1));
    }

    #[test]
    fn wpr_heap_size_of_8gb_and_the_min_that_never_applies() {
        // 22 MiB + 8 MiB + ALIGN(96 KiB * 8, 1 MiB) + ALIGN(96 MiB, 1 MiB) = 127 MiB
        assert_eq!(wpr_heap_size(FB, &R570_LIBOS3), 127 << 20);
        // nouveau's `max(bytes, 170)`: the "MB" minimum is a no-op.
        assert!(R570_LIBOS3.heap_size_min < 1 << 20);
        // 6 GB board (ceil of 6 GiB = 6): 22 + 8 + 1 + 96
        assert_eq!(wpr_heap_size(6 << 30, &R570_LIBOS3), 127 << 20);
        // 12 GB: ALIGN(96 KiB * 12 = 1.125 MiB, 1 MiB) = 2 MiB
        assert_eq!(wpr_heap_size(12 << 30, &R570_LIBOS3), 128 << 20);
        // a size that is not a whole number of GiB rounds up; 96 KiB * 10 = 960 KiB fits
        // one MiB, 96 KiB * 11 = 1056 KiB needs two (and 128 KiB * 10 would not fit one)
        assert_eq!(wpr_heap_size(10 << 30, &R570_LIBOS3), 127 << 20);
        assert_eq!(wpr_heap_size((10 << 30) + 1, &R570_LIBOS3), 128 << 20);
        assert_eq!(wpr_heap_size(11 << 30, &R570_LIBOS3), 128 << 20);
        // a param set whose minimum does apply (the type allows it)
        let p = WprParams { heap_size_min: 1 << 40, ..R570_LIBOS3 };
        assert_eq!(wpr_heap_size(FB, &p), 1 << 40);
    }

    #[test]
    fn layout_alignments_hold_for_odd_sizes() {
        // an ELF and a bootloader that are not round numbers
        let l = fb_layout(FB, FB - 0x10_0000, 0x6123, 0x3c9_9123, &R570_LIBOS3);
        assert_eq!(l.boot.addr % 0x1000, 0);
        assert_eq!(l.elf.addr % 0x1_0000, 0);
        assert_eq!(l.heap.addr % 0x10_0000, 0);
        assert_eq!(l.heap.size % 0x10_0000, 0);
        assert_eq!(l.wpr2.addr % 0x10_0000, 0);
        assert!(l.elf.addr + l.elf.size <= l.boot.addr);
        assert!(l.boot.addr + l.boot.size <= l.frts.addr);
        assert!(l.heap.addr + l.heap.size <= l.elf.addr);
        assert!(l.wpr2.addr + WPR_META_SIZE as u64 <= l.heap.addr);
        assert_eq!(l.wpr2.addr + l.wpr2.size, l.frts.addr + l.frts.size);
    }

    #[test]
    fn frts_and_wpr_end_are_128k_aligned_below_an_unaligned_workspace() {
        // the register has 64 KiB granularity: the workspace can be at a 64 KiB multiple
        let vga = FB - 0x7_0000;
        let l = fb_layout(FB, vga, 0x6000, 0x3c9_9000, &R570_LIBOS3);
        assert_eq!(l.bios, Region { addr: vga, size: 0x7_0000 });
        assert_eq!(l.frts.addr, FB - 0x8_0000 - 0x10_0000);
        let m = wpr_meta(
            &l,
            &HostBufs { radix3_lvl0: 0, bootloader: 0, signature: 0, signature_size: 0, code_offset: 0, data_offset: 0, manifest_offset: 0 },
        );
        assert_eq!(m.gsp_fw_wpr_end, FB - 0x8_0000);
        assert_eq!((m.vga_workspace_offset, m.vga_workspace_size), (vga, 0x7_0000));
    }

    #[test]
    fn a_display_workspace_moves_everything_down() {
        // bit 3 set with an address in the last MiB: the workspace starts there.
        let vga = crate::fwsec::vga_workspace(FB, (((FB - 0x8_0000) >> 8) as u32 & 0xffff_ff00) | 8);
        assert_eq!(vga, FB - 0x8_0000);
        let l = fb_layout(FB, vga, 0x6000, 0x3c9_9000, &R570_LIBOS3);
        assert_eq!(l.bios.size, 0x8_0000);
        assert_eq!(l.frts.addr, ((FB - 0x8_0000) & !0x1_ffff) - 0x10_0000);
    }

    // ---- WPR meta: offsets of the C struct (nvgpu/gen/abi.c, clang) ----

    fn meta() -> WprMeta {
        let l = fb_layout(FB, FB - 0x10_0000, 0x6000, 0x3c9_9000, &R570_LIBOS3);
        wpr_meta(
            &l,
            &HostBufs {
                radix3_lvl0: 0x1111_0000,
                bootloader: 0x2222_0000,
                signature: 0x3333_0000,
                signature_size: 0x1000,
                code_offset: 0x1800,
                data_offset: 0x800,
                manifest_offset: 0,
            },
        )
    }

    #[test]
    fn wpr_meta_bytes_match_the_c_struct() {
        let b = meta().to_bytes();
        assert_eq!(b.len(), 256);
        // offsets from `clang` + the real header
        let at = |o: usize| rd64(&b, o);
        assert_eq!(at(0), 0xdc3a_ae21_371a_60b3); // magic
        assert_eq!(at(8), 1); // revision
        assert_eq!(at(16), 0x1111_0000); // sysmemAddrOfRadix3Elf
        assert_eq!(at(24), 0x3c9_9000); // sizeOfRadix3Elf
        assert_eq!(at(32), 0x2222_0000); // sysmemAddrOfBootloader
        assert_eq!(at(40), 0x6000); // sizeOfBootloader
        assert_eq!(at(48), 0x1800); // bootloaderCodeOffset
        assert_eq!(at(56), 0x800); // bootloaderDataOffset
        assert_eq!(at(64), 0); // bootloaderManifestOffset
        assert_eq!(at(72), 0x3333_0000); // sysmemAddrOfSignature
        assert_eq!(at(80), 0x1000); // sizeOfSignature
        assert_eq!(at(88), 0x1_f400_0000); // gspFwRsvdStart
        assert_eq!(at(96), 0x1_f400_0000); // nonWprHeapOffset
        assert_eq!(at(104), 0x10_0000); // nonWprHeapSize
        assert_eq!(at(112), 0x1_f410_0000); // gspFwWprStart
        assert_eq!(at(120), 0x1_f420_0000); // gspFwHeapOffset
        assert_eq!(at(128), 0x7f0_0000); // gspFwHeapSize
        assert_eq!(at(136), 0x1_fc16_0000); // gspFwOffset
        assert_eq!(at(144), 0x1_ffdf_a000); // bootBinOffset
        assert_eq!(at(152), 0x1_ffe0_0000); // frtsOffset
        assert_eq!(at(160), 0x10_0000); // frtsSize
        assert_eq!(at(168), 0x1_fff0_0000); // gspFwWprEnd
        assert_eq!(at(176), FB); // fbSize
        assert_eq!(at(184), 0x1_fff0_0000); // vgaWorkspaceOffset
        assert_eq!(at(192), 0x10_0000); // vgaWorkspaceSize
        // bootCount .. partition fields .. flags .. pmuReservedSize .. verified: all 0
        assert!(b[200..].iter().all(|&x| x == 0));
    }

    #[test]
    fn wpr_meta_fields_are_distinct_offsets() {
        // every field has its own value, so a mixed-up pair of offsets shows
        let m = WprMeta {
            sysmem_addr_of_radix3_elf: 1,
            size_of_radix3_elf: 2,
            sysmem_addr_of_bootloader: 3,
            size_of_bootloader: 4,
            bootloader_code_offset: 5,
            bootloader_data_offset: 6,
            bootloader_manifest_offset: 7,
            sysmem_addr_of_signature: 8,
            size_of_signature: 9,
            gsp_fw_rsvd_start: 10,
            non_wpr_heap_offset: 11,
            non_wpr_heap_size: 12,
            gsp_fw_wpr_start: 13,
            gsp_fw_heap_offset: 14,
            gsp_fw_heap_size: 15,
            gsp_fw_offset: 16,
            boot_bin_offset: 17,
            frts_offset: 18,
            frts_size: 19,
            gsp_fw_wpr_end: 20,
            fb_size: 21,
            vga_workspace_offset: 22,
            vga_workspace_size: 23,
        };
        let b = m.to_bytes();
        for i in 0..23 {
            assert_eq!(rd64(&b, 16 + i * 8), i as u64 + 1, "field {i}");
        }
    }

    // ---- radix3 ----

    #[test]
    fn radix3_of_a_small_image() {
        let img: Vec<u64> = (0..5).map(|i| 0x10_0000 + i * 0x1000).collect();
        assert_eq!(radix3_lvl2_pages(5 * 4096), 1);
        let r = radix3(&img, 0xaaaa_0000, &[0xbbbb_0000]).unwrap();
        assert_eq!(r.lvl0.len(), 4096);
        assert_eq!(r.lvl1.len(), 4096);
        assert_eq!(r.lvl2.len(), 4096);
        assert_eq!(rd64(&r.lvl0, 0), 0xaaaa_0000);
        assert_eq!(rd64(&r.lvl0, 8), 0);
        assert_eq!(rd64(&r.lvl1, 0), 0xbbbb_0000);
        assert_eq!(rd64(&r.lvl1, 8), 0);
        for i in 0..5 {
            assert_eq!(rd64(&r.lvl2, i * 8), 0x10_0000 + i as u64 * 0x1000);
        }
        assert_eq!(rd64(&r.lvl2, 5 * 8), 0);
    }

    #[test]
    fn radix3_of_the_real_firmware_size() {
        // .fwimage = 0x3c99000 bytes = 15 513 pages; level 2 = 15 513 * 8 -> 31 pages
        let pages = 0x3c9_9000usize / PAGE;
        assert_eq!(pages, 15_513);
        assert_eq!(radix3_lvl2_pages(0x3c9_9000), 31);
        let img: Vec<u64> = (0..pages as u64).map(|i| i * 0x1000).collect();
        let l2: Vec<u64> = (0..31u64).map(|i| 0x9000_0000 + i * 0x1000).collect();
        let r = radix3(&img, 0x8000_0000, &l2).unwrap();
        // level 1 lists 31 pages, the rest empty; level 2 fills 15 513 entries
        for i in 0..31 {
            assert_eq!(rd64(&r.lvl1, i * 8), 0x9000_0000 + i as u64 * 0x1000);
        }
        assert_eq!(rd64(&r.lvl1, 31 * 8), 0);
        assert_eq!(rd64(&r.lvl2, (pages - 1) * 8), (pages as u64 - 1) * 0x1000);
        assert_eq!(rd64(&r.lvl2, pages * 8), 0);
    }

    #[test]
    fn radix3_refuses_bad_inputs() {
        assert_eq!(radix3(&[], 0, &[]), Err(MemError::BadImage));
        let img = [0u64; 513]; // 513 pages -> 4104 bytes of entries -> 2 pages
        assert_eq!(radix3(&img, 0, &[1]), Err(MemError::WrongPageCount { want: 2, got: 1 }));
        assert!(radix3(&img, 0, &[1, 2]).is_ok());
        assert_eq!(radix3_lvl2_pages(512 * 4096), 1);
        assert_eq!(radix3_lvl2_pages(513 * 4096), 2);
        // more pages than 512 * 512
        let big = vec![0u64; 512 * 512 + 1];
        assert_eq!(radix3(&big, 0, &vec![0; 513]), Err(MemError::BadImage));
    }

    // ---- LibOS ----

    #[test]
    fn libos_ids_and_arguments() {
        assert_eq!(libos_id8("LOGINIT"), 0x004c_4f47_494e_4954);
        assert_eq!(libos_id8("LOGRM"), 0x0000_004c_4f47_524d);
        assert_eq!(libos_id8("RMARGS"), 0x0000_524d_4152_4753);
        assert_eq!(libos_id8("ABCDEFGHIJ"), 0x4142_4344_4546_4748, "only 8 characters");
        let p = libos_args(&[(0x1000, 0x10000), (0x2000, 0x10000), (0x3000, 0x10000), (0x4000, 0x1000)]);
        assert_eq!(p.len(), 4096);
        for (i, (name, pa, size)) in [("LOGINIT", 0x1000u64, 0x10000u64), ("LOGINTR", 0x2000, 0x10000), ("LOGRM", 0x3000, 0x10000), ("RMARGS", 0x4000, 0x1000)].into_iter().enumerate() {
            let at = i * 32;
            assert_eq!(rd64(&p, at), libos_id8(name));
            assert_eq!(rd64(&p, at + 8), pa);
            assert_eq!(rd64(&p, at + 16), size);
            assert_eq!(p[at + 24], 1, "contiguous");
            assert_eq!(p[at + 25], 1, "sysmem");
            assert!(p[at + 26..at + 32].iter().all(|&b| b == 0));
        }
        assert!(p[4 * 32..].iter().all(|&b| b == 0), "terminated by zeros");
        assert_eq!(LIBOS_ARG_SIZE, 32);
    }

    #[test]
    fn log_buffers_carry_their_page_table_after_the_put_pointer() {
        let mut buf = vec![0u8; 0x10000];
        log_buffer_init(&mut buf, 0x5_0000_0000);
        assert_eq!(rd64(&buf, 0), 0, "put pointer");
        for i in 0..16 {
            assert_eq!(rd64(&buf, 8 + i * 8), 0x5_0000_0000 + i as u64 * 0x1000);
        }
        assert_eq!(rd64(&buf, 8 + 16 * 8), 0);
    }

    // ---- shared memory and RM arguments ----

    #[test]
    fn shared_memory_layout() {
        // 128 queue pages + 1 page of PTEs for them = 129 entries -> 1 page
        assert_eq!(SHARED_PTES, 129);
        assert_eq!(SHARED_PTES_BYTES, 0x1000);
        assert_eq!(SHARED_SIZE, 0x1000 + 0x80000);
        assert_eq!((CMDQ_OFFSET, MSGQ_OFFSET), (0x1000, 0x41000));
        let m = shared_memory(0x7_0000_0000);
        assert_eq!(m.len(), SHARED_SIZE);
        for i in 0..129 {
            assert_eq!(rd64(&m, i * 8), 0x7_0000_0000 + i as u64 * 0x1000);
        }
        assert_eq!(rd64(&m, 129 * 8), 0);
        // msgqTxHeader at the command queue (offsets from clang)
        let q = 0x1000;
        assert_eq!(rd32(&m, q), 0); // version
        assert_eq!(rd32(&m, q + 4), 0x40000); // size
        assert_eq!(rd32(&m, q + 8), 0x1000); // msgSize
        assert_eq!(rd32(&m, q + 12), 63); // msgCount = (0x40000 - 0x1000) / 0x1000
        assert_eq!(rd32(&m, q + 16), 0); // writePtr
        assert_eq!(rd32(&m, q + 20), 1); // flags
        assert_eq!(rd32(&m, q + 24), 32); // rxHdrOff = sizeof(msgqTxHeader)
        assert_eq!(rd32(&m, q + 28), 0x1000); // entryOff
        // the rest of the command queue and the whole status queue are zero
        assert!(m[q + 32..].iter().all(|&b| b == 0));
    }

    #[test]
    fn rm_args_match_the_c_struct() {
        let p = rm_args(0x7_0000_0000);
        assert_eq!(p.len(), 4096);
        assert_eq!(rd64(&p, 0), 0x7_0000_0000); // sharedMemPhysAddr
        assert_eq!(rd32(&p, 8), 129); // pageTableEntryCount
        assert_eq!(rd64(&p, 16), 0x1000); // cmdQueueOffset
        assert_eq!(rd64(&p, 24), 0x41000); // statQueueOffset
        assert_eq!(rd32(&p, 32), 0); // srInitArguments.oldLevel
        assert_eq!(rd32(&p, 36), 0); // flags
        assert_eq!(p[40], 0); // bInPMTransition
        assert_eq!(rd32(&p, 44), 0); // gpuInstance
        assert_eq!(p[48], 1); // bDmemStack (r570)
        assert!(p[49..].iter().all(|&b| b == 0)); // profilerArgs, the rest of the page
        assert_eq!(RM_ARGS_SIZE, 72);
    }
}
