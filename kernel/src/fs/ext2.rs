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
// `ext2::testimg::build_minimal_image`) and exercise this same read-write path with
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
// ext2-native symlinks ARE implemented (`Ext2Inode::symlink`/`readlink`) —
// see CLAUDE.md's "Filesystem: ext2" section for the fast/slow on-disk
// representation split.
//
// Permission bits: real on-disk `i_mode`, not a hardcoded per-filesystem
// constant — see CLAUDE.md's "Filesystem: ext2" section. New files/dirs
// still get a fixed initial mode (`create`/`mkdir` have no caller-supplied
// mode to honor — `sys_open`/`sys_mkdir` don't take one at all, see their
// doc comments in `process/syscall/fs.rs`); `chmod` can change it afterward.
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
// That last sentence is not just a convention maintained by hand: it is
// actually guarded, though only implicitly and only under QEMU. This
// crate's half of ext2 (the `ext2` crate) contains no locks at all — every
// `EXT2_LOCK` acquisition is in this adapter — so no host test can reach
// the invariant. What does reach it is
// `kernel::hw_tests::ext2_memdisk_roundtrip`, which drives
// create/mkdir/rename/symlink/unlink/rmdir through the real VFS; `rename`
// goes through `take_child`, which locks and *then* calls `self.lookup`.
// Verified by sabotage, not by reading: adding `let _g = EXT2_LOCK.lock();`
// to `Ext2Inode::lookup` makes that test hang mid-run — it prints its name
// and never reports `[ok]`, and the third test never starts — instead of
// failing. So the failure mode here is a HANG, not a red test; if
// `run-kernel-tests.sh` ever stops with `ext2_memdisk_roundtrip` as the
// last line printed, suspect a lock added to a read-only ext2 path first.
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
//     unrecoverable "in principle." A reclaimed inode's own record is
//     zeroed and `i_dtime`-stamped as it goes, the same as an ordinary
//     `unlink`/`rmdir` above — the bitmap bit alone isn't enough, since
//     e2fsck's Pass 1 reads the inode table directly. See
//     `ext2::Ext2Core::reclaim_orphans` for the mechanics and the sweep's
//     own crash-ordering rule; the wall clock it stamps comes from this
//     file's wrapper, since `ext2` has no clock of its own.

