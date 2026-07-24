// ext2/src/volume.rs
//
// `Ext2Core` — the device-backed half of migration steps 1 (raw block I/O,
// needed to actually read/write the superblock/BGD/bitmap bytes the parse
// functions elsewhere in this crate decode), 2 (block/inode allocation and
// freeing, including the block-group-descriptor + superblock free-count
// bookkeeping), and 3 (inode-table read/write, direct/singly/doubly/triply
// -indirect block-pointer addressing, and file byte-range read/write).
// Moved verbatim out of `kernel::fs::ext2::Ext2Fs` — same on-disk format,
// same write ordering, same error conditions. See the crate doc comment for
// why every method here takes `&self` rather than splitting `&self`/
// `&mut self` by read/write.
//
// Inode-table read/write (`inode_location`/`read_inode`/`write_inode`)
// moved as part of step 3, not step 1/2, even though `RawInode` itself
// (the pure record format) has lived here since step 1 — see that struct's
// doc comment in `inode.rs`. They weren't needed until `write_file_range`
// below needed somewhere to persist a growing file's size, and moving them
// now (rather than leaving them one more layer removed in the kernel
// adapter) means `write_file_range` can call `self.write_inode(...)`
// directly, exactly mirroring how it worked before extraction.
//
// What is deliberately NOT here (still in `kernel::fs::ext2`, unmigrated):
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
use crate::inode::RawInode;
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

    // ── Inode table ──────────────────────────────────────────────────

    /// Locate the inode table block + byte offset for `ino` — shared by
    /// `read_inode` and `write_inode` so the two can never disagree about
    /// where an inode lives.
    pub fn inode_location(&self, ino: u32) -> Result<(u32, usize), Ext2Error> {
        // Real check, not `debug_assert!` — this driver trusts `ino`
        // values read back out of directory entries on disk, so a
        // corrupted dirent must not reach the `ino - 1` subtraction below
        // (a `u32` underflow: a panic in debug builds, a wraparound to a
        // huge-but-in-range-looking group index in release).
        if ino < 1 || ino > self.sb.inodes_count {
            return Err(Ext2Error::Io);
        }
        let group = (ino - 1) / self.sb.inodes_per_group;
        if group >= self.sb.num_groups {
            return Err(Ext2Error::Io);
        }
        let index_in_group = (ino - 1) % self.sb.inodes_per_group;

        // Block Group Descriptor for `group` (32 bytes each). bg_inode_table
        // is the THIRD u32 field (bg_block_bitmap, bg_inode_bitmap, then
        // bg_inode_table) — +8 bytes into the descriptor, not +0.
        let (bgd_block, bgd_off) = self.bgd_location(group);
        let bgd_buf = self.block_vec(bgd_block)?;
        let inode_table_block = u32::from_le_bytes(bgd_buf[bgd_off + 8..bgd_off + 12].try_into().unwrap());

        let inodes_per_block = self.sb.block_size / self.sb.inode_size as u32;
        let table_block = inode_table_block + index_in_group / inodes_per_block;
        let offset_in_block = ((index_in_group % inodes_per_block) * self.sb.inode_size as u32) as usize;
        Ok((table_block, offset_in_block))
    }

    /// Read the raw on-disk inode record for `ino`.
    ///
    /// `ino` should always be a value read out of this same filesystem (a
    /// directory entry, or the well-known root inode 2) — bounds-checked
    /// against the superblock's own counts as a corruption tripwire, not
    /// because callers are expected to pass arbitrary numbers.
    pub fn read_inode(&self, ino: u32) -> Result<RawInode, Ext2Error> {
        let (table_block, offset_in_block) = self.inode_location(ino)?;
        let block_buf = self.block_vec(table_block)?;
        Ok(RawInode::parse(&block_buf[offset_in_block..offset_in_block + self.sb.inode_size as usize]))
    }

    /// Write `raw` back to `ino`'s on-disk inode record. Read-modify-write:
    /// the inode table block holds several inodes, so the rest of the
    /// block must survive untouched.
    pub fn write_inode(&self, ino: u32, raw: &RawInode) -> Result<(), Ext2Error> {
        let (table_block, offset_in_block) = self.inode_location(ino)?;
        let mut block_buf = self.block_vec(table_block)?;
        block_buf[offset_in_block..offset_in_block + self.sb.inode_size as usize].copy_from_slice(&raw.buf);
        self.write_block(table_block, &block_buf)
    }

    // ── Block-pointer mapping (read-only lookup) ────────────────────────

    /// Map a file-relative block index to a filesystem block number.
    /// Direct (0..12), singly-indirect, and doubly-indirect — returns
    /// `Ok(None)` if the block is a hole (not yet allocated) or beyond what
    /// this driver supports (see the crate doc comment / module doc
    /// comment on indirect-block capacity).
    pub fn block_for_index(&self, raw: &RawInode, index: u32) -> Result<Option<u32>, Ext2Error> {
        if index < 12 {
            let b = raw.i_block(index as usize);
            return Ok(if b == 0 { None } else { Some(b) });
        }
        let ptrs_per_block = self.sb.block_size / 4;

        let indirect_index = index - 12;
        if indirect_index < ptrs_per_block {
            let indirect_block = raw.i_block(12);
            if indirect_block == 0 {
                return Ok(None);
            }
            return self.read_block_ptr(indirect_block, indirect_index);
        }

        let dbl_index = indirect_index - ptrs_per_block;
        let dbl_capacity = ptrs_per_block * ptrs_per_block;
        if dbl_index < dbl_capacity {
            let dbl_indirect_block = raw.i_block(13);
            if dbl_indirect_block == 0 {
                return Ok(None);
            }
            let first_level_index = dbl_index / ptrs_per_block;
            let second_level_index = dbl_index % ptrs_per_block;
            let Some(first_level_block) = self.read_block_ptr(dbl_indirect_block, first_level_index)? else {
                return Ok(None);
            };
            return self.read_block_ptr(first_level_block, second_level_index);
        }

        let tpl_index = dbl_index - dbl_capacity;
        let tpl_capacity = dbl_capacity * ptrs_per_block;
        if tpl_index < tpl_capacity {
            let tpl_block = raw.i_block(14);
            if tpl_block == 0 {
                return Ok(None);
            }
            let first_level_index = tpl_index / dbl_capacity;
            let rem = tpl_index % dbl_capacity;
            let second_level_index = rem / ptrs_per_block;
            let third_level_index = rem % ptrs_per_block;
            let Some(first_level_block) = self.read_block_ptr(tpl_block, first_level_index)? else {
                return Ok(None);
            };
            let Some(second_level_block) = self.read_block_ptr(first_level_block, second_level_index)? else {
                return Ok(None);
            };
            return self.read_block_ptr(second_level_block, third_level_index);
        }

        Ok(None) // beyond even triply-indirect capacity
    }

    /// Read the `index`-th block-pointer `u32` out of an indirect (or
    /// doubly-indirect first-level) pointer block — shared by both levels
    /// of `block_for_index` above.
    fn read_block_ptr(&self, block_num: u32, index: u32) -> Result<Option<u32>, Ext2Error> {
        let buf = self.block_vec(block_num)?;
        let off = (index * 4) as usize;
        let b = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        Ok(if b == 0 { None } else { Some(b) })
    }

    // ── Block-pointer mapping (allocate-on-demand) ──────────────────────

    /// Read (or allocate, if zero) the `index`-th pointer slot in an
    /// indirect/doubly-indirect pointer block, writing the new pointer back
    /// immediately — shared by both levels of `block_for_index_alloc`.
    fn get_or_alloc_ptr(&self, container_block: u32, index: u32) -> Result<u32, Ext2Error> {
        let mut buf = self.block_vec(container_block)?;
        let off = (index * 4) as usize;
        let existing = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        if existing != 0 {
            return Ok(existing);
        }
        let new_block = self.alloc_block()?.ok_or(Ext2Error::NoSpace)?;
        buf[off..off + 4].copy_from_slice(&new_block.to_le_bytes());
        self.write_block(container_block, &buf)?;
        Ok(new_block)
    }

    /// Like `block_for_index`, but allocates whatever's missing (data
    /// block, and any indirect/doubly-indirect pointer blocks along the
    /// way) instead of returning `None`. Mutates `raw`'s direct pointers
    /// in place — caller is responsible for persisting `raw` afterward.
    pub fn block_for_index_alloc(&self, raw: &mut RawInode, index: u32) -> Result<u32, Ext2Error> {
        if index < 12 {
            let b = raw.i_block(index as usize);
            if b != 0 {
                return Ok(b);
            }
            let nb = self.alloc_block()?.ok_or(Ext2Error::NoSpace)?;
            raw.set_i_block(index as usize, nb);
            return Ok(nb);
        }

        let ptrs_per_block = self.sb.block_size / 4;
        let indirect_index = index - 12;
        if indirect_index < ptrs_per_block {
            let indirect_block = raw.i_block(12);
            let indirect_block = if indirect_block == 0 {
                let nb = self.alloc_block()?.ok_or(Ext2Error::NoSpace)?;
                raw.set_i_block(12, nb);
                nb
            } else {
                indirect_block
            };
            return self.get_or_alloc_ptr(indirect_block, indirect_index);
        }

        let dbl_index = indirect_index - ptrs_per_block;
        let dbl_capacity = ptrs_per_block * ptrs_per_block;
        if dbl_index < dbl_capacity {
            let dbl_block = raw.i_block(13);
            let dbl_block = if dbl_block == 0 {
                let nb = self.alloc_block()?.ok_or(Ext2Error::NoSpace)?;
                raw.set_i_block(13, nb);
                nb
            } else {
                dbl_block
            };
            let first_level_index = dbl_index / ptrs_per_block;
            let second_level_index = dbl_index % ptrs_per_block;
            let first_level_block = self.get_or_alloc_ptr(dbl_block, first_level_index)?;
            return self.get_or_alloc_ptr(first_level_block, second_level_index);
        }

        let tpl_index = dbl_index - dbl_capacity;
        let tpl_capacity = dbl_capacity * ptrs_per_block;
        if tpl_index < tpl_capacity {
            let tpl_block = raw.i_block(14);
            let tpl_block = if tpl_block == 0 {
                let nb = self.alloc_block()?.ok_or(Ext2Error::NoSpace)?;
                raw.set_i_block(14, nb);
                nb
            } else {
                tpl_block
            };
            let first_level_index = tpl_index / dbl_capacity;
            let rem = tpl_index % dbl_capacity;
            let second_level_index = rem / ptrs_per_block;
            let third_level_index = rem % ptrs_per_block;
            let first_level_block = self.get_or_alloc_ptr(tpl_block, first_level_index)?;
            let second_level_block = self.get_or_alloc_ptr(first_level_block, second_level_index)?;
            return self.get_or_alloc_ptr(second_level_block, third_level_index);
        }

        Err(Ext2Error::TooLarge) // beyond even triply-indirect capacity — genuinely unsupported
    }

    // ── File data read/write ─────────────────────────────────────────────

    /// Read `buf.len()` bytes of file data starting at byte `offset`.
    pub fn read_file_range(&self, raw: &RawInode, offset: usize, buf: &mut [u8]) -> Result<(), Ext2Error> {
        let bs = self.sb.block_size as usize;
        let mut done = 0;
        while done < buf.len() {
            let file_pos = offset + done;
            let block_index = (file_pos / bs) as u32;
            let block_off = file_pos % bs;
            let n = (bs - block_off).min(buf.len() - done);

            match self.block_for_index(raw, block_index)? {
                Some(block_num) => {
                    let block_buf = self.block_vec(block_num)?;
                    buf[done..done + n].copy_from_slice(&block_buf[block_off..block_off + n]);
                }
                None => {
                    // Hole (sparse file) or past what block_for_index supports — zero-fill.
                    for b in &mut buf[done..done + n] { *b = 0; }
                }
            }
            done += n;
        }
        Ok(())
    }

    /// Write `data` at byte `offset`, allocating whatever blocks are
    /// needed (including growing the file past its current size — a
    /// "hole" between the old EOF and `offset` reads back as zeros, same
    /// as any real sparse file, since unallocated `block_for_index` reads
    /// already zero-fill). Updates and persists `raw`'s size + on-disk
    /// inode record before returning.
    pub fn write_file_range(&self, ino: u32, raw: &mut RawInode, offset: usize, data: &[u8]) -> Result<usize, Ext2Error> {
        if data.is_empty() {
            return Ok(0); // true no-op — access(2)'s W_OK probe relies on this
        }

        let bs = self.sb.block_size as usize;
        let mut done = 0;
        while done < data.len() {
            let file_pos = offset + done;
            let block_index = (file_pos / bs) as u32;
            let block_off = file_pos % bs;
            let n = (bs - block_off).min(data.len() - done);

            let block_num = self.block_for_index_alloc(raw, block_index)?;
            if n == bs {
                self.write_block(block_num, &data[done..done + n])?;
            } else {
                // Partial-block write — preserve the rest of the block's content.
                let mut block_buf = self.block_vec(block_num)?;
                block_buf[block_off..block_off + n].copy_from_slice(&data[done..done + n]);
                self.write_block(block_num, &block_buf)?;
            }
            done += n;
        }

        let new_size = (offset + data.len()) as u64;
        if new_size > raw.size() {
            raw.set_size(new_size);
        }
        self.write_inode(ino, raw)?;
        Ok(data.len())
    }

    // ── Block tree walk ──────────────────────────────────────────────────

    /// Call `visit(block_num)` for every block number this inode owns —
    /// direct, singly-, doubly-, and triply-indirect, pointer blocks
    /// themselves as well as their leaf targets. Shared tree-walk shape
    /// behind both `kernel::fs::ext2::Ext2Fs::free_all_blocks` (frees what
    /// it visits) and the mount-time orphan scan `reclaim_orphans`/
    /// `mark_reachable` (marks what it visits as reachable, never frees
    /// anything itself, both still unmigrated) — same traversal, different
    /// action per block, so the shape only needs to be right in one place.
    ///
    /// Callers are responsible for the `has_block_pointers()` guard (see
    /// `free_all_blocks`'s doc comment in the kernel adapter — a fast
    /// symlink's `i_block` bytes are inline text, not real pointers, and
    /// walking them as if they were would try to "free"/"mark" whatever
    /// garbage block numbers the text happens to decode to) — this
    /// function trusts `i_block` to hold real pointers unconditionally.
    pub fn visit_inode_blocks(&self, raw: &RawInode, mut visit: impl FnMut(u32) -> Result<(), Ext2Error>) -> Result<(), Ext2Error> {
        for i in 0..12 {
            let b = raw.i_block(i);
            if b != 0 {
                visit(b)?;
            }
        }

        let ptrs_per_block = self.sb.block_size / 4;

        let indirect = raw.i_block(12);
        if indirect != 0 {
            visit(indirect)?;
            self.visit_pointer_block_targets(indirect, &mut visit)?;
        }

        let dbl = raw.i_block(13);
        if dbl != 0 {
            visit(dbl)?;
            let buf = self.block_vec(dbl)?;
            for idx in 0..ptrs_per_block {
                let off = (idx * 4) as usize;
                let first_level = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
                if first_level != 0 {
                    visit(first_level)?;
                    self.visit_pointer_block_targets(first_level, &mut visit)?;
                }
            }
        }

        let tpl = raw.i_block(14);
        if tpl != 0 {
            visit(tpl)?;
            let buf = self.block_vec(tpl)?;
            for idx in 0..ptrs_per_block {
                let off = (idx * 4) as usize;
                let second_level = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
                if second_level != 0 {
                    visit(second_level)?;
                    let buf2 = self.block_vec(second_level)?;
                    for idx2 in 0..ptrs_per_block {
                        let off2 = (idx2 * 4) as usize;
                        let first_level = u32::from_le_bytes(buf2[off2..off2 + 4].try_into().unwrap());
                        if first_level != 0 {
                            visit(first_level)?;
                            self.visit_pointer_block_targets(first_level, &mut visit)?;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Call `visit` for every block number a pointer block (indirect, or
    /// one doubly-/triply-indirect first-level block) itself points at —
    /// NOT the pointer block's own number. Shared leaf-level step of
    /// `visit_inode_blocks`.
    fn visit_pointer_block_targets(&self, block_num: u32, visit: &mut impl FnMut(u32) -> Result<(), Ext2Error>) -> Result<(), Ext2Error> {
        let buf = self.block_vec(block_num)?;
        let ptrs_per_block = self.sb.block_size / 4;
        for idx in 0..ptrs_per_block {
            let off = (idx * 4) as usize;
            let b = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
            if b != 0 {
                visit(b)?;
            }
        }
        Ok(())
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

    // ── Migration step 3: inode table + indirect addressing + byte-range
    // read/write ──────────────────────────────────────────────────────
    //
    // `minimal_image()` is 1024-byte blocks, so `ptrs_per_block` = 256:
    //   direct:          block index  0..12
    //   singly-indirect: block index 12..268    (12 + 256)
    //   doubly-indirect: block index 268..65804  (268 + 256*256)
    //   triply-indirect: block index 65804..     (65804 + 256*256*256)
    // These tests write/read at a handful of individual indices near each
    // boundary rather than filling a whole region — `write_file_range`
    // only ever allocates blocks actually covered by the requested byte
    // range (see its own doc comment), so exercising index 268 costs 3
    // fresh blocks (doubly-indirect pointer block + one first-level
    // pointer block + one data block), not 65536 of them — the 64-block
    // `minimal_image()` fixture has more than enough headroom for every
    // test below, even a triply-indirect one.

    const BS: usize = 1024; // minimal_image()'s block_size, as a byte count
    // `write_file_range`/`write_inode`/`read_inode` all need a real,
    // already-allocated `ino` to write through — root (2) is the one
    // `minimal_image()` marks used in the inode bitmap, so it's reused
    // here purely as a valid inode number, unrelated to its real root-
    // directory role.
    const ROOT_INO: u32 = crate::superblock::ROOT_INO;

    #[test]
    fn block_for_index_direct_returns_i_block_entry_or_none() {
        let core = mount(minimal_image());
        let mut raw = RawInode::zeroed(128);
        raw.set_i_block(5, 42);
        assert_eq!(core.block_for_index(&raw, 5), Ok(Some(42)));
        assert_eq!(core.block_for_index(&raw, 6), Ok(None)); // hole
    }

    #[test]
    fn write_file_range_crosses_into_singly_indirect_block() {
        let core = mount(minimal_image());
        let mut raw = RawInode::zeroed(128);
        let data = b"cross-1";
        let offset = 12 * BS; // first singly-indirect index (12)
        core.write_file_range(ROOT_INO, &mut raw, offset, data).expect("write");

        // i_block(12) is the indirect pointer block itself, not the data
        // block written to — must be nonzero (allocated) but distinct from
        // whatever `block_for_index` resolves index 12 to.
        assert_ne!(raw.i_block(12), 0);
        let data_block = core.block_for_index(&raw, 12).unwrap().expect("data block mapped");
        assert_ne!(data_block, raw.i_block(12));

        let mut readback = alloc::vec![0u8; data.len()];
        core.read_file_range(&raw, offset, &mut readback).expect("read");
        assert_eq!(&readback, data);
    }

    #[test]
    fn write_file_range_crosses_into_doubly_indirect_block() {
        let core = mount(minimal_image());
        let mut raw = RawInode::zeroed(128);
        let data = b"cross-2";
        let offset = (12 + 256) * BS; // first doubly-indirect index (268)
        core.write_file_range(ROOT_INO, &mut raw, offset, data).expect("write");

        assert_ne!(raw.i_block(13), 0, "doubly-indirect pointer block must be allocated");

        let mut readback = alloc::vec![0u8; data.len()];
        core.read_file_range(&raw, offset, &mut readback).expect("read");
        assert_eq!(&readback, data);
    }

    #[test]
    fn write_file_range_crosses_into_triply_indirect_block() {
        let core = mount(minimal_image());
        let mut raw = RawInode::zeroed(128);
        let data = b"cross-3";
        let offset = (12 + 256 + 256 * 256) * BS; // first triply-indirect index (65804)
        core.write_file_range(ROOT_INO, &mut raw, offset, data).expect("write");

        assert_ne!(raw.i_block(14), 0, "triply-indirect pointer block must be allocated");

        let mut readback = alloc::vec![0u8; data.len()];
        core.read_file_range(&raw, offset, &mut readback).expect("read");
        assert_eq!(&readback, data);
    }

    #[test]
    fn get_or_alloc_ptr_reuses_the_same_pointer_block_for_neighboring_indices() {
        // Two writes landing in the same singly-indirect pointer block
        // (indices 12 and 13, both < 12 + ptrs_per_block) must share one
        // allocated indirect block, not allocate a fresh one each time.
        let core = mount(minimal_image());
        let mut raw = RawInode::zeroed(128);
        core.write_file_range(ROOT_INO, &mut raw, 12 * BS, b"a").expect("write 1");
        let indirect_after_first = raw.i_block(12);
        assert_ne!(indirect_after_first, 0);

        core.write_file_range(ROOT_INO, &mut raw, 13 * BS, b"b").expect("write 2");
        assert_eq!(raw.i_block(12), indirect_after_first, "must reuse the same indirect block");
    }

    #[test]
    fn read_file_range_partial_read_spans_two_blocks() {
        let core = mount(minimal_image());
        let mut raw = RawInode::zeroed(128);
        // Write a distinctive byte pattern covering all of block 0 and the
        // start of block 1.
        let mut pattern = alloc::vec![0u8; BS + 16];
        for (i, b) in pattern.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        core.write_file_range(ROOT_INO, &mut raw, 0, &pattern).expect("write");

        // Read a small window straddling the block-0/block-1 boundary.
        let mut readback = alloc::vec![0u8; 8];
        core.read_file_range(&raw, BS - 4, &mut readback).expect("read");
        assert_eq!(&readback, &pattern[BS - 4..BS + 4]);
    }

    #[test]
    fn write_file_range_extends_file_size_but_never_shrinks_it() {
        let core = mount(minimal_image());
        let mut raw = RawInode::zeroed(128);
        core.write_file_range(ROOT_INO, &mut raw, 100, &[1u8; 50]).expect("write");
        assert_eq!(raw.size(), 150);

        // A second, entirely-earlier write must not shrink the recorded size.
        core.write_file_range(ROOT_INO, &mut raw, 0, &[2u8; 10]).expect("write");
        assert_eq!(raw.size(), 150);
    }

    #[test]
    fn write_file_range_persists_size_to_the_on_disk_inode() {
        let core = mount(minimal_image());
        let mut raw = RawInode::zeroed(128);
        raw.set_i_mode(0x8000 | 0o644); // regular file, needed for is_reg()-gated size fields
        core.write_file_range(ROOT_INO, &mut raw, 0, b"hello").expect("write");

        let reread = core.read_inode(ROOT_INO).expect("read_inode");
        assert_eq!(reread.size(), 5);
    }

    #[test]
    fn read_file_range_beyond_eof_zero_fills_the_hole() {
        // A byte range past the last block `write_file_range` ever touched
        // is an unallocated hole — `block_for_index` returns `None` for it,
        // and `read_file_range` must zero-fill rather than error, exactly
        // like a real sparse file.
        let core = mount(minimal_image());
        let mut raw = RawInode::zeroed(128);
        core.write_file_range(ROOT_INO, &mut raw, 0, b"hi").expect("write");

        let mut readback = alloc::vec![0xAAu8; 16]; // poisoned, so a bug would be visible
        core.read_file_range(&raw, 10 * BS, &mut readback).expect("read past EOF");
        assert!(readback.iter().all(|&b| b == 0));
    }

    #[test]
    fn write_file_range_partial_block_write_preserves_the_rest_of_the_block() {
        let core = mount(minimal_image());
        let mut raw = RawInode::zeroed(128);
        core.write_file_range(ROOT_INO, &mut raw, 0, &[0xFFu8; BS]).expect("fill block");
        // Overwrite 4 bytes in the middle — the rest of the block must survive.
        core.write_file_range(ROOT_INO, &mut raw, 100, &[0x11, 0x22, 0x33, 0x44]).expect("partial write");

        let mut readback = alloc::vec![0u8; BS];
        core.read_file_range(&raw, 0, &mut readback).expect("read");
        assert_eq!(&readback[0..100], &[0xFFu8; 100][..]);
        assert_eq!(&readback[100..104], &[0x11, 0x22, 0x33, 0x44]);
        assert_eq!(&readback[104..], &alloc::vec![0xFFu8; BS - 104][..]);
    }

    #[test]
    fn write_file_range_empty_data_is_a_true_no_op() {
        // access(2)'s W_OK probe (kernel adapter) relies on this exact
        // behavior — see write_file_range's own doc comment.
        let core = mount(minimal_image());
        let mut raw = RawInode::zeroed(128);
        let n = core.write_file_range(ROOT_INO, &mut raw, 0, &[]).expect("empty write");
        assert_eq!(n, 0);
        assert_eq!(raw.size(), 0);
    }

    #[test]
    fn visit_inode_blocks_visits_pointer_blocks_and_leaf_data_blocks() {
        let core = mount(minimal_image());
        let mut raw = RawInode::zeroed(128);
        // One direct block, one singly-indirect data block, one
        // doubly-indirect data block.
        core.write_file_range(ROOT_INO, &mut raw, 0, b"direct").expect("direct");
        core.write_file_range(ROOT_INO, &mut raw, 12 * BS, b"indirect").expect("indirect");
        core.write_file_range(ROOT_INO, &mut raw, (12 + 256) * BS, b"dbl-indirect").expect("dbl indirect");

        let mut visited = alloc::vec::Vec::new();
        core.visit_inode_blocks(&raw, |b| { visited.push(b); Ok(()) }).expect("visit");

        // Expect: 1 direct data block, 1 indirect pointer block + its 1
        // leaf data block, 1 doubly-indirect pointer block + 1 first-level
        // pointer block + its 1 leaf data block = 6 total.
        assert_eq!(visited.len(), 6, "expected every pointer block AND every leaf data block visited");
        assert!(visited.contains(&raw.i_block(0)));
        assert!(visited.contains(&raw.i_block(12)), "indirect pointer block itself must be visited");
        assert!(visited.contains(&raw.i_block(13)), "doubly-indirect pointer block itself must be visited");

        // No duplicates, and no zero/sentinel entries.
        let mut sorted = visited.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), visited.len(), "no block should be visited twice");
        assert!(visited.iter().all(|&b| b != 0));
    }

    #[test]
    fn inode_location_rejects_out_of_range_ino() {
        let core = mount(minimal_image());
        assert_eq!(core.inode_location(0), Err(Ext2Error::Io));
        assert_eq!(core.inode_location(1000), Err(Ext2Error::Io));
    }

    #[test]
    fn read_write_inode_round_trips() {
        let core = mount(minimal_image());
        let mut raw = RawInode::zeroed(128);
        raw.set_i_mode(0x8000 | 0o600);
        raw.set_links_count(3);
        core.write_inode(ROOT_INO, &raw).expect("write_inode");

        let reread = core.read_inode(ROOT_INO).expect("read_inode");
        assert_eq!(reread.i_mode(), 0x8000 | 0o600);
        assert_eq!(reread.links_count(), 3);
    }
}
