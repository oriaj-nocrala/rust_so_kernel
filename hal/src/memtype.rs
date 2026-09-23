//! x86 memory-type decoding: MTRRs and the PAT.
//!
//! Pure arithmetic over MSR values somebody else read — no `rdmsr` here,
//! the same split every other module in this crate uses. The kernel side
//! (`kernel/src/memory/memtype.rs`) reads `IA32_MTRRCAP`,
//! `IA32_MTRR_DEF_TYPE`, the variable-range pairs and `IA32_PAT`, then
//! hands them here.
//!
//! WHY THIS EXISTS
//! ───────────────
//! The framebuffer console is fast in QEMU (the framebuffer is host RAM)
//! and slow on the physical machine this kernel is brought up on, where it
//! lives behind PCIe. How slow depends entirely on the *effective memory
//! type* of that mapping: write-combining batches sequential stores into
//! burst transactions, uncacheable does not — every store becomes its own
//! bus transaction regardless of size.
//!
//! That type is not a constant and not something to assume. The
//! bootloader maps the framebuffer with no PCD/PWT bits (PAT index 0), and
//! the firmware's MTRRs decide what index 0 actually means for that
//! physical range. This module turns those raw MSR values into an answer
//! that can be printed and read off a screen — which, on a machine with no
//! serial capture, is the only way to find out at all.
//!
//! Deliberately **not** here: the MTRR × PAT combination table (SDM
//! vol. 3A table 11-7). The two inputs are reported separately and the
//! ground truth is a measured write throughput, because the one thing
//! that is genuinely easy to get wrong from memory is exactly that table.

/// An x86 memory type, as encoded in an MTRR type field or a PAT entry.
///
/// `UcMinus` (7) only ever appears in the PAT — MTRRs have no such type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemType {
    /// 0 — uncacheable. Every store is its own bus transaction.
    Uc,
    /// 1 — write-combining. Sequential stores batch into bursts.
    Wc,
    /// 4 — write-through.
    Wt,
    /// 5 — write-protected.
    Wp,
    /// 6 — write-back. What ordinary RAM is.
    Wb,
    /// 7 — UC-, PAT only: uncacheable, but a WC MTRR still wins.
    UcMinus,
    /// A reserved encoding (2, 3, or anything above 7). Carries the raw
    /// value rather than being discarded — a reserved type in a live MSR
    /// means something is wrong, and which value it was is the clue.
    Reserved(u8),
}

impl MemType {
    pub fn from_raw(v: u8) -> Self {
        match v {
            0 => MemType::Uc,
            1 => MemType::Wc,
            4 => MemType::Wt,
            5 => MemType::Wp,
            6 => MemType::Wb,
            7 => MemType::UcMinus,
            other => MemType::Reserved(other),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            MemType::Uc => "UC",
            MemType::Wc => "WC",
            MemType::Wt => "WT",
            MemType::Wp => "WP",
            MemType::Wb => "WB",
            MemType::UcMinus => "UC-",
            MemType::Reserved(_) => "reserved",
        }
    }
}

// ── PAT ──────────────────────────────────────────────────────────────────

/// The PAT index a 4 KiB page's flags select: `PAT<<2 | PCD<<1 | PWT`.
///
/// For a 4 KiB page the PAT bit is bit 7 of the PTE (where a large page
/// would keep its PS bit); PCD is bit 4 and PWT bit 3. The caller passes
/// them already extracted, since `x86_64`'s `PageTableFlags` names them.
pub fn pat_index(pat: bool, pcd: bool, pwt: bool) -> u8 {
    ((pat as u8) << 2) | ((pcd as u8) << 1) | (pwt as u8)
}

/// The caching bits of a *leaf* paging-structure entry, as
/// `(pat, pcd, pwt)`, from the raw 64-bit entry.
///
/// `large` is true for a leaf that maps a 2 MiB or 1 GiB page (a PDE or
/// PDPTE with PS set). The PAT bit moves with the page size: bit 7 in a
/// 4 KiB PTE, bit 12 in a large-page entry, because bit 7 is PS there.
/// PCD (bit 4) and PWT (bit 3) are in the same place at every level.
///
/// Takes the raw entry rather than `x86_64::PageTableFlags` because that
/// type builds with `from_bits_truncate` and has no bit 12. A large page's
/// PAT bit is dropped before the caller sees it, and bit 7 reads as set,
/// since it is PS. Reading the PAT bit from bit 7 through that type
/// reports PAT=1 for every large page. `/proc/fbinfo` got it wrong in
/// exactly that way until this existed.
pub fn leaf_cache_bits(entry: u64, large: bool) -> (bool, bool, bool) {
    let pat_bit = if large { 12 } else { 7 };
    (
        entry & (1 << pat_bit) != 0,
        entry & (1 << 4) != 0,
        entry & (1 << 3) != 0,
    )
}

