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

use x86_64::structures::paging::{Page, Size4KiB};
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

/// Everything `/proc/fbinfo` needs to say about one mapping's memory type.
pub struct MemTypeReport {
    pub phys: Option<u64>,
    /// PTE bits that select the PAT entry, as read from the live mapping.
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
    let page = Page::<Size4KiB>::containing_address(virt);
    let table = super::page_table_manager::OwnedPageTable::from_current();
    // SAFETY: reads the live kernel page table through the bootloader's
    // physical window, exactly as every other caller of this method does.
    let mapping = unsafe { table.translate_with_flags(page) };

    let (phys, flags) = match mapping {
        Some((frame, flags)) => (
            Some(frame.start_address().as_u64() + (virt.as_u64() & 0xFFF)),
            flags.bits(),
        ),
        None => (None, 0),
    };

    // 4 KiB PTE: bit 7 is PAT (where a large page keeps PS), bit 4 PCD,
    // bit 3 PWT.
    let pat_bit = flags & (1 << 7) != 0;
    let pcd = flags & (1 << 4) != 0;
    let pwt = flags & (1 << 3) != 0;

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
