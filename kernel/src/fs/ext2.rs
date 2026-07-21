// kernel/src/fs/ext2.rs
//
// Read-write ext2, mounted at /mnt over the ATA disk (block::ata) attached
// to the secondary IDE channel — see src/main.rs for how that disk image
// gets created and attached, and scripts docs there for how its content is
// seeded via `mke2fs -d`.
//
// SCOPE
// ─────
// Every mutation (block/inode bitmap alloc+free, group descriptor + super-
// block free-count bookkeeping, inode write-back, directory entry
// insert/remove) is applied directly to disk as it happens — there's no
// write-back cache and no journal, same as a real ext2 mount without a
// journal (ext3/4's main addition): a power loss mid multi-block operation
// (e.g. halfway through growing a doubly-indirect chain) can still leave
// the filesystem inconsistent. Not a regression this port introduces, just
// not fixed either — `e2fsck` exists for a reason.
//
// Direct, singly-indirect, and doubly-indirect blocks (see
// `block_for_index`/`block_for_index_alloc`) — up to ptrs_per_block² +
// ptrs_per_block + 12 blocks, 64MiB+ at this driver's 1024-byte block size;
// triply-indirect isn't implemented (nothing this image ships needs a file
// bigger than that) — reads short/writes fail with `EFBIG` rather than
// misbehaving.
//
// ext2-native symlinks aren't implemented (same "ramfs-only" convention the
// `symlink()` syscall already documents in kernel/src/process/syscall.rs —
// `Inode::symlink`'s EROFS default is left as-is here).
//
// Requires `s_feature_incompat` to only have FILETYPE set — anything else
// (in particular EXTENTS, i.e. an ext4 image) would misinterpret i_block
// completely, so mounting refuses outright rather than guess. FILETYPE is
// also what makes the on-disk dirent file_type byte meaningful, which the
// write path relies on when creating new entries.

use alloc::{boxed::Box, string::String, string::ToString, sync::Arc, vec::Vec};
use spin::{Mutex, Once};

use crate::fs::{
    types::{DirEntry, Errno, FileType, OpenFlags, Stat},
    vfs::{Filesystem, Inode},
};
use crate::process::file::{FileError, FileHandle, FileResult};

const EXT2_MAGIC: u16 = 0xEF53;
const ROOT_INO: u32 = 2;
const FEATURE_INCOMPAT_FILETYPE: u32 = 0x0002;

// ── Global mount state ──────────────────────────────────────────────────────
//
// Only one ext2 disk is ever mounted, so a global (rather than plumbing an
// Arc<Ext2Fs> through every Inode — see ramfs.rs's RamDirNode for why that
// self-reference is awkward without one) keeps this simple. Matches the
// existing BUDDY/SCHEDULERS/KEYBOARD_BUFFER style already used throughout
// the kernel for singleton state.

static EXT2: Once<Ext2Fs> = Once::new();

/// Mount the ext2 filesystem from the ATA disk. Call once, before the VFS
/// mounts `/mnt`. Returns `Err` (not panics) on any problem — a missing or
/// unreadable disk shouldn't take down boot, just leave `/mnt` unmounted.
pub fn init() -> Result<(), &'static str> {
    if !crate::block::ata::present() {
        return Err("no disk on the secondary IDE channel");
    }
    let fs = Ext2Fs::mount()?;
    EXT2.call_once(|| fs);
    Ok(())
}

fn fs() -> &'static Ext2Fs {
    EXT2.get().expect("fs::ext2::fs() called before init()")
}

// ── Superblock / filesystem-wide state ──────────────────────────────────────

struct Ext2Fs {
    block_size: u32,
    inodes_count: u32,
    blocks_count: u32,
    inodes_per_group: u32,
    blocks_per_group: u32,
    first_data_block: u32,
    inode_size: u16,
    bgdt_block: u32,
    num_groups: u32,
}

/// The subset of a block group descriptor's fields this driver reads/writes.
struct BgdRaw {
    block_bitmap: u32,
    inode_bitmap: u32,
    free_blocks: u16,
    free_inodes: u16,
}

impl Ext2Fs {
    fn mount() -> Result<Self, &'static str> {
        // Superblock is always at byte 1024, regardless of block size —
        // read it directly by sector before we know the block size at all.
        let mut raw = [0u8; 1024];
        crate::block::ata::read_sectors(2, 2, &mut raw)
            .map_err(|_| "ATA read of superblock failed")?;

        let magic = u16::from_le_bytes([raw[56], raw[57]]);
        if magic != EXT2_MAGIC {
            return Err("bad ext2 magic (not an ext2 filesystem, or wrong LBA)");
        }

        let log_block_size = u32::from_le_bytes(raw[24..28].try_into().unwrap());
        let block_size = 1024u32 << log_block_size;
        let blocks_per_group = u32::from_le_bytes(raw[32..36].try_into().unwrap());
        let inodes_per_group = u32::from_le_bytes(raw[40..44].try_into().unwrap());
        let inodes_count = u32::from_le_bytes(raw[0..4].try_into().unwrap());
        let blocks_count = u32::from_le_bytes(raw[4..8].try_into().unwrap());
        let first_data_block = u32::from_le_bytes(raw[20..24].try_into().unwrap());
        let rev_level = u32::from_le_bytes(raw[76..80].try_into().unwrap());

        let inode_size = if rev_level == 0 {
            128
        } else {
            u16::from_le_bytes(raw[88..90].try_into().unwrap())
        };
        let feature_incompat = if rev_level == 0 {
            0
        } else {
            u32::from_le_bytes(raw[96..100].try_into().unwrap())
        };

        if feature_incompat & !FEATURE_INCOMPAT_FILETYPE != 0 {
            return Err("unsupported ext2 incompat features (ext4 extents? journal?) — refusing to mount");
        }

        let num_groups = (blocks_count + blocks_per_group - 1) / blocks_per_group;
        let bgdt_block = first_data_block + 1;

