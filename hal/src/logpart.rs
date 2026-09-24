//! On-disk format of the raw kernel-log partition (`constanos-log`) —
//! pure logic, host-tested.
//!
//! The physical machine this kernel is brought up on has no serial
//! capture, so everything `klog` keeps in RAM dies with the next reboot.
//! This partition is where the kernel copies that ring so the next boot of
//! the machine's own Linux can read it back (`scripts/usb-log.sh read`).
//! No filesystem on purpose: nothing here has metadata a torn write could
//! corrupt, so the worst a crash mid-flush can do is spoil the log itself —
//! never the ext2 data partition, never the boot partition.
//!
//! Layout, in partition-relative 512-byte sectors:
//!
//! ```text
//!   0                       format marker (written by the host, never by the kernel)
//!   STRIDE * 1              slot 0: header sector, then the ring's bytes
//!   STRIDE * 2              slot 1
//!   ...                     up to MAX_SLOTS slots, as many as fit
//! ```
//!
//! **One slot per boot**, reused oldest-first, so booting the kernel twice
//! before reading does not destroy the first boot's log — the one that
//! usually matters, since the second boot was typically just "try again".
//!
//! **The ring is stored raw, not linearised**: sector *k* of a slot's data
//! is always sector *k* of `klog`'s array, and the header records
//! `write_pos` so the reader can rotate it. That makes a flush incremental
//! — only the sectors the ring touched since the last flush are rewritten
//! ([`dirty`]) — which matters because every USB transfer runs with
//! interrupts off.
//!
//! **The kernel refuses a partition without the host's marker in sector
//! 0.** A partition that is merely *named* `constanos-log` is not enough to
//! start writing raw sectors into it: the marker is the host saying "this
//! one is mine to scribble on", checked every boot, independently of the
//! GPT lookup being right.

use alloc::vec::Vec;

use crate::block::SECTOR_SIZE;

/// GPT partition name the kernel looks up (exactly — there is no fallback,
/// unlike the data partition's lookup).
pub const PARTITION_NAME: &str = "constanos-log";

/// Sector 0 of a formatted partition starts with this; the rest of the
/// sector is zero. Written by `scripts/usb-log.sh init`.
pub const MARKER: &[u8] = b"CONSTANOS-KLOG-PARTITION v1\n";

/// Sectors between slot starts (128 KiB). A slot's ring may use all but
/// the header sector.
pub const STRIDE: u64 = 256;

/// Slots used at most, however big the partition is.
pub const MAX_SLOTS: u32 = 16;

/// Largest ring a slot can hold.
pub const MAX_RING_BYTES: usize = (STRIDE as usize - 1) * SECTOR_SIZE;

const HEADER_MAGIC: &[u8; 8] = b"KLOGSLOT";
const VERSION: u32 = 1;
/// Bytes of the header the CRC covers; the CRC itself sits right after.
const HEADER_BODY: usize = 56;

/// Why a flush happened — recorded so the reader can tell a log cut short
/// by a panic from one that simply stopped being flushed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Periodic = 1,
    Sync = 2,
    Panic = 3,
    /// The last flush before `reboot(2)` resets the machine.
    Reboot = 4,
}

impl Reason {
    fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Reason::Periodic),
            2 => Some(Reason::Sync),
            3 => Some(Reason::Panic),
            4 => Some(Reason::Reboot),
            _ => None,
        }
    }
}

/// The header sector at the start of every slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Boot number: one more than the highest found on the partition.
    pub seq: u64,
    /// Size of the ring stored after this header, in bytes.
    pub ring_bytes: u32,
    /// `klog::mark()` at the flush — total bytes ever logged, which
    /// locates the ring's oldest byte.
    pub write_pos: u64,
    pub uptime_ns: u64,
    /// Wall clock at the flush (the RTC's reading plus uptime); 0 if the
    /// RTC never settled.
    pub unix_secs: u64,
    /// Flushes of this slot so far, this boot.
    pub flushes: u32,
    pub reason: Reason,
}

