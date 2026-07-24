// ext2/src/volume.rs
//
// `Ext2Core` — the device-backed half of migration steps 1 (raw block I/O,
// needed to actually read/write the superblock/BGD/bitmap bytes the parse
// functions elsewhere in this crate decode) and 2 (block/inode allocation
// and freeing, including the block-group-descriptor + superblock free-
// count bookkeeping). Moved verbatim out of `kernel::fs::ext2::Ext2Fs` —
// same on-disk format, same write ordering, same error conditions. See the
// crate doc comment for why every method here takes `&self` rather than
// splitting `&self`/`&mut self` by read/write.
//
// What is deliberately NOT here (still in `kernel::fs::ext2`, unmigrated):
// indirect block addressing + file byte-range read/write (step 3),
// directory operations (step 4), and `mount`'s own repair passes
// (`reconcile_free_counts`/`reclaim_orphans`, step 5) — `mount` below only
// parses the superblock and constructs `Self`, matching what
// `Ext2Fs::mount()` used to do before its repair-pass calls were added
// (those live in `kernel::fs::ext2::mount_and_repair` now, unmigrated).

use alloc::boxed::Box;
use alloc::vec::Vec;

use hal::block::{BlockDevice, SECTOR_SIZE};

use crate::bgd::{self, BlockGroupDesc};
use crate::bitmap;
use crate::error::Ext2Error;
use crate::superblock::Superblock;

/// The device-backed half of ext2: raw block I/O plus block/inode bitmap
/// allocation and free-count bookkeeping. See the module doc comment for
/// exactly what has (and hasn't) moved here.
pub struct Ext2Core {
    /// Every sector read/write funnels through here — `AtaBlockDevice` at
    /// real kernel boot, `hal::block::MemDisk` in this crate's own tests
    /// (and the kernel's QEMU integration test).
    pub device: Box<dyn BlockDevice>,
    pub sb: Superblock,
}

impl Ext2Core {
    /// Parse the superblock and construct a mounted `Ext2Core`. Does NOT
    /// run the mount-time repair passes (`reconcile_free_counts`/
    /// `reclaim_orphans`) — those stay in the kernel adapter (migration
    /// step 5), which calls them right after this returns, before
    /// publishing the result anywhere shared.
    pub fn mount(device: Box<dyn BlockDevice>) -> Result<Self, Ext2Error> {
        // Superblock is always at byte 1024, regardless of block size —
        // read it directly by sector before we know the block size at all.
        let mut raw = [0u8; 1024];
        device.read_sectors(2, 2, &mut raw).map_err(|_| Ext2Error::Io)?;
        let sb = Superblock::parse(&raw)?;
        Ok(Self { device, sb })
    }

    // ── Raw block I/O ────────────────────────────────────────────────

    /// Read one filesystem block (`sb.block_size` bytes) into `buf`.
    /// Rejects any `block_num` outside `0..blocks_count` — the single
    /// choke point every on-disk pointer passes through, so a corrupted
    /// BGD/inode pointer can't turn into a wild read at an arbitrary LBA.
    pub fn read_block(&self, block_num: u32, buf: &mut [u8]) -> Result<(), Ext2Error> {
        debug_assert!(buf.len() >= self.sb.block_size as usize);
        if block_num >= self.sb.blocks_count {
            return Err(Ext2Error::Io);
        }
        let sectors_per_block = (self.sb.block_size / SECTOR_SIZE as u32) as u8;
        let lba = block_num * sectors_per_block as u32;
        self.device.read_sectors(lba, sectors_per_block, buf).map_err(|_| Ext2Error::Io)
    }

    pub fn block_vec(&self, block_num: u32) -> Result<Vec<u8>, Ext2Error> {
        let mut buf = alloc::vec![0u8; self.sb.block_size as usize];
        self.read_block(block_num, &mut buf)?;
        Ok(buf)
    }

