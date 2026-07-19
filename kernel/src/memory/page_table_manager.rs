// kernel/src/memory/page_table_manager.rs
//
// Per-process page tables, mapped WITHOUT activating (via physical memory offset).
//
// ⚠️ CRITICAL DESIGN NOTES:
//
// 1. The bootloader (bootloader_api + Mapping::Dynamic) places the kernel
//    in the LOWER half of the address space:
//      - Kernel code:   PML4 entry 2  (0x0000_0100_xxxx_xxxx)
//      - Phys offset:   PML4 entry 5  (0x0000_2800_xxxx_xxxx)
//    Therefore we must copy lower-half kernel entries too, not just 256-511.
//
// 2. We must NOT copy PML4 entries that overlap with user virtual addresses
//    (code at 0x400000 → PML4[0], stack at 0x710000000000 → PML4[226]).
//    Copying them would SHARE the intermediate page tables (PDPT/PD/PT)
//    between processes, causing PageAlreadyMapped on the second process.
//
// 3. All frame allocations use the Buddy allocator (not BootInfoFrameAllocator)
//    to avoid double-allocation with the heap.
//
// 4. NX (No-Execute) bit: Do NOT set unless EFER.NXE is confirmed enabled.

use x86_64::{
    PhysAddr, VirtAddr,
    registers::control::Cr3,
    structures::paging::{
        FrameAllocator, Mapper, OffsetPageTable, Page, PageTable,
        PageTableFlags, PhysFrame, Size2MiB, Size4KiB,
        page_table::FrameError,
        mapper::MapToError,
    },
};


// ============================================================================
// User address layout — which PML4 entries user processes own
// ============================================================================

/// User code base address (0x400000).  Falls in PML4 entry 0.
const USER_CODE_BASE: u64 = 0x0000_0000_0040_0000;

/// User stack base address (0x710000000000).  Falls in PML4 entry 226.
const USER_STACK_BASE: u64 = 0x0000_7100_0000_0000;

/// Base address for anonymous mmap allocations (0x4000_0000_0000). Falls in PML4 entry 128.
pub const USER_MMAP_BASE: u64 = 0x0000_4000_0000_0000;

/// Convert a virtual address to its PML4 index (bits 47:39).
#[inline]
const fn pml4_index(va: u64) -> usize {
    ((va >> 39) & 0x1FF) as usize
}

/// PML4 indices that belong to user processes.
/// These must NOT be copied from the kernel — each process builds its own.
/// Adding the mmap entry ensures `release_user_pages` frees mmap frames on exit.
const USER_PML4_ENTRIES: [usize; 3] = [
    pml4_index(USER_CODE_BASE),   // 0   — user code
    pml4_index(USER_STACK_BASE),  // 226 — user stack
    pml4_index(USER_MMAP_BASE),   // 128 — anonymous mmap region
];

/// Returns true if `index` is a PML4 entry reserved for user space.
#[inline]
fn is_user_pml4_entry(index: usize) -> bool {
    USER_PML4_ENTRIES.contains(&index)
}

// ============================================================================
// BuddyFrameAllocator
// ============================================================================

pub struct BuddyFrameAllocator;

unsafe impl FrameAllocator<Size4KiB> for BuddyFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        unsafe {
            crate::allocator::phys_alloc(12)
                .map(|addr| PhysFrame::containing_address(addr))
        }
    }
}

unsafe impl FrameAllocator<Size2MiB> for BuddyFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size2MiB>> {
        unsafe {
            crate::allocator::phys_alloc(21)
                .map(|addr| PhysFrame::containing_address(addr))
        }
    }
}

// ============================================================================
// OwnedPageTable
// ============================================================================

pub struct OwnedPageTable {
    pml4_frame: PhysFrame,
    owned: bool,
}

unsafe impl Send for OwnedPageTable {}
unsafe impl Sync for OwnedPageTable {}

impl OwnedPageTable {
    // ====================================================================
    // CONSTRUCTORS
    // ====================================================================

    /// Wrap the CURRENT kernel page table (CR3).
    /// For kernel processes (idle, shell) that share the kernel address space.
    pub fn from_current() -> Self {
        let (frame, _) = Cr3::read();
        Self {
            pml4_frame: frame,
            owned: false,
        }
    }