impl Header {
    pub fn encode(&self) -> [u8; SECTOR_SIZE] {
        let mut s = [0u8; SECTOR_SIZE];
        s[0..8].copy_from_slice(HEADER_MAGIC);
        s[8..12].copy_from_slice(&VERSION.to_le_bytes());
        s[12..16].copy_from_slice(&self.ring_bytes.to_le_bytes());
        s[16..24].copy_from_slice(&self.seq.to_le_bytes());
        s[24..32].copy_from_slice(&self.write_pos.to_le_bytes());
        s[32..40].copy_from_slice(&self.uptime_ns.to_le_bytes());
        s[40..48].copy_from_slice(&self.unix_secs.to_le_bytes());
        s[48..52].copy_from_slice(&self.flushes.to_le_bytes());
        s[52] = self.reason as u8;
        let crc = crate::gpt::crc32(&s[..HEADER_BODY]);
        s[HEADER_BODY..HEADER_BODY + 4].copy_from_slice(&crc.to_le_bytes());
        s
    }

    /// `None` for anything that is not a valid header of this version —
    /// a never-written slot, a torn write, garbage. The ring size is
    /// bounded here because it later sizes a read.
    pub fn decode(s: &[u8]) -> Option<Header> {
        if s.len() < SECTOR_SIZE || &s[0..8] != HEADER_MAGIC {
            return None;
        }
        let u32_at = |o: usize| u32::from_le_bytes(s[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(s[o..o + 8].try_into().unwrap());
        if u32_at(HEADER_BODY) != crate::gpt::crc32(&s[..HEADER_BODY]) || u32_at(8) != VERSION {
            return None;
        }
        let ring_bytes = u32_at(12);
        if ring_bytes == 0 || ring_bytes as usize > MAX_RING_BYTES || ring_bytes as usize % SECTOR_SIZE != 0 {
            return None;
        }
        Some(Header {
            seq: u64_at(16),
            ring_bytes,
            write_pos: u64_at(24),
            uptime_ns: u64_at(32),
            unix_secs: u64_at(40),
            flushes: u32_at(48),
            reason: Reason::from_u8(s[52])?,
        })
    }
}

/// The marker sector as the host writes it.
pub fn marker_sector() -> [u8; SECTOR_SIZE] {
    let mut s = [0u8; SECTOR_SIZE];
    s[..MARKER.len()].copy_from_slice(MARKER);
    s
}

/// Whether sector 0 carries the host's marker. The whole sector is
/// checked, zero tail included: a partition that happens to start with the
/// right bytes but holds something else after them is not ours.
pub fn is_formatted(sector0: &[u8]) -> bool {
    sector0.len() >= SECTOR_SIZE && sector0[..SECTOR_SIZE] == marker_sector()[..]
}

/// Slots that fit in a partition of `sectors` sectors (sector 0 and the
/// first stride are the marker's).
pub fn slot_count(sectors: u64) -> u32 {
    ((sectors / STRIDE).saturating_sub(1)).min(MAX_SLOTS as u64) as u32
}

/// Partition-relative LBA of slot `i`'s header.
pub fn slot_lba(i: u32) -> u64 {
    STRIDE * (i as u64 + 1)
}

/// Picks this boot's slot from the headers currently on disk (one entry
/// per slot, `None` for an invalid one): the first never-used slot, else
/// the one holding the oldest boot. Returns `(slot, seq)`, `seq` being one
/// past the newest boot on disk.
pub fn choose_slot(headers: &[Option<Header>]) -> Option<(u32, u64)> {
    if headers.is_empty() {
        return None;
    }
    let seq = headers.iter().flatten().map(|h| h.seq).max().unwrap_or(0) + 1;
    let slot = match headers.iter().position(|h| h.is_none()) {
        Some(free) => free,
        None => headers
            .iter()
            .enumerate()
            .min_by_key(|(_, h)| h.map(|h| h.seq).unwrap_or(0))
            .map(|(i, _)| i)?,
    };
    Some((slot as u32, seq))
}

/// Up to two runs of ring sectors, as `(first_sector, count)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Dirty {
    pub runs: [(u32, u32); 2],
    pub len: usize,
}

impl Dirty {
    pub fn runs(&self) -> &[(u32, u32)] {
        &self.runs[..self.len]
    }

