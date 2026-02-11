// kernel/src/memory/demand_paging.rs
//
// Page fault handler for demand paging.
//
// Flow:
//   1. CPU faults on unmapped page → pushes error code, jumps to vector 14
//   2. This handler reads CR2 (faulting address)
//   3. Looks up the current process's VMAs
//   4. If address is in a valid Anonymous VMA → allocate frame, map, zero, resume
//   5. If address is invalid → return Err (caller panics or kills process)

use x86_64::{
    VirtAddr,
    registers::control::Cr3,
    structures::paging::{
        FrameAllocator, Mapper, OffsetPageTable, Page, PageTable,
        PageTableFlags, Size4KiB,
    },
};

use crate::memory::vma::{self, VmaKind};
use crate::memory::page_table_manager::BuddyFrameAllocator;

// Page fault error code bits
const PF_PRESENT: u64 = 1 << 0;    // 0 = not present, 1 = protection violation
const PF_WRITE: u64 = 1 << 1;      // 0 = read, 1 = write
const PF_USER: u64 = 1 << 2;       // 0 = kernel mode, 1 = user mode
const PF_RESERVED: u64 = 1 << 3;   // 1 = reserved bit set in page table

/// Read CR2 (faulting address) via inline assembly.
#[inline]
fn read_cr2() -> u64 {
    let addr: u64;
    unsafe {
        core::arch::asm!("mov {}, cr2", out(reg) addr);
    }
    addr
}

/// Attempt to handle a page fault via demand paging.
///
/// Returns `Ok(())` if a page was successfully mapped and execution
/// can resume.  Returns `Err(reason)` if the fault is not recoverable.
pub fn handle_page_fault(error_code: u64) -> Result<(), &'static str> {
    let fault_addr = read_cr2();

    // ── 1. Filter: only handle user-mode, not-present faults ──────────

    if error_code & PF_RESERVED != 0 {
        return Err("Reserved bit set in page table entry");
    }

    if error_code & PF_USER == 0 {
        return Err("Kernel-mode page fault (not demand-pageable)");
    }

    if error_code & PF_PRESENT != 0 {
        // Page IS present but faulted → protection violation.
        // Future: this is where Copy-on-Write would go.
        return Err("Protection violation (page present, future CoW)");
    }

    // ── 2. Look up current process ────────────────────────────────────

    let pid = crate::process::scheduler::current_pid()
        .ok_or("Page fault with no current process")?;

    // ── 3. Find VMA for the faulting address ──────────────────────────

    let vma = vma::find_vma(pid, fault_addr)
        .ok_or("Segmentation fault: no VMA for address")?;

    // ── 4. Only demand-page Anonymous regions ─────────────────────────

    match vma.kind {
        VmaKind::Anonymous => { /* proceed */ }
        VmaKind::Code => {
            return Err("Code page not present (should be pre-mapped)");
        }
    }

    // ── 5. Allocate a physical frame ──────────────────────────────────

    let mut buddy_alloc = BuddyFrameAllocator;

    let frame = buddy_alloc
        .allocate_frame()
        .ok_or("Demand paging: frame allocation failed (OOM)")?;

    // ── 6. Zero the frame (security + correctness) ────────────────────

    unsafe {
        let phys_offset = crate::memory::physical_memory_offset();
        let frame_virt = phys_offset + frame.start_address().as_u64();
        core::ptr::write_bytes(frame_virt.as_mut_ptr::<u8>(), 0, 4096);
    }

    // ── 7. Map the page in the current page table ─────────────────────

    let page: Page<Size4KiB> = Page::containing_address(
        VirtAddr::new(fault_addr & !0xFFF)
    );

    unsafe {
        let phys_offset = crate::memory::physical_memory_offset();
        let (cr3_frame, _) = Cr3::read();
        let pml4_virt = phys_offset + cr3_frame.start_address().as_u64();
        let pml4: &mut PageTable = &mut *pml4_virt.as_mut_ptr::<PageTable>();
        let mut mapper = OffsetPageTable::new(pml4, phys_offset);

        mapper
            .map_to(page, frame, vma.page_table_flags(), &mut buddy_alloc)
            .map_err(|_| "Demand paging: map_to failed")?
            .flush();
    }

    // ── 8. Success! ───────────────────────────────────────────────────

    crate::serial_println!(
        "📄 Demand page: PID {} fault at {:#x} → mapped {:#x} (phys {:#x})",
        pid,
        fault_addr,
        page.start_address().as_u64(),
        frame.start_address().as_u64(),
    );

    Ok(())
}