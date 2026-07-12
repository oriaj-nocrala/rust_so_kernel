// kernel/src/memory/elf_loader.rs
//
// ELF64 loader — maps PT_LOAD segments into a new user AddressSpace.
//
// DESIGN:
//   1. Parse ELF headers (elf.rs)
//   2. Create a fresh user AddressSpace
//   3. For each PT_LOAD segment:
//      a. Compute page-aligned virtual range
//      b. Map pages with correct flags (R/W/X → PageTableFlags)
//      c. Copy file data (p_filesz) into the mapped pages
//      d. Zero the remainder (p_memsz - p_filesz) — BSS
//      e. Register a VMA for the region
//   4. Register a demand-paged stack VMA (no physical pages yet)
//   5. Return LoadedElf { entry_point, address_space, user_stack_top }
//
// LIMITATIONS:
//   - Static executables only (no dynamic linker / PT_INTERP).
//   - No relocations.
//   - Segments must not overlap (undefined behavior if they do).
//   - User code must live in the lower half of the address space.

use x86_64::{
    VirtAddr,
    structures::paging::{Page, PageTableFlags, Size4KiB},
};

use super::elf::{Elf64, PF_R, PF_W, PF_X, PT_LOAD};
use super::address_space::AddressSpace;
use super::vma::{Vma, VmaKind};

// ============================================================================
// Configuration
// ============================================================================

/// Default user stack base address.
/// Each process gets its stack at a unique offset (base + pid * gap).
const DEFAULT_STACK_BASE: u64 = 0x0000_7100_0000_0000;

/// Gap between process stacks (64 KiB guard + 64 KiB stack = 128 KiB per process).
const STACK_PROCESS_GAP: u64 = 0x10000;

/// Number of stack pages per process (demand-paged).
const STACK_PAGES: usize = 16; // 64 KiB

// ============================================================================
// Result type
// ============================================================================

/// Everything needed to create a Process from a loaded ELF.
pub struct LoadedElf {
    /// The process's address space (page table + VMAs).
    pub address_space: AddressSpace,
    /// Virtual address of the ELF entry point (_start).
    pub entry_point: VirtAddr,
    /// Top of the user stack (grows downward).
    pub user_stack_top: VirtAddr,
}

// ============================================================================
// Loader
// ============================================================================

