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
pub const MAX_VMAS_PER_PROCESS: usize = 16;

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