use alloc::{boxed::Box, string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use spin::{Mutex, Once};

use crate::block::BlockDevice;

use crate::fs::{
    types::{DirEntry, Errno, FileType, OpenFlags, Stat},
    vfs::{Filesystem, Inode},
};
use crate::process::file::{FileError, FileHandle, FileResult};

// ── ext2 core crate ─────────────────────────────────────────────────────────
//
// On-disk format/parsing, block/inode allocation + free-count bookkeeping,
// indirect-block addressing, byte-range I/O, directory operations,
// symlinks, and mount-time repair all live in the standalone, host-testable
// `ext2` crate (`ext2/src/`, `cd ext2 && cargo test`), speaking only in
// inode numbers/byte ranges/its own `Ext2Error`, never VFS types. This file
// is a thin adapter over `ext2::Ext2Core`: most methods here just delegate
// and convert between the core's `Ext2Error`/raw `file_type: u8` and this
// crate's `Errno`/`fs::types::FileType` at the boundary
// (`ext2_file_type_to_vfs`/`vfs_file_type_to_ext2`, also used directly by
// `mkdir` and the test image builders).
//
// Block/inode bitmap allocation has no wrapper here: `create`/`mkdir`/
// `unlink`/`rmdir`/`symlink` call `f.core.alloc_block`/`free_block`/
// `alloc_inode`/`free_inode` directly, the same way `free_all_blocks` calls
// `self.core.free_block`. Deliberately not duplicated in this file: an
// earlier byte-identical copy of that same bitmap logic used to live here
// too, live on the same mounted filesystem simultaneously with the core's
// copy, harmless only because `EXT2_LOCK` serialized both — a bitmap fix
// landing in only one of two identical allocators is silent corruption, so
// don't reintroduce a second copy.
use ext2::RawInode;
use ext2::ROOT_INO;

/// Local wrapper that exists purely to keep the `Ext2Error` → `Errno`
/// conversion legal: since the `vfs` crate extraction, `Errno` lives in
/// `vfs::types` and `Ext2Error` lives in the `ext2` crate, so a direct
/// `impl From<ext2::Ext2Error> for Errno` here would violate the orphan
/// rule (E0117) — neither type is local to this crate. Wrapping the
/// foreign error in a local type restores the "at least one local type"
/// requirement, and `.map_err(ExtErr)?` at call sites keeps `?` working
/// exactly as before.
struct ExtErr(ext2::Ext2Error);

impl From<ExtErr> for Errno {
    fn from(e: ExtErr) -> Self {
        match e.0 {
            // Only ever occur inside `Ext2Core::mount()`, which this
            // adapter's own `Ext2Fs::mount()` maps to a `&'static str`
            // directly (see below) rather than through this impl — EIO is
            // a reasonable fallback all the same, since every one of these
            // is fundamentally "the disk didn't give us what we expected."
            ext2::Ext2Error::Io | ext2::Ext2Error::BadMagic | ext2::Ext2Error::UnsupportedFeature => Errno::EIO,
            // Must map to their own distinct `Errno` values, not collapse
            // into EIO — `Ext2FileHandle::write` pattern-matches on
            // `Errno::ENOSPC` specifically to report `FileError::NoSpace`
            // instead of a generic I/O error (see `ext2::Ext2Error`'s own
            // doc comment on these two variants for the full reasoning).
            ext2::Ext2Error::NoSpace => Errno::ENOSPC,
            ext2::Ext2Error::TooLarge => Errno::EFBIG,
            // What a real `unlink(2)`/`rmdir(2)` reports for a name that
            // doesn't exist — `unlink`/`rmdir`/`take_child` need this exact
            // value, not a generic I/O error.
            ext2::Ext2Error::NotFound => Errno::ENOENT,
            // `reclaim_orphans`'s `mark_reachable` hit its hard recursion-
            // depth guard — same `Errno` value a real deep-symlink-
            // resolution guard uses, and the exact value
            // `mount_and_repair`'s own doc comment already promises ("a
            // directory tree too deep").
            ext2::Ext2Error::TooDeep => Errno::ELOOP,
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

/// The mounted filesystem's block-cache counters (`hal::blockcache`), for
/// `/proc/kdebug` — `None` before (or without) a mount. Lock-free: plain
/// atomic loads.
pub fn cache_stats() -> Option<hal::blockcache::CacheStats> {
    EXT2.get().map(|fs| fs.core.device.stats())
}

/// Serializes every mutating ext2 operation — see the module-level
/// ROBUSTNESS doc comment for why this exists and why read-only paths
/// don't take it.
static EXT2_LOCK: Mutex<()> = Mutex::new(());

/// Set when `/mnt` was mounted read-only (`init_read_only`). Every
/// mutating path takes [`write_lock`] instead of `EXT2_LOCK` directly, so
/// this one flag turns them all into `EROFS` — there is no mutation that
/// can forget to check it.
static READ_ONLY: AtomicBool = AtomicBool::new(false);

/// The lock every mutating operation takes, or `EROFS` on a read-only
/// mount.
fn write_lock() -> Result<spin::MutexGuard<'static, ()>, Errno> {
    if READ_ONLY.load(Ordering::Relaxed) {
        return Err(Errno::EROFS);
    }
    Ok(EXT2_LOCK.lock())
}

/// Whether `/mnt` is mounted read-only.
pub fn is_read_only() -> bool {
    READ_ONLY.load(Ordering::Relaxed)
}

/// Mount the ext2 filesystem: the USB boot pendrive's data partition if
/// there is one (read-only, see `init_read_only`), else the real ATA disk
/// (`crate::block::AtaBlockDevice`). Call once, before the VFS mounts
/// `/mnt`. Returns `Err`
/// (not panics) on any problem — a missing or unreadable disk shouldn't
/// take down boot, just leave `/mnt` unmounted.
pub fn init() -> Result<(), &'static str> {
    // The boot pendrive first: on the target machine it is the only disk
    // there is (no IDE), and in QEMU it is only present when asked for
    // (`QEMU_USB_STORAGE`), so the ATA path below still serves every
    // ordinary QEMU boot unchanged.
    if crate::usb::storage().is_some() {
        match crate::block::usb::data_partition() {
            Ok(part) => match init_read_only(Box::new(part)) {
                Ok(()) => {
                    crate::kalert!("ext2: /mnt montado desde el pendrive USB (solo lectura)");
                    return Ok(());
                }
                Err(e) => crate::kalert!("ext2: pendrive USB sin montar: {}", e),
            },
            Err(e) => crate::kalert!("ext2: pendrive USB sin montar: {}", e),
        }
    }

    let device: Box<dyn BlockDevice> = Box::new(crate::block::AtaBlockDevice);
    if !device.present() {
        return Err("no disk on the secondary IDE channel");
    }
    mount_and_repair(device)
}

/// Mounts ext2 **read-only** from an arbitrary device — the USB pendrive's
/// data partition. Skips the mount-time repair passes, which write, and
/// marks the mount read-only so every mutation fails with `EROFS` instead
/// of reaching the disk.
///
/// Read-only first because there is no journal and the stick is also the
/// boot key: a hang in the middle of `reclaim_orphans` on a new, barely
/// exercised transport would leave the only copy of the filesystem
/// inconsistent. Writing comes once reading is boring — see
/// `docs/storage/usb-msc-plan.md`, step 6.
pub fn init_read_only(device: Box<dyn BlockDevice>) -> Result<(), &'static str> {
    let fs = Ext2Fs::mount(device)?;
    READ_ONLY.store(true, Ordering::Relaxed);
    EXT2.call_once(|| fs);
    Ok(())
}

/// Alternate entry point used only by the QEMU integration test
/// (`kernel/src/hw_tests.rs`): mounts ext2 against an arbitrary
/// `BlockDevice` — a `hal::block::MemDisk` backed by a hand-built image
/// (`ext2::testimg::build_minimal_image`) in practice — instead of the real ATA
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
    /// Parse the superblock (delegated to `ext2::Ext2Core::mount`) and
    /// construct the adapter. Does NOT run the mount-time repair passes
    /// (`reconcile_free_counts`/`reclaim_orphans`) — `mount_and_repair`
    /// above calls those right after this returns, before publishing the
    /// result anywhere shared.
    fn mount(device: Box<dyn BlockDevice>) -> Result<Self, &'static str> {
        let core = ext2::Ext2Core::mount(device).map_err(|e| match e {
            ext2::Ext2Error::Io => "block device read of superblock failed",
            ext2::Ext2Error::BadMagic => "bad ext2 magic (not an ext2 filesystem, or wrong LBA)",
            ext2::Ext2Error::UnsupportedFeature => {
                "unsupported ext2 incompat features (ext4 extents? journal?) — refusing to mount"
            }
            // `Ext2Core::mount()` itself can never produce these — they're
            // only ever returned by other methods on an already-mounted
            // filesystem (block allocation during a write, directory-entry
            // removal, `reclaim_orphans`'s `mark_reachable`). Matched here
            // anyway because `Ext2Error` is a single enum shared across
            // every method in the crate, so this `match` must stay
            // exhaustive; `unreachable!()` documents that exhaustiveness
            // rather than silently falling back to a misleading message.
            ext2::Ext2Error::NoSpace | ext2::Ext2Error::TooLarge | ext2::Ext2Error::NotFound | ext2::Ext2Error::TooDeep => {
                unreachable!("mount() cannot produce this error")
            }
        })?;
        Ok(Self { core })
    }

    // ── Raw block I/O ────────────────────────────────────────────────────

    /// Write one filesystem block (`self.core.sb.block_size` bytes) from
    /// `buf`. Propagates an ATA failure as `Errno::EIO` instead of
    /// panicking, and rejects any `block_num` outside `0..blocks_count` —
    /// the single choke point every on-disk pointer passes through, so a
    /// corrupted BGD/inode pointer can't turn into a wild write at an
    /// arbitrary LBA (see the module-level ROBUSTNESS comment).
    fn write_block(&self, block_num: u32, buf: &[u8]) -> Result<(), Errno> {
        self.core.write_block(block_num, buf).map_err(|e| Errno::from(ExtErr(e)))
    }

    // ── Inode table / file byte-range I/O ─────────────────────────────────

    /// Read the raw on-disk inode record for `ino`.
    ///
    /// `ino` should always be a value read out of this same filesystem (a
    /// directory entry, or the well-known root inode 2) — bounds-checked
    /// against the superblock's own counts as a corruption tripwire, not
    /// because callers are expected to pass arbitrary numbers.
    fn read_inode(&self, ino: u32) -> Result<RawInode, Errno> {
        self.core.read_inode(ino).map_err(|e| Errno::from(ExtErr(e)))
    }

    /// Write `raw` back to `ino`'s on-disk inode record. Read-modify-write:
    /// the inode table block holds several inodes, so the rest of the
    /// block must survive untouched.
    fn write_inode(&self, ino: u32, raw: &RawInode) -> Result<(), Errno> {
        self.core.write_inode(ino, raw).map_err(|e| Errno::from(ExtErr(e)))
    }

    /// Read `buf.len()` bytes of file data starting at byte `offset`.
    fn read_file_range(&self, raw: &RawInode, offset: usize, buf: &mut [u8]) -> Result<(), Errno> {
        self.core.read_file_range(raw, offset, buf).map_err(|e| Errno::from(ExtErr(e)))
    }

    /// Write `data` at byte `offset`, allocating whatever blocks are
    /// needed (including growing the file past its current size — a
    /// "hole" between the old EOF and `offset` reads back as zeros, same
    /// as any real sparse file, since unallocated `block_for_index` reads
    /// already zero-fill). Updates and persists `raw`'s size + on-disk
    /// inode record before returning.
    fn write_file_range(&self, ino: u32, raw: &mut RawInode, offset: usize, data: &[u8]) -> Result<usize, Errno> {
        self.core.write_file_range(ino, raw, offset, data).map_err(|e| Errno::from(ExtErr(e)))
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
            // No kernel-side wrapper for `visit_inode_blocks`: this and
            // `mark_reachable` below are its only two call sites, and each
            // needs a differently-typed closure — `self.core.free_block`
            // here, an in-memory bitmap mark there — so a wrapper would
            // just relay the same `Ext2Error`/`Errno` split `.map_err(ExtErr)?`
            // already handles via the `From<ExtErr>` impl above (see that
            // impl's doc comment for why it's not a direct `From<Ext2Error>`).
            self.core.visit_inode_blocks(raw, |b| self.core.free_block(b)).map_err(ExtErr)?;
            for i in 0..15 {
                raw.set_i_block(i, 0);
            }
        }
        raw.set_size(0);
        raw.set_blocks_512(0);
        Ok(())
    }

    /// Truncate a file to zero length: frees all its data blocks and
    /// persists the now-empty inode. Backs `O_TRUNC`.
    fn truncate_to_zero(&self, ino: u32, raw: &mut RawInode) -> Result<(), Errno> {
        self.free_all_blocks(raw)?;
        self.write_inode(ino, raw)
    }

    /// Read a symlink inode's target string.
    fn read_symlink_target(&self, raw: &RawInode) -> Result<String, Errno> {
        self.core.read_symlink_target(raw).map_err(|e| Errno::from(ExtErr(e)))
    }

    // ── Directory entries ────────────────────────────────────────────────

    /// Parse every directory entry out of `raw`'s data blocks (direct +
    /// indirect, same limit as file reads).
    fn read_dir_entries(&self, raw: &RawInode) -> Result<Vec<Ext2DirEntry>, Errno> {
        Ok(self.core.read_dir_entries(raw).map_err(ExtErr)?
            .into_iter()
            .map(|e| Ext2DirEntry { ino: e.ino, kind: ext2_file_type_to_vfs(e.file_type), name: e.name })
            .collect())
    }

    /// Insert a new `(name -> ino)` directory entry into `dir_raw`'s data.
    fn add_dir_entry(&self, dir_ino: u32, dir_raw: &mut RawInode, name: &str, ino: u32, kind: FileType) -> Result<(), Errno> {
        self.core.add_dir_entry(dir_ino, dir_raw, name, ino, vfs_file_type_to_ext2(kind)).map_err(|e| Errno::from(ExtErr(e)))
    }

    /// Remove the directory entry named `name` from `dir_raw`'s data.
    /// Returns the removed entry's inode number and kind.
    fn remove_dir_entry(&self, dir_raw: &RawInode, name: &str) -> Result<(u32, FileType), Errno> {
        let (ino, file_type) = self.core.remove_dir_entry(dir_raw, name).map_err(ExtErr)?;
        Ok((ino, ext2_file_type_to_vfs(file_type)))
    }

    /// Rewrite a directory's `".."` entry to point at `new_parent_ino` —
    /// used when moving (rename) a subdirectory to a different parent.
    fn set_dotdot(&self, dir_raw: &RawInode, new_parent_ino: u32) -> Result<(), Errno> {
        self.core.set_dotdot(dir_raw, new_parent_ino).map_err(|e| Errno::from(ExtErr(e)))
    }

    // ── Mount-time consistency repair ───────────────────────────────────
    //
    // Both methods below wrap `ext2::Ext2Core` methods of the same name:
    // the bitmap walk, the write ordering, and — critically — the
    // reachability-walk-before-reserved-inodes ordering in
    // `reclaim_orphans` (see `CLAUDE.md`'s "Filesystem: ext2" section,
    // "Critical ordering invariant in reclaim_orphans") all live in
    // `ext2::repair`. What stays here is the part that can't move: this
    // crate's `ktrace!`/`kernel::debug` tracing infra, which `ext2` can't
    // call without depending on the kernel — see `ext2::repair`'s own
    // module doc comment. Both core methods report what they found/fixed
    // through their return values; these wrappers turn that into a trace
    // line + (for `reclaim_orphans`) the permanent `/proc/kdebug` counter.

    /// Recompute every group's true free block/inode counts directly from
    /// its bitmap and correct the stored BGD + superblock counters if they
    /// disagree. Called once from `init()`, before this filesystem is
    /// exposed to the VFS. See `ext2::Ext2Core::reconcile_free_counts`'s
    /// own doc comment for the full rationale (why drift happens, why it's
    /// a real correctness bug and not just cosmetic). Only traces
    /// (`kdebug fs on`) whether anything drifted and the final corrected
    /// totals — see `ext2::repair::ReconcileReport`'s own doc comment.
    fn reconcile_free_counts(&self) -> Result<(), Errno> {
        let report = self.core.reconcile_free_counts().map_err(ExtErr)?;
        if report.bgd_drift || report.sb_drift {
            crate::ktrace!(
                crate::debug::FS,
                "ext2: free-count drift detected, repaired (now {} free block(s), {} free inode(s))",
                report.total_free_blocks, report.total_free_inodes
            );
        }
        Ok(())
    }

    /// Mount-time orphan scan — see the module-level ROBUSTNESS comment
    /// for the full rationale, and `ext2::Ext2Core::reclaim_orphans`'s own
    /// doc comment for the full mechanics (the reachability walk, the
    /// sweep, and the safety-critical "only sweep if the walk completed
    /// with no error at all" property). The wall clock stamped into each
    /// reclaimed inode's `i_dtime` is supplied from here for the same
    /// reason the tracing below is: `ext2` has no clock of its own and
    /// can't call into the kernel for one.
    fn reclaim_orphans(&self) -> Result<(), Errno> {
        let (freed_blocks, freed_inodes) =
            self.core.reclaim_orphans(crate::time::now_unix_secs() as u32).map_err(ExtErr)?;
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
        }
        Ok(())
    }
}

