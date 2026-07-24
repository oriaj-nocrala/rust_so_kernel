// ext2/src/bitmap.rs
//
// Pure byte-packed bitmap operations shared by block/inode allocation
// (migration step 2). `find_first_free_bit` factors out the identical
// scan loop `alloc_block`/`alloc_inode` used to each duplicate inline in
// `kernel::fs::ext2` — same behavior, now in one place with its own edge-
// case tests instead of two copies exercised only by however `alloc_block`
// itself happened to get called under QEMU.
//
// LSB-first within each byte throughout, matching every on-disk ext2
// block/inode bitmap (bit `n`'s byte is `n / 8`, its mask is `1 << (n %
// 8)`) — this is the same convention `kernel::fs::ext2`'s own
// `mark_bit`/`bit_set` (used by `reclaim_orphans`, not migrated yet) uses.

/// Scan the first `valid_bits` bits of `bitmap` for the first clear
/// (free) bit. `None` if all `valid_bits` are set — the "this group/
/// filesystem is full" case `Ext2Core::alloc_block`/`alloc_inode`
/// translate into `ENOSPC` (via `Ok(None)`) in the kernel adapter.
///
/// Bits at or beyond `valid_bits` are never inspected — callers pass the
/// real in-group count (`Ext2Core::blocks_in_group`/`inodes_in_group`),
/// which can be smaller than a whole bitmap block's bit capacity for the
/// last, partially-populated group.
pub fn find_first_free_bit(bitmap: &[u8], valid_bits: u32) -> Option<u32> {
    for bit in 0..valid_bits {
        let byte = (bit / 8) as usize;
        let mask = 1u8 << (bit % 8);
        if bitmap[byte] & mask == 0 {
            return Some(bit);
        }
    }
    None
}

/// Set bit `bit` (0-based).
pub fn set_bit(bitmap: &mut [u8], bit: u32) {
    let byte = (bit / 8) as usize;
    bitmap[byte] |= 1u8 << (bit % 8);
}

/// Clear bit `bit` (0-based).
pub fn clear_bit(bitmap: &mut [u8], bit: u32) {
    let byte = (bit / 8) as usize;
    bitmap[byte] &= !(1u8 << (bit % 8));
}

/// Whether bit `bit` (0-based) is set. Out-of-range reads as clear rather
/// than panicking — mirrors `kernel::fs::ext2`'s own `bit_set` helper.
pub fn bit_is_set(bitmap: &[u8], bit: u32) -> bool {
    let byte = (bit / 8) as usize;
    bitmap.get(byte).is_some_and(|b| b & (1u8 << (bit % 8)) != 0)
}

/// Count clear (free) bits among the first `valid_bits` bits of `bitmap` —
/// shared by `repair::Ext2Core::reconcile_free_counts`'s block- and inode-
/// bitmap passes (migration step 5, moved verbatim out of
/// `kernel::fs::ext2`'s free-standing function of the same name) and by the
/// `true_free_counts_group0` test-inspection accessor.
pub fn count_free_bits(bitmap: &[u8], valid_bits: u32) -> u16 {
    let mut free = 0u32;
    for bit in 0..valid_bits {
        let byte = (bit / 8) as usize;
        let mask = 1u8 << (bit % 8);
        if bitmap[byte] & mask == 0 {
            free += 1;
        }
    }
    free as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn empty_bitmap_first_bit_is_free() {
        let bitmap = vec![0u8; 4];
        assert_eq!(find_first_free_bit(&bitmap, 32), Some(0));
    }

    #[test]
    fn full_bitmap_has_no_free_bit() {
        let bitmap = vec![0xFFu8; 4];
        assert_eq!(find_first_free_bit(&bitmap, 32), None);
    }

    #[test]
    fn finds_first_free_bit_mid_byte() {
        // Byte 0 = 0b0000_0111 -> bits 0,1,2 set, bit 3 is the first free one.
        let bitmap = [0b0000_0111u8, 0xFF, 0xFF, 0xFF];
        assert_eq!(find_first_free_bit(&bitmap, 32), Some(3));
    }

    #[test]
    fn finds_last_valid_bit_when_only_it_is_free() {
        // 16 valid bits, all set except bit 15 (the last one, MSB of byte 1).
        let bitmap = [0xFFu8, 0b0111_1111];
        assert_eq!(find_first_free_bit(&bitmap, 16), Some(15));
    }

    #[test]
    fn free_bit_beyond_valid_bits_is_not_found() {
        // Bit 20 is free, but valid_bits=16 means the scan must not see it.
        let bitmap = [0xFFu8, 0xFF, 0b1110_1111, 0xFF];
        assert_eq!(find_first_free_bit(&bitmap, 16), None);
    }

    #[test]
    fn crosses_byte_boundary_correctly() {
        // Bits 0..8 all set (byte 0 full), bit 8 (first bit of byte 1) free.
        let bitmap = [0xFFu8, 0b1111_1110];
        assert_eq!(find_first_free_bit(&bitmap, 16), Some(8));
    }

    #[test]
    fn set_then_clear_bit_round_trips() {
        let mut bitmap = vec![0u8; 2];
        set_bit(&mut bitmap, 5);
        assert!(bit_is_set(&bitmap, 5));
        assert!(!bit_is_set(&bitmap, 4));
        assert!(!bit_is_set(&bitmap, 6));
        clear_bit(&mut bitmap, 5);
        assert!(!bit_is_set(&bitmap, 5));
    }

    #[test]
    fn set_bit_crossing_into_second_byte() {
        let mut bitmap = vec![0u8; 2];
        set_bit(&mut bitmap, 9); // bit 1 of byte 1
        assert_eq!(bitmap[0], 0);
        assert_eq!(bitmap[1], 0b0000_0010);
    }

    #[test]
    fn bit_is_set_out_of_range_is_false_not_panic() {
        let bitmap = [0xFFu8; 2];
        assert!(!bit_is_set(&bitmap, 1000));
    }

    #[test]
    fn single_free_bit_among_otherwise_full_bitmap() {
        let mut bitmap = vec![0xFFu8; 4];
        clear_bit(&mut bitmap, 17);
        assert_eq!(find_first_free_bit(&bitmap, 32), Some(17));
    }
}
