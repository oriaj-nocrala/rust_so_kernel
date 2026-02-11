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
        PageTableFlags, PhysFrame, Size4KiB,
        mapper::MapToError,
    },
};

use crate::allocator::buddy_allocator::BUDDY;

// ============================================================================
// User address layout — which PML4 entries user processes own
// ============================================================================

/// User code base address (0x400000).  Falls in PML4 entry 0.
const USER_CODE_BASE: u64 = 0x0000_0000_0040_0000;

/// User stack base address (0x710000000000).  Falls in PML4 entry 226.
const USER_STACK_BASE: u64 = 0x0000_7100_0000_0000;

/// Convert a virtual address to its PML4 index (bits 47:39).
#[inline]
const fn pml4_index(va: u64) -> usize {
    ((va >> 39) & 0x1FF) as usize
}

/// PML4 indices that belong to user processes.
/// These must NOT be copied from the kernel — each process builds its own.
const USER_PML4_ENTRIES: [usize; 2] = [
    pml4_index(USER_CODE_BASE),   // 0  — user code
    pml4_index(USER_STACK_BASE),  // 226 — user stack
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
            BUDDY.lock()
                .allocate(12)
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
            let mut buddy = BUDDY.lock();
            let phys_addr = buddy
                .allocate(12)
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

    /// Map one user page.  Allocates data + intermediate frames from Buddy.
    pub unsafe fn map_user_page(
        &self,
        page: Page<Size4KiB>,
        flags: PageTableFlags,
    ) -> Result<PhysFrame, MapToError<Size4KiB>> {
        let mut buddy_alloc = BuddyFrameAllocator;

        let frame = buddy_alloc
            .allocate_frame()
            .ok_or(MapToError::FrameAllocationFailed)?;

        let mut mapper = self.create_mapper();

        mapper
            .map_to(page, frame, flags, &mut buddy_alloc)?
            .flush();

        Ok(frame)
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
}