fn ext2_file_type_to_vfs(ft: u8) -> FileType {
    match ft {
        2 => FileType::Directory,
        7 => FileType::Symlink,
        3 => FileType::BlockDevice,
        4 => FileType::CharDevice,
        6 => FileType::Socket,
        _ => FileType::Regular,
    }
}

fn vfs_file_type_to_ext2(kind: FileType) -> u8 {
    match kind {
        FileType::Directory => 2,
        FileType::Symlink => 7,
        FileType::BlockDevice => 3,
        FileType::CharDevice => 4,
        FileType::Socket => 6,
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
            // Refused at open, as Linux does on a read-only mount, rather
            // than handing out a handle whose every write fails —
            // `access(W_OK)` probes writability by opening `O_WRONLY`, so
            // this is also what makes `vi` say `[Readonly]` truthfully.
            if flags.is_write() && is_read_only() {
                return Err(Errno::EROFS);
            }
            let mut raw = self.raw.clone();
            if flags.is_write() && flags.0 & OpenFlags::TRUNC.0 != 0 {
                let _guard = write_lock()?;
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
        let _guard = write_lock()?;
        if let Ok(existing) = self.lookup(name) {
            if existing.file_type() == FileType::Directory {
                return Err(Errno::EISDIR);
            }
            return Ok(existing);
        }

        let f = fs();
        let new_ino = f.core.alloc_inode(false).map_err(ExtErr)?.ok_or(Errno::ENOSPC)?;
        let mut new_raw = RawInode::zeroed(f.core.sb.inode_size as usize);
        new_raw.set_i_mode(0x8000 | 0o644);
        new_raw.set_links_count(1);
        f.write_inode(new_ino, &new_raw)?;

        let mut dir_raw = self.raw.clone();
        if let Err(e) = f.add_dir_entry(self.ino, &mut dir_raw, name, new_ino, FileType::Regular) {
            let _ = f.core.free_inode(new_ino, false); // best-effort cleanup — original error wins either way
            return Err(e);
        }
        Ok(Arc::new(Ext2Inode::new(new_ino)?))
    }

    fn mkdir(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        if !self.raw.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        let _guard = write_lock()?;
        if self.lookup(name).is_ok() {
            return Err(Errno::EEXIST);
        }

        let f = fs();
        let new_ino = f.core.alloc_inode(true).map_err(ExtErr)?.ok_or(Errno::ENOSPC)?;
        let new_block = match f.core.alloc_block().map_err(ExtErr)? {
            Some(b) => b,
            None => { let _ = f.core.free_inode(new_ino, true); return Err(Errno::ENOSPC); }
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
            let _ = f.core.free_block(new_block);
            let _ = f.core.free_inode(new_ino, true);
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
        let _guard = write_lock()?;
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
            f.core.free_inode(child_ino, false).map_err(ExtErr)?;
        } else {
            f.write_inode(child_ino, &child_raw)?;
        }
        Ok(())
    }

    fn rmdir(&self, name: &str) -> Result<(), Errno> {
        if !self.raw.is_dir() {
            return Err(Errno::ENOTDIR);
        }
        let _guard = write_lock()?;
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
        f.core.free_inode(child_ino, true).map_err(ExtErr)?;

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
        let _guard = write_lock()?;
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
        let _guard = write_lock()?;
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
        let _guard = write_lock()?;
        if self.lookup(name).is_ok() {
            return Err(Errno::EEXIST);
        }

        let f = fs();
        let new_ino = f.core.alloc_inode(false).map_err(ExtErr)?.ok_or(Errno::ENOSPC)?;
        let mut new_raw = RawInode::zeroed(f.core.sb.inode_size as usize);
        new_raw.set_i_mode(0xA000 | 0o777);
        new_raw.set_links_count(1);

        // Fast (target inline in `i_block`, no data block allocated) vs
        // slow (ordinary file content) representation — whichever fits —
        // decided by `ext2::Ext2Core::write_symlink_target`.
        // `free_all_blocks` is a safe no-op here whichever step
        // failed: on a fast-representation failure `new_raw`'s mode marks
        // it a symlink whose `size()` is still < 60 (either 0, if the
        // failure was in the inode write itself, or the target's own
        // length, if that write actually landed), so `has_block_pointers()`
        // is false and the walk it would otherwise drive never runs; on a
        // slow-representation failure partway through `write_file_range`,
        // `size` isn't updated until that call fully succeeds (see its own
        // doc comment), so the same `size < 60` short-circuit applies even
        // though some block pointers may already be set — a pre-existing
        // leak in this exact narrow window, not something this refactor
        // changes (see `write_symlink_target`'s doc comment in the `ext2`
        // crate).
        if let Err(e) = f.core.write_symlink_target(&mut new_raw, new_ino, target) {
            let _ = f.free_all_blocks(&mut new_raw);
            let _ = f.core.free_inode(new_ino, false);
            return Err(Errno::from(ExtErr(e)));
        }

        let mut dir_raw = self.raw.clone();
        if let Err(e) = f.add_dir_entry(self.ino, &mut dir_raw, name, new_ino, FileType::Symlink) {
            let _ = f.free_all_blocks(&mut new_raw); // no-op if it was a fast symlink (no blocks allocated)
            let _ = f.core.free_inode(new_ino, false);
            return Err(e);
        }
        Ok(Arc::new(Ext2Inode::new(new_ino)?))
    }

    fn chmod(&self, mode: u32) -> Result<(), Errno> {
        let _guard = write_lock()?;
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
        let _guard = write_lock().map_err(|_| FileError::IOError)?;
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
        let _guard = write_lock().map_err(|_| FileError::IOError)?;
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