    /// Write one filesystem block (`sb.block_size` bytes) from `buf`. Same
    /// out-of-range-`block_num` rejection as `read_block`.
    pub fn write_block(&self, block_num: u32, buf: &[u8]) -> Result<(), Ext2Error> {
        debug_assert!(buf.len() >= self.sb.block_size as usize);
        if block_num >= self.sb.blocks_count {
            return Err(Ext2Error::Io);
        }
        let sectors_per_block = (self.sb.block_size / SECTOR_SIZE as u32) as u8;
        let lba = block_num * sectors_per_block as u32;
        self.device.write_sectors(lba, sectors_per_block, buf).map_err(|_| Ext2Error::Io)
    }

    // ── Block group descriptors ─────────────────────────────────────

    pub fn bgd_location(&self, group: u32) -> (u32, usize) {
        bgd::bgd_location(&self.sb, group)
    }

    pub fn read_bgd(&self, group: u32) -> Result<BlockGroupDesc, Ext2Error> {
        let (blk, off) = self.bgd_location(group);
        let buf = self.block_vec(blk)?;
        Ok(BlockGroupDesc::parse(&buf[off..]))
    }

    pub fn adjust_bgd_counts(
        &self,
        group: u32,
        free_blocks_delta: i32,
        free_inodes_delta: i32,
        used_dirs_delta: i32,
    ) -> Result<(), Ext2Error> {
        let (blk, off) = self.bgd_location(group);
        let mut buf = self.block_vec(blk)?;
        if free_blocks_delta != 0 {
            let cur = u16::from_le_bytes(buf[off + 12..off + 14].try_into().unwrap());
            let new = (cur as i32 + free_blocks_delta) as u16;
            buf[off + 12..off + 14].copy_from_slice(&new.to_le_bytes());
        }
        if free_inodes_delta != 0 {
            let cur = u16::from_le_bytes(buf[off + 14..off + 16].try_into().unwrap());
            let new = (cur as i32 + free_inodes_delta) as u16;
            buf[off + 14..off + 16].copy_from_slice(&new.to_le_bytes());
        }
        if used_dirs_delta != 0 {
            let cur = u16::from_le_bytes(buf[off + 16..off + 18].try_into().unwrap());
            let new = (cur as i32 + used_dirs_delta) as u16;
            buf[off + 16..off + 18].copy_from_slice(&new.to_le_bytes());
        }
        self.write_block(blk, &buf)
    }

    /// Patch the superblock's free block/inode counts directly on disk —
    /// re-reads the fixed byte-1024 superblock sectors fresh each time
    /// (same as `mount()`) rather than keeping a cached copy, since the
    /// free counters are the only mutable superblock state this driver
    /// tracks.
    pub fn adjust_sb_counts(&self, free_blocks_delta: i32, free_inodes_delta: i32) -> Result<(), Ext2Error> {
        let mut raw = [0u8; 1024];
        self.device.read_sectors(2, 2, &mut raw).map_err(|_| Ext2Error::Io)?;
        if free_blocks_delta != 0 {
            let cur = u32::from_le_bytes(raw[12..16].try_into().unwrap());
            let new = (cur as i64 + free_blocks_delta as i64) as u32;
            raw[12..16].copy_from_slice(&new.to_le_bytes());
        }
        if free_inodes_delta != 0 {
            let cur = u32::from_le_bytes(raw[16..20].try_into().unwrap());
            let new = (cur as i64 + free_inodes_delta as i64) as u32;
            raw[16..20].copy_from_slice(&new.to_le_bytes());
        }
        self.device.write_sectors(2, 2, &raw).map_err(|_| Ext2Error::Io)
    }

    pub fn blocks_in_group(&self, group: u32) -> u32 {
        let start = self.sb.first_data_block + group * self.sb.blocks_per_group;
        self.sb.blocks_count.saturating_sub(start).min(self.sb.blocks_per_group)
    }

    pub fn inodes_in_group(&self, group: u32) -> u32 {
        let start = group * self.sb.inodes_per_group;
        self.sb.inodes_count.saturating_sub(start).min(self.sb.inodes_per_group)
    }

    // ── Block/inode allocation ──────────────────────────────────────

