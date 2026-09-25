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
    /// Physical address of the entry itself, for `set_pat_index_range`
    /// to rewrite it in place.
    pub entry_phys: u64,
    /// Virtual address of the first byte the leaf maps.
    pub virt_base: u64,
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
            return Some(Leaf {
                phys: base + (v & (size - 1)),
                entry,
                page_size: size,
                entry_phys: table + idx * 8,
                virt_base: v & !(size - 1),
            });
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

// ── Reprogramming the PAT (phase 2 of docs/fb/wc-shadow-plan.md) ────────

/// What `program_pat` did, kept for `/proc/fbinfo` — on the serial-less
/// target that file is the only place the outcome can be read.
#[derive(Clone, Copy)]
pub enum PatProgram {
    /// `IA32_PAT` rewritten and read back equal to `after`.
    Programmed { before: u64, after: u64 },
    /// Entry `PAT_WC_INDEX` was already WC (firmware or an earlier call).
    AlreadyWc { pat: u64 },
    /// `CPUID.01H:EDX[16]` says there is no PAT.
    Unsupported,
    /// A live paging-structure entry selects `PAT_WC_INDEX`; changing its
    /// meaning would silently retype that mapping, so nothing was written.
    Blocked { user: hal::memtype::PatIndexUser, pat: u64 },
    /// `wrmsr` went through but the read-back differs.
    Mismatch { wrote: u64, read: u64 },
}

impl core::fmt::Display for PatProgram {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            PatProgram::Programmed { before, after } => {
                write!(f, "programmed, entry 1 = WC ({:#018x} -> {:#018x})", before, after)
            }
            PatProgram::AlreadyWc { pat } => write!(f, "entry 1 already WC ({:#018x})", pat),
            PatProgram::Unsupported => write!(f, "not programmed: CPU has no PAT"),
            PatProgram::Blocked { user, pat } => write!(
                f,
                "not programmed: level-{} {} entry {:#x} at virt {:#x} already selects index 1 ({:#018x} kept)",
                user.level,
                if user.leaf { "leaf" } else { "table" },
                user.entry,
                user.virt,
                pat,
            ),
            PatProgram::Mismatch { wrote, read } => {
                write!(f, "WRITE MISMATCH: wrote {:#018x}, read back {:#018x}", wrote, read)
            }
        }
    }
}

static PAT_PROGRAM: spin::Once<PatProgram> = spin::Once::new();

/// The outcome of the boot-time `program_pat`, if it has run.
pub fn pat_program_status() -> Option<PatProgram> {
    PAT_PROGRAM.get().copied()
}

fn cpu_has_pat() -> bool {
    // `cpuid` leaf 1 exists on every x86-64 processor.
    core::arch::x86_64::__cpuid(1).edx & (1 << 16) != 0
}

/// Make PAT entry `hal::memtype::PAT_WC_INDEX` (1: `PWT` without `PCD`)
/// write-combining, leaving the other seven entries alone.
///
/// No mapping changes type here: nothing is mapped with index 1, which is
/// checked first by walking the live kernel page table (leaves *and*
/// tables, and CR3) — if anything is, the MSR is left as it was and the
/// reason is recorded. Phase 3 is what points the framebuffer at index 1.
///
/// Must run before any process exists (every address space is cloned from
/// the kernel's, so the walk covers them all) and before any later code
/// could map `PWT`-only. BSP only, once: it *decides* the PAT. Every CPU —
/// the BSP included — then gets the decided value from `init_this_cpu`
/// (the SDM requires the PAT to be identical on all of them: a CPU left at
/// the reset value would see the WC framebuffer mapping as write-through).
///
/// The sequence is the SDM's (vol. 3A §11.11.8, which §11.12.4 points to
/// for the PAT): caches off in no-fill mode, write back and invalidate,
/// flush the TLB including global entries, write, flush both again, caches
/// back on. With interrupts off from the first `CR0` write to the last: a
/// tick taken with `CR0.CD=1` runs its whole handler uncached, and a hang
/// here would leave no trace at all on a machine with no serial.
pub fn program_pat() -> PatProgram {
    let r = program_pat_inner();
    PAT_PROGRAM.call_once(|| r);
    if cpu_has_pat() {
        // Whatever the outcome — programmed, already WC, blocked, even a
        // mismatch — this is the PAT the BSP now runs with, so it is the one
        // every other CPU must match.
        PAT_REFERENCE.call_once(|| rdmsr(IA32_PAT));
    }
    r
}