        Ok(Self {
            block_size,
            inodes_count,
            blocks_count,
            inodes_per_group: inodes_per_group.max(1),
            blocks_per_group: blocks_per_group.max(1),
            first_data_block,
            inode_size: if inode_size == 0 { 128 } else { inode_size },
            bgdt_block,
            num_groups: num_groups.max(1),
        })
    }

    // ── Raw block I/O ────────────────────────────────────────────────────

    /// Read one filesystem block (`self.block_size` bytes) into `buf`.
    fn read_block(&self, block_num: u32, buf: &mut [u8]) {
        debug_assert!(buf.len() >= self.block_size as usize);
        let sectors_per_block = (self.block_size / crate::block::ata::SECTOR_SIZE as u32) as u8;
        let lba = block_num * sectors_per_block as u32;
        crate::block::ata::read_sectors(lba, sectors_per_block, buf)
            .expect("fs::ext2: ATA read failed");
    }

    fn block_vec(&self, block_num: u32) -> Vec<u8> {
        let mut buf = alloc::vec![0u8; self.block_size as usize];
        self.read_block(block_num, &mut buf);
        buf
    }

    /// Write one filesystem block (`self.block_size` bytes) from `buf`.
    fn write_block(&self, block_num: u32, buf: &[u8]) {
        debug_assert!(buf.len() >= self.block_size as usize);
        let sectors_per_block = (self.block_size / crate::block::ata::SECTOR_SIZE as u32) as u8;
        let lba = block_num * sectors_per_block as u32;
        crate::block::ata::write_sectors(lba, sectors_per_block, buf)
            .expect("fs::ext2: ATA write failed");
    }

    // ── Inode table ──────────────────────────────────────────────────────

    /// Locate the inode table block + byte offset for `ino` — shared by
    /// `read_inode` and `write_inode` so the two can never disagree about
    /// where an inode lives.
    fn inode_location(&self, ino: u32) -> (u32, usize) {
        debug_assert!(ino >= 1 && ino <= self.inodes_count, "ext2: inode {} out of range", ino);
        let group = (ino - 1) / self.inodes_per_group;
        debug_assert!(group < self.num_groups, "ext2: inode {} maps to out-of-range group {}", ino, group);
        let index_in_group = (ino - 1) % self.inodes_per_group;

        // Block Group Descriptor for `group` (32 bytes each). bg_inode_table
        // is the THIRD u32 field (bg_block_bitmap, bg_inode_bitmap, then
        // bg_inode_table) — +8 bytes into the descriptor, not +0.
        let (bgd_block, bgd_off) = self.bgd_location(group);
        let bgd_buf = self.block_vec(bgd_block);
        let inode_table_block = u32::from_le_bytes(bgd_buf[bgd_off + 8..bgd_off + 12].try_into().unwrap());

        let inodes_per_block = self.block_size / self.inode_size as u32;
        let table_block = inode_table_block + index_in_group / inodes_per_block;
        let offset_in_block = ((index_in_group % inodes_per_block) * self.inode_size as u32) as usize;
        (table_block, offset_in_block)
    }

    /// Read the raw on-disk inode record for `ino`.
    ///
    /// `ino` should always be a value read out of this same filesystem (a
    /// directory entry, or the well-known root inode 2) — bounds-checked
    /// against the superblock's own counts as a corruption tripwire, not
    /// because callers are expected to pass arbitrary numbers.
    fn read_inode(&self, ino: u32) -> RawInode {
        let (table_block, offset_in_block) = self.inode_location(ino);
        let block_buf = self.block_vec(table_block);
        RawInode::parse(&block_buf[offset_in_block..offset_in_block + self.inode_size as usize])
    }

    /// Write `raw` back to `ino`'s on-disk inode record. Read-modify-write:
    /// the inode table block holds several inodes, so the rest of the
    /// block must survive untouched.
    fn write_inode(&self, ino: u32, raw: &RawInode) {
        let (table_block, offset_in_block) = self.inode_location(ino);
        let mut block_buf = self.block_vec(table_block);
        block_buf[offset_in_block..offset_in_block + self.inode_size as usize].copy_from_slice(&raw.buf);
        self.write_block(table_block, &block_buf);
    }

    // ── Block-pointer mapping (read-only lookup) ────────────────────────

    /// Map a file-relative block index to a filesystem block number.
    /// Direct (0..12), singly-indirect, and doubly-indirect — returns
    /// `None` if the block is a hole (not yet allocated) or beyond what
    /// this driver supports (see module doc comment).
    fn block_for_index(&self, raw: &RawInode, index: u32) -> Option<u32> {
        if index < 12 {
            let b = raw.i_block(index as usize);
            return if b == 0 { None } else { Some(b) };
        }
        let ptrs_per_block = self.block_size / 4;

        let indirect_index = index - 12;
        if indirect_index < ptrs_per_block {
            let indirect_block = raw.i_block(12);
            if indirect_block == 0 {
                return None;
            }
            return self.read_block_ptr(indirect_block, indirect_index);
        }

        let dbl_index = indirect_index - ptrs_per_block;
        let dbl_capacity = ptrs_per_block * ptrs_per_block;
        if dbl_index < dbl_capacity {
            let dbl_indirect_block = raw.i_block(13);
            if dbl_indirect_block == 0 {
                return None;
            }
            let first_level_index = dbl_index / ptrs_per_block;
            let second_level_index = dbl_index % ptrs_per_block;
            let first_level_block = self.read_block_ptr(dbl_indirect_block, first_level_index)?;
            return self.read_block_ptr(first_level_block, second_level_index);
        }

        None // triply indirect — not implemented
    }

    /// Read the `index`-th block-pointer `u32` out of an indirect (or
    /// doubly-indirect first-level) pointer block — shared by both levels
    /// of `block_for_index` above.
    fn read_block_ptr(&self, block_num: u32, index: u32) -> Option<u32> {
        let buf = self.block_vec(block_num);
        let off = (index * 4) as usize;
        let b = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        if b == 0 { None } else { Some(b) }
    }

    // ── Block-pointer mapping (allocate-on-demand) ──────────────────────

    /// Read (or allocate, if zero) the `index`-th pointer slot in an
    /// indirect/doubly-indirect pointer block, writing the new pointer back
    /// immediately — shared by both levels of `block_for_index_alloc`.
    fn get_or_alloc_ptr(&self, container_block: u32, index: u32) -> Result<u32, Errno> {
        let mut buf = self.block_vec(container_block);
        let off = (index * 4) as usize;
        let existing = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        if existing != 0 {
            return Ok(existing);
        }
        let new_block = self.alloc_block().ok_or(Errno::ENOSPC)?;
        buf[off..off + 4].copy_from_slice(&new_block.to_le_bytes());
        self.write_block(container_block, &buf);
        Ok(new_block)
    }

    /// Like `block_for_index`, but allocates whatever's missing (data
    /// block, and any indirect/doubly-indirect pointer blocks along the
    /// way) instead of returning `None`. Mutates `raw`'s direct pointers
    /// in place — caller is responsible for persisting `raw` afterward.
    fn block_for_index_alloc(&self, raw: &mut RawInode, index: u32) -> Result<u32, Errno> {
        if index < 12 {
            let b = raw.i_block(index as usize);
            if b != 0 {
                return Ok(b);
            }
            let nb = self.alloc_block().ok_or(Errno::ENOSPC)?;
            raw.set_i_block(index as usize, nb);
            return Ok(nb);
        }

        let ptrs_per_block = self.block_size / 4;
        let indirect_index = index - 12;
        if indirect_index < ptrs_per_block {
            let indirect_block = raw.i_block(12);
            let indirect_block = if indirect_block == 0 {
                let nb = self.alloc_block().ok_or(Errno::ENOSPC)?;
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
                let nb = self.alloc_block().ok_or(Errno::ENOSPC)?;
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

        Err(Errno::EFBIG) // triply indirect — not implemented, see module doc comment
    }

    // ── File data read/write ─────────────────────────────────────────────

    /// Read `buf.len()` bytes of file data starting at byte `offset`.
    fn read_file_range(&self, raw: &RawInode, offset: usize, buf: &mut [u8]) {
        let bs = self.block_size as usize;
        let mut done = 0;
        while done < buf.len() {
            let file_pos = offset + done;
            let block_index = (file_pos / bs) as u32;
            let block_off = file_pos % bs;
            let n = (bs - block_off).min(buf.len() - done);

            match self.block_for_index(raw, block_index) {
                Some(block_num) => {
                    let block_buf = self.block_vec(block_num);
                    buf[done..done + n].copy_from_slice(&block_buf[block_off..block_off + n]);
                }
                None => {
                    // Hole (sparse file) or past what block_for_index supports — zero-fill.
                    for b in &mut buf[done..done + n] { *b = 0; }
                }
            }
            done += n;
        }
    }

    /// Write `data` at byte `offset`, allocating whatever blocks are
    /// needed (including growing the file past its current size — a
    /// "hole" between the old EOF and `offset` reads back as zeros, same
    /// as any real sparse file, since unallocated `block_for_index` reads
    /// already zero-fill). Updates and persists `raw`'s size + on-disk
    /// inode record before returning.
    fn write_file_range(&self, ino: u32, raw: &mut RawInode, offset: usize, data: &[u8]) -> Result<usize, Errno> {
        if data.is_empty() {
            return Ok(0); // true no-op — access(2)'s W_OK probe relies on this
        }

        let bs = self.block_size as usize;
        let mut done = 0;
        while done < data.len() {
            let file_pos = offset + done;
            let block_index = (file_pos / bs) as u32;
            let block_off = file_pos % bs;
            let n = (bs - block_off).min(data.len() - done);

            let block_num = self.block_for_index_alloc(raw, block_index)?;
            if n == bs {
                self.write_block(block_num, &data[done..done + n]);
            } else {
                // Partial-block write — preserve the rest of the block's content.
                let mut block_buf = self.block_vec(block_num);
                block_buf[block_off..block_off + n].copy_from_slice(&data[done..done + n]);
                self.write_block(block_num, &block_buf);
            }
            done += n;
        }

        let new_size = (offset + data.len()) as u64;
        if new_size > raw.size() {
            raw.set_size(new_size);
        }
        self.write_inode(ino, raw);
        Ok(data.len())
    }

    /// Free every block this inode owns (direct, singly-indirect data +
    /// pointer block, doubly-indirect data + first-level pointer blocks +
    /// the doubly-indirect block itself) and zero its size. Does NOT free
    /// the inode itself — callers decide that based on link count.
    fn free_all_blocks(&self, raw: &mut RawInode) {
        for i in 0..12 {
            let b = raw.i_block(i);
            if b != 0 {
                self.free_block(b);
                raw.set_i_block(i, 0);
            }
        }

        let ptrs_per_block = self.block_size / 4;
        let indirect = raw.i_block(12);
        if indirect != 0 {
            self.free_pointer_block_targets(indirect);
            self.free_block(indirect);
            raw.set_i_block(12, 0);
        }

        let dbl = raw.i_block(13);
        if dbl != 0 {
            let buf = self.block_vec(dbl);
            for idx in 0..ptrs_per_block {
                let off = (idx * 4) as usize;
                let first_level = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
                if first_level != 0 {
                    self.free_pointer_block_targets(first_level);
                    self.free_block(first_level);
                }
            }
            self.free_block(dbl);
            raw.set_i_block(13, 0);
        }

        // Triply-indirect (i_block(14)) is never allocated by this driver
        // (see block_for_index_alloc), so there's nothing to free there.

        raw.set_size(0);
        raw.set_blocks_512(0);
    }

    /// Free every block a pointer block (indirect, or one doubly-indirect
    /// first-level block) points at — NOT the pointer block itself.
    fn free_pointer_block_targets(&self, block_num: u32) {
        let buf = self.block_vec(block_num);
        let ptrs_per_block = self.block_size / 4;
        for idx in 0..ptrs_per_block {
            let off = (idx * 4) as usize;
            let b = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
            if b != 0 { self.free_block(b); }
        }
    }

    /// Truncate a file to zero length: frees all its data blocks and
    /// persists the now-empty inode. Backs `O_TRUNC`.
    fn truncate_to_zero(&self, ino: u32, raw: &mut RawInode) {
        self.free_all_blocks(raw);
        self.write_inode(ino, raw);
    }

    // ── Directory entries ────────────────────────────────────────────────

    /// Parse every directory entry out of `raw`'s data blocks (direct +
    /// indirect, same limit as file reads).
    fn read_dir_entries(&self, raw: &RawInode) -> Vec<Ext2DirEntry> {
        let mut entries = Vec::new();
        let bs = self.block_size;
        let num_blocks = (raw.size() + bs as u64 - 1) / bs as u64;

        for block_index in 0..num_blocks as u32 {
            let Some(block_num) = self.block_for_index(raw, block_index) else { continue };
            let buf = self.block_vec(block_num);
            let mut off = 0usize;
            while off + 8 <= buf.len() {
                let inode = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
                let rec_len = u16::from_le_bytes(buf[off + 4..off + 6].try_into().unwrap());
                let name_len = buf[off + 6] as usize;
                let file_type = buf[off + 7];
                if rec_len < 8 {
                    break; // corrupt — stop rather than loop forever
                }
                if inode != 0 && name_len > 0 && off + 8 + name_len <= buf.len() {
                    let name = String::from_utf8_lossy(&buf[off + 8..off + 8 + name_len]).to_string();
                    if name != "." && name != ".." {
                        entries.push(Ext2DirEntry {
                            ino: inode,
                            kind: ext2_file_type_to_vfs(file_type),
                            name,
                        });
                    }
                }
                off += rec_len as usize;
            }
        }
        entries
    }

    /// Insert a new `(name -> ino)` directory entry into `dir_raw`'s data,
    /// splitting an existing entry's slack space (real ext2's own
    /// approach) if one is big enough, or reusing a deleted (`inode == 0`)
    /// slot, or — only if nothing fits — allocating and appending a whole
    /// new directory block.
    fn add_dir_entry(&self, dir_ino: u32, dir_raw: &mut RawInode, name: &str, ino: u32, kind: FileType) -> Result<(), Errno> {
        let bs = self.block_size as usize;
        let needed = dirent_len(name.len());
        let num_blocks = ((dir_raw.size() as usize) + bs - 1) / bs;

        for block_index in 0..num_blocks as u32 {
            let Some(block_num) = self.block_for_index(dir_raw, block_index) else { continue };
            let mut buf = self.block_vec(block_num);
            let mut off = 0usize;
            while off + 8 <= buf.len() {
                let entry_ino = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
                let rec_len = u16::from_le_bytes(buf[off + 4..off + 6].try_into().unwrap()) as usize;
                if rec_len < 8 {
                    return Err(Errno::EIO); // corrupt directory
                }
                let name_len = buf[off + 6] as usize;
                let used_len = if entry_ino == 0 { 0 } else { dirent_len(name_len) };
                let slack = rec_len - used_len;

                if slack >= needed {
                    if entry_ino != 0 {
                        // Split: shrink the existing entry to its real
                        // length, place the new one in the freed tail.
                        buf[off + 4..off + 6].copy_from_slice(&(used_len as u16).to_le_bytes());
                        let new_off = off + used_len;
                        write_dirent(&mut buf[new_off..new_off + slack], ino, slack as u16, name, kind);
                    } else {
                        // Reuse a deleted slot in place, keeping its rec_len.
                        write_dirent(&mut buf[off..off + rec_len], ino, rec_len as u16, name, kind);
                    }
                    self.write_block(block_num, &buf);
                    return Ok(());
                }
                off += rec_len;
            }
        }

        // No room anywhere — grow the directory by one block.
        let new_block_index = num_blocks as u32;
        let new_block = self.block_for_index_alloc(dir_raw, new_block_index)?;
        let mut buf = alloc::vec![0u8; bs];
        write_dirent(&mut buf[..], ino, bs as u16, name, kind);
        self.write_block(new_block, &buf);
        dir_raw.set_size((new_block_index as u64 + 1) * bs as u64);
        self.write_inode(dir_ino, dir_raw);
        Ok(())
    }

    /// Remove the directory entry named `name` from `dir_raw`'s data.
    /// Merges its `rec_len` into the previous entry in the same block
    /// (real ext2's approach), or — if it's the first entry in the block —
    /// just zeroes its inode field, leaving a reusable deleted slot.
    /// Returns the removed entry's inode number and kind.
    fn remove_dir_entry(&self, dir_raw: &RawInode, name: &str) -> Result<(u32, FileType), Errno> {
        let bs = self.block_size as usize;
        let num_blocks = ((dir_raw.size() as usize) + bs - 1) / bs;

        for block_index in 0..num_blocks as u32 {
            let Some(block_num) = self.block_for_index(dir_raw, block_index) else { continue };
            let mut buf = self.block_vec(block_num);
            let mut off = 0usize;
            let mut prev_off: Option<usize> = None;
            while off + 8 <= buf.len() {
                let entry_ino = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
                let rec_len = u16::from_le_bytes(buf[off + 4..off + 6].try_into().unwrap()) as usize;
                if rec_len < 8 {
                    break;
                }
                let name_len = buf[off + 6] as usize;
                let file_type = buf[off + 7];
                if entry_ino != 0 && name_len == name.len() && off + 8 + name_len <= buf.len()
                    && &buf[off + 8..off + 8 + name_len] == name.as_bytes()
                {
                    if let Some(p) = prev_off {
                        let p_rec_len = u16::from_le_bytes(buf[p + 4..p + 6].try_into().unwrap()) as usize;
                        buf[p + 4..p + 6].copy_from_slice(&((p_rec_len + rec_len) as u16).to_le_bytes());
                    } else {
                        buf[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
                    }
                    self.write_block(block_num, &buf);
                    return Ok((entry_ino, ext2_file_type_to_vfs(file_type)));
                }
                prev_off = Some(off);
                off += rec_len;
            }
        }
        Err(Errno::ENOENT)
    }

    /// Rewrite a directory's `".."` entry to point at `new_parent_ino` —
    /// used when moving (rename) a subdirectory to a different parent.
    /// `".."` is always in the directory's first data block (it's written
    /// there by `mkdir` and this driver never reorders entries).
    fn set_dotdot(&self, dir_raw: &RawInode, new_parent_ino: u32) -> Result<(), Errno> {
        let Some(block_num) = self.block_for_index(dir_raw, 0) else { return Err(Errno::EIO) };
        let mut buf = self.block_vec(block_num);
        let mut off = 0usize;
        while off + 8 <= buf.len() {
            let rec_len = u16::from_le_bytes(buf[off + 4..off + 6].try_into().unwrap()) as usize;
            if rec_len < 8 {
                break;
            }
            let name_len = buf[off + 6] as usize;
            if name_len == 2 && off + 8 + 2 <= buf.len() && &buf[off + 8..off + 10] == b".." {
                buf[off..off + 4].copy_from_slice(&new_parent_ino.to_le_bytes());
                self.write_block(block_num, &buf);
                return Ok(());
            }
            off += rec_len;
        }
        Err(Errno::EIO)
    }

    // ── Block group descriptors / bitmaps ───────────────────────────────

    fn bgd_location(&self, group: u32) -> (u32, usize) {
        let bgd_per_block = self.block_size / 32;
        let bgd_block = self.bgdt_block + group / bgd_per_block;
        let bgd_offset = ((group % bgd_per_block) * 32) as usize;
        (bgd_block, bgd_offset)
    }

    fn read_bgd(&self, group: u32) -> BgdRaw {
        let (blk, off) = self.bgd_location(group);
        let buf = self.block_vec(blk);
        BgdRaw {
            block_bitmap: u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()),
            inode_bitmap: u32::from_le_bytes(buf[off + 4..off + 8].try_into().unwrap()),
            free_blocks: u16::from_le_bytes(buf[off + 12..off + 14].try_into().unwrap()),
            free_inodes: u16::from_le_bytes(buf[off + 14..off + 16].try_into().unwrap()),
        }
    }

    fn adjust_bgd_counts(&self, group: u32, free_blocks_delta: i32, free_inodes_delta: i32, used_dirs_delta: i32) {
        let (blk, off) = self.bgd_location(group);
        let mut buf = self.block_vec(blk);
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
        self.write_block(blk, &buf);
    }

    /// Patch the superblock's free block/inode counts directly on disk —
    /// re-reads the fixed byte-1024 superblock sectors fresh each time
    /// (same as `mount()`) rather than keeping a cached copy, since this is
    /// the only mutable superblock state this driver tracks.
    fn adjust_sb_counts(&self, free_blocks_delta: i32, free_inodes_delta: i32) {
        let mut raw = [0u8; 1024];
        crate::block::ata::read_sectors(2, 2, &mut raw).expect("fs::ext2: superblock re-read failed");
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
        crate::block::ata::write_sectors(2, 2, &raw).expect("fs::ext2: superblock write failed");
    }

    fn blocks_in_group(&self, group: u32) -> u32 {
        let start = self.first_data_block + group * self.blocks_per_group;
        self.blocks_count.saturating_sub(start).min(self.blocks_per_group)
    }

    fn inodes_in_group(&self, group: u32) -> u32 {
        let start = group * self.inodes_per_group;
        self.inodes_count.saturating_sub(start).min(self.inodes_per_group)
    }

    /// Allocate a free data block: scan each group's block bitmap for a
    /// clear bit, set it, update the group + superblock free counts, and
    /// zero the block's content (so a demand-paging-style hole never
    /// exposes stale disk data). Returns `None` when the filesystem is
    /// full (`ENOSPC`).
    fn alloc_block(&self) -> Option<u32> {
        for group in 0..self.num_groups {
            let bgd = self.read_bgd(group);
            if bgd.free_blocks == 0 {
                continue;
            }
            let group_blocks = self.blocks_in_group(group);
            let mut bitmap = self.block_vec(bgd.block_bitmap);
            for bit in 0..group_blocks {
                let byte = (bit / 8) as usize;
                let mask = 1u8 << (bit % 8);
                if bitmap[byte] & mask == 0 {
                    bitmap[byte] |= mask;
                    self.write_block(bgd.block_bitmap, &bitmap);
                    self.adjust_bgd_counts(group, -1, 0, 0);
                    self.adjust_sb_counts(-1, 0);
                    let block_num = self.first_data_block + group * self.blocks_per_group + bit;
                    let zeros = alloc::vec![0u8; self.block_size as usize];
                    self.write_block(block_num, &zeros);
                    return Some(block_num);
                }
            }
        }
        None
    }

    fn free_block(&self, block_num: u32) {
        let group = (block_num - self.first_data_block) / self.blocks_per_group;
        let bit = (block_num - self.first_data_block) % self.blocks_per_group;
        let bgd = self.read_bgd(group);
        let mut bitmap = self.block_vec(bgd.block_bitmap);
        let byte = (bit / 8) as usize;
        let mask = 1u8 << (bit % 8);
        bitmap[byte] &= !mask;
        self.write_block(bgd.block_bitmap, &bitmap);
        self.adjust_bgd_counts(group, 1, 0, 0);
        self.adjust_sb_counts(1, 0);
    }

    /// Allocate a free inode. `is_dir` also bumps the group's directory
    /// count (`bg_used_dirs_count`) — cosmetic bookkeeping real ext2 tools
    /// (e2fsck, `df -i` equivalents) rely on, harmless if never read here.
    fn alloc_inode(&self, is_dir: bool) -> Option<u32> {
        for group in 0..self.num_groups {
            let bgd = self.read_bgd(group);
            if bgd.free_inodes == 0 {
                continue;
            }
            let group_inodes = self.inodes_in_group(group);
            let mut bitmap = self.block_vec(bgd.inode_bitmap);
            for bit in 0..group_inodes {
                let byte = (bit / 8) as usize;
                let mask = 1u8 << (bit % 8);
                if bitmap[byte] & mask == 0 {
                    bitmap[byte] |= mask;
                    self.write_block(bgd.inode_bitmap, &bitmap);
                    self.adjust_bgd_counts(group, 0, -1, if is_dir { 1 } else { 0 });
                    self.adjust_sb_counts(0, -1);
                    return Some(group * self.inodes_per_group + bit + 1);
                }
            }
        }
        None
    }

    fn free_inode(&self, ino: u32, is_dir: bool) {
        let group = (ino - 1) / self.inodes_per_group;
        let bit = (ino - 1) % self.inodes_per_group;
        let bgd = self.read_bgd(group);
        let mut bitmap = self.block_vec(bgd.inode_bitmap);
        let byte = (bit / 8) as usize;
        let mask = 1u8 << (bit % 8);
        bitmap[byte] &= !mask;
        self.write_block(bgd.inode_bitmap, &bitmap);
        self.adjust_bgd_counts(group, 0, 1, if is_dir { -1 } else { 0 });
        self.adjust_sb_counts(0, 1);
    }
}

fn ext2_file_type_to_vfs(ft: u8) -> FileType {
    match ft {
        2 => FileType::Directory,
        7 => FileType::Symlink,
        3 => FileType::BlockDevice,
        4 => FileType::CharDevice,
        _ => FileType::Regular,
    }
}

fn vfs_file_type_to_ext2(kind: FileType) -> u8 {
    match kind {
        FileType::Directory => 2,
        FileType::Symlink => 7,
        FileType::BlockDevice => 3,
        FileType::CharDevice => 4,
        FileType::Regular => 1,
    }
}

/// On-disk `ext2_dir_entry_2` record length for a `name_len`-byte name,
/// rounded up to 4-byte alignment (`8 + name_len`, then rounded).
fn dirent_len(name_len: usize) -> usize {
    (8 + name_len + 3) & !3
}

/// Serialize one directory entry into `buf` (must be exactly `rec_len`
/// bytes — the caller decides how much slack this entry claims).
fn write_dirent(buf: &mut [u8], ino: u32, rec_len: u16, name: &str, kind: FileType) {
    buf[0..4].copy_from_slice(&ino.to_le_bytes());
    buf[4..6].copy_from_slice(&rec_len.to_le_bytes());
    buf[6] = name.len() as u8;
    buf[7] = vfs_file_type_to_ext2(kind);
    buf[8..8 + name.len()].copy_from_slice(name.as_bytes());
}

struct Ext2DirEntry {
    ino: u32,
    kind: FileType,
    name: String,
}

// ── Raw inode (subset of fields we use) ─────────────────────────────────────

/// A raw on-disk inode record, kept as its exact `inode_size` bytes rather
/// than a handful of decoded fields — so a write-back only patches the
/// fields this driver actually understands/manages (mode, size, links
/// count, block count, block pointers) and leaves everything else (times,
/// uid/gid, ACL/generation fields) exactly as read, instead of silently
/// zeroing them.
#[derive(Clone)]
struct RawInode {
    buf: Vec<u8>,
}

impl RawInode {
    fn parse(buf: &[u8]) -> Self {
        Self { buf: buf.to_vec() }
    }

    /// A brand-new, all-zero inode record of `size` bytes (`inode_size`) —
    /// used by `create`/`mkdir` before filling in mode/links/blocks.
    fn zeroed(size: usize) -> Self {
        Self { buf: alloc::vec![0u8; size] }
    }

    fn i_mode(&self) -> u16 {
        u16::from_le_bytes(self.buf[0..2].try_into().unwrap())
    }

    fn set_i_mode(&mut self, v: u16) {
        self.buf[0..2].copy_from_slice(&v.to_le_bytes());
    }

    fn links_count(&self) -> u16 {
        u16::from_le_bytes(self.buf[26..28].try_into().unwrap())
    }

    fn set_links_count(&mut self, v: u16) {
        self.buf[26..28].copy_from_slice(&v.to_le_bytes());
    }

    fn set_blocks_512(&mut self, v: u32) {
        self.buf[28..32].copy_from_slice(&v.to_le_bytes());
    }

    /// `size_hi` (`i_dir_acl`/`i_size_high`) only means "upper size bits"
    /// for regular files under the large_file feature; for directories
    /// it's genuinely the (unused, by us) ACL block pointer, so it's only
    /// read/written when this inode is a regular file.
    fn size(&self) -> u64 {
        let size_lo = u32::from_le_bytes(self.buf[4..8].try_into().unwrap());
        if self.is_reg() {
            let size_hi = u32::from_le_bytes(self.buf[108..112].try_into().unwrap());
            ((size_hi as u64) << 32) | size_lo as u64
        } else {
            size_lo as u64
        }
    }

    fn set_size(&mut self, v: u64) {
        self.buf[4..8].copy_from_slice(&((v & 0xFFFF_FFFF) as u32).to_le_bytes());
        if self.is_reg() {
            self.buf[108..112].copy_from_slice(&((v >> 32) as u32).to_le_bytes());
        }
    }

    fn i_block(&self, i: usize) -> u32 {
        let off = 40 + i * 4;
        u32::from_le_bytes(self.buf[off..off + 4].try_into().unwrap())
    }

    fn set_i_block(&mut self, i: usize, v: u32) {
        let off = 40 + i * 4;
        self.buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn is_dir(&self) -> bool {
        (self.i_mode() & 0xF000) == 0x4000
    }

    fn is_reg(&self) -> bool {
        (self.i_mode() & 0xF000) == 0x8000
    }
}

// ── VFS glue ─────────────────────────────────────────────────────────────────

pub struct Ext2FsHandle;

impl Filesystem for Ext2FsHandle {
    fn name(&self) -> &str { "ext2" }

    fn root(&self) -> Arc<dyn Inode> {
        Arc::new(Ext2Inode::new(ROOT_INO))
    }
}

struct Ext2Inode {
    ino: u32,
    raw: RawInode,
}

impl Ext2Inode {
    fn new(ino: u32) -> Self {
        let raw = fs().read_inode(ino);
        Self { ino, raw }
    }
}

impl Inode for Ext2Inode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        if self.raw.is_dir() {
            Stat::dir(self.ino as u64)
        } else {
            Stat::regular_writable(self.ino as u64, self.raw.size() as i64)
        }
    }

    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        if self.raw.is_dir() {
            if flags.is_write() {
                return Err(Errno::EISDIR);
            }
            let entries = fs().read_dir_entries(&self.raw);
            Ok(Box::new(Ext2DirHandle { ino: self.ino, entries, offset: 0 }))
        } else {
            let mut raw = self.raw.clone();
            if flags.is_write() && flags.0 & OpenFlags::TRUNC.0 != 0 {
                fs().truncate_to_zero(self.ino, &mut raw);
            }
            let start_offset = if flags.0 & OpenFlags::APPEND.0 != 0 {
                raw.size() as usize
            } else {
                0
            };
            Ok(Box::new(Ext2FileHandle {
                ino: self.ino,
                raw: Arc::new(Mutex::new(raw)),
                offset: Arc::new(Mutex::new(start_offset)),
            }))
        }
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        if !self.raw.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        fs().read_dir_entries(&self.raw)
            .into_iter()
            .find(|e| e.name == name)
            .map(|e| Arc::new(Ext2Inode::new(e.ino)) as Arc<dyn Inode>)
            .ok_or(Errno::ENOENT)
    }

    fn readdir(&self, offset: u64) -> Result<Option<DirEntry>, Errno> {
        if !self.raw.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        match offset {
            0 => Ok(Some(DirEntry::new(self.ino as u64, FileType::Directory, b"."))),
            1 => Ok(Some(DirEntry::new(self.ino as u64, FileType::Directory, b".."))),
            n => {
                let entries = fs().read_dir_entries(&self.raw);
                let idx = (n - 2) as usize;
                Ok(entries.get(idx).map(|e| DirEntry::new(e.ino as u64, e.kind, e.name.as_bytes())))
            }
        }
    }

    fn create(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        if !self.raw.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        if let Ok(existing) = self.lookup(name) {
            if existing.file_type() == FileType::Directory {
                return Err(Errno::EISDIR);
            }
            return Ok(existing);
        }

        let f = fs();
        let new_ino = f.alloc_inode(false).ok_or(Errno::ENOSPC)?;
        let mut new_raw = RawInode::zeroed(f.inode_size as usize);
        new_raw.set_i_mode(0x8000 | 0o644);
        new_raw.set_links_count(1);
        f.write_inode(new_ino, &new_raw);

        let mut dir_raw = self.raw.clone();
        if let Err(e) = f.add_dir_entry(self.ino, &mut dir_raw, name, new_ino, FileType::Regular) {
            f.free_inode(new_ino, false);
            return Err(e);
        }
        Ok(Arc::new(Ext2Inode::new(new_ino)))
    }

    fn mkdir(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        if !self.raw.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        if self.lookup(name).is_ok() {
            return Err(Errno::EEXIST);
        }

        let f = fs();
        let new_ino = f.alloc_inode(true).ok_or(Errno::ENOSPC)?;
        let new_block = match f.alloc_block() {
            Some(b) => b,
            None => { f.free_inode(new_ino, true); return Err(Errno::ENOSPC); }
        };

        let mut new_raw = RawInode::zeroed(f.inode_size as usize);
        new_raw.set_i_mode(0x4000 | 0o755);
        new_raw.set_links_count(2);
        new_raw.set_i_block(0, new_block);
        new_raw.set_size(f.block_size as u64);

        let bs = f.block_size as usize;
        let mut buf = alloc::vec![0u8; bs];
        let dot_len = dirent_len(1);
        write_dirent(&mut buf[0..dot_len], new_ino, dot_len as u16, ".", FileType::Directory);
        let remaining = bs - dot_len;
        write_dirent(&mut buf[dot_len..dot_len + remaining], self.ino, remaining as u16, "..", FileType::Directory);
        f.write_block(new_block, &buf);
        f.write_inode(new_ino, &new_raw);

        let mut dir_raw = self.raw.clone();
        if let Err(e) = f.add_dir_entry(self.ino, &mut dir_raw, name, new_ino, FileType::Directory) {
            f.free_block(new_block);
            f.free_inode(new_ino, true);
            return Err(e);
        }
        // The new subdirectory's ".." counts as a link to this parent.
        let mut parent_raw = dir_raw;
        parent_raw.set_links_count(parent_raw.links_count() + 1);
        f.write_inode(self.ino, &parent_raw);

        Ok(Arc::new(Ext2Inode::new(new_ino)))
    }

    fn unlink(&self, name: &str) -> Result<(), Errno> {
        if !self.raw.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        let child = self.lookup(name)?;
        if child.file_type() == FileType::Directory {
            return Err(Errno::EISDIR);
        }

        let f = fs();
        let dir_raw = self.raw.clone();
        let (child_ino, _kind) = f.remove_dir_entry(&dir_raw, name)?;

        let mut child_raw = f.read_inode(child_ino);
        let links = child_raw.links_count().saturating_sub(1);
        child_raw.set_links_count(links);
        if links == 0 {
            f.free_all_blocks(&mut child_raw);
            f.free_inode(child_ino, false);
        } else {
            f.write_inode(child_ino, &child_raw);
        }
        Ok(())
    }

    fn rmdir(&self, name: &str) -> Result<(), Errno> {
        if !self.raw.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        let child = self.lookup(name)?;
        if child.file_type() != FileType::Directory {
            return Err(Errno::ENOTDIR);
        }
        // offset 2 is the first entry past "." and ".." — Ok(None) there
        // means the directory holds nothing else.
        if child.readdir(2)?.is_some() {
            return Err(Errno::ENOTEMPTY);
        }

        let f = fs();
        let dir_raw = self.raw.clone();
        let (child_ino, _kind) = f.remove_dir_entry(&dir_raw, name)?;

        let mut child_raw = f.read_inode(child_ino);
        f.free_all_blocks(&mut child_raw);
        f.free_inode(child_ino, true);

        // This directory loses the link the removed child's ".." held.
        let mut parent_raw = self.raw.clone();
        parent_raw.set_links_count(parent_raw.links_count().saturating_sub(1));
        f.write_inode(self.ino, &parent_raw);
        Ok(())
    }

    fn take_child(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        if !self.raw.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        let child = self.lookup(name)?;
        let f = fs();
        let dir_raw = self.raw.clone();
        let (_child_ino, kind) = f.remove_dir_entry(&dir_raw, name)?;
        if kind == FileType::Directory {
            let mut parent_raw = self.raw.clone();
            parent_raw.set_links_count(parent_raw.links_count().saturating_sub(1));
            f.write_inode(self.ino, &parent_raw);
        }
        Ok(child)
    }

    fn insert_child(&self, name: &str, node: Arc<dyn Inode>) -> Result<(), Errno> {
        if !self.raw.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        if self.lookup(name).is_ok() {
            return Err(Errno::EEXIST);
        }
        // ext2 dirents can only reference ext2 inode numbers — refuse
        // (matches vfs::rename's documented "no cross-filesystem support")
        // rather than risk writing a dirent that points at whatever inode
        // number happens to collide in a foreign filesystem.
        let kind = node.file_type();
        let Some(ext2_node) = node.as_any().downcast_ref::<Ext2Inode>() else {
            return Err(Errno::ENOSYS);
        };

        let f = fs();
        let mut dir_raw = self.raw.clone();
        f.add_dir_entry(self.ino, &mut dir_raw, name, ext2_node.ino, kind)?;

        if kind == FileType::Directory {
            f.set_dotdot(&ext2_node.raw, self.ino)?;
            let mut parent_raw = dir_raw;
            parent_raw.set_links_count(parent_raw.links_count() + 1);
            f.write_inode(self.ino, &parent_raw);
        }
        Ok(())
    }
}

