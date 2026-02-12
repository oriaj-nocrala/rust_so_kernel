// kernel/src/allocator/buddy_allocator.rs
//
// Buddy allocator for physical memory management.
//
// CORRECTED:
//   - Removed `remove_block` which assumed addr == head without checking.
//     `remove_arbitrary_block` now calls `remove_from_head` for the head case.
//   - `remove_from_head` has debug_assert to verify the invariant.
//   - Cleaned up commented-out debug code.

use x86_64::PhysAddr;
use spin::Mutex;

const MIN_ORDER: usize = 12; // 4KB (2^12)
const MAX_ORDER: usize = 28; // 256MB (2^28)

pub struct BuddyAllocator {
    free_lists: [FreeList; MAX_ORDER - MIN_ORDER + 1],
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
            free_lists: [INIT; MAX_ORDER - MIN_ORDER + 1],
            total_memory: 0,
        }
    }

    /// Convert absolute order (12..=28) to array index (0..=16).
    #[inline]
    fn order_to_index(&self, order: usize) -> usize {
        order - MIN_ORDER
    }

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

    /// Add a block to its order's free list (push to head).
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
    }

    /// Remove the HEAD block from its order's free list.
    ///
    /// PRECONDITION: `addr` MUST be the current head of the free list.
    /// Verified by debug_assert in debug builds.
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
    }

    // NOTE: The old `remove_block(order, addr)` was deleted.
    // It did the same thing as `remove_from_head` but WITHOUT the
    // debug_assert, making it silently corrupt the free list if
    // addr != head.  All callers now use either `remove_from_head`
    // (when addr is known to be head) or `remove_arbitrary_block`
    // (which handles both cases safely).

    /// Remove an ARBITRARY block from its order's free list.
    ///
    /// Handles both the head case (O(1)) and the general case (O(n) scan).
    /// Called during coalescing, where the buddy may be anywhere in the list.
    unsafe fn remove_arbitrary_block(&mut self, order: usize, addr: PhysAddr) {
        let idx = self.order_to_index(order);
        let phys_offset = crate::memory::physical_memory_offset();

        // Fast path: block is the head
        if self.free_lists[idx].head == Some(addr) {
            self.remove_from_head(order, addr);
            return;
        }

        // Slow path: scan the list for the block
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
                    return;
                }
                Some(next_addr) => {
                    prev_addr = next_addr;
                }
                None => return,
            }
        }
    }

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

    /// Check if a block is in the free list for its order.
    ///
    /// NOTE: O(n) in the length of the free list.
    /// TODO(P2): Replace with a bitmap for O(1) lookup.
    unsafe fn is_free(&self, order: usize, addr: PhysAddr) -> bool {
        let idx = self.order_to_index(order);
        let mut current = self.free_lists[idx].head;

        while let Some(block_addr) = current {
            if block_addr == addr {
                return true;
            }

            let phys_offset = crate::memory::physical_memory_offset();
            let virt = phys_offset + block_addr.as_u64();
            let block = &*(virt.as_ptr::<FreeBlock>());
            current = block.next;
        }

        false
    }

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

        crate::serial_print_raw!("Buddy: OOM for order ");
        print_usize(order);
        crate::serial_print_raw!("\n");
        None
    }

    /// Free a previously allocated block.
    ///
    /// # Safety
    /// - `addr` must have been returned by `allocate(order)` with the same order.
    /// - Must not be freed twice (no double-free).
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

        // Coalesce with buddy until MAX_ORDER or buddy is not free
        while current_order < MAX_ORDER {
            let buddy_addr = self.buddy_of(current_addr, current_order);

            if !self.is_free(current_order, buddy_addr) {
                break;
            }

            self.remove_arbitrary_block(current_order, buddy_addr);

            current_addr = PhysAddr::new(current_addr.as_u64().min(buddy_addr.as_u64()));
            current_order += 1;
        }

        self.add_block(current_order, current_addr);
    }

    /// Debug: print statistics without using format!() (avoids deadlocks).
    pub fn debug_print_stats(&self) {
        crate::serial_print_raw!("Buddy Allocator Stats:\n");
        crate::serial_print_raw!("  Total memory: ");
        print_u64(self.total_memory / (1024 * 1024));
        crate::serial_print_raw!("MB\n");

        for order in MIN_ORDER..=MAX_ORDER {
            let idx = self.order_to_index(order);
            let mut count = 0;

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
                crate::serial_print_raw!("  Order ");
                print_usize(order);
                crate::serial_print_raw!(": ");
                print_usize(count);
                crate::serial_print_raw!(" blocks of ");

                if block_size >= 1024 * 1024 {
                    print_u64(block_size / (1024 * 1024));
                    crate::serial_print_raw!("MB\n");
                } else {
                    print_u64(block_size / 1024);
                    crate::serial_print_raw!("KB\n");
                }
            }
        }
    }
}

// Global instance
pub static BUDDY: Mutex<BuddyAllocator> = Mutex::new(BuddyAllocator::new());

// ============================================================================
// Raw print helpers (no allocation, direct port I/O)
// ============================================================================

fn print_u64(mut n: u64) {
    if n == 0 {
        crate::serial_print_raw!("0");
        return;
    }

    let mut buf = [0u8; 20];
    let mut i = 0;

    while n > 0 {
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }

    while i > 0 {
        i -= 1;
        unsafe {
            let mut port = x86_64::instructions::port::Port::<u8>::new(0x3F8);
            port.write(buf[i]);
        }
    }
}

fn print_hex(n: u64) {
    crate::serial_print_raw!("0x");

    if n == 0 {
        crate::serial_print_raw!("0");
        return;
    }

    let mut buf = [0u8; 16];
    let mut num = n;
    let mut i = 0;

    while num > 0 {
        let digit = (num % 16) as u8;
        buf[i] = if digit < 10 { b'0' + digit } else { b'a' + (digit - 10) };
        num /= 16;
        i += 1;
    }

    while i > 0 {
        i -= 1;
        unsafe {
            let mut port = x86_64::instructions::port::Port::<u8>::new(0x3F8);
            port.write(buf[i]);
        }
    }
}

fn print_usize(n: usize) {
    print_u64(n as u64);
}