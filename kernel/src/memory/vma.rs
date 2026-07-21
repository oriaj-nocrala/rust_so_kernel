// kernel/src/memory/vma.rs
//
// Virtual Memory Areas — track which virtual address ranges are valid
// for each process.  Used by the demand paging fault handler to
// distinguish legitimate faults (allocate a page) from invalid ones
// (kill the process).
//
// ── REFACTOR NOTE ──────────────────────────────────────────────────
// VMAs now live INSIDE AddressSpace (which lives inside Process).
// The global VMA_TABLE indexed by PID has been removed.
// This file only exports the data types and VmaList container.
// ───────────────────────────────────────────────────────────────────

use x86_64::structures::paging::PageTableFlags;

// ============================================================================
// Constants
// ============================================================================

/// Maximum VMAs per process (code + stack + heap + extras).
pub const MAX_VMAS_PER_PROCESS: usize = 64;

/// How far below a `GrowableStack` VMA's current low boundary a fault is
/// still treated as legitimate stack growth rather than a wild pointer —
/// see `VmaList::grow_stack`'s doc comment.
const STACK_GROWTH_GUARD_PAGES: u64 = 64; // 256 KiB

/// Hard cap on how far any `GrowableStack` VMA can grow, in 4 KiB pages —
/// matches a real OS's `RLIMIT_STACK`-style ceiling (8 MiB is a common
/// real-world default). A single global constant rather than a per-VMA
/// field on `VmaKind::GrowableStack`: every stack in this kernel wants the
/// same cap, and keeping `VmaKind` a plain fieldless enum keeps `Vma`
/// (and the fixed-size `[Option<Vma>; MAX_VMAS_PER_PROCESS]` array backing
/// every process's VMA list) exactly the same size it always was — see
/// `elf_loader::STACK_PAGES`'s doc comment for why that matters here more
/// than it would look at first glance.
pub const STACK_MAX_PAGES: usize = 2048; // 8 MiB

// ============================================================================
// VMA types
// ============================================================================

/// What kind of backing does this region have?
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VmaKind {
    /// Zero-filled on demand (stack, heap, anonymous mmap).
    Anonymous,
    /// Pre-loaded code/data — tracked for validation but NOT demand-paged.
    /// If a code page faults, something is wrong.
    Code,
    /// Demand-paged anonymous region backed by 2 MiB huge pages.
    /// `size_pages` is still in 4 KiB units; each huge page covers 512 entries.
    Huge2M,
    /// Like `Anonymous`, but the page fault handler is allowed to extend
    /// `start` downward (never upward — this is specifically the "stack
    /// grows down" shape) when a fault lands just below the current low
    /// boundary, up to `STACK_MAX_PAGES` total. Used for every process's
    /// user stack: no program needs its actual stack usage known in
    /// advance — it starts small and grows exactly as far as it's
    /// actually used, same idea as a real OS's `RLIMIT_STACK`-capped
    /// growable stack VMA. See `VmaList::grow_stack`.
    GrowableStack,
}

/// A single virtual memory area.
#[derive(Debug, Clone, Copy)]
pub struct Vma {
    /// Page-aligned start address.
    pub start: u64,
    /// Number of 4 KiB pages in this region.
    pub size_pages: usize,
    /// Page table flags to use when mapping (USER_ACCESSIBLE, WRITABLE, etc.).
    /// PRESENT is added automatically by map_to().
    pub flags: u64,
    /// Backing type.
    pub kind: VmaKind,
}

impl Vma {
    /// Exclusive end address.
    #[inline]
    pub fn end(&self) -> u64 {
        self.start + (self.size_pages as u64 * 4096)
    }

    /// Does this VMA contain `addr`?
    #[inline]
    pub fn contains(&self, addr: u64) -> bool {
        addr >= self.start && addr < self.end()
    }

    /// Reconstruct PageTableFlags from stored bits.
    #[inline]
    pub fn page_table_flags(&self) -> PageTableFlags {
        PageTableFlags::from_bits_truncate(self.flags)
    }
}

// ============================================================================
// Per-process VMA list (owned by AddressSpace)
// ============================================================================

#[derive(Clone)]
pub struct VmaList {
    entries: [Option<Vma>; MAX_VMAS_PER_PROCESS],
}

impl VmaList {
    pub const fn new() -> Self {
        Self {
            entries: [None; MAX_VMAS_PER_PROCESS],
        }
    }

