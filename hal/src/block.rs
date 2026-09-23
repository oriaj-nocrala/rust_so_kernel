// hal/src/block.rs
//
// Block device seam — lets a filesystem (ext2 today) read/write fixed-size
// sectors without knowing whether they live on real ATA hardware or in a
// plain in-memory buffer.
//
// Lives in `hal`, not `kernel`, for the same reason `PortIo`/`PhysMem` do:
// `hal` has zero bare-metal dependencies, so anything built against this
// trait is host-testable with a plain `cargo test`. Two things benefit from
// that today:
//   - `MemDisk` below, exercised by this file's own host tests.
//   - `kernel::fs::ext2`'s QEMU integration test (`kernel/src/hw_tests.rs`),
//     which mounts a hand-built minimal ext2 image on a `MemDisk` instead of
//     touching the real ATA disk / `disk.img` — see that file and
//     `fs::ext2::build_minimal_image` for how.
// `fs::ext2` itself stays inside the `kernel` crate (it depends on the
// `Inode`/`FileHandle` traits that live there, and extracting it is real,
// separate work — see `docs/drivers/architecture.md`'s storage-stack
// section for why that's future work, not part of this seam), so its own
// ~2000 lines of bitmap/inode/directory logic aren't host-testable yet.
// This trait is still worth having in `hal` now: it's the same seam shape
// as `PortIo`/`PhysMem`, and it's what a future ext2-logic extraction would
// already need in place.
//
// The production implementation (`AtaBlockDevice`, wrapping
// `kernel::block::ata`'s real port I/O) lives in `kernel/src/block/mod.rs` —
// same split as `hal::PortIo` / `kernel::hal::X86PortIo`.
//
// ## Why sectors, not filesystem blocks
//
// The trait speaks in fixed 512-byte sectors and LBA28-style addressing —
// deliberately mirroring `block::ata`'s existing free-function API
// (`read_sectors(lba, count, buf)`) rather than a filesystem's own block
// size (1024/2048/4096 — ext2's `s_log_block_size`). Two reasons:
//
// 1. **Real hardware works this way.** A real block layer sits beneath the
//    filesystem at a fixed sector granularity; a filesystem's own block
//    size is a software convention layered on top of it (`fs::ext2::Ext2Fs::
//    read_block` already does exactly this translation:
//    `sectors_per_block = block_size / SECTOR_SIZE`). Baking a particular
//    filesystem's block size into the device seam would couple the seam to
//    that filesystem's convention and get it wrong for the next one (a
//    hypothetical FAT or a differently-configured ext2 image with a 4096-
//    byte block size).
// 2. **Minimal-diff migration.** `fs::ext2.rs` is a large (~2000-line),
//    invariant-heavy file (coarse-lock discipline, e2fsck-mirroring orphan
//    reclaim whose *ordering* is load-bearing — see its module doc comment).
//    Keeping the trait's shape identical to the free functions it replaces
//    (`ata::read_sectors`/`write_sectors`/`present`) makes the migration a
//    mechanical `crate::block::ata::X(...)` -> `self.device.X(...)` rename
//    at each call site instead of a structural rewrite that would risk
//    those carefully-documented invariants.

use alloc::vec::Vec;

/// Sector size every `BlockDevice` implementation speaks in (matches the
/// real legacy ATA/SCSI sector size). Not configurable per-device — both
/// `AtaBlockDevice` (real LBA28 PIO) and `MemDisk` below hardcode it, same
/// as `block::ata::SECTOR_SIZE` does today for the ATA driver itself.
pub const SECTOR_SIZE: usize = 512;

/// A block-addressable storage device, sector-granular (see module doc for
/// why sectors and not filesystem blocks).
///
/// `Send + Sync`: every filesystem built against this trait (`fs::ext2::
/// Ext2Fs`) is reachable from any process's syscall context through a
/// `spin::Once` global, so the `Box<dyn BlockDevice>` it owns has to be
/// safely shareable across whichever CPU/interrupt context calls in.
pub trait BlockDevice: Send + Sync {
    /// True if the device is actually there and answering.
    /// `AtaBlockDevice` probes real hardware (status register not stuck at
    /// the "nothing here" 0xFF); `MemDisk` is always "present" once
    /// constructed and just returns `true` unconditionally.
    fn present(&self) -> bool;

