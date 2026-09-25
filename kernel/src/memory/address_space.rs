// kernel/src/memory/address_space.rs
//
// AddressSpace: groups a process's page table + VMAs into a single
// unit that does NOT depend on PID.
//
// This is the only structural addition of the refactor.  Everything
// else is wiring changes.

use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::{
    VirtAddr,
    structures::paging::{Page, PageTableFlags, PhysFrame, Size2MiB, Size4KiB, mapper::MapToError},
};

use super::page_table_manager::{OwnedPageTable, USER_MMAP_BASE};
use super::vma::{Vma, VmaKind, VmaList};
use crate::allocator::KernelIrq;
use diag::IrqMutex;

/// Why a fault in this address space could not be resolved.
pub enum FaultError {
    /// No VMA covers the address (and no stack could grow to it).
    NoVma,
    /// A VMA covers it, but mapping failed (OOM, write to a read-only VMA…).
    Failed(&'static str),
}

/// `AddressSpace` is shared via `Arc` between the `Process`es of a thread
/// group (`clone()`), so everything here takes `&self`.
///
/// **`vmas` is also the lock for this address space's page table**
/// (stage 6 of `docs/smp/smp-plan.md`): every PTE change of a user mapping
/// — demand paging, COW resolution, `fork`'s write-protect, `munmap`, the
/// kernel's copies into another process's buffer — happens inside
/// `vmas.with`, and so does the VMA lookup that justified it. Two threads
/// on two CPUs faulting on one page then serialise, and the second finds
/// the page already resolved (`handle_*_fault` treat that as success — a
/// spurious fault, not an error); a `munmap` cannot free a frame between a
/// fault's VMA lookup and its mapping; and a COW decision ("refcount 1, I
/// am the last owner") cannot be invalidated by a sibling's `fork()`,
/// which is the only way a count rises. An `IrqMutex` because the page
/// fault handler takes it (IF=0) and so do syscalls: a holder preempted
/// with IF=1 would leave a faulting sibling thread spinning on its own
/// CPU forever — true on one CPU already, for the plain `spin::Mutex` this
/// replaced. Lock order: this → `BUDDY`/`SLAB_ALLOCATOR`; nothing is taken
/// before it except the scheduler lock (`sys_fork`/`sys_munmap` via
/// `with_current_process`). Never touch user memory through its virtual
/// address while holding it: that fault would take it again.
pub struct AddressSpace {
    pub page_table: OwnedPageTable,
    vmas: IrqMutex<VmaList, KernelIrq>,
    /// Bump pointer for kernel-assigned anonymous mmap addresses.
    /// Starts at USER_MMAP_BASE; advances on each mmap allocation. Only
    /// changed under `vmas`; atomic so reading it needs no lock.
    mmap_base: AtomicU64,
}

// SAFETY: the page table is changed only under `vmas` (see above), and
// every other field is itself `Sync`. `Sync` is what lets
// `Arc<AddressSpace>` be shared by a thread group.
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
            vmas: IrqMutex::new(VmaList::new()),
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
            vmas: IrqMutex::new(VmaList::new()),
            mmap_base: AtomicU64::new(USER_MMAP_BASE),
        })
    }

    // ====================================================================
    // VMA MANAGEMENT
    // ====================================================================

    /// Register a virtual memory area.
    pub fn add_vma(&self, vma: Vma) -> Result<(), &'static str> {
        self.vmas.with(|v| v.add(vma))
    }

    /// Find the VMA containing `addr`, if any.
    /// Returns a copy (Vma is Copy).
    pub fn find_vma(&self, addr: u64) -> Option<Vma> {
        self.vmas.with(|v| v.find(addr).copied())
    }

    /// Debug: print all VMAs (uses serial, no allocation).
    pub fn dump_vmas(&self, label: usize) {
        self.vmas.with(|v| v.dump(label));
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
        self.vmas.with(|_| self.page_table.map_user_page(page, flags))
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
        // The whole walk under the parent's lock: a sibling thread's fault
        // or munmap on another CPU must not change a PTE between reading
        // it and write-protecting it. The child is not shared with anyone
        // yet; its lock is taken only because it is the way in.
        self.vmas.with(|vmas| {
            child.vmas.with(|c| *c = vmas.clone());
            child.mmap_base.store(self.mmap_base.load(Ordering::Relaxed), Ordering::Relaxed);

            for vma in vmas.iter() {
                let orig_flags = vma.page_table_flags();
                // Shared mapping is always read-only regardless of original flags.
                let shared_flags = orig_flags & !PageTableFlags::WRITABLE;

                for page_idx in 0..vma.size_pages {
                    let addr = vma.start + page_idx as u64 * 4096;
                    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(addr));

                    // Only share pages that are already mapped in the parent.
                    // Unmapped anonymous pages (stack, heap not yet touched) will
                    // be demand-paged independently by parent and child.
                    let src_frame = match self.page_table.translate_page(page) {
                        Some(f) => f,
                        None => continue,
                    };

                    // Two VMAs can overlap the same page (e.g. .text/.rodata sharing
                    // a 4K boundary).  Skip if the child already has this page.
                    if child.page_table.translate_page(page).is_some() {
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
            Ok::<(), &'static str>(())
        })?;

        Ok(child)
    }

    /// A not-present fault at `fault_addr`, from the page fault handler:
    /// find the VMA (growing a `GrowableStack` down to it if that is what
    /// the address is — see `VmaList::grow_stack`) and demand-map the page.
    /// `is_write` picks a private zeroed frame over the shared zero frame.
    ///
    /// The page being mapped already is not an error: a sibling thread on
    /// another CPU faulted on it first and won the lock.
    ///
    /// # Safety
    /// Buddy allocator initialized; not called with this lock held.
    pub unsafe fn handle_not_present_fault(&self, fault_addr: u64, is_write: bool) -> Result<(), FaultError> {
        self.vmas.with(|vmas| {
            let vma = vmas
                .find(fault_addr)
                .copied()
                .or_else(|| vmas.grow_stack(fault_addr))
                .ok_or(FaultError::NoVma)?;
            if self.page_table.is_mapped(VirtAddr::new(fault_addr)) {
                return Ok(());
            }
            super::demand_paging::map_demand_page(&self.page_table, fault_addr, &vma, is_write)
                .map_err(FaultError::Failed)
        })
    }

    /// A write fault on a present page at `fault_addr` (a COW-shared frame
    /// or the zero frame), from the page fault handler.
    ///
    /// # Safety
    /// As [`Self::handle_not_present_fault`].
    pub unsafe fn handle_cow_fault(&self, fault_addr: u64) -> Result<(), FaultError> {
        let r = self.vmas.with(|vmas| {
            let vma = vmas.find(fault_addr).copied().ok_or(FaultError::NoVma)?;
            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(fault_addr));
            self.make_writable_locked(page, vma.page_table_flags()).map_err(FaultError::Failed)
        });
        if r.is_ok() { crate::debug::inc_cow_resolved(); } else { crate::debug::inc_cow_failed(); }
        r
    }

    /// Give `page` a private, writable frame: resolve COW sharing or the
    /// zero frame. Already writable is success (a sibling thread got there
    /// first). Caller holds `vmas`; `vma_flags` are the covering VMA's.
    ///
    ///   - zero frame: a fresh zeroed frame (the zero frame is never counted);
    ///   - refcount ≤ 1 (last owner): just restore WRITABLE — no copy;
    ///   - refcount ≥ 2 (shared): copy into a new frame, swap it in with one
    ///     PTE store (`replace_frame`), drop our share of the old one.
    unsafe fn make_writable_locked(&self, page: Page<Size4KiB>, vma_flags: PageTableFlags) -> Result<(), &'static str> {
        use crate::debug::MM;

        if !vma_flags.contains(PageTableFlags::WRITABLE) {
            return Err("COW: write to a read-only VMA");
        }
        let pte = self.page_table.get_pte_raw(page);
        if pte & PageTableFlags::PRESENT.bits() == 0 {
            return Err("COW: page not mapped");
        }
        if pte & PageTableFlags::WRITABLE.bits() != 0 {
            return Ok(());
        }
        let old_frame = self.page_table.translate_page(page).ok_or("COW: page not mapped")?;
        let phys_offset = crate::memory::physical_memory_offset();

        // ── Zero-page: promote the shared zero frame to a private writable copy.
        // Must be checked BEFORE the refcount path (zero frame has refcount 0).
        if crate::memory::cow::is_zero_frame(old_frame) {
            let new_frame = crate::allocator::phys_alloc(12)
                .map(PhysFrame::containing_address)
                .ok_or("COW zero-frame: OOM")?;
            crate::memory::cow::set_ref(new_frame, 1);
            let dst = (phys_offset + new_frame.start_address().as_u64()).as_mut_ptr::<u8>();
            core::ptr::write_bytes(dst, 0, 4096);
            // Do NOT dec_ref the zero frame — it is permanent.
            crate::ktrace!(MM, "zero-frame promotion at {:#x} -> new_frame {:#x}",
                page.start_address().as_u64(), new_frame.start_address().as_u64());
            return self.page_table.replace_frame(page, new_frame, vma_flags);
        }

        let refcount = crate::memory::cow::get_ref(old_frame);
        crate::ktrace!(MM, "addr={:#x} old_frame={:#x} ref={} vma_flags={:#x} pml4={:#x}",
            page.start_address().as_u64(),
            old_frame.start_address().as_u64(),
            refcount,
            vma_flags.bits(),
            self.page_table.pml4_phys().as_u64(),
        );

        if refcount <= 1 {
            // Last owner — just restore the WRITABLE flag (no copy). Only a
            // fork of this address space could raise the count, and that
            // holds the lock we hold.
            return self.page_table.update_page_flags(page, vma_flags);
        }

        // Shared frame — allocate a new frame and copy.
        let new_frame = crate::allocator::phys_alloc(12)
            .map(PhysFrame::containing_address)
            .ok_or("COW: out of memory")?;
        crate::ktrace!(MM, "path=copy new_frame={:#x}", new_frame.start_address().as_u64());
        crate::memory::cow::set_ref(new_frame, 1);

        let src = (phys_offset + old_frame.start_address().as_u64()).as_ptr::<u8>();
        let dst = (phys_offset + new_frame.start_address().as_u64()).as_mut_ptr::<u8>();
        core::ptr::copy_nonoverlapping(src, dst, 4096);

        self.page_table.replace_frame(page, new_frame, vma_flags)?;

        // Drop our share of the old frame. Atomic: of two address spaces
        // dropping their shares at once, exactly one sees zero.
        if crate::memory::cow::dec_ref(old_frame) == 0 {
            crate::allocator::phys_free(old_frame.start_address(), 12);
        }
        Ok(())
    }

    // ====================================================================
    // KERNEL ACCESS TO USER MEMORY THAT MAY NOT BE LOADED
    // ====================================================================

    /// Make every page of `[addr, addr+len)` mapped and privately writable,
    /// as if the owning process had written to each: demand-map what is
    /// absent, un-share COW and zero-frame pages. Pages outside any
    /// writable VMA are left alone (the caller's write will fault or be cut
    /// short, as a user write would). Returns whether all of them made it.
    ///
    /// # Safety
    /// As [`Self::handle_not_present_fault`].
    pub unsafe fn prepare_user_write(&self, addr: u64, len: u64) -> bool {
        self.vmas.with(|vmas| self.prepare_user_write_locked(vmas, addr, len))
    }

    unsafe fn prepare_user_write_locked(&self, vmas: &mut VmaList, addr: u64, len: u64) -> bool {
        if len == 0 {
            return true;
        }
        let last = addr.saturating_add(len - 1) & !0xFFF;
        let mut page_addr = addr & !0xFFF;
        let mut all = true;
        while page_addr <= last {
            let ok = match vmas.find(page_addr).copied().or_else(|| vmas.grow_stack(page_addr)) {
                None => false,
                Some(vma) => {
                    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(page_addr));
                    if !self.page_table.is_mapped(VirtAddr::new(page_addr)) {
                        super::demand_paging::map_demand_page(&self.page_table, page_addr, &vma, true).is_ok()
                    } else if vma.kind == VmaKind::Huge2M {
                        true // never COW-shared: fork does not share huge pages
                    } else {
                        self.make_writable_locked(page, vma.page_table_flags()).is_ok()
                    }
                }
            };
            all &= ok;
            page_addr += 0x1000;
        }
        all
    }

    /// Copy `src` into this address space at `user_addr`, whether or not it
    /// is the one loaded in CR3, with the semantics of a user write: see
    /// [`Self::prepare_user_write`]. Stops at the first page that cannot be
    /// written and returns the bytes copied. This is what `pipe.rs` uses to
    /// complete a blocked reader's `read()` from the writer's context — it
    /// used to translate and write through the physmap without either
    /// step, which put the data in the reader's *zero frame* (every
    /// untouched anonymous page in the system then read it) or in a frame
    /// still shared with the reader's fork parent (`pipe_cow_test`).
    ///
    /// # Safety
    /// As [`Self::handle_not_present_fault`].
    pub unsafe fn copy_to_user(&self, user_addr: u64, src: &[u8]) -> usize {
        self.vmas.with(|vmas| {
            self.prepare_user_write_locked(vmas, user_addr, src.len() as u64);
            self.copy_locked(user_addr, src.len(), |frame_ptr, done, chunk| {
                core::ptr::copy_nonoverlapping(src[done..].as_ptr(), frame_ptr, chunk);
            })
        })
    }

    /// Copy from this address space at `user_addr` into `dst`, whether or
    /// not it is loaded. Absent pages are demand-mapped (the zero frame for
    /// an anonymous one), as a user read would. Returns the bytes copied.
    ///
    /// # Safety
    /// As [`Self::handle_not_present_fault`].
    pub unsafe fn copy_from_user(&self, user_addr: u64, dst: &mut [u8]) -> usize {
        self.vmas.with(|vmas| {
            if !dst.is_empty() {
                let last = user_addr.saturating_add(dst.len() as u64 - 1) & !0xFFF;
                let mut page_addr = user_addr & !0xFFF;
                while page_addr <= last {
                    if !self.page_table.is_mapped(VirtAddr::new(page_addr)) {
                        if let Some(vma) = vmas.find(page_addr).copied() {
                            let _ = super::demand_paging::map_demand_page(&self.page_table, page_addr, &vma, false);
                        }
                    }
                    page_addr += 0x1000;
                }
            }
            let len = dst.len();
            self.copy_locked(user_addr, len, |frame_ptr, done, chunk| {
                core::ptr::copy_nonoverlapping(frame_ptr as *const u8, dst[done..].as_mut_ptr(), chunk);
            })
        })
    }

    /// Walk `[user_addr, user_addr+len)` page by page through the physmap,
    /// calling `f(frame_ptr, done, chunk)` for each mapped piece; stops at
    /// the first unmapped page. Caller holds `vmas`.
    unsafe fn copy_locked(&self, user_addr: u64, len: usize, mut f: impl FnMut(*mut u8, usize, usize)) -> usize {
        use x86_64::structures::paging::Translate;
        let phys_offset = crate::memory::physical_memory_offset();
        let mapper = self.page_table.create_mapper();
        let mut done = 0usize;
        while done < len {
            let vaddr = user_addr + done as u64;
            let chunk = core::cmp::min(len - done, 0x1000 - (vaddr & 0xFFF) as usize);
            // `translate_addr` handles 2 MiB pages too.
            let Some(phys) = mapper.translate_addr(VirtAddr::new(vaddr)) else { break };
            f((phys_offset + phys.as_u64()).as_mut_ptr::<u8>(), done, chunk);
            done += chunk;
        }
        done
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

            if addr != 0 && addr & (HUGE_2M - 1) != 0 {
                return Err("mmap: huge page addr not 2MB-aligned");
            }
            // Choose, check and register under one lock: two threads
            // mmapping at once must not get the same range.
            return self.vmas.with(|vmas| {
                let vaddr = if addr == 0 {
                    // Align bump pointer up to 2 MiB boundary.
                    let base = (self.mmap_base.load(Ordering::Relaxed) + HUGE_2M - 1) & !(HUGE_2M - 1);
                    // Advance past the allocation + one 2 MiB guard region.
                    self.mmap_base.store(base + length_aligned + HUGE_2M, Ordering::Relaxed);
                    base
                } else {
                    if vmas.overlaps(addr, size_pages) {
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
                vmas.add(vma).map_err(|_| "mmap: VMA list full")?;
                Ok(vaddr)
            });
        }

        // ── Normal 4 KiB anonymous pages ──────────────────────────────
        let size_pages = ((length + 4095) / 4096) as usize;

        if addr != 0 && addr & 0xFFF != 0 {
            return Err("mmap: addr not page-aligned");
        }

        // PRESENT is required so intermediate page-table entries get the
        // PRESENT bit set during demand paging — without it, map_to sets
        // intermediate entries without PRESENT, the CPU re-faults, and the
        // second map_to call panics in create_or_next_table_mut.
        self.vmas.with(|vmas| {
            let vaddr = if addr == 0 {
                let base = self.mmap_base.load(Ordering::Relaxed);
                // Advance bump pointer; add one guard page between allocations.
                self.mmap_base.store(base + size_pages as u64 * 4096 + 4096, Ordering::Relaxed);
                base
            } else {
                if vmas.overlaps(addr, size_pages) {
                    return Err("mmap: MAP_FIXED conflict with existing VMA");
                }
                addr
            };
            let vma = Vma {
                start: vaddr,
                size_pages,
                flags: flags.bits(),
                kind: VmaKind::Anonymous,
            };
            vmas.add(vma).map_err(|_| "mmap: VMA list full")?;
            Ok(vaddr)
        })
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
        // Under the lock from the VMA removal to the last PTE: a sibling
        // thread's fault must not map a page of this range in between.
        self.vmas.with(|vmas| {
            let vma = vmas.remove(addr).map_err(|_| "munmap: VMA not found")?;

            if vma.size_pages != size_pages {
                // Re-insert and signal partial munmap is unsupported.
                let _ = vmas.add(vma);
                return Err("munmap: partial unmap not supported");
            }

            match vma.kind {
                VmaKind::Anonymous | VmaKind::Code | VmaKind::GrowableStack => {
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
        })
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
        // `try_with` on both: the interrupted code on this CPU may hold
        // either lock.
        let result = self.vmas.try_with(|vmas| {
            crate::allocator::BUDDY.try_with(|buddy| {
                let n_huge = size_pages / 512;
                for i in 0..n_huge {
                    let va = start + i as u64 * 0x200_000;
                    let page = Page::<Size2MiB>::containing_address(VirtAddr::new(va));
                    let _ = self.page_table.unmap_page_and_free_2m_with_buddy(page, buddy);
                }
                let _ = vmas.remove(start);
            })
        });
        matches!(result, Some(Some(())))
    }
}