/// Decode one of `IA32_PAT`'s eight entries.
///
/// Returns `None` for an index above 7 rather than wrapping — an
/// out-of-range index is a caller bug, not a memory type.
pub fn pat_entry(pat_msr: u64, index: u8) -> Option<MemType> {
    if index > 7 {
        return None;
    }
    Some(MemType::from_raw(((pat_msr >> (index * 8)) & 0xFF) as u8))
}

/// The PAT's power-on value: UC-/WT/WB and no WC entry anywhere.
///
/// Worth naming because it is the reason "just map it write-combining"
/// is not a one-line change: with this PAT there is no flag combination
/// that selects WC at all, so enabling it means reprogramming the MSR
/// first.
pub const PAT_RESET_VALUE: u64 = 0x0007_0406_0007_0406;

/// Whether any of the eight PAT entries encodes WC — i.e. whether a
/// write-combining mapping is reachable at all without reprogramming
/// `IA32_PAT`.
pub fn pat_has_wc(pat_msr: u64) -> bool {
    (0..8).any(|i| pat_entry(pat_msr, i) == Some(MemType::Wc))
}

/// The PAT entry this kernel reprograms to WC: `PAT=0, PCD=0, PWT=1`.
///
/// Index 1 because it is the one Linux uses, and because it is selected
/// without the PAT bit — which lives in bit 7 of a 4 KiB PTE but bit 12 of
/// a large-page entry — so the same two flag bits (`WRITE_THROUGH`, no
/// `NO_CACHE`) mean WC at every page size. The price: after
/// `program_pat`, **any mapping with `PWT` set and `PCD` clear is WC, not
/// WT.** Nothing in this kernel maps that way (`memory::mmio` sets both,
/// index 3), and `program_pat` refuses to run if a live entry already
/// does — see `find_pat_index_user`.
pub const PAT_WC_INDEX: u8 = 1;

/// `pat_msr` with entry `index` replaced by `ty`, the other seven left
/// exactly as they were.
///
/// `None` for an index above 7 or a reserved type: `wrmsr` of a reserved
/// encoding into `IA32_PAT` is a #GP, and on a machine with no serial
/// capture a #GP this early in boot is a black screen.
///
/// Only the one entry changes, rather than writing Linux's whole layout
/// (`WB WC UC- UC WB WP UC- WT`): entries 5 and 7 would also move there
/// (WT→WP, UC→WT), and nothing here needs them, so rewriting them would
/// only add a way for an existing `PAT=1` mapping to change type.
pub fn pat_with_entry(pat_msr: u64, index: u8, ty: MemType) -> Option<u64> {
    if index > 7 {
        return None;
    }
    let raw = match ty {
        MemType::Uc => 0u64,
        MemType::Wc => 1,
        MemType::Wt => 4,
        MemType::Wp => 5,
        MemType::Wb => 6,
        MemType::UcMinus => 7,
        MemType::Reserved(_) => return None,
    };
    let shift = index as u64 * 8;
    Some((pat_msr & !(0xFF << shift)) | (raw << shift))
}

/// A paging-structure entry that selects a given PAT index — the reason
/// `program_pat` declines to change that entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatIndexUser {
    /// Canonical virtual address of the first byte the entry covers. For
    /// `level == 5` (CR3 itself) this is 0.
    pub virt: u64,
    /// 1 = PT (4 KiB leaf), 2 = PD, 3 = PDPT, 4 = PML4, 5 = CR3.
    pub level: u8,
    /// The raw entry (or CR3 value).
    pub entry: u64,
    /// Whether the entry maps a page (as opposed to pointing at a table).
    pub leaf: bool,
}