    /// Create a NEW page table for a user process.
    ///
    /// Copies all kernel PML4 entries EXCEPT those that overlap with user
    /// virtual address ranges.  This ensures:
    ///   - Kernel code, physical memory offset, framebuffer etc. are visible.
    ///   - Each process gets INDEPENDENT intermediate tables for user code
    ///     and user stack, preventing PageAlreadyMapped conflicts.
    ///
    /// # Safety
    /// Must be called after the Buddy allocator is initialized.
    pub unsafe fn new_user() -> Result<Self, &'static str> {
        let phys_offset = crate::memory::physical_memory_offset();

        // 1. Allocate PML4 frame from the Buddy
        let new_frame = {
            let phys_addr = crate::allocator::phys_alloc(12)
                .ok_or("Failed to allocate PML4 frame from buddy")?;
            PhysFrame::containing_address(phys_addr)
        };

        let new_pml4_virt = phys_offset + new_frame.start_address().as_u64();
        let new_pml4: &mut PageTable = &mut *new_pml4_virt.as_mut_ptr::<PageTable>();

        // 2. Zero the entire table (defense against stale data)
        new_pml4.zero();

        // 3. Read the kernel's PML4
        let (kernel_frame, _) = Cr3::read();
        let kernel_pml4_virt = phys_offset + kernel_frame.start_address().as_u64();
        let kernel_pml4: &PageTable = &*kernel_pml4_virt.as_ptr::<PageTable>();

        // 4. Copy kernel entries, SKIPPING user-owned entries
        //
        // Why skip? PML4 entries point to PDPTs. If two processes share the
        // same PDPT, their PD/PT modifications are visible to each other.
        // Process 0 maps 0x400000, creating entries under PML4[0]'s PDPT.
        // If process 1 has the same PML4[0], it sees 0x400000 as already
        // mapped → PageAlreadyMapped error.
        //
        // By skipping user entries, each process creates its own PDPT/PD/PT
        // chain independently via map_user_page().
        let mut copied = 0u16;
        let mut skipped = 0u16;

        for i in 0..512 {
            if kernel_pml4[i].is_unused() {
                continue;
            }

            if is_user_pml4_entry(i) {
                skipped += 1;
                crate::serial_println!(
                    "  PML4[{}]: SKIPPED (user address range, flags={:#x})",
                    i,
                    kernel_pml4[i].flags().bits()
                );
                continue;
            }

            new_pml4[i] = kernel_pml4[i].clone();
            copied += 1;
        }

        crate::serial_println!(
            "  Creating new page table: PML4 at {:#x}",
            new_frame.start_address().as_u64()
        );
        crate::serial_println!(
            "  Copied {} kernel entries, skipped {} user-range entries",
            copied, skipped
        );

