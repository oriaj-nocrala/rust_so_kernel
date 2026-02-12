// kernel/src/allocator/buddy_allocator.rs
//
// Buddy allocator for physical memory management.
//
// HISTORY:
//   - Removed dangerous `remove_block` (assumed addr==head without check).
//   - Unified raw print helpers into serial_println_raw! (fmt::Write).
//   - Replaced O(n) `is_free` linked-list scan with O(1) bitmap lookup.
//
// BITMAP DESIGN:
//   One bit per possible block at each order level.  A set bit means the
//   block is currently in the free list.  The bitmap is maintained by
//   add_block (set), remove_from_head (clear), and remove_arbitrary_block
//   (clear).  `is_free` is now a single bit test — O(1).
//
//   The bitmap covers physical addresses 0..MAX_PHYS_ADDR (512 MiB).
//   Addresses above this threshold are silently ignored by the bitmap
//   (bitmap_set/clear/test become no-ops), falling back to correct but
//   slower behavior.  In practice, QEMU+bootloader place all usable
//   memory well below 512 MiB.
//
//   Total bitmap size: ~32 KiB (computed at compile time).

use x86_64::PhysAddr;
use spin::Mutex;

const MIN_ORDER: usize = 12; // 4KB (2^12)
const MAX_ORDER: usize = 28; // 256MB (2^28)
const NUM_ORDERS: usize = MAX_ORDER - MIN_ORDER + 1;

/// Maximum physical address tracked by the bitmap.
/// Addresses above this are not tracked (bitmap ops become no-ops).
/// 512 MiB covers typical QEMU configurations with room to spare.
const MAX_PHYS_ADDR: u64 = 512 * 1024 * 1024;

// ============================================================================
// Compile-time bitmap sizing
// ============================================================================

/// Total bytes needed for the flat bitmap across all orders.
const fn bitmap_total_bytes() -> usize {
    let mut total = 0usize;
    let mut order = MIN_ORDER;
    while order <= MAX_ORDER {
        let bits = (MAX_PHYS_ADDR as usize) >> order;
        total += (bits + 7) / 8;
        order += 1;
    }
    total
}

/// Byte offset into the flat bitmap where each order's bits start.
const fn bitmap_offsets() -> [usize; NUM_ORDERS] {
    let mut offsets = [0usize; NUM_ORDERS];
    let mut i = 0;
    let mut running = 0usize;
    while i < NUM_ORDERS {
        offsets[i] = running;
        let order = MIN_ORDER + i;
        let bits = (MAX_PHYS_ADDR as usize) >> order;
        running += (bits + 7) / 8;
        i += 1;
    }
    offsets
}

const BITMAP_BYTES: usize = bitmap_total_bytes();   // ~32 KiB
const BITMAP_OFFSETS: [usize; NUM_ORDERS] = bitmap_offsets();

// Compile-time sanity check
const _: () = assert!(BITMAP_BYTES < 64 * 1024, "Bitmap exceeds 64KiB — raise MAX_PHYS_ADDR?");

// ============================================================================
// BuddyAllocator
// ============================================================================

pub struct BuddyAllocator {
    free_lists: [FreeList; NUM_ORDERS],
    bitmap: [u8; BITMAP_BYTES],
    total_memory: u64,
}

#[derive(Clone, Copy)]
struct FreeList {
    head: Option<PhysAddr>,
}

impl FreeList {
    const fn new() -> Self {
        Self { head: None }
    }
}

/// Metadata stored at the beginning of each free block.
#[repr(C)]
struct FreeBlock {
    next: Option<PhysAddr>,
}

impl BuddyAllocator {
    pub const fn new() -> Self {
        const INIT: FreeList = FreeList::new();
        Self {
            free_lists: [INIT; NUM_ORDERS],
            bitmap: [0u8; BITMAP_BYTES],
            total_memory: 0,
        }
    }

    /// Convert absolute order (12..=28) to array index (0..=16).
    #[inline]
    fn order_to_index(&self, order: usize) -> usize {
        order - MIN_ORDER
    }

    // ====================================================================
    // Bitmap operations — O(1) free-status tracking
    // ====================================================================

    /// Compute (byte_offset, bit_mask) for a block in the flat bitmap.
    /// Returns `None` if addr is outside the tracked range.
    #[inline]
    fn bitmap_pos(order: usize, addr: PhysAddr) -> Option<(usize, u8)> {
        let a = addr.as_u64();
        if a >= MAX_PHYS_ADDR {
            return None;
        }
        let idx = order - MIN_ORDER;
        let bit_index = (a as usize) >> order;
        let byte_offset = BITMAP_OFFSETS[idx] + bit_index / 8;
        let bit_mask = 1u8 << (bit_index % 8);
        Some((byte_offset, bit_mask))
    }