/// Walk a 4-level page table and return the first entry — leaf *or not* —
/// whose caching bits select PAT entry `index`.
///
/// Non-leaf entries matter too: the processor reads the next-level table
/// with the memory type its parent's `PCD`/`PWT` select (PAT bit taken as
/// 0), and CR3's own `PCD`/`PWT` do the same for the PML4. Changing the
/// meaning of an index those use would change how page tables are cached,
/// which is worse than changing a data mapping.
///
/// `cr3` is the raw register. `read(phys)` returns the `u64` at a
/// physical address — the kernel reads through its physical-memory
/// window, host tests through a map. Pure otherwise: no allocation, and
/// the recursion depth is the paging depth, so a malformed table cannot
/// make it unbounded (a table that points back at an ancestor is only
/// walked again at a lower level, at most four deep).
pub fn find_pat_index_user(
    cr3: u64,
    index: u8,
    read: &mut dyn FnMut(u64) -> u64,
) -> Option<PatIndexUser> {
    // CR3: PWT bit 3, PCD bit 4 — same positions as in an entry.
    if nonleaf_pat_index(cr3) == index {
        return Some(PatIndexUser { virt: 0, level: 5, entry: cr3, leaf: false });
    }
    walk_table(cr3 & ADDR_MASK, 4, 0, index, read)
}

const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

fn nonleaf_pat_index(entry: u64) -> u8 {
    pat_index(false, entry & (1 << 4) != 0, entry & (1 << 3) != 0)
}

fn walk_table(
    table: u64,
    level: u8,
    virt_base: u64,
    index: u8,
    read: &mut dyn FnMut(u64) -> u64,
) -> Option<PatIndexUser> {
    let shift = 12 + 9 * (level as u64 - 1);
    for i in 0..512u64 {
        let entry = read(table + i * 8);
        if entry & 1 == 0 {
            continue;
        }
        let virt = canonical(virt_base | (i << shift));
        // PS (bit 7) makes a PDPTE/PDE a leaf; in a PML4E it is reserved
        // and in a PTE it is the PAT bit, so it only counts at 2 and 3.
        let leaf = level == 1 || ((level == 2 || level == 3) && entry & (1 << 7) != 0);
        let selected = if leaf {
            let (pat, pcd, pwt) = leaf_cache_bits(entry, level != 1);
            pat_index(pat, pcd, pwt)
        } else {
            nonleaf_pat_index(entry)
        };
        if selected == index {
            return Some(PatIndexUser { virt, level, entry, leaf });
        }
        if !leaf {
            if let Some(hit) = walk_table(entry & ADDR_MASK, level - 1, virt, index, read) {
                return Some(hit);
            }
        }
    }
    None
}

/// Sign-extend bit 47, as a 4-level virtual address must be.
fn canonical(v: u64) -> u64 {
    if v & (1 << 47) != 0 { v | 0xFFFF_0000_0000_0000 } else { v & 0x0000_FFFF_FFFF_FFFF }
}

// ── MTRRs ────────────────────────────────────────────────────────────────

/// One variable-range MTRR, as the raw `IA32_MTRR_PHYSBASE_n` /
/// `IA32_MTRR_PHYSMASK_n` pair.
#[derive(Debug, Clone, Copy)]
pub struct VariableMtrr {
    pub base: u64,
    pub mask: u64,
}

impl VariableMtrr {
    /// Bit 11 of PHYSMASK — an invalid entry matches nothing, whatever
    /// its base says.
    pub fn valid(&self) -> bool {
        self.mask & (1 << 11) != 0
    }

    pub fn mem_type(&self) -> MemType {
        MemType::from_raw((self.base & 0xFF) as u8)
    }

    /// Does this range cover `phys`? The hardware's own test:
    /// `(addr & mask) == (base & mask)`, both truncated to the physical
    /// address bits (63:12, below `max_phys_addr_bits`).
    pub fn matches(&self, phys: u64, max_phys_addr_bits: u8) -> bool {
        if !self.valid() {
            return false;
        }
        let addr_mask = phys_addr_mask(max_phys_addr_bits);
        let m = self.mask & addr_mask;
        (phys & m) == (self.base & addr_mask & m)
    }
}

/// Bits 63:12 truncated to `bits` physical address bits — the mask the
/// hardware applies to both PHYSBASE and PHYSMASK.
fn phys_addr_mask(bits: u8) -> u64 {
    let bits = bits.clamp(36, 52);
    (((1u64 << bits) - 1)) & !0xFFFu64
}