/// Load an ELF64 binary into a new user address space.
///
/// `elf_bytes` is the raw ELF file content (e.g. from `include_bytes!`).
/// `process_index` is used to offset the stack base so processes don't
/// share stack addresses.
///
/// # Safety
/// - Buddy allocator must be initialized.
/// - The ELF must be a valid static x86_64 executable.
pub unsafe fn load_elf(
    elf_bytes: &[u8],
    process_index: usize,
) -> Result<LoadedElf, &'static str> {
    // ── 1. Parse ELF ──────────────────────────────────────────────────

    let elf = Elf64::parse(elf_bytes)?;

    crate::serial_println!(
        "ELF: entry={:#x}, {} program headers",
        elf.entry_point(),
        elf.ph_count(),
    );

    // ── 2. Create address space ───────────────────────────────────────

    let mut address_space = AddressSpace::new_user()
        .map_err(|_| "ELF loader: failed to create address space")?;

    crate::serial_println!(
        "ELF: address space created, PML4 at {:#x}",
        address_space.root_frame().start_address().as_u64(),
    );

    // ── 3. Map each PT_LOAD segment ───────────────────────────────────

    for ph in elf.load_segments() {
        load_segment(&elf, ph, &mut address_space)?;
    }

    // ── 4. Set up demand-paged stack VMA ──────────────────────────────

    let stack_base = DEFAULT_STACK_BASE + (process_index as u64 * STACK_PROCESS_GAP);

    let stack_flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE;

    address_space.add_vma(Vma {
        start: stack_base,
        size_pages: STACK_PAGES,
        flags: stack_flags.bits(),
        kind: VmaKind::Anonymous,
    }).map_err(|_| "ELF loader: failed to register stack VMA")?;

    crate::serial_println!(
        "ELF: stack VMA {:#x}..{:#x} ({} pages, demand-paged)",
        stack_base,
        stack_base + (STACK_PAGES as u64 * 4096),
        STACK_PAGES,
    );

    // ── 5. Pre-map top stack page and write initial ABI stack frame ───
    //
    // The System V AMD64 ABI requires the initial stack to contain:
    //   [RSP+0]  argc
    //   [RSP+8]  argv[0..argc], NULL
    //   [RSP+?]  envp[], NULL
    //   [RSP+?]  AT_xxx pairs (auxiliary vector), AT_NULL
    //
    // Without this, mlibc's __dlapi_enter reads past the stack VMA top
    // (entry_stack[1] = *(RSP+8) would be outside the VMA) and crashes.
    // We also provide AT_PHDR so mlibc can find and load the PT_TLS
    // segment (needed for correct thread-local errno setup).

    // Find the virtual address of the program headers (AT_PHDR).
    // The phdrs are at file offset e_phoff, which is in the first PT_LOAD.
    let e_phoff = elf.phdr_file_offset();
    let mut phdr_vaddr: u64 = 0;
    for ph in elf.program_headers() {
        if ph.p_type == super::elf::PT_LOAD
            && e_phoff >= ph.p_offset
            && e_phoff < ph.p_offset + ph.p_filesz
        {
            phdr_vaddr = ph.p_vaddr + (e_phoff - ph.p_offset);
            break;
        }
    }

    // Pre-allocate and map the top stack page so we can write to it now.
    let top_page_vaddr = stack_base + ((STACK_PAGES as u64 - 1) * 4096);
    let top_page = Page::<Size4KiB>::containing_address(VirtAddr::new(top_page_vaddr));
    let stack_page_frame = address_space
        .map_user_page(top_page, stack_flags)
        .map_err(|_| "ELF loader: failed to map initial stack page")?;

    let phys_offset = crate::memory::physical_memory_offset();
    let page_virt = phys_offset + stack_page_frame.start_address().as_u64();
    let page_ptr = page_virt.as_mut_ptr::<u8>();

    // Zero the page.
    core::ptr::write_bytes(page_ptr, 0, 4096);

    // Write the initial stack frame at the top of the page (last 128 bytes).
    // Layout: argc=0, NULL(argv end), NULL(envp end), AT_PHDR, AT_PHENT,
    //         AT_PHNUM, AT_ENTRY, AT_NULL
    const FRAME_BYTES: usize = 128; // 16 u64 slots
    let frame_page_offset = 4096 - FRAME_BYTES;
    let rsp_va = top_page_vaddr + frame_page_offset as u64;
    let f = (page_ptr as usize + frame_page_offset) as *mut u64;

    *f.add(0)  = 0;                         // argc = 0
    *f.add(1)  = 0;                         // argv[0] = NULL (end of argv)
    *f.add(2)  = 0;                         // envp[0] = NULL (end of envp)
    *f.add(3)  = 3;                         // AT_PHDR type
    *f.add(4)  = phdr_vaddr;                // AT_PHDR value
    *f.add(5)  = 4;                         // AT_PHENT type
    *f.add(6)  = 56;                        // AT_PHENT value (sizeof Elf64_Phdr)
    *f.add(7)  = 5;                         // AT_PHNUM type
    *f.add(8)  = elf.ph_count() as u64;     // AT_PHNUM value
    *f.add(9)  = 9;                         // AT_ENTRY type
    *f.add(10) = elf.entry_point();         // AT_ENTRY value
    *f.add(11) = 0;                         // AT_NULL type
    *f.add(12) = 0;                         // AT_NULL value
    // slots 13-15 remain zero

    crate::serial_println!(
        "ELF: initial stack at {:#x} (phdr_vaddr={:#x}, ph_count={})",
        rsp_va, phdr_vaddr, elf.ph_count(),
    );

    // ── 5b. Map the sigreturn trampoline ──────────────────────────────
    //
    // One fixed page, read+exec only, identical in every process — see
    // `process::signal` module doc comment. Mapped here (once, at process
    // creation) rather than lazily so `fork()`'s existing COW-share loop
    // (which walks every VMA with an already-mapped page) picks it up for
    // children automatically, and `clone()` (threads) gets it for free by
    // sharing the whole `AddressSpace`.
    {
        use super::signal_trampoline::{TRAMPOLINE_VA, TRAMPOLINE_CODE};

        let tramp_flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        let tramp_page = Page::<Size4KiB>::containing_address(VirtAddr::new(TRAMPOLINE_VA));
        let tramp_frame = address_space
            .map_user_page(tramp_page, tramp_flags)
            .map_err(|_| "ELF loader: failed to map sigreturn trampoline")?;

        let tramp_virt = (phys_offset + tramp_frame.start_address().as_u64()).as_mut_ptr::<u8>();
        core::ptr::write_bytes(tramp_virt, 0, 4096);
        core::ptr::copy_nonoverlapping(TRAMPOLINE_CODE.as_ptr(), tramp_virt, TRAMPOLINE_CODE.len());

        address_space.add_vma(Vma {
            start: TRAMPOLINE_VA,
            size_pages: 1,
            flags: tramp_flags.bits(),
            kind: VmaKind::Code,
        }).map_err(|_| "ELF loader: failed to register trampoline VMA")?;
    }

    // ── 6. Done ───────────────────────────────────────────────────────

    Ok(LoadedElf {
        address_space,
        entry_point: VirtAddr::new(elf.entry_point()),
        user_stack_top: VirtAddr::new(rsp_va),
    })
}

