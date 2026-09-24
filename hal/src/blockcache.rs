// hal/src/blockcache.rs
//
// A write-through read cache with read-ahead, as a `BlockDevice` wrapper.
//
// Why it exists: `ext2` reads one filesystem block (1 KiB, two sectors) per
// device request, re-reads the indirect pointer blocks for every data block
// it maps, and has no cache of its own. On the ATA disk in QEMU that costs
// nothing; on the USB pendrive every request is a whole SCSI command (CBW,
// data, CSW) with real flash latency. Measured in QEMU with a usb-storage
// stick (2026-09-23): starting `doom` issued ~100,000 SCSI commands, all
// but a handful of exactly 1 KiB — `freedoom1.wad` sits almost entirely in
// the doubly-indirect range, so every KiB of data cost three requests.
//
// Chunks of `CHUNK_SECTORS` sectors are cached and evicted by CLOCK (second
// chance: O(1) amortized, where a least-recently-used scan cost a pass over
// every slot on each miss once the cache was full — measurable at
// `opt-level 0`, where the kernel runs). Steady state allocates nothing: a
// miss reads into one persistent scratch buffer and reuses the evicted
// chunk's storage (the kernel allocator logs every allocation this size to
// serial). A
// miss reads the missing chunk plus up to `READAHEAD_CHUNKS - 1` following
// chunks that are not cached yet, in one device request: files on ext2 are
// mostly contiguous, and on USB a 64 KiB transfer costs little more than a
// 1 KiB one.
//
// Coherence is by construction, not by convention: the wrapped device is
// owned here, so every write passes through `write_sectors`, which writes
// the device first and then updates whichever chunks are cached. The lock
// is held across the device request so that a read racing a write cannot
// insert data the write has already superseded (ext2's read paths do not
// take its own mutation lock).

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::block::{BlockDevice, SECTOR_SIZE};

/// Sectors per cached chunk (4 KiB).
pub const CHUNK_SECTORS: u32 = 8;
const CHUNK_BYTES: usize = CHUNK_SECTORS as usize * SECTOR_SIZE;

/// Largest single device request a miss may turn into, in chunks: 64 KiB,
/// which is also one USB mass-storage transfer (`usb::xhci::MAX_SECTORS`).
pub const READAHEAD_CHUNKS: u32 = 16;

/// Counters, read with [`CachedDevice::stats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Chunks served from the cache.
    pub hits: u64,
    /// Chunks that had to come from the device.
    pub misses: u64,
    /// Read requests actually issued to the wrapped device.
    pub device_reads: u64,
    /// Sectors those requests transferred (read-ahead included).
    pub device_sectors: u64,
    /// Reads passed straight through uncached (range reaching past
    /// `capacity_sectors`).
    pub passthrough: u64,
}

/// Chunks per storage segment: slot `i` lives in segment `i / SEGMENT_CHUNKS`.
/// Storage is allocated a segment (64 KiB) at a time as the cache fills,
/// rather than one 4 KiB allocation per chunk.
const SEGMENT_CHUNKS: usize = 16;

struct Slot {
    chunk: u32,
    /// CLOCK's reference bit: set on every hit, cleared as the hand passes.
    referenced: bool,
}

struct State {
    slots: Vec<Slot>,
    index: BTreeMap<u32, usize>,
    /// Chunk storage, indexed like `slots` (see [`SEGMENT_CHUNKS`]).
    segments: Vec<Box<[u8]>>,
    /// CLOCK hand: the next slot considered for eviction.
    hand: usize,
    /// Where a miss's device read lands (`READAHEAD_CHUNKS` chunks),
    /// allocated on the first miss and kept.
    scratch: Vec<u8>,
}

impl State {
    fn data(&self, i: usize) -> &[u8] {
        let off = (i % SEGMENT_CHUNKS) * CHUNK_BYTES;
        &self.segments[i / SEGMENT_CHUNKS][off..off + CHUNK_BYTES]
    }

    fn data_mut(&mut self, i: usize) -> &mut [u8] {
        let off = (i % SEGMENT_CHUNKS) * CHUNK_BYTES;
        &mut self.segments[i / SEGMENT_CHUNKS][off..off + CHUNK_BYTES]
    }
}