    /// Read `count` sectors (`SECTOR_SIZE` bytes each) starting at `lba`
    /// into `buf`. `buf.len()` must be at least `count as usize *
    /// SECTOR_SIZE`. `count == 0` means 256 sectors — the same LBA28
    /// convention `block::ata::read_sectors` documents (unused by
    /// `fs::ext2` today, same as there, but part of the contract every
    /// implementation honors).
    fn read_sectors(&self, lba: u32, count: u8, buf: &mut [u8]) -> Result<(), &'static str>;

    /// Write `count` sectors from `buf` starting at `lba`. Same
    /// size/`count == 0` contract as `read_sectors`.
    fn write_sectors(&self, lba: u32, count: u8, buf: &[u8]) -> Result<(), &'static str>;
}

/// A `Vec<u8>`-backed `BlockDevice` — an in-RAM disk.
///
/// Used by:
/// - This file's own host tests (fast, no QEMU).
/// - `kernel/src/hw_tests.rs`'s ext2 QEMU integration test, which mounts a
///   hand-built minimal ext2 image on one instead of touching the real ATA
///   disk / `disk.img`.
pub struct MemDisk {
    data: spin::Mutex<Vec<u8>>,
}

impl MemDisk {
    /// A zero-filled disk of exactly `sectors` sectors.
    pub fn new(sectors: usize) -> Self {
        MemDisk { data: spin::Mutex::new(alloc::vec![0u8; sectors * SECTOR_SIZE]) }
    }

    /// Wrap an existing byte buffer (e.g. a hand-built filesystem image) as
    /// a disk. `data.len()` must be a whole multiple of `SECTOR_SIZE` —
    /// debug-asserted, not enforced in release, since every caller in this
    /// codebase builds `data` from whole-sector/whole-block writes anyway.
    pub fn from_vec(data: Vec<u8>) -> Self {
        debug_assert!(
            data.len() % SECTOR_SIZE == 0,
            "MemDisk::from_vec: length {} is not a whole number of {}-byte sectors",
            data.len(), SECTOR_SIZE
        );
        MemDisk { data: spin::Mutex::new(data) }
    }

    /// Total sector count backing this disk.
    pub fn sector_count(&self) -> usize {
        self.data.lock().len() / SECTOR_SIZE
    }

    /// Snapshot the current contents out — lets a test inspect or re-mount
    /// the resulting image after a sequence of writes.
    pub fn snapshot(&self) -> Vec<u8> {
        self.data.lock().clone()
    }
}

impl BlockDevice for MemDisk {
    fn present(&self) -> bool {
        true
    }

    fn read_sectors(&self, lba: u32, count: u8, buf: &mut [u8]) -> Result<(), &'static str> {
        let n = if count == 0 { 256 } else { count as usize };
        if buf.len() < n * SECTOR_SIZE {
            return Err("MemDisk::read_sectors: buf too small");
        }
        let data = self.data.lock();
        let start = lba as usize * SECTOR_SIZE;
        let end = start + n * SECTOR_SIZE;
        if end > data.len() {
            return Err("MemDisk::read_sectors: read past end of disk");
        }
        buf[..n * SECTOR_SIZE].copy_from_slice(&data[start..end]);
        Ok(())
    }

    fn write_sectors(&self, lba: u32, count: u8, buf: &[u8]) -> Result<(), &'static str> {
        let n = if count == 0 { 256 } else { count as usize };
        if buf.len() < n * SECTOR_SIZE {
            return Err("MemDisk::write_sectors: buf too small");
        }
        let mut data = self.data.lock();
        let start = lba as usize * SECTOR_SIZE;
        let end = start + n * SECTOR_SIZE;
        if end > data.len() {
            return Err("MemDisk::write_sectors: write past end of disk");
        }
        data[start..end].copy_from_slice(&buf[..n * SECTOR_SIZE]);
        Ok(())
    }
}