/// `IA32_MTRR_DEF_TYPE` (MSR 0x2FF), decoded.
#[derive(Debug, Clone, Copy)]
pub struct MtrrDefType(pub u64);

impl MtrrDefType {
    /// Bit 11 — MTRRs enabled at all. When clear, the whole physical
    /// address space is UC and the default type field is ignored.
    pub fn enabled(&self) -> bool {
        self.0 & (1 << 11) != 0
    }
    /// Bit 10 — fixed-range MTRRs enabled (they only cover 0-1 MiB).
    pub fn fixed_enabled(&self) -> bool {
        self.0 & (1 << 10) != 0
    }
    pub fn default_type(&self) -> MemType {
        MemType::from_raw((self.0 & 0xFF) as u8)
    }
}

/// What the MTRRs say about `phys`, plus how they said it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MtrrResolution {
    /// MTRRs are disabled (`DEF_TYPE.E == 0`): everything is UC.
    Disabled,
    /// No variable range covered the address; the default type applies.
    Default(MemType),
    /// Exactly one range covered it, or several that agree.
    Matched(MemType),
    /// Several ranges covered it and disagreed. `resolved` is what the
    /// hardware does (UC wins over everything; WT wins over WB), and
    /// `conflicting` is true when the overlap is one the SDM leaves
    /// undefined — worth printing rather than hiding, since a machine in
    /// that state is one whose firmware is doing something unusual.
    Overlapping { resolved: MemType, conflicting: bool },
}

impl MtrrResolution {
    pub fn mem_type(&self) -> MemType {
        match *self {
            MtrrResolution::Disabled => MemType::Uc,
            MtrrResolution::Default(t) => t,
            MtrrResolution::Matched(t) => t,
            MtrrResolution::Overlapping { resolved, .. } => resolved,
        }
    }
}

/// Resolve the MTRR memory type covering `phys`.
///
/// Only the variable ranges are consulted: the fixed ones cover 0-1 MiB
/// and no PCI memory BAR lands there. Overlap follows the SDM's rules —
/// UC beats everything, WT beats WB, anything else overlapping is
/// undefined and reported as such.
pub fn mtrr_type_for(
    phys: u64,
    def_type: MtrrDefType,
    entries: &[VariableMtrr],
    max_phys_addr_bits: u8,
) -> MtrrResolution {
    if !def_type.enabled() {
        return MtrrResolution::Disabled;
    }

    let mut found: Option<MemType> = None;
    let mut conflicting = false;

    for e in entries {
        if !e.matches(phys, max_phys_addr_bits) {
            continue;
        }
        let t = e.mem_type();
        found = Some(match found {
            None => t,
            Some(prev) if prev == t => prev,
            Some(prev) => match (prev, t) {
                (MemType::Uc, _) | (_, MemType::Uc) => MemType::Uc,
                (MemType::Wt, MemType::Wb) | (MemType::Wb, MemType::Wt) => MemType::Wt,
                _ => {
                    conflicting = true;
                    MemType::Uc
                }
            },
        });
    }

    match found {
        None => MtrrResolution::Default(def_type.default_type()),
        Some(t) => {
            // "Matched" means one range, or several agreeing; the moment
            // two disagreed, say so — an operator reading this off a
            // screen needs to know the answer came from a tie-break.
            let mut matches = 0usize;
            let mut all_same = true;
            let mut first: Option<MemType> = None;
            for e in entries {
                if e.matches(phys, max_phys_addr_bits) {
                    matches += 1;
                    match first {
                        None => first = Some(e.mem_type()),
                        Some(f) if f != e.mem_type() => all_same = false,
                        _ => {}
                    }
                }
            }
            if matches > 1 && !all_same {
                MtrrResolution::Overlapping { resolved: t, conflicting }
            } else {
                MtrrResolution::Matched(t)
            }
        }
    }
}

/// `IA32_MTRRCAP`'s variable-range count (bits 7:0), clamped to what any
/// real implementation can have — a nonsense VCNT from a bad read must
/// not make the caller walk hundreds of MSRs that fault.
pub fn mtrr_variable_count(mtrrcap: u64) -> usize {
    ((mtrrcap & 0xFF) as usize).min(64)
}

