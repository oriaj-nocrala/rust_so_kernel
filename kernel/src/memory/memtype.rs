// kernel/src/memory/memtype.rs
//
// Reads the MSRs and page-table bits that decide what memory type a
// mapping actually has, and hands them to `hal::memtype` to decode.
//
// The kernel half of the split: `rdmsr`, `cpuid` and the page-table walk
// live here; every bit of arithmetic that turns those numbers into an
// answer lives in `hal::memtype`, where `cargo test` reaches it.
//
// WHY: the framebuffer console is imperceptibly fast in QEMU and takes
// about a second to clear the screen on the physical machine. The size of
// that gap is decided by whether the framebuffer aperture is uncacheable
// (every store its own bus transaction) or write-combining (sequential
// stores batched into bursts) — and that is a fact about this machine's
// firmware, not something to assume from the code. See `/proc/fbinfo`,
// which is how it gets read on a machine with no serial capture.

use x86_64::registers::control::Cr3;
use x86_64::VirtAddr;

use hal::memtype::{MemType, MtrrDefType, MtrrResolution, VariableMtrr};

const IA32_MTRRCAP: u32 = 0xFE;
const IA32_MTRR_PHYSBASE0: u32 = 0x200;
const IA32_MTRR_DEF_TYPE: u32 = 0x2FF;
const IA32_PAT: u32 = 0x277;

/// Up to this many variable-range MTRR pairs are read. Real processors
/// report 8 or 10; the cap is a guard against a nonsense `MTRRCAP`
/// (`hal::memtype::mtrr_variable_count` clamps to 64, this array is what
/// actually bounds the walk).
const MAX_VARIABLE_MTRRS: usize = 16;

/// One leaf of the live kernel page table: the entry itself, raw, and
/// the size of the page it maps.
#[derive(Clone, Copy)]
pub struct Leaf {
    pub phys: u64,
    /// The raw 64-bit entry, every bit kept. `x86_64::PageTableFlags`
    /// drops bit 12, which is a large page's PAT bit.
    pub entry: u64,
    pub page_size: u64,
}

impl Leaf {
    pub fn size_name(&self) -> &'static str {
        match self.page_size {
            0x1000 => "4K",
            0x20_0000 => "2M",
            _ => "1G",
        }
    }

    /// `(pat, pcd, pwt)`, with the PAT bit read from where this page
    /// size keeps it. See `hal::memtype::leaf_cache_bits`.
    pub fn cache_bits(&self) -> (bool, bool, bool) {
        hal::memtype::leaf_cache_bits(self.entry, self.page_size != 0x1000)
    }
}

/// Walk the live page table (CR3) down to the leaf that maps `virt`.
///
/// Written by hand rather than through `OffsetPageTable::translate`
/// because that API hands back `PageTableFlags`, which has lost a large
/// page's PAT bit by the time the caller sees it. The walk reads the
/// tables through the bootloader's physical-memory window, the same way
/// every mapper in this kernel does.
pub fn leaf_for(virt: VirtAddr) -> Option<Leaf> {
    const PRESENT: u64 = 1;
    const PS: u64 = 1 << 7;
    const ADDR: u64 = 0x000F_FFFF_FFFF_F000;

    let offset = super::physical_memory_offset().as_u64();
    let v = virt.as_u64();
    let (pml4, _) = Cr3::read();
    let mut table = pml4.start_address().as_u64();

    // (index shift, page size mapped by a leaf at this level)
    for (shift, size) in [(39u32, 0u64), (30, 1 << 30), (21, 1 << 21), (12, 1 << 12)] {
        let idx = (v >> shift) & 0x1FF;
        // SAFETY: `table` is the physical address of a paging structure
        // taken from CR3 or from a present non-leaf entry above it, and the
        // physical window maps all of RAM. Read-only.
        let entry = unsafe { core::ptr::read_volatile((offset + table + idx * 8) as *const u64) };
        if entry & PRESENT == 0 {
            return None;
        }
        let leaf = shift == 12 || (size != 0 && entry & PS != 0);
        if leaf {
            // A large page's address field starts above its own PAT bit
            // (bit 12), so mask to the page size, not just to 4 KiB.
            let base = entry & ADDR & !(size - 1);
            return Some(Leaf { phys: base + (v & (size - 1)), entry, page_size: size });
        }
        table = entry & ADDR;
    }
    None
}

