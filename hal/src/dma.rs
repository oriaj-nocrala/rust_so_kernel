//! The pure half of `kernel/src/memory/dma.rs`: block sizing and the
//! device's address-mask check (phase 1 of `docs/gpu/gpu-plan.md`).

/// Smallest block the buddy allocator hands out (`mm::buddy::MIN_ORDER`).
pub const PAGE_ORDER: usize = 12;

/// The buddy order (log2 of the block size in bytes) of the smallest block
/// holding `len` bytes. `None` for zero or a length whose block would not
/// fit in a `u64`.
pub fn order_for(len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let order = (usize::BITS - (len - 1).leading_zeros()) as usize;
    if order >= 64 {
        return None;
    }
    Some(order.max(PAGE_ORDER))
}

/// Whether a device whose DMA address mask is `mask` can reach every byte
/// of `phys..phys+len` (a mask of `(1 << 28) - 1` is QEMU `edu`'s default;
/// the GA106's is 47 bits).
pub fn fits(phys: u64, len: u64, mask: u64) -> bool {
    len != 0 && phys.checked_add(len - 1).is_some_and(|end| end <= mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders() {
        assert_eq!(order_for(0), None);
        assert_eq!(order_for(1), Some(12));
        assert_eq!(order_for(4096), Some(12));
        assert_eq!(order_for(4097), Some(13));
        assert_eq!(order_for(1 << 20), Some(20));
        assert_eq!(order_for((1 << 20) + 1), Some(21));
        assert_eq!(order_for(usize::MAX), None);
    }

    #[test]
    fn mask() {
        let m28 = (1u64 << 28) - 1;
        assert!(fits(0, 4096, m28));
        assert!(fits(m28 + 1 - 4096, 4096, m28));
        assert!(!fits(m28 + 1 - 4096, 4097, m28));
        assert!(!fits(m28 + 1, 1, m28));
        assert!(!fits(0, 0, u64::MAX));
        assert!(fits(u64::MAX, 1, u64::MAX));
        assert!(!fits(u64::MAX, 2, u64::MAX));
    }
}
