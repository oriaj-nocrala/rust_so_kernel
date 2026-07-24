// kernel/src/fs/ext2.rs
//
// Read-write ext2, mounted at /mnt over a `hal::block::BlockDevice` — at
// real boot that's `crate::block::AtaBlockDevice`, wrapping the ATA disk
// (block::ata) attached to the secondary IDE channel (see src/main.rs for
// how that disk image gets created and attached, and scripts docs there for
// how its content is seeded via `mke2fs -d`). Every disk access in this
// file goes through `Ext2Fs::device` (`self.core.device.read_sectors`/
// `write_sectors`), not `block::ata` directly — this is what lets the QEMU
// integration test (`kernel/src/hw_tests.rs`) mount an entirely different
// `BlockDevice` (`hal::block::MemDisk`, a hand-built image, see
// `build_minimal_image` below) and exercise this same read-write path with
// zero risk to the real disk.img. See `hal/src/block.rs`'s module doc
// comment for why the seam speaks in raw 512-byte sectors rather than
// filesystem blocks, and `docs/drivers/architecture.md`'s storage-stack
// section for the bigger picture.
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
// Direct, singly-, doubly-, and triply-indirect blocks are all implemented
// (see `block_for_index`/`block_for_index_alloc`) — up to ptrs_per_block³ +
// ptrs_per_block² + ptrs_per_block + 12 blocks, ~16 GiB+ at this driver's
// 1024-byte block size (`EFBIG` beyond that is now purely theoretical: no
// disk image this kernel builds is anywhere near that size).
//
// ext2-native symlinks ARE implemented (`Ext2Inode::symlink`/`readlink`),
// matching real ext2's own two on-disk representations: "fast" (target
// under 60 bytes, stored directly in the inode's `i_block` array, no data
// block ever allocated) when it fits, "slow" (ordinary file content,
// exactly like a regular file) otherwise. This driver always *writes*
// whichever representation fits, and reads both — a real `mke2fs`/host-
// authored image may contain either.
//
// Permission bits: `Ext2Inode::stat()` reports the real on-disk `i_mode`
// permission bits (not a hardcoded per-filesystem constant like every
// other filesystem here — see `fs::types::Stat`'s doc comments) and
// `Ext2Inode::chmod`/`Ext2FileHandle::chmod` persist real changes to them.
// New files/dirs still get a fixed initial mode (`create`/`mkdir` have no
// caller-supplied mode to honor — `sys_open`/`sys_mkdir` don't take one at
// all, see their doc comments in `process/syscall/fs.rs`), but that mode
// is now correctly round-tripped through `stat()` afterward, and `chmod`
// can change it for real.
//
// Requires `s_feature_incompat` to only have FILETYPE set — anything else
// (in particular EXTENTS, i.e. an ext4 image) would misinterpret i_block
// completely, so mounting refuses outright rather than guess. FILETYPE is
// also what makes the on-disk dirent file_type byte meaningful, which the
// write path relies on when creating new entries.
//
// ROBUSTNESS
// ──────────
// Every method that touches disk propagates ATA I/O failures as
// `Errno::EIO` (via `read_block`/`write_block`) instead of panicking —
// including `Filesystem::root()` (`vfs.rs`'s `Filesystem` trait makes this
// `Result`-returning specifically so ext2 can propagate a real read
// failure cleanly, even though it's re-invoked on *every* `/mnt` path
// resolution, not just at mount time — see `vfs.rs`'s `resolve_inner`).
//
// A single coarse `EXT2_LOCK` (`spin::Mutex<()>`) is held across every
// mutating operation (`create`/`mkdir`/`unlink`/`rmdir`/`take_child`/
// `insert_child`/truncate-on-open/`Ext2FileHandle::write`) — without it,
// two processes racing `alloc_block`/`alloc_inode`'s read-bitmap-then-
// write-bitmap sequence (this kernel is preemptible; syscalls run with
// interrupts enabled) could both see the same clear bit and silently
// double-allocate a block or inode. Read-only paths (`lookup`/`readdir`/
// `open` for reading) don't take it: besides being unnecessary for the
// bitmap race specifically, `lookup` is called internally by every
// mutating method above *while already holding the lock*, and
// `spin::Mutex` isn't reentrant — locking there would deadlock.
//
// `read_block`/`write_block` reject any block number `>= blocks_count`
// before ever issuing the ATA command — this is the single choke point
// every on-disk pointer (BGD block/inode-table pointers, direct/indirect
// `i_block` entries) flows through before being trusted, so it catches a
// corrupted pointer wherever it originated instead of needing a bounds
// check at every call site. `inode_location`, `free_block`, and
// `free_inode` additionally validate their own `ino`/`block_num` inputs
// *before* subtracting (a corrupt value below `first_data_block`/`1`
// would otherwise underflow the `u32` group/bit computation — a panic in
// debug builds, a wraparound to a wrong-but-in-range group in release).
//
// Crash consistency: this driver still keeps no journal (see SCOPE above)
// — a power loss mid multi-step operation can still leak an allocated
// block/inode that never got linked into any inode/directory. What *is*
// handled: every multi-step mutation in this file already orders its
// writes "allocate & write content, then link" (never the reverse), so
// the only failure mode a crash can produce is an unreachable-but-still-
// marked-used block/inode (a leak) — never a dangling pointer into freed/
// reused space. Two mount-time passes clean up after exactly that failure
// mode, both run from `init()` before `/mnt` is exposed to the VFS, both
// deliberately mirroring what real `e2fsck` does most often in practice:
//   - `Ext2Fs::reconcile_free_counts` — the free block/inode *counters*
//     (BGD + superblock) are separate, independently-flushed writes from
//     the bitmaps they summarize, so a crash between the two leaves them
//     drifted ("Free blocks count wrong for group #N... FIXED"). Recomputes
//     the true counts directly from the bitmaps and corrects any mismatch.
//   - `Ext2Fs::reclaim_orphans` — walks every inode actually reachable
//     from the root directory (`mark_reachable`, reusing the same
//     `visit_inode_blocks` tree-walk `free_all_blocks` uses) and frees any
//     block/inode the bitmaps mark used that the walk never reached: real
//     e2fsck's passes 1-4 (build the "should be used" picture from the
//     directory tree, reconcile it against what the bitmaps claim), just
//     without the deeper structural checks (bad mode bits, cross-linked
//     blocks, etc.) a full e2fsck also performs. This is what actually
//     reclaims a block/inode a crash left allocated-but-never-linked —
//     the one concrete gap the paragraph above used to describe as
//     unrecoverable "in principle."

use alloc::{boxed::Box, string::String, string::ToString, sync::Arc, vec::Vec};
use spin::{Mutex, Once};

use crate::block::{BlockDevice, SECTOR_SIZE};

use crate::fs::{
    types::{DirEntry, Errno, FileType, OpenFlags, Stat},
    vfs::{Filesystem, Inode},
};
use crate::process::file::{FileError, FileHandle, FileResult};

// ── ext2 core crate ─────────────────────────────────────────────────────────
//
// On-disk structs + parsing (superblock, block group descriptor, raw inode
// record) and block/inode allocation + free-count bookkeeping now live in
// the standalone, host-testable `ext2` crate (`ext2/src/`, `cd ext2 &&
// cargo test`) — see `docs/fs/ext2-extraction-plan.md` (migration steps 1
// and 2). Everything else in this file (indirect block addressing,
// directory operations, mount + its repair passes) is unmigrated and stays
// here, per that plan's steps 3-6.
//
// `RawInode`/`BgdRaw` are re-exported/aliased here (not redefined) so every
// existing call site in this file (`RawInode::parse(...)`, `bgd.
// block_bitmap`, etc.) keeps working unchanged — only the *type
// definitions* moved, not how they're used.
use ext2::RawInode;
use ext2::BlockGroupDesc as BgdRaw;
use ext2::{EXT2_MAGIC, ROOT_INO};

impl From<ext2::Ext2Error> for Errno {
    fn from(e: ext2::Ext2Error) -> Self {
        match e {
            // BadMagic/UnsupportedFeature only ever occur inside
            // `Ext2Core::mount()`, which this adapter's own `Ext2Fs::mount()`
            // maps to a `&'static str` directly (see below) rather than
            // through this impl — EIO is a reasonable fallback all the same,
            // since every one of these is fundamentally "the disk didn't
            // give us what we expected."
            ext2::Ext2Error::Io | ext2::Ext2Error::BadMagic | ext2::Ext2Error::UnsupportedFeature => Errno::EIO,
        }
    }
}

// ── Global mount state ──────────────────────────────────────────────────────
//
// Only one ext2 disk is ever mounted, so a global (rather than plumbing an
// Arc<Ext2Fs> through every Inode — see ramfs.rs's RamDirNode for why that
// self-reference is awkward without one) keeps this simple. Matches the
// existing BUDDY/SCHEDULERS/KEYBOARD_BUFFER style already used throughout
// the kernel for singleton state.

static EXT2: Once<Ext2Fs> = Once::new();

/// Serializes every mutating ext2 operation — see the module-level
/// ROBUSTNESS doc comment for why this exists and why read-only paths
/// don't take it.
static EXT2_LOCK: Mutex<()> = Mutex::new(());

/// Mount the ext2 filesystem from the real ATA disk (`crate::block::
/// AtaBlockDevice`). Call once, before the VFS mounts `/mnt`. Returns `Err`
/// (not panics) on any problem — a missing or unreadable disk shouldn't
/// take down boot, just leave `/mnt` unmounted.
pub fn init() -> Result<(), &'static str> {
    let device: Box<dyn BlockDevice> = Box::new(crate::block::AtaBlockDevice);
    if !device.present() {
        return Err("no disk on the secondary IDE channel");
    }
    mount_and_repair(device)
}

/// Alternate entry point used only by the QEMU integration test
/// (`kernel/src/hw_tests.rs`): mounts ext2 against an arbitrary
/// `BlockDevice` — a `hal::block::MemDisk` backed by a hand-built image
/// (`build_minimal_image` below) in practice — instead of the real ATA
/// disk. This is the whole point of the `BlockDevice` seam: exercising
/// ext2's create/mkdir/unlink/rename/symlink path end to end with zero risk
/// to the real `disk.img`. Real boot always goes through `init()` above.
/// `#[cfg(test)]` because it's only ever called from `hw_tests.rs`, which
/// is itself `#[cfg(test)]`-only (see `main.rs`).
#[cfg(test)]
pub(crate) fn init_with_device(device: Box<dyn BlockDevice>) -> Result<(), &'static str> {
    if !device.present() {
        return Err("block device not present");
    }
    mount_and_repair(device)
}

/// Shared tail of `init()`/`init_with_device()`: parse the superblock,
/// repair any drift an unclean shutdown left behind, and publish the
/// result as the global singleton. Split out so both entry points run the
/// exact same mount-time repair sequence — nothing here is ATA-specific.
fn mount_and_repair(device: Box<dyn BlockDevice>) -> Result<(), &'static str> {
    let fs = Ext2Fs::mount(device)?;
    // Repair any free-count drift left by an unclean shutdown before this
    // filesystem is exposed to the VFS — see `reconcile_free_counts`'s doc
    // comment for why this matters beyond cosmetics.
    fs.reconcile_free_counts()
        .map_err(|_| "ext2: mount-time free-count reconciliation failed (I/O error)")?;
    // Reclaim any block/inode an unclean shutdown left allocated but never
    // linked into the directory tree — see `reclaim_orphans`'s doc
    // comment. Also before `/mnt` is exposed to the VFS: if this can't
    // complete safely (I/O error, or a directory tree deeper than its
    // guard), refuse the mount rather than risk sweeping against an
    // incomplete picture of what's actually in use.
    fs.reclaim_orphans()
        .map_err(|_| "ext2: mount-time orphan reclaim failed (I/O error or directory tree too deep)")?;
    EXT2.call_once(|| fs);
    Ok(())
}