pub struct CachedDevice {
    inner: Box<dyn BlockDevice>,
    /// Sectors the cache may read, read-ahead included: `0..capacity`.
    /// Anything reaching beyond goes straight to the device, uncached.
    capacity_sectors: u32,
    max_chunks: usize,
    state: spin::Mutex<State>,
    hits: AtomicU64,
    misses: AtomicU64,
    device_reads: AtomicU64,
    device_sectors: AtomicU64,
    passthrough: AtomicU64,
}

impl CachedDevice {
    /// Wraps `inner`. `capacity_sectors` bounds read-ahead (the device, or
    /// the filesystem on it, ends there); `max_chunks` bounds memory, at
    /// `CHUNK_SECTORS * 512` bytes each.
    pub fn new(inner: Box<dyn BlockDevice>, capacity_sectors: u32, max_chunks: usize) -> Self {
        CachedDevice {
            inner,
            capacity_sectors,
            max_chunks: max_chunks.max(1),
            state: spin::Mutex::new(State { slots: Vec::new(), index: BTreeMap::new(), segments: Vec::new(), hand: 0, scratch: Vec::new() }),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            device_reads: AtomicU64::new(0),
            device_sectors: AtomicU64::new(0),
            passthrough: AtomicU64::new(0),
        }
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            device_reads: self.device_reads.load(Ordering::Relaxed),
            device_sectors: self.device_sectors.load(Ordering::Relaxed),
            passthrough: self.passthrough.load(Ordering::Relaxed),
        }
    }

    /// Chunks cached right now.
    pub fn cached_chunks(&self) -> usize {
        self.state.lock().slots.len()
    }

    fn chunks_in_capacity(&self) -> u32 {
        self.capacity_sectors / CHUNK_SECTORS
    }

    fn device_read(&self, lba: u32, sectors: u32, buf: &mut [u8]) -> Result<(), &'static str> {
        self.device_reads.fetch_add(1, Ordering::Relaxed);
        self.device_sectors.fetch_add(sectors as u64, Ordering::Relaxed);
        // `sectors` never exceeds READAHEAD_CHUNKS * CHUNK_SECTORS = 128.
        self.inner.read_sectors(lba, sectors as u8, buf)
    }

    /// Makes sure `chunk` is cached, reading it plus read-ahead on a miss,
    /// and returns its slot.
    fn fill(&self, st: &mut State, chunk: u32) -> Result<usize, &'static str> {
        if let Some(&i) = st.index.get(&chunk) {
            st.slots[i].referenced = true;
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(i);
        }

        // Read-ahead: following chunks, stopping at the first cached one
        // (never re-read what we have) or the end of the capacity. Never
        // prefetch more than a quarter of the cache, or a read-ahead would
        // evict the very chunks being used.
        let limit = self.chunks_in_capacity();
        let max_run = READAHEAD_CHUNKS.min((self.max_chunks / 4).max(1) as u32);
        let mut run = 1u32;
        while run < max_run
            && chunk + run < limit
            && !st.index.contains_key(&(chunk + run))
        {
            run += 1;
        }
        if st.scratch.is_empty() {
            st.scratch = vec![0u8; READAHEAD_CHUNKS as usize * CHUNK_BYTES];
        }
        let mut scratch = core::mem::take(&mut st.scratch);
        let read = self.device_read(chunk * CHUNK_SECTORS, run * CHUNK_SECTORS, &mut scratch[..run as usize * CHUNK_BYTES]);
        if let Err(e) = read {
            st.scratch = scratch;
            return Err(e);
        }
        self.misses.fetch_add(1, Ordering::Relaxed);

        // Read-ahead chunks first, unreferenced (an unused prefetch is the
        // first thing CLOCK takes back); the requested chunk last, so no
        // eviction in this call can pick it.
        let mut first = 0;
        for k in (1..run).chain(core::iter::once(0)) {
            let bytes = &scratch[k as usize * CHUNK_BYTES..(k as usize + 1) * CHUNK_BYTES];
            first = self.store(st, chunk + k, k == 0, bytes);
        }
        st.scratch = scratch;
        Ok(first)
    }

    /// Puts `bytes` in a free slot, or the one CLOCK evicts.
    fn store(&self, st: &mut State, chunk: u32, referenced: bool, bytes: &[u8]) -> usize {
        let i = if st.slots.len() < self.max_chunks {
            let i = st.slots.len();
            if i / SEGMENT_CHUNKS == st.segments.len() {
                st.segments.push(vec![0u8; SEGMENT_CHUNKS * CHUNK_BYTES].into_boxed_slice());
            }
            st.slots.push(Slot { chunk, referenced });
            i
        } else {
            let i = Self::clock_victim(st);
            let old = st.slots[i].chunk;
            st.index.remove(&old);
            st.slots[i] = Slot { chunk, referenced };
            i
        };
        st.data_mut(i).copy_from_slice(bytes);
        st.index.insert(chunk, i);
        i
    }

    /// Second chance: advance the hand, clearing reference bits, until an
    /// unreferenced slot turns up. Terminates within two sweeps.
    fn clock_victim(st: &mut State) -> usize {
        loop {
            if st.hand >= st.slots.len() {
                st.hand = 0;
            }
            let i = st.hand;
            st.hand += 1;
            if st.slots[i].referenced {
                st.slots[i].referenced = false;
            } else {
                return i;
            }
        }
    }

    fn remove_slot(st: &mut State, i: usize) {
        let chunk = st.slots[i].chunk;
        st.index.remove(&chunk);
        let last = st.slots.len() - 1;
        if i != last {
            // Move the last slot, storage included, into the hole.
            let mut tmp = [0u8; CHUNK_BYTES];
            tmp.copy_from_slice(st.data(last));
            st.data_mut(i).copy_from_slice(&tmp);
        }
        st.slots.swap_remove(i);
        if i < st.slots.len() {
            let moved = st.slots[i].chunk;
            st.index.insert(moved, i);
        }
    }
}

