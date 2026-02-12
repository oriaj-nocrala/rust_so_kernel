// kernel/src/memory/demand_paging.rs
//
// Demand paging — pure memory operations, NO process layer dependency.
//
// This module provides two functions:
//   1. `is_demand_pageable(error_code)` — pre-filter on CPU error code
//   2. `map_demand_page(fault_addr, vma, pid)` — allocate, zero, map
//
// The PAGE FAULT HANDLER (in init/devices.rs) is responsible for:
//   - Reading CR2
//   - Calling `is_demand_pageable` to filter
//   - Looking up the VMA via the scheduler (process layer)
//   - Calling `map_demand_page` with the VMA
//
// This keeps the dependency arrow one-way:
//   init/devices → memory (demand_paging)
//   init/devices → process (scheduler)
//   memory does NOT import process
//
// ── PREVIOUS DESIGN ────────────────────────────────────────────────
// `handle_page_fault` did everything: read CR2, filter error code,
// call `crate::process::scheduler::find_current_vma(fault_addr)`,
// allocate frame, map page.  This created a circular dependency
// between the memory and process layers.
// ───────────────────────────────────────────────────────────────────

use x86_64::{
    VirtAddr,
    registers::control::Cr3,
    structures::paging::{
        FrameAllocator, Mapper, OffsetPageTable, Page, PageTable,
        PageTableFlags, Size4KiB,
    },
};

use crate::memory::vma::{Vma, VmaKind};
use crate::memory::page_table_manager::BuddyFrameAllocator;

// Page fault error code bits
const PF_PRESENT: u64 = 1 << 0;    // 0 = not present, 1 = protection violation
const PF_WRITE: u64 = 1 << 1;      // 0 = read, 1 = write
const PF_USER: u64 = 1 << 2;       // 0 = kernel mode, 1 = user mode
const PF_RESERVED: u64 = 1 << 3;   // 1 = reserved bit set in page table

/// Read CR2 (faulting address) via inline assembly.
#[inline]
pub fn read_cr2() -> u64 {
    let addr: u64;
    unsafe {
        core::arch::asm!("mov {}, cr2", out(reg) addr);
    }
    addr
}

/// Pre-filter: can this page fault potentially be resolved by demand paging?
///
/// Returns `Ok(())` if the fault is a candidate (user-mode, not-present).
/// Returns `Err(reason)` if the fault is definitely not demand-pageable.
///
/// This is a pure function of the CPU error code — no process state needed.
pub fn is_demand_pageable(error_code: u64) -> Result<(), &'static str> {
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

    Ok(())
}

/// Allocate a physical frame, zero it, and map it at `fault_addr` using
/// the flags from `vma`.
///
/// This is a pure memory operation: it touches the Buddy allocator and
/// the CURRENT page table (CR3).  The caller is responsible for ensuring
/// that CR3 points to the faulting process's page table (which it does
/// during a page fault — the CPU doesn't change CR3).
///
/// `pid` is used only for the log message.
///
/// # Errors
/// - VMA kind is not Anonymous (code pages should be pre-mapped)
/// - Frame allocation failed (OOM)
/// - Page table mapping failed
pub fn map_demand_page(fault_addr: u64, vma: &Vma, pid: usize) -> Result<(), &'static str> {
    // ── 1. Only demand-page Anonymous regions ─────────────────────────

    match vma.kind {
        VmaKind::Anonymous => { /* proceed */ }
        VmaKind::Code => {
            return Err("Code page not present (should be pre-mapped)");
        }
    }

    // ── 2. Allocate a physical frame ──────────────────────────────────

    let mut buddy_alloc = BuddyFrameAllocator;

    let frame = buddy_alloc
        .allocate_frame()
        .ok_or("Demand paging: frame allocation failed (OOM)")?;

    // ── 3. Zero the frame (security + correctness) ────────────────────

    unsafe {
        let phys_offset = crate::memory::physical_memory_offset();
        let frame_virt = phys_offset + frame.start_address().as_u64();
        core::ptr::write_bytes(frame_virt.as_mut_ptr::<u8>(), 0, 4096);
    }

    // ── 4. Map the page in the current page table ─────────────────────

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

    // ── 5. Success! ───────────────────────────────────────────────────

    crate::serial_println!(
        "📄 Demand page: PID {} fault at {:#x} → mapped {:#x} (phys {:#x})",
        pid,
        fault_addr,
        page.start_address().as_u64(),
        frame.start_address().as_u64(),
    );

    Ok(())
}