//! The register seam: BAR0 as 32-bit reads and writes at byte offsets.

/// BAR0 of the GPU. The kernel implements it over an uncached mapping;
/// tests implement it over captured values.
pub trait Mmio {
    fn rd32(&self, offset: u32) -> u32;
    fn wr32(&self, offset: u32, value: u32);

    /// Busy-wait `us` microseconds: the time base of every bounded poll
    /// (nouveau's `udelay` next to its register accesses). Tests make it a
    /// no-op; the loops that call it are bounded by iteration count too, so
    /// they end either way.
    fn udelay(&self, us: u32);

    /// `nvkm_mask`: read, clear `mask`, or in `value`, write; returns the
    /// value read. A read and a write even when nothing changes — the
    /// traces show both, and some registers (AUX status) are acknowledged by
    /// the write.
    fn mask(&self, offset: u32, mask: u32, value: u32) -> u32 {
        let old = self.rd32(offset);
        self.wr32(offset, (old & !mask) | value);
        old
    }
}

#[cfg(test)]
pub mod testing {
    use super::Mmio;
    use alloc::collections::BTreeMap;
    use alloc::string::String;
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
        fn udelay(&self, _us: u32) {}
    }

    /// `ReplayMmio` (plan, principle 4): serves each register's reads in the
    /// order a trace extract recorded them — the last one sticks once the
    /// queue runs dry — and records every write, to compare with the
    /// extract's writes. Registers the extract never read answer from
    /// `fallback`, else `0xbadf1100`.
    pub struct ReplayMmio {
        reads: RefCell<BTreeMap<u32, (Vec<u32>, usize)>>,
        pub fallback: Vec<(u32, u32)>,
        /// The extract's writes, in order.
        pub expected_writes: Vec<(u32, u32)>,
        pub writes: RefCell<Vec<(u32, u32)>>,
    }

    impl ReplayMmio {
        /// Parses `R|W offset value` lines (`scripts/gpu-trace.py aux ...`);
        /// `#` starts a comment.
        pub fn from_extract(text: &str) -> Self {
            let mut reads: BTreeMap<u32, (Vec<u32>, usize)> = BTreeMap::new();
            let mut expected_writes = Vec::new();
            for line in text.lines() {
                let line = line.split('#').next().unwrap().trim();
                if line.is_empty() {
                    continue;
                }
                let f: Vec<&str> = line.split_whitespace().collect();
                let num = |s: &str| u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap();
                let (o, v) = (num(f[1]), num(f[2]));
                match f[0] {
                    "R" => reads.entry(o).or_default().0.push(v),
                    "W" => expected_writes.push((o, v)),
                    k => panic!("bad extract line kind {k}"),
                }
            }
            ReplayMmio { reads: RefCell::new(reads), fallback: Vec::new(), expected_writes, writes: RefCell::new(Vec::new()) }
        }

        /// Reads of `offset` still queued (0 = the replay consumed exactly
        /// what the trace read).
        pub fn unread(&self, offset: u32) -> usize {
            self.reads.borrow().get(&offset).map_or(0, |(q, i)| q.len().saturating_sub(*i))
        }

        /// Writes (ours, then the trace's) restricted to `offsets`, as text
        /// lines for a readable assert diff.
        pub fn write_diff(&self, offsets: &[u32]) -> (String, String) {
            let fmt = |w: &[(u32, u32)]| {
                w.iter()
                    .filter(|(o, _)| offsets.contains(o))
                    .map(|(o, v)| alloc::format!("W {o:#06x} {v:#010x}\n"))
                    .collect::<String>()
            };
            (fmt(&self.writes.borrow()), fmt(&self.expected_writes))
        }
    }

    impl Mmio for ReplayMmio {
        fn rd32(&self, offset: u32) -> u32 {
            let mut reads = self.reads.borrow_mut();
            if let Some((q, i)) = reads.get_mut(&offset) {
                let v = q[(*i).min(q.len() - 1)];
                *i += 1;
                return v;
            }
            self.fallback.iter().find(|(o, _)| *o == offset).map_or(0xbadf_1100, |(_, v)| *v)
        }
        fn wr32(&self, offset: u32, value: u32) {
            self.writes.borrow_mut().push((offset, value));
        }
        fn udelay(&self, _us: u32) {}
    }
}
