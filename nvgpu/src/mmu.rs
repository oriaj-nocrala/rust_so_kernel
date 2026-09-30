//! GA10x GPU page tables (phase 6a): the "GP100 v2" format with 4 KiB pages
//! (`gp100_vmm_desc_12`, `vmmgp100.c:411-418`; `tu102_vmm.page[]`,
//! `vmmtu102.c:52-60`). Pure: [`PageTables`] builds the table images in memory
//! for a caller-chosen physical range; the kernel adapter copies them into VRAM
//! and hands RM the root's address (`NV0080_CTRL_CMD_DMA_SET_PAGE_DIRECTORY`,
//! [`crate::rm::set_page_directory_params`]). Notes: `docs/gpu/mmu-v3-notes.md`.
//!
//! Tree, from the root (49-bit VAs): PD3 2 bits (VA 48:47), PD2 9 (46:38), PD1 9
//! (37:29), PD0 8 (28:21, 16-byte dual PDEs: the big-page PDE in the first 8 bytes,
//! the small-page PDE in the second: `NV_MMU_VER2_DUAL_PDE`, NVIDIA's `dev_mmu.ref`), PT
//! 9 (20:12). Every table is one 4 KiB page. Only small pages: the big-page
//! half of a PD0 entry stays 0.

use alloc::vec;
use alloc::vec::Vec;

/// Every table is 4 KiB (`desc_12[].size`, `0x1000`).
pub const TABLE_SIZE: usize = 0x1000;
/// The VA space is 49 bits; the root has `1 << 2` entries (`numEntries`).
pub const VA_BITS: u32 = 49;
pub const ROOT_ENTRIES: u32 = 4;
pub const PAGE_SHIFT: u32 = 12;
/// Byte offset of the small-page half of a PD0 dual PDE: `NV_MMU_VER2_DUAL_PDE_APERTURE_SMALL`
/// is bits 66:65 and `ADDRESS_SMALL` 117:72 (`dev_mmu.ref`, Turing; Ampere is the same
/// format), the big-page half being bits 0..63. Same as `gp100_vmm_pd0_pde`: `data[0]` is
/// the LPT (`pt[0]`), `data[1]` the SPT (`pt[1]`, `nvkm_vmm_ref_hwpt`: `type = desc->type == SPT`).
pub const PD0_SMALL: usize = 8;

/// The size of the page a PD0 entry can map by itself (`NV_MMU_VER2_DUAL_PDE_IS_PTE`).
pub const HUGE_PAGE: u64 = 2 << 20;
/// 64 KiB pages (`"LPT"`, `docs/gpu/mmu-v3-notes.md`): the big-page half of a PD0 entry points to a table of 32 PTEs
/// (5 index bits, 0x100 bytes), and nouveau maps context buffers of 64 KiB and up with them.
pub const BIG_PAGE: u64 = 64 << 10;
const BIG_ENTRIES: usize = 32;

/// Where a page or table lives (`nvkm_memory_target`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Vram,
    /// System memory, coherent (`NVKM_MEM_TARGET_HOST`).
    Host,
    /// System memory, non-coherent (`NVKM_MEM_TARGET_NCOH`).
    NonCoherent,
}

// PTE bits (`gp100_vmm_valid`, `vmmgp100.c:487-493`).
const PTE_VALID: u64 = 1 << 0;
const PTE_VOL: u64 = 1 << 3;
const PTE_PRIV: u64 = 1 << 5;
const PTE_RO: u64 = 1 << 6;
const PTE_KIND_SHIFT: u32 = 56;

/// The aperture field of a PTE (`gf100_vmm_aper`, `vmmgf100.c:324-333`), bits 2:1.
fn pte_aperture(t: Target) -> u64 {
    (match t {
        Target::Vram => 0,
        Target::Host => 2,
        Target::NonCoherent => 3,
    }) << 1
}

/// The aperture field of a PDE (`gp100_vmm_pde`, `vmmgp100.c:237-251`): note it
/// is numbered differently from a PTE's (VRAM is 1 here), and host memory also
/// sets VOL (bit 3).
fn pde_aperture(t: Target) -> u64 {
    match t {
        Target::Vram => 1 << 1,
        Target::Host => (2 << 1) | (1 << 3),
        Target::NonCoherent => 3 << 1,
    }
}

/// What a mapping is allowed to do. `kind` 0 = pitch linear (no compression).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags {
    pub read_only: bool,
    pub privileged: bool,
    pub kind: u8,
}

/// One PTE for a 4 KiB page at `pa` (`gp100_vmm_valid` + `gp100_vmm_pgt_pte`):
/// `(pa >> 4) | VALID | aperture | VOL (host memory) | PRIV | RO | kind << 56`.
pub fn pte(pa: u64, target: Target, f: Flags) -> u64 {
    let vol = if target == Target::Host { PTE_VOL } else { 0 };
    (pa >> 4)
        | PTE_VALID
        | pte_aperture(target)
        | vol
        | if f.privileged { PTE_PRIV } else { 0 }
        | if f.read_only { PTE_RO } else { 0 }
        | (f.kind as u64) << PTE_KIND_SHIFT
}

/// A PDE pointing at the table at `pa`: `(pa >> 4) | aperture`. There is no
/// valid bit: 0 is "absent".
pub fn pde(pa: u64, target: Target) -> u64 {
    (pa >> 4) | pde_aperture(target)
}

/// Why a mapping was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// `va` or `pa` is not 4 KiB aligned.
    Unaligned,
    /// The VA does not fit in 49 bits.
    OutOfRange,
    /// The table pool is used up.
    NoTables,
    /// The VA already has a mapping.
    AlreadyMapped,
    /// A 2 MiB page and a table of 4 KiB pages cannot share a PD0 slot.
    Overlap,
}

/// The index of `va` at each level, root first: [PD3, PD2, PD1, PD0, PT].
pub fn indices(va: u64) -> [usize; 5] {
    [
        ((va >> 47) & 0x3) as usize,
        ((va >> 38) & 0x1ff) as usize,
        ((va >> 29) & 0x1ff) as usize,
        ((va >> 21) & 0xff) as usize,
        ((va >> 12) & 0x1ff) as usize,
    ]
}