/// Everything `/proc/fbinfo` needs to say about one mapping's memory type.
pub struct MemTypeReport {
    pub phys: Option<u64>,
    /// Page size of the live mapping, or `None` if it is not mapped.
    pub page_size: Option<&'static str>,
    /// Leaf-entry bits that select the PAT entry, as read from the live
    /// mapping, with the PAT bit taken from where that page size keeps it.
    pub pat_bit: bool,
    pub pcd: bool,
    pub pwt: bool,
    pub pat_msr: u64,
    pub pat_index: u8,
    pub pat_type: Option<MemType>,
    pub pat_has_wc: bool,
    pub mtrrcap: u64,
    pub def_type: u64,
    pub mtrr_wc_supported: bool,
    pub mtrr: Option<MtrrResolution>,
    pub max_phys_addr_bits: u8,
    /// Every valid variable range, for the cases where the summary is not
    /// enough — an operator reading this off a photographed screen cannot
    /// come back and ask a follow-up question.
    pub ranges: [Option<(u64, u64, MemType)>; MAX_VARIABLE_MTRRS],
    pub range_count: usize,
}

fn rdmsr(msr: u32) -> u64 {
    // SAFETY: all four MSRs read here are architectural and present on
    // every x86-64 processor that supports MTRRs/PAT — both are required
    // features of the long mode this kernel already runs in.
    unsafe { x86_64::registers::model_specific::Msr::new(msr).read() }
}

/// Physical-address width from `CPUID.80000008H:EAX[7:0]`, defaulting to
/// 36 (the architectural minimum) when the leaf is unavailable — the only
/// consequence of guessing low is that MTRR base/mask comparisons ignore
/// bits no real range uses anyway.
fn max_phys_addr_bits() -> u8 {
    // SAFETY: `cpuid` is unprivileged and has no side effects; the
    // extended-leaf maximum is checked before trusting leaf 0x80000008.
    unsafe {
        let max_ext = core::arch::x86_64::__cpuid(0x8000_0000).eax;
        if max_ext < 0x8000_0008 {
            return 36;
        }
        (core::arch::x86_64::__cpuid(0x8000_0008).eax & 0xFF) as u8
    }
}

/// Resolve the effective memory-type inputs for the kernel mapping at
/// `virt`: its physical address and caching bits, the PAT, and whatever
/// MTRR covers the physical page.
pub fn report_for(virt: VirtAddr) -> MemTypeReport {
    let leaf = leaf_for(virt);
    let phys = leaf.map(|l| l.phys);
    let (pat_bit, pcd, pwt) = leaf.map(|l| l.cache_bits()).unwrap_or((false, false, false));

    let pat_msr = rdmsr(IA32_PAT);
    let pat_index = hal::memtype::pat_index(pat_bit, pcd, pwt);

    let mtrrcap = rdmsr(IA32_MTRRCAP);
    let def_type_raw = rdmsr(IA32_MTRR_DEF_TYPE);
    let def_type = MtrrDefType(def_type_raw);
    let bits = max_phys_addr_bits();

    let count = hal::memtype::mtrr_variable_count(mtrrcap).min(MAX_VARIABLE_MTRRS);
    let mut entries = [VariableMtrr { base: 0, mask: 0 }; MAX_VARIABLE_MTRRS];
    let mut ranges = [None; MAX_VARIABLE_MTRRS];
    for i in 0..count {
        let base = rdmsr(IA32_MTRR_PHYSBASE0 + (i as u32) * 2);
        let mask = rdmsr(IA32_MTRR_PHYSBASE0 + (i as u32) * 2 + 1);
        entries[i] = VariableMtrr { base, mask };
        if entries[i].valid() {
            ranges[i] = Some((base, mask, entries[i].mem_type()));
        }
    }

    MemTypeReport {
        phys,
        page_size: leaf.map(|l| l.size_name()),
        pat_bit,
        pcd,
        pwt,
        pat_msr,
        pat_index,
        pat_type: hal::memtype::pat_entry(pat_msr, pat_index),
        pat_has_wc: hal::memtype::pat_has_wc(pat_msr),
        mtrrcap,
        def_type: def_type_raw,
        mtrr_wc_supported: hal::memtype::mtrr_wc_supported(mtrrcap),
        mtrr: phys.map(|p| hal::memtype::mtrr_type_for(p, def_type, &entries[..count], bits)),
        max_phys_addr_bits: bits,
        ranges,
        range_count: count,
    }
}