        Ok(Self {
            pml4_frame: new_frame,
            owned: true,
        })
    }

    // ====================================================================
    // ACCESSORS
    // ====================================================================

    pub fn root_frame(&self) -> PhysFrame {
        self.pml4_frame
    }

    #[inline]
    pub fn pml4_phys(&self) -> PhysAddr {
        self.pml4_frame.start_address()
    }

    // ====================================================================
    // ACTIVATE (change CR3)
    // ====================================================================

    /// Switch the CPU to this page table.
    /// No-op if CR3 already matches (avoids TLB flush).
    pub unsafe fn activate(&self) {
        use x86_64::registers::control::Cr3Flags;

        let (current_frame, _) = Cr3::read();
        if current_frame == self.pml4_frame {
            return;
        }

        Cr3::write(self.pml4_frame, Cr3Flags::empty());
    }

    // ====================================================================
    // MAP WITHOUT ACTIVATING
    // ====================================================================

    unsafe fn create_mapper(&self) -> OffsetPageTable<'static> {
        let phys_offset = crate::memory::physical_memory_offset();
        let pml4_virt = phys_offset + self.pml4_phys().as_u64();
        let pml4: &mut PageTable = &mut *pml4_virt.as_mut_ptr::<PageTable>();
        OffsetPageTable::new(pml4, phys_offset)
    }

    /// Look up the physical frame backing an already-mapped page.
    /// Returns `None` if the page is not present in this page table.
    pub unsafe fn translate_page(&self, page: Page<Size4KiB>) -> Option<PhysFrame> {
        let mapper = self.create_mapper();
        mapper.translate_page(page).ok()
    }

    /// Map one user page.  Allocates data + intermediate frames from Buddy.
    /// Sets the frame's COW refcount to 1 (single owner).
    pub unsafe fn map_user_page(
        &self,
        page: Page<Size4KiB>,
        flags: PageTableFlags,
    ) -> Result<PhysFrame, MapToError<Size4KiB>> {
        let mut buddy_alloc = BuddyFrameAllocator;

        let frame = buddy_alloc
            .allocate_frame()
            .ok_or(MapToError::FrameAllocationFailed)?;

        // Track as single owner (COW refcount = 1).
        crate::memory::cow::set_ref(frame, 1);

        let mut mapper = self.create_mapper();

        mapper
            .map_to(page, frame, flags, &mut buddy_alloc)?
            .flush();

        Ok(frame)
    }

    /// Map an existing physical frame into a page without allocating a new data frame.
    /// Only intermediate PT frames (PDPT/PD/PT) are allocated from the Buddy.
    /// Used for COW fork: child shares the parent's frame.
    ///
    /// IMPORTANT: intermediate tables are always created with PRESENT|WRITABLE|USER_ACCESSIBLE
    /// even when `flags` (the leaf PTE) is read-only.  This is required so that the COW fault
    /// handler can later restore WRITABLE on just the leaf entry: the CPU checks R/W on ALL
    /// page table levels, so if any intermediate entry lacks WRITABLE the page stays read-only
    /// even after the leaf is updated.
    pub unsafe fn map_existing_frame(
        &self,
        page: Page<Size4KiB>,
        frame: PhysFrame,
        flags: PageTableFlags,
    ) -> Result<(), &'static str> {
        let mut mapper = self.create_mapper();
        let mut buddy_alloc = BuddyFrameAllocator;
        // Always use fully-permissive flags for intermediate tables.
        let parent_flags = PageTableFlags::PRESENT
            | PageTableFlags::WRITABLE
            | PageTableFlags::USER_ACCESSIBLE;
        mapper
            .map_to_with_table_flags(page, frame, flags, parent_flags, &mut buddy_alloc)
            .map_err(|_| "map_existing_frame: map_to failed")?
            .flush();
        Ok(())
    }

    /// Update the flags of an already-mapped page without changing its physical frame.
    /// Flushes the TLB entry via invlpg.
    /// Used for COW fork: mark parent's writable pages as read-only.
    pub unsafe fn update_page_flags(
        &self,
        page: Page<Size4KiB>,
        flags: PageTableFlags,
    ) -> Result<(), &'static str> {
        let mut mapper = self.create_mapper();
        mapper
            .update_flags(page, flags)
            .map_err(|_| "update_page_flags: failed")?
            .flush();
        Ok(())
    }

    /// Unmap a page then remap it to a new physical frame with new flags.
    /// Used in COW fault resolution: replace a shared read-only frame with
    /// a private writable copy.
    /// Intermediate PT frames are preserved (unmap does not free them).
    pub unsafe fn unmap_and_remap(
        &self,
        page: Page<Size4KiB>,
        new_frame: PhysFrame,
        flags: PageTableFlags,
    ) -> Result<(), &'static str> {
        let mut mapper = self.create_mapper();

        // Clear the leaf PT entry; intermediate tables remain intact.
        let (_, flush) = mapper
            .unmap(page)
            .map_err(|_| "unmap_and_remap: unmap failed")?;
        flush.flush();

        // Remap with new frame; intermediate tables are reused.
        let mut buddy_alloc = BuddyFrameAllocator;
        mapper
            .map_to(page, new_frame, flags, &mut buddy_alloc)
            .map_err(|_| "unmap_and_remap: map_to failed")?
            .flush();

        Ok(())
    }

    /// Unmap a single user page and free its backing physical frame.
    ///
    /// If the page is not mapped (not yet demand-paged), this is a no-op.
    /// Decrements the COW refcount; if it reaches zero, returns the frame
    /// to the Buddy allocator.  Intermediate page table frames are preserved.
    ///
    /// # Safety
    /// Must be called with interrupts disabled (cli).
    pub unsafe fn unmap_page_and_free(&self, page: Page<Size4KiB>) -> Result<(), &'static str> {
        let frame = match self.translate_page(page) {
            Some(f) => f,
            None => return Ok(()),  // never demand-paged; nothing to free
        };

        let mut mapper = self.create_mapper();
        let (_, flush) = mapper
            .unmap(page)
            .map_err(|_| "unmap_page_and_free: unmap failed")?;
        flush.flush();

        // Zero-frame is permanent — it has no refcount entry, never free it.
        if !crate::memory::cow::is_zero_frame(frame) {
            if crate::memory::cow::dec_ref(frame) == 0 {
                crate::allocator::phys_free(frame.start_address(), 12);
            }
        }

        Ok(())
    }

    /// Unmap a single 2 MiB huge page and free its backing physical frame.
    ///
    /// If the page is not mapped, this is a no-op.
    /// Huge frames are freed with order=21 directly (no COW refcount).
    ///
    /// # Safety
    /// Must be called with interrupts disabled (cli).
    pub unsafe fn unmap_page_and_free_2m(
        &self,
        page: Page<Size2MiB>,
    ) -> Result<(), &'static str> {
        let mut buddy = crate::allocator::buddy_allocator::BUDDY.lock();
        self.unmap_page_and_free_2m_with_buddy(page, &mut buddy)
    }

    /// Same as `unmap_page_and_free_2m`, but takes an already-locked Buddy
    /// instead of locking it itself — lets a caller that obtained the lock
    /// via `try_lock()` (because blocking isn't safe in its context, e.g.
    /// ISR/tick — see `AddressSpace::try_free_huge_vma`) reuse this without
    /// a second, nested `lock()` call.
    ///
    /// # Safety
    /// Must be called with interrupts disabled (cli).
    pub unsafe fn unmap_page_and_free_2m_with_buddy(
        &self,
        page: Page<Size2MiB>,
        buddy: &mut crate::allocator::buddy_allocator::BuddyAllocator,
    ) -> Result<(), &'static str> {
        let mut mapper = self.create_mapper();
        let (frame, flush) = match mapper.unmap(page) {
            Ok(r) => r,
            Err(_) => return Ok(()),  // not mapped — nothing to free
        };
        flush.flush();
        buddy.deallocate(frame.start_address(), 21);
        Ok(())
    }

    /// Walk all user-owned PML4 entries (indices 0, 226, and 128) and free:
    ///   - Data frames (leaf PT entries): dec_ref; if → 0, deallocate to Buddy.
    ///   - Intermediate frames (PT/PD/PDPT): deallocate directly (no refcount).
    ///   - PML4 frame itself: deallocated at the end.
    ///
    /// Called only for owned (user) page tables from `Drop`.
    unsafe fn release_user_pages(&self) {
        use x86_64::structures::paging::PageTable;

        crate::ktrace!(crate::debug::MM, "rpu: start PML4={:#x}", self.pml4_frame.start_address().as_u64());

        let phys_offset = crate::memory::physical_memory_offset();
        let pml4_virt = phys_offset + self.pml4_frame.start_address().as_u64();
        let pml4: &PageTable = &*pml4_virt.as_ptr::<PageTable>();

        for &pml4_idx in &USER_PML4_ENTRIES {
            let pml4_entry = &pml4[pml4_idx];
            if !pml4_entry.flags().contains(PageTableFlags::PRESENT) {
                continue;
            }
            let pdpt_frame = match pml4_entry.frame() {
                Ok(f) => f,
                Err(_) => continue,
            };

            crate::ktrace!(crate::debug::MM, "rpu: PML4[{}] → PDPT={:#x}", pml4_idx, pdpt_frame.start_address().as_u64());

            let pdpt_virt = phys_offset + pdpt_frame.start_address().as_u64();
            let pdpt: &PageTable = &*pdpt_virt.as_ptr::<PageTable>();

            for pdpt_entry in pdpt.iter() {
                if !pdpt_entry.flags().contains(PageTableFlags::PRESENT) {
                    continue;
                }
                let pd_frame = match pdpt_entry.frame() {
                    Ok(f) => f,
                    Err(_) => continue, // huge page — skip
                };
                crate::ktrace!(crate::debug::MM, "rpu:   PDPT entry → PD={:#x}", pd_frame.start_address().as_u64());

                let pd_virt = phys_offset + pd_frame.start_address().as_u64();
                let pd: &PageTable = &*pd_virt.as_ptr::<PageTable>();

                for (pd_idx, pd_entry) in pd.iter().enumerate() {
                    if !pd_entry.flags().contains(PageTableFlags::PRESENT) {
                        continue;
                    }
                    let pt_frame = match pd_entry.frame() {
                        Ok(f) => f,
                        Err(FrameError::HugeFrame) => {
                            crate::ktrace!(crate::debug::MM, "rpu:     PD[{}] huge={:#x} free order=21", pd_idx, pd_entry.addr().as_u64());
                            unsafe { crate::allocator::phys_free(pd_entry.addr(), 21); }
                            continue;
                        }
                        Err(_) => continue,
                    };

                    crate::ktrace!(crate::debug::MM, "rpu:     PD[{}] → PT={:#x}", pd_idx, pt_frame.start_address().as_u64());

                    let pt_virt = phys_offset + pt_frame.start_address().as_u64();
                    let pt: &PageTable = &*pt_virt.as_ptr::<PageTable>();

                    for pt_entry in pt.iter() {
                        if !pt_entry.flags().contains(PageTableFlags::PRESENT) {
                            continue;
                        }
                        if let Ok(data_frame) = pt_entry.frame() {
                            // Zero-frame is permanent — never free it.
                            if !crate::memory::cow::is_zero_frame(data_frame) {
                                let ref_after = crate::memory::cow::dec_ref(data_frame);
                                if ref_after == 0 {
                                    unsafe { crate::allocator::phys_free(data_frame.start_address(), 12); }
                                }
                            }
                        }
                    }

                    crate::ktrace!(crate::debug::MM, "rpu:     PT={:#x} done, freeing PT frame", pt_frame.start_address().as_u64());
                    unsafe { crate::allocator::phys_free(pt_frame.start_address(), 12); }
                    crate::ktrace!(crate::debug::MM, "rpu:     PT frame freed");
                }

                // PD is an intermediate frame — free directly.
                crate::ktrace!(crate::debug::MM, "rpu:   freeing PD={:#x}", pd_frame.start_address().as_u64());
                unsafe { crate::allocator::phys_free(pd_frame.start_address(), 12); }
                crate::ktrace!(crate::debug::MM, "rpu:   PD freed");
            }

            // PDPT is an intermediate frame — free directly.
            crate::ktrace!(crate::debug::MM, "rpu: freeing PDPT={:#x}", pdpt_frame.start_address().as_u64());
            unsafe { crate::allocator::phys_free(pdpt_frame.start_address(), 12); }
            crate::ktrace!(crate::debug::MM, "rpu: PDPT freed");
        }

        // Free the PML4 frame itself.
        crate::ktrace!(crate::debug::MM, "rpu: freeing PML4={:#x}", self.pml4_frame.start_address().as_u64());
        unsafe { crate::allocator::phys_free(self.pml4_frame.start_address(), 12); }
        crate::ktrace!(crate::debug::MM, "rpu: done");
    }

    /// Map `num_pages` contiguous user pages starting at `start`.
    pub unsafe fn map_user_pages(
        &self,
        start: VirtAddr,
        num_pages: usize,
        flags: PageTableFlags,
    ) -> Result<(), &'static str> {
        for i in 0..num_pages {
            let page_addr = start + (i as u64 * 4096);
            let page: Page<Size4KiB> = Page::containing_address(page_addr);
            self.map_user_page(page, flags)
                .map_err(|_| "Failed to map user page")?;
        }
        Ok(())
    }

    /// Write raw bytes into a physical frame via the phys offset.
    pub unsafe fn write_to_frame(frame: PhysFrame, data: &[u8], offset: usize) {
        let phys_offset = crate::memory::physical_memory_offset();
        let dst = (phys_offset + frame.start_address().as_u64())
            .as_mut_ptr::<u8>()
            .add(offset);
        core::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
    }

    /// Zero an entire 4 KiB physical frame.
    pub unsafe fn zero_frame(frame: PhysFrame) {
        let phys_offset = crate::memory::physical_memory_offset();
        let virt = phys_offset + frame.start_address().as_u64();
        core::ptr::write_bytes(virt.as_mut_ptr::<u8>(), 0, 4096);
    }

    /// Read the raw 64-bit PTE for `page` by manually walking this page table.
    /// Returns [pml4_e, pdpt_e, pd_e, pt_e].  Any level missing → 0 for that
    /// and all subsequent entries.  Used for COW diagnostics only.
    pub unsafe fn get_pte_all_levels(&self, page: Page<Size4KiB>) -> [u64; 4] {
        let phys_offset = crate::memory::physical_memory_offset();
        let virt = page.start_address().as_u64();
        let pml4_idx = ((virt >> 39) & 0x1FF) as usize;
        let pdpt_idx = ((virt >> 30) & 0x1FF) as usize;
        let pd_idx   = ((virt >> 21) & 0x1FF) as usize;
        let pt_idx   = ((virt >> 12) & 0x1FF) as usize;

        let pml4 = &*(phys_offset + self.pml4_phys().as_u64()).as_ptr::<[u64; 512]>();
        let e4 = pml4[pml4_idx];
        if e4 & 1 == 0 { return [e4, 0, 0, 0]; }

        let pdpt = &*((phys_offset + (e4 & 0x000F_FFFF_FFFF_F000)).as_ptr::<[u64; 512]>());
        let e3 = pdpt[pdpt_idx];
        if e3 & 1 == 0 { return [e4, e3, 0, 0]; }

        let pd = &*((phys_offset + (e3 & 0x000F_FFFF_FFFF_F000)).as_ptr::<[u64; 512]>());
        let e2 = pd[pd_idx];
        if e2 & 1 == 0 { return [e4, e3, e2, 0]; }

        let pt = &*((phys_offset + (e2 & 0x000F_FFFF_FFFF_F000)).as_ptr::<[u64; 512]>());
        let e1 = pt[pt_idx];
        [e4, e3, e2, e1]
    }

    /// Convenience wrapper: return only the leaf PTE.
    pub unsafe fn get_pte_raw(&self, page: Page<Size4KiB>) -> u64 {
        self.get_pte_all_levels(page)[3]
    }
}