/// A page-table tree under construction. Table `i` sits at physical address
/// `base + i * 0x1000`; table 0 is the root. Tables are handed out in order from
/// the pool `[base, base + capacity * 0x1000)`.
pub struct PageTables {
    base: u64,
    capacity: usize,
    target: Target,
    tables: Vec<[u8; TABLE_SIZE]>,
    /// Tables written since the adapter last copied them out ([`PageTables::take_dirty`]): a run-time bind touches a few.
    dirty: Vec<bool>,
}

/// The address a PDE points to: bits 55:4 hold `addr >> 4` (bits 3:0 are the aperture and VOL; a big-page table is
/// only 256-byte aligned, so bits 7:4 are address).
fn entry_addr(e: u64) -> u64 {
    (e & 0x00ff_ffff_ffff_fff0) << 4
}

/// The physical address a PTE maps: bits 55:8 (bits 7:0 are flags: valid, aperture, VOL, encrypted, PRIV at 5, RO at
/// 6, ATOMIC_DISABLE at 7; bits 63:56 the kind). Pages are 4 KiB-aligned.
fn pte_addr(e: u64) -> u64 {
    (e & 0x00ff_ffff_ffff_ff00) << 4
}

fn rd64(t: &[u8; TABLE_SIZE], at: usize) -> u64 {
    u64::from_le_bytes(t[at..at + 8].try_into().unwrap())
}
fn wr64(t: &mut [u8; TABLE_SIZE], at: usize, v: u64) {
    t[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

impl PageTables {
    /// A tree whose root is at `base` and whose tables live in `target`
    /// memory, drawing at most `capacity` tables (root included) from the pool.
    pub fn new(base: u64, capacity: usize, target: Target) -> Self {
        // a PDE of 0 means "absent", so no table can be at physical address 0
        assert!(base != 0 && base % TABLE_SIZE as u64 == 0 && capacity >= 1);
        PageTables { base, capacity, target, tables: vec![[0u8; TABLE_SIZE]], dirty: vec![true] }
    }

    /// The physical address of the root (`SET_PAGE_DIRECTORY.physAddress`).
    pub fn root(&self) -> u64 {
        self.base
    }

    /// Tables allocated so far, root included.
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    /// The table images with their physical addresses, for the adapter to
    /// write out.
    pub fn images(&self) -> impl Iterator<Item = (u64, &[u8; TABLE_SIZE])> {
        let base = self.base;
        self.tables.iter().enumerate().map(move |(i, t)| (base + (i * TABLE_SIZE) as u64, t))
    }

    /// Write one entry and remember that its table changed.
    fn put(&mut self, table: usize, at: usize, v: u64) {
        wr64(&mut self.tables[table], at, v);
        self.dirty[table] = true;
    }

    /// The tables written since the last call (a fresh tree reports all of them), in pool order, each with its physical
    /// address and image: what the adapter copies into VRAM after a bind or an unbind. The root is table 0.
    pub fn take_dirty(&mut self) -> Vec<(u64, [u8; TABLE_SIZE])> {
        let base = self.base;
        let mut out = Vec::new();
        for (i, d) in self.dirty.iter_mut().enumerate() {
            if core::mem::take(d) {
                out.push((base + (i * TABLE_SIZE) as u64, self.tables[i]));
            }
        }
        out
    }

    fn table_at(&self, pa: u64) -> Option<usize> {
        let off = pa.checked_sub(self.base)?;
        let i = (off / TABLE_SIZE as u64) as usize;
        (off % TABLE_SIZE as u64 == 0 && i < self.tables.len()).then_some(i)
    }

    /// The child table a directory entry points to, allocating it if the slot
    /// is empty. `at` is the byte offset of the (small-page) entry in table
    /// `parent`.
    fn child(&mut self, parent: usize, at: usize) -> Result<usize, MapError> {
        let cur = rd64(&self.tables[parent], at);
        if cur != 0 {
            return Ok(self.table_at(entry_addr(cur)).expect("directory entry outside the pool"));
        }
        if self.tables.len() >= self.capacity {
            return Err(MapError::NoTables);
        }
        let i = self.tables.len();
        self.tables.push([0u8; TABLE_SIZE]);
        self.dirty.push(true);
        let pa = self.base + (i * TABLE_SIZE) as u64;
        self.put(parent, at, pde(pa, self.target));
        Ok(i)
    }

    /// Map one 4 KiB page.
    pub fn map(&mut self, va: u64, pa: u64, target: Target, f: Flags) -> Result<(), MapError> {
        if va & 0xfff != 0 || pa & 0xfff != 0 {
            return Err(MapError::Unaligned);
        }
        if va >> VA_BITS != 0 {
            return Err(MapError::OutOfRange);
        }
        let ix = indices(va);
        let mut t = 0;
        t = self.child(t, ix[0] * 8)?;
        t = self.child(t, ix[1] * 8)?;
        t = self.child(t, ix[2] * 8)?;
        // PD0: 16-byte dual PDE, the small-page PDE is the second half; the first
        // half holding a PTE means a 2 MiB page covers this VA
        if rd64(&self.tables[t], ix[3] * 16) & PTE_VALID != 0 {
            return Err(MapError::Overlap);
        }
        t = self.child(t, ix[3] * 16 + PD0_SMALL)?;
        let at = ix[4] * 8;
        if rd64(&self.tables[t], at) != 0 {
            return Err(MapError::AlreadyMapped);
        }
        self.put(t, at, pte(pa, target, f));
        Ok(())
    }

    /// Map `len` bytes (a multiple of 4 KiB) of contiguous physical memory.
    pub fn map_range(&mut self, va: u64, pa: u64, len: u64, target: Target, f: Flags) -> Result<(), MapError> {
        if len & 0xfff != 0 {
            return Err(MapError::Unaligned);
        }
        let mut off = 0;
        while off < len {
            self.map(va + off, pa + off, target, f)?;
            off += 0x1000;
        }
        Ok(())
    }

    /// Map one 64 KiB page: the PTE is one of the 32 of the big-page table the PD0 entry's first half points to (a
    /// 4 KiB table page of ours holds it, only its first 0x100 bytes are used). The small-page half of the same
    /// entry may hold a table of 4 KiB pages elsewhere in the same 2 MiB: the hardware takes the big PTE when it
    /// is valid.
    pub fn map_big(&mut self, va: u64, pa: u64, target: Target, f: Flags) -> Result<(), MapError> {
        if va & (BIG_PAGE - 1) != 0 || pa & (BIG_PAGE - 1) != 0 {
            return Err(MapError::Unaligned);
        }
        if va >> VA_BITS != 0 {
            return Err(MapError::OutOfRange);
        }
        let ix = indices(va);
        let mut t = 0;
        t = self.child(t, ix[0] * 8)?;
        t = self.child(t, ix[1] * 8)?;
        t = self.child(t, ix[2] * 8)?;
        if rd64(&self.tables[t], ix[3] * 16) & PTE_VALID != 0 {
            return Err(MapError::Overlap); // a 2 MiB page covers this VA
        }
        t = self.child(t, ix[3] * 16)?;
        let at = ((va >> 16) as usize & (BIG_ENTRIES - 1)) * 8;
        if rd64(&self.tables[t], at) != 0 {
            return Err(MapError::AlreadyMapped);
        }
        self.put(t, at, pte(pa, target, f));
        Ok(())
    }

    /// Map `len` bytes (a multiple of 64 KiB) of contiguous physical memory with 64 KiB pages.
    pub fn map_big_range(&mut self, va: u64, pa: u64, len: u64, target: Target, f: Flags) -> Result<(), MapError> {
        if len & (BIG_PAGE - 1) != 0 {
            return Err(MapError::Unaligned);
        }
        let mut off = 0;
        while off < len {
            self.map_big(va + off, pa + off, target, f)?;
            off += BIG_PAGE;
        }
        Ok(())
    }

    /// Map one 2 MiB page: the PD0 entry itself is the PTE (`gp100_vmm_pd0_pte`,
    /// `vmmgp100.c:215-227`: `(addr >> 4) | type` in the first 8 bytes of the
    /// 16-byte entry, 0 in the second; `NV_MMU_VER2_DUAL_PDE_IS_PTE` is bit 0).
    pub fn map_huge(&mut self, va: u64, pa: u64, target: Target, f: Flags) -> Result<(), MapError> {
        if va & (HUGE_PAGE - 1) != 0 || pa & (HUGE_PAGE - 1) != 0 {
            return Err(MapError::Unaligned);
        }
        if va >> VA_BITS != 0 {
            return Err(MapError::OutOfRange);
        }
        let ix = indices(va);
        let mut t = 0;
        t = self.child(t, ix[0] * 8)?;
        t = self.child(t, ix[1] * 8)?;
        t = self.child(t, ix[2] * 8)?;
        let at = ix[3] * 16;
        if rd64(&self.tables[t], at) != 0 {
            return Err(MapError::AlreadyMapped);
        }
        if rd64(&self.tables[t], at + PD0_SMALL) != 0 {
            return Err(MapError::Overlap);
        }
        self.put(t, at, pte(pa, target, f));
        Ok(())
    }

    /// Map `len` bytes (a multiple of 2 MiB) of contiguous physical memory with 2 MiB pages.
    pub fn map_huge_range(&mut self, va: u64, pa: u64, len: u64, target: Target, f: Flags) -> Result<(), MapError> {
        if len & (HUGE_PAGE - 1) != 0 {
            return Err(MapError::Unaligned);
        }
        let mut off = 0;
        while off < len {
            self.map_huge(va + off, pa + off, target, f)?;
            off += HUGE_PAGE;
        }
        Ok(())
    }

    /// Remove the mapping of one page; `false` if there was none.
    pub fn unmap(&mut self, va: u64) -> bool {
        if let Some((t, at)) = self.locate_huge(va) {
            if rd64(&self.tables[t], at) != 0 {
                self.put(t, at, 0);
                return true;
            }
        }
        match self.locate(va) {
            Some((t, at)) if rd64(&self.tables[t], at) != 0 => {
                self.put(t, at, 0);
                true
            }
            _ => false,
        }
    }

    /// The (PD0 table, byte offset) of the 2 MiB PTE that would cover `va`, if the
    /// directories above it exist.
    fn locate_huge(&self, va: u64) -> Option<(usize, usize)> {
        if va >> VA_BITS != 0 {
            return None;
        }
        let ix = indices(va);
        let mut t = 0;
        for &i in &ix[..3] {
            t = self.table_at(entry_addr(rd64(&self.tables[t], i * 8)))?;
        }
        Some((t, ix[3] * 16))
    }

    /// The (table, byte offset) of `va`'s PTE, if all its directories exist.
    fn locate(&self, va: u64) -> Option<(usize, usize)> {
        if va >> VA_BITS != 0 {
            return None;
        }
        let ix = indices(va);
        let mut t = 0;
        for (level, &i) in ix[..4].iter().enumerate() {
            let e = rd64(&self.tables[t], if level == 3 { i * 16 + PD0_SMALL } else { i * 8 });
            // an absent entry is 0, whose address (0) is no table of ours
            t = self.table_at(entry_addr(e))?;
        }
        Some((t, ix[4] * 8))
    }

    /// The physical addresses of the root and of the PD2 and PD1 tables on `va`'s path, if they exist: what
    /// `NV90F1_CTRL_CMD_VASPACE_COPY_SERVER_RESERVED_PDES` names for a VA space RM manages over tables of ours
    /// (`levels[i].physAddress`, root first; nouveau's `pd->pt[0]->addr`, then `pd->pde[0]`, `vmm.c:122-131`).
    pub fn directories(&self, va: u64) -> Option<[u64; 3]> {
        if va >> VA_BITS != 0 {
            return None;
        }
        let ix = indices(va);
        let pd2 = self.table_at(entry_addr(rd64(&self.tables[0], ix[0] * 8)))?;
        let pd1 = self.table_at(entry_addr(rd64(&self.tables[pd2], ix[1] * 8)))?;
        let at = |i: usize| self.base + (i * TABLE_SIZE) as u64;
        Some([at(0), at(pd2), at(pd1)])
    }

    /// Translate `va` through the images: the physical address and the PTE.
    pub fn translate(&self, va: u64) -> Option<(u64, u64)> {
        if let Some((t, at)) = self.locate_huge(va) {
            let e = rd64(&self.tables[t], at);
            if e & PTE_VALID != 0 {
                return Some((pte_addr(e) | (va & (HUGE_PAGE - 1)), e));
            }
        }
        // a valid 64 KiB PTE wins over the table of 4 KiB pages
        if let Some((t, at)) = self.locate_huge(va) {
            let pde = rd64(&self.tables[t], at);
            if pde != 0 && pde & PTE_VALID == 0 {
                if let Some(bt) = self.table_at(entry_addr(pde)) {
                    let e = rd64(&self.tables[bt], ((va >> 16) as usize & (BIG_ENTRIES - 1)) * 8);
                    if e & PTE_VALID != 0 {
                        return Some((pte_addr(e) | (va & (BIG_PAGE - 1)), e));
                    }
                }
            }
        }
        let (t, at) = self.locate(va)?;
        let e = rd64(&self.tables[t], at);
        (e & PTE_VALID != 0).then(|| (pte_addr(e) | (va & 0xfff), e))
    }
}

#[cfg(test)]
impl PageTables {
    /// A tree rebuilt from table images (in pool order): what the GPU would walk after they were written to VRAM.
    pub(crate) fn from_images(base: u64, capacity: usize, images: &[[u8; TABLE_SIZE]]) -> Self {
        PageTables { base, capacity, target: Target::Vram, tables: images.to_vec(), dirty: vec![false; images.len()] }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x0400_0000; // 64 MiB

    /// An independent walk over raw images, written from the format alone
    /// (bit positions from the table in the module docs), not from `map`.
    fn walk(pt: &PageTables, va: u64) -> Option<(u64, u64)> {
        let get = |pa: u64, off: usize| -> Option<u64> {
            let (_, img) = pt.images().find(|(a, _)| *a == pa)?;
            Some(u64::from_le_bytes(img[off..off + 8].try_into().unwrap()))
        };
        let mut table = pt.root();
        let shifts_bits = [(47u32, 2u32, 8usize), (38, 9, 8), (29, 9, 8), (21, 8, 16)];
        for (shift, bits, size) in shifts_bits {
            let i = ((va >> shift) & ((1 << bits) - 1)) as usize;
            if size == 16 {
                // the big-page half with its bit 0 set is a 2 MiB PTE (NV_MMU_VER2_DUAL_PDE_IS_PTE)
                let big = get(table, i * 16)?;
                if big & 1 == 1 {
                    return Some(((((big >> 8) & 0x00ff_ffff_ffff) << 12) | (va & 0x1f_ffff), big));
                }
            }
            if size == 16 {
                // a big-page table (first half, bit 0 clear): a valid PTE in it wins over the small tables
                let big = get(table, i * 16)?;
                if big != 0 {
                    let bt = ((big >> 8) & 0x00ff_ffff_ffff) << 12;
                    if let Some(e) = get(bt, (((va >> 16) & 0x1f) as usize) * 8) {
                        if e & 1 == 1 {
                            return Some(((((e >> 8) & 0x00ff_ffff_ffff) << 12) | (va & 0xffff), e));
                        }
                    }
                }
            }
            let e = get(table, i * size + if size == 16 { 8 } else { 0 })?;
            if e == 0 {
                return None;
            }
            table = ((e >> 8) & 0x00ff_ffff_ffff) << 12;
        }
        let e = get(table, (((va >> 12) & 0x1ff) as usize) * 8)?;
        if e & 1 == 0 {
            return None;
        }
        Some((((e >> 8) & 0x00ff_ffff_ffff) << 12 | (va & 0xfff), e))
    }

    #[test]
    fn pte_words_are_hand_computed() {
        // VRAM page at 0x1_2345_6000: (pa >> 4) = 0x1_2345_600, VALID, aperture 0
        assert_eq!(pte(0x1_2345_6000, Target::Vram, Flags::default()), 0x0_1234_5600 | 1);
        // host coherent: aperture 2 -> bits 2:1 = 0b10, VOL bit 3
        assert_eq!(pte(0x2000, Target::Host, Flags::default()), 0x200 | 1 | (2 << 1) | (1 << 3));
        // non-coherent: aperture 3, no VOL
        assert_eq!(pte(0x2000, Target::NonCoherent, Flags::default()), 0x200 | 1 | (3 << 1));
        // flags
        let f = Flags { read_only: true, privileged: true, kind: 0xfe };
        assert_eq!(pte(0x1000, Target::Vram, f), 0x100 | 1 | (1 << 5) | (1 << 6) | (0xfeu64 << 56));
        assert_eq!(pte(0x1000, Target::Vram, Flags { read_only: true, ..Flags::default() }), 0x100 | 1 | (1 << 6));
        assert_eq!(pte(0x1000, Target::Vram, Flags { privileged: true, ..Flags::default() }), 0x100 | 1 | (1 << 5));
    }

    #[test]
    fn pde_words_are_hand_computed() {
        // VRAM = 1 in bits 2:1, i.e. 0b010; no valid bit
        assert_eq!(pde(0x1_f07d_1000, Target::Vram), 0x1f07d100 | 0x2);
        assert_eq!(pde(0x3000, Target::Host), 0x300 | 0x4 | 0x8);
        assert_eq!(pde(0x3000, Target::NonCoherent), 0x300 | 0x6);
        assert_ne!(pde(0x1000, Target::Vram) & 0xf, 0, "a PDE is never 0 for a table at 0x1000");
    }

    #[test]
    fn indices_split_a_49_bit_va() {
        assert_eq!(indices(0), [0; 5]);
        assert_eq!(indices(0x1000), [0, 0, 0, 0, 1]);
        assert_eq!(indices(1 << 21), [0, 0, 0, 1, 0]);
        assert_eq!(indices(1 << 29), [0, 0, 1, 0, 0]);
        assert_eq!(indices(1 << 38), [0, 1, 0, 0, 0]);
        assert_eq!(indices(1 << 47), [1, 0, 0, 0, 0]);
        assert_eq!(indices((1 << 49) - 0x1000), [3, 0x1ff, 0x1ff, 0xff, 0x1ff]);
        // the address the trace's RM-managed window starts at (4 GiB): PD1 slot 8
        assert_eq!(indices(0x1_0000_0000), [0, 0, 8, 0, 0]);
    }

    #[test]
    fn the_first_mapping_builds_a_chain_of_five_tables() {
        let mut pt = PageTables::new(BASE, 16, Target::Vram);
        assert_eq!((pt.root(), pt.len()), (BASE, 1));
        pt.map(0x20_0000_1000, 0x1_0000_5000, Target::Vram, Flags::default()).unwrap();
        assert_eq!(pt.len(), 5, "PD3 + PD2 + PD1 + PD0 + PT");
        // the root's slot points at table 1, which is at BASE + 0x1000
        let ix = indices(0x20_0000_1000);
        let root = pt.images().next().unwrap().1;
        assert_eq!(rd64(root, ix[0] * 8), (((BASE + 0x1000) >> 4) | 2));
        // and the leaf is where the format says
        let (_, leaf) = pt.images().nth(4).unwrap();
        assert_eq!(rd64(leaf, ix[4] * 8), 0x1_0000_5000 >> 4 | 1);
        // the same tables serve a neighbour without new ones
        pt.map(0x20_0000_2000, 0x1_0000_6000, Target::Vram, Flags::default()).unwrap();
        assert_eq!(pt.len(), 5);
    }

    #[test]
    fn directories_name_the_root_pd2_and_pd1_on_a_path() {
        let mut pt = PageTables::new(BASE, 16, Target::Vram);
        assert_eq!(pt.directories(0x1000), None, "no PD2 yet");
        // a first path under root[1] / PD2[3], then a second under root[0] / PD2[0]: the tables are not in
        // order, so the addresses must come from the entries
        pt.map((1 << 47) | (3 << 38), 0x5000, Target::Vram, Flags::default()).unwrap();
        pt.map(0x1000, 0x6000, Target::Vram, Flags::default()).unwrap();
        assert_eq!(pt.directories((1 << 47) | (3 << 38) | 0x1234_5000), Some([BASE, BASE + 0x1000, BASE + 0x2000]));
        assert_eq!(pt.directories(0x1000), Some([BASE, BASE + 0x5000, BASE + 0x6000]));
        // the server-reserved window (4 GiB) shares root[0] and PD2[0] with VA 0
        assert_eq!(pt.directories(0x1_0000_0000), pt.directories(0x1000));
        assert_eq!(pt.directories(1 << 49), None);
        // cross-check against the independent walk: PD1 is the table whose slot 0 leads to the leaf
        let pd1 = pt.directories(0x1000).unwrap()[2];
        let img = pt.images().find(|(a, _)| *a == pd1).unwrap().1;
        assert_ne!(rd64(img, 0), 0);
        assert_eq!(walk(&pt, 0x1000).map(|(pa, _)| pa), Some(0x6000));
    }

    #[test]
    fn pd0_entries_are_sixteen_bytes_and_use_the_small_page_half() {
        let mut pt = PageTables::new(BASE, 16, Target::Vram);
        let va = 3u64 << 21; // PD0 index 3
        pt.map(va, 0x5000, Target::Vram, Flags::default()).unwrap();
        let (_, pd0) = pt.images().nth(3).unwrap();
        assert_eq!(rd64(pd0, 3 * 16 + 8), pde(BASE + 4 * 0x1000, Target::Vram), "small-page half is the SECOND qword");
        assert_eq!(rd64(pd0, 3 * 16), 0, "big-page half (the first qword) stays empty");
        assert!(pd0[..3 * 16].iter().all(|&b| b == 0) && pd0[4 * 16..].iter().all(|&b| b == 0));
        // the last PD0 slot is at offset 255 * 16
        let mut pt = PageTables::new(BASE, 16, Target::Vram);
        pt.map(255 << 21, 0x5000, Target::Vram, Flags::default()).unwrap();
        let (_, pd0) = pt.images().nth(3).unwrap();
        assert_ne!(rd64(pd0, 255 * 16 + 8), 0);
    }

    #[test]
    fn map_and_translate_agree_with_the_independent_walk() {
        let mut pt = PageTables::new(BASE, 64, Target::Vram);
        let cases = [
            (0x0000_0000_1000u64, 0x1_f000_0000u64),
            (0x0000_0020_0000, 0x1_f000_1000),
            (0x0000_4000_0000, 0x0_0100_0000),
            (0x0040_0000_0000, 0x0_0200_0000),
            (0x0008_0000_0000_0u64, 0x0_0300_0000),
            (0x1_ffff_ffff_f000, 0xa_bcde_f000),
        ];
        for &(va, pa) in &cases {
            pt.map(va, pa, Target::Vram, Flags::default()).unwrap();
        }
        for &(va, pa) in &cases {
            assert_eq!(walk(&pt, va).map(|w| w.0), Some(pa), "walk {va:#x}");
            assert_eq!(pt.translate(va).map(|w| w.0), Some(pa), "translate {va:#x}");
            assert_eq!(pt.translate(va + 0x123).map(|w| w.0), Some(pa + 0x123), "offset kept");
            assert_eq!(walk(&pt, va + 0xabc).map(|w| w.0), Some(pa + 0xabc));
        }
        assert_eq!(walk(&pt, 0x3000), None);
        assert_eq!(pt.translate(0x3000), None);
        assert_eq!(pt.translate(1 << 49), None);
    }

    #[test]
    fn big_pages_use_the_first_half_of_pd0_and_a_32_entry_table() {
        let mut pt = PageTables::new(BASE, 32, Target::Vram);
        // two 64 KiB pages in the same 2 MiB, one in the next
        let (va, pa) = (0x3_0001_0000u64, 0x1_2345_0000u64);
        pt.map_big(va, pa, Target::Vram, Flags { privileged: true, ..Flags::default() }).unwrap();
        pt.map_big(va + 0x10000, pa + 0x10000, Target::Vram, Flags::default()).unwrap();
        pt.map_big(va + 0x20_0000, 0x7_0000, Target::Vram, Flags::default()).unwrap();
        // 5 directory/table pages for the first (PD3 PD2 PD1 PD0 + LPT), one more LPT for the next 2 MiB
        assert_eq!(pt.len(), 6);
        let ix = indices(va);
        let (_, pd0) = pt.images().nth(3).unwrap();
        // first qword of the PD0 entry: the big-page table, not a PTE (bit 0 clear); second qword empty
        let lpt = BASE + 4 * 0x1000;
        assert_eq!(rd64(pd0, ix[3] * 16), pde(lpt, Target::Vram));
        assert_eq!(rd64(pd0, ix[3] * 16) & 1, 0);
        assert_eq!(rd64(pd0, ix[3] * 16 + 8), 0);
        // the table: entry (va >> 16) & 31 = 1 and 2
        let (_, t) = pt.images().nth(4).unwrap();
        assert_eq!(rd64(t, 8), pte(pa, Target::Vram, Flags { privileged: true, ..Flags::default() }));
        assert_eq!(rd64(t, 16), pte(pa + 0x10000, Target::Vram, Flags::default()));
        assert_eq!(rd64(t, 0), 0);
        assert!(t[0x100..].iter().all(|&b| b == 0), "only 32 entries are used");
        // translate and the independent walk agree, offsets inside the 64 KiB kept, neighbours unmapped
        for (v, p) in [(va, pa), (va + 0x10000, pa + 0x10000), (va + 0x20_0000, 0x7_0000)] {
            for off in [0u64, 0x123, 0xffff] {
                assert_eq!(walk(&pt, v + off).map(|w| w.0), Some(p + off), "walk {v:#x}+{off:#x}");
                assert_eq!(pt.translate(v + off).map(|w| w.0), Some(p + off), "translate {v:#x}+{off:#x}");
            }
        }
        assert_eq!(walk(&pt, va + 0x20000), None);
        assert_eq!(pt.translate(va + 0x20000), None);
        assert_eq!(pt.translate(va - 0x10000), None);
    }

    #[test]
    fn big_and_small_pages_can_share_a_pd0_slot_and_big_wins() {
        let mut pt = PageTables::new(BASE, 32, Target::Vram);
        let base = 0x3_0000_0000u64;
        pt.map_big(base + 0x10000, 0x100_0000, Target::Vram, Flags::default()).unwrap();
        pt.map(base + 0x2000, 0x5000, Target::Vram, Flags::default()).unwrap();
        pt.map(base + 0x11000, 0x6000, Target::Vram, Flags::default()).unwrap(); // under the big page: shadowed
        assert_eq!(pt.translate(base + 0x2000).map(|w| w.0), Some(0x5000));
        assert_eq!(walk(&pt, base + 0x2000).map(|w| w.0), Some(0x5000));
        assert_eq!(pt.translate(base + 0x11000).map(|w| w.0), Some(0x100_1000), "the big PTE is valid: it wins");
        assert_eq!(walk(&pt, base + 0x11000).map(|w| w.0), Some(0x100_1000));
        let (_, pd0) = pt.images().nth(3).unwrap();
        let ix = indices(base);
        assert_ne!(rd64(pd0, ix[3] * 16), 0);
        assert_ne!(rd64(pd0, ix[3] * 16 + 8), 0);
    }

    #[test]
    fn big_pages_refuse_what_they_cannot_hold() {
        let mut pt = PageTables::new(BASE, 32, Target::Vram);
        assert_eq!(pt.map_big(0x1_0000 + 0x1000, 0x10000, Target::Vram, Flags::default()), Err(MapError::Unaligned));
        assert_eq!(pt.map_big(0x1_0000, 0x10800, Target::Vram, Flags::default()), Err(MapError::Unaligned));
        assert_eq!(pt.map_big(1 << 49, 0x10000, Target::Vram, Flags::default()), Err(MapError::OutOfRange));
        pt.map_big(0x1_0000, 0x10000, Target::Vram, Flags::default()).unwrap();
        assert_eq!(pt.map_big(0x1_0000, 0x20000, Target::Vram, Flags::default()), Err(MapError::AlreadyMapped));
        assert_eq!(pt.map_big_range(0x4_0000, 0x40000, 0x8001, Target::Vram, Flags::default()), Err(MapError::Unaligned));
        // a 2 MiB page in the same slot
        pt.map_huge(0x20_0000, 0x40_0000, Target::Vram, Flags::default()).unwrap();
        assert_eq!(pt.map_big(0x20_0000, 0x10000, Target::Vram, Flags::default()), Err(MapError::Overlap));
        // and a range crossing a 2 MiB boundary
        let mut pt = PageTables::new(BASE, 32, Target::Vram);
        pt.map_big_range(0x1f_0000, 0x1f0000, 0x3_0000, Target::Vram, Flags::default()).unwrap();
        for k in 0..3u64 {
            assert_eq!(pt.translate(0x1f_0000 + k * 0x10000 + 5).map(|w| w.0), Some(0x1f0000 + k * 0x10000 + 5));
            assert_eq!(walk(&pt, 0x1f_0000 + k * 0x10000 + 5).map(|w| w.0), Some(0x1f0000 + k * 0x10000 + 5));
        }
        assert_eq!(BIG_PAGE, 0x10000);
    }

    #[test]
    fn flags_in_a_pte_do_not_leak_into_its_address() {
        let f = Flags { privileged: true, read_only: true, kind: 0 };
        let mut pt = PageTables::new(BASE, 32, Target::Vram);
        pt.map(0x1000, 0x7_0000, Target::Vram, f).unwrap();
        pt.map_big(0x40_0000, 0x8_0000, Target::Vram, f).unwrap();
        pt.map_huge(0x80_0000, 0x120_0000, Target::Vram, f).unwrap();
        for (va, pa) in [(0x1000u64, 0x7_0000u64), (0x40_0000, 0x8_0000), (0x80_0000, 0x120_0000)] {
            assert_eq!(pt.translate(va + 5).map(|w| w.0), Some(pa + 5), "translate {va:#x}");
            assert_eq!(walk(&pt, va + 5).map(|w| w.0), Some(pa + 5), "walk {va:#x}");
        }
    }

    #[test]
    fn an_invalid_big_pte_falls_through_to_the_small_table() {
        let mut pt = PageTables::new(BASE, 32, Target::Vram);
        let base = 0x3_0000_0000u64;
        pt.map_big(base + 0x10000, 0x100_0000, Target::Vram, Flags::default()).unwrap();
        pt.map(base + 0x21000, 0x9000, Target::Vram, Flags::default()).unwrap();
        // the big table's entry for base + 0x20000 holds a sparse marker (VOL without VALID): not a mapping
        let lpt = 4; // PD3 PD2 PD1 PD0 are tables 0..=3, the big table is 4
        wr64(&mut pt.tables[lpt], 2 * 8, 1 << 3);
        assert_eq!(pt.translate(base + 0x21000).map(|w| w.0), Some(0x9000), "the small mapping still answers");
        assert_eq!(walk(&pt, base + 0x21000).map(|w| w.0), Some(0x9000));
    }

    #[test]
    fn map_range_and_unmap() {
        let mut pt = PageTables::new(BASE, 16, Target::Vram);
        pt.map_range(0x10_0000, 0x8000_0000, 0x5000, Target::Host, Flags::default()).unwrap();
        for i in 0..5u64 {
            let (pa, e) = pt.translate(0x10_0000 + i * 0x1000).unwrap();
            assert_eq!(pa, 0x8000_0000 + i * 0x1000);
            assert_eq!(e & 0xe, (2 << 1) | (1 << 3), "host aperture + VOL");
        }
        assert_eq!(pt.translate(0x10_5000), None);
        assert!(pt.unmap(0x10_2000));
        assert_eq!(pt.translate(0x10_2000), None);
        assert!(pt.translate(0x10_3000).is_some());
        assert!(!pt.unmap(0x10_2000), "already gone");
        assert!(!pt.unmap(0x9000_0000_0000), "no directories there");
        assert!(!pt.unmap(1 << 49));
        // a freed slot can be mapped again
        pt.map(0x10_2000, 0x9000, Target::Vram, Flags::default()).unwrap();
        assert_eq!(pt.translate(0x10_2000).map(|w| w.0), Some(0x9000));
    }

    #[test]
    fn refusals() {
        let mut pt = PageTables::new(BASE, 5, Target::Vram);
        let f = Flags::default();
        assert_eq!(pt.map(0x1001, 0x1000, Target::Vram, f), Err(MapError::Unaligned));
        assert_eq!(pt.map(0x1000, 0x1001, Target::Vram, f), Err(MapError::Unaligned));
        assert_eq!(pt.map_range(0x1000, 0x1000, 0x1800, Target::Vram, f), Err(MapError::Unaligned));
        assert_eq!(pt.map(1 << 49, 0x1000, Target::Vram, f), Err(MapError::OutOfRange));
        assert_eq!(pt.len(), 1, "refusals allocate nothing");
        pt.map(0x1000, 0x1000, Target::Vram, f).unwrap();
        assert_eq!(pt.map(0x1000, 0x2000, Target::Vram, f), Err(MapError::AlreadyMapped));
        // the pool is 5 tables: a mapping in another PD3 slot needs 4 more
        assert_eq!(pt.map(1 << 47, 0x1000, Target::Vram, f), Err(MapError::NoTables));
        // the same PT still works
        pt.map(0x2000, 0x2000, Target::Vram, f).unwrap();
    }

    #[test]
    fn directories_carry_the_tables_target() {
        let mut pt = PageTables::new(0x1000, 8, Target::Host);
        pt.map(0x1000, 0x1000, Target::Vram, Flags::default()).unwrap();
        let root = pt.images().next().unwrap().1;
        assert_eq!(rd64(root, 0), pde(0x2000, Target::Host));
        assert_eq!(rd64(root, 0) & 0xe, 0x4 | 0x8);
        // pool at a high VRAM address: the PDE keeps every address bit
        let mut pt = PageTables::new(0x1_f07d_1000, 8, Target::Vram);
        pt.map(0x1000, 0x1000, Target::Vram, Flags::default()).unwrap();
        assert_eq!(rd64(pt.images().next().unwrap().1, 0), (0x1_f07d_2000u64 >> 4) | 2);
        assert_eq!(pt.translate(0x1000).map(|w| w.0), Some(0x1000));
    }

    #[test]
    fn every_address_bit_of_the_entry_survives() {
        // a physical address with bit 55 set lands in bit 51 of the entry
        let pa = (1u64 << 55) | 0x1000;
        let mut pt = PageTables::new(BASE, 8, Target::NonCoherent);
        pt.map(0x1000, pa, Target::NonCoherent, Flags::default()).unwrap();
        assert_eq!(pt.translate(0x1000).map(|w| w.0), Some(pa));
        assert_eq!(pte(pa, Target::NonCoherent, Flags::default()) >> 51 & 1, 1);
    }

    #[test]
    fn the_pool_is_never_exceeded() {
        let mut pt = PageTables::new(BASE, 5, Target::Vram);
        pt.map(0x1000, 0x1000, Target::Vram, Flags::default()).unwrap();
        assert_eq!(pt.len(), 5, "a mapping needing exactly the pool fits");
        assert_eq!(pt.map(1 << 47, 0x1000, Target::Vram, Flags::default()), Err(MapError::NoTables));
        assert_eq!(pt.len(), 5);
        assert_eq!(PageTables::new(BASE, 1, Target::Vram).map(0x1000, 0x1000, Target::Vram, Flags::default()), Err(MapError::NoTables));
    }

    #[test]
    fn a_corrupt_directory_entry_is_not_followed() {
        let mut pt = PageTables::new(BASE, 8, Target::Vram);
        pt.map(0x1000, 0x1000, Target::Vram, Flags::default()).unwrap();
        // points one table past the last, misaligned, below the pool, at the root's own address
        for bad in [BASE + 5 * 0x1000, BASE + 0x1800, BASE - 0x1000, 0x10] {
            wr64(&mut pt.tables[0], 0, pde(bad, Target::Vram));
            assert_eq!(pt.translate(0x1000), None, "{bad:#x}");
        }
        wr64(&mut pt.tables[0], 0, pde(BASE + 4 * 0x1000, Target::Vram)); // the last real table
        assert_eq!(pt.translate(0x1000), None, "a table with no entries there");
    }

    #[test]
    fn images_come_with_their_addresses() {
        let mut pt = PageTables::new(BASE, 16, Target::Vram);
        pt.map(0x1000, 0x1000, Target::Vram, Flags::default()).unwrap();
        let addrs: Vec<u64> = pt.images().map(|(a, _)| a).collect();
        assert_eq!(addrs, [BASE, BASE + 0x1000, BASE + 0x2000, BASE + 0x3000, BASE + 0x4000]);
        assert_eq!((TABLE_SIZE, VA_BITS, ROOT_ENTRIES, PAGE_SHIFT), (0x1000, 49, 4, 12));
    }

    #[test]
    fn a_huge_page_is_the_first_half_of_the_pd0_entry() {
        let mut pt = PageTables::new(BASE, 16, Target::Vram);
        let va = 0x2_1000_0000u64; // PD0 index 128 of PD1 slot 8
        let pa = 0x9_0020_0000u64;
        pt.map_huge(va, pa, Target::Vram, Flags::default()).unwrap();
        assert_eq!(pt.len(), 4, "PD3 + PD2 + PD1 + PD0, no PT");
        let ix = indices(va);
        let (_, pd0) = pt.images().nth(3).unwrap();
        assert_eq!(rd64(pd0, ix[3] * 16), (pa >> 4) | 1, "the PTE is the FIRST qword");
        assert_eq!(rd64(pd0, ix[3] * 16 + 8), 0, "the small-page half stays empty");
        assert_eq!(pt.translate(va).map(|w| w.0), Some(pa));
        assert_eq!(pt.translate(va + 0x1f_ffff).map(|w| w.0), Some(pa + 0x1f_ffff), "offset inside the page");
        assert_eq!(pt.translate(va + 0x20_0000), None, "the next 2 MiB is not mapped");
        assert_eq!(walk(&pt, va + 0x12_3456).map(|w| w.0), Some(pa + 0x12_3456));
        // host memory, read-only, privileged: the same flag bits as a 4 KiB PTE
        let f = Flags { read_only: true, privileged: true, kind: 0 };
        pt.map_huge(va + 0x20_0000, 0x4000_0000, Target::Host, f).unwrap();
        let (_, e) = pt.translate(va + 0x20_0000).unwrap();
        assert_eq!(e, pte(0x4000_0000, Target::Host, f));
        assert_eq!(e & 0xf, 1 | (2 << 1) | (1 << 3));
    }

    #[test]
    fn huge_and_small_pages_do_not_share_a_pd0_slot() {
        let f = Flags::default();
        let mut pt = PageTables::new(BASE, 16, Target::Vram);
        pt.map(0x40_1000, 0x5000, Target::Vram, f).unwrap();
        assert_eq!(pt.map_huge(0x40_0000, 0x20_0000, Target::Vram, f), Err(MapError::Overlap));
        pt.map_huge(0x60_0000, 0x40_0000, Target::Vram, f).unwrap();
        assert_eq!(pt.map(0x60_1000, 0x5000, Target::Vram, f), Err(MapError::Overlap));
        assert_eq!(pt.map(0x80_1000, 0x5000, Target::Vram, f), Ok(()), "the next slot is free");
        assert_eq!(pt.map_huge(0x60_0000, 0x80_0000, Target::Vram, f), Err(MapError::AlreadyMapped));
        assert_eq!(pt.translate(0x40_1000).map(|w| w.0), Some(0x5000), "the refused ones changed nothing");
        assert_eq!(pt.translate(0x60_1000).map(|w| w.0), Some(0x40_1000));
    }

    #[test]
    fn huge_page_refusals_and_ranges() {
        let f = Flags::default();
        let mut pt = PageTables::new(BASE, 5, Target::Vram);
        assert_eq!(pt.map_huge(0x1000, 0x20_0000, Target::Vram, f), Err(MapError::Unaligned));
        assert_eq!(pt.map_huge(0x20_0000, 0x20_1000, Target::Vram, f), Err(MapError::Unaligned));
        assert_eq!(pt.map_huge(1 << 49, 0x20_0000, Target::Vram, f), Err(MapError::OutOfRange));
        assert_eq!(pt.map_huge_range(0x20_0000, 0x20_0000, 0x30_0000, Target::Vram, f), Err(MapError::Unaligned));
        assert_eq!(pt.len(), 1, "refusals allocate nothing");
        pt.map_huge_range(0x20_0000, 0x80_0000, 3 * HUGE_PAGE, Target::Vram, f).unwrap();
        assert_eq!(pt.len(), 4, "three pages share one PD0");
        for i in 0..3u64 {
            assert_eq!(pt.translate(0x20_0000 + i * HUGE_PAGE + 0x10).map(|w| w.0), Some(0x80_0010 + i * HUGE_PAGE));
        }
        assert_eq!(pt.translate(0x20_0000 + 3 * HUGE_PAGE), None);
        // needs one table more than the pool has
        assert_eq!(PageTables::new(BASE, 3, Target::Vram).map_huge(0x20_0000, 0x20_0000, Target::Vram, f), Err(MapError::NoTables));
        assert_eq!(HUGE_PAGE, 0x20_0000);
    }

    #[test]
    fn unmap_removes_a_huge_page_whole() {
        let f = Flags::default();
        let mut pt = PageTables::new(BASE, 8, Target::Vram);
        pt.map_huge(0x20_0000, 0x40_0000, Target::Vram, f).unwrap();
        pt.map_huge(0x40_0000, 0x60_0000, Target::Vram, f).unwrap();
        assert!(pt.unmap(0x20_4000), "any address inside the page");
        assert_eq!(pt.translate(0x20_0000), None);
        assert_eq!(pt.translate(0x3f_ffff), None);
        assert_eq!(pt.translate(0x40_0000).map(|w| w.0), Some(0x60_0000), "the neighbour stays");
        assert!(!pt.unmap(0x20_0000), "already gone");
        pt.map_huge(0x20_0000, 0x80_0000, Target::Vram, f).unwrap();
        assert_eq!(pt.translate(0x20_0000).map(|w| w.0), Some(0x80_0000));
        // a 4 KiB page next to it is still unmapped as before
        pt.map(0x60_0000, 0x1000, Target::Vram, f).unwrap();
        assert!(pt.unmap(0x60_0000));
        assert_eq!(pt.translate(0x60_0000), None);
    }
}
