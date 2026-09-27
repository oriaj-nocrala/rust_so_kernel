//! The register seam: BAR0 as 32-bit reads and writes at byte offsets.

/// BAR0 of the GPU. The kernel implements it over an uncached mapping;
/// tests implement it over captured values.
pub trait Mmio {
    fn rd32(&self, offset: u32) -> u32;
    fn wr32(&self, offset: u32, value: u32);
}

#[cfg(test)]
pub mod testing {
    use super::Mmio;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    /// Serves reads from a fixed table (unknown offsets read as
    /// `0xbadf1100`, a value no test expects) and records
    /// every write. A read of an unlisted offset is recorded too, so a test
    /// can assert that a sequence touched only what the trace shows.
    pub struct TableMmio {
        pub regs: Vec<(u32, u32)>,
        pub reads: RefCell<Vec<u32>>,
        pub writes: RefCell<Vec<(u32, u32)>>,
    }

    impl TableMmio {
        pub fn new(regs: &[(u32, u32)]) -> Self {
            TableMmio { regs: regs.to_vec(), reads: RefCell::new(Vec::new()), writes: RefCell::new(Vec::new()) }
        }
    }

    impl Mmio for TableMmio {
        fn rd32(&self, offset: u32) -> u32 {
            self.reads.borrow_mut().push(offset);
            self.regs.iter().find(|(o, _)| *o == offset).map_or(0xbadf_1100, |(_, v)| *v)
        }
        fn wr32(&self, offset: u32, value: u32) {
            self.writes.borrow_mut().push((offset, value));
        }
    }
}
