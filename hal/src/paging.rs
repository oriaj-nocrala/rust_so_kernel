//! Counting what a 4-level page table actually maps — resident set size.
//!
//! `/proc/<pid>/stat`'s `rss` and `/proc/<pid>/statm` are the number of
//! user pages present in a process's page table. Linux keeps per-mm
//! counters updated on every map and unmap; here every path that changes a
//! user PTE (demand paging, COW, `fork`, `munmap`, `exec`, the ELF loader,
//! stack growth, shared mappings) would have to remember to update one, so
//! the kernel instead walks the table when `/proc` is read. Correct by
//! construction, and cheap: absent tables are skipped whole, so the cost is
//! proportional to the page tables that exist, not to the address range.
//!
//! Pure: `read(phys)` returns the `u64` at a physical address — the kernel
//! reads through its physical-memory window, host tests through a map
//! (`hal::memtype::find_pat_index_user`'s seam).

const PRESENT: u64 = 1 << 0;
const USER: u64 = 1 << 2;
/// PS: a PDPTE or PDE that maps a page instead of pointing at a table.
const HUGE: u64 = 1 << 7;
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// Number of 4 KiB pages in `[start, end)` that `pml4` maps present and
/// user-accessible, **excluding** any 4 KiB leaf whose frame is
/// `skip_frame` (the shared zero frame — Linux does not count the zero
/// page as resident either). A 2 MiB or 1 GiB leaf counts as the 4 KiB
/// pages of it that fall inside the range.
///
/// `start`/`end` are canonical lower-half addresses; anything at or above
/// 2^47 is ignored (user space ends there). A table entry without the
/// USER bit prunes its whole subtree: the processor would deny user access
/// to everything under it anyway.
///
/// The recursion depth is the paging depth, so a malformed table cannot
/// make the walk unbounded.
pub fn count_resident(
    pml4: u64,
    start: u64,
    end: u64,
    skip_frame: Option<u64>,
    read: &mut dyn FnMut(u64) -> u64,
) -> u64 {
    let end = end.min(1 << 47);
    if start >= end {
        return 0;
    }
    walk(pml4 & ADDR_MASK, 4, 0, start, end, skip_frame, read)
}