    fn one(first: u32, count: u32) -> Self {
        Dirty { runs: [(first, count), (0, 0)], len: 1 }
    }
}

/// The ring sectors bytes `from..to` (positions in `klog`'s monotonic
/// numbering) landed in. The run starts at the sector holding `from`, not
/// the next whole one, so bytes that were reserved before the previous
/// flush but filled after it — the ring's writers are lock-free — are
/// rewritten here instead of lost.
pub fn dirty(from: u64, to: u64, ring_bytes: usize) -> Dirty {
    let ring = ring_bytes as u64;
    let sector = SECTOR_SIZE as u64;
    let ring_sectors = (ring / sector) as u32;
    if to <= from {
        return Dirty::default();
    }
    if to - from >= ring {
        return Dirty::one(0, ring_sectors);
    }
    let a = from % ring;
    let end = a + (to - from); // may run past the end of the ring
    let first = (a / sector) as u32;
    if end <= ring {
        return Dirty::one(first, (end.div_ceil(sector)) as u32 - first);
    }
    let tail = (end - ring).div_ceil(sector) as u32;
    if tail > first {
        // The wrapped part reaches back into the first run's sector.
        return Dirty::one(0, ring_sectors);
    }
    Dirty { runs: [(first, ring_sectors - first), (0, tail)], len: 2 }
}

/// The ring's content oldest-first, as the reader must reconstruct it:
/// the first `write_pos` bytes if it never wrapped, else rotated so the
/// byte at `write_pos % len` (the oldest) comes first. Mirrored in
/// `scripts/usb-log.sh`; this copy exists so the rule is tested.
pub fn linearize(ring: &[u8], write_pos: u64) -> Vec<u8> {
    let len = ring.len() as u64;
    if write_pos <= len {
        return ring[..write_pos as usize].to_vec();
    }
    let split = (write_pos % len) as usize;
    let mut out = Vec::with_capacity(ring.len());
    out.extend_from_slice(&ring[split..]);
    out.extend_from_slice(&ring[..split]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn header(seq: u64) -> Header {
        Header {
            seq,
            ring_bytes: 64 * 1024,
            write_pos: 123_456,
            uptime_ns: 5_000_000_000,
            unix_secs: 1_790_000_000,
            flushes: 7,
            reason: Reason::Sync,
        }
    }

    #[test]
    fn header_round_trips() {
        let h = header(42);
        assert_eq!(Header::decode(&h.encode()), Some(h));
    }

    #[test]
    fn header_rejects_any_flipped_bit_in_its_body() {
        let good = header(1).encode();
        for byte in 0..HEADER_BODY + 4 {
            let mut s = good;
            s[byte] ^= 0x01;
            assert_eq!(Header::decode(&s), None, "byte {} flipped still decoded", byte);
        }
    }

    #[test]
    fn header_rejects_zeroes_and_bad_ring_sizes() {
        assert_eq!(Header::decode(&[0u8; SECTOR_SIZE]), None);
        for bad in [0u32, 100, (MAX_RING_BYTES + SECTOR_SIZE) as u32] {
            let h = Header { ring_bytes: bad, ..header(1) };
            assert_eq!(Header::decode(&h.encode()), None, "ring_bytes={}", bad);
        }
        let max = Header { ring_bytes: MAX_RING_BYTES as u32, ..header(1) };
        assert_eq!(Header::decode(&max.encode()), Some(max));
    }

    #[test]
    fn marker_must_match_the_whole_sector() {
        assert!(is_formatted(&marker_sector()));
        assert!(!is_formatted(&[0u8; SECTOR_SIZE]));
        let mut s = marker_sector();
        s[SECTOR_SIZE - 1] = 1; // right prefix, something else behind it
        assert!(!is_formatted(&s));
        assert!(!is_formatted(&marker_sector()[..100]));
    }

    #[test]
    fn slot_geometry() {
        // 64 MiB partition: plenty of room, capped.
        assert_eq!(slot_count(64 * 1024 * 2), MAX_SLOTS);
        // Marker stride plus exactly two slots.
        assert_eq!(slot_count(STRIDE * 3), 2);
        assert_eq!(slot_count(STRIDE * 3 - 1), 1);
        // Too small for even one.
        assert_eq!(slot_count(STRIDE), 0);
        assert_eq!(slot_count(0), 0);
        assert_eq!(slot_lba(0), STRIDE);
        // A slot's header plus its largest ring stays inside its stride.
        assert_eq!(slot_lba(0) + 1 + (MAX_RING_BYTES / SECTOR_SIZE) as u64, slot_lba(1));
    }

    #[test]
    fn choose_slot_prefers_unused_then_oldest() {
        assert_eq!(choose_slot(&[]), None);
        assert_eq!(choose_slot(&[None, None, None]), Some((0, 1)));
        assert_eq!(choose_slot(&[Some(header(1)), None, None]), Some((1, 2)));
        // Full: the oldest boot (seq 3, slot 1) is overwritten.
        let full = [Some(header(5)), Some(header(3)), Some(header(4))];
        assert_eq!(choose_slot(&full), Some((1, 6)));
        // A torn slot counts as free, and seq still continues past the max.
        assert_eq!(choose_slot(&[Some(header(9)), None]), Some((1, 10)));
    }

    const RING: usize = 64 * 1024;
    const RS: u32 = (RING / SECTOR_SIZE) as u32;

    #[test]
    fn dirty_nothing_new() {
        assert_eq!(dirty(1000, 1000, RING).runs(), &[]);
        assert_eq!(dirty(1000, 900, RING).runs(), &[]);
    }

    #[test]
    fn dirty_within_one_sector_and_across_sectors() {
        assert_eq!(dirty(0, 1, RING).runs(), &[(0, 1)]);
        assert_eq!(dirty(0, 512, RING).runs(), &[(0, 1)]);
        assert_eq!(dirty(0, 513, RING).runs(), &[(0, 2)]);
        // Starts mid-sector 1: sector 1 is rewritten, not skipped.
        assert_eq!(dirty(700, 1100, RING).runs(), &[(1, 2)]);
    }

    #[test]
    fn dirty_whole_ring_when_it_moved_a_full_lap() {
        assert_eq!(dirty(0, RING as u64, RING).runs(), &[(0, RS)]);
        assert_eq!(dirty(10, 10 + 5 * RING as u64, RING).runs(), &[(0, RS)]);
    }

    #[test]
    fn dirty_wraps_into_two_runs() {
        let from = RING as u64 - 600; // inside the second-to-last sector
        let to = RING as u64 + 100; // into sector 0 of the next lap
        assert_eq!(dirty(from, to, RING).runs(), &[(RS - 2, 2), (0, 1)]);
    }

    #[test]
    fn dirty_wrap_that_reaches_its_own_start_is_the_whole_ring() {
        // Starts at byte 300 of sector 0, wraps around to byte 200 of
        // sector 0 again: both runs would touch sector 0.
        let from = RING as u64 + 300;
        let to = from + RING as u64 - 100;
        assert_eq!(dirty(from, to, RING).runs(), &[(0, RS)]);
    }

    /// The property that matters: flushing the dirty sectors after every
    /// batch of appends keeps the on-disk copy identical to the ring.
    #[test]
    fn incremental_flushes_reproduce_the_ring() {
        const SMALL: usize = 4 * SECTOR_SIZE;
        let mut ring = vec![0u8; SMALL];
        let mut disk = vec![0u8; SMALL];
        let mut pos: u64 = 0;
        let mut flushed: u64 = 0;
        let mut seed: u32 = 12345;
        for round in 0..400 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let n = (seed >> 16) as usize % (SMALL + 700);
            for _ in 0..n {
                ring[(pos % SMALL as u64) as usize] = (pos as u8) ^ (round as u8);
                pos += 1;
            }
            for &(first, count) in dirty(flushed, pos, SMALL).runs() {
                let a = first as usize * SECTOR_SIZE;
                let b = (first + count) as usize * SECTOR_SIZE;
                disk[a..b].copy_from_slice(&ring[a..b]);
            }
            flushed = pos;
            assert_eq!(disk, ring, "round {} (n={})", round, n);
            assert_eq!(linearize(&disk, pos), linearize(&ring, pos));
        }
    }

    #[test]
    fn linearize_rotates_only_once_wrapped() {
        let ring = b"EFGHABCD";
        assert_eq!(linearize(ring, 3), b"EFG");
        assert_eq!(linearize(ring, 8), b"EFGHABCD");
        // 12 bytes written into 8: the oldest surviving byte is at 12 % 8.
        assert_eq!(linearize(ring, 12), b"ABCDEFGH");
    }
}
