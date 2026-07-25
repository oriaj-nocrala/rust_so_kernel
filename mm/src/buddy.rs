// mm/src/buddy.rs
//
// Buddy allocator for physical memory management.
//
// Moved verbatim (mechanical refactor, see `crate` doc comment for the two
// seams this needed) out of `kernel/src/allocator/buddy_allocator.rs`. Same
// order arithmetic, same bitmap layout, same split/merge order, same
// `debug_assert!`s as the pre-extraction file — do not "fix" anything found
// while reading this, note it instead (see the extraction report).
//
// HISTORY (carried over from the pre-extraction file):
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

use crate::PhysMap;
use x86_64::PhysAddr;

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
// Reported-by-value events — see crate doc comment's "Two seams" section.
// ============================================================================

/// A "phantom" bitmap entry encountered while coalescing during
/// `deallocate` — the bitmap said a buddy block was free but the
/// intrusive free list didn't actually contain it. Recoverable: the
/// caller clears the bitmap bit and coalescing stops at the current order
/// (same as the pre-extraction code). `deallocate` returns at most one of
/// these per call — coalescing stops immediately the first time this
/// happens, exactly like the original `if !remove_arbitrary_block(..) {
/// break; }`.
///
/// Carries exactly the values the pre-extraction kernel code printed
/// inline, so `kernel/src/allocator/mod.rs` can reproduce the identical
/// `serial_println_raw!` text. `NotFound` preserves a pre-existing
/// anomaly on purpose: the original format string's `free_list[{}]` slot
/// there was fed `order`, not the list index (`idx = order - MIN_ORDER`,
/// what `EmptyList`/`LoopLimit` both correctly use) — kept byte-for-byte
/// identical, not fixed, since this is a mechanical move, not a bugfix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PhantomEvent {
    /// `free_lists[idx]` was already fully empty.
    EmptyList { addr: PhysAddr, order: usize, idx: usize },
    /// Scanned more than 4096 links without finding the block.
    LoopLimit { addr: PhysAddr, idx: usize },
    /// Walked the whole list without finding `addr`. See this enum's doc
    /// comment: `order` here is what the original code printed in the
    /// `free_list[{}]` slot, not `idx` — preserved as-is.
    NotFound { addr: PhysAddr, order: usize },
}