fn fs() -> &'static Ext2Fs {
    EXT2.get().expect("fs::ext2::fs() called before init()")
}

// ── Superblock / filesystem-wide state ──────────────────────────────────────

/// Thin adapter over `ext2::Ext2Core` — see the "ext2 core crate" note
/// above. `core.sb` carries every geometry field this driver used to keep
/// as its own flat fields (`block_size`, `inodes_count`, ...); `core.
/// device` is the `BlockDevice` every sector read/write goes through
/// (`crate::block::AtaBlockDevice` at real boot, `hal::block::MemDisk`
/// under the QEMU integration test — see the module doc comment).
struct Ext2Fs {
    core: ext2::Ext2Core,
}

impl Ext2Fs {
    /// Parse the superblock (delegated to `ext2::Ext2Core::mount` —
    /// migration step 1) and construct the adapter. Does NOT run the
    /// mount-time repair passes (`reconcile_free_counts`/
    /// `reclaim_orphans`, migration step 5, unmigrated) — `mount_and_repair`
    /// above calls those right after this returns, before publishing the
    /// result anywhere shared. Error strings match exactly what this
    /// function used to produce inline, one per `ext2::Ext2Error` variant.
    fn mount(device: Box<dyn BlockDevice>) -> Result<Self, &'static str> {
        let core = ext2::Ext2Core::mount(device).map_err(|e| match e {
            ext2::Ext2Error::Io => "block device read of superblock failed",
            ext2::Ext2Error::BadMagic => "bad ext2 magic (not an ext2 filesystem, or wrong LBA)",
            ext2::Ext2Error::UnsupportedFeature => {
                "unsupported ext2 incompat features (ext4 extents? journal?) — refusing to mount"
            }
        })?;
        Ok(Self { core })
    }

    // ── Raw block I/O ────────────────────────────────────────────────────
    //
    // `read_block`/`block_vec`/`write_block` below are thin wrappers over
    // `ext2::Ext2Core` (migration step 1/2 — see the "ext2 core crate"
    // note near the top of this file); every other method in this file
    // keeps calling `self.read_block(...)`/`self.write_block(...)`/
    // `self.block_vec(...)` exactly as before, unaware anything moved.

    /// Read one filesystem block (`self.core.sb.block_size` bytes) into
    /// `buf`. Propagates an ATA failure as `Errno::EIO` instead of
    /// panicking — every caller in this file ultimately funnels a failure
    /// here up through the VFS's own `Result<_, Errno>` surface. Also rejects any
    /// `block_num` outside `0..blocks_count` — the single choke point
    /// every on-disk pointer passes through, so a corrupted BGD/inode
    /// pointer can't turn into a wild read at an arbitrary LBA (see the
    /// module-level ROBUSTNESS comment).
    fn read_block(&self, block_num: u32, buf: &mut [u8]) -> Result<(), Errno> {
        self.core.read_block(block_num, buf).map_err(Into::into)
    }

    fn block_vec(&self, block_num: u32) -> Result<Vec<u8>, Errno> {
        self.core.block_vec(block_num).map_err(Into::into)
    }

    /// Write one filesystem block (`self.core.sb.block_size` bytes) from
    /// `buf`. Same EIO-not-panic contract and out-of-range-`block_num`
    /// rejection as `read_block`.
    fn write_block(&self, block_num: u32, buf: &[u8]) -> Result<(), Errno> {
        self.core.write_block(block_num, buf).map_err(Into::into)
    }

    // ── Inode table ──────────────────────────────────────────────────────

    /// Locate the inode table block + byte offset for `ino` — shared by
    /// `read_inode` and `write_inode` so the two can never disagree about
    /// where an inode lives.
    fn inode_location(&self, ino: u32) -> Result<(u32, usize), Errno> {
        // Real check, not `debug_assert!` — this driver trusts `ino`
        // values read back out of directory entries on disk, so a
        // corrupted dirent must not reach the `ino - 1` subtraction below
        // (a `u32` underflow: a panic in debug builds, a wraparound to a
        // huge-but-in-range-looking group index in release).
        if ino < 1 || ino > self.core.sb.inodes_count {
            return Err(Errno::EIO);
        }
        let group = (ino - 1) / self.core.sb.inodes_per_group;
        if group >= self.core.sb.num_groups {
            return Err(Errno::EIO);
        }
        let index_in_group = (ino - 1) % self.core.sb.inodes_per_group;

        // Block Group Descriptor for `group` (32 bytes each). bg_inode_table
        // is the THIRD u32 field (bg_block_bitmap, bg_inode_bitmap, then
        // bg_inode_table) — +8 bytes into the descriptor, not +0.
        let (bgd_block, bgd_off) = self.bgd_location(group);
        let bgd_buf = self.block_vec(bgd_block)?;
        let inode_table_block = u32::from_le_bytes(bgd_buf[bgd_off + 8..bgd_off + 12].try_into().unwrap());

        let inodes_per_block = self.core.sb.block_size / self.core.sb.inode_size as u32;
        let table_block = inode_table_block + index_in_group / inodes_per_block;
        let offset_in_block = ((index_in_group % inodes_per_block) * self.core.sb.inode_size as u32) as usize;
        Ok((table_block, offset_in_block))
    }

    /// Read the raw on-disk inode record for `ino`.
    ///
    /// `ino` should always be a value read out of this same filesystem (a
    /// directory entry, or the well-known root inode 2) — bounds-checked
    /// against the superblock's own counts as a corruption tripwire, not
    /// because callers are expected to pass arbitrary numbers.
    fn read_inode(&self, ino: u32) -> Result<RawInode, Errno> {
        let (table_block, offset_in_block) = self.inode_location(ino)?;
        let block_buf = self.block_vec(table_block)?;
        Ok(RawInode::parse(&block_buf[offset_in_block..offset_in_block + self.core.sb.inode_size as usize]))
    }

    /// Write `raw` back to `ino`'s on-disk inode record. Read-modify-write:
    /// the inode table block holds several inodes, so the rest of the
    /// block must survive untouched.
    fn write_inode(&self, ino: u32, raw: &RawInode) -> Result<(), Errno> {
        let (table_block, offset_in_block) = self.inode_location(ino)?;
        let mut block_buf = self.block_vec(table_block)?;
        block_buf[offset_in_block..offset_in_block + self.core.sb.inode_size as usize].copy_from_slice(&raw.buf);
        self.write_block(table_block, &block_buf)
    }

    // ── Block-pointer mapping (read-only lookup) ────────────────────────

    /// Map a file-relative block index to a filesystem block number.
    /// Direct (0..12), singly-indirect, and doubly-indirect — returns
    /// `Ok(None)` if the block is a hole (not yet allocated) or beyond what
    /// this driver supports (see module doc comment).
    fn block_for_index(&self, raw: &RawInode, index: u32) -> Result<Option<u32>, Errno> {
        if index < 12 {
            let b = raw.i_block(index as usize);
            return Ok(if b == 0 { None } else { Some(b) });
        }
        let ptrs_per_block = self.core.sb.block_size / 4;

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
    fn read_block_ptr(&self, block_num: u32, index: u32) -> Result<Option<u32>, Errno> {
        let buf = self.block_vec(block_num)?;
        let off = (index * 4) as usize;
        let b = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        Ok(if b == 0 { None } else { Some(b) })
    }

    // ── Block-pointer mapping (allocate-on-demand) ──────────────────────

    /// Read (or allocate, if zero) the `index`-th pointer slot in an
    /// indirect/doubly-indirect pointer block, writing the new pointer back
    /// immediately — shared by both levels of `block_for_index_alloc`.
    fn get_or_alloc_ptr(&self, container_block: u32, index: u32) -> Result<u32, Errno> {
        let mut buf = self.block_vec(container_block)?;
        let off = (index * 4) as usize;
        let existing = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
        if existing != 0 {
            return Ok(existing);
        }
        let new_block = self.alloc_block()?.ok_or(Errno::ENOSPC)?;
        buf[off..off + 4].copy_from_slice(&new_block.to_le_bytes());
        self.write_block(container_block, &buf)?;
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
            let nb = self.alloc_block()?.ok_or(Errno::ENOSPC)?;
            raw.set_i_block(index as usize, nb);
            return Ok(nb);
        }

        let ptrs_per_block = self.core.sb.block_size / 4;
        let indirect_index = index - 12;
        if indirect_index < ptrs_per_block {
            let indirect_block = raw.i_block(12);
            let indirect_block = if indirect_block == 0 {
                let nb = self.alloc_block()?.ok_or(Errno::ENOSPC)?;
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
                let nb = self.alloc_block()?.ok_or(Errno::ENOSPC)?;
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
                let nb = self.alloc_block()?.ok_or(Errno::ENOSPC)?;
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

        Err(Errno::EFBIG) // beyond even triply-indirect capacity — genuinely unsupported
    }

    // ── File data read/write ─────────────────────────────────────────────

    /// Read `buf.len()` bytes of file data starting at byte `offset`.
    fn read_file_range(&self, raw: &RawInode, offset: usize, buf: &mut [u8]) -> Result<(), Errno> {
        let bs = self.core.sb.block_size as usize;
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
    fn write_file_range(&self, ino: u32, raw: &mut RawInode, offset: usize, data: &[u8]) -> Result<usize, Errno> {
        if data.is_empty() {
            return Ok(0); // true no-op — access(2)'s W_OK probe relies on this
        }

        let bs = self.core.sb.block_size as usize;
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

    /// Free every block this inode owns (direct, singly-, doubly-, and
    /// triply-indirect data + every pointer block along the way) and zero
    /// its size. Does NOT free the inode itself — callers decide that
    /// based on link count.
    ///
    /// Guarded by `has_block_pointers()`: a fast symlink's `i_block` bytes
    /// are inline text, not real pointers (see module doc comment) —
    /// walking them as if they were would try to "free" whatever garbage
    /// block numbers the text happens to decode to. Before this guard
    /// existed, `unlink()` on a fast symlink hit exactly that: the first
    /// four bytes of a target like `"realfile.txt"` decode to block
    /// `0x6C616572` (huge — safely rejected by `free_block`'s bounds
    /// check, see the module-level ROBUSTNESS comment — but the rejection
    /// itself made the whole `unlink()` fail with `EIO` instead of
    /// succeeding).
    fn free_all_blocks(&self, raw: &mut RawInode) -> Result<(), Errno> {
        if raw.has_block_pointers() {
            self.visit_inode_blocks(raw, |b| self.free_block(b))?;
            for i in 0..15 {
                raw.set_i_block(i, 0);
            }
        }
        raw.set_size(0);
        raw.set_blocks_512(0);
        Ok(())
    }

    /// Call `visit(block_num)` for every block number this inode owns —
    /// direct, singly-, doubly-, and triply-indirect, pointer blocks
    /// themselves as well as their leaf targets. Shared tree-walk shape
    /// behind both `free_all_blocks` (frees what it visits) and the
    /// mount-time orphan scan `reclaim_orphans`/`mark_reachable` (marks
    /// what it visits as reachable, never frees anything itself) — same
    /// traversal, different action per block, so the shape only needs to
    /// be right in one place.
    ///
    /// Callers are responsible for the `has_block_pointers()` guard (see
    /// `free_all_blocks`'s doc comment) — this function trusts `i_block`
    /// to hold real pointers unconditionally.
    fn visit_inode_blocks(&self, raw: &RawInode, mut visit: impl FnMut(u32) -> Result<(), Errno>) -> Result<(), Errno> {
        for i in 0..12 {
            let b = raw.i_block(i);
            if b != 0 {
                visit(b)?;
            }
        }

        let ptrs_per_block = self.core.sb.block_size / 4;

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
    fn visit_pointer_block_targets(&self, block_num: u32, visit: &mut impl FnMut(u32) -> Result<(), Errno>) -> Result<(), Errno> {
        let buf = self.block_vec(block_num)?;
        let ptrs_per_block = self.core.sb.block_size / 4;
        for idx in 0..ptrs_per_block {
            let off = (idx * 4) as usize;
            let b = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
            if b != 0 {
                visit(b)?;
            }
        }
        Ok(())
    }

    /// Truncate a file to zero length: frees all its data blocks and
    /// persists the now-empty inode. Backs `O_TRUNC`.
    fn truncate_to_zero(&self, ino: u32, raw: &mut RawInode) -> Result<(), Errno> {
        self.free_all_blocks(raw)?;
        self.write_inode(ino, raw)
    }

    /// Read a symlink inode's target string. Real ext2 has two on-disk
    /// representations, and this driver reads both (see module doc
    /// comment): "fast" — target under 60 bytes, stored directly in the
    /// inode's `i_block` bytes, no data block ever allocated — and "slow"
    /// — target stored as ordinary file content, same as a regular file.
    /// `size < 60` (not, say, `i_block(0) == 0`) is the only reliable way
    /// to tell them apart *while reading*: the fast representation's inline
    /// storage bytes physically alias `i_block`'s own byte range (that's
    /// the whole space-saving trick — see `Ext2Inode::symlink`), so a
    /// short target whose own bytes happen to decode to a nonzero
    /// `i_block(0)` would otherwise be misread as a slow symlink and its
    /// text bytes reinterpreted as real block pointers. `size < 60` has no
    /// such ambiguity: a target that size can only ever have been written
    /// as "fast" (60 bytes is the hard physical limit of the inline area,
    /// on any ext2 image, not just this driver's own writes), so this
    /// matches both this driver's own symlinks and a real `mke2fs`/host-
    /// authored image's.
    fn read_symlink_target(&self, raw: &RawInode) -> Result<String, Errno> {
        let size = raw.size() as usize;
        if raw.is_fast_symlink() {
            let bytes = &raw.buf[40..40 + size];
            return Ok(String::from_utf8_lossy(bytes).to_string());
        }
        let mut buf = alloc::vec![0u8; size];
        self.read_file_range(raw, 0, &mut buf)?;
        Ok(String::from_utf8_lossy(&buf).to_string())
    }

    // ── Directory entries ────────────────────────────────────────────────

    /// Parse every directory entry out of `raw`'s data blocks (direct +
    /// indirect, same limit as file reads).
    fn read_dir_entries(&self, raw: &RawInode) -> Result<Vec<Ext2DirEntry>, Errno> {
        let mut entries = Vec::new();
        let bs = self.core.sb.block_size;
        let num_blocks = (raw.size() + bs as u64 - 1) / bs as u64;

        for block_index in 0..num_blocks as u32 {
            let Some(block_num) = self.block_for_index(raw, block_index)? else { continue };
            let buf = self.block_vec(block_num)?;
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
        Ok(entries)
    }

    /// Insert a new `(name -> ino)` directory entry into `dir_raw`'s data,
    /// splitting an existing entry's slack space (real ext2's own
    /// approach) if one is big enough, or reusing a deleted (`inode == 0`)
    /// slot, or — only if nothing fits — allocating and appending a whole
    /// new directory block.
    fn add_dir_entry(&self, dir_ino: u32, dir_raw: &mut RawInode, name: &str, ino: u32, kind: FileType) -> Result<(), Errno> {
        let bs = self.core.sb.block_size as usize;
        let needed = dirent_len(name.len());
        let num_blocks = ((dir_raw.size() as usize) + bs - 1) / bs;

        for block_index in 0..num_blocks as u32 {
            let Some(block_num) = self.block_for_index(dir_raw, block_index)? else { continue };
            let mut buf = self.block_vec(block_num)?;
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
                    self.write_block(block_num, &buf)?;
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
        self.write_block(new_block, &buf)?;
        dir_raw.set_size((new_block_index as u64 + 1) * bs as u64);
        self.write_inode(dir_ino, dir_raw)?;
        Ok(())
    }

    /// Remove the directory entry named `name` from `dir_raw`'s data.
    /// Merges its `rec_len` into the previous entry in the same block
    /// (real ext2's approach), or — if it's the first entry in the block —
    /// just zeroes its inode field, leaving a reusable deleted slot.
    /// Returns the removed entry's inode number and kind.
    fn remove_dir_entry(&self, dir_raw: &RawInode, name: &str) -> Result<(u32, FileType), Errno> {
        let bs = self.core.sb.block_size as usize;
        let num_blocks = ((dir_raw.size() as usize) + bs - 1) / bs;

        for block_index in 0..num_blocks as u32 {
            let Some(block_num) = self.block_for_index(dir_raw, block_index)? else { continue };
            let mut buf = self.block_vec(block_num)?;
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
                    self.write_block(block_num, &buf)?;
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
        let Some(block_num) = self.block_for_index(dir_raw, 0)? else { return Err(Errno::EIO) };
        let mut buf = self.block_vec(block_num)?;
        let mut off = 0usize;
        while off + 8 <= buf.len() {
            let rec_len = u16::from_le_bytes(buf[off + 4..off + 6].try_into().unwrap()) as usize;
            if rec_len < 8 {
                break;
            }
            let name_len = buf[off + 6] as usize;
            if name_len == 2 && off + 8 + 2 <= buf.len() && &buf[off + 8..off + 10] == b".." {
                buf[off..off + 4].copy_from_slice(&new_parent_ino.to_le_bytes());
                self.write_block(block_num, &buf)?;
                return Ok(());
            }
            off += rec_len;
        }
        Err(Errno::EIO)
    }

    // ── Block group descriptors / bitmaps ───────────────────────────────

    fn bgd_location(&self, group: u32) -> (u32, usize) {
        let bgd_per_block = self.core.sb.block_size / 32;
        let bgd_block = self.core.sb.bgdt_block + group / bgd_per_block;
        let bgd_offset = ((group % bgd_per_block) * 32) as usize;
        (bgd_block, bgd_offset)
    }

    fn read_bgd(&self, group: u32) -> Result<BgdRaw, Errno> {
        let (blk, off) = self.bgd_location(group);
        let buf = self.block_vec(blk)?;
        Ok(BgdRaw {
            block_bitmap: u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()),
            inode_bitmap: u32::from_le_bytes(buf[off + 4..off + 8].try_into().unwrap()),
            inode_table: u32::from_le_bytes(buf[off + 8..off + 12].try_into().unwrap()),
            free_blocks: u16::from_le_bytes(buf[off + 12..off + 14].try_into().unwrap()),
            free_inodes: u16::from_le_bytes(buf[off + 14..off + 16].try_into().unwrap()),
        })
    }

    fn adjust_bgd_counts(&self, group: u32, free_blocks_delta: i32, free_inodes_delta: i32, used_dirs_delta: i32) -> Result<(), Errno> {
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
    /// (same as `mount()`) rather than keeping a cached copy, since this is
    /// the only mutable superblock state this driver tracks.
    fn adjust_sb_counts(&self, free_blocks_delta: i32, free_inodes_delta: i32) -> Result<(), Errno> {
        let mut raw = [0u8; 1024];
        self.core.device.read_sectors(2, 2, &mut raw).map_err(|_| Errno::EIO)?;
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
        self.core.device.write_sectors(2, 2, &raw).map_err(|_| Errno::EIO)
    }

    fn blocks_in_group(&self, group: u32) -> u32 {
        let start = self.core.sb.first_data_block + group * self.core.sb.blocks_per_group;
        self.core.sb.blocks_count.saturating_sub(start).min(self.core.sb.blocks_per_group)
    }

    fn inodes_in_group(&self, group: u32) -> u32 {
        let start = group * self.core.sb.inodes_per_group;
        self.core.sb.inodes_count.saturating_sub(start).min(self.core.sb.inodes_per_group)
    }

    /// Allocate a free data block: scan each group's block bitmap for a
    /// clear bit, set it, update the group + superblock free counts, and
    /// zero the block's content (so a demand-paging-style hole never
    /// exposes stale disk data). Returns `Ok(None)` when the filesystem is
    /// full (`ENOSPC`), `Err` on an I/O failure.
    fn alloc_block(&self) -> Result<Option<u32>, Errno> {
        for group in 0..self.core.sb.num_groups {
            let bgd = self.read_bgd(group)?;
            if bgd.free_blocks == 0 {
                continue;
            }
            let group_blocks = self.blocks_in_group(group);
            let mut bitmap = self.block_vec(bgd.block_bitmap)?;
            for bit in 0..group_blocks {
                let byte = (bit / 8) as usize;
                let mask = 1u8 << (bit % 8);
                if bitmap[byte] & mask == 0 {
                    bitmap[byte] |= mask;
                    self.write_block(bgd.block_bitmap, &bitmap)?;
                    self.adjust_bgd_counts(group, -1, 0, 0)?;
                    self.adjust_sb_counts(-1, 0)?;
                    let block_num = self.core.sb.first_data_block + group * self.core.sb.blocks_per_group + bit;
                    let zeros = alloc::vec![0u8; self.core.sb.block_size as usize];
                    self.write_block(block_num, &zeros)?;
                    return Ok(Some(block_num));
                }
            }
        }
        Ok(None)
    }

    fn free_block(&self, block_num: u32) -> Result<(), Errno> {
        // Validate before subtracting — `block_num` here always originates
        // from an on-disk `i_block`/indirect pointer (see callers in
        // `free_all_blocks`/`free_pointer_block_targets`), so a corrupted
        // value below `first_data_block` must not underflow the `u32`
        // group/bit computation below.
        if block_num < self.core.sb.first_data_block || block_num >= self.core.sb.blocks_count {
            return Err(Errno::EIO);
        }
        let group = (block_num - self.core.sb.first_data_block) / self.core.sb.blocks_per_group;
        let bit = (block_num - self.core.sb.first_data_block) % self.core.sb.blocks_per_group;
        let bgd = self.read_bgd(group)?;
        let mut bitmap = self.block_vec(bgd.block_bitmap)?;
        let byte = (bit / 8) as usize;
        let mask = 1u8 << (bit % 8);
        bitmap[byte] &= !mask;
        self.write_block(bgd.block_bitmap, &bitmap)?;
        self.adjust_bgd_counts(group, 1, 0, 0)?;
        self.adjust_sb_counts(1, 0)
    }

    /// Allocate a free inode. `is_dir` also bumps the group's directory
    /// count (`bg_used_dirs_count`) — cosmetic bookkeeping real ext2 tools
    /// (e2fsck, `df -i` equivalents) rely on, harmless if never read here.
    fn alloc_inode(&self, is_dir: bool) -> Result<Option<u32>, Errno> {
        for group in 0..self.core.sb.num_groups {
            let bgd = self.read_bgd(group)?;
            if bgd.free_inodes == 0 {
                continue;
            }
            let group_inodes = self.inodes_in_group(group);
            let mut bitmap = self.block_vec(bgd.inode_bitmap)?;
            for bit in 0..group_inodes {
                let byte = (bit / 8) as usize;
                let mask = 1u8 << (bit % 8);
                if bitmap[byte] & mask == 0 {
                    bitmap[byte] |= mask;
                    self.write_block(bgd.inode_bitmap, &bitmap)?;
                    self.adjust_bgd_counts(group, 0, -1, if is_dir { 1 } else { 0 })?;
                    self.adjust_sb_counts(0, -1)?;
                    return Ok(Some(group * self.core.sb.inodes_per_group + bit + 1));
                }
            }
        }
        Ok(None)
    }

    fn free_inode(&self, ino: u32, is_dir: bool) -> Result<(), Errno> {
        // Same corrupted-input-before-underflow guard as `free_block`.
        if ino < 1 || ino > self.core.sb.inodes_count {
            return Err(Errno::EIO);
        }
        let group = (ino - 1) / self.core.sb.inodes_per_group;
        let bit = (ino - 1) % self.core.sb.inodes_per_group;
        let bgd = self.read_bgd(group)?;
        let mut bitmap = self.block_vec(bgd.inode_bitmap)?;
        let byte = (bit / 8) as usize;
        let mask = 1u8 << (bit % 8);
        bitmap[byte] &= !mask;
        self.write_block(bgd.inode_bitmap, &bitmap)?;
        self.adjust_bgd_counts(group, 0, 1, if is_dir { -1 } else { 0 })?;
        self.adjust_sb_counts(0, 1)
    }

    // ── Mount-time consistency repair ───────────────────────────────────

    /// Recompute every group's true free block/inode counts directly from
    /// its bitmap and correct the stored BGD + superblock counters if they
    /// disagree. Called once from `init()`, before this filesystem is
    /// exposed to the VFS.
    ///
    /// Bitmap writes are always durable the instant they happen (this
    /// driver has no write-back cache), but the *counters* that track free
    /// space are separate, independently-flushed writes (see the module
    /// doc comment) — an unclean shutdown between a bitmap write and its
    /// matching counter update leaves the bitmap correct but the counter
    /// stale. Left unrepaired, that drift is a real correctness bug, not
    /// just cosmetic: `alloc_block`/`alloc_inode` use the counter as a
    /// fast "is this group full" pre-check, so a counter that's stuck too
    /// low makes them wrongly skip a group that actually has free bits,
    /// eventually surfacing as spurious `ENOSPC`. This is the same repair
    /// real `e2fsck` applies most often in practice; it does not attempt
    /// the harder problem of reclaiming blocks/inodes that a crash left
    /// allocated-but-unlinked (an orphan scan needs a full reachability
    /// walk from the root, which this driver doesn't implement) — that
    /// space just stays leaked until a real `e2fsck` run outside this
    /// kernel.
    fn reconcile_free_counts(&self) -> Result<(), Errno> {
        let mut sb_raw = [0u8; 1024];
        self.core.device.read_sectors(2, 2, &mut sb_raw).map_err(|_| Errno::EIO)?;
        let sb_free_blocks = u32::from_le_bytes(sb_raw[12..16].try_into().unwrap());
        let sb_free_inodes = u32::from_le_bytes(sb_raw[16..20].try_into().unwrap());

        let mut total_free_blocks: u32 = 0;
        let mut total_free_inodes: u32 = 0;

        for group in 0..self.core.sb.num_groups {
            let bgd = self.read_bgd(group)?;

            let block_bitmap = self.block_vec(bgd.block_bitmap)?;
            let real_free_blocks = count_free_bits(&block_bitmap, self.blocks_in_group(group));

            let inode_bitmap = self.block_vec(bgd.inode_bitmap)?;
            let real_free_inodes = count_free_bits(&inode_bitmap, self.inodes_in_group(group));

            if real_free_blocks != bgd.free_blocks || real_free_inodes != bgd.free_inodes {
                crate::ktrace!(
                    crate::debug::FS,
                    "ext2: group {} free-count drift (blocks {}->{}, inodes {}->{}), repairing",
                    group, bgd.free_blocks, real_free_blocks, bgd.free_inodes, real_free_inodes
                );
                self.adjust_bgd_counts(
                    group,
                    real_free_blocks as i32 - bgd.free_blocks as i32,
                    real_free_inodes as i32 - bgd.free_inodes as i32,
                    0,
                )?;
            }

            total_free_blocks += real_free_blocks as u32;
            total_free_inodes += real_free_inodes as u32;
        }

        if total_free_blocks != sb_free_blocks || total_free_inodes != sb_free_inodes {
            crate::ktrace!(
                crate::debug::FS,
                "ext2: superblock free-count drift (blocks {}->{}, inodes {}->{}), repairing",
                sb_free_blocks, total_free_blocks, sb_free_inodes, total_free_inodes
            );
            self.adjust_sb_counts(
                total_free_blocks as i32 - sb_free_blocks as i32,
                total_free_inodes as i32 - sb_free_inodes as i32,
            )?;
        }

        Ok(())
    }

    /// Mount-time orphan scan — see the module-level ROBUSTNESS comment
    /// for the full rationale. Builds a "should be used" bitmap pair by
    /// walking every inode actually reachable from the root directory
    /// (fixed metadata + reserved inodes are seeded in as used up front,
    /// same convention real ext2 tools use), then clears any bit the real
    /// on-disk bitmaps mark used that the walk never reached.
    ///
    /// Safety-critical property: the sweep only ever runs if the walk
    /// completed with no error at all (`?` on every fallible step here
    /// means a single I/O failure or a directory tree deeper than the
    /// depth guard aborts the *whole* function before the sweep, via
    /// `mark_reachable`'s own `Err` return — never partially). An
    /// incomplete "should be used" picture must never be swept against,
    /// or a still-live block/inode could be freed out from under a file
    /// that's simply reached through a deep path.
    fn reclaim_orphans(&self) -> Result<(), Errno> {
        let block_bytes = ((self.core.sb.blocks_count as usize) + 7) / 8;
        let inode_bytes = ((self.core.sb.inodes_count as usize) + 7) / 8;
        let mut used_blocks = alloc::vec![0u8; block_bytes];
        let mut used_inodes = alloc::vec![0u8; inode_bytes];

        // Fixed metadata: boot block, the superblock itself, the
        // block-group descriptor table, and every group's own bitmaps +
        // inode table. None of this is owned by any inode, so the tree
        // walk below would never mark it, but it's legitimately in use.
        // The superblock lives AT block `first_data_block` (`bgdt_block`
        // above is computed as `first_data_block + 1`, i.e. right after
        // it) — `0..first_data_block` only covers the boot block ahead of
        // it, not the superblock's own block, so that block must be
        // included too (`0..=first_data_block`). Missing this let the
        // sweep below "reclaim" the superblock's block as an orphan on
        // first mount, and the very next allocation handed it out to real
        // file data, corrupting the superblock.
        for b in 0..=self.core.sb.first_data_block {
            mark_bit(&mut used_blocks, b);
        }
        let bgd_per_block = self.core.sb.block_size / 32;
        let bgdt_blocks = (self.core.sb.num_groups + bgd_per_block - 1) / bgd_per_block;
        for b in self.core.sb.bgdt_block..self.core.sb.bgdt_block + bgdt_blocks {
            mark_bit(&mut used_blocks, b);
        }
        let inodes_per_block = self.core.sb.block_size / self.core.sb.inode_size as u32;
        for group in 0..self.core.sb.num_groups {
            let bgd = self.read_bgd(group)?;
            mark_bit(&mut used_blocks, bgd.block_bitmap);
            mark_bit(&mut used_blocks, bgd.inode_bitmap);
            let inode_table_blocks = (self.core.sb.inodes_per_group + inodes_per_block - 1) / inodes_per_block;
            for b in bgd.inode_table..bgd.inode_table + inode_table_blocks {
                mark_bit(&mut used_blocks, b);
            }

            // Real ext2 (sparse_super, mke2fs's default) keeps backup
            // superblock+BGDT copies in group 0, group 1, and every group
            // whose number is a power of 3/5/7 — mirroring that placement
            // logic exactly here would be one more way to get it subtly
            // wrong, so instead just reserve every group's leading
            // `1 + bgdt_blocks` blocks unconditionally. Overkill for a
            // group that doesn't actually have a backup (those blocks were
            // never marked used in the real per-group bitmap to begin
            // with, so reserving them here is a no-op), but it means a
            // group that *does* have one — which this driver has no
            // reason to expect specifically — can never be misread as an
            // orphan and freed into real file data the way group 0's own
            // primary copy already was (see the loop above this one).
            let group_start = self.core.sb.first_data_block + group * self.core.sb.blocks_per_group;
            for b in group_start..group_start + 1 + bgdt_blocks {
                mark_bit(&mut used_blocks, b);
            }
        }

        // Walk the real directory tree from root, marking every inode and
        // block actually reachable. 64 levels of nesting is far beyond
        // anything a shell/script here would ever create — hitting it is
        // treated as a hard error (not a silent stop), since silently
        // under-marking a legitimately-deep subtree would make the sweep
        // below wrongly reclaim it.
        //
        // MUST run before the reserved-inode marking just below: root's
        // own ino (2) falls inside that reserved range, and
        // `mark_reachable`'s cycle guard treats an already-marked bit as
        // "already visited, nothing more to do" — pre-marking root first
        // used to make the very first call return immediately without
        // ever reading root's own blocks or descending into a single
        // child. That silently treated the *entire* real directory tree
        // (every file this filesystem was seeded with) as unreachable, so
        // the sweep below freed almost every real block/inode on the very
        // first mount — the very first new file/dir write after that
        // then handed out an already-live block to something else,
        // corrupting whatever legitimately owned it (this is what
        // produced the `add_dir_entry` "range end index ... out of range"
        // panic: the root directory's own data block had been reused for
        // unrelated file content).
        self.mark_reachable(ROOT_INO, &mut used_inodes, &mut used_blocks, 64)?;

        // Reserved inodes below `first_ino` (root's own ino=2 among them)
        // are always "in use" even though this driver never reaches most
        // of them (1, 3..=10 have no directory entry pointing at them at
        // all, ever) via the walk above.
        for ino in 1..self.core.sb.first_ino {
            mark_bit_1based(&mut used_inodes, ino);
        }

        // Sweep: anything the real bitmaps mark used that the walk above
        // never reached is an orphan — clear it. Counter bookkeeping is
        // deliberately NOT duplicated here: `reconcile_free_counts` (just
        // above) already knows how to recompute BGD/superblock free
        // counts from a bitmap, so just clear bits and re-run it once at
        // the end if anything actually changed.
        let mut freed_blocks: u32 = 0;
        let mut freed_inodes: u32 = 0;
        for group in 0..self.core.sb.num_groups {
            let bgd = self.read_bgd(group)?;

            let mut block_bitmap = self.block_vec(bgd.block_bitmap)?;
            let mut changed = false;
            for bit in 0..self.blocks_in_group(group) {
                let byte = (bit / 8) as usize;
                let mask = 1u8 << (bit % 8);
                if block_bitmap[byte] & mask == 0 {
                    continue;
                }
                let block_num = self.core.sb.first_data_block + group * self.core.sb.blocks_per_group + bit;
                if !bit_set(&used_blocks, block_num) {
                    block_bitmap[byte] &= !mask;
                    changed = true;
                    freed_blocks += 1;
                }
            }
            if changed {
                self.write_block(bgd.block_bitmap, &block_bitmap)?;
            }

            let mut inode_bitmap = self.block_vec(bgd.inode_bitmap)?;
            let mut ichanged = false;
            for bit in 0..self.inodes_in_group(group) {
                let byte = (bit / 8) as usize;
                let mask = 1u8 << (bit % 8);
                if inode_bitmap[byte] & mask == 0 {
                    continue;
                }
                let ino = group * self.core.sb.inodes_per_group + bit + 1;
                if !bit_set_1based(&used_inodes, ino) {
                    inode_bitmap[byte] &= !mask;
                    ichanged = true;
                    freed_inodes += 1;
                }
            }
            if ichanged {
                self.write_block(bgd.inode_bitmap, &inode_bitmap)?;
            }
        }

        if freed_blocks > 0 || freed_inodes > 0 {
            crate::ktrace!(
                crate::debug::FS,
                "ext2: reclaimed {} orphaned block(s), {} orphaned inode(s) left by an unclean shutdown",
                freed_blocks, freed_inodes
            );
            // Permanent counter (see kernel::debug), not just a trace line
            // — readable via /proc/kdebug regardless of whether FS
            // tracing happened to be on for this particular boot.
            crate::debug::add_orphans_reclaimed(freed_blocks as u64, freed_inodes as u64);
            self.reconcile_free_counts()?;
        }

        Ok(())
    }

    /// Recursive step of `reclaim_orphans`: mark `ino` (and, if it's a
    /// directory, everything reachable through it) as used in
    /// `used_inodes`/`used_blocks`. `used_inodes` doubles as the
    /// already-visited set — an inode marked on entry short-circuits
    /// immediately, which is what makes a cyclic (corrupted) directory
    /// structure terminate instead of recursing forever, on top of the
    /// hard `depth_left` bound below.
    fn mark_reachable(&self, ino: u32, used_inodes: &mut [u8], used_blocks: &mut [u8], depth_left: u32) -> Result<(), Errno> {
        if bit_set_1based(used_inodes, ino) {
            return Ok(()); // already visited — cycle guard
        }
        if depth_left == 0 {
            // See `reclaim_orphans`'s doc comment: failing loudly here
            // (instead of silently stopping) is what keeps an
            // unexpectedly-deep-but-legitimate subtree from being
            // mistaken for garbage by the sweep.
            return Err(Errno::ELOOP);
        }
        mark_bit_1based(used_inodes, ino);

        let raw = self.read_inode(ino)?;
        if raw.has_block_pointers() {
            self.visit_inode_blocks(&raw, |b| {
                mark_bit(used_blocks, b);
                Ok(())
            })?;
        }

        if raw.is_dir() {
            for entry in self.read_dir_entries(&raw)? {
                self.mark_reachable(entry.ino, used_inodes, used_blocks, depth_left - 1)?;
            }
        }

        Ok(())
    }
}

/// Set bit `n` (0-based) in a byte-packed bitmap — shared by
/// `reclaim_orphans`'s block-side bookkeeping, which (like the real
/// on-disk block bitmap it mirrors) indexes by absolute block number.
fn mark_bit(bitmap: &mut [u8], n: u32) {
    let byte = (n / 8) as usize;
    if byte < bitmap.len() {
        bitmap[byte] |= 1u8 << (n % 8);
    }
}

fn bit_set(bitmap: &[u8], n: u32) -> bool {
    let byte = (n / 8) as usize;
    bitmap.get(byte).is_some_and(|b| b & (1u8 << (n % 8)) != 0)
}

/// Same as `mark_bit`/`bit_set`, but for a 1-based inode number (real
/// ext2's own convention — inode 0 doesn't exist, inode 1 is bit 0).
fn mark_bit_1based(bitmap: &mut [u8], ino: u32) {
    if ino >= 1 {
        mark_bit(bitmap, ino - 1);
    }
}

fn bit_set_1based(bitmap: &[u8], ino: u32) -> bool {
    ino >= 1 && bit_set(bitmap, ino - 1)
}

/// Count clear (free) bits among the first `valid_bits` bits of `bitmap` —
/// shared by `reconcile_free_counts`'s block- and inode-bitmap passes.
fn count_free_bits(bitmap: &[u8], valid_bits: u32) -> u16 {
    let mut free = 0u32;
    for bit in 0..valid_bits {
        let byte = (bit / 8) as usize;
        let mask = 1u8 << (bit % 8);
        if bitmap[byte] & mask == 0 {
            free += 1;
        }
    }
    free as u16
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
//
// `RawInode` itself now lives in the `ext2` crate (migration step 1 — see
// the "ext2 core crate" note near the top of this file) and is imported
// there (`use ext2::RawInode;`). Nothing here redefines it.

// ── VFS glue ─────────────────────────────────────────────────────────────────

pub struct Ext2FsHandle;

impl Filesystem for Ext2FsHandle {
    fn name(&self) -> &str { "ext2" }

    fn root(&self) -> Result<Arc<dyn Inode>, Errno> {
        // `Filesystem::root()` being `Result`-returning (see its doc
        // comment in vfs.rs) is what lets this just reuse the ordinary
        // fallible constructor below — a disk read failure here
        // propagates as a clean `EIO` through `resolve()` like any other
        // failed path-resolution step. No synthetic stand-in inode
        // needed.
        Ok(Arc::new(Ext2Inode::new(ROOT_INO)?))
    }
}

struct Ext2Inode {
    ino: u32,
    raw: RawInode,
}

impl Ext2Inode {
    fn new(ino: u32) -> Result<Self, Errno> {
        let raw = fs().read_inode(ino)?;
        Ok(Self { ino, raw })
    }
}

impl Inode for Ext2Inode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        // Real on-disk permission bits, not a hardcoded per-filesystem
        // constant (see module doc comment) — overlaid onto whichever
        // constructor already set the right type bits/size shape.
        let perm = (self.raw.i_mode() & 0o7777) as u32;
        let nlink = self.raw.links_count() as u64;
        if self.raw.is_dir() {
            Stat::dir(self.ino as u64).with_perm_bits(perm).with_nlink(nlink)
        } else if self.raw.is_symlink() {
            Stat::symlink(self.ino as u64, self.raw.size() as i64)
        } else {
            Stat::regular_writable(self.ino as u64, self.raw.size() as i64).with_perm_bits(perm).with_nlink(nlink)
        }
    }

    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        if self.raw.is_symlink() {
            // Dead code through normal traversal: `vfs::resolve` always
            // dereferences a symlink (via `readlink()`) before `open()` is
            // ever called on the final inode — same defensive-only
            // rejection ramfs's `RamSymlinkNode::open()` uses.
            return Err(Errno::EINVAL);
        }
        if self.raw.is_dir() {
            if flags.is_write() {
                return Err(Errno::EISDIR);
            }
            // Snapshot into plain `DirEntry`s (synthetic "."/".." included)
            // up front, same shape ramfs's `RamDirHandle` uses — lets both
            // share `vfs::getdents64_from_snapshot` instead of each
            // hand-rolling their own packing loop.
            let raw_entries = fs().read_dir_entries(&self.raw)?;
            let mut snapshot: Vec<DirEntry> = Vec::with_capacity(raw_entries.len() + 2);
            snapshot.push(DirEntry::new(self.ino as u64, FileType::Directory, b"."));
            snapshot.push(DirEntry::new(self.ino as u64, FileType::Directory, b".."));
            for e in raw_entries {
                snapshot.push(DirEntry::new(e.ino as u64, e.kind, e.name.as_bytes()));
            }
            Ok(Box::new(Ext2DirHandle { ino: self.ino, snapshot, offset: 0 }))
        } else {
            let mut raw = self.raw.clone();
            if flags.is_write() && flags.0 & OpenFlags::TRUNC.0 != 0 {
                let _guard = EXT2_LOCK.lock();
                fs().truncate_to_zero(self.ino, &mut raw)?;
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
        let entries = fs().read_dir_entries(&self.raw)?;
        let e = entries.into_iter().find(|e| e.name == name).ok_or(Errno::ENOENT)?;
        Ok(Arc::new(Ext2Inode::new(e.ino)?) as Arc<dyn Inode>)
    }

    fn readdir(&self, offset: u64) -> Result<Option<DirEntry>, Errno> {
        if !self.raw.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        match offset {
            0 => Ok(Some(DirEntry::new(self.ino as u64, FileType::Directory, b"."))),
            1 => Ok(Some(DirEntry::new(self.ino as u64, FileType::Directory, b".."))),
            n => {
                let entries = fs().read_dir_entries(&self.raw)?;
                let idx = (n - 2) as usize;
                Ok(entries.get(idx).map(|e| DirEntry::new(e.ino as u64, e.kind, e.name.as_bytes())))
            }
        }
    }

    fn create(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        if !self.raw.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        let _guard = EXT2_LOCK.lock();
        if let Ok(existing) = self.lookup(name) {
            if existing.file_type() == FileType::Directory {
                return Err(Errno::EISDIR);
            }
            return Ok(existing);
        }

        let f = fs();
        let new_ino = f.alloc_inode(false)?.ok_or(Errno::ENOSPC)?;
        let mut new_raw = RawInode::zeroed(f.core.sb.inode_size as usize);
        new_raw.set_i_mode(0x8000 | 0o644);
        new_raw.set_links_count(1);
        f.write_inode(new_ino, &new_raw)?;

        let mut dir_raw = self.raw.clone();
        if let Err(e) = f.add_dir_entry(self.ino, &mut dir_raw, name, new_ino, FileType::Regular) {
            let _ = f.free_inode(new_ino, false); // best-effort cleanup — original error wins either way
            return Err(e);
        }
        Ok(Arc::new(Ext2Inode::new(new_ino)?))
    }

    fn mkdir(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        if !self.raw.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        let _guard = EXT2_LOCK.lock();
        if self.lookup(name).is_ok() {
            return Err(Errno::EEXIST);
        }

        let f = fs();
        let new_ino = f.alloc_inode(true)?.ok_or(Errno::ENOSPC)?;
        let new_block = match f.alloc_block()? {
            Some(b) => b,
            None => { let _ = f.free_inode(new_ino, true); return Err(Errno::ENOSPC); }
        };

        let mut new_raw = RawInode::zeroed(f.core.sb.inode_size as usize);
        new_raw.set_i_mode(0x4000 | 0o755);
        new_raw.set_links_count(2);
        new_raw.set_i_block(0, new_block);
        new_raw.set_size(f.core.sb.block_size as u64);

        let bs = f.core.sb.block_size as usize;
        let mut buf = alloc::vec![0u8; bs];
        let dot_len = dirent_len(1);
        write_dirent(&mut buf[0..dot_len], new_ino, dot_len as u16, ".", FileType::Directory);
        let remaining = bs - dot_len;
        write_dirent(&mut buf[dot_len..dot_len + remaining], self.ino, remaining as u16, "..", FileType::Directory);
        f.write_block(new_block, &buf)?;
        f.write_inode(new_ino, &new_raw)?;

        let mut dir_raw = self.raw.clone();
        if let Err(e) = f.add_dir_entry(self.ino, &mut dir_raw, name, new_ino, FileType::Directory) {
            let _ = f.free_block(new_block);
            let _ = f.free_inode(new_ino, true);
            return Err(e);
        }
        // The new subdirectory's ".." counts as a link to this parent.
        let mut parent_raw = dir_raw;
        parent_raw.set_links_count(parent_raw.links_count() + 1);
        f.write_inode(self.ino, &parent_raw)?;

        Ok(Arc::new(Ext2Inode::new(new_ino)?))
    }

    fn unlink(&self, name: &str) -> Result<(), Errno> {
        if !self.raw.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        let _guard = EXT2_LOCK.lock();
        let child = self.lookup(name)?;
        if child.file_type() == FileType::Directory {
            return Err(Errno::EISDIR);
        }

        let f = fs();
        let dir_raw = self.raw.clone();
        let (child_ino, _kind) = f.remove_dir_entry(&dir_raw, name)?;

        let mut child_raw = f.read_inode(child_ino)?;
        let links = child_raw.links_count().saturating_sub(1);
        child_raw.set_links_count(links);
        if links == 0 {
            f.free_all_blocks(&mut child_raw)?;
            child_raw.set_dtime(crate::time::now_unix_secs() as u32);
            // Persist the now-zeroed record (size/blocks/pointers) before
            // dropping the inode bitmap bit — `free_all_blocks` only
            // updates `child_raw` in memory. Skipping this left the old,
            // pre-delete inode record (nonzero mode, stale block
            // pointers into blocks whose bits `free_all_blocks` had
            // already cleared) sitting on disk with a freed bitmap bit —
            // a real `e2fsck` sees that as a "disconnected inode" with
            // dangling pointers into blocks a later allocation could
            // legitimately reuse for something else.
            f.write_inode(child_ino, &child_raw)?;
            f.free_inode(child_ino, false)?;
        } else {
            f.write_inode(child_ino, &child_raw)?;
        }
        Ok(())
    }

    fn rmdir(&self, name: &str) -> Result<(), Errno> {
        if !self.raw.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        let _guard = EXT2_LOCK.lock();
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

        let mut child_raw = f.read_inode(child_ino)?;
        f.free_all_blocks(&mut child_raw)?;
        child_raw.set_dtime(crate::time::now_unix_secs() as u32);
        // Same "persist the zeroed record before freeing the bitmap bit"
        // fix as `unlink` above.
        f.write_inode(child_ino, &child_raw)?;
        f.free_inode(child_ino, true)?;

        // This directory loses the link the removed child's ".." held.
        let mut parent_raw = self.raw.clone();
        parent_raw.set_links_count(parent_raw.links_count().saturating_sub(1));
        f.write_inode(self.ino, &parent_raw)?;
        Ok(())
    }

    fn take_child(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        if !self.raw.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        let _guard = EXT2_LOCK.lock();
        let child = self.lookup(name)?;
        let f = fs();
        let dir_raw = self.raw.clone();
        let (_child_ino, kind) = f.remove_dir_entry(&dir_raw, name)?;
        if kind == FileType::Directory {
            let mut parent_raw = self.raw.clone();
            parent_raw.set_links_count(parent_raw.links_count().saturating_sub(1));
            f.write_inode(self.ino, &parent_raw)?;
        }
        Ok(child)
    }

    fn insert_child(&self, name: &str, node: Arc<dyn Inode>) -> Result<(), Errno> {
        if !self.raw.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        let _guard = EXT2_LOCK.lock();
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
            f.write_inode(self.ino, &parent_raw)?;
        }
        Ok(())
    }

    fn readlink(&self) -> Result<String, Errno> {
        if !self.raw.is_symlink() {
            return Err(Errno::EINVAL);
        }
        fs().read_symlink_target(&self.raw)
    }

    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn Inode>, Errno> {
        if !self.raw.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        let _guard = EXT2_LOCK.lock();
        if self.lookup(name).is_ok() {
            return Err(Errno::EEXIST);
        }

        let f = fs();
        let new_ino = f.alloc_inode(false)?.ok_or(Errno::ENOSPC)?;
        let mut new_raw = RawInode::zeroed(f.core.sb.inode_size as usize);
        new_raw.set_i_mode(0xA000 | 0o777);
        new_raw.set_links_count(1);

        if target.len() < 60 {
            // Fast symlink: target lives directly in the i_block bytes,
            // no data block allocated — see module doc comment.
            new_raw.buf[40..40 + target.len()].copy_from_slice(target.as_bytes());
            new_raw.set_size(target.len() as u64);
            if let Err(e) = f.write_inode(new_ino, &new_raw) {
                let _ = f.free_inode(new_ino, false);
                return Err(e);
            }
        } else {
            // Slow symlink: persist mode/links first (same "write content
            // before linking" ordering as `create`), then grow it exactly
            // like a regular file's content.
            if let Err(e) = f.write_inode(new_ino, &new_raw) {
                let _ = f.free_inode(new_ino, false);
                return Err(e);
            }
            if let Err(e) = f.write_file_range(new_ino, &mut new_raw, 0, target.as_bytes()) {
                let _ = f.free_all_blocks(&mut new_raw);
                let _ = f.free_inode(new_ino, false);
                return Err(e);
            }
        }

        let mut dir_raw = self.raw.clone();
        if let Err(e) = f.add_dir_entry(self.ino, &mut dir_raw, name, new_ino, FileType::Symlink) {
            let _ = f.free_all_blocks(&mut new_raw); // no-op if it was a fast symlink (no blocks allocated)
            let _ = f.free_inode(new_ino, false);
            return Err(e);
        }
        Ok(Arc::new(Ext2Inode::new(new_ino)?))
    }

    fn chmod(&self, mode: u32) -> Result<(), Errno> {
        let _guard = EXT2_LOCK.lock();
        let f = fs();
        let mut raw = f.read_inode(self.ino)?; // fresh, not `self.raw` — don't clobber a concurrent write's size/blocks
        let new_mode = (raw.i_mode() & 0xF000) | (mode as u16 & 0o7777);
        raw.set_i_mode(new_mode);
        f.write_inode(self.ino, &raw)
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
        fs().read_file_range(&raw, *offset, &mut buf[..n]).map_err(|_| FileError::IOError)?;
        *offset += n;
        Ok(n)
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        let _guard = EXT2_LOCK.lock();
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

    fn chmod(&mut self, mode: u32) -> FileResult<()> {
        let _guard = EXT2_LOCK.lock();
        let mut raw = self.raw.lock();
        let new_mode = (raw.i_mode() & 0xF000) | (mode as u16 & 0o7777);
        raw.set_i_mode(new_mode);
        fs().write_inode(self.ino, &raw).map_err(|_| FileError::IOError)
    }

    fn name(&self) -> &str { "ext2" }
}

struct Ext2DirHandle {
    ino: u32,
    snapshot: Vec<DirEntry>,
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
        crate::fs::vfs::getdents64_from_snapshot(&self.snapshot, &mut self.offset, buf)
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::dir(self.ino as u64))
    }

    fn name(&self) -> &str { "ext2/dir" }
}

// ── Test-only: minimal hand-built ext2 image ────────────────────────────────
//
// Backs the QEMU integration test in `kernel/src/hw_tests.rs`
// (`ext2_memdisk_roundtrip`). Building a real on-disk image from scratch is
// normally `mke2fs`'s job (the root `build.rs` already shells out to it for
// `disk.img`) — this doesn't duplicate that to work around a missing tool.
// The point is a *self-contained* image with zero host-tool dependency,
// built at kernel **runtime**, inside the test itself, instead of an
// embedded build-time artifact. That keeps the QEMU test binary buildable
// on any machine (no `mke2fs`/`e2fsprogs` requirement beyond what the root
// build already needs for `disk.img`) and, more importantly, means the test
// never touches the real disk.img at all.
//
// The image is deliberately the smallest thing that satisfies every field
// `Ext2Fs::mount()`/`read_bgd`/`alloc_block`/`alloc_inode`/
// `reconcile_free_counts`/`reclaim_orphans` actually read: revision 0 (so
// `inode_size`=128 and `first_ino`=11 are the fixed rev-0 defaults, and
// `s_feature_incompat` can stay all-zero — no FILETYPE feature needed for
// this driver's own read/write round trip, see the module doc comment on
// that field), a single block group, and free-count fields computed to
// exactly match what's actually marked used — so `reconcile_free_counts`
// and `reclaim_orphans` both find nothing to repair on a fresh mount,
// exactly like a real freshly-`mke2fs`'d image would.
//
// Layout (1024-byte blocks):
//   block 0        — boot block (unused)
//   block 1        — superblock          (first_data_block)
//   block 2        — block group descriptor table (1 group fits in 1 block)
//   block 3        — block bitmap
//   block 4        — inode bitmap
//   blocks 5..=20  — inode table (128 inodes * 128 bytes = 16 blocks)
//   block 21       — root directory data ("." and ".." only)
//   blocks 22..255 — free data blocks (234 of them — plenty for a test that
//                    creates a handful of files/dirs)
#[cfg(test)]
pub(crate) fn build_minimal_image() -> Vec<u8> {
    const BLOCK_SIZE: u32 = 1024;
    const TOTAL_BLOCKS: u32 = 256; // 256 KiB image
    const INODES_COUNT: u32 = 128;
    const INODE_SIZE: u32 = 128; // rev 0, fixed
    const FIRST_DATA_BLOCK: u32 = 1; // always 1 when block_size == 1024
    const BGDT_BLOCK: u32 = FIRST_DATA_BLOCK + 1;
    const BLOCK_BITMAP_BLOCK: u32 = 3;
    const INODE_BITMAP_BLOCK: u32 = 4;
    const INODE_TABLE_START: u32 = 5;

    let inodes_per_block = BLOCK_SIZE / INODE_SIZE; // 8
    let inode_table_blocks = (INODES_COUNT + inodes_per_block - 1) / inodes_per_block; // 16
    let root_data_block = INODE_TABLE_START + inode_table_blocks; // 21

    let mut img = alloc::vec![0u8; (TOTAL_BLOCKS * BLOCK_SIZE) as usize];
    let put_block = |img: &mut Vec<u8>, block_num: u32, data: &[u8]| {
        let off = (block_num * BLOCK_SIZE) as usize;
        img[off..off + data.len()].copy_from_slice(data);
    };

    // Every block from FIRST_DATA_BLOCK..=root_data_block is metadata the
    // block bitmap must mark used; group 0 covers all of `blocks_count`
    // here (a single group), so this is also the group's whole "used"
    // footprint before any test writes happen.
    let used_block_bits = root_data_block - FIRST_DATA_BLOCK + 1; // 21
    let blocks_per_group = TOTAL_BLOCKS; // one group covers the whole image
    let blocks_in_group0 = TOTAL_BLOCKS - FIRST_DATA_BLOCK; // 255 (block 0 excluded)
    let free_blocks = blocks_in_group0 - used_block_bits;
    let free_inodes = INODES_COUNT - 1; // only root (ino 2) is used

    // ── Superblock (lives at block FIRST_DATA_BLOCK, i.e. byte 1024) ──
    let mut sb = alloc::vec![0u8; BLOCK_SIZE as usize];
    sb[0..4].copy_from_slice(&INODES_COUNT.to_le_bytes());
    sb[4..8].copy_from_slice(&TOTAL_BLOCKS.to_le_bytes());
    sb[12..16].copy_from_slice(&free_blocks.to_le_bytes());
    sb[16..20].copy_from_slice(&free_inodes.to_le_bytes());
    sb[20..24].copy_from_slice(&FIRST_DATA_BLOCK.to_le_bytes());
    sb[24..28].copy_from_slice(&0u32.to_le_bytes()); // s_log_block_size = 0 -> 1024-byte blocks
    sb[32..36].copy_from_slice(&blocks_per_group.to_le_bytes());
    sb[40..44].copy_from_slice(&INODES_COUNT.to_le_bytes()); // inodes_per_group (1 group)
    sb[56..58].copy_from_slice(&EXT2_MAGIC.to_le_bytes());
    // s_rev_level (offset 76) left at 0 (rev 0) — see doc comment above for
    // why that's the simplest valid choice here.
    put_block(&mut img, FIRST_DATA_BLOCK, &sb);

    // ── Block group descriptor (block BGDT_BLOCK, offset 0 — only 1 group) ──
    let mut bgd = alloc::vec![0u8; 32];
    bgd[0..4].copy_from_slice(&BLOCK_BITMAP_BLOCK.to_le_bytes());
    bgd[4..8].copy_from_slice(&INODE_BITMAP_BLOCK.to_le_bytes());
    bgd[8..12].copy_from_slice(&INODE_TABLE_START.to_le_bytes());
    bgd[12..14].copy_from_slice(&(free_blocks as u16).to_le_bytes());
    bgd[14..16].copy_from_slice(&(free_inodes as u16).to_le_bytes());
    bgd[16..18].copy_from_slice(&1u16.to_le_bytes()); // bg_used_dirs_count (root)
    let mut bgdt_block_buf = alloc::vec![0u8; BLOCK_SIZE as usize];
    bgdt_block_buf[0..32].copy_from_slice(&bgd);
    put_block(&mut img, BGDT_BLOCK, &bgdt_block_buf);

    // ── Block bitmap: bits 0..used_block_bits (blocks FIRST_DATA_BLOCK..=root_data_block) ──
    let mut block_bitmap = alloc::vec![0u8; BLOCK_SIZE as usize];
    for bit in 0..used_block_bits {
        block_bitmap[(bit / 8) as usize] |= 1u8 << (bit % 8);
    }
    put_block(&mut img, BLOCK_BITMAP_BLOCK, &block_bitmap);

    // ── Inode bitmap: only ino 2 (root) is used ──
    let mut inode_bitmap = alloc::vec![0u8; BLOCK_SIZE as usize];
    inode_bitmap[0] |= 1u8 << (ROOT_INO - 1); // ino is 1-based; bit 0 = ino 1
    put_block(&mut img, INODE_BITMAP_BLOCK, &inode_bitmap);

    // ── Inode table: root's record only, everything else zeroed ──
    let mut root_raw = RawInode::zeroed(INODE_SIZE as usize);
    root_raw.set_i_mode(0x4000 | 0o755);
    root_raw.set_links_count(2); // "." + being a directory's own self-link
    root_raw.set_i_block(0, root_data_block);
    root_raw.set_size(BLOCK_SIZE as u64);
    root_raw.set_blocks_512(BLOCK_SIZE / SECTOR_SIZE as u32);

    let index_in_group = ROOT_INO - 1; // 1
    let table_block = INODE_TABLE_START + index_in_group / inodes_per_block;
    let offset_in_block = ((index_in_group % inodes_per_block) * INODE_SIZE) as usize;
    let mut table_block_buf = alloc::vec![0u8; BLOCK_SIZE as usize];
    table_block_buf[offset_in_block..offset_in_block + INODE_SIZE as usize]
        .copy_from_slice(&root_raw.buf);
    put_block(&mut img, table_block, &table_block_buf);

    // ── Root directory data: "." and ".." (both -> ROOT_INO, root has no parent) ──
    let mut root_dir = alloc::vec![0u8; BLOCK_SIZE as usize];
    let dot_len = dirent_len(1);
    write_dirent(&mut root_dir[0..dot_len], ROOT_INO, dot_len as u16, ".", FileType::Directory);
    let remaining = BLOCK_SIZE as usize - dot_len;
    write_dirent(&mut root_dir[dot_len..dot_len + remaining], ROOT_INO, remaining as u16, "..", FileType::Directory);
    put_block(&mut img, root_data_block, &root_dir);

    img
}

// ── Test-only: a minimal image with two orphans baked in ───────────────────
//
// Backs `hw_tests.rs`'s diagnostic for whether `reclaim_orphans` actually
// closes the gap `e2fsck -fn disk.img` reports against the real disk (an
// inode + a block marked used in the bitmaps with nothing reachable from
// root pointing at them) — without ever touching that disk. Same base
// layout as `build_minimal_image` (see its doc comment for the block map),
// extended with two more inodes/data-blocks marked used in the bitmaps but
// **not** linked from root's directory data — i.e. deliberately orphaned
// by construction, the same shape an interrupted write or an out-of-band
// tool (`debugfs -w`) can leave behind:
//
//   - ino `ORPHAN_FILE_INO` (20): a plain regular file with one data
//     block — the simplest possible orphan.
//   - ino `ORPHAN_DIR_INO` (31 — deliberately the same inode number
//     `e2fsck -fn disk.img` reported disconnected, so this reproduces that
//     report's exact shape, not just "some orphan"): a *directory* whose
//     own data block has "." pointing at itself and ".." pointing at root
//     (ino 2), exactly like a real subdirectory — but with no directory
//     entry anywhere under root pointing back at it. This is the precise
//     on-disk shape behind e2fsck's report: "El directorio del nodo-i 31
//     está desconectado (estaba en /)" + "'..' ... es / (2) y debería ser
//     <El nodo-i NULO> (0)".
//
// Both orphan inodes/blocks live in table/data regions `build_minimal_image`
// already reserves as valid-but-unused (inode table blocks 5..=20, free
// data blocks from 22 on), so no metadata region needs resizing. The
// superblock/BGD free-block/free-inode counters are set to already agree
// with the bitmaps as modified here (root + 2 orphan inodes used; root +
// both orphan data blocks used) — deliberately isolating what's under test
// to `reclaim_orphans` alone. `reconcile_free_counts` already has its own
// coverage via `build_minimal_image`'s normal use in
// `ext2_memdisk_roundtrip`.
#[cfg(test)]
/// Regular-file orphan inode number baked into `build_image_with_orphans`:
/// unreachable from root, no directory entry anywhere points at it.
pub(crate) const ORPHAN_FILE_INO: u32 = 20;
#[cfg(test)]
/// Directory orphan inode number baked into `build_image_with_orphans`:
/// matches the exact inode number from the real `e2fsck -fn disk.img`
/// report (see that function's doc comment).
pub(crate) const ORPHAN_DIR_INO: u32 = 31;
#[cfg(test)]
/// Data block backing `ORPHAN_FILE_INO`, marked used in the block bitmap
/// with nothing under root pointing at it.
pub(crate) const ORPHAN_FILE_BLOCK: u32 = 22;
#[cfg(test)]
/// Data block backing `ORPHAN_DIR_INO`'s own "."/".." directory data.
pub(crate) const ORPHAN_DIR_BLOCK: u32 = 23;
#[cfg(test)]
/// The *actual* shape found by reproducing the real `debugfs -w mkdir`
/// leak this module's doc comment on `build_image_with_orphans`
/// describes: a real, fully-formed directory inode (mode/links/block
/// pointer all set, a real "."/".."->root data block written) that is
/// disconnected from root the same way `ORPHAN_DIR_INO` is — but with its
/// inode-bitmap bit and block-bitmap bit both left **clear** (i.e.
/// "free"), not set. This is the opposite polarity from
/// `ORPHAN_FILE_INO`/`ORPHAN_DIR_INO` above, and it is deliberately
/// outside what `reclaim_orphans`'s sweep can touch: that sweep only ever
/// *clears* a bit that starts out set (see its inner loop's `if ... == 0
/// { continue; }` skip on an already-clear bit) — a bit that's already
/// clear, no matter what stale content sits behind it, is invisible to
/// it by construction, not by omission.
pub(crate) const PHANTOM_DIR_INO: u32 = 45;
#[cfg(test)]
/// Data block backing `PHANTOM_DIR_INO`'s own "."/".." directory data —
/// real content, bitmap bit left clear (see `PHANTOM_DIR_INO`).
pub(crate) const PHANTOM_DIR_BLOCK: u32 = 24;

#[cfg(test)]
pub(crate) fn build_image_with_orphans() -> Vec<u8> {
    const BLOCK_SIZE: u32 = 1024;
    const TOTAL_BLOCKS: u32 = 256;
    const INODES_COUNT: u32 = 128;
    const INODE_SIZE: u32 = 128;
    const FIRST_DATA_BLOCK: u32 = 1;
    const BGDT_BLOCK: u32 = FIRST_DATA_BLOCK + 1;
    const BLOCK_BITMAP_BLOCK: u32 = 3;
    const INODE_BITMAP_BLOCK: u32 = 4;
    const INODE_TABLE_START: u32 = 5;

    let inodes_per_block = BLOCK_SIZE / INODE_SIZE; // 8
    let inode_table_blocks = (INODES_COUNT + inodes_per_block - 1) / inodes_per_block; // 16
    let root_data_block = INODE_TABLE_START + inode_table_blocks; // 21
    let orphan_file_block = ORPHAN_FILE_BLOCK;
    let orphan_dir_block = ORPHAN_DIR_BLOCK;
    debug_assert_eq!(orphan_file_block, root_data_block + 1);
    debug_assert_eq!(orphan_dir_block, root_data_block + 2);

    let mut img = alloc::vec![0u8; (TOTAL_BLOCKS * BLOCK_SIZE) as usize];
    let put_block = |img: &mut Vec<u8>, block_num: u32, data: &[u8]| {
        let off = (block_num * BLOCK_SIZE) as usize;
        img[off..off + data.len()].copy_from_slice(data);
    };

    // root's own metadata footprint (same as build_minimal_image), plus
    // the two orphan data blocks.
    let used_block_bits = root_data_block - FIRST_DATA_BLOCK + 1; // 21
    let blocks_per_group = TOTAL_BLOCKS;
    let blocks_in_group0 = TOTAL_BLOCKS - FIRST_DATA_BLOCK; // 255
    let free_blocks = blocks_in_group0 - used_block_bits - 2; // minus both orphan blocks
    let free_inodes = INODES_COUNT - 1 - 2; // minus root and both orphan inodes

    // ── Superblock ──
    let mut sb = alloc::vec![0u8; BLOCK_SIZE as usize];
    sb[0..4].copy_from_slice(&INODES_COUNT.to_le_bytes());
    sb[4..8].copy_from_slice(&TOTAL_BLOCKS.to_le_bytes());
    sb[12..16].copy_from_slice(&free_blocks.to_le_bytes());
    sb[16..20].copy_from_slice(&free_inodes.to_le_bytes());
    sb[20..24].copy_from_slice(&FIRST_DATA_BLOCK.to_le_bytes());
    sb[24..28].copy_from_slice(&0u32.to_le_bytes());
    sb[32..36].copy_from_slice(&blocks_per_group.to_le_bytes());
    sb[40..44].copy_from_slice(&INODES_COUNT.to_le_bytes());
    sb[56..58].copy_from_slice(&EXT2_MAGIC.to_le_bytes());
    put_block(&mut img, FIRST_DATA_BLOCK, &sb);

    // ── Block group descriptor ──
    let mut bgd = alloc::vec![0u8; 32];
    bgd[0..4].copy_from_slice(&BLOCK_BITMAP_BLOCK.to_le_bytes());
    bgd[4..8].copy_from_slice(&INODE_BITMAP_BLOCK.to_le_bytes());
    bgd[8..12].copy_from_slice(&INODE_TABLE_START.to_le_bytes());
    bgd[12..14].copy_from_slice(&(free_blocks as u16).to_le_bytes());
    bgd[14..16].copy_from_slice(&(free_inodes as u16).to_le_bytes());
    // bg_used_dirs_count: root + the orphan directory (it IS a directory
    // on disk, even though nothing links to it — a real mke2fs/e2fsck
    // would count it here too).
    bgd[16..18].copy_from_slice(&2u16.to_le_bytes());
    let mut bgdt_block_buf = alloc::vec![0u8; BLOCK_SIZE as usize];
    bgdt_block_buf[0..32].copy_from_slice(&bgd);
    put_block(&mut img, BGDT_BLOCK, &bgdt_block_buf);

    // ── Block bitmap: metadata footprint + both orphan data blocks ──
    let mut block_bitmap = alloc::vec![0u8; BLOCK_SIZE as usize];
    for bit in 0..used_block_bits {
        block_bitmap[(bit / 8) as usize] |= 1u8 << (bit % 8);
    }
    mark_bit(&mut block_bitmap, orphan_file_block - FIRST_DATA_BLOCK);
    mark_bit(&mut block_bitmap, orphan_dir_block - FIRST_DATA_BLOCK);
    put_block(&mut img, BLOCK_BITMAP_BLOCK, &block_bitmap);

    // ── Inode bitmap: root + both orphans ──
    let mut inode_bitmap = alloc::vec![0u8; BLOCK_SIZE as usize];
    inode_bitmap[0] |= 1u8 << (ROOT_INO - 1);
    mark_bit_1based(&mut inode_bitmap, ORPHAN_FILE_INO);
    mark_bit_1based(&mut inode_bitmap, ORPHAN_DIR_INO);
    put_block(&mut img, INODE_BITMAP_BLOCK, &inode_bitmap);

    // Helper: write one inode record into whatever inode-table block it
    // belongs to (each orphan lands in its own previously-all-zero table
    // block here, so a single put_block of that whole block is enough —
    // same pattern build_minimal_image uses for root's own record).
    let write_inode_record = |img: &mut Vec<u8>, ino: u32, raw: &RawInode| {
        let index_in_group = ino - 1;
        let table_block = INODE_TABLE_START + index_in_group / inodes_per_block;
        let offset_in_block = ((index_in_group % inodes_per_block) * INODE_SIZE) as usize;
        let mut table_block_buf = alloc::vec![0u8; BLOCK_SIZE as usize];
        table_block_buf[offset_in_block..offset_in_block + INODE_SIZE as usize]
            .copy_from_slice(&raw.buf);
        put_block(img, table_block, &table_block_buf);
    };

    // ── Root's own inode + directory data (identical to build_minimal_image) ──
    let mut root_raw = RawInode::zeroed(INODE_SIZE as usize);
    root_raw.set_i_mode(0x4000 | 0o755);
    root_raw.set_links_count(2);
    root_raw.set_i_block(0, root_data_block);
    root_raw.set_size(BLOCK_SIZE as u64);
    root_raw.set_blocks_512(BLOCK_SIZE / SECTOR_SIZE as u32);
    write_inode_record(&mut img, ROOT_INO, &root_raw);

    let mut root_dir = alloc::vec![0u8; BLOCK_SIZE as usize];
    let dot_len = dirent_len(1);
    write_dirent(&mut root_dir[0..dot_len], ROOT_INO, dot_len as u16, ".", FileType::Directory);
    let remaining = BLOCK_SIZE as usize - dot_len;
    write_dirent(&mut root_dir[dot_len..dot_len + remaining], ROOT_INO, remaining as u16, "..", FileType::Directory);
    // Deliberately NOT adding entries for either orphan here — that
    // omission is exactly what makes them orphans.
    put_block(&mut img, root_data_block, &root_dir);

    // ── Orphan regular file: ino 20, one data block, no directory entry ──
    let mut file_raw = RawInode::zeroed(INODE_SIZE as usize);
    file_raw.set_i_mode(0x8000 | 0o644);
    file_raw.set_links_count(1);
    file_raw.set_i_block(0, orphan_file_block);
    file_raw.set_size(BLOCK_SIZE as u64);
    file_raw.set_blocks_512(BLOCK_SIZE / SECTOR_SIZE as u32);
    write_inode_record(&mut img, ORPHAN_FILE_INO, &file_raw);
    // Data block content is irrelevant to the reachability question —
    // leave it zeroed (already is).

    // ── Orphan directory: ino 31, "." -> self, ".." -> root, no entry
    // anywhere under root pointing at it ──
    let mut dir_raw = RawInode::zeroed(INODE_SIZE as usize);
    dir_raw.set_i_mode(0x4000 | 0o755);
    dir_raw.set_links_count(2); // "." + its own directory-ness, same as root
    dir_raw.set_i_block(0, orphan_dir_block);
    dir_raw.set_size(BLOCK_SIZE as u64);
    dir_raw.set_blocks_512(BLOCK_SIZE / SECTOR_SIZE as u32);
    write_inode_record(&mut img, ORPHAN_DIR_INO, &dir_raw);

    let mut orphan_dir_data = alloc::vec![0u8; BLOCK_SIZE as usize];
    write_dirent(&mut orphan_dir_data[0..dot_len], ORPHAN_DIR_INO, dot_len as u16, ".", FileType::Directory);
    write_dirent(&mut orphan_dir_data[dot_len..dot_len + remaining], ROOT_INO, remaining as u16, "..", FileType::Directory);
    put_block(&mut img, orphan_dir_block, &orphan_dir_data);

    // ── Phantom directory: same disconnected shape as ORPHAN_DIR_INO —
    // real inode-table record + real "."/".."->root data block — but
    // *neither* bitmap bit is set, reproducing the actual shape found by
    // empirically reproducing the real `debugfs -w`/`mkdir` leak (see
    // PHANTOM_DIR_INO's doc comment): `ext2fs_mkdir2()` writes the new
    // inode record and its directory data block before it ever attempts
    // to link the name into the parent, and its EEXIST error path leaves
    // that already-written content behind WITHOUT ever marking either
    // bitmap bit — the opposite polarity from a crash-interrupted normal
    // allocation, which sets the bitmap bit before/while writing content.
    let mut phantom_raw = RawInode::zeroed(INODE_SIZE as usize);
    phantom_raw.set_i_mode(0x4000 | 0o755);
    phantom_raw.set_links_count(2);
    phantom_raw.set_i_block(0, PHANTOM_DIR_BLOCK);
    phantom_raw.set_size(BLOCK_SIZE as u64);
    phantom_raw.set_blocks_512(BLOCK_SIZE / SECTOR_SIZE as u32);
    write_inode_record(&mut img, PHANTOM_DIR_INO, &phantom_raw);

    let mut phantom_dir_data = alloc::vec![0u8; BLOCK_SIZE as usize];
    write_dirent(&mut phantom_dir_data[0..dot_len], PHANTOM_DIR_INO, dot_len as u16, ".", FileType::Directory);
    write_dirent(&mut phantom_dir_data[dot_len..dot_len + remaining], ROOT_INO, remaining as u16, "..", FileType::Directory);
    put_block(&mut img, PHANTOM_DIR_BLOCK, &phantom_dir_data);
    // Deliberately NOT marking PHANTOM_DIR_INO/PHANTOM_DIR_BLOCK used in
    // either bitmap, and NOT accounted for in free_blocks/free_inodes
    // above — the bitmaps already (accidentally) agree this is "free",
    // which is exactly the point.

    img
}

/// Test-only: mount a `BlockDevice` as a *standalone* `Ext2Fs`, entirely
/// bypassing the `EXT2` global `Once` (see `init_with_device`'s doc
/// comment for why that global exists and what it costs a test that needs
/// more than one fresh mount per QEMU boot). `ext2_memdisk_roundtrip`
/// (`hw_tests.rs`) drives the real VFS through the global and that's the
/// right tool for exercising the read-write path end to end — but a test
/// that specifically wants to inspect *this* image's own bitmap state
/// before and after `reclaim_orphans`/`reconcile_free_counts`, independent
/// of whatever the global happens to hold from another test case in the
/// same boot, needs its own private `Ext2Fs` instead. Never used by
/// production code (`init()`/`init_with_device()`/`mount_and_repair()` are
/// untouched by this) — `#[cfg(test)]`, same gate as `init_with_device`.
///
/// Deliberately does NOT call `reconcile_free_counts`/`reclaim_orphans`
/// itself the way `mount_and_repair` does — the caller drives those
/// explicitly so it can observe bitmap/counter state at each step.
#[cfg(test)]
pub(crate) struct TestFs(Ext2Fs);

#[cfg(test)]
impl TestFs {
    pub(crate) fn mount(device: Box<dyn BlockDevice>) -> Result<Self, &'static str> {
        Ext2Fs::mount(device).map(TestFs)
    }

    pub(crate) fn reconcile_free_counts(&self) -> Result<(), Errno> {
        self.0.reconcile_free_counts()
    }

    pub(crate) fn reclaim_orphans(&self) -> Result<(), Errno> {
        self.0.reclaim_orphans()
    }

    /// Whether `ino` (1-based) is marked used in its group's on-disk
    /// inode bitmap right now.
    pub(crate) fn inode_used(&self, ino: u32) -> Result<bool, Errno> {
        let group = (ino - 1) / self.0.core.sb.inodes_per_group;
        let bit = (ino - 1) % self.0.core.sb.inodes_per_group;
        let bgd = self.0.read_bgd(group)?;
        let bitmap = self.0.block_vec(bgd.inode_bitmap)?;
        Ok(bit_set(&bitmap, bit))
    }

    /// Whether `block` (absolute block number) is marked used in its
    /// group's on-disk block bitmap right now.
    pub(crate) fn block_used(&self, block: u32) -> Result<bool, Errno> {
        let group = (block - self.0.core.sb.first_data_block) / self.0.core.sb.blocks_per_group;
        let bit = (block - self.0.core.sb.first_data_block) % self.0.core.sb.blocks_per_group;
        let bgd = self.0.read_bgd(group)?;
        let bitmap = self.0.block_vec(bgd.block_bitmap)?;
        Ok(bit_set(&bitmap, bit))
    }

    /// Raw `i_mode` of `ino`'s on-disk inode record, read directly (not
    /// gated on the bitmap at all) — lets a test prove a "phantom" inode's
    /// real content (mode/links/block pointer) is still sitting there
    /// untouched after a repair pass that, by design, never looks at
    /// content behind an already-clear bitmap bit.
    pub(crate) fn inode_mode(&self, ino: u32) -> Result<u16, Errno> {
        Ok(self.0.read_inode(ino)?.i_mode())
    }

    /// `(free_blocks, free_inodes)` straight off the on-disk superblock.
    pub(crate) fn sb_free_counts(&self) -> Result<(u32, u32), Errno> {
        let mut raw = [0u8; 1024];
        self.0.core.device.read_sectors(2, 2, &mut raw).map_err(|_| Errno::EIO)?;
        let free_blocks = u32::from_le_bytes(raw[12..16].try_into().unwrap());
        let free_inodes = u32::from_le_bytes(raw[16..20].try_into().unwrap());
        Ok((free_blocks, free_inodes))
    }

    /// `(free_blocks, free_inodes)` straight off group `group`'s on-disk
    /// BGD entry.
    pub(crate) fn bgd_free_counts(&self, group: u32) -> Result<(u16, u16), Errno> {
        let bgd = self.0.read_bgd(group)?;
        Ok((bgd.free_blocks, bgd.free_inodes))
    }

    /// Recompute the *true* free block/inode counts directly from the
    /// bitmaps (same formula `reconcile_free_counts` uses internally) —
    /// lets a test assert the on-disk counters are self-consistent with
    /// the bitmaps, not just with what the test expects.
    pub(crate) fn true_free_counts_group0(&self) -> Result<(u16, u16), Errno> {
        let bgd = self.0.read_bgd(0)?;
        let block_bitmap = self.0.block_vec(bgd.block_bitmap)?;
        let real_free_blocks = count_free_bits(&block_bitmap, self.0.blocks_in_group(0));
        let inode_bitmap = self.0.block_vec(bgd.inode_bitmap)?;
        let real_free_inodes = count_free_bits(&inode_bitmap, self.0.inodes_in_group(0));
        Ok((real_free_blocks, real_free_inodes))
    }
}
