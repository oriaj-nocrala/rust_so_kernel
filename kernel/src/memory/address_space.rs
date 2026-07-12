// kernel/src/memory/address_space.rs
//
// AddressSpace: groups a process's page table + VMAs into a single
// unit that does NOT depend on PID.
//
// This is the only structural addition of the refactor.  Everything
// else is wiring changes.

use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;
use x86_64::{
    VirtAddr,
    structures::paging::{Page, PageTableFlags, PhysFrame, Size2MiB, Size4KiB, mapper::MapToError},
};

use super::page_table_manager::{OwnedPageTable, USER_MMAP_BASE};
use super::vma::{Vma, VmaKind, VmaList};

/// `vmas` and `mmap_base` use interior mutability (`Mutex`/`AtomicU64`
/// instead of requiring `&mut self`) so that `AddressSpace` can be shared
/// via `Arc` between multiple `Process`es (real threads created by
/// `clone()`, which all run in the same address space). Everything else
/// here already only needs `&self` — page-table mutation goes through raw
/// pointer writes into the physical-offset-mapped page table memory itself,
/// not through Rust-level `&mut`.
pub struct AddressSpace {
    pub page_table: OwnedPageTable,
    vmas: Mutex<VmaList>,
    /// Bump pointer for kernel-assigned anonymous mmap addresses.
    /// Starts at USER_MMAP_BASE; advances on each mmap allocation.
    mmap_base: AtomicU64,
}