    /// Allocate a free data block: scan each group's block bitmap for a
    /// clear bit, set it, update the group + superblock free counts, and
    /// zero the block's content (so a demand-paging-style hole never
    /// exposes stale disk data). Returns `Ok(None)` when the filesystem is
    /// full, `Err` on an I/O failure.
    pub fn alloc_block(&self) -> Result<Option<u32>, Ext2Error> {
        for group in 0..self.sb.num_groups {
            let bgd = self.read_bgd(group)?;
            if bgd.free_blocks == 0 {
                continue;
            }
            let group_blocks = self.blocks_in_group(group);
            let mut bmap = self.block_vec(bgd.block_bitmap)?;
            if let Some(bit) = bitmap::find_first_free_bit(&bmap, group_blocks) {
                bitmap::set_bit(&mut bmap, bit);
                self.write_block(bgd.block_bitmap, &bmap)?;
                self.adjust_bgd_counts(group, -1, 0, 0)?;
                self.adjust_sb_counts(-1, 0)?;
                let block_num = self.sb.first_data_block + group * self.sb.blocks_per_group + bit;
                let zeros = alloc::vec![0u8; self.sb.block_size as usize];
                self.write_block(block_num, &zeros)?;
                return Ok(Some(block_num));
            }
        }
        Ok(None)
    }

    pub fn free_block(&self, block_num: u32) -> Result<(), Ext2Error> {
        // Validate before subtracting — `block_num` here always originates
        // from an on-disk `i_block`/indirect pointer, so a corrupted value
        // below `first_data_block` must not underflow the `u32` group/bit
        // computation below.
        if block_num < self.sb.first_data_block || block_num >= self.sb.blocks_count {
            return Err(Ext2Error::Io);
        }
        let group = (block_num - self.sb.first_data_block) / self.sb.blocks_per_group;
        let bit = (block_num - self.sb.first_data_block) % self.sb.blocks_per_group;
        let bgd = self.read_bgd(group)?;
        let mut bmap = self.block_vec(bgd.block_bitmap)?;
        bitmap::clear_bit(&mut bmap, bit);
        self.write_block(bgd.block_bitmap, &bmap)?;
        self.adjust_bgd_counts(group, 1, 0, 0)?;
        self.adjust_sb_counts(1, 0)
    }

    /// Allocate a free inode. `is_dir` also bumps the group's directory
    /// count (`bg_used_dirs_count`) — cosmetic bookkeeping real ext2 tools
    /// (e2fsck, `df -i` equivalents) rely on, harmless if never read here.
    pub fn alloc_inode(&self, is_dir: bool) -> Result<Option<u32>, Ext2Error> {
        for group in 0..self.sb.num_groups {
            let bgd = self.read_bgd(group)?;
            if bgd.free_inodes == 0 {
                continue;
            }
            let group_inodes = self.inodes_in_group(group);
            let mut bmap = self.block_vec(bgd.inode_bitmap)?;
            if let Some(bit) = bitmap::find_first_free_bit(&bmap, group_inodes) {
                bitmap::set_bit(&mut bmap, bit);
                self.write_block(bgd.inode_bitmap, &bmap)?;
                self.adjust_bgd_counts(group, 0, -1, if is_dir { 1 } else { 0 })?;
                self.adjust_sb_counts(0, -1)?;
                return Ok(Some(group * self.sb.inodes_per_group + bit + 1));
            }
        }
        Ok(None)
    }

