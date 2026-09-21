// kernel/src/memory/mmio.rs
//
// Uncached kernel mappings for device register windows (PCI memory BARs).
//
// Every other driver in this kernel reaches its hardware through legacy
// port I/O, which needs no mapping at all — this module exists because the
// xHCI USB controller's registers are a memory BAR, the first in this
// kernel.
//
// Why not just use the bootloader's physical-memory window
// (`physical_memory_offset() + bar`), the way ac97's DMA buffers do? That
// window does cover the BAR — `bootloader` 0.11 maps at least the first
// 4 GiB precisely so MMIO stays reachable — but it maps it **write-back
// cacheable**, and device registers are not memory: a cached read can hand
// back a stale copy of a status register that the device has since
// changed, and a write can sit in the store buffer or be merged with a
// neighbouring one. On most PCs the firmware's MTRRs already mark the PCI
// hole uncacheable, which is why drivers that skip this step often appear
// to work; relying on that is relying on the firmware, and the machine
// this driver was written for is exactly the one where a wrong guess costs
// a reboot to find out.
//
// So: a separate 4 KiB-granular mapping with PWT|PCD set (uncacheable
// under the default PAT), in a virtual region of this module's own.
//
// DMA buffers deliberately do *not* go through here and stay on the
// cacheable window: x86 DMA is cache-coherent (the hardware snoops), so
// making ring memory uncacheable would only make every TRB read slow.

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::structures::paging::{Page, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

use super::page_table_manager::OwnedPageTable;

/// Virtual address the next mapping starts at, bumped forward as windows
/// are handed out. Zero until the first call picks a base.
static NEXT_VIRT: AtomicU64 = AtomicU64::new(0);

/// Upper bound on how much of the chosen PML4 slot this module will hand
/// out — one 512 GiB slot is far more than a handful of BARs need, but the
/// bump pointer is capped anyway so a caller passing a nonsense length
/// can't walk out of the region it was given.
const REGION_SIZE: u64 = 1 << 30; // 1 GiB

/// Picks the virtual base for the MMIO region: the first unused entry in
/// the current (kernel) PML4, walked from the top down so it lands far
/// from anything the bootloader placed low.
///
/// Choosing an entry that is *unused* matters twice over. It guarantees no
/// collision with the bootloader's own mappings (kernel image, physical
/// window, framebuffer) without having to know where it put them; and
/// because `OwnedPageTable::new_user` copies every non-user kernel PML4
/// entry into each new address space, a mapping created here before the
/// first process exists is inherited by all of them — which is what lets
/// the timer ISR touch these registers no matter whose page table is live.
fn choose_base() -> Option<u64> {
    let phys_offset = super::physical_memory_offset();
    let (frame, _) = x86_64::registers::control::Cr3::read();
    let pml4_virt = phys_offset + frame.start_address().as_u64();
    // SAFETY: CR3's table is mapped through the bootloader's physical
    // window, exactly as `OwnedPageTable::new_user` reads it.
    let pml4 = unsafe { &*pml4_virt.as_ptr::<x86_64::structures::paging::PageTable>() };

    // Higher half only (256..512), and never the last entry — some
    // firmware and the recursive-mapping convention both like to live
    // there.
    for index in (256..511).rev() {
        if pml4[index].is_unused() {
            // Sign-extend: any higher-half index has bits 63:48 set.
            return Some(0xFFFF_0000_0000_0000 | (index as u64) << 39);
        }
    }
    None
}

/// Maps `len` bytes of physical MMIO at `phys` into kernel space,
/// uncached, and returns the virtual address of `phys` itself (the page
/// offset is preserved, so an unaligned BAR still resolves correctly).
///
/// Best-effort like every hardware path here: returns `None` rather than
/// panicking if there is no free PML4 slot, the region is exhausted, or a
/// page can't be mapped.
///
/// # Safety
///
/// `phys`..`phys+len` must be a device register window, not RAM in use by
/// anything else — mapping ordinary memory uncached would silently wreck
/// its performance, and mapping someone else's frames would alias them.
pub unsafe fn map(phys: PhysAddr, len: usize) -> Option<VirtAddr> {
    if len == 0 {
        return None;
    }

    // Publish a base on first use. A plain compare-exchange is enough:
    // this runs at boot, single-threaded, before any process exists.
    let mut base = NEXT_VIRT.load(Ordering::Relaxed);
    if base == 0 {
        base = choose_base()?;
        NEXT_VIRT.store(base, Ordering::Relaxed);
    }
    let region_start = base & !(REGION_SIZE - 1);

    let page_offset = phys.as_u64() & 0xFFF;
    let first_frame = PhysFrame::<Size4KiB>::containing_address(phys);
    let page_count = ((page_offset as usize + len) + 0xFFF) / 0x1000;

    let virt_start = (base + 0xFFF) & !0xFFFu64;
    let virt_end = virt_start + (page_count as u64) * 0x1000;
    if virt_end > region_start + REGION_SIZE {
        crate::serial_println!("mmio: region exhausted, cannot map {:#x}", phys.as_u64());
        return None;
    }

    // PWT|PCD — uncacheable under the default PAT. No NO_EXECUTE: whether
    // EFER.NXE is enabled here isn't this module's business, and setting
    // the bit without it is a reserved-bit page fault.
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::WRITE_THROUGH
        | PageTableFlags::NO_CACHE;

    let kernel_table = OwnedPageTable::from_current();
    for i in 0..page_count {
        let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt_start + (i as u64) * 0x1000));
        let frame = first_frame + i as u64;
        if let Err(e) = kernel_table.map_existing_frame(page, frame, flags) {
            crate::serial_println!(
                "mmio: failed to map {:#x} -> {:#x}: {}",
                frame.start_address().as_u64(),
                page.start_address().as_u64(),
                e
            );
            return None;
        }
    }

    NEXT_VIRT.store(virt_end, Ordering::Relaxed);
    Some(VirtAddr::new(virt_start + page_offset))
}