/// One order's worth of free-list bookkeeping — what
/// `kernel/src/allocator/mod.rs` prints as one line of the (former)
/// `debug_print_stats` output when `block_count > 0`.
#[derive(Debug, Clone, Copy)]
pub struct OrderStat {
    pub order: usize,
    pub block_count: usize,
}

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
    ///
    /// # Panics
    /// If the block is already marked free (double-free). The
    /// pre-extraction code printed and then `loop { hlt }`ed here instead
    /// — see the crate doc comment for why `panic!` is safe and correct in
    /// this crate instead.
    #[inline]
    fn bitmap_set(&mut self, order: usize, addr: PhysAddr) {
        if let Some((byte, mask)) = Self::bitmap_pos(order, addr) {
            if self.bitmap[byte] & mask != 0 {
                panic!(
                    "[BUDDY] DOUBLE-FREE: block {:#x} order {} already marked free!",
                    addr.as_u64(), order
                );
            }
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
    pub unsafe fn add_region(&mut self, mem: &dyn PhysMap, start: u64, end: u64) {
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

            self.add_block(mem, order, PhysAddr::new(current_addr));
            current_addr += block_size;
        }
    }

    // ====================================================================
    // Free list manipulation (all maintain bitmap invariant)
    // ====================================================================

    /// Add a block to its order's free list (push to head).
    /// Also sets the bitmap bit.
    unsafe fn add_block(&mut self, mem: &dyn PhysMap, order: usize, addr: PhysAddr) {
        let idx = self.order_to_index(order);

        let new_block = FreeBlock {
            next: self.free_lists[idx].head,
        };

        let ptr = mem.virt_for(addr) as *mut FreeBlock;
        ptr.write(new_block);

        self.free_lists[idx].head = Some(addr);
        self.bitmap_set(order, addr);
    }

    /// Remove the HEAD block from its order's free list.
    /// Also clears the bitmap bit.
    ///
    /// PRECONDITION: `addr` MUST be the current head of the free list.
    unsafe fn remove_from_head(&mut self, mem: &dyn PhysMap, order: usize, addr: PhysAddr) {
        let idx = self.order_to_index(order);

        debug_assert_eq!(
            self.free_lists[idx].head,
            Some(addr),
            "remove_from_head: addr {:#x} is not the head of order {} free list",
            addr.as_u64(),
            order
        );

        let block = &*(mem.virt_for(addr) as *const FreeBlock);
        self.free_lists[idx].head = block.next;
        self.bitmap_clear(order, addr);
    }

    /// Remove an ARBITRARY block from its order's free list.
    /// Also clears the bitmap bit.
    ///
    /// Returns `Ok(())` if the block was found and removed, `Err(event)` if
    /// the bitmap had a phantom entry (block not actually in the free
    /// list). In the `Err` case the phantom bitmap bit is cleared so
    /// future coalescing won't loop.
    ///
    /// Handles both the head case (O(1)) and the general case (O(n) scan).
    /// Called during coalescing, where the buddy may be anywhere in the list.
    unsafe fn remove_arbitrary_block(
        &mut self,
        mem: &dyn PhysMap,
        order: usize,
        addr: PhysAddr,
    ) -> Result<(), PhantomEvent> {
        let idx = self.order_to_index(order);

        // Fast path: block is the head
        if self.free_lists[idx].head == Some(addr) {
            self.remove_from_head(mem, order, addr);
            return Ok(());
        }

        // Slow path: scan the list for the block and unlink it
        let mut prev_addr = match self.free_lists[idx].head {
            Some(a) => a,
            None => {
                // Phantom bitmap entry — free list is completely empty.
                // Clear the phantom bit so future coalescing won't see it again.
                self.bitmap_clear(order, addr);
                return Err(PhantomEvent::EmptyList { addr, order, idx });
            }
        };

        let mut iters: usize = 0;
        loop {
            iters += 1;
            if iters > 4096 {
                // Pathological list — treat as phantom, clear bitmap, abort.
                self.bitmap_clear(order, addr);
                return Err(PhantomEvent::LoopLimit { addr, idx });
            }

            let prev_block = &mut *(mem.virt_for(prev_addr) as *mut FreeBlock);

            match prev_block.next {
                Some(next_addr) if next_addr == addr => {
                    let target_block = &*(mem.virt_for(addr) as *const FreeBlock);
                    prev_block.next = target_block.next;
                    self.bitmap_clear(order, addr);
                    return Ok(());
                }
                Some(next_addr) => {
                    prev_addr = next_addr;
                }
                None => {
                    // Block not found in list — phantom entry. Clear and abort.
                    self.bitmap_clear(order, addr);
                    return Err(PhantomEvent::NotFound { addr, order });
                }
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
    unsafe fn split_block(&mut self, mem: &dyn PhysMap, from_order: usize, addr: PhysAddr, to_order: usize) {
        let mut current_order = from_order;

        while current_order > to_order {
            current_order -= 1;
            let block_size = 1u64 << current_order;
            let buddy_addr = PhysAddr::new(addr.as_u64() + block_size);
            self.add_block(mem, current_order, buddy_addr);
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
    /// or `None` if no memory is available (OOM — the pre-extraction code
    /// printed `"Buddy: OOM for order {}"` here; the kernel adapter's
    /// `phys_alloc` now does that when this returns `None`).
    pub unsafe fn allocate(&mut self, mem: &dyn PhysMap, order: usize) -> Option<PhysAddr> {
        debug_assert!(order >= MIN_ORDER, "Order {} below MIN_ORDER {}", order, MIN_ORDER);
        debug_assert!(order <= MAX_ORDER, "Order {} exceeds MAX_ORDER {}", order, MAX_ORDER);

        let idx = self.order_to_index(order);

        // Case 1: Exact-size block available
        if let Some(addr) = self.free_lists[idx].head {
            self.remove_from_head(mem, order, addr);
            return Some(addr);
        }

        // Case 2: Split a larger block
        for larger_order in (order + 1)..=MAX_ORDER {
            let larger_idx = self.order_to_index(larger_order);

            if let Some(addr) = self.free_lists[larger_idx].head {
                self.remove_from_head(mem, larger_order, addr);
                self.split_block(mem, larger_order, addr, order);
                return Some(addr);
            }
        }

        None
    }

    /// Free a previously allocated block.
    ///
    /// Returns `Some(event)` if coalescing hit a phantom bitmap entry (see
    /// [`PhantomEvent`]) — the pre-extraction code printed inline here;
    /// callers now report it themselves (`kernel/src/allocator/mod.rs`'s
    /// `log_phantom_event`).
    ///
    /// # Safety
    /// - `addr` must have been returned by `allocate(order)` with the same order.
    /// - Must not be freed twice (caught by bitmap panic — see `bitmap_set`).
    pub unsafe fn deallocate(&mut self, mem: &dyn PhysMap, addr: PhysAddr, order: usize) -> Option<PhantomEvent> {
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
        let mut phantom = None;

        // Coalesce with buddy until MAX_ORDER or buddy is not free.
        // is_free is O(1) via bitmap — this was the hot-path bottleneck.
        while current_order < MAX_ORDER {
            let buddy_addr = self.buddy_of(current_addr, current_order);

            if !self.is_free(current_order, buddy_addr) {
                break;
            }

            // Buddy is free — remove it from its list and merge.
            // If remove fails, the bitmap had a phantom entry; abort
            // coalescing so we don't create a merged block that includes
            // memory that was never actually freed.
            match self.remove_arbitrary_block(mem, current_order, buddy_addr) {
                Ok(()) => {}
                Err(event) => {
                    phantom = Some(event);
                    break;
                }
            }

            current_addr = PhysAddr::new(current_addr.as_u64().min(buddy_addr.as_u64()));
            current_order += 1;
        }

        self.add_block(mem, current_order, current_addr);
        phantom
    }

    // ====================================================================
    // Debug / introspection
    // ====================================================================

    /// Total free physical memory, in bytes — same free-list traversal as
    /// `order_stats`, just summed instead of returned per-order. Cheap
    /// enough to call from a syscall (e.g. a shell `meminfo` command): each
    /// order's free list is normally short, and there are only
    /// `NUM_ORDERS` (17) of them.
    pub fn free_bytes(&self, mem: &dyn PhysMap) -> u64 {
        let mut total = 0u64;
        for order in MIN_ORDER..=MAX_ORDER {
            let idx = self.order_to_index(order);
            let mut count = 0u64;
            unsafe {
                let mut current = self.free_lists[idx].head;
                while let Some(addr) = current {
                    count += 1;
                    let block = &*(mem.virt_for(addr) as *const FreeBlock);
                    current = block.next;
                }
            }
            total += count * (1u64 << order);
        }
        total
    }

    /// Total physical memory this allocator owns, in bytes (sum of every
    /// region handed to it at boot via `add_region`/init — see `total_memory`).
    pub fn total_bytes(&self) -> u64 {
        self.total_memory
    }

    /// Size of the internal free-status bitmap, in bytes — what the
    /// pre-extraction `debug_print_stats` printed as `"Bitmap size: {}
    /// bytes"`.
    pub fn bitmap_bytes(&self) -> usize {
        BITMAP_BYTES
    }

    /// Per-order free-list block counts — what `debug_print_stats` used to
    /// walk and print directly. `kernel/src/allocator/mod.rs` now does the
    /// printing (skipping zero-count orders, same as before).
    pub fn order_stats(&self, mem: &dyn PhysMap) -> [OrderStat; NUM_ORDERS] {
        let mut out = [OrderStat { order: 0, block_count: 0 }; NUM_ORDERS];
        for (i, order) in (MIN_ORDER..=MAX_ORDER).enumerate() {
            let idx = self.order_to_index(order);
            let mut count = 0usize;
            unsafe {
                let mut current = self.free_lists[idx].head;
                while let Some(addr) = current {
                    count += 1;
                    let block = &*(mem.virt_for(addr) as *const FreeBlock);
                    current = block.next;
                }
            }
            out[i] = OrderStat { order, block_count: count };
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    /// Host-only `PhysMap`: a flat `Vec<u8>` standing in for physical
    /// memory, addressed the same way the kernel's real
    /// `physical_memory_offset()`-based implementation is — `virt_for(pa)`
    /// just returns a pointer into the backing buffer at offset `pa`.
    /// Sized generously enough (16 MiB) for every test below without
    /// needing anywhere near the real 512 MiB `MAX_PHYS_ADDR` the bitmap
    /// tracks — the bitmap itself is a fixed-size compile-time array
    /// regardless of how much of it a given test actually touches.
    struct VecMem {
        buf: std::cell::UnsafeCell<Vec<u8>>,
    }

    impl VecMem {
        fn new(size: usize) -> Self {
            Self { buf: std::cell::UnsafeCell::new(std::vec![0u8; size]) }
        }
    }

    // Safety: single-threaded tests only.
    unsafe impl Sync for VecMem {}

    impl PhysMap for VecMem {
        fn virt_for(&self, pa: PhysAddr) -> *mut u8 {
            unsafe {
                let buf = &mut *self.buf.get();
                assert!((pa.as_u64() as usize) < buf.len(), "test PhysMap OOB: {:#x}", pa.as_u64());
                buf.as_mut_ptr().add(pa.as_u64() as usize)
            }
        }
    }

    const TEST_MEM_SIZE: usize = 16 * 1024 * 1024; // 16 MiB

    fn new_allocator_with_region(mem: &VecMem, start: u64, end: u64) -> BuddyAllocator {
        let mut buddy = BuddyAllocator::new();
        unsafe { buddy.add_region(mem, start, end); }
        buddy
    }

    // ── Pure order/bitmap arithmetic ────────────────────────────────────

    #[test]
    fn order_to_index_roundtrip() {
        let buddy = BuddyAllocator::new();
        assert_eq!(buddy.order_to_index(MIN_ORDER), 0);
        assert_eq!(buddy.order_to_index(MAX_ORDER), NUM_ORDERS - 1);
        assert_eq!(buddy.order_to_index(16), 4);
    }

    #[test]
    fn buddy_of_is_involution() {
        let buddy = BuddyAllocator::new();
        let addr = PhysAddr::new(0x10000);
        let order = 12;
        let b = buddy.buddy_of(addr, order);
        // buddy_of(buddy_of(x)) == x, since it's a pure XOR with the block size.
        assert_eq!(buddy.buddy_of(b, order), addr);
        assert_ne!(b, addr);
    }

    #[test]
    fn buddy_of_differs_only_in_order_bit() {
        let buddy = BuddyAllocator::new();
        // Two adjacent order-12 (4 KiB) blocks starting at a 8 KiB-aligned
        // address are buddies of each other.
        let a = PhysAddr::new(0x100000);
        let b = PhysAddr::new(0x101000);
        assert_eq!(buddy.buddy_of(a, 12), b);
        assert_eq!(buddy.buddy_of(b, 12), a);
    }

    #[test]
    fn bitmap_pos_within_range() {
        let pos_lo = BuddyAllocator::bitmap_pos(MIN_ORDER, PhysAddr::new(0));
        assert!(pos_lo.is_some());
        let pos_hi = BuddyAllocator::bitmap_pos(MIN_ORDER, PhysAddr::new(MAX_PHYS_ADDR - 4096));
        assert!(pos_hi.is_some());
    }

    #[test]
    fn bitmap_pos_out_of_range_is_none() {
        assert_eq!(BuddyAllocator::bitmap_pos(MIN_ORDER, PhysAddr::new(MAX_PHYS_ADDR)), None);
        assert_eq!(BuddyAllocator::bitmap_pos(MIN_ORDER, PhysAddr::new(MAX_PHYS_ADDR + 4096)), None);
    }

    #[test]
    fn bitmap_pos_distinct_orders_distinct_offsets() {
        let a = BuddyAllocator::bitmap_pos(MIN_ORDER, PhysAddr::new(0)).unwrap();
        let b = BuddyAllocator::bitmap_pos(MIN_ORDER + 1, PhysAddr::new(0)).unwrap();
        assert_ne!(a.0, b.0, "different orders must land in different bitmap regions");
    }

    #[test]
    fn bitmap_total_bytes_matches_offsets_sum() {
        // The last order's offset plus its own bit count should reach BITMAP_BYTES.
        let last_order = MAX_ORDER;
        let last_idx = last_order - MIN_ORDER;
        let bits = (MAX_PHYS_ADDR as usize) >> last_order;
        let expected_end = BITMAP_OFFSETS[last_idx] + (bits + 7) / 8;
        assert_eq!(expected_end, BITMAP_BYTES);
    }

    // ── add_region ───────────────────────────────────────────────────────

    #[test]
    fn add_region_tracks_total_memory() {
        let mem = VecMem::new(TEST_MEM_SIZE);
        let buddy = new_allocator_with_region(&mem, 0, 1024 * 1024);
        assert_eq!(buddy.total_bytes(), 1024 * 1024);
    }

    #[test]
    fn add_region_below_min_order_is_dropped() {
        // A region smaller than one page (4 KiB) can't be tracked at all.
        let mem = VecMem::new(TEST_MEM_SIZE);
        let mut buddy = BuddyAllocator::new();
        unsafe { buddy.add_region(&mem, 0, 100); }
        // total_memory still accounts for the requested span (matches
        // pre-extraction behavior: total_memory += region_size happens
        // unconditionally before the loop that may add zero blocks).
        assert_eq!(buddy.total_bytes(), 100);
        assert_eq!(buddy.free_bytes(&mem), 0);
    }

    #[test]
    fn add_region_produces_free_bytes_equal_to_region_size_when_aligned() {
        let mem = VecMem::new(TEST_MEM_SIZE);
        let size = 1024 * 1024; // 1 MiB, page-aligned
        let buddy = new_allocator_with_region(&mem, 0, size);
        assert_eq!(buddy.free_bytes(&mem), size);
    }

    // ── allocate / deallocate round trip ────────────────────────────────

    #[test]
    fn allocate_exact_order_from_head() {
        let mem = VecMem::new(TEST_MEM_SIZE);
        let mut buddy = new_allocator_with_region(&mem, 0, 1 << 16); // 64 KiB, one order-16 block
        let addr = unsafe { buddy.allocate(&mem, 16) };
        assert_eq!(addr, Some(PhysAddr::new(0)));
        assert_eq!(buddy.free_bytes(&mem), 0);
    }

    #[test]
    fn allocate_splits_larger_block() {
        let mem = VecMem::new(TEST_MEM_SIZE);
        let mut buddy = new_allocator_with_region(&mem, 0, 1 << 16); // one 64 KiB (order 16) block
        let addr = unsafe { buddy.allocate(&mem, 12) }; // ask for one 4 KiB page
        assert_eq!(addr, Some(PhysAddr::new(0)));
        // The rest (60 KiB) should have been split off into the free lists.
        assert_eq!(buddy.free_bytes(&mem), (1 << 16) - (1 << 12));
    }

    #[test]
    fn allocate_oom_returns_none() {
        let mem = VecMem::new(TEST_MEM_SIZE);
        let mut buddy = new_allocator_with_region(&mem, 0, 1 << 12); // exactly one page
        let first = unsafe { buddy.allocate(&mem, 12) };
        assert!(first.is_some());
        let second = unsafe { buddy.allocate(&mem, 12) };
        assert_eq!(second, None);
    }

    #[test]
    fn deallocate_returns_memory_to_free_list() {
        let mem = VecMem::new(TEST_MEM_SIZE);
        let mut buddy = new_allocator_with_region(&mem, 0, 1 << 16);
        let addr = unsafe { buddy.allocate(&mem, 16) }.unwrap();
        assert_eq!(buddy.free_bytes(&mem), 0);
        unsafe { buddy.deallocate(&mem, addr, 16); }
        assert_eq!(buddy.free_bytes(&mem), 1 << 16);
    }

    #[test]
    fn alloc_dealloc_roundtrip_preserves_total_free() {
        let mem = VecMem::new(TEST_MEM_SIZE);
        let mut buddy = new_allocator_with_region(&mem, 0, 1 << 20); // 1 MiB
        let total = buddy.free_bytes(&mem);

        let mut allocs = std::vec::Vec::new();
        for _ in 0..16 {
            let a = unsafe { buddy.allocate(&mem, 12) }.expect("should have room");
            allocs.push(a);
        }
        assert_eq!(buddy.free_bytes(&mem), total - 16 * 4096);

        for a in allocs {
            unsafe { buddy.deallocate(&mem, a, 12); }
        }
        assert_eq!(buddy.free_bytes(&mem), total, "coalescing should fully reassemble the original block");
    }

    #[test]
    fn deallocate_coalesces_buddies_back_into_one_block() {
        let mem = VecMem::new(TEST_MEM_SIZE);
        let mut buddy = new_allocator_with_region(&mem, 0, 1 << 13); // two order-12 pages
        let a = unsafe { buddy.allocate(&mem, 12) }.unwrap();
        let b = unsafe { buddy.allocate(&mem, 12) }.unwrap();
        assert_eq!(buddy.free_bytes(&mem), 0);

        unsafe { buddy.deallocate(&mem, a, 12); }
        unsafe { buddy.deallocate(&mem, b, 12); }

        // After both buddies are freed they should have coalesced into a
        // single order-13 block — verified indirectly: a fresh order-13
        // allocation should now succeed.
        let whole = unsafe { buddy.allocate(&mem, 13) };
        assert_eq!(whole, Some(PhysAddr::new(0)));
    }

    #[test]
    fn deallocate_does_not_coalesce_when_buddy_still_allocated() {
        let mem = VecMem::new(TEST_MEM_SIZE);
        let mut buddy = new_allocator_with_region(&mem, 0, 1 << 13);
        let a = unsafe { buddy.allocate(&mem, 12) }.unwrap();
        let _b = unsafe { buddy.allocate(&mem, 12) }.unwrap();

        unsafe { buddy.deallocate(&mem, a, 12); }
        // Buddy (`_b`) is still allocated — a fresh order-12 alloc should
        // find the just-freed page again, not something coalesced.
        let again = unsafe { buddy.allocate(&mem, 12) };
        assert_eq!(again, Some(a));
    }

    #[test]
    #[should_panic(expected = "DOUBLE-FREE")]
    fn double_free_panics() {
        let mem = VecMem::new(TEST_MEM_SIZE);
        let mut buddy = new_allocator_with_region(&mem, 0, 1 << 12);
        let a = unsafe { buddy.allocate(&mem, 12) }.unwrap();
        unsafe {
            buddy.deallocate(&mem, a, 12);
            buddy.deallocate(&mem, a, 12); // second free of the same block
        }
    }

    // ── order_stats / bitmap_bytes ──────────────────────────────────────

    #[test]
    fn order_stats_reports_block_counts_per_order() {
        let mem = VecMem::new(TEST_MEM_SIZE);
        let buddy = new_allocator_with_region(&mem, 0, 1 << 16); // single order-16 block
        let stats = buddy.order_stats(&mem);
        let order16 = stats.iter().find(|s| s.order == 16).unwrap();
        assert_eq!(order16.block_count, 1);
        let order12 = stats.iter().find(|s| s.order == 12).unwrap();
        assert_eq!(order12.block_count, 0);
    }

    #[test]
    fn bitmap_bytes_matches_compile_time_constant() {
        let buddy = BuddyAllocator::new();
        assert_eq!(buddy.bitmap_bytes(), BITMAP_BYTES);
    }
}