impl BlockDevice for CachedDevice {
    fn present(&self) -> bool {
        self.inner.present()
    }

    fn read_sectors(&self, lba: u32, count: u8, buf: &mut [u8]) -> Result<(), &'static str> {
        let n = if count == 0 { 256 } else { count as u32 };
        if buf.len() < n as usize * SECTOR_SIZE {
            return Err("blockcache: buffer too small");
        }
        let end = lba as u64 + n as u64;
        let cached_end = self.chunks_in_capacity() as u64 * CHUNK_SECTORS as u64;
        if end > cached_end {
            self.passthrough.fetch_add(1, Ordering::Relaxed);
            let _st = self.state.lock(); // order against concurrent writes
            return self.inner.read_sectors(lba, count, buf);
        }

        let mut st = self.state.lock();
        let mut sector = lba;
        while (sector as u64) < end {
            let chunk = sector / CHUNK_SECTORS;
            let off = (sector % CHUNK_SECTORS) as usize;
            let take = ((CHUNK_SECTORS as usize - off) as u64).min(end - sector as u64) as usize;
            let i = self.fill(&mut st, chunk)?;
            let dst = (sector - lba) as usize * SECTOR_SIZE;
            buf[dst..dst + take * SECTOR_SIZE]
                .copy_from_slice(&st.data(i)[off * SECTOR_SIZE..(off + take) * SECTOR_SIZE]);
            sector += take as u32;
        }
        Ok(())
    }

    fn write_sectors(&self, lba: u32, count: u8, buf: &[u8]) -> Result<(), &'static str> {
        let n = if count == 0 { 256 } else { count as u32 };
        if buf.len() < n as usize * SECTOR_SIZE {
            return Err("blockcache: buffer too small");
        }
        let mut st = self.state.lock();
        let result = self.inner.write_sectors(lba, count, buf);
        let end = lba as u64 + n as u64;
        let mut sector = lba;
        while (sector as u64) < end {
            let chunk = sector / CHUNK_SECTORS;
            let off = (sector % CHUNK_SECTORS) as usize;
            let take = ((CHUNK_SECTORS as usize - off) as u64).min(end - sector as u64) as usize;
            if let Some(&i) = st.index.get(&chunk) {
                if result.is_ok() {
                    let src = (sector - lba) as usize * SECTOR_SIZE;
                    st.data_mut(i)[off * SECTOR_SIZE..(off + take) * SECTOR_SIZE]
                        .copy_from_slice(&buf[src..src + take * SECTOR_SIZE]);
                } else {
                    // What reached the device is unknown: forget the chunk
                    // so the next read asks the device.
                    Self::remove_slot(&mut st, i);
                }
            }
            sector += take as u32;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::MemDisk;
    use alloc::sync::Arc;

    /// Shares one `MemDisk` between the cache and the test, and counts
    /// the requests the cache actually makes.
    struct Probe {
        disk: Arc<MemDisk>,
        reads: Arc<AtomicU64>,
        fail_writes: bool,
    }

    impl BlockDevice for Probe {
        fn present(&self) -> bool {
            true
        }
        fn read_sectors(&self, lba: u32, count: u8, buf: &mut [u8]) -> Result<(), &'static str> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.disk.read_sectors(lba, count, buf)
        }
        fn write_sectors(&self, lba: u32, count: u8, buf: &[u8]) -> Result<(), &'static str> {
            if self.fail_writes {
                return Err("probe: write failed");
            }
            self.disk.write_sectors(lba, count, buf)
        }
    }

    const DISK_SECTORS: usize = 1024;

    fn patterned_disk() -> Arc<MemDisk> {
        let mut v = vec![0u8; DISK_SECTORS * SECTOR_SIZE];
        for (i, b) in v.iter_mut().enumerate() {
            *b = (i / SECTOR_SIZE) as u8 ^ (i as u8).wrapping_mul(31);
        }
        Arc::new(MemDisk::from_vec(v))
    }

    fn cache(disk: &Arc<MemDisk>, capacity: u32, max_chunks: usize, fail_writes: bool) -> (CachedDevice, Arc<AtomicU64>) {
        let reads = Arc::new(AtomicU64::new(0));
        let probe = Probe { disk: disk.clone(), reads: reads.clone(), fail_writes };
        (CachedDevice::new(Box::new(probe), capacity, max_chunks), reads)
    }

    fn read(dev: &dyn BlockDevice, lba: u32, count: u8) -> Vec<u8> {
        let mut b = vec![0u8; count as usize * SECTOR_SIZE];
        dev.read_sectors(lba, count, &mut b).unwrap();
        b
    }

    #[test]
    fn reads_match_the_device_at_any_alignment() {
        let disk = patterned_disk();
        let (c, _) = cache(&disk, DISK_SECTORS as u32, 64, false);
        for &(lba, count) in &[(0u32, 1u8), (3, 2), (7, 2), (8, 8), (5, 30), (100, 255), (1020, 4)] {
            assert_eq!(read(&c, lba, count), read(&*disk, lba, count), "lba {} count {}", lba, count);
        }
    }

    #[test]
    fn repeated_reads_hit_the_cache() {
        let disk = patterned_disk();
        let (c, reads) = cache(&disk, DISK_SECTORS as u32, 64, false);
        read(&c, 10, 2);
        let after_first = reads.load(Ordering::Relaxed);
        for _ in 0..100 {
            read(&c, 10, 2);
        }
        assert_eq!(reads.load(Ordering::Relaxed), after_first);
        assert_eq!(c.stats().hits, 100);
    }

    #[test]
    fn sequential_reads_are_batched_by_read_ahead() {
        let disk = patterned_disk();
        let (c, reads) = cache(&disk, DISK_SECTORS as u32, 256, false);
        // 256 KiB in ext2-sized 1 KiB requests: 256 requests without the
        // cache, 4 with 64 KiB read-ahead.
        for k in 0..256u32 {
            assert_eq!(read(&c, 2 * k, 2), read(&*disk, 2 * k, 2));
        }
        assert_eq!(reads.load(Ordering::Relaxed), 512 / (READAHEAD_CHUNKS * CHUNK_SECTORS) as u64);
    }

    #[test]
    fn read_ahead_stops_at_capacity_and_at_cached_chunks() {
        let disk = patterned_disk();
        // Capacity 40 sectors = 5 chunks: a read at chunk 3 may prefetch
        // only chunk 4.
        let (c, _) = cache(&disk, 40, 64, false);
        read(&c, 24, 1);
        assert_eq!(c.stats().device_sectors, 16);
        // Chunk 1 cached, then a miss at 0 must read chunk 0 alone.
        let (c, _) = cache(&disk, DISK_SECTORS as u32, 64, false);
        read(&c, 8, 1); // chunks 1..=16
        read(&c, 0, 1);
        assert_eq!(c.stats().device_sectors, 16 * 8 + 8);
    }

    #[test]
    fn beyond_capacity_passes_through_uncached() {
        let disk = patterned_disk();
        let (c, reads) = cache(&disk, 44, 64, false); // last whole chunk ends at 40
        assert_eq!(read(&c, 38, 4), read(&*disk, 38, 4));
        assert_eq!(read(&c, 38, 4), read(&*disk, 38, 4));
        assert_eq!(reads.load(Ordering::Relaxed), 2);
        assert_eq!(c.stats().passthrough, 2);
    }

    #[test]
    fn writes_reach_the_device_and_the_cached_copy() {
        let disk = patterned_disk();
        let (c, _) = cache(&disk, DISK_SECTORS as u32, 64, false);
        read(&c, 0, 64); // cache chunks 0..8
        let data = vec![0xABu8; 3 * SECTOR_SIZE];
        c.write_sectors(6, 3, &data).unwrap(); // straddles chunks 0 and 1
        assert_eq!(read(&*disk, 6, 3), data);
        assert_eq!(read(&c, 6, 3), data);
        assert_eq!(read(&c, 0, 64), read(&*disk, 0, 64));
    }

    #[test]
    fn failed_write_drops_the_cached_chunk() {
        let disk = patterned_disk();
        let (c, reads) = cache(&disk, DISK_SECTORS as u32, 64, true);
        read(&c, 0, 1);
        let before = reads.load(Ordering::Relaxed);
        assert!(c.write_sectors(0, 1, &[0u8; SECTOR_SIZE]).is_err());
        read(&c, 0, 1);
        assert_eq!(reads.load(Ordering::Relaxed), before + 1);

        // Dropping a slot from the middle moves the last one into its
        // place, storage and all: everything must still read correctly.
        read(&c, 64, 64); // more chunks, so the dropped one is not last
        assert!(c.write_sectors(3 * 8, 1, &[0u8; SECTOR_SIZE]).is_err());
        assert_eq!(read(&c, 0, 128), read(&*disk, 0, 128));
        assert_eq!(read(&c, 0, 128), read(&*disk, 0, 128));
    }

    #[test]
    fn eviction_keeps_memory_bounded_and_data_correct() {
        let disk = patterned_disk();
        let (c, _) = cache(&disk, DISK_SECTORS as u32, 20, false);
        for k in (0..DISK_SECTORS as u32).step_by(8).rev() {
            assert_eq!(read(&c, k, 1), read(&*disk, k, 1));
            assert!(c.cached_chunks() <= 20);
        }
        for k in 0..DISK_SECTORS as u32 / 2 {
            assert_eq!(read(&c, k * 2, 2), read(&*disk, k * 2, 2));
        }
    }

    #[test]
    fn clock_evicts_unreferenced_chunks_and_spares_referenced_ones() {
        let disk = patterned_disk();
        // 16 slots, read-ahead capped at 16 / 4 = 4 chunks.
        let (c, reads) = cache(&disk, DISK_SECTORS as u32, 16, false);
        for chunk in [0u32, 4, 8, 12] {
            read(&c, chunk * 8, 1); // requested chunk + 3 prefetched
        }
        assert_eq!(c.cached_chunks(), 16);
        read(&c, 8, 1); // hit: gives prefetched chunk 1 its reference bit
        read(&c, 16 * 8, 1); // full: four evictions

        let before = reads.load(Ordering::Relaxed);
        for hot in [0u32, 1, 4, 8, 12] {
            read(&c, hot * 8, 1);
        }
        assert_eq!(reads.load(Ordering::Relaxed), before, "a referenced chunk was evicted");
        read(&c, 2 * 8, 1); // never referenced: evicted
        assert_eq!(reads.load(Ordering::Relaxed), before + 1);
        assert_eq!(c.cached_chunks(), 16);
    }
}