    pub fn free_inode(&self, ino: u32, is_dir: bool) -> Result<(), Ext2Error> {
        // Same corrupted-input-before-underflow guard as `free_block`.
        if ino < 1 || ino > self.sb.inodes_count {
            return Err(Ext2Error::Io);
        }
        let group = (ino - 1) / self.sb.inodes_per_group;
        let bit = (ino - 1) % self.sb.inodes_per_group;
        let bgd = self.read_bgd(group)?;
        let mut bmap = self.block_vec(bgd.inode_bitmap)?;
        bitmap::clear_bit(&mut bmap, bit);
        self.write_block(bgd.inode_bitmap, &bmap)?;
        self.adjust_bgd_counts(group, 0, 1, if is_dir { -1 } else { 0 })?;
        self.adjust_sb_counts(0, 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::boxed::Box;
    use hal::block::MemDisk;

    /// Builds a minimal, self-consistent rev-0 ext2 image on a `MemDisk`:
    /// one block group, 1024-byte blocks, root inode (ino 2) already
    /// allocated with one data block for "."/"..", everything else free.
    /// Mirrors (a smaller-scope copy of) `kernel::fs::ext2::
    /// build_minimal_image` — this crate has its own copy rather than
    /// depending on the kernel for test fixtures, since it must never
    /// depend on `kernel` at all (see the crate doc comment).
    fn minimal_image() -> MemDisk {
        const BLOCK_SIZE: u32 = 1024;
        const TOTAL_BLOCKS: u32 = 64;
        const INODES_COUNT: u32 = 32;
        const INODE_SIZE: u32 = 128;
        const FIRST_DATA_BLOCK: u32 = 1;
        const BGDT_BLOCK: u32 = FIRST_DATA_BLOCK + 1;
        const BLOCK_BITMAP_BLOCK: u32 = 3;
        const INODE_BITMAP_BLOCK: u32 = 4;
        const INODE_TABLE_START: u32 = 5;

        let inodes_per_block = BLOCK_SIZE / INODE_SIZE; // 8
        let inode_table_blocks = (INODES_COUNT + inodes_per_block - 1) / inodes_per_block; // 4
        let root_data_block = INODE_TABLE_START + inode_table_blocks; // 9

        let mut img = alloc::vec![0u8; (TOTAL_BLOCKS * BLOCK_SIZE) as usize];
        let put_block = |img: &mut alloc::vec::Vec<u8>, block_num: u32, data: &[u8]| {
            let off = (block_num * BLOCK_SIZE) as usize;
            img[off..off + data.len()].copy_from_slice(data);
        };

        let used_block_bits = root_data_block - FIRST_DATA_BLOCK + 1;
        let blocks_in_group0 = TOTAL_BLOCKS - FIRST_DATA_BLOCK;
        let free_blocks = blocks_in_group0 - used_block_bits;
        let free_inodes = INODES_COUNT - 1;

        // Superblock.
        let mut sb = alloc::vec![0u8; BLOCK_SIZE as usize];
        sb[0..4].copy_from_slice(&INODES_COUNT.to_le_bytes());
        sb[4..8].copy_from_slice(&TOTAL_BLOCKS.to_le_bytes());
        sb[12..16].copy_from_slice(&free_blocks.to_le_bytes());
        sb[16..20].copy_from_slice(&free_inodes.to_le_bytes());
        sb[20..24].copy_from_slice(&FIRST_DATA_BLOCK.to_le_bytes());
        sb[24..28].copy_from_slice(&0u32.to_le_bytes());
        sb[32..36].copy_from_slice(&TOTAL_BLOCKS.to_le_bytes()); // one group
        sb[40..44].copy_from_slice(&INODES_COUNT.to_le_bytes());
        sb[56..58].copy_from_slice(&crate::superblock::EXT2_MAGIC.to_le_bytes());
        put_block(&mut img, FIRST_DATA_BLOCK, &sb);

        // Block group descriptor.
        let mut bgd_buf = alloc::vec![0u8; 32];
        bgd_buf[0..4].copy_from_slice(&BLOCK_BITMAP_BLOCK.to_le_bytes());
        bgd_buf[4..8].copy_from_slice(&INODE_BITMAP_BLOCK.to_le_bytes());
        bgd_buf[8..12].copy_from_slice(&INODE_TABLE_START.to_le_bytes());
        bgd_buf[12..14].copy_from_slice(&(free_blocks as u16).to_le_bytes());
        bgd_buf[14..16].copy_from_slice(&(free_inodes as u16).to_le_bytes());
        let mut bgdt_block_buf = alloc::vec![0u8; BLOCK_SIZE as usize];
        bgdt_block_buf[0..32].copy_from_slice(&bgd_buf);
        put_block(&mut img, BGDT_BLOCK, &bgdt_block_buf);

        // Block bitmap: metadata footprint used.
        let mut block_bitmap = alloc::vec![0u8; BLOCK_SIZE as usize];
        for bit in 0..used_block_bits {
            block_bitmap[(bit / 8) as usize] |= 1u8 << (bit % 8);
        }
        put_block(&mut img, BLOCK_BITMAP_BLOCK, &block_bitmap);

        // Inode bitmap: only root (ino 2) used.
        let mut inode_bitmap = alloc::vec![0u8; BLOCK_SIZE as usize];
        inode_bitmap[0] |= 1u8 << (crate::superblock::ROOT_INO - 1);
        put_block(&mut img, INODE_BITMAP_BLOCK, &inode_bitmap);

        MemDisk::from_vec(img)
    }

    fn mount(disk: MemDisk) -> Ext2Core {
        Ext2Core::mount(Box::new(disk)).expect("mount")
    }

    #[test]
    fn mount_parses_expected_geometry() {
        let core = mount(minimal_image());
        assert_eq!(core.sb.block_size, 1024);
        assert_eq!(core.sb.num_groups, 1);
    }

    #[test]
    fn read_block_rejects_out_of_range() {
        let core = mount(minimal_image());
        let mut buf = alloc::vec![0u8; 1024];
        assert_eq!(core.read_block(64, &mut buf), Err(Ext2Error::Io));
        assert_eq!(core.read_block(1000, &mut buf), Err(Ext2Error::Io));
    }

    #[test]
    fn write_then_read_block_round_trips() {
        let core = mount(minimal_image());
        let mut pattern = alloc::vec![0u8; 1024];
        for (i, b) in pattern.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        core.write_block(20, &pattern).expect("write");
        let mut readback = alloc::vec![0u8; 1024];
        core.read_block(20, &mut readback).expect("read");
        assert_eq!(readback, pattern);
    }

    #[test]
    fn read_bgd_matches_seeded_values() {
        let core = mount(minimal_image());
        let bgd = core.read_bgd(0).expect("read_bgd");
        assert_eq!(bgd.block_bitmap, 3);
        assert_eq!(bgd.inode_bitmap, 4);
        assert_eq!(bgd.inode_table, 5);
    }

    #[test]
    fn alloc_block_returns_first_free_and_marks_it_used() {
        let core = mount(minimal_image());
        let before = core.read_bgd(0).unwrap().free_blocks;
        let b1 = core.alloc_block().unwrap().expect("has space");
        let after = core.read_bgd(0).unwrap().free_blocks;
        assert_eq!(after, before - 1);

        // Allocating again must not return the same block.
        let b2 = core.alloc_block().unwrap().expect("still has space");
        assert_ne!(b1, b2);
    }

    #[test]
    fn alloc_block_zeroes_the_new_block() {
        let core = mount(minimal_image());
        // Dirty the region first so a stale-data bug would be visible.
        let dirty = alloc::vec![0xAAu8; 1024];
        // Free block region starts right after root's data block (10);
        // write garbage there directly via write_block before allocating.
        core.write_block(10, &dirty).unwrap();
        let b = core.alloc_block().unwrap().expect("space");
        let content = core.block_vec(b).unwrap();
        assert!(content.iter().all(|&x| x == 0), "newly allocated block must be zeroed");
    }

    #[test]
    fn free_block_updates_counts_and_allows_reallocation() {
        let core = mount(minimal_image());
        let b = core.alloc_block().unwrap().unwrap();
        let free_after_alloc = core.read_bgd(0).unwrap().free_blocks;
        core.free_block(b).unwrap();
        let free_after_free = core.read_bgd(0).unwrap().free_blocks;
        assert_eq!(free_after_free, free_after_alloc + 1);

        // The freed block's bit must be clear again — reallocating enough
        // blocks to exhaust everything else must eventually reuse it.
        let mut seen = alloc::vec::Vec::new();
        while let Some(nb) = core.alloc_block().unwrap() {
            seen.push(nb);
        }
        assert!(seen.contains(&b), "freed block should become allocatable again");
    }

    #[test]
    fn alloc_block_returns_none_when_filesystem_full() {
        let core = mount(minimal_image());
        while core.alloc_block().unwrap().is_some() {}
        assert_eq!(core.alloc_block().unwrap(), None);
    }

    #[test]
    fn free_block_rejects_out_of_range() {
        let core = mount(minimal_image());
        assert_eq!(core.free_block(0), Err(Ext2Error::Io)); // below first_data_block
        assert_eq!(core.free_block(1000), Err(Ext2Error::Io)); // beyond blocks_count
    }

    #[test]
    fn alloc_inode_returns_first_free_and_marks_it_used() {
        let core = mount(minimal_image());
        let before = core.read_bgd(0).unwrap().free_inodes;
        let ino = core.alloc_inode(false).unwrap().expect("has space");
        assert_ne!(ino, crate::superblock::ROOT_INO); // root already allocated
        let after = core.read_bgd(0).unwrap().free_inodes;
        assert_eq!(after, before - 1);
    }

    #[test]
    fn alloc_inode_is_dir_bumps_used_dirs_count() {
        let core = mount(minimal_image());
        // bg_used_dirs_count lives at BGD offset +16, not exposed on
        // `BlockGroupDesc` (see its doc comment) — read it directly.
        let (blk, off) = core.bgd_location(0);
        let before = {
            let buf = core.block_vec(blk).unwrap();
            u16::from_le_bytes(buf[off + 16..off + 18].try_into().unwrap())
        };
        core.alloc_inode(true).unwrap().unwrap();
        let after = {
            let buf = core.block_vec(blk).unwrap();
            u16::from_le_bytes(buf[off + 16..off + 18].try_into().unwrap())
        };
        assert_eq!(after, before + 1);
    }

    #[test]
    fn free_inode_updates_counts_and_allows_reallocation() {
        let core = mount(minimal_image());
        let ino = core.alloc_inode(false).unwrap().unwrap();
        let free_after_alloc = core.read_bgd(0).unwrap().free_inodes;
        core.free_inode(ino, false).unwrap();
        let free_after_free = core.read_bgd(0).unwrap().free_inodes;
        assert_eq!(free_after_free, free_after_alloc + 1);
    }

    #[test]
    fn free_inode_rejects_out_of_range() {
        let core = mount(minimal_image());
        assert_eq!(core.free_inode(0, false), Err(Ext2Error::Io));
        assert_eq!(core.free_inode(1000, false), Err(Ext2Error::Io));
    }

    #[test]
    fn alloc_inode_returns_none_when_all_used() {
        let core = mount(minimal_image());
        while core.alloc_inode(false).unwrap().is_some() {}
        assert_eq!(core.alloc_inode(false).unwrap(), None);
    }

    #[test]
    fn sb_counts_reflect_alloc_and_free() {
        let core = mount(minimal_image());
        let mut raw = [0u8; 1024];
        core.device.read_sectors(2, 2, &mut raw).unwrap();
        let sb_free_before = u32::from_le_bytes(raw[12..16].try_into().unwrap());

        let b = core.alloc_block().unwrap().unwrap();
        core.device.read_sectors(2, 2, &mut raw).unwrap();
        let sb_free_after_alloc = u32::from_le_bytes(raw[12..16].try_into().unwrap());
        assert_eq!(sb_free_after_alloc, sb_free_before - 1);

        core.free_block(b).unwrap();
        core.device.read_sectors(2, 2, &mut raw).unwrap();
        let sb_free_after_free = u32::from_le_bytes(raw[12..16].try_into().unwrap());
        assert_eq!(sb_free_after_free, sb_free_before);
    }

    #[test]
    fn blocks_in_group_caps_at_last_partial_group() {
        let core = mount(minimal_image());
        // Single group covering the whole 64-block image (minus block 0).
        assert_eq!(core.blocks_in_group(0), 63);
    }

    #[test]
    fn inodes_in_group_matches_total_for_single_group() {
        let core = mount(minimal_image());
        assert_eq!(core.inodes_in_group(0), 32);
    }
}
