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

use alloc::sync::Arc;
use alloc::vec::Vec;
use x86_64::structures::paging::PageTableFlags;

use super::shm::ShmObject;

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
/// same cap.
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
    /// Pages of a `ShmObject` (`memfd_create`, `MAP_SHARED`): every
    /// mapping of the object maps the object's own frames, so writes are
    /// seen by all of them. Never COW, never the zero frame, never
    /// write-protected by `fork`. `Vma::shm` names the object.
    Shared,
}

/// A `Shared` VMA's hold on its object: the object stays alive while any
/// VMA maps it, and `ShmObject::mappings` counts these (a clone is one
/// more) — the bound that keeps a frame's `u8` refcount from saturating.
#[derive(Debug)]
pub struct ShmMapping {
    pub obj: Arc<ShmObject>,
    /// Object page mapped at the VMA's `start`.
    pub offset_pages: usize,
}

impl ShmMapping {
    pub fn new(obj: Arc<ShmObject>, offset_pages: usize) -> Self {
        obj.mapping_added();
        Self { obj, offset_pages }
    }
}

impl Clone for ShmMapping {
    fn clone(&self) -> Self {
        Self::new(self.obj.clone(), self.offset_pages)
    }
}

impl Drop for ShmMapping {
    fn drop(&mut self) {
        self.obj.mapping_removed();
    }
}

/// A single virtual memory area.
///
/// Not `Copy`: a `Shared` VMA holds its object (`shm`), and copying one
/// has to count as one more mapping of it.
#[derive(Debug, Clone)]
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
    /// The object behind a `Shared` VMA; `None` for every other kind.
    pub shm: Option<ShmMapping>,
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

/// A process's VMAs, at most `MAX_VMAS_PER_PROCESS`, in the order they
/// were added (`find` returns the first match, which matters where two
/// ELF segments share a page).
///
/// A `Vec`, not an inline array: `VmaList` used to be
/// `[Option<Vma>; 64]` by value, and at `opt-level 0` every `new()` and
/// `IrqMutex::new` copies it through the stack. Growing `Vma` by the
/// `shm` field took that copy past the bootloader's 80 KiB boot stack
/// (a double fault loading PID 1). Empty, this is three words; `fork`
/// clones only the VMAs that exist. Allocates under the address-space
/// lock, which the lock order allows (address space → `SLAB_ALLOCATOR`).
#[derive(Clone)]
pub struct VmaList {
    entries: Vec<Vma>,
}

impl VmaList {
    pub const fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// Register a VMA.  Returns error if the list is full.
    pub fn add(&mut self, vma: Vma) -> Result<(), &'static str> {
        if self.entries.len() >= MAX_VMAS_PER_PROCESS {
            return Err("VMA list full");
        }
        self.entries.try_reserve(1).map_err(|_| "VMA list: out of memory")?;
        self.entries.push(vma);
        Ok(())
    }

    /// Find the VMA containing `addr`, if any.
    pub fn find(&self, addr: u64) -> Option<&Vma> {
        self.entries.iter().find(|v| v.contains(addr))
    }

    /// Remove the VMA that starts exactly at `start`.
    /// Returns the removed VMA, or `Err` if not found.
    pub fn remove(&mut self, start: u64) -> Result<Vma, &'static str> {
        let i = self.entries.iter().position(|v| v.start == start).ok_or("VMA not found")?;
        Ok(self.entries.remove(i))
    }

    /// Returns true if any existing VMA overlaps [start, start + size_pages * 4096).
    pub fn overlaps(&self, start: u64, size_pages: usize) -> bool {
        let end = start + size_pages as u64 * 4096;
        self.entries.iter().any(|v| v.start < end && v.end() > start)
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
        for (i, vma) in self.entries.iter().enumerate() {
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
            .filter_map(|(j, v)| if j == idx { None } else { Some(v) })
            .any(|other| other.start < old_start && other.end() > page_addr);
        if would_overlap {
            return None;
        }

        let slot = &mut self.entries[idx];
        slot.start = page_addr;
        slot.size_pages = new_size_pages;
        Some(slot.clone())
    }

    /// Remove all VMAs (for process exit).
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Iterator over registered VMAs.
    pub fn iter(&self) -> impl Iterator<Item = &Vma> {
        self.entries.iter()
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
                VmaKind::Shared => "shared",
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