    /// Register a VMA.  Returns error if the list is full.
    pub fn add(&mut self, vma: Vma) -> Result<(), &'static str> {
        for slot in self.entries.iter_mut() {
            if slot.is_none() {
                *slot = Some(vma);
                return Ok(());
            }
        }
        Err("VMA list full")
    }

    /// Find the VMA containing `addr`, if any.
    pub fn find(&self, addr: u64) -> Option<&Vma> {
        self.entries
            .iter()
            .filter_map(|v| v.as_ref())
            .find(|v| v.contains(addr))
    }

    /// Remove the VMA that starts exactly at `start`.
    /// Returns the removed VMA, or `Err` if not found.
    pub fn remove(&mut self, start: u64) -> Result<Vma, &'static str> {
        for slot in self.entries.iter_mut() {
            if let Some(v) = slot {
                if v.start == start {
                    let vma = *v;
                    *slot = None;
                    return Ok(vma);
                }
            }
        }
        Err("VMA not found")
    }

    /// Returns true if any existing VMA overlaps [start, start + size_pages * 4096).
    pub fn overlaps(&self, start: u64, size_pages: usize) -> bool {
        let end = start + size_pages as u64 * 4096;
        self.entries
            .iter()
            .filter_map(|v| v.as_ref())
            .any(|v| v.start < end && v.end() > start)
    }

    /// Try to grow a `GrowableStack` VMA downward to cover `addr` (which
    /// must be below every existing VMA's start — `find` already found
    /// nothing, or this wouldn't be called). Returns the updated VMA on
    /// success.
    ///
    /// Fails (returns `None`, meaning "treat this as a real segfault") if:
    /// - `addr` is more than `STACK_GROWTH_GUARD_PAGES` below the nearest
    ///   `GrowableStack` VMA's current boundary — a wild pointer landing
    ///   in the (large) unmapped gap between the stack and everything
    ///   else should still segfault instead of silently "growing" a stack
    ///   that was never actually being used that far down.
    /// - Growing would exceed `STACK_MAX_PAGES`.
    /// - The newly-covered range would overlap another VMA — unlikely in
    ///   practice (stacks live at a fixed high address with nothing else
    ///   registered nearby) but checked rather than assumed.
    pub fn grow_stack(&mut self, addr: u64) -> Option<Vma> {
        let page_addr = addr & !0xFFF;

        // Find a growth candidate first (immutable pass — `overlaps`-style
        // scan below needs its own immutable iteration, so don't hold a
        // `&mut` into `self.entries` across it).
        let mut target: Option<(usize, u64, usize)> = None; // (index, old_start, new_size_pages)
        for (i, slot) in self.entries.iter().enumerate() {
            let Some(vma) = slot else { continue };
            if vma.kind != VmaKind::GrowableStack {
                continue;
            }
            if page_addr >= vma.start {
                continue; // not below this VMA's current boundary
            }
            let gap_pages = (vma.start - page_addr) / 4096;
            if gap_pages > STACK_GROWTH_GUARD_PAGES {
                continue; // too far below — likely a wild pointer
            }
            let new_size_pages = ((vma.end() - page_addr) / 4096) as usize;
            if new_size_pages > STACK_MAX_PAGES {
                continue; // would exceed the stack growth cap
            }
            target = Some((i, vma.start, new_size_pages));
            break;
        }

        let (idx, old_start, new_size_pages) = target?;

        let would_overlap = self.entries.iter().enumerate()
            .filter_map(|(j, s)| if j == idx { None } else { s.as_ref() })
            .any(|other| other.start < old_start && other.end() > page_addr);
        if would_overlap {
            return None;
        }

        let slot = self.entries[idx].as_mut().unwrap();
        slot.start = page_addr;
        slot.size_pages = new_size_pages;
        Some(*slot)
    }

    /// Remove all VMAs (for process exit).
    pub fn clear(&mut self) {
        for slot in self.entries.iter_mut() {
            *slot = None;
        }
    }

    /// Iterator over registered VMAs.
    pub fn iter(&self) -> impl Iterator<Item = &Vma> {
        self.entries.iter().filter_map(|v| v.as_ref())
    }

    /// Debug: print all VMAs to serial.
    /// `label` is typically the PID, used only for the log line.
    pub fn dump(&self, label: usize) {
        crate::serial_println!("VMAs for PID {}:", label);
        for vma in self.iter() {
            let kind_str = match vma.kind {
                VmaKind::Anonymous => "anon",
                VmaKind::Code => "code",
                VmaKind::Huge2M => "huge2m",
                VmaKind::GrowableStack => "stack(grows down)",
            };
            crate::serial_println!(
                "  {:#x}..{:#x} ({} pages) [{}] flags={:#x}",
                vma.start,
                vma.end(),
                vma.size_pages,
                kind_str,
                vma.flags,
            );
        }
    }
}