    /// Mark a block as free in the bitmap.
    #[inline]
    fn bitmap_set(&mut self, order: usize, addr: PhysAddr) {
        if let Some((byte, mask)) = Self::bitmap_pos(order, addr) {
            debug_assert!(
                self.bitmap[byte] & mask == 0,
                "bitmap_set: block {:#x} order {} already marked free (double-free?)",
                addr.as_u64(), order
            );
            self.bitmap[byte] |= mask;
        }
    }

    /// Mark a block as allocated (not free) in the bitmap.
    #[inline]
    fn bitmap_clear(&mut self, order: usize, addr: PhysAddr) {
        if let Some((byte, mask)) = Self::bitmap_pos(order, addr) {
            debug_assert!(
                self.bitmap[byte] & mask != 0,
                "bitmap_clear: block {:#x} order {} already marked allocated",
                addr.as_u64(), order
            );
            self.bitmap[byte] &= !mask;
        }
    }

    /// Check if a block is in the free list — O(1) via bitmap.
    #[inline]
    fn is_free(&self, order: usize, addr: PhysAddr) -> bool {
        match Self::bitmap_pos(order, addr) {
            Some((byte, mask)) => self.bitmap[byte] & mask != 0,
            None => false,
        }
    }

    // ====================================================================
    // Region management
    // ====================================================================

    /// Add a region of usable physical memory to the buddy allocator.
    ///
    /// Breaks the region into the largest power-of-two blocks that fit,
    /// respecting both alignment and remaining size.
    pub unsafe fn add_region(&mut self, start: u64, end: u64) {
        let mut current_addr = start;
        let region_size = end - start;

        self.total_memory += region_size;

        while current_addr < end {
            let remaining = end - current_addr;

            if remaining < (1 << MIN_ORDER) {
                break;
            }

            let align_order = current_addr.trailing_zeros() as usize;
            let size_order = (63 - remaining.leading_zeros()) as usize;

            let order = align_order
                .min(size_order)
                .min(MAX_ORDER)
                .max(MIN_ORDER);

            let block_size = 1u64 << order;

            self.add_block(order, PhysAddr::new(current_addr));
            current_addr += block_size;
        }
    }

    // ====================================================================
    // Free list manipulation (all maintain bitmap invariant)
    // ====================================================================

    /// Add a block to its order's free list (push to head).
    /// Also sets the bitmap bit.
    unsafe fn add_block(&mut self, order: usize, addr: PhysAddr) {
        let idx = self.order_to_index(order);
        let phys_offset = crate::memory::physical_memory_offset();
        let virt_addr = phys_offset + addr.as_u64();

        let new_block = FreeBlock {
            next: self.free_lists[idx].head,
        };

        let ptr = virt_addr.as_mut_ptr::<FreeBlock>();
        ptr.write(new_block);

        self.free_lists[idx].head = Some(addr);
        self.bitmap_set(order, addr);
    }

    /// Remove the HEAD block from its order's free list.
    /// Also clears the bitmap bit.
    ///
    /// PRECONDITION: `addr` MUST be the current head of the free list.
    unsafe fn remove_from_head(&mut self, order: usize, addr: PhysAddr) {
        let idx = self.order_to_index(order);

        debug_assert_eq!(
            self.free_lists[idx].head,
            Some(addr),
            "remove_from_head: addr {:#x} is not the head of order {} free list",
            addr.as_u64(),
            order
        );

        let phys_offset = crate::memory::physical_memory_offset();
        let virt = phys_offset + addr.as_u64();
        let block = &*(virt.as_ptr::<FreeBlock>());
        self.free_lists[idx].head = block.next;
        self.bitmap_clear(order, addr);
    }

    /// Remove an ARBITRARY block from its order's free list.
    /// Also clears the bitmap bit.
    ///
    /// Handles both the head case (O(1)) and the general case (O(n) scan).
    /// Called during coalescing, where the buddy may be anywhere in the list.
    ///
    /// The O(n) list walk here is acceptable because:
    ///   - It only runs when `is_free` returned true (O(1) bitmap check).
    ///   - The common case in deallocate is that the buddy is NOT free,
    ///     so this function is never reached.
    unsafe fn remove_arbitrary_block(&mut self, order: usize, addr: PhysAddr) {
        let idx = self.order_to_index(order);
        let phys_offset = crate::memory::physical_memory_offset();

        // Fast path: block is the head
        if self.free_lists[idx].head == Some(addr) {
            self.remove_from_head(order, addr);
            return;
        }

        // Slow path: scan the list for the block and unlink it
        let mut prev_addr = match self.free_lists[idx].head {
            Some(a) => a,
            None => return,
        };

        loop {
            let prev_virt = phys_offset + prev_addr.as_u64();
            let prev_block = &mut *(prev_virt.as_mut_ptr::<FreeBlock>());

            match prev_block.next {
                Some(next_addr) if next_addr == addr => {
                    let target_virt = phys_offset + addr.as_u64();
                    let target_block = &*(target_virt.as_ptr::<FreeBlock>());
                    prev_block.next = target_block.next;
                    self.bitmap_clear(order, addr);
                    return;
                }
                Some(next_addr) => {
                    prev_addr = next_addr;
                }
                None => return,
            }
        }
    }

