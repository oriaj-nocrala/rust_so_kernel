// kernel/src/memory/vma.rs
//
// Virtual Memory Areas — track which virtual address ranges are valid
// for each process.  Used by the demand paging fault handler to
// distinguish legitimate faults (allocate a page) from invalid ones
// (kill the process).
//
// Design:
//   - Fixed-size arrays (no heap allocation in the VMA subsystem itself)
//   - Global table indexed by PID
//   - Lock-free reads are NOT needed because the fault handler runs
//     with interrupts disabled anyway (x86 page fault)

use spin::Mutex;
use x86_64::structures::paging::PageTableFlags;

// ============================================================================
// Constants
// ============================================================================

/// Maximum number of processes tracked.
pub const MAX_PROCESSES: usize = 64;

/// Maximum VMAs per process (code + stack + heap + extras).
const MAX_VMAS_PER_PROCESS: usize = 16;

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
// Per-process VMA list
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
}

// ============================================================================
// Global VMA registry
// ============================================================================

static VMA_TABLE: Mutex<VmaTable> = Mutex::new(VmaTable::new());

struct VmaTable {
    lists: [VmaList; MAX_PROCESSES],
}

impl VmaTable {
    const fn new() -> Self {
        const INIT: VmaList = VmaList::new();
        Self {
            lists: [INIT; MAX_PROCESSES],
        }
    }
}

// ============================================================================
// Public API
// ============================================================================

/// Register a VMA for a process.
///
/// # Example
/// ```ignore
/// register_vma(pid, Vma {
///     start: 0x7100_0000_0000,
///     size_pages: 16,
///     flags: (PRESENT | WRITABLE | USER_ACCESSIBLE).bits(),
///     kind: VmaKind::Anonymous,
/// })?;
/// ```
pub fn register_vma(pid: usize, vma: Vma) -> Result<(), &'static str> {
    if pid >= MAX_PROCESSES {
        return Err("PID out of range for VMA table");
    }
    let mut table = VMA_TABLE.lock();
    table.lists[pid].add(vma)
}

/// Find the VMA containing `addr` for process `pid`.
/// Returns a copy (Vma is Copy) to avoid holding the lock.
pub fn find_vma(pid: usize, addr: u64) -> Option<Vma> {
    if pid >= MAX_PROCESSES {
        return None;
    }
    let table = VMA_TABLE.lock();
    table.lists[pid].find(addr).copied()
}

/// Clear all VMAs for a process (on exit).
pub fn clear_vmas(pid: usize) {
    if pid < MAX_PROCESSES {
        VMA_TABLE.lock().lists[pid].clear();
    }
}

/// Debug: print all VMAs for a process.
pub fn dump_vmas(pid: usize) {
    if pid >= MAX_PROCESSES {
        return;
    }
    let table = VMA_TABLE.lock();
    crate::serial_println!("VMAs for PID {}:", pid);
    for vma in table.lists[pid].iter() {
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