// ── Open file handles ────────────────────────────────────────────────────────

struct Ext2FileHandle {
    ino: u32,
    // Arc'd so dup()/dup2() see a growing/truncating write done through a
    // sibling fd — same "one true open file description" reasoning as the
    // offset below, just extended to size/block-pointer state too, since a
    // write can change both.
    raw: Arc<Mutex<RawInode>>,
    offset: Arc<Mutex<usize>>,
}

impl FileHandle for Ext2FileHandle {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        let raw = self.raw.lock();
        let size = raw.size() as usize;
        let mut offset = self.offset.lock();
        if *offset >= size {
            return Ok(0);
        }
        let n = buf.len().min(size - *offset);
        fs().read_file_range(&raw, *offset, &mut buf[..n]);
        *offset += n;
        Ok(n)
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        let mut raw = self.raw.lock();
        let mut offset = self.offset.lock();
        match fs().write_file_range(self.ino, &mut raw, *offset, buf) {
            Ok(n) => { *offset += n; Ok(n) }
            Err(Errno::ENOSPC) => Err(FileError::NoSpace),
            Err(_) => Err(FileError::IOError),
        }
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::regular_writable(self.ino as u64, self.raw.lock().size() as i64))
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(Ext2FileHandle {
            ino: self.ino,
            raw: self.raw.clone(),
            offset: self.offset.clone(),
        }))
    }

    fn seek(&mut self, offset: i64, whence: i32) -> FileResult<i64> {
        let mut cur = self.offset.lock();
        let size = self.raw.lock().size() as i64;
        let new_pos = crate::process::file::compute_seek(*cur as i64, size, offset, whence)?;
        *cur = new_pos as usize;
        Ok(new_pos)
    }

    fn name(&self) -> &str { "ext2" }
}

