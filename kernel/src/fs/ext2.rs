// kernel/src/fs/ext2.rs
//
// Read-only ext2, mounted at /mnt over the ATA disk (block::ata) attached
// to the secondary IDE channel — see src/main.rs for how that disk image
// gets created and attached, and scripts docs there for how its content is
// seeded via `mke2fs -d`.
//
// SCOPE (deliberately not a full implementation — see the project's
// "por implementar" notes): read-only. No write support (no block/inode
// allocation, no bitmap updates) — that's real complexity (crash-safety,
// allocator correctness) saved for later, once this read path has proven
// itself. Direct, singly-indirect, and doubly-indirect blocks (see
// `block_for_index`) — up to ptrs_per_block² + ptrs_per_block + 12 blocks,
// 64MiB+ at this driver's 1024-byte block size — triply-indirect isn't
// implemented (nothing this image ships needs a file bigger than that);
// a file needing that simply reads short rather than corrupting anything.
//
// Requires `s_feature_incompat` to only have FILETYPE set — anything else
// (in particular EXTENTS, i.e. an ext4 image) would misinterpret i_block
// completely, so mounting refuses outright rather than guess.

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
    inodes_per_group: u32,
    inode_size: u16,
    bgdt_block: u32,
    num_groups: u32,
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
            inodes_per_group: inodes_per_group.max(1),
            inode_size: if inode_size == 0 { 128 } else { inode_size },
            bgdt_block,
            num_groups: num_groups.max(1),
        })
    }

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

    /// Locate and read the inode table block containing `ino`, returning
    /// the raw `RawInode` fields we care about.
    ///
    /// `ino` should always be a value read out of this same filesystem (a
    /// directory entry, or the well-known root inode 2) — bounds-checked
    /// against the superblock's own counts as a corruption tripwire, not
    /// because callers are expected to pass arbitrary numbers.
    fn read_inode(&self, ino: u32) -> RawInode {
        debug_assert!(ino >= 1 && ino <= self.inodes_count, "ext2: inode {} out of range", ino);
        let group = (ino - 1) / self.inodes_per_group;
        debug_assert!(group < self.num_groups, "ext2: inode {} maps to out-of-range group {}", ino, group);
        let index_in_group = (ino - 1) % self.inodes_per_group;

        // Block Group Descriptor for `group` (32 bytes each). bg_inode_table
        // is the THIRD u32 field (bg_block_bitmap, bg_inode_bitmap, then
        // bg_inode_table) — +8 bytes into the descriptor, not +0.
        let bgd_per_block = self.block_size / 32;
        let bgd_block = self.bgdt_block + group / bgd_per_block;
        let bgd_offset = ((group % bgd_per_block) * 32) as usize + 8;
        let bgd_buf = self.block_vec(bgd_block);
        let inode_table_block = u32::from_le_bytes(bgd_buf[bgd_offset..bgd_offset + 4].try_into().unwrap());

        let inodes_per_block = self.block_size / self.inode_size as u32;
        let table_block = inode_table_block + index_in_group / inodes_per_block;
        let offset_in_block = ((index_in_group % inodes_per_block) * self.inode_size as u32) as usize;

        let block_buf = self.block_vec(table_block);
        RawInode::parse(&block_buf[offset_in_block..offset_in_block + 128])
    }

    /// Map a file-relative block index to a filesystem block number.
    /// Direct (0..12), singly-indirect, and doubly-indirect — returns
    /// `None` beyond that (see module doc comment: triply-indirect would
    /// only matter for files bigger than doubly-indirect's own reach,
    /// ptrs_per_block² blocks — 64MiB+ at the 1024-byte block size this
    /// driver is built around — not needed by anything this image ships).
    fn block_for_index(&self, raw: &RawInode, index: u32) -> Option<u32> {
        if index < 12 {
            let b = raw.i_block[index as usize];
            return if b == 0 { None } else { Some(b) };
        }
        let ptrs_per_block = self.block_size / 4;

        let indirect_index = index - 12;
        if indirect_index < ptrs_per_block {
            let indirect_block = raw.i_block[12];
            if indirect_block == 0 {
                return None;
            }
            return self.read_block_ptr(indirect_block, indirect_index);
        }

        let dbl_index = indirect_index - ptrs_per_block;
        let dbl_capacity = ptrs_per_block * ptrs_per_block;
        if dbl_index < dbl_capacity {
            let dbl_indirect_block = raw.i_block[13];
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

    /// Parse every directory entry out of `raw`'s data blocks (direct +
    /// singly-indirect, same limit as file reads).
    fn read_dir_entries(&self, raw: &RawInode) -> Vec<Ext2DirEntry> {
        let mut entries = Vec::new();
        let bs = self.block_size;
        let num_blocks = (raw.size + bs as u64 - 1) / bs as u64;

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

struct Ext2DirEntry {
    ino: u32,
    kind: FileType,
    name: String,
}

// ── Raw inode (subset of fields we use) ─────────────────────────────────────

#[derive(Clone)]
struct RawInode {
    i_mode: u16,
    size: u64,
    i_block: [u32; 15],
}

impl RawInode {
    fn parse(buf: &[u8]) -> Self {
        let i_mode = u16::from_le_bytes(buf[0..2].try_into().unwrap());
        let size_lo = u32::from_le_bytes(buf[4..8].try_into().unwrap());
        let size_hi = u32::from_le_bytes(buf[108..112].try_into().unwrap());
        // size_hi (i_dir_acl / i_size_high) only means "upper size bits" for
        // regular files under the large_file feature; for directories it's
        // genuinely the (unused, by us) ACL block pointer. Only regular
        // files can plausibly be big enough for this to matter here.
        let is_reg = (i_mode & 0xF000) == 0x8000;
        let size = if is_reg {
            ((size_hi as u64) << 32) | size_lo as u64
        } else {
            size_lo as u64
        };
        let mut i_block = [0u32; 15];
        for i in 0..15 {
            let off = 40 + i * 4;
            i_block[i] = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        }
        Self { i_mode, size, i_block }
    }

    fn is_dir(&self) -> bool {
        (self.i_mode & 0xF000) == 0x4000
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
    fn stat(&self) -> Stat {
        if self.raw.is_dir() {
            Stat::dir(self.ino as u64)
        } else {
            Stat::regular(self.ino as u64, self.raw.size as i64)
        }
    }

    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        if flags.is_write() {
            return Err(Errno::EROFS);
        }
        if self.raw.is_dir() {
            let entries = fs().read_dir_entries(&self.raw);
            Ok(Box::new(Ext2DirHandle { ino: self.ino, entries, offset: 0 }))
        } else {
            Ok(Box::new(Ext2FileHandle { raw: self.raw.clone(), offset: Arc::new(Mutex::new(0)) }))
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
}

// ── Open file handles ────────────────────────────────────────────────────────

struct Ext2FileHandle {
    raw: RawInode,
    // Arc'd so dup()/dup2() can share one true "open file description"
    // position between two fds — same reasoning as ramfs's RamFileHandle.
    offset: Arc<Mutex<usize>>,
}

impl FileHandle for Ext2FileHandle {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        let size = self.raw.size as usize;
        let mut offset = self.offset.lock();
        if *offset >= size {
            return Ok(0);
        }
        let n = buf.len().min(size - *offset);
        fs().read_file_range(&self.raw, *offset, &mut buf[..n]);
        *offset += n;
        Ok(n)
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::NotSupported)
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::regular(0, self.raw.size as i64))
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(Ext2FileHandle {
            raw: self.raw.clone(),
            offset: self.offset.clone(),
        }))
    }

    fn seek(&mut self, offset: i64, whence: i32) -> FileResult<i64> {
        let mut cur = self.offset.lock();
        let new_pos = crate::process::file::compute_seek(*cur as i64, self.raw.size as i64, offset, whence)?;
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
