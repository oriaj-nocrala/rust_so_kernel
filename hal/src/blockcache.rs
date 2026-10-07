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
// the device first and then updates whichever chunks are cached.
//
// The lock is NOT held across a device request. It is a spin lock that
// callers reach with interrupts off (the kernel's `sys_read` holds the
// fd-table lock), and a device request can take long: on the ATA disk of a
// VM every port access is a VM exit, and holding the lock across a 64 KiB
// PIO read left the other CPUs spinning with IF=0 for over a second, deaf
// to TLB shootdowns (VirtualBox, 2026-10-07: a panic within seconds of a
// burst of `exec`s). Instead a miss notes the write generation, drops the
// lock, reads, and caches what it read only if no write finished in the
// meantime (ext2's read paths do not take its own mutation lock, so a read
// can race a write; the reader still gets the device's answer, which is
// one of the two orders). Writes must be serialized by the caller (ext2
// holds its mutation lock across them): two overlapping writes could
// otherwise update the cache in the opposite order to the device.
//
// The lock itself is taken through [`set_lock_hooks`]: the kernel disables
// interrupts while it is held (so its holder is never preempted) and
// answers TLB shootdowns while spinning for it.

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

/// The smallest cache [`default_cache_chunks`] picks: 32 MiB, enough for a 28 MiB game data file.
pub const MIN_CACHE_CHUNKS: usize = 8 * 1024;
/// The largest: 512 MiB. Nothing the system reads repeatedly is bigger than that, and the memory is never given back.
pub const MAX_CACHE_CHUNKS: usize = 128 * 1024;

/// How big the cache of a machine with `ram_bytes` of memory may grow: an eighth of it, between [`MIN_CACHE_CHUNKS`] and
/// [`MAX_CACHE_CHUNKS`]. A fixed 32 MiB held `freedoom1.wad` but not two 16 MB Vulkan programs, so starting the second one read
/// the first one's pages from the USB stick again, and each `exec` of a binary that size took seconds; the cache fills only as
/// files are read, so a large ceiling costs nothing until something needs it.
pub fn default_cache_chunks(ram_bytes: u64) -> usize {
    let chunks = ram_bytes / 8 / CHUNK_BYTES as u64;
    chunks.clamp(MIN_CACHE_CHUNKS as u64, MAX_CACHE_CHUNKS as u64) as usize
}

/// How the cache's lock meets the kernel's interrupt and SMP rules: `irq_save` disables interrupts and returns whether they
/// were on, `irq_restore` puts that back, `relax` runs on every failed attempt to take the lock (the kernel answers TLB
/// shootdowns there; it must not take a lock). Without [`set_lock_hooks`] (host tests) all three do nothing.
#[derive(Clone, Copy)]
pub struct LockHooks {
    pub irq_save: fn() -> bool,
    pub irq_restore: fn(bool),
    pub relax: fn(),
}

static LOCK_HOOKS: spin::Once<LockHooks> = spin::Once::new();

/// Installs the hooks for every cache, once, before the first mount.
pub fn set_lock_hooks(hooks: LockHooks) {
    LOCK_HOOKS.call_once(|| hooks);
}