/// A window onto part of another `BlockDevice` — one partition of a disk.
///
/// Adds the partition's first LBA to every request and **refuses** any
/// request that would leave the window, rather than clamping it: a sector
/// past the end of the ext2 partition is some other partition's data (on
/// the boot stick, nothing — but on another disk, anything), and a
/// filesystem bug that asks for it must surface as an I/O error, never as
/// a read of the neighbour or, worse, a write to it.
///
/// `read_only` refuses writes at the device level too. `fs::ext2`'s own
/// read-only mount already never issues one; this is the second guard,
/// independent of that code being right.
pub struct Partition {
    inner: alloc::boxed::Box<dyn BlockDevice>,
    first_lba: u32,
    sectors: u32,
    read_only: bool,
}

impl Partition {
    /// `None` if the window does not fit in 32-bit LBAs.
    pub fn new(inner: alloc::boxed::Box<dyn BlockDevice>, first_lba: u32, sectors: u32, read_only: bool) -> Option<Self> {
        first_lba.checked_add(sectors)?;
        Some(Partition { inner, first_lba, sectors, read_only })
    }

    pub fn sectors(&self) -> u32 {
        self.sectors
    }

    /// The absolute LBA for a request of `count` sectors at `lba`, or an
    /// error if any of it falls outside the partition.
    fn translate(&self, lba: u32, count: u8) -> Result<u32, &'static str> {
        let n = if count == 0 { 256 } else { count as u32 };
        match lba.checked_add(n) {
            Some(end) if end <= self.sectors => Ok(self.first_lba + lba),
            _ => Err("partition: request outside the partition"),
        }
    }
}

impl BlockDevice for Partition {
    fn present(&self) -> bool {
        self.inner.present()
    }