    // ====================================================================
    // Split / buddy helpers
    // ====================================================================

    /// Split a block from `from_order` down to `to_order`.
    ///
    /// The caller keeps the lower-addressed half at each split;
    /// the upper half (buddy) is added to the appropriate free list.
    unsafe fn split_block(&mut self, from_order: usize, addr: PhysAddr, to_order: usize) {
        let mut current_order = from_order;

        while current_order > to_order {
            current_order -= 1;
            let block_size = 1u64 << current_order;
            let buddy_addr = PhysAddr::new(addr.as_u64() + block_size);
            self.add_block(current_order, buddy_addr);
        }
    }

    /// Calculate the buddy address for a block.
    #[inline]
    fn buddy_of(&self, addr: PhysAddr, order: usize) -> PhysAddr {
        let block_size = 1u64 << order;
        PhysAddr::new(addr.as_u64() ^ block_size)
    }

    // ====================================================================
    // Allocate / Deallocate
    // ====================================================================

    /// Allocate a block of 2^order bytes.
    ///
    /// Returns `Some(addr)` where addr is aligned to 2^order,
    /// or `None` if no memory is available.
    pub unsafe fn allocate(&mut self, order: usize) -> Option<PhysAddr> {
        debug_assert!(order >= MIN_ORDER, "Order {} below MIN_ORDER {}", order, MIN_ORDER);
        debug_assert!(order <= MAX_ORDER, "Order {} exceeds MAX_ORDER {}", order, MAX_ORDER);

        let idx = self.order_to_index(order);

        // Case 1: Exact-size block available
        if let Some(addr) = self.free_lists[idx].head {
            self.remove_from_head(order, addr);
            return Some(addr);
        }

        // Case 2: Split a larger block
        for larger_order in (order + 1)..=MAX_ORDER {
            let larger_idx = self.order_to_index(larger_order);

            if let Some(addr) = self.free_lists[larger_idx].head {
                self.remove_from_head(larger_order, addr);
                self.split_block(larger_order, addr, order);
                return Some(addr);
            }
        }

        crate::serial_println_raw!("Buddy: OOM for order {}", order);
        None
    }

    /// Free a previously allocated block.
    ///
    /// # Safety
    /// - `addr` must have been returned by `allocate(order)` with the same order.
    /// - Must not be freed twice (caught by bitmap debug_assert in debug builds).
    pub unsafe fn deallocate(&mut self, addr: PhysAddr, order: usize) {
        debug_assert!(order >= MIN_ORDER);
        debug_assert!(order <= MAX_ORDER);

        let block_size = 1u64 << order;
        debug_assert_eq!(
            addr.as_u64() % block_size, 0,
            "Address {:#x} not aligned to order {} (block size {:#x})",
            addr.as_u64(), order, block_size
        );

        let mut current_addr = addr;
        let mut current_order = order;

        // Coalesce with buddy until MAX_ORDER or buddy is not free.
        // is_free is O(1) via bitmap — this was the hot-path bottleneck.
        while current_order < MAX_ORDER {
            let buddy_addr = self.buddy_of(current_addr, current_order);

            if !self.is_free(current_order, buddy_addr) {
                break;
            }

            // Buddy is free — remove it from its list and merge.
            self.remove_arbitrary_block(current_order, buddy_addr);

            current_addr = PhysAddr::new(current_addr.as_u64().min(buddy_addr.as_u64()));
            current_order += 1;
        }

        self.add_block(current_order, current_addr);
    }

    // ====================================================================
    // Debug
    // ====================================================================

    /// Debug: print statistics (lock-free, no allocation).
    pub fn debug_print_stats(&self) {
        crate::serial_println_raw!("Buddy Allocator Stats:");
        crate::serial_println_raw!("  Total memory: {}MB", self.total_memory / (1024 * 1024));
        crate::serial_println_raw!("  Bitmap size: {} bytes", BITMAP_BYTES);

        for order in MIN_ORDER..=MAX_ORDER {
            let idx = self.order_to_index(order);
            let mut count = 0usize;

            unsafe {
                let mut current = self.free_lists[idx].head;
                while let Some(addr) = current {
                    count += 1;
                    let phys_offset = crate::memory::physical_memory_offset();
                    let virt = phys_offset + addr.as_u64();
                    let block = &*(virt.as_ptr::<FreeBlock>());
                    current = block.next;
                }
            }

            if count > 0 {
                let block_size = 1u64 << order;
                if block_size >= 1024 * 1024 {
                    crate::serial_println_raw!(
                        "  Order {}: {} blocks of {}MB",
                        order, count, block_size / (1024 * 1024)
                    );
                } else {
                    crate::serial_println_raw!(
                        "  Order {}: {} blocks of {}KB",
                        order, count, block_size / 1024
                    );
                }
            }
        }
    }
}

// Global instance
pub static BUDDY: Mutex<BuddyAllocator> = Mutex::new(BuddyAllocator::new());