/// `IA32_MTRRCAP` bit 10 — whether the processor supports the WC memory
/// type at all.
pub fn mtrr_wc_supported(mtrrcap: u64) -> bool {
    mtrrcap & (1 << 10) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_4k_pte_keeps_its_pat_bit_in_bit_7() {
        // PRESENT | PWT | PAT(7)
        assert_eq!(leaf_cache_bits(0x1 | 0x8 | 0x80, false), (true, false, true));
        // Bit 12 means nothing in a 4 KiB PTE: it is an address bit.
        assert_eq!(leaf_cache_bits(0x1 | 0x1000, false), (false, false, false));
    }

    #[test]
    fn a_large_page_keeps_its_pat_bit_in_bit_12_and_bit_7_is_ps() {
        // PRESENT | PS(7): PS set, PAT clear. The wrong reading says PAT=1.
        assert_eq!(leaf_cache_bits(0x1 | 0x80, true), (false, false, false));
        // PRESENT | PCD | PS | PAT(12)
        assert_eq!(leaf_cache_bits(0x1 | 0x10 | 0x80 | 0x1000, true), (true, true, false));
    }

    #[test]
    fn pcd_and_pwt_do_not_move_with_page_size() {
        for large in [false, true] {
            assert_eq!(leaf_cache_bits(0x18, large), (false, true, true));
        }
    }

    #[test]
    fn memtype_raw_encodings_round_trip_to_their_names() {
        assert_eq!(MemType::from_raw(0), MemType::Uc);
        assert_eq!(MemType::from_raw(1), MemType::Wc);
        assert_eq!(MemType::from_raw(4), MemType::Wt);
        assert_eq!(MemType::from_raw(5), MemType::Wp);
        assert_eq!(MemType::from_raw(6), MemType::Wb);
        assert_eq!(MemType::from_raw(7), MemType::UcMinus);
        assert_eq!(MemType::from_raw(2), MemType::Reserved(2));
        assert_eq!(MemType::from_raw(3), MemType::Reserved(3));
        assert_eq!(MemType::Wc.name(), "WC");
        assert_eq!(MemType::UcMinus.name(), "UC-");
    }

    #[test]
    fn pat_index_packs_pat_pcd_pwt_in_that_bit_order() {
        assert_eq!(pat_index(false, false, false), 0);
        assert_eq!(pat_index(false, false, true), 1);
        assert_eq!(pat_index(false, true, false), 2);
        assert_eq!(pat_index(false, true, true), 3);
        assert_eq!(pat_index(true, false, false), 4);
        assert_eq!(pat_index(true, true, true), 7);
    }

    #[test]
    fn reset_pat_decodes_to_the_documented_wb_wt_ucminus_uc_pattern() {
        let p = PAT_RESET_VALUE;
        assert_eq!(pat_entry(p, 0), Some(MemType::Wb));
        assert_eq!(pat_entry(p, 1), Some(MemType::Wt));
        assert_eq!(pat_entry(p, 2), Some(MemType::UcMinus));
        assert_eq!(pat_entry(p, 3), Some(MemType::Uc));
        assert_eq!(pat_entry(p, 4), Some(MemType::Wb));
        assert_eq!(pat_entry(p, 5), Some(MemType::Wt));
        assert_eq!(pat_entry(p, 6), Some(MemType::UcMinus));
        assert_eq!(pat_entry(p, 7), Some(MemType::Uc));
    }

    #[test]
    fn pat_index_out_of_range_is_none_not_a_wrapped_entry() {
        assert_eq!(pat_entry(PAT_RESET_VALUE, 8), None);
        assert_eq!(pat_entry(PAT_RESET_VALUE, 255), None);
    }

    #[test]
    fn reset_pat_has_no_wc_entry_which_is_why_wc_needs_reprogramming() {
        assert!(!pat_has_wc(PAT_RESET_VALUE));
        // Linux's arrangement: entry 1 becomes WC.
        let linux_like = (PAT_RESET_VALUE & !0xFF00) | (0x01 << 8);
        assert!(pat_has_wc(linux_like));
        assert_eq!(pat_entry(linux_like, 1), Some(MemType::Wc));
    }

    #[test]
    fn wc_at_index_1_changes_that_entry_and_leaves_the_other_seven() {
        let p = pat_with_entry(PAT_RESET_VALUE, PAT_WC_INDEX, MemType::Wc).unwrap();
        // WB WC UC- UC WB WT UC- UC — the reset layout with entry 1 as WC.
        assert_eq!(p, 0x0007_0406_0007_0106);
        assert_eq!(pat_entry(p, 1), Some(MemType::Wc));
        for i in [0u8, 2, 3, 4, 5, 6, 7] {
            assert_eq!(pat_entry(p, i), pat_entry(PAT_RESET_VALUE, i), "entry {}", i);
        }
        assert!(pat_has_wc(p));
    }

    #[test]
    fn pat_with_entry_round_trips_every_real_type_at_every_index() {
        let types = [MemType::Uc, MemType::Wc, MemType::Wt, MemType::Wp, MemType::Wb, MemType::UcMinus];
        for i in 0..8u8 {
            for t in types {
                let p = pat_with_entry(PAT_RESET_VALUE, i, t).unwrap();
                assert_eq!(pat_entry(p, i), Some(t));
            }
        }
    }

    #[test]
    fn pat_with_entry_refuses_reserved_types_and_out_of_range_indices() {
        assert_eq!(pat_with_entry(PAT_RESET_VALUE, 1, MemType::Reserved(2)), None);
        assert_eq!(pat_with_entry(PAT_RESET_VALUE, 1, MemType::Reserved(3)), None);
        assert_eq!(pat_with_entry(PAT_RESET_VALUE, 8, MemType::Wc), None);
    }

    // ── find_pat_index_user ──────────────────────────────────────────

    use alloc::collections::BTreeMap;

    const P: u64 = 1; // present
    const PWT: u64 = 1 << 3;
    const PCD: u64 = 1 << 4;
    const PS: u64 = 1 << 7;

    /// Fake physical memory holding page tables; absent words read 0.
    struct Mem(BTreeMap<u64, u64>);
    impl Mem {
        fn new() -> Self { Mem(BTreeMap::new()) }
        fn set(&mut self, table: u64, idx: u64, entry: u64) { self.0.insert(table + idx * 8, entry); }
        fn find(&self, cr3: u64, index: u8) -> Option<PatIndexUser> {
            find_pat_index_user(cr3, index, &mut |a| *self.0.get(&a).unwrap_or(&0))
        }
    }

    const PML4: u64 = 0x1000;
    const PDPT: u64 = 0x2000;
    const PD: u64 = 0x3000;
    const PT: u64 = 0x4000;

    /// A higher-half chain PML4[511] → PDPT[1] → PD[2] → PT, all
    /// plain (index 0) and each PT entry a WB 4 KiB page.
    fn chain() -> Mem {
        let mut m = Mem::new();
        m.set(PML4, 511, PDPT | P);
        m.set(PDPT, 1, PD | P);
        m.set(PD, 2, PT | P);
        for i in 0..4 {
            m.set(PT, i, 0x10_0000 + i * 0x1000 | P);
        }
        m
    }

    #[test]
    fn a_plain_table_has_no_user_of_index_1() {
        assert_eq!(chain().find(PML4, 1), None);
    }

    #[test]
    fn a_4k_leaf_with_pwt_only_is_found_with_its_canonical_address() {
        let mut m = chain();
        m.set(PT, 3, 0x20_0000 | P | PWT);
        let hit = m.find(PML4, 1).unwrap();
        assert_eq!(hit.level, 1);
        assert!(hit.leaf);
        let expect = 0xFFFF_0000_0000_0000 | (511 << 39) | (1 << 30) | (2 << 21) | (3 << 12);
        assert_eq!(hit.virt, expect);
    }

    #[test]
    fn a_4k_leaf_with_pwt_and_its_pat_bit_selects_index_5_not_1() {
        let mut m = chain();
        m.set(PT, 3, 0x20_0000 | P | PWT | (1 << 7));
        assert_eq!(m.find(PML4, 1), None);
        assert_eq!(m.find(PML4, 5).map(|h| h.level), Some(1));
    }

    #[test]
    fn pwt_and_pcd_together_is_index_3_the_mmio_mapping_and_not_a_conflict() {
        let mut m = chain();
        m.set(PT, 0, 0x20_0000 | P | PWT | PCD);
        assert_eq!(m.find(PML4, 1), None);
        assert_eq!(m.find(PML4, 3).map(|h| h.level), Some(1));
    }

    #[test]
    fn a_2m_leaf_reads_its_pat_bit_from_bit_12_and_ps_is_not_mistaken_for_it() {
        let mut m = chain();
        m.set(PD, 7, 0x4000_0000 | P | PS | PWT);
        assert_eq!(m.find(PML4, 1).map(|h| (h.level, h.leaf)), Some((2, true)));

        let mut m = chain();
        m.set(PD, 7, 0x4000_0000 | P | PS | PWT | (1 << 12));
        assert_eq!(m.find(PML4, 1), None, "PAT bit 12 set: index 5");
        assert_eq!(m.find(PML4, 5).map(|h| h.level), Some(2));
    }

    #[test]
    fn a_1g_leaf_is_a_leaf_and_is_not_descended_into() {
        let mut m = chain();
        // Points at PT's frame: descending into it would find the PWT
        // entry planted there. A leaf must not be walked as a table.
        m.set(PDPT, 5, PT | P | PS);
        m.set(PT, 9, 0x30_0000 | P | PWT);
        m.set(PD, 2, 0); // PT is no longer reachable as a table
        assert_eq!(m.find(PML4, 1), None);
    }

    #[test]
    fn a_non_leaf_entry_with_pwt_only_is_found_because_it_types_the_table_below() {
        let mut m = chain();
        m.set(PDPT, 1, PD | P | PWT);
        let hit = m.find(PML4, 1).unwrap();
        assert_eq!((hit.level, hit.leaf), (3, false));
    }

    #[test]
    fn bit_7_of_a_pml4_entry_is_not_ps_so_the_walk_still_descends() {
        // In a PML4E bit 7 is reserved, not PS. Reading it as PS would
        // treat the entry as a 512 GiB leaf and never see the PWT-only
        // page underneath.
        let mut m = chain();
        m.set(PML4, 511, PDPT | P | (1 << 7));
        m.set(PT, 3, 0x20_0000 | P | PWT);
        assert_eq!(m.find(PML4, 1).map(|h| h.level), Some(1));
    }

    #[test]
    fn cr3_with_pwt_only_is_reported_as_level_5() {
        let m = chain();
        assert_eq!(m.find(PML4 | PWT, 1).map(|h| h.level), Some(5));
    }

    #[test]
    fn non_present_entries_are_ignored_whatever_their_bits() {
        let mut m = chain();
        m.set(PT, 10, 0x20_0000 | PWT); // not present
        assert_eq!(m.find(PML4, 1), None);
    }

    #[test]
    fn a_lower_half_address_stays_lower_half() {
        let mut m = Mem::new();
        m.set(PML4, 0, PDPT | P);
        m.set(PDPT, 0, PD | P);
        m.set(PD, 0, PT | P);
        m.set(PT, 1, 0x5000 | P | PWT);
        assert_eq!(m.find(PML4, 1).map(|h| h.virt), Some(0x1000));
    }

    fn mtrr(base: u64, ty: u8, size: u64, valid: bool) -> VariableMtrr {
        // A power-of-two-sized range: mask = ~(size-1) within 36 bits.
        let mask = (!(size - 1)) & 0x0000_000F_FFFF_F000;
        VariableMtrr {
            base: base | ty as u64,
            mask: mask | if valid { 1 << 11 } else { 0 },
        }
    }

    const ENABLED_WB: MtrrDefType = MtrrDefType((1 << 11) | 6);

    #[test]
    fn address_inside_a_variable_range_takes_that_ranges_type() {
        let e = [mtrr(0xC000_0000, 0, 0x1000_0000, true)]; // 256 MiB UC
        assert_eq!(
            mtrr_type_for(0xC010_0000, ENABLED_WB, &e, 36),
            MtrrResolution::Matched(MemType::Uc)
        );
    }

    #[test]
    fn address_outside_every_range_falls_back_to_the_default_type() {
        let e = [mtrr(0xC000_0000, 0, 0x1000_0000, true)];
        assert_eq!(
            mtrr_type_for(0x1_0000, ENABLED_WB, &e, 36),
            MtrrResolution::Default(MemType::Wb)
        );
    }

    #[test]
    fn an_invalid_entry_matches_nothing_even_when_its_base_covers_the_address() {
        let e = [mtrr(0xC000_0000, 0, 0x1000_0000, false)];
        assert_eq!(
            mtrr_type_for(0xC010_0000, ENABLED_WB, &e, 36),
            MtrrResolution::Default(MemType::Wb)
        );
    }

    #[test]
    fn disabled_mtrrs_make_the_whole_space_uc_regardless_of_default_type() {
        let e = [mtrr(0xC000_0000, 6, 0x1000_0000, true)];
        let disabled = MtrrDefType(6); // WB default, E == 0
        assert_eq!(mtrr_type_for(0xC010_0000, disabled, &e, 36), MtrrResolution::Disabled);
        assert_eq!(
            mtrr_type_for(0xC010_0000, disabled, &e, 36).mem_type(),
            MemType::Uc
        );
    }

    #[test]
    fn uc_wins_over_wb_when_two_ranges_overlap() {
        let e = [
            mtrr(0xC000_0000, 6, 0x1000_0000, true), // WB
            mtrr(0xC000_0000, 0, 0x0100_0000, true), // UC, smaller
        ];
        assert_eq!(
            mtrr_type_for(0xC000_1000, ENABLED_WB, &e, 36),
            MtrrResolution::Overlapping { resolved: MemType::Uc, conflicting: false }
        );
    }

    #[test]
    fn wt_wins_over_wb_when_two_ranges_overlap() {
        let e = [
            mtrr(0xC000_0000, 6, 0x1000_0000, true), // WB
            mtrr(0xC000_0000, 4, 0x0100_0000, true), // WT
        ];
        assert_eq!(
            mtrr_type_for(0xC000_1000, ENABLED_WB, &e, 36),
            MtrrResolution::Overlapping { resolved: MemType::Wt, conflicting: false }
        );
    }

    #[test]
    fn an_undefined_overlap_resolves_to_uc_and_says_it_was_undefined() {
        let e = [
            mtrr(0xC000_0000, 1, 0x1000_0000, true), // WC
            mtrr(0xC000_0000, 6, 0x0100_0000, true), // WB
        ];
        assert_eq!(
            mtrr_type_for(0xC000_1000, ENABLED_WB, &e, 36),
            MtrrResolution::Overlapping { resolved: MemType::Uc, conflicting: true }
        );
    }

    #[test]
    fn two_overlapping_ranges_that_agree_are_a_plain_match_not_an_overlap() {
        let e = [
            mtrr(0xC000_0000, 0, 0x1000_0000, true),
            mtrr(0xC000_0000, 0, 0x0100_0000, true),
        ];
        assert_eq!(
            mtrr_type_for(0xC000_1000, ENABLED_WB, &e, 36),
            MtrrResolution::Matched(MemType::Uc)
        );
    }

    #[test]
    fn a_range_above_four_gib_still_matches_with_more_physical_address_bits() {
        // The USB BAR on the target machine sits at 0xC0_0000_0000; a
        // framebuffer aperture can too, and truncating to 32 bits would
        // silently report the wrong type.
        let e = [VariableMtrr {
            base: 0x0000_00C0_0000_0000 | 1, // WC
            mask: 0x0000_00FF_8000_0000 | (1 << 11),
        }];
        assert_eq!(
            mtrr_type_for(0x0000_00C0_1000_0000, ENABLED_WB, &e, 48),
            MtrrResolution::Matched(MemType::Wc)
        );
        assert_eq!(
            mtrr_type_for(0x0000_00C1_0000_0000, ENABLED_WB, &e, 48),
            MtrrResolution::Default(MemType::Wb)
        );
    }

    #[test]
    fn variable_count_is_clamped_so_a_bad_mtrrcap_cannot_drive_an_unbounded_walk() {
        assert_eq!(mtrr_variable_count(8), 8);
        assert_eq!(mtrr_variable_count(0x0000_0000_0000_0A08), 8);
        assert_eq!(mtrr_variable_count(0xFF), 64);
        assert_eq!(mtrr_variable_count(0), 0);
    }

    #[test]
    fn mtrrcap_reports_wc_support_from_bit_ten() {
        assert!(mtrr_wc_supported(1 << 10));
        assert!(!mtrr_wc_supported(0x08));
    }

    #[test]
    fn def_type_bits_split_into_enable_fixed_enable_and_type() {
        let d = MtrrDefType((1 << 11) | (1 << 10) | 6);
        assert!(d.enabled());
        assert!(d.fixed_enabled());
        assert_eq!(d.default_type(), MemType::Wb);

        let d = MtrrDefType(0);
        assert!(!d.enabled());
        assert!(!d.fixed_enabled());
        assert_eq!(d.default_type(), MemType::Uc);
    }
}