fn walk(
    table: u64,
    level: u32,
    base: u64,
    start: u64,
    end: u64,
    skip_frame: Option<u64>,
    read: &mut dyn FnMut(u64) -> u64,
) -> u64 {
    let shift = 12 + 9 * (level - 1);
    let span = 1u64 << shift;
    // Only the entries that overlap [start, end).
    let first = if start > base { (start - base) >> shift } else { 0 };
    let last = ((end - 1).saturating_sub(base) >> shift).min(511);
    let mut total = 0;
    for i in first..=last {
        let entry = read(table + i * 8);
        if entry & PRESENT == 0 || entry & USER == 0 {
            continue;
        }
        let lo = base + i * span;
        let hi = lo + span;
        let leaf = level == 1 || ((level == 2 || level == 3) && entry & HUGE != 0);
        if leaf {
            if level == 1 && skip_frame == Some(entry & ADDR_MASK) {
                continue;
            }
            let (a, b) = (lo.max(start), hi.min(end));
            total += (b - a) >> 12;
        } else if level > 1 {
            total += walk(entry & ADDR_MASK, level - 1, lo, start, end, skip_frame, read);
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;

    /// A page table in a map from physical address to `u64`, built by
    /// `map` the way the kernel's mapper builds one: intermediate tables
    /// PRESENT|WRITABLE|USER, allocated on first use.
    struct Tables {
        mem: BTreeMap<u64, u64>,
        next: u64,
        pml4: u64,
    }

    const P: u64 = PRESENT | (1 << 1) | USER;

    impl Tables {
        fn new() -> Self {
            Tables { mem: BTreeMap::new(), next: 0x10_0000, pml4: 0x1000 }
        }
        fn alloc(&mut self) -> u64 {
            self.next += 0x1000;
            self.next
        }
        fn get(&self, pa: u64) -> u64 {
            *self.mem.get(&pa).unwrap_or(&0)
        }
        /// Walk down to `level`, creating tables; returns the entry's address.
        fn slot(&mut self, va: u64, level: u32, parent_flags: u64) -> u64 {
            let mut table = self.pml4;
            for l in (level + 1..=4).rev() {
                let idx = (va >> (12 + 9 * (l - 1))) & 0x1FF;
                let e = self.get(table + idx * 8);
                table = if e & PRESENT == 0 {
                    let t = self.alloc();
                    self.mem.insert(table + idx * 8, t | parent_flags);
                    t
                } else {
                    e & ADDR_MASK
                };
            }
            table + ((va >> (12 + 9 * (level - 1))) & 0x1FF) * 8
        }
        fn map(&mut self, va: u64, frame: u64, flags: u64) {
            let s = self.slot(va, 1, P);
            self.mem.insert(s, frame | flags);
        }
        fn map_2m(&mut self, va: u64, frame: u64) {
            let s = self.slot(va, 2, P);
            self.mem.insert(s, frame | P | HUGE);
        }
        fn count(&self, start: u64, end: u64, skip: Option<u64>) -> u64 {
            count_resident(self.pml4, start, end, skip, &mut |pa| self.get(pa))
        }
    }

    const ALL: u64 = 1 << 47;

    #[test]
    fn empty_table_is_zero() {
        let t = Tables::new();
        assert_eq!(t.count(0, ALL, None), 0);
    }

    #[test]
    fn counts_present_user_leaves_only() {
        let mut t = Tables::new();
        t.map(0x40_0000, 0x5000_0000, P);
        t.map(0x40_1000, 0x5000_1000, PRESENT | USER); // read-only still counts
        t.map(0x40_2000, 0x5000_2000, (1 << 1) | USER); // not present
        t.map(0x40_3000, 0x5000_3000, PRESENT); // kernel-only leaf
        assert_eq!(t.count(0, ALL, None), 2);
    }

    #[test]
    fn range_bounds_are_half_open_and_page_exact() {
        let mut t = Tables::new();
        for i in 0..8 {
            t.map(0x40_0000 + i * 0x1000, 0x5000_0000 + i * 0x1000, P);
        }
        assert_eq!(t.count(0x40_0000, 0x40_8000, None), 8);
        assert_eq!(t.count(0x40_1000, 0x40_3000, None), 2);
        assert_eq!(t.count(0x40_8000, 0x50_0000, None), 0);
        assert_eq!(t.count(0x40_3000, 0x40_3000, None), 0);
        assert_eq!(t.count(0x40_5000, 0x40_1000, None), 0);
    }

    #[test]
    fn zero_frame_is_not_resident() {
        let mut t = Tables::new();
        let zero = 0x7777_0000;
        t.map(0x40_0000, zero, PRESENT | USER);
        t.map(0x40_1000, zero, PRESENT | USER);
        t.map(0x40_2000, 0x5000_2000, P);
        assert_eq!(t.count(0, ALL, Some(zero)), 1);
        assert_eq!(t.count(0, ALL, None), 3);
    }

    #[test]
    fn huge_page_counts_its_4k_pages_clipped_to_the_range() {
        let mut t = Tables::new();
        t.map_2m(0x4000_0020_0000, 0x8000_0000);
        assert_eq!(t.count(0, ALL, None), 512);
        // A VMA covering only part of it (the kernel never makes one, but
        // the arithmetic must still clip rather than count 512).
        assert_eq!(t.count(0x4000_0020_0000, 0x4000_0020_4000, None), 4);
        assert_eq!(t.count(0x4000_003F_F000, 0x4000_0050_0000, None), 1);
        // The zero-frame exclusion applies to 4 KiB leaves only.
        assert_eq!(t.count(0, ALL, Some(0x8000_0000)), 512);
    }

    #[test]
    fn walks_across_table_boundaries() {
        let mut t = Tables::new();
        // Last page under one PT, first under the next; last under one
        // PML4 entry, first under the next.
        t.map(0x1F_F000, 0x5000_0000, P);
        t.map(0x20_0000, 0x5000_1000, P);
        t.map(0x7F_FFFF_F000, 0x5000_2000, P);
        t.map(0x80_0000_0000, 0x5000_3000, P);
        assert_eq!(t.count(0, ALL, None), 4);
        assert_eq!(t.count(0x1F_F000, 0x20_1000, None), 2);
        assert_eq!(t.count(0x7F_FFFF_F000, 0x80_0000_1000, None), 2);
    }

    #[test]
    fn non_user_table_prunes_its_subtree() {
        let mut t = Tables::new();
        t.map(0x40_0000, 0x5000_0000, P);
        // Clear USER on the PML4 entry above it.
        let e = t.get(t.pml4);
        t.mem.insert(t.pml4, e & !USER);
        assert_eq!(t.count(0, ALL, None), 0);
    }

    #[test]
    fn upper_half_is_ignored() {
        let mut t = Tables::new();
        t.map(0x7FFF_FFFF_F000, 0x5000_0000, P);
        assert_eq!(t.count(0, u64::MAX, None), 1);
        assert_eq!(t.count(1 << 47, u64::MAX, None), 0);
    }

    #[test]
    fn reads_only_tables_that_exist() {
        // Two pages 64 GiB apart: the walk must read a handful of entries
        // per level, not every entry of the range.
        let mut t = Tables::new();
        t.map(0x40_0000, 0x5000_0000, P);
        t.map(0x10_0040_0000, 0x5000_1000, P);
        let mut reads = 0u64;
        let n = count_resident(t.pml4, 0, ALL, None, &mut |pa| {
            reads += 1;
            t.get(pa)
        });
        assert_eq!(n, 2);
        // PML4: 256 entries; the one PDPT: 512; two PDs and two PTs of 512
        // each would be 2048 more — the bound is what a full scan of the
        // tables that exist costs, far below one read per page.
        assert!(reads <= 256 + 512 + 4 * 512, "{reads} reads");
    }
}