    fn read_sectors(&self, lba: u32, count: u8, buf: &mut [u8]) -> Result<(), &'static str> {
        let abs = self.translate(lba, count)?;
        self.inner.read_sectors(abs, count, buf)
    }

    fn write_sectors(&self, lba: u32, count: u8, buf: &[u8]) -> Result<(), &'static str> {
        if self.read_only {
            return Err("partition: read-only");
        }
        let abs = self.translate(lba, count)?;
        self.inner.write_sectors(abs, count, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn present_is_always_true() {
        let disk = MemDisk::new(4);
        assert!(disk.present());
    }

    #[test]
    fn write_then_read_round_trips() {
        let disk = MemDisk::new(4); // 4 sectors = 2048 bytes
        let mut pattern = alloc::vec![0u8; SECTOR_SIZE * 2];
        for (i, b) in pattern.iter_mut().enumerate() {
            *b = (i % 251) as u8; // arbitrary non-trivial byte pattern
        }
        disk.write_sectors(1, 2, &pattern).expect("write_sectors");

        let mut readback = alloc::vec![0u8; SECTOR_SIZE * 2];
        disk.read_sectors(1, 2, &mut readback).expect("read_sectors");
        assert_eq!(readback, pattern);

        // Sector 0 and sector 3 (untouched) must still be zero.
        let mut untouched = alloc::vec![0u8; SECTOR_SIZE];
        disk.read_sectors(0, 1, &mut untouched).expect("read_sectors");
        assert!(untouched.iter().all(|&b| b == 0));
    }

    #[test]
    fn from_vec_preserves_content() {
        let mut img = alloc::vec![0u8; SECTOR_SIZE * 3];
        img[SECTOR_SIZE] = 0xAB; // one marker byte in the middle sector
        let disk = MemDisk::from_vec(img);
        assert_eq!(disk.sector_count(), 3);

        let mut buf = alloc::vec![0u8; SECTOR_SIZE];
        disk.read_sectors(1, 1, &mut buf).unwrap();
        assert_eq!(buf[0], 0xAB);
    }

    #[test]
    fn read_past_end_of_disk_errors() {
        let disk = MemDisk::new(2); // 2 sectors
        let mut buf = alloc::vec![0u8; SECTOR_SIZE * 3];
        assert!(disk.read_sectors(0, 3, &mut buf).is_err());
        // Off-the-end single sector too.
        let mut buf2 = alloc::vec![0u8; SECTOR_SIZE];
        assert!(disk.read_sectors(2, 1, &mut buf2).is_err());
    }

    #[test]
    fn write_past_end_of_disk_errors() {
        let disk = MemDisk::new(2);
        let buf = alloc::vec![0xFFu8; SECTOR_SIZE * 3];
        assert!(disk.write_sectors(0, 3, &buf).is_err());
    }

    #[test]
    fn buf_too_small_errors_both_directions() {
        let disk = MemDisk::new(4);
        let mut short = alloc::vec![0u8; SECTOR_SIZE - 1];
        assert!(disk.read_sectors(0, 1, &mut short).is_err());
        let short_ro = alloc::vec![0u8; SECTOR_SIZE - 1];
        assert!(disk.write_sectors(0, 1, &short_ro).is_err());
    }

    #[test]
    fn snapshot_reflects_writes() {
        let disk = MemDisk::new(2);
        let pattern = alloc::vec![0x42u8; SECTOR_SIZE];
        disk.write_sectors(0, 1, &pattern).unwrap();
        let snap = disk.snapshot();
        assert_eq!(&snap[..SECTOR_SIZE], &pattern[..]);
        assert!(snap[SECTOR_SIZE..].iter().all(|&b| b == 0));
    }

    // ── Partition ───────────────────────────────────────────────────────

    /// A 16-sector disk whose every sector is filled with its own number.
    fn numbered_disk() -> alloc::boxed::Box<MemDisk> {
        let mut img = alloc::vec![0u8; 16 * SECTOR_SIZE];
        for (i, sector) in img.chunks_mut(SECTOR_SIZE).enumerate() {
            sector.fill(i as u8);
        }
        alloc::boxed::Box::new(MemDisk::from_vec(img))
    }

    #[test]
    fn partition_offsets_reads() {
        let p = Partition::new(numbered_disk(), 4, 8, true).unwrap();
        let mut buf = alloc::vec![0u8; 2 * SECTOR_SIZE];
        p.read_sectors(0, 2, &mut buf).unwrap();
        assert!(buf[..SECTOR_SIZE].iter().all(|&b| b == 4));
        assert!(buf[SECTOR_SIZE..].iter().all(|&b| b == 5));
        p.read_sectors(6, 2, &mut buf).unwrap(); // last two sectors: 10, 11
        assert!(buf[SECTOR_SIZE..].iter().all(|&b| b == 11));
    }

    /// One sector past the end is the neighbour's data — must be refused,
    /// not clamped, including via `count == 0` (= 256) and via LBA
    /// overflow.
    #[test]
    fn partition_refuses_to_leave_its_window() {
        let p = Partition::new(numbered_disk(), 4, 8, false).unwrap();
        let mut buf = alloc::vec![0u8; 256 * SECTOR_SIZE];
        assert!(p.read_sectors(7, 2, &mut buf).is_err());
        assert!(p.read_sectors(8, 1, &mut buf).is_err());
        assert!(p.read_sectors(0, 0, &mut buf).is_err());
        assert!(p.read_sectors(u32::MAX, 1, &mut buf).is_err());
        assert!(p.write_sectors(7, 2, &buf).is_err());
    }

    #[test]
    fn partition_write_lands_in_the_window_or_is_refused_when_read_only() {
        let ro = Partition::new(numbered_disk(), 4, 8, true).unwrap();
        assert_eq!(ro.write_sectors(0, 1, &[0xEE; SECTOR_SIZE]), Err("partition: read-only"));

        let disk = numbered_disk();
        let raw: *const MemDisk = &*disk;
        let rw = Partition::new(disk, 4, 8, false).unwrap();
        rw.write_sectors(1, 1, &[0xEE; SECTOR_SIZE]).unwrap();
        // SAFETY: `rw` owns the box and is still alive; this only reads.
        let snap = unsafe { (*raw).snapshot() };
        assert!(snap[5 * SECTOR_SIZE..6 * SECTOR_SIZE].iter().all(|&b| b == 0xEE));
        assert!(snap[4 * SECTOR_SIZE..5 * SECTOR_SIZE].iter().all(|&b| b == 4));
        assert!(snap[6 * SECTOR_SIZE..7 * SECTOR_SIZE].iter().all(|&b| b == 6));
    }

    #[test]
    fn partition_window_must_fit_32_bit_lbas() {
        assert!(Partition::new(numbered_disk(), u32::MAX, 2, true).is_none());
    }
}
