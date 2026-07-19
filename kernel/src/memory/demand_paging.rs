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
        PageTableFlags, Size2MiB, Size4KiB,
    },
};

use crate::memory::vma::{Vma, VmaKind};
use crate::memory::page_table_manager::BuddyFrameAllocator;

// Page fault error code bits
const PF_PRESENT: u64 = 1 << 0;    // 0 = not present, 1 = protection violation
const PF_WRITE: u64 = 1 << 1;      // 0 = read, 1 = write
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
/// Returns `Ok(())` if the fault is a candidate (not-present, non-reserved).
/// Returns `Err(reason)` if the fault is definitely not demand-pageable.
///
/// This is a pure function of the CPU error code — no process state needed.
///
/// Deliberately NOT gated on `PF_USER` — same reasoning as the COW-fault
/// check in `init/devices.rs::page_fault_handler`, which this mirrors: a
/// syscall handler (`read()`, `getdents64()`, ...) writes into the
/// *calling* process's own buffer using its own CR3/address space, but
/// executes in ring 0, so the exact same "first touch of a freshly
/// mmap'd/never-yet-faulted anonymous page" fault can arrive with
/// `PF_USER` clear instead of set — confirmed live: `doom`'s WAD loader
/// (`sys_read` into a buffer straight off `malloc()`, never touched from
/// user mode first) reliably panicked the kernel here before this fix.
/// Safe to drop the check for the same reason the COW case already is:
/// the caller's subsequent VMA lookup (`find_vma_fast`) only ever matches
/// an address inside the *current* process's own registered VMA, so a
/// fault on real kernel memory still correctly falls through un-resolved
/// (panics) either way — this only ever widens what's demand-pageable,
/// never what's excused from the "no VMA" panic.
pub fn is_demand_pageable(error_code: u64) -> Result<(), &'static str> {
    if error_code & PF_RESERVED != 0 {
        return Err("Reserved bit set in page table entry");
    }

    if error_code & PF_PRESENT != 0 {
        // Page IS present but faulted → protection violation (the COW
        // case is already handled earlier, before this function is ever
        // called — see page_fault_handler).
        return Err("Protection violation (page present, future CoW)");
    }

    Ok(())
}

/// Build an `OffsetPageTable` over the currently active CR3.
///
/// # Safety
/// The caller must ensure single-CPU access (e.g. interrupts disabled).
unsafe fn create_cr3_mapper() -> OffsetPageTable<'static> {
    let phys_offset = crate::memory::physical_memory_offset();
    let (cr3_frame, _) = Cr3::read();
    let pml4_virt = phys_offset + cr3_frame.start_address().as_u64();
    let pml4: &mut PageTable = &mut *pml4_virt.as_mut_ptr::<PageTable>();
    OffsetPageTable::new(pml4, phys_offset)
}

/// Allocate a physical frame, zero it, and map it at `fault_addr` using
/// the flags from `vma`.
///
/// When `is_write` is false and the VMA is Anonymous, the shared zero frame
/// is mapped read-only instead of allocating a real frame (zero-page trick).
/// A subsequent write fault will be handled by the COW path, which detects
/// the zero frame and allocates a private writable copy.
///
/// `pid` is used only for the log message.
///
/// # Errors
/// - VMA kind is Code (code pages should be pre-mapped)
/// - Frame allocation failed (OOM)
/// - Page table mapping failed
pub fn map_demand_page(
    fault_addr: u64,
    vma: &Vma,
    pid: usize,
    is_write: bool,
) -> Result<(), &'static str> {
    match vma.kind {
        VmaKind::Code => {
            return Err("Code page not present (should be pre-mapped)");
        }
        VmaKind::Huge2M => {
            return map_demand_page_2m(fault_addr, vma, pid);
        }
        VmaKind::Anonymous => { /* fall through */ }
    }

    let page: Page<Size4KiB> = Page::containing_address(
        VirtAddr::new(fault_addr & !0xFFF)
    );

    // ── Zero-page trick: read faults map the shared zero frame ────────
    if !is_write {
        let zero = crate::memory::cow::zero_frame();
        let ro_flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        let mut buddy_alloc = BuddyFrameAllocator;
        unsafe {
            let mut mapper = create_cr3_mapper();
            mapper
                .map_to(page, zero, ro_flags, &mut buddy_alloc)
                .map_err(|_| "zero-page: map_to failed")?
                .flush();
        }
        return Ok(());
    }

    // ── Write fault: allocate a real frame, zero-fill, map writable ───
    let mut buddy_alloc = BuddyFrameAllocator;
    let frame = buddy_alloc
        .allocate_frame()
        .ok_or("Demand paging: frame allocation failed (OOM)")?;

    unsafe { crate::memory::cow::set_ref(frame, 1); }

    unsafe {
        let phys_offset = crate::memory::physical_memory_offset();
        let frame_virt = phys_offset + frame.start_address().as_u64();
        core::ptr::write_bytes(frame_virt.as_mut_ptr::<u8>(), 0, 4096);
    }

    unsafe {
        let mut mapper = create_cr3_mapper();
        mapper
            .map_to(page, frame, vma.page_table_flags(), &mut buddy_alloc)
            .map_err(|_| "Demand paging: map_to failed")?
            .flush();
    }

    Ok(())
}

/// Map a 2 MiB huge page for `fault_addr` inside a `Huge2M` VMA.
fn map_demand_page_2m(fault_addr: u64, vma: &Vma, _pid: usize) -> Result<(), &'static str> {
    const PAGE_2M: u64 = 0x200000;
    let page_start = fault_addr & !(PAGE_2M - 1);
    let page = Page::<Size2MiB>::containing_address(VirtAddr::new(page_start));

    let mut buddy_alloc = BuddyFrameAllocator;
    let frame: x86_64::structures::paging::PhysFrame<Size2MiB> = buddy_alloc
        .allocate_frame()
        .ok_or("Demand paging 2M: OOM")?;

    // Zero-fill 2 MiB.
    unsafe {
        let phys_offset = crate::memory::physical_memory_offset();
        let virt = (phys_offset + frame.start_address().as_u64()).as_mut_ptr::<u8>();
        core::ptr::write_bytes(virt, 0, 0x200000);
    }

    // map_to for Size2MiB sets the HUGE_PAGE bit automatically.
    unsafe {
        let mut mapper = create_cr3_mapper();
        mapper
            .map_to(page, frame, vma.page_table_flags(), &mut buddy_alloc)
            .map_err(|_| "map_to 2M failed")?
            .flush();
    }

    Ok(())
}