impl Drop for OwnedPageTable {
    fn drop(&mut self) {
        if !self.owned {
            // Kernel page table (from_current) — never free, it belongs to the kernel.
            return;
        }
        unsafe { self.release_user_pages(); }
    }
}

// ============================================================================
// Kernel stack guard pages
// ============================================================================
//
// Kernel stacks are plain physmap addresses (physical_memory_offset() +
// phys_addr from a Buddy allocation) — see init::processes::allocate_kernel_stack.
// The physmap itself is built by the `bootloader` crate before our code runs,
// using 2MiB pages, so an overflow doesn't fault: it silently corrupts
// whatever Buddy block happens to sit at the next-lower address, which is
// exactly what init::processes::KERNEL_STACK_ORDER's doc comment describes.
//
// `unmap_kernel_guard_page` fixes this for real: it splits the 2MiB page
// covering a given address into 4KiB pages (if not already split) and
// clears the leaf entry for that one page, so any access to it faults
// instead of reading/writing real memory.

/// Split the 2MiB huge page in the KERNEL's page table covering `virt_addr`
/// into 4KiB pages. No-op if it's already split (by an earlier call covering
/// a different stack in the same 2MiB region).
///
/// # Why this is safe to do once, globally, from the kernel's own table
/// `OwnedPageTable::new_user` copies kernel PML4 entries by *cloning the
/// entry itself* (a pointer to a PDPT frame), not by deep-copying the
/// PDPT/PD/PT chain underneath it. So every process's page table shares the
/// exact same PD/PT frames for the physmap region — splitting it once here
/// (via the kernel's own unowned table, from `Cr3::read()`) is immediately
/// visible to every process, current and future, with no per-process work
/// needed.
///
/// # Safety
/// Caller must ensure `virt_addr` falls within the physmap (i.e. is
/// `physical_memory_offset() + some_phys_addr`), and that no other CPU
/// could be concurrently walking/modifying this same PD (true here: single
/// core, and this only ever runs with a Buddy block freshly allocated by
/// the caller, before it's handed to anyone else).
unsafe fn split_physmap_2m(virt_addr: VirtAddr) -> Result<(), &'static str> {
    let phys_offset = crate::memory::physical_memory_offset();
    let (kernel_frame, _) = Cr3::read();

    let va = virt_addr.as_u64();
    let pml4_idx = ((va >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((va >> 30) & 0x1FF) as usize;
    let pd_idx   = ((va >> 21) & 0x1FF) as usize;

    let pml4_virt = phys_offset + kernel_frame.start_address().as_u64();
    let pml4: &PageTable = &*pml4_virt.as_ptr::<PageTable>();
    let pml4_entry = &pml4[pml4_idx];
    if !pml4_entry.flags().contains(PageTableFlags::PRESENT) {
        return Err("split_physmap_2m: PML4 entry not present");
    }
    let pdpt_frame = pml4_entry.frame().map_err(|_| "split_physmap_2m: bad PML4 entry")?;

    let pdpt_virt = phys_offset + pdpt_frame.start_address().as_u64();
    let pdpt: &PageTable = &*pdpt_virt.as_ptr::<PageTable>();
    let pdpt_entry = &pdpt[pdpt_idx];
    if !pdpt_entry.flags().contains(PageTableFlags::PRESENT) {
        return Err("split_physmap_2m: PDPT entry not present");
    }
    let pd_frame = pdpt_entry.frame()
        .map_err(|_| "split_physmap_2m: PDPT entry is a 1GiB huge page (unsupported)")?;

    let pd_virt = phys_offset + pd_frame.start_address().as_u64();
    let pd: &mut PageTable = &mut *pd_virt.as_mut_ptr::<PageTable>();
    let pd_entry = &mut pd[pd_idx];

    if !pd_entry.flags().contains(PageTableFlags::HUGE_PAGE) {
        return Ok(()); // already split by an earlier call — nothing to do
    }

    let huge_phys_base = pd_entry.addr();
    let huge_flags = pd_entry.flags();
    let leaf_flags = huge_flags & !PageTableFlags::HUGE_PAGE;

    let new_pt_phys = crate::allocator::phys_alloc(12)
        .ok_or("split_physmap_2m: OOM allocating replacement PT")?;
    let new_pt_virt = phys_offset + new_pt_phys.as_u64();
    let new_pt: &mut PageTable = &mut *new_pt_virt.as_mut_ptr::<PageTable>();
    new_pt.zero();

    for i in 0..512u64 {
        let frame: PhysFrame = PhysFrame::containing_address(huge_phys_base + i * 4096);
        new_pt[i as usize].set_frame(frame, leaf_flags);
    }

    // Table-descriptor entry: PRESENT|WRITABLE regardless of leaf
    // permissions (same convention as map_existing_frame's parent_flags —
    // restriction belongs on the leaf, not the pointer to it). GLOBAL is
    // leaf-only, dropped here; USER_ACCESSIBLE carried over defensively
    // even though physmap is never user-mapped in practice.
    let parent_flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | (huge_flags & PageTableFlags::USER_ACCESSIBLE);
    pd_entry.set_frame(PhysFrame::containing_address(new_pt_phys), parent_flags);

    // Cold, rare, structural change — a full flush is simpler and safer
    // than invlpg-ing 512 individual addresses one at a time.
    x86_64::instructions::tlb::flush_all();

    Ok(())
}

/// Walk the kernel's own page table down to the 4KiB-granular PT covering
/// `virt_addr`, returning `(PT, pt_idx)`. Requires the covering 2MiB region
/// to already be split (see `split_physmap_2m`) — errors if it's still a
/// huge page.
unsafe fn walk_to_pt(virt_addr: VirtAddr) -> Result<(&'static mut PageTable, usize), &'static str> {
    let phys_offset = crate::memory::physical_memory_offset();
    let (kernel_frame, _) = Cr3::read();

    let va = virt_addr.as_u64();
    let pml4_idx = ((va >> 39) & 0x1FF) as usize;
    let pdpt_idx = ((va >> 30) & 0x1FF) as usize;
    let pd_idx   = ((va >> 21) & 0x1FF) as usize;
    let pt_idx   = ((va >> 12) & 0x1FF) as usize;

    let pml4_virt = phys_offset + kernel_frame.start_address().as_u64();
    let pml4: &PageTable = &*pml4_virt.as_ptr::<PageTable>();
    let pdpt_frame = pml4[pml4_idx].frame().map_err(|_| "walk_to_pt: bad PML4 entry")?;

    let pdpt_virt = phys_offset + pdpt_frame.start_address().as_u64();
    let pdpt: &PageTable = &*pdpt_virt.as_ptr::<PageTable>();
    let pd_frame = pdpt[pdpt_idx].frame().map_err(|_| "walk_to_pt: bad PDPT entry")?;

    let pd_virt = phys_offset + pd_frame.start_address().as_u64();
    let pd: &PageTable = &*pd_virt.as_ptr::<PageTable>();
    let pt_frame = pd[pd_idx].frame().map_err(|_| "walk_to_pt: PD entry not a 4KiB PT")?;

    let pt_virt = phys_offset + pt_frame.start_address().as_u64();
    let pt: &mut PageTable = &mut *pt_virt.as_mut_ptr::<PageTable>();
    Ok((pt, pt_idx))
}

/// Unmap the single 4KiB page at `virt_addr` in the physmap, turning any
/// access to it into a page fault. Splits the covering 2MiB page first if
/// needed (see `split_physmap_2m`).
///
/// # Safety
/// `virt_addr` must be the base of a Buddy block the caller exclusively
/// owns (e.g. the bottom of a freshly `phys_alloc`'d kernel stack) — see
/// `split_physmap_2m`'s doc comment for why that ownership is what makes
/// this safe to do process-table-wide instead of just locally.
pub unsafe fn unmap_kernel_guard_page(virt_addr: VirtAddr) -> Result<(), &'static str> {
    split_physmap_2m(virt_addr)?;
    let (pt, pt_idx) = walk_to_pt(virt_addr)?;
    pt[pt_idx].set_unused();
    x86_64::instructions::tlb::flush(virt_addr);
    Ok(())
}

/// Undo `unmap_kernel_guard_page`: restore a normal present mapping for
/// `virt_addr` (identity within the physmap: this VA maps its own
/// `virt_addr - physical_memory_offset()` physical frame).
///
/// MUST be called before returning a kernel stack's block to the Buddy
/// allocator. Buddy's free lists are intrusive — `add_block` writes the
/// linked-list node directly into the freed block's first bytes via the
/// physmap, at exactly the address this guard page unmapped. Skipping this
/// call turns the next `free_kernel_stack`/`try_free_kernel_stack` into a
/// guaranteed kernel-mode page fault (a write to a page we ourselves just
/// made not-present).
///
/// Takes its flags from `virt_addr`'s immediate neighbour (`pt_idx + 1`,
/// the next page of the same stack block) rather than hardcoding
/// PRESENT|WRITABLE|GLOBAL — that neighbour was never unmapped (the guard
/// is always the block's *first* page, and Buddy's minimum block here is
/// 16 pages), so its flags are exactly what this page had before it became
/// a guard.
///
/// # Safety
/// `virt_addr` must be a page previously unmapped by `unmap_kernel_guard_page`.
pub unsafe fn remap_kernel_guard_page(virt_addr: VirtAddr) -> Result<(), &'static str> {
    let phys_offset = crate::memory::physical_memory_offset();
    let (pt, pt_idx) = walk_to_pt(virt_addr)?;

    let template_flags = pt[pt_idx + 1].flags();
    let phys = PhysAddr::new(virt_addr.as_u64() - phys_offset.as_u64());
    pt[pt_idx].set_frame(PhysFrame::containing_address(phys), template_flags);

    x86_64::instructions::tlb::flush(virt_addr);
    Ok(())
}