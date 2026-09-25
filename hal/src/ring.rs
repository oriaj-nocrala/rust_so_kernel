//! `Ring` — a fixed-capacity FIFO of `Copy` values that drops what does
//! not fit.
//!
//! The kernel's input queues (`keyboard_buffer::KEYBOARD_BUFFER`,
//! `RAW_KEY_EVENTS`, `mouse::MOUSE_EVENTS`) used to be lock-free
//! single-producer/single-consumer rings. None of them was: the character
//! queue has three producers (the PS/2 ISR, the COM1 ISR, the USB poll) and
//! any number of reading processes, and on one CPU only IF=0 kept those
//! from overlapping. With SMP a USB report can be decoded on whichever CPU
//! is draining the xHCI event ring while IRQ1 lands on another, and two
//! processes can pop at once. So the ring is now plain data here, with no
//! synchronisation of its own, and the kernel puts each one behind an
//! `IrqMutex` (stage 6 of `docs/smp/smp-plan.md`).
//!
//! Full means drop the *new* value, exactly as the old rings did: a burst
//! typed faster than the reader drains loses its tail, never scrambles
//! what is already queued.

/// FIFO of up to `N` values.
pub struct Ring<T: Copy, const N: usize> {
    buf: [Option<T>; N],
    head: usize,
    len: usize,
}

impl<T: Copy, const N: usize> Ring<T, N> {
    pub const fn new() -> Self {
        Self { buf: [None; N], head: 0, len: 0 }
    }

    /// Append `v`; `false` (and `v` dropped) if the ring is full.
    pub fn push(&mut self, v: T) -> bool {
        if self.len == N {
            return false;
        }
        self.buf[(self.head + self.len) % N] = Some(v);
        self.len += 1;
        true
    }

    /// Remove and return the oldest value.
    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let v = self.buf[self.head].take();
        self.head = (self.head + 1) % N;
        self.len -= 1;
        v
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn len(&self) -> usize {
        self.len
    }
}

impl<T: Copy, const N: usize> Default for Ring<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_order_across_wraparound() {
        let mut r: Ring<u32, 4> = Ring::new();
        let mut next_in = 0;
        let mut next_out = 0;
        // Interleave so head walks around the buffer several times.
        for _ in 0..10 {
            for _ in 0..3 {
                assert!(r.push(next_in));
                next_in += 1;
            }
            for _ in 0..3 {
                assert_eq!(r.pop(), Some(next_out));
                next_out += 1;
            }
        }
        assert!(r.is_empty());
        assert_eq!(r.pop(), None);
    }

    #[test]
    fn full_drops_the_new_value_and_keeps_the_old() {
        let mut r: Ring<char, 3> = Ring::new();
        assert!(r.push('a') && r.push('b') && r.push('c'));
        assert!(!r.push('d'));
        assert_eq!(r.len(), 3);
        assert_eq!(r.pop(), Some('a'));
        assert!(r.push('e'));
        let rest: Vec<char> = core::iter::from_fn(|| r.pop()).collect();
        assert_eq!(rest, ['b', 'c', 'e']);
    }

    #[test]
    fn uses_every_slot() {
        // The old rings kept one slot empty to tell full from empty; this
        // one counts, so capacity N means N.
        let mut r: Ring<u8, 5> = Ring::new();
        for i in 0..5 {
            assert!(r.push(i));
        }
        assert!(!r.push(99));
    }
}