/// The BSP's `IA32_PAT` after `program_pat`; `None` without a PAT.
static PAT_REFERENCE: spin::Once<u64> = spin::Once::new();

/// Give this CPU the PAT `program_pat` settled on the BSP. Per-CPU step of
/// `cpu::init_this_cpu`; writes only if this CPU's value differs.
pub fn init_this_cpu() {
    let Some(&want) = PAT_REFERENCE.get() else { return };
    if rdmsr(IA32_PAT) != want {
        // SAFETY: `want` is the PAT the BSP already runs with — a valid value
        // (it was read back from hardware), and the one every mapping was
        // made against.
        unsafe { write_pat_sdm_sequence(want) };
    }
}

/// Is this CPU's PAT the BSP's?
pub fn verify_this_cpu() -> Result<(), &'static str> {
    if !cpu_has_pat() {
        return Ok(());
    }
    let want = *PAT_REFERENCE.get().ok_or("program_pat has not run")?;
    if rdmsr(IA32_PAT) != want {
        return Err("IA32_PAT differs from the BSP's");
    }
    Ok(())
}

fn program_pat_inner() -> PatProgram {
    use hal::memtype::{MemType, PAT_WC_INDEX};

    if !cpu_has_pat() {
        return PatProgram::Unsupported;
    }
    let before = rdmsr(IA32_PAT);
    if hal::memtype::pat_entry(before, PAT_WC_INDEX) == Some(MemType::Wc) {
        return PatProgram::AlreadyWc { pat: before };
    }

    let offset = super::physical_memory_offset().as_u64();
    let cr3 = x86_64::registers::control::Cr3::read_raw();
    let cr3_raw = cr3.0.start_address().as_u64() | cr3.1 as u64;
    let mut read = |phys: u64| -> u64 {
        // SAFETY: `phys` is a paging-structure entry address derived from
        // CR3 or a present non-leaf entry, and the bootloader's physical
        // window maps all of RAM. Read-only.
        unsafe { core::ptr::read_volatile((offset + phys) as *const u64) }
    };
    if let Some(user) = hal::memtype::find_pat_index_user(cr3_raw, PAT_WC_INDEX, &mut read) {
        return PatProgram::Blocked { user, pat: before };
    }

    let after = hal::memtype::pat_with_entry(before, PAT_WC_INDEX, MemType::Wc)
        .expect("PAT_WC_INDEX is in range and WC is not reserved");
    // SAFETY: `after` differs from the live PAT only in an entry nothing
    // maps with (checked above), and holds no reserved encoding.
    unsafe { write_pat_sdm_sequence(after) };

    let read_back = rdmsr(IA32_PAT);
    if read_back != after {
        return PatProgram::Mismatch { wrote: after, read: read_back };
    }
    PatProgram::Programmed { before, after }
}

/// # Safety
/// `value` must be a valid `IA32_PAT` value (no reserved encodings) and
/// must not change the type of any entry a live mapping uses.
unsafe fn write_pat_sdm_sequence(value: u64) {
    use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};

    #[inline(always)]
    unsafe fn wbinvd() {
        unsafe { core::arch::asm!("wbinvd", options(nostack, preserves_flags)) };
    }

    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let cr0 = Cr0::read();
        let cr4 = Cr4::read();
        let pge = cr4.contains(Cr4Flags::PAGE_GLOBAL);

        // No-fill cache mode: CD=1, NW=0.
        Cr0::write((cr0 | Cr0Flags::CACHE_DISABLE) - Cr0Flags::NOT_WRITE_THROUGH);
        wbinvd();
        // Flush the TLB, global entries included: clearing PGE does that
        // by itself; without PGE a CR3 reload is enough.
        if pge {
            Cr4::write(cr4 - Cr4Flags::PAGE_GLOBAL);
        } else {
            crate::memory::tlb::invalidate_all_this_cpu();
        }

        x86_64::registers::model_specific::Msr::new(IA32_PAT).write(value);

        wbinvd();
        // PGE is still clear here, so a CR3 reload flushes everything.
        crate::memory::tlb::invalidate_all_this_cpu();
        Cr0::write(cr0);
        if pge {
            Cr4::write(cr4);
        }
    });
}

