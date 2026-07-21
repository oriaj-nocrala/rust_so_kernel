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

use alloc::vec::Vec;
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

/// Initial stack size, in 4 KiB pages — every process starts here
/// regardless of what it'll actually need. The VMA is registered as
/// `VmaKind::GrowableStack` (see `memory::vma`), so the page fault
/// handler extends it downward on demand as a process actually touches
/// more of its stack, up to `STACK_MAX_PAGES`. No program needs its real
/// stack usage known in advance — this replaced an earlier per-program
/// stack-size override (added for Quake, whose `Host_Init` call chain
/// overflows a small fixed stack) that required guessing every future
/// program's needs by name; a growable stack is the standard OS answer
/// instead. (An early attempt at that per-program override tried raising
/// this constant globally to 1 MiB instead, and *seemed* to make
/// `busybox --install`'s `fork()` hang reproducibly — that turned out to
/// be a red herring: the exact same hang/crash, at the exact same spot,
/// reproduces with this constant left untouched too, roughly 1 boot in
/// 3-4, on the unmodified pre-Quake code. It's a real, pre-existing, still
/// unresolved flaky bug in that fork() path — see the
/// `busybox_install_fork_flake` memory — not something this constant's
/// value controls.)
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
/// share stack addresses. The stack VMA starts at `STACK_PAGES` and grows
/// on demand up to `STACK_MAX_PAGES` — see that constant's doc comment.
///
/// # Safety
/// - Buddy allocator must be initialized.
/// - The ELF must be a valid static x86_64 executable.
pub unsafe fn load_elf(
    elf_bytes: &[u8],
    process_index: usize,
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
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
        kind: VmaKind::GrowableStack,
    }).map_err(|_| "ELF loader: failed to register stack VMA")?;

    crate::serial_println!(
        "ELF: stack VMA {:#x}..{:#x} ({} pages, demand-paged, grows to {} max)",
        stack_base,
        stack_base + (STACK_PAGES as u64 * 4096),
        STACK_PAGES,
        crate::memory::vma::STACK_MAX_PAGES,
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

    // Write argc/argv/envp/auxv (see build_initial_stack's doc comment for
    // the exact layout) into this page and get back the resulting RSP.
    let rsp_va = build_initial_stack(
        page_ptr, top_page_vaddr, argv, envp,
        phdr_vaddr, elf.ph_count(), elf.entry_point(),
    )?;

    crate::serial_println!(
        "ELF: initial stack at {:#x} (argc={}, envc={}, phdr_vaddr={:#x}, ph_count={})",
        rsp_va, argv.len(), envp.len(), phdr_vaddr, elf.ph_count(),
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
// Initial stack construction (argc/argv/envp/auxv)
// ============================================================================

/// Writes the SysV ABI initial-stack frame — argc, argv[], envp[], and the
/// auxiliary vector — into the (already zeroed) top stack page, and
/// returns the resulting RSP as a virtual address.
///
/// Page layout, high to low addresses:
///   `[4096 - strings_bytes, 4096)`  argv/envp string bytes (envp first,
///                                   then argv — order between the two
///                                   string blocks is arbitrary, only the
///                                   pointer tables built from them matter)
///   `[rsp, frame_top)`              argc, argv ptrs + NULL, envp ptrs +
///                                   NULL, then 5 auxv (type, value) pairs
///                                   (AT_PHDR/AT_PHENT/AT_PHNUM/AT_ENTRY/
///                                   AT_NULL)
///
/// `rsp` always comes out 16-byte aligned, as the ABI requires at process
/// entry: `frame_top` is rounded down to 16 first, and if the slot count
/// above is odd, one inert zero word is appended *after* AT_NULL to round
/// it to an even (so 16-byte-multiple) size — safe because a conforming
/// reader stops at the first AT_NULL entry and never looks further, so
/// that trailing word is just dangling, never misread as another entry.
/// Padding can't go anywhere else without shifting argc off `RSP+0`, which
/// every caller (mlibc's `__dlapi_enter`, any C runtime) assumes unconditionally.
unsafe fn build_initial_stack(
    page_ptr: *mut u8,
    top_page_vaddr: u64,
    argv: &[Vec<u8>],
    envp: &[Vec<u8>],
    phdr_vaddr: u64,
    ph_count: usize,
    entry_point: u64,
) -> Result<u64, &'static str> {
    const AUXV_PAIRS: usize = 5; // AT_PHDR, AT_PHENT, AT_PHNUM, AT_ENTRY, AT_NULL

    let strings_bytes: usize = argv.iter().chain(envp.iter()).map(|s| s.len() + 1).sum();

    let mut slot_count = 1                  // argc
        + (argv.len() + 1)                  // argv[..] + NULL
        + (envp.len() + 1)                  // envp[..] + NULL
        + AUXV_PAIRS * 2;
    let pad = slot_count % 2 != 0;
    if pad { slot_count += 1; }
    let frame_bytes = slot_count * 8;

    // 16 bytes of slack for frame_top's alignment rounding below.
    if strings_bytes + frame_bytes + 16 > 4096 {
        return Err("ELF loader: argv/envp too large for the initial stack page");
    }

    // ── Place strings ───────────────────────────────────────────────────
    let content_top = 4096 - strings_bytes;
    let mut cursor = content_top;

    let mut envp_addrs: Vec<u64> = Vec::with_capacity(envp.len());
    for s in envp {
        core::ptr::copy_nonoverlapping(s.as_ptr(), page_ptr.add(cursor), s.len());
        *page_ptr.add(cursor + s.len()) = 0;
        envp_addrs.push(top_page_vaddr + cursor as u64);
        cursor += s.len() + 1;
    }
    let mut argv_addrs: Vec<u64> = Vec::with_capacity(argv.len());
    for s in argv {
        core::ptr::copy_nonoverlapping(s.as_ptr(), page_ptr.add(cursor), s.len());
        *page_ptr.add(cursor + s.len()) = 0;
        argv_addrs.push(top_page_vaddr + cursor as u64);
        cursor += s.len() + 1;
    }

    // ── Place argc/argv/envp/auxv ────────────────────────────────────────
    let frame_top = content_top & !0xF;
    if frame_bytes > frame_top {
        return Err("ELF loader: argv/envp too large for the initial stack page");
    }
    let rsp = frame_top - frame_bytes;

    let f = (page_ptr as usize + rsp) as *mut u64;
    let mut i = 0usize;
    macro_rules! put {
        ($v:expr) => {{ *f.add(i) = $v; i += 1; }};
    }

    put!(argv.len() as u64);
    for a in &argv_addrs { put!(*a); }
    put!(0); // argv NULL terminator
    for e in &envp_addrs { put!(*e); }
    put!(0); // envp NULL terminator

    put!(3); put!(phdr_vaddr);      // AT_PHDR
    put!(4); put!(56);              // AT_PHENT (sizeof Elf64_Phdr)
    put!(5); put!(ph_count as u64); // AT_PHNUM
    put!(9); put!(entry_point);     // AT_ENTRY
    put!(0); put!(0);               // AT_NULL
    if pad { put!(0); }

    Ok(top_page_vaddr + rsp as u64)
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