struct Ext2DirHandle {
    ino: u32,
    entries: Vec<Ext2DirEntry>,
    offset: usize,
}

impl FileHandle for Ext2DirHandle {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument) // directories use getdents64
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument)
    }

    fn getdents64(&mut self, buf: &mut [u8]) -> i64 {
        let mut written = 0usize;
        let synthetic = [
            DirEntry::new(self.ino as u64, FileType::Directory, b"."),
            DirEntry::new(self.ino as u64, FileType::Directory, b".."),
        ];

        loop {
            let entry = if self.offset < synthetic.len() {
                let e = &synthetic[self.offset];
                DirEntry::new(e.ino, e.kind, &e.name[..e.name_len])
            } else {
                let idx = self.offset - synthetic.len();
                match self.entries.get(idx) {
                    Some(e) => DirEntry::new(e.ino as u64, e.kind, e.name.as_bytes()),
                    None => break,
                }
            };

            let needed = entry.dirent64_size();
            if written + needed > buf.len() {
                break;
            }
            let next_off = self.offset as i64 + 1;
            entry.write_dirent64(next_off, &mut buf[written..written + needed]);
            written += needed;
            self.offset += 1;
        }

        written as i64
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::dir(self.ino as u64))
    }

    fn name(&self) -> &str { "ext2/dir" }
}