// ── Retyping a mapping (phase 3 of docs/fb/wc-shadow-plan.md) ───────────

/// What `set_pat_index_range` changed.
#[derive(Clone, Copy)]
pub struct Retyped {
    pub pages_4k: usize,
    pub pages_large: usize,
    /// PAT index the first leaf selected before the change.
    pub old_index: u8,
}

/// Why `set_pat_index_range` changed nothing.
#[derive(Clone, Copy, Debug)]
pub enum RetypeError {
    /// Some page of the range has no mapping.
    NotMapped { virt: u64 },
    /// A 2 MiB/1 GiB leaf maps memory outside the range too: retyping it
    /// would retype that memory as well.
    LargeLeafOutside { virt: u64, page_size: u64 },
    BadIndex,
}

impl core::fmt::Display for RetypeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            RetypeError::NotMapped { virt } => write!(f, "{:#x} not mapped", virt),
            RetypeError::LargeLeafOutside { virt, page_size } => write!(
                f,
                "{:#x} is inside a {} KiB page that extends past the range",
                virt,
                page_size / 1024,
            ),
            RetypeError::BadIndex => write!(f, "PAT index out of range"),
        }
    }
}

/// Point every leaf that maps `[virt, virt + len)` at PAT entry `index`,
/// in the live kernel page table, and invalidate those TLB entries.
///
/// All or nothing: the whole range is checked first (every page mapped,
/// no large leaf reaching outside it), and only then written, so a range
/// that fails leaves every entry as it was. The range is rounded out to
/// whole 4 KiB pages.
///
/// Only the leaves change, never the tables above them, and the kernel's
/// lower-level tables are shared by every address space
/// (`OwnedPageTable::new_user` copies the upper entries, not the tables),
/// so a process created later sees the new type too; one that already
/// exists does as well, for the same reason. Single CPU: there is no
/// other TLB to shoot down.
///
/// Ends with `wbinvd`. Moving *to* a non-cacheable type while a line of
/// the range sits in a cache would leave that line to be written back
/// later over whatever the new mapping wrote; the aperture this exists
/// for was UC and has no cached lines, but the function should not rely
/// on what its caller used to be.
pub fn set_pat_index_range(virt: u64, len: u64, index: u8) -> Result<Retyped, RetypeError> {
    if index > 7 {
        return Err(RetypeError::BadIndex);
    }
    let start = virt & !0xFFF;
    let end = (virt + len + 0xFFF) & !0xFFF;

    // Pass 1: check, change nothing.
    let mut v = start;
    let mut out = Retyped { pages_4k: 0, pages_large: 0, old_index: 0 };
    while v < end {
        let leaf = leaf_for(VirtAddr::new(v)).ok_or(RetypeError::NotMapped { virt: v })?;
        if leaf.page_size != 0x1000
            && (leaf.virt_base < start || leaf.virt_base + leaf.page_size > end)
        {
            return Err(RetypeError::LargeLeafOutside { virt: v, page_size: leaf.page_size });
        }
        if v == start {
            let (pat, pcd, pwt) = leaf.cache_bits();
            out.old_index = hal::memtype::pat_index(pat, pcd, pwt);
        }
        if leaf.page_size == 0x1000 { out.pages_4k += 1 } else { out.pages_large += 1 }
        v = leaf.virt_base + leaf.page_size;
    }

    // Pass 2: rewrite each leaf and drop its TLB entry.
    let offset = super::physical_memory_offset().as_u64();
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut v = start;
        while v < end {
            let leaf = leaf_for(VirtAddr::new(v)).expect("checked in pass 1");
            let new = hal::memtype::with_pat_index(leaf.entry, leaf.page_size != 0x1000, index)
                .expect("index checked above");
            // SAFETY: `entry_phys` is the live leaf entry `leaf_for` just
            // walked to, reached through the physical window; only its
            // caching bits change, so it maps the same frame as before.
            unsafe { core::ptr::write_volatile((offset + leaf.entry_phys) as *mut u64, new) };
            crate::memory::tlb::invalidate_kernel_page(VirtAddr::new(leaf.virt_base));
            v = leaf.virt_base + leaf.page_size;
        }
        // SAFETY: `wbinvd` writes back and invalidates caches; no memory
        // is changed from the program's point of view.
        unsafe { core::arch::asm!("wbinvd", options(nostack, preserves_flags)) };
    });
    Ok(out)
}