// SAFETY: same invariant as the existing `Send` impl below — this kernel is
// single-CPU with cli-discipline around every scheduler/address-space
// mutation, so there is never true concurrent access. `Sync` is required so
// `Arc<AddressSpace>` (used to share an address space between the Processes
// that make up a real thread group created by clone()) is itself `Send`.
unsafe impl Sync for AddressSpace {}

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
            vmas: Mutex::new(VmaList::new()),
            mmap_base: AtomicU64::new(USER_MMAP_BASE),
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
            vmas: Mutex::new(VmaList::new()),
            mmap_base: AtomicU64::new(USER_MMAP_BASE),
        })
    }

    // ====================================================================
    // VMA MANAGEMENT
    // ====================================================================

    /// Register a virtual memory area.
    pub fn add_vma(&self, vma: Vma) -> Result<(), &'static str> {
        self.vmas.lock().add(vma)
    }

    /// Find the VMA containing `addr`, if any.
    /// Returns a copy (Vma is Copy).
    pub fn find_vma(&self, addr: u64) -> Option<Vma> {
        self.vmas.lock().find(addr).copied()
    }

    /// Debug: print all VMAs (uses serial, no allocation).
    pub fn dump_vmas(&self, label: usize) {
        self.vmas.lock().dump(label);
    }

    // ====================================================================
    // PAGE TABLE DELEGATION
    // ====================================================================

    /// Activate this address space (write CR3).
    /// No-op if already active.
    pub unsafe fn activate(&self) {
        self.page_table.activate();
    }

    /// Look up the physical frame for an already-mapped page.
    /// Returns `None` if the page is not present.
    pub unsafe fn translate_page(&self, page: Page<Size4KiB>) -> Option<PhysFrame> {
        self.page_table.translate_page(page)
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

    // ====================================================================
    // FORK (Copy-on-Write)
    // ====================================================================

    /// Create a child address space using Copy-on-Write semantics.
    ///
    /// - Creates a fresh PML4 with kernel entries copied.
    /// - Copies the VMA list verbatim (same virtual ranges).
    /// - For every page already present in self:
    ///     * Marks the parent's page as read-only (COW protection).
    ///     * Maps the SAME physical frame into the child (also read-only).
    ///     * Increments the frame's refcount (1 → 2).
    /// - Pages not yet demand-paged are NOT mapped; parent and child will
    ///   each fault and map independently.
    ///
    /// COW faults are resolved by `handle_cow_fault` called from the page
    /// fault handler in `init/devices.rs`.
    ///
    /// # Safety
    /// Buddy allocator must be initialized.  Call with interrupts disabled.
    pub unsafe fn fork(&self) -> Result<Self, &'static str> {
        let child = Self::new_user()?;
        let vmas_snapshot = self.vmas.lock().clone();
        *child.vmas.lock() = vmas_snapshot.clone();
        child.mmap_base.store(self.mmap_base.load(Ordering::Relaxed), Ordering::Relaxed);

        for vma in vmas_snapshot.iter() {
            let orig_flags = vma.page_table_flags();
            // Shared mapping is always read-only regardless of original flags.
            let shared_flags = orig_flags & !PageTableFlags::WRITABLE;

            for page_idx in 0..vma.size_pages {
                let addr = vma.start + page_idx as u64 * 4096;
                let page = Page::<Size4KiB>::containing_address(VirtAddr::new(addr));

                // Only share pages that are already mapped in the parent.
                // Unmapped anonymous pages (stack, heap not yet touched) will
                // be demand-paged independently by parent and child.
                let src_frame = match self.translate_page(page) {
                    Some(f) => f,
                    None => continue,
                };

                // Two VMAs can overlap the same page (e.g. .text/.rodata sharing
                // a 4K boundary).  Skip if the child already has this page.
                if child.translate_page(page).is_some() {
                    continue;
                }

                // Mark the parent's page read-only (COW protection).
                if orig_flags.contains(PageTableFlags::WRITABLE) {
                    self.page_table.update_page_flags(page, shared_flags)?;
                }

                // Map the same frame in the child (read-only).
                child.page_table.map_existing_frame(page, src_frame, shared_flags)?;

                // refcount: 1 → 2 (shared between parent and child).
                crate::memory::cow::inc_ref(src_frame);
            }
        }

        Ok(child)
    }

    /// Resolve a COW write fault at `fault_addr`.
    ///
    /// Two cases:
    ///   - refcount ≤ 1 (last owner): just restore WRITABLE — no copy needed.
    ///   - refcount ≥ 2 (shared): allocate a new frame, copy 4 KiB, remap,
    ///     decrement the old frame's refcount.
    ///
    /// `vma_flags` must be the original flags from the faulting VMA
    /// (including WRITABLE so the restored mapping is writable).
    ///
    /// Takes `&self` so it is compatible with `running_ref()`.
    ///
    /// # Safety
    /// Must be called with interrupts disabled (cli), which protects
    /// refcount operations and the page table walk.
    pub unsafe fn handle_cow_fault(
        &self,
        fault_addr: u64,
        vma_flags: PageTableFlags,
    ) -> Result<(), &'static str> {
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(fault_addr));
        let old_frame = self.translate_page(page).ok_or("COW: page not mapped")?;

        // ── Zero-page: promote the shared zero frame to a private writable copy.
        // Must be checked BEFORE the refcount path (zero frame has refcount 0).
        if crate::memory::cow::is_zero_frame(old_frame) {
            let phys_offset = crate::memory::physical_memory_offset();
            let new_frame = crate::allocator::phys_alloc(12)
                .map(|a| PhysFrame::containing_address(a))
                .ok_or("COW zero-frame: OOM")?;
            crate::memory::cow::set_ref(new_frame, 1);
            let dst = (phys_offset + new_frame.start_address().as_u64()).as_mut_ptr::<u8>();
            core::ptr::write_bytes(dst, 0, 4096);
            // Do NOT dec_ref the zero frame — it is permanent.
            crate::serial_println!(
                "[COW] zero-frame promotion at {:#x} → new_frame {:#x}",
                fault_addr, new_frame.start_address().as_u64()
            );
            return self.page_table.unmap_and_remap(page, new_frame, vma_flags);
        }

        let refcount = crate::memory::cow::get_ref(old_frame);

        let cr3 = unsafe {
            let cr3_val: u64;
            core::arch::asm!("mov {}, cr3", out(reg) cr3_val);
            cr3_val
        };
        crate::serial_println!(
            "[COW] addr={:#x} old_frame={:#x} ref={} vma_flags={:#x} pml4={:#x} cr3={:#x}",
            fault_addr,
            old_frame.start_address().as_u64(),
            refcount,
            vma_flags.bits(),
            self.page_table.pml4_phys().as_u64(),
            cr3,
        );

        if refcount <= 1 {
            // Last owner — just restore the WRITABLE flag (no copy).
            let levels_before = self.page_table.get_pte_all_levels(page);
            crate::serial_println!(
                "[COW] path=update_flags  PTE_all=[{:#x},{:#x},{:#x},{:#x}]",
                levels_before[0], levels_before[1], levels_before[2], levels_before[3]
            );
            let r = self.page_table.update_page_flags(page, vma_flags);
            let pte_after = self.page_table.get_pte_raw(page);
            crate::serial_println!("[COW] update_flags result={} PTE_leaf_after={:#x}",
                r.is_ok(), pte_after);
            r
        } else {
            // Shared frame — allocate a new frame and copy.
            let phys_offset = crate::memory::physical_memory_offset();

            let new_frame = unsafe {
                crate::allocator::phys_alloc(12)
                    .map(|a| PhysFrame::containing_address(a))
                    .ok_or("COW: out of memory")?
            };
            crate::serial_println!(
                "[COW] path=copy new_frame={:#x}",
                new_frame.start_address().as_u64()
            );
            crate::memory::cow::set_ref(new_frame, 1);

            // Copy 4 KiB from the shared frame to the private frame.
            let src = (phys_offset + old_frame.start_address().as_u64()).as_ptr::<u8>();
            let dst = (phys_offset + new_frame.start_address().as_u64()).as_mut_ptr::<u8>();
            core::ptr::copy_nonoverlapping(src, dst, 4096);

            // Replace the page table entry with the new private frame.
            let r = self.page_table.unmap_and_remap(page, new_frame, vma_flags);
            crate::serial_println!("[COW] unmap_and_remap result: {}", r.is_ok());

            // Verify fix
            if let Some(f) = self.translate_page(page) {
                crate::serial_println!(
                    "[COW] after fix: frame={:#x} (expected {:#x})",
                    f.start_address().as_u64(),
                    new_frame.start_address().as_u64()
                );
            } else {
                crate::serial_println!("[COW] after fix: translate_page returned None!");
            }

            // Drop our share of the old frame.
            if crate::memory::cow::dec_ref(old_frame) == 0 {
                unsafe { crate::allocator::phys_free(old_frame.start_address(), 12); }
            }

            r
        }
    }

    // ====================================================================
    // MMAP / MUNMAP
    // ====================================================================

    /// Map an anonymous (zero-initialized, demand-paged) region.
    ///
    /// If `addr == 0`: kernel picks the address via the bump pointer.
    /// If `addr != 0`: used as MAP_FIXED — must be page-aligned and non-overlapping.
    ///
    /// `prot` bits: PROT_READ=1, PROT_WRITE=2 (PROT_EXEC ignored — NX not enabled).
    /// `length` is rounded up to the next page boundary.
    ///
    /// Returns the mapped virtual address on success.
    /// No physical frames are allocated here; the demand paging fault handler
    /// handles first-touch allocation for Anonymous VMAs.
    pub fn sys_mmap_anon(
        &self,
        addr: u64,
        length: u64,
        prot: u32,
    ) -> Result<u64, &'static str> {
        if length == 0 {
            return Err("mmap: zero length");
        }

        const PROT_WRITE: u32 = 2;
        let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        if prot & PROT_WRITE != 0 {
            flags |= PageTableFlags::WRITABLE;
        }

        // ── Huge pages (2 MiB) for large allocations ──────────────────
        const HUGE_2M: u64 = 0x200_000;
        if length >= HUGE_2M {
            let length_aligned = (length + HUGE_2M - 1) & !(HUGE_2M - 1);
            let size_pages = (length_aligned / 4096) as usize; // in 4 KiB units

            let vaddr = if addr == 0 {
                // Align bump pointer up to 2 MiB boundary.
                let base = (self.mmap_base.load(Ordering::Relaxed) + HUGE_2M - 1) & !(HUGE_2M - 1);
                // Advance past the allocation + one 2 MiB guard region.
                self.mmap_base.store(base + length_aligned + HUGE_2M, Ordering::Relaxed);
                base
            } else {
                if addr & (HUGE_2M - 1) != 0 {
                    return Err("mmap: huge page addr not 2MB-aligned");
                }
                if self.vmas.lock().overlaps(addr, size_pages) {
                    return Err("mmap: MAP_FIXED conflict with existing VMA");
                }
                addr
            };

            let vma = Vma {
                start: vaddr,
                size_pages,
                flags: flags.bits(),
                kind: VmaKind::Huge2M,
            };
            self.vmas.lock().add(vma).map_err(|_| "mmap: VMA list full")?;
            return Ok(vaddr);
        }

        // ── Normal 4 KiB anonymous pages ──────────────────────────────
        let size_pages = ((length + 4095) / 4096) as usize;

        let vaddr = if addr == 0 {
            let base = self.mmap_base.load(Ordering::Relaxed);
            // Advance bump pointer; add one guard page between allocations.
            self.mmap_base.store(base + size_pages as u64 * 4096 + 4096, Ordering::Relaxed);
            base
        } else {
            if addr & 0xFFF != 0 {
                return Err("mmap: addr not page-aligned");
            }
            if self.vmas.lock().overlaps(addr, size_pages) {
                return Err("mmap: MAP_FIXED conflict with existing VMA");
            }
            addr
        };

        // PRESENT is required so intermediate page-table entries get the
        // PRESENT bit set during demand paging — without it, map_to sets
        // intermediate entries without PRESENT, the CPU re-faults, and the
        // second map_to call panics in create_or_next_table_mut.
        let vma = Vma {
            start: vaddr,
            size_pages,
            flags: flags.bits(),
            kind: VmaKind::Anonymous,
        };
        self.vmas.lock().add(vma).map_err(|_| "mmap: VMA list full")?;

        Ok(vaddr)
    }

    /// Unmap an anonymous region previously created by `sys_mmap_anon`.
    ///
    /// Currently requires an exact match on `addr` (the VMA start address).
    /// The `length` must also match the VMA size, rounded up to pages.
    /// Partial unmapping returns `Err`.
    ///
    /// For each page that was demand-paged (physically mapped), decrements
    /// the COW refcount and frees the frame to Buddy if the count reaches zero.
    ///
    /// # Safety
    /// Must be called with interrupts disabled (cli).
    pub unsafe fn sys_munmap(&self, addr: u64, length: u64) -> Result<(), &'static str> {
        if addr & 0xFFF != 0 {
            return Err("munmap: addr not page-aligned");
        }
        if length == 0 {
            return Err("munmap: zero length");
        }

        let size_pages = ((length + 4095) / 4096) as usize;
        let vma = self.vmas.lock().remove(addr).map_err(|_| "munmap: VMA not found")?;

        if vma.size_pages != size_pages {
            // Re-insert and signal partial munmap is unsupported.
            let _ = self.vmas.lock().add(vma);
            return Err("munmap: partial unmap not supported");
        }

        match vma.kind {
            VmaKind::Anonymous | VmaKind::Code => {
                for i in 0..vma.size_pages {
                    let va = vma.start + i as u64 * 4096;
                    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(va));
                    self.page_table.unmap_page_and_free(page)?;
                }
            }
            VmaKind::Huge2M => {
                // size_pages is in 4 KiB units; each huge page covers 512 of them.
                let n_huge = vma.size_pages / 512;
                for i in 0..n_huge {
                    let va = vma.start + i as u64 * 0x200_000;
                    let page = Page::<Size2MiB>::containing_address(VirtAddr::new(va));
                    self.page_table.unmap_page_and_free_2m(page)?;
                }
            }
        }

        Ok(())
    }

    /// Non-blocking munmap of a Huge2M VMA — used to free a dead thread's
    /// stack (see `Process::owned_stack_vma`) from `Scheduler::tick`'s
    /// `pending_vma_frees` drain, which runs in timer-ISR context and can't
    /// block on the Buddy lock for the same reason `kernel_stack`'s
    /// deferred free can't — see `init::processes::try_free_kernel_stack`'s
    /// doc comment for the full story (an ISR blocking on a lock some
    /// interrupted code already holds deadlocks the whole single core).
    ///
    /// Returns `false` (try again next tick) if the Buddy lock is
    /// contended, instead of the `Result` `sys_munmap` uses — there's no
    /// caller here to hand an error to.
    ///
    /// Only handles Huge2M (what `sys_clone` only ever records — see its
    /// doc comment) — no COW refcount involved, unlike the 4 KiB Anonymous
    /// path `sys_munmap` also supports.
    pub unsafe fn try_free_huge_vma(&self, start: u64, size_pages: usize) -> bool {
        let mut buddy = match crate::allocator::buddy_allocator::BUDDY.try_lock() {
            Some(b) => b,
            None => return false,
        };

        let n_huge = size_pages / 512;
        for i in 0..n_huge {
            let va = start + i as u64 * 0x200_000;
            let page = Page::<Size2MiB>::containing_address(VirtAddr::new(va));
            let _ = self.page_table.unmap_page_and_free_2m_with_buddy(page, &mut buddy);
        }
        let _ = self.vmas.lock().remove(start);
        true
    }
}