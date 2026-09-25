// kernel/src/memory/demand_paging.rs
//
// Demand paging — pure memory operations, NO process layer dependency.
//
// This module provides two functions:
//   1. `is_demand_pageable(error_code)` — pre-filter on CPU error code
//   2. `map_demand_page(pt, fault_addr, vma, is_write)` — allocate, zero, map
//
// The PAGE FAULT HANDLER (in init/devices.rs) is responsible for:
//   - Reading CR2
//   - Calling `is_demand_pageable` to filter
//   - Finding the running process's AddressSpace (process layer, lock-free)
//   - Calling `AddressSpace::handle_not_present_fault`, which looks the VMA
//     up and calls `map_demand_page` under that address space's lock
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
    structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags, Size2MiB, Size4KiB},
};

use crate::memory::vma::{Vma, VmaKind};
use crate::memory::page_table_manager::{BuddyFrameAllocator, OwnedPageTable};

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
/// the caller's subsequent VMA lookup (`AddressSpace::handle_not_present_fault`) only ever matches
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

/// Allocate a physical frame, zero it, and map it at `fault_addr` in `pt`
/// using the flags from `vma`.
///
/// When `is_write` is false and the VMA is Anonymous, the shared zero frame
/// is mapped read-only instead of allocating a real frame (zero-page trick).
/// A subsequent write fault will be handled by the COW path, which detects
/// the zero frame and allocates a private writable copy.
///
/// `pt` is the table of the address space that owns `vma` — not
/// necessarily the one loaded in CR3: a pipe writer completing a blocked
/// reader's `read()` maps into the *reader's* table. The caller holds that
/// address space's lock (`AddressSpace::handle_not_present_fault` and the
/// copy helpers next to it), so two threads faulting on one page cannot
/// both map it.
///
/// # Errors
/// - VMA kind is Code (code pages should be pre-mapped)
/// - Frame allocation failed (OOM)
/// - Page table mapping failed
pub(super) unsafe fn map_demand_page(
    pt: &OwnedPageTable,
    fault_addr: u64,
    vma: &Vma,
    is_write: bool,
) -> Result<(), &'static str> {
    match vma.kind {
        VmaKind::Code => {
            return Err("Code page not present (should be pre-mapped)");
        }
        VmaKind::Huge2M => {
            return map_demand_page_2m(pt, fault_addr, vma);
        }
        VmaKind::Shared => {
            return map_shared_page(pt, fault_addr, vma);
        }
        VmaKind::Anonymous | VmaKind::GrowableStack => { /* fall through */ }
    }

    let page: Page<Size4KiB> = Page::containing_address(
        VirtAddr::new(fault_addr & !0xFFF)
    );

    // ── Zero-page trick: read faults map the shared zero frame ────────
    if !is_write {
        let zero = crate::memory::cow::zero_frame();
        let ro_flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        let mut buddy_alloc = BuddyFrameAllocator;
        pt.create_mapper()
            .map_to(page, zero, ro_flags, &mut buddy_alloc)
            .map_err(|_| "zero-page: map_to failed")?
            .ignore();
        crate::memory::tlb::invalidate_page(pt.pml4_phys(), page.start_address());
        return Ok(());
    }

    // ── Write fault: allocate a real frame, zero-fill, map writable ───
    let mut buddy_alloc = BuddyFrameAllocator;
    let frame = buddy_alloc
        .allocate_frame()
        .ok_or("Demand paging: frame allocation failed (OOM)")?;

    crate::memory::cow::set_ref(frame, 1);

    let phys_offset = crate::memory::physical_memory_offset();
    let frame_virt = phys_offset + frame.start_address().as_u64();
    core::ptr::write_bytes(frame_virt.as_mut_ptr::<u8>(), 0, 4096);

    pt.create_mapper()
        .map_to(page, frame, vma.page_table_flags(), &mut buddy_alloc)
        .map_err(|_| "Demand paging: map_to failed")?
        .ignore();
    crate::memory::tlb::invalidate_page(pt.pml4_phys(), page.start_address());

    Ok(())
}

/// Map the object's own frame for `fault_addr` inside a `Shared` VMA,
/// read or write alike: never the zero frame, which a later write would
/// have to replace in every address space at once. The frame comes with
/// the reference this PTE holds (`ShmObject::frame_for_mapping`).
unsafe fn map_shared_page(pt: &OwnedPageTable, fault_addr: u64, vma: &Vma) -> Result<(), &'static str> {
    let m = vma.shm.as_ref().ok_or("shared VMA without an object")?;
    let page_addr = fault_addr & !0xFFF;
    let idx = m.offset_pages + ((page_addr - vma.start) / 4096) as usize;
    let frame = m.obj.frame_for_mapping(idx)?;
    let page: Page<Size4KiB> = Page::containing_address(VirtAddr::new(page_addr));
    if let Err(e) = pt.map_existing_frame(page, frame, vma.page_table_flags()) {
        // The reference taken for this PTE; the object still holds its own.
        crate::memory::cow::dec_ref(frame);
        return Err(e);
    }
    Ok(())
}

/// Map a 2 MiB huge page for `fault_addr` inside a `Huge2M` VMA.
unsafe fn map_demand_page_2m(pt: &OwnedPageTable, fault_addr: u64, vma: &Vma) -> Result<(), &'static str> {
    const PAGE_2M: u64 = 0x200000;
    let page_start = fault_addr & !(PAGE_2M - 1);
    let page = Page::<Size2MiB>::containing_address(VirtAddr::new(page_start));

    let mut buddy_alloc = BuddyFrameAllocator;
    let frame: x86_64::structures::paging::PhysFrame<Size2MiB> = buddy_alloc
        .allocate_frame()
        .ok_or("Demand paging 2M: OOM")?;

    // Zero-fill 2 MiB.
    let phys_offset = crate::memory::physical_memory_offset();
    let virt = (phys_offset + frame.start_address().as_u64()).as_mut_ptr::<u8>();
    core::ptr::write_bytes(virt, 0, 0x200000);

    // map_to for Size2MiB sets the HUGE_PAGE bit automatically.
    pt.create_mapper()
        .map_to(page, frame, vma.page_table_flags(), &mut buddy_alloc)
        .map_err(|_| "map_to 2M failed")?
        .ignore();
    crate::memory::tlb::invalidate_page(pt.pml4_phys(), page.start_address());

    Ok(())
}