fn hooks() -> LockHooks {
    LOCK_HOOKS.get().copied().unwrap_or(LockHooks {
        irq_save: || false,
        irq_restore: |_| {},
        relax: core::hint::spin_loop,
    })
}

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
    /// Chunks the cache may hold: [`CachedDevice::set_max_chunks`] moves it.
    max_chunks: usize,
    /// Bumped by every write once it has reached the device: a miss caches
    /// what it read only if this did not move while it was reading.
    write_gen: u64,
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
            state: spin::Mutex::new(State {
                slots: Vec::new(),
                index: BTreeMap::new(),
                segments: Vec::new(),
                hand: 0,
                scratch: Vec::new(),
                max_chunks: max_chunks.max(1),
                write_gen: 0,
            }),
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
        self.locked(|st| st.slots.len())
    }

    /// The most chunks the cache may hold.
    pub fn max_chunks(&self) -> usize {
        self.locked(|st| st.max_chunks)
    }

    /// Changes the ceiling. Growing is free (storage is allocated as the cache fills); shrinking drops the newest slots at once
    /// and gives their storage back. What stays is still valid: slots are independent, and the CLOCK hand wraps on its own.
    pub fn set_max_chunks(&self, max_chunks: usize) {
        self.locked(|st| {
            st.max_chunks = max_chunks.max(1);
            while st.slots.len() > st.max_chunks {
                let last = st.slots.len() - 1;
                Self::remove_slot(st, last);
            }
            let segments = st.slots.len().div_ceil(SEGMENT_CHUNKS);
            st.segments.truncate(segments);
            st.segments.shrink_to_fit();
        })
    }

    /// Whether nobody holds the lock right now.
    #[cfg(test)]
    fn lock_is_free(&self) -> bool {
        self.state.try_lock().is_some()
    }

    /// Every slot is indexed, under its own chunk, exactly once.
    #[cfg(test)]
    fn assert_consistent(&self) {
        self.locked(|st| {
            assert_eq!(st.index.len(), st.slots.len(), "a slot is not indexed (one chunk cached twice?)");
            for (i, slot) in st.slots.iter().enumerate() {
                assert_eq!(st.index.get(&slot.chunk), Some(&i), "slot {} (chunk {}) is not the indexed one", i, slot.chunk);
            }
        })
    }

    /// Runs `f` under the lock, taken through the [`LockHooks`]. Never
    /// around a device request.
    fn locked<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let h = hooks();
        let were_on = (h.irq_save)();
        let r = {
            let mut st = loop {
                if let Some(g) = self.state.try_lock() {
                    break g;
                }
                (h.relax)();
            };
            f(&mut st)
        };
        (h.irq_restore)(were_on);
        r
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

    /// How many chunks a miss at `chunk` reads: it plus the following ones,
    /// stopping at the first cached one (never re-read what we have) or the
    /// end of the capacity. Never more than a quarter of the cache, or a
    /// read-ahead would evict the very chunks being used.
    fn readahead_run(&self, st: &State, chunk: u32) -> u32 {
        let limit = self.chunks_in_capacity();
        let max_run = READAHEAD_CHUNKS.min((st.max_chunks / 4).max(1) as u32);
        let mut run = 1u32;
        while run < max_run && chunk + run < limit && !st.index.contains_key(&(chunk + run)) {
            run += 1;
        }
        run
    }

    /// Caches the `run` chunks a miss at `chunk` read into `data`:
    /// read-ahead chunks first, unreferenced (an unused prefetch is the
    /// first thing CLOCK takes back), the requested chunk last, so no
    /// eviction in this call can pick it. A chunk some other miss cached
    /// while this one was reading is left as it is: two slots for one chunk
    /// would leave one of them unindexed, and a write would update only the
    /// other.
    fn insert_run(&self, st: &mut State, chunk: u32, run: u32, data: &[u8]) {
        for k in (1..run).chain(core::iter::once(0)) {
            let c = chunk + k;
            if st.index.contains_key(&c) {
                continue;
            }
            let bytes = &data[k as usize * CHUNK_BYTES..(k as usize + 1) * CHUNK_BYTES];
            self.store(st, c, k == 0, bytes);
        }
    }

    /// Puts `bytes` in a free slot, or the one CLOCK evicts.
    fn store(&self, st: &mut State, chunk: u32, referenced: bool, bytes: &[u8]) -> usize {
        let i = if st.slots.len() < st.max_chunks {
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
            return self.inner.read_sectors(lba, count, buf);
        }

        let mut sector = lba;
        while (sector as u64) < end {
            let chunk = sector / CHUNK_SECTORS;
            let off = (sector % CHUNK_SECTORS) as usize;
            let take = ((CHUNK_SECTORS as usize - off) as u64).min(end - sector as u64) as usize;
            let dst = (sector - lba) as usize * SECTOR_SIZE;
            let range = off * SECTOR_SIZE..(off + take) * SECTOR_SIZE;

            // A hit is served under the lock; a miss leaves with the plan.
            let miss = self.locked(|st| match st.index.get(&chunk) {
                Some(&i) => {
                    st.slots[i].referenced = true;
                    buf[dst..dst + take * SECTOR_SIZE].copy_from_slice(&st.data(i)[range.clone()]);
                    None
                }
                None => Some((self.readahead_run(st, chunk), st.write_gen, core::mem::take(&mut st.scratch))),
            });
            let Some((run, gen, mut scratch)) = miss else {
                self.hits.fetch_add(1, Ordering::Relaxed);
                sector += take as u32;
                continue;
            };

            // The device request, unlocked. Another miss running at the same
            // time found the scratch buffer taken and reads into its own.
            if scratch.is_empty() {
                scratch = vec![0u8; READAHEAD_CHUNKS as usize * CHUNK_BYTES];
            }
            let read = self.device_read(chunk * CHUNK_SECTORS, run * CHUNK_SECTORS, &mut scratch[..run as usize * CHUNK_BYTES]);
            if read.is_ok() {
                buf[dst..dst + take * SECTOR_SIZE].copy_from_slice(&scratch[range]);
                self.misses.fetch_add(1, Ordering::Relaxed);
            }
            let spare = self.locked(|st| {
                if read.is_ok() && st.write_gen == gen {
                    self.insert_run(st, chunk, run, &scratch);
                }
                if st.scratch.is_empty() {
                    st.scratch = scratch;
                    None
                } else {
                    Some(scratch)
                }
            });
            drop(spare); // freed outside the lock
            read?;
            sector += take as u32;
        }
        Ok(())
    }

    fn write_sectors(&self, lba: u32, count: u8, buf: &[u8]) -> Result<(), &'static str> {
        let n = if count == 0 { 256 } else { count as u32 };
        if buf.len() < n as usize * SECTOR_SIZE {
            return Err("blockcache: buffer too small");
        }
        let result = self.inner.write_sectors(lba, count, buf);
        self.locked(|st| {
            st.write_gen += 1;
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
                        Self::remove_slot(st, i);
                    }
                }
                sector += take as u32;
            }
        });
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
    /// Runs once, inside the next device read, after the disk was read:
    /// what another CPU does while a miss is waiting for the device.
    type Hook = Arc<spin::Mutex<Option<Box<dyn FnMut() + Send>>>>;

    struct Probe {
        disk: Arc<MemDisk>,
        reads: Arc<AtomicU64>,
        fail_writes: bool,
        hook: Hook,
    }

    impl BlockDevice for Probe {
        fn present(&self) -> bool {
            true
        }
        fn read_sectors(&self, lba: u32, count: u8, buf: &mut [u8]) -> Result<(), &'static str> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let r = self.disk.read_sectors(lba, count, buf);
            let hook = self.hook.lock().take();
            if let Some(mut f) = hook {
                f();
            }
            r
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
        let probe = Probe { disk: disk.clone(), reads: reads.clone(), fail_writes, hook: Hook::default() };
        (CachedDevice::new(Box::new(probe), capacity, max_chunks), reads)
    }

    /// A cache whose next device read runs `f(cache)` in the middle.
    fn hooked_cache(disk: &Arc<MemDisk>, f: impl FnOnce(&CachedDevice) + Send + 'static) -> Arc<CachedDevice> {
        let hook = Hook::default();
        let probe = Probe { disk: disk.clone(), reads: Arc::new(AtomicU64::new(0)), fail_writes: false, hook: hook.clone() };
        let c = Arc::new(CachedDevice::new(Box::new(probe), DISK_SECTORS as u32, 64));
        let weak = Arc::downgrade(&c);
        let mut f = Some(f);
        *hook.lock() = Some(Box::new(move || {
            if let (Some(f), Some(c)) = (f.take(), weak.upgrade()) {
                f(&c);
            }
        }));
        c
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

    #[test]
    fn the_default_size_is_an_eighth_of_ram_within_bounds() {
        const MIB: u64 = 1024 * 1024;
        let mib = |chunks: usize| chunks as u64 * CHUNK_BYTES as u64 / MIB;
        assert_eq!(mib(default_cache_chunks(128 * MIB)), 32, "a tiny machine still gets the floor");
        assert_eq!(mib(default_cache_chunks(512 * MIB)), 64);
        assert_eq!(mib(default_cache_chunks(2048 * MIB)), 256);
        assert_eq!(mib(default_cache_chunks(32 * 1024 * MIB)), 512, "a big one stops at the ceiling");
        assert_eq!(default_cache_chunks(0), MIN_CACHE_CHUNKS);
        assert_eq!(default_cache_chunks(u64::MAX), MAX_CACHE_CHUNKS);
    }

    #[test]
    fn growing_the_cache_keeps_what_was_evicted_from_the_small_one() {
        let disk = patterned_disk();
        // 24 slots, read-ahead capped at 6 chunks: reading 40 chunks one at a time cannot keep them all.
        let (c, reads) = cache(&disk, DISK_SECTORS as u32, 24, false);
        for k in 0..40u32 {
            read(&c, k * 8, 1);
        }
        assert!(c.cached_chunks() <= 24);
        c.set_max_chunks(128);
        assert_eq!(c.max_chunks(), 128);
        // The first pass over all 40 chunks fills the new room; the second must not touch the device.
        for k in 0..40u32 {
            assert_eq!(read(&c, k * 8, 8), read(&*disk, k * 8, 8));
        }
        let before = reads.load(Ordering::Relaxed);
        for k in 0..40u32 {
            assert_eq!(read(&c, k * 8, 8), read(&*disk, k * 8, 8));
        }
        assert_eq!(reads.load(Ordering::Relaxed), before, "40 chunks fit in 128 slots: the second pass is all hits");
    }

    #[test]
    fn shrinking_the_cache_drops_slots_but_never_serves_stale_or_wrong_data() {
        let disk = patterned_disk();
        let (c, _) = cache(&disk, DISK_SECTORS as u32, 128, false);
        read(&c, 0, 255);
        read(&c, 255, 255);
        read(&c, 510, 255);
        let full = c.cached_chunks();
        assert!(full >= 90);
        // Write through the cache so a cached copy differs from the pattern, then shrink under it.
        let data = vec![0x5Au8; 2 * SECTOR_SIZE];
        c.write_sectors(100, 2, &data).unwrap();
        c.set_max_chunks(20);
        assert_eq!(c.cached_chunks(), 20, "shrinking is immediate");
        assert_eq!(c.max_chunks(), 20);
        assert_eq!(read(&c, 100, 2), data, "what was written is read back, from cache or device");
        for k in (0..DISK_SECTORS as u32).step_by(8) {
            // The wrapped disk saw the write too (write-through), so it is the reference everywhere.
            assert_eq!(read(&c, k, 8), read(&*disk, k, 8), "chunk at sector {}", k);
            assert!(c.cached_chunks() <= 20, "the cache outgrew its new ceiling");
        }
        // And it still grows back.
        c.set_max_chunks(200);
        read(&c, 0, 255);
        read(&c, 255, 255);
        assert!(c.cached_chunks() > 20);
    }

    #[test]
    fn the_device_is_read_without_the_lock() {
        // Held across the request, the lock kept other CPUs spinning with
        // IF=0 through a whole ATA PIO transfer, deaf to TLB shootdowns.
        let disk = patterned_disk();
        let free = Arc::new(core::sync::atomic::AtomicBool::new(false));
        let seen = free.clone();
        let c = hooked_cache(&disk, move |c| seen.store(c.lock_is_free(), Ordering::Relaxed));
        assert_eq!(read(&*c, 0, 1), read(&*disk, 0, 1));
        assert!(free.load(Ordering::Relaxed), "the lock was held during the device read");
    }

    #[test]
    fn a_read_racing_a_write_does_not_cache_what_the_write_replaced() {
        // The miss reads the old sectors, then a write lands before it
        // caches them: they must not be cached, or every later read
        // returns the old data.
        let disk = patterned_disk();
        let new = vec![0xCDu8; 8 * SECTOR_SIZE];
        let written = new.clone();
        let c = hooked_cache(&disk, move |c| c.write_sectors(0, 8, &written).unwrap());
        let racing = read(&*c, 0, 8);
        assert!(racing == new || racing == read(&*patterned_disk(), 0, 8), "the racing read is one of the two orders");
        assert_eq!(read(&*disk, 0, 8), new);
        assert_eq!(read(&*c, 0, 8), new, "the cache kept the sectors the write replaced");
        c.assert_consistent();
    }

    #[test]
    fn a_chunk_cached_during_a_miss_is_not_cached_twice() {
        // Another miss caches chunk 5 while this one is reading chunks
        // 0..16. A second slot for chunk 5 would be unindexed, a write
        // would update only one of them, and evicting the stray one would
        // unindex the other.
        let disk = patterned_disk();
        let c = hooked_cache(&disk, |c| {
            read(c, 5 * 8, 8);
        });
        read(&*c, 0, 1);
        c.assert_consistent();
        let data = vec![0x77u8; 8 * SECTOR_SIZE];
        c.write_sectors(5 * 8, 8, &data).unwrap();
        for k in 0..32u32 {
            assert_eq!(read(&*c, k * 8, 8), read(&*disk, k * 8, 8), "chunk {}", k);
        }
        c.assert_consistent();
    }
}
