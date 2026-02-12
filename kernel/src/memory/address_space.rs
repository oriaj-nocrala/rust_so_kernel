// kernel/src/memory/address_space.rs
//
// AddressSpace: groups a process's page table + VMAs into a single
// unit that does NOT depend on PID.
//
// This is the only structural addition of the refactor.  Everything
// else is wiring changes.

use x86_64::{
    VirtAddr,
    structures::paging::{Page, PageTableFlags, PhysFrame, Size4KiB, mapper::MapToError},
};

use super::page_table_manager::OwnedPageTable;
use super::vma::{Vma, VmaList};

pub struct AddressSpace {
    pub page_table: OwnedPageTable,
    pub vmas: VmaList,
}

unsafe impl Send for AddressSpace {}

impl AddressSpace {
    // ====================================================================
    // CONSTRUCTORS
    // ====================================================================

    /// Kernel address space: wraps the current CR3, no VMAs.
    /// Used by idle and shell processes.
    pub fn kernel() -> Self {
        Self {
            page_table: OwnedPageTable::from_current(),
            vmas: VmaList::new(),
        }
    }

    /// New user address space: fresh page table with kernel entries
    /// copied, empty VMA list.
    ///
    /// # Safety
    /// Buddy allocator must be initialized.
    pub unsafe fn new_user() -> Result<Self, &'static str> {
        let page_table = OwnedPageTable::new_user()?;
        Ok(Self {
            page_table,
            vmas: VmaList::new(),
        })
    }

    // ====================================================================
    // VMA MANAGEMENT
    // ====================================================================

    /// Register a virtual memory area.
    pub fn add_vma(&mut self, vma: Vma) -> Result<(), &'static str> {
        self.vmas.add(vma)
    }

    /// Find the VMA containing `addr`, if any.
    /// Returns a copy (Vma is Copy).
    pub fn find_vma(&self, addr: u64) -> Option<Vma> {
        self.vmas.find(addr).copied()
    }

    /// Debug: print all VMAs (uses serial, no allocation).
    pub fn dump_vmas(&self, label: usize) {
        self.vmas.dump(label);
    }

    // ====================================================================
    // PAGE TABLE DELEGATION
    // ====================================================================

    /// Activate this address space (write CR3).
    /// No-op if already active.
    pub unsafe fn activate(&self) {
        self.page_table.activate();
    }

    /// Map a single user page.  Allocates data + intermediate frames
    /// from the Buddy allocator.
    pub unsafe fn map_user_page(
        &self,
        page: Page<Size4KiB>,
        flags: PageTableFlags,
    ) -> Result<PhysFrame, MapToError<Size4KiB>> {
        self.page_table.map_user_page(page, flags)
    }

    /// Physical address of the PML4 root frame.
    pub fn pml4_phys(&self) -> x86_64::PhysAddr {
        self.page_table.pml4_phys()
    }

    /// The root PhysFrame (for debug logging).
    pub fn root_frame(&self) -> PhysFrame {
        self.page_table.root_frame()
    }
}