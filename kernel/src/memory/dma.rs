// kernel/src/memory/dma.rs
//
// Memory a device reads or writes by bus-master DMA (phase 1 of
// docs/gpu/gpu-plan.md).
//
// - Blocks come from the buddy allocator (the only frame allocator), are
//   zeroed, and are reached by the kernel through the physical-memory
//   window. That window is write-back cacheable, which is right for DMA on
//   x86: device accesses snoop the caches (see `memory::mmio`'s header).
// - The bus address is the physical address: this kernel does not enable
//   the IOMMU (decision D4 of the GPU plan). If it ever does, this module
//   is the one place that changes.
// - A device reaches only addresses under its DMA mask; `alloc` checks the
//   block against it and fails rather than hand out one the device would
//   truncate. The buddy cannot allocate below a limit, so there is no
//   retry: callers with narrow masks (QEMU `edu`, 28 bits) get an error.
// - Release is explicit (`free`). There is no `Drop`: a buffer that a
//   device may still be writing must never be freed by an unwinding scope
//   or by a `-> !` path skipping it (CLAUDE.md), so forgetting one leaks
//   instead of handing a live DMA target back to the allocator.

use alloc::vec::Vec;
use x86_64::PhysAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmaError {
    ZeroLength,
    OutOfMemory,
    /// The block the allocator returned is not reachable under the mask.
    AboveMask { phys: u64, mask: u64 },
}

/// A physically contiguous, zeroed block of `2^order` bytes.
#[must_use = "a DmaBuf is only released by DmaBuf::free"]
#[derive(Debug)]
pub struct DmaBuf {
    phys: u64,
    order: usize,
}

impl DmaBuf {
    /// At least `len` bytes, rounded up to a power-of-two number of pages.
    pub fn alloc(len: usize, mask: u64) -> Result<DmaBuf, DmaError> {
        let order = hal::dma::order_for(len).ok_or(DmaError::ZeroLength)?;
        // SAFETY: the block is ours until `free`.
        let phys = unsafe { crate::allocator::phys_alloc(order) }.ok_or(DmaError::OutOfMemory)?.as_u64();
        if !hal::dma::fits(phys, 1 << order, mask) {
            // SAFETY: just allocated with this order, never exposed.
            unsafe { crate::allocator::phys_free(PhysAddr::new(phys), order) };
            return Err(DmaError::AboveMask { phys, mask });
        }
        let buf = DmaBuf { phys, order };
        // SAFETY: `virt()` maps exactly this block, which nothing else uses.
        unsafe { core::ptr::write_bytes(buf.virt(), 0, buf.len()) };
        Ok(buf)
    }

    /// The address to program into the device.
    pub fn bus_addr(&self) -> u64 {
        self.phys
    }

    pub fn len(&self) -> usize {
        1 << self.order
    }

    /// The kernel's view of the block. Accesses racing a device DMA must be
    /// volatile (`read`/`write` below are).
    pub fn virt(&self) -> *mut u8 {
        (super::physical_memory_offset().as_u64() + self.phys) as *mut u8
    }

    /// Copies `data` into the block at `offset`. Panics on an out-of-range
    /// request (a driver bug, not a device condition).
    pub fn write(&self, offset: usize, data: &[u8]) {
        assert!(offset.checked_add(data.len()).is_some_and(|end| end <= self.len()));
        for (i, b) in data.iter().enumerate() {
            // SAFETY: in range per the assert; the block is ours.
            unsafe { core::ptr::write_volatile(self.virt().add(offset + i), *b) };
        }
    }

    /// Like [`write`](Self::write) for large copies (the 63 MB GSP image):
    /// plain `memcpy` then a store fence, instead of one volatile store per byte.
    /// The device must not be reading the range meanwhile.
    pub fn copy_in(&self, offset: usize, data: &[u8]) {
        assert!(offset.checked_add(data.len()).is_some_and(|end| end <= self.len()));
        // SAFETY: in range per the assert; the block is ours and not yet visible to the device.
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), self.virt().add(offset), data.len()) };
        // SAFETY: a store fence.
        unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)) };
    }

    /// Copies from the block at `offset` into `out`.
    pub fn read(&self, offset: usize, out: &mut [u8]) {
        assert!(offset.checked_add(out.len()).is_some_and(|end| end <= self.len()));
        for (i, b) in out.iter_mut().enumerate() {
            // SAFETY: in range per the assert; the block is ours.
            *b = unsafe { core::ptr::read_volatile(self.virt().add(offset + i)) };
        }
    }

    /// Returns the block to the allocator. The device must no longer be
    /// able to touch it.
    pub fn free(self) {
        // SAFETY: allocated by `alloc` with this order; consumed here.
        unsafe { crate::allocator::phys_free(PhysAddr::new(self.phys), self.order) };
    }
}

/// Many single pages that need not be contiguous: what a device that walks
/// its own page tables takes (the GSP firmware image goes through radix3
/// tables of these, so it never needs tens of MiB contiguous).
#[must_use = "DmaPages are only released by DmaPages::free"]
#[derive(Debug)]
pub struct DmaPages {
    pages: Vec<DmaBuf>,
}

impl DmaPages {
    pub const PAGE: usize = 4096;

    pub fn alloc(count: usize, mask: u64) -> Result<DmaPages, DmaError> {
        if count == 0 {
            return Err(DmaError::ZeroLength);
        }
        let mut pages = Vec::with_capacity(count);
        for _ in 0..count {
            match DmaBuf::alloc(Self::PAGE, mask) {
                Ok(p) => pages.push(p),
                Err(e) => {
                    for p in pages {
                        p.free();
                    }
                    return Err(e);
                }
            }
        }
        Ok(DmaPages { pages })
    }

    pub fn count(&self) -> usize {
        self.pages.len()
    }

    pub fn page(&self, i: usize) -> &DmaBuf {
        &self.pages[i]
    }

    pub fn free(self) {
        for p in self.pages {
            p.free();
        }
    }
}