// ============================================================================
// Segment loading (internal)
// ============================================================================

/// Map a single PT_LOAD segment into the address space.
///
/// Steps:
///   1. Compute the page-aligned virtual range.
///   2. Convert ELF flags (PF_R/W/X) to PageTableFlags.
///   3. Map each page, copy file data, zero BSS portion.
///   4. Register a VMA.
unsafe fn load_segment(
    elf: &Elf64,
    ph: &super::elf::Elf64ProgramHeader,
    address_space: &mut AddressSpace,
) -> Result<(), &'static str> {
    if ph.p_memsz == 0 {
        return Ok(()); // Empty segment, skip
    }

    // ── Page-aligned range ────────────────────────────────────────────
    //
    // ELF segments are not necessarily page-aligned.  We must:
    //   - Round down p_vaddr to the page boundary
    //   - Account for the offset within the first page
    //   - Round up the total size to full pages

    let seg_vaddr = ph.p_vaddr;
    let seg_memsz = ph.p_memsz;
    let seg_filesz = ph.p_filesz;

    let page_offset = seg_vaddr & 0xFFF; // offset within first page
    let aligned_start = seg_vaddr & !0xFFF; // page-aligned start
    let aligned_end = ((seg_vaddr + seg_memsz) + 0xFFF) & !0xFFF;
    let num_pages = ((aligned_end - aligned_start) / 4096) as usize;

    // ── Flags ─────────────────────────────────────────────────────────

    let flags = elf_flags_to_page_flags(ph.p_flags);

    crate::serial_println!(
        "ELF: LOAD {:#x}..{:#x} ({} pages) filesz={:#x} memsz={:#x} flags={:#x}",
        aligned_start,
        aligned_end,
        num_pages,
        seg_filesz,
        seg_memsz,
        flags.bits(),
    );

    // ── Get segment file data ─────────────────────────────────────────

    let file_data = elf.segment_data(ph)
        .ok_or("ELF loader: segment data out of bounds")?;

    // ── Map pages and copy data ───────────────────────────────────────

    let phys_offset = crate::memory::physical_memory_offset();

    for page_idx in 0..num_pages {
        let page_vaddr = aligned_start + (page_idx as u64 * 4096);
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(page_vaddr));

        // Allocate and map one page.
        // Two PT_LOAD segments can share a page when one segment ends
        // mid-page and the next begins in the same page (e.g. .text
        // ends at 0x4010f7 and .rodata starts at 0x4010f8 — both fall
        // in the page 0x401000).  In that case, reuse the existing frame
        // instead of trying to map it again (which would fail with
        // PageAlreadyMapped).
        let (frame, is_new_page) = match address_space.translate_page(page) {
            Some(existing) => (existing, false),
            None => {
                let f = address_space
                    .map_user_page(page, flags)
                    .map_err(|_| "ELF loader: failed to map page")?;
                (f, true)
            }
        };

        // Compute how much of this page comes from the file vs BSS
        let frame_virt = phys_offset + frame.start_address().as_u64();
        let dst = frame_virt.as_mut_ptr::<u8>();

        // Zero only freshly-allocated pages.
        //
        // If the page was already mapped by a previous PT_LOAD segment
        // (e.g. .text and .rodata share the same 4K page), zeroing it
        // would destroy the first segment's data.  Reused pages already
        // contain valid content — we only write the current segment's
        // bytes at the correct intra-page offset below.
        if is_new_page {
            core::ptr::write_bytes(dst, 0, 4096);
        }

        // Compute overlap between this page and the file data
        let page_start_in_seg = if page_vaddr >= seg_vaddr {
            page_vaddr - seg_vaddr
        } else {
            0
        };

        // Where in this page does the segment data start?
        let dst_offset = if page_idx == 0 { page_offset as usize } else { 0 };

        // How many file bytes belong on this page?
        if (page_start_in_seg as u64) < seg_filesz {
            let file_offset_start = page_start_in_seg as usize;
            let file_bytes_remaining = (seg_filesz as usize).saturating_sub(file_offset_start);
            let page_bytes_available = 4096 - dst_offset;
            let copy_len = file_bytes_remaining.min(page_bytes_available);

            if copy_len > 0 && file_offset_start < file_data.len() {
                let actual_copy = copy_len.min(file_data.len() - file_offset_start);
                core::ptr::copy_nonoverlapping(
                    file_data.as_ptr().add(file_offset_start),
                    dst.add(dst_offset),
                    actual_copy,
                );
            }
        }
    }

    // ── Register VMA ──────────────────────────────────────────────────

    let vma_kind = if ph.p_flags & super::elf::PF_X != 0 {
        VmaKind::Code
    } else {
        // Writable data segments (.data, .bss) are Anonymous
        // so demand paging can handle additional pages if needed.
        VmaKind::Anonymous
    };

    address_space.add_vma(Vma {
        start: aligned_start,
        size_pages: num_pages,
        flags: flags.bits(),
        kind: vma_kind,
    }).map_err(|_| "ELF loader: failed to register VMA")?;

    Ok(())
}

// ============================================================================
// Flag conversion
// ============================================================================

/// Convert ELF segment flags (PF_R, PF_W, PF_X) to x86_64 PageTableFlags.
///
/// All user pages need PRESENT + USER_ACCESSIBLE.
/// PF_W → WRITABLE.
/// PF_X → we do NOT set NO_EXECUTE (NX bit is not confirmed enabled in EFER).
/// PF_R → implied by PRESENT.
fn elf_flags_to_page_flags(elf_flags: u32) -> PageTableFlags {
    let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;

    if elf_flags & PF_W != 0 {
        flags |= PageTableFlags::WRITABLE;
    }

    // NOTE: We intentionally do NOT set NO_EXECUTE for non-executable
    // segments because EFER.NXE may not be enabled.  When NX support
    // is confirmed, add:
    //   if elf_flags & PF_X == 0 { flags |= PageTableFlags::NO_EXECUTE; }

    flags
}