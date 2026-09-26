// vfs/src/inode.rs
//
// ABSTRACTIONS
// ────────────
//   Inode      — a file or directory in a filesystem (reference-counted).
//                Filesystems implement this trait to expose their nodes.
//   Filesystem — a mounted filesystem instance with a root Inode.
//
// The mount table (longest-prefix matching, path resolution, mutations)
// lives in `crate::mount` (`MountTable`) — see
// `docs/fs/vfs-extraction-plan.md`.

use alloc::{boxed::Box, string::String, sync::Arc};

use crate::file::FileHandle;
use crate::types::{DirEntry, Errno, FileType, OpenFlags, Stat};

// ── Inode ────────────────────────────────────────────────────────────────────

/// A VFS inode — the identity and metadata of a file or directory.
///
/// Inodes are reference-counted so they can be shared (e.g. two open FDs on
/// the same file share the inode but each has its own `FileHandle` cursor).
///
/// Default implementations for `lookup` and `readdir` return `ENOTDIR`; only
/// directory inodes need to override them.
pub trait Inode: Send + Sync {
    /// Inode metadata (type, size, permissions, …).
    fn stat(&self) -> Stat;

    /// Open this inode, producing an independent `FileHandle` with its own
    /// cursor.  Called by `vfs::open` and `sys_open`.
    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno>;

    /// Look up a child by name.  Valid only on directory inodes.
    fn lookup(&self, _name: &str) -> Result<Arc<dyn Inode>, Errno> {
        Err(Errno::ENOTDIR)
    }

    /// Iterate directory entries.
    ///
    /// `offset` is an opaque, monotonically-increasing index (starts at 0).
    /// Returns `Ok(None)` when the directory is exhausted.
    /// Returns `Err(ENOTDIR)` for non-directory inodes.
    fn readdir(&self, _offset: u64) -> Result<Option<DirEntry>, Errno> {
        Err(Errno::ENOTDIR)
    }

    /// Create a new child `name` under this (directory) inode and return it.
    ///
    /// Called by `vfs::open` when `O_CREAT` is set and the target path
    /// doesn't exist yet. Read-only filesystems (initramfs, devfs) keep the
    /// default, which rejects with `EROFS`; writable ones (ramfs) override
    /// it.
    fn create(&self, _name: &str) -> Result<Arc<dyn Inode>, Errno> {
        Err(Errno::EROFS)
    }

    /// This inode's type, derived from `stat().st_mode`'s type bits.
    ///
    /// Lets directory implementations that store heterogeneous children as
    /// `Arc<dyn Inode>` (files and subdirectories side by side, e.g. ramfs)
    /// tell them apart without needing a parallel enum or downcasting.
    fn file_type(&self) -> FileType {
        match self.stat().st_mode & 0o170000 {
            0o040000 => FileType::Directory,
            0o020000 => FileType::CharDevice,
            0o060000 => FileType::BlockDevice,
            0o120000 => FileType::Symlink,
            0o140000 => FileType::Socket,
            _        => FileType::Regular,
        }
    }

    /// Create a new subdirectory `name` under this (directory) inode.
    ///
    /// Same read-only-by-default convention as `create()`.
    fn mkdir(&self, _name: &str) -> Result<Arc<dyn Inode>, Errno> {
        Err(Errno::EROFS)
    }

    /// Remove a non-directory child `name`. Must fail with `EISDIR` if
    /// `name` refers to a directory (use `rmdir` for those instead).
    fn unlink(&self, _name: &str) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    /// Remove an empty directory child `name`. Must fail with `ENOTDIR` if
    /// `name` isn't a directory, or `ENOTEMPTY` if it has entries.
    fn rmdir(&self, _name: &str) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    /// Detach and return child `name` (file or directory, empty or not) —
    /// the "remove" half of a rename. Unlike `unlink`/`rmdir`, this never
    /// checks emptiness: POSIX `rename()` allows moving non-empty
    /// directories, only `rmdir()` requires them empty.
    fn take_child(&self, _name: &str) -> Result<Arc<dyn Inode>, Errno> {
        Err(Errno::EROFS)
    }

    /// Insert an already-existing inode under a new name — the "attach"
    /// half of a rename. Fails with `EEXIST` if `name` is already taken
    /// (this VFS doesn't support rename-clobbering an existing target).
    fn insert_child(&self, _name: &str, _node: Arc<dyn Inode>) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    /// Read this inode's symlink target — a path string, either absolute
    /// or relative to the symlink's own containing directory. Only
    /// meaningful on `Symlink`-type inodes (see `file_type`); the default
    /// matches `readlink(2)` on a non-symlink.
    fn readlink(&self) -> Result<String, Errno> {
        Err(Errno::EINVAL)
    }

    /// Create a new symlink child `name` under this (directory) inode,
    /// pointing at `target` (an arbitrary string — not resolved or checked
    /// to exist, matching real `symlink(2)`: a dangling target is legal).
    ///
    /// Same read-only-by-default convention as `create()`/`mkdir()`.
    fn symlink(&self, _name: &str, _target: &str) -> Result<Arc<dyn Inode>, Errno> {
        Err(Errno::EROFS)
    }

    /// Create an AF_UNIX socket node `name` under this (directory) inode —
    /// what `bind()` does with a pathname address.
    ///
    /// The node holds no data of its own: it is a name in the filesystem
    /// that a `connect()` can resolve and an `unlink()` can remove, while
    /// the socket itself lives in the kernel's socket table. Opening one is
    /// deliberately not supported (Linux answers `ENXIO`); the only way to
    /// reach the socket is `connect()`/`sendto()` with its address.
    ///
    /// Same read-only-by-default convention as `create()`/`mkdir()`/
    /// `symlink()`.
    fn mksocket(&self, _name: &str) -> Result<Arc<dyn Inode>, Errno> {
        Err(Errno::EROFS)
    }

    /// Change this inode's permission bits (the low 12 bits of `st_mode`
    /// — `chmod(2)`'s `mode` argument). Default `Ok(())` matches the
    /// pre-existing "validity-checked stub" behavior every filesystem had
    /// before this method existed (`sys_chmod`/`sys_fchmod` used to just
    /// confirm the path/fd resolved and otherwise no-op) — filesystems
    /// with no real per-inode permission storage (ramfs, devfs,
    /// initramfs, procfs) keep exactly that behavior by inheriting this
    /// default. Only `ext2::Ext2Inode` overrides it: it has a real on-disk
    /// `i_mode` field to persist the change into.
    fn chmod(&self, _mode: u32) -> Result<(), Errno> {
        Ok(())
    }

    /// `utimensat(2)`: set the access and/or modification time (Unix
    /// seconds; `None` leaves that one as it is), and with it the change
    /// time to now. A filesystem that keeps no times is read-only here:
    /// `EROFS`, the same default as `create()`/`mkdir()`.
    fn set_times(&self, _atime: Option<u64>, _mtime: Option<u64>) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    /// Type-erased downcast handle. Lets a filesystem whose directory
    /// entries can only reference its own inodes (ext2: a dirent is
    /// literally an inode *number*, meaningless outside that filesystem)
    /// verify, inside `insert_child`, that a node handed across the
    /// generic `Arc<dyn Inode>` VFS boundary during `rename()` is actually
    /// one of its own before trusting its inode number — otherwise a
    /// cross-filesystem rename could write a dirent pointing at whatever
    /// inode number happens to collide in the wrong filesystem.
    ///
    /// No default body: `Self` has no implicit `Sized` bound inside a
    /// trait definition (traits stay dyn-compatible by default), so a
    /// shared `{ self }` default can't coerce `&Self` to `&dyn Any`
    /// without also adding `where Self: Sized` — which would exclude the
    /// method from the vtable entirely, making it uncallable through
    /// `Arc<dyn Inode>` (the whole point). Every implementor below adds
    /// the same one-line `{ self }` body instead, where `Self` is the
    /// concrete, `Sized` type.
    fn as_any(&self) -> &dyn core::any::Any;
}

// ── Filesystem ───────────────────────────────────────────────────────────────

/// A mounted filesystem instance.
///
/// Implement this trait to plug a new filesystem (initramfs, ext2, tmpfs, …)
/// into the VFS mount table.
pub trait Filesystem: Send + Sync {
    /// Human-readable filesystem type name (shown in mount listings).
    fn name(&self) -> &str;

    /// Root inode of this filesystem.
    ///
    /// `Result`-returning (not a bare `Arc<dyn Inode>`) because this is
    /// re-invoked on *every* path resolution into this mount (see
    /// `resolve_inner` below), not just at mount time — for most
    /// filesystems here (ramfs, devfs, initramfs, procfs) the root inode
    /// can never fail to produce, so they just wrap it in `Ok`. `ext2` is
    /// the exception: its root is a real disk read that can genuinely
    /// fail, and this `Result` is what lets that failure propagate as a
    /// clean `EIO` through `resolve()` like any other failed path-
    /// resolution step, instead of needing a synthetic stand-in inode.
    fn root(&self) -> Result<Arc<dyn Inode>, Errno>;
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use core::any::Any;

    // A minimal inode implementing only the required methods, to pin down
    // exactly what every default does.
    struct MinimalInode;

    impl Inode for MinimalInode {
        fn stat(&self) -> Stat {
            Stat::regular(1, 0)
        }
        fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
            Err(Errno::ENOSYS)
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn default_lookup_is_enotdir() {
        assert_eq!(MinimalInode.lookup("x").err(), Some(Errno::ENOTDIR));
    }

    #[test]
    fn default_readdir_is_enotdir() {
        assert_eq!(MinimalInode.readdir(0).err(), Some(Errno::ENOTDIR));
    }

    #[test]
    fn default_create_is_erofs() {
        assert_eq!(MinimalInode.create("x").err(), Some(Errno::EROFS));
    }

    #[test]
    fn default_mkdir_is_erofs() {
        assert_eq!(MinimalInode.mkdir("x").err(), Some(Errno::EROFS));
    }

    #[test]
    fn default_unlink_is_erofs() {
        assert_eq!(MinimalInode.unlink("x"), Err(Errno::EROFS));
    }

    #[test]
    fn default_rmdir_is_erofs() {
        assert_eq!(MinimalInode.rmdir("x"), Err(Errno::EROFS));
    }

    #[test]
    fn default_take_child_is_erofs() {
        assert_eq!(MinimalInode.take_child("x").err(), Some(Errno::EROFS));
    }

    #[test]
    fn default_insert_child_is_erofs() {
        let node: Arc<dyn Inode> = Arc::new(MinimalInode);
        assert_eq!(MinimalInode.insert_child("x", node), Err(Errno::EROFS));
    }

    #[test]
    fn default_symlink_is_erofs() {
        assert_eq!(MinimalInode.symlink("x", "target").err(), Some(Errno::EROFS));
    }

    #[test]
    fn default_readlink_is_einval() {
        // Matches real readlink(2) on a non-symlink.
        assert_eq!(MinimalInode.readlink(), Err(Errno::EINVAL));
    }

    #[test]
    fn default_chmod_is_ok() {
        // Deliberately NOT an error: reproduces the pre-existing
        // "validity-checked stub" behavior every filesystem with no real
        // per-inode permission storage had before chmod() existed —
        // sys_chmod/sys_fchmod used to just confirm the path/fd resolved
        // and otherwise no-op. This is the one default in the trait that
        // does not reject.
        assert_eq!(MinimalInode.chmod(0o755), Ok(()));
    }

    // ── file_type() ──────────────────────────────────────────────────────
    //
    // `file_type()` derives purely from `stat().st_mode & 0o170000` — build
    // a `Stat` with an arbitrary raw mode (its fields are all `pub`) and
    // check what `file_type()` reports for it.

    struct ModeInode(u32); // st_mode to report

    impl Inode for ModeInode {
        fn stat(&self) -> Stat {
            let mut s = Stat::regular(1, 0);
            s.st_mode = self.0;
            s
        }
        fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
            Err(Errno::ENOSYS)
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[test]
    fn file_type_directory() {
        assert_eq!(ModeInode(0o040000).file_type(), FileType::Directory);
    }

    #[test]
    fn file_type_chardevice() {
        assert_eq!(ModeInode(0o020000).file_type(), FileType::CharDevice);
    }

    #[test]
    fn file_type_blockdevice() {
        assert_eq!(ModeInode(0o060000).file_type(), FileType::BlockDevice);
    }

    #[test]
    fn file_type_symlink() {
        assert_eq!(ModeInode(0o120000).file_type(), FileType::Symlink);
    }

    #[test]
    fn file_type_regular() {
        assert_eq!(ModeInode(0o100000).file_type(), FileType::Regular);
    }

    #[test]
    fn file_type_socket() {
        assert_eq!(ModeInode(0o140000).file_type(), FileType::Socket);
    }

    #[test]
    fn file_type_unknown_mode_defaults_to_regular() {
        // A FIFO (0o010000) has no FileType variant of its own — this
        // kernel has no named pipes — so the trait's `_ =>` arm applies.
        assert_eq!(ModeInode(0o010000).file_type(), FileType::Regular);
    }

    #[test]
    fn file_type_permission_bits_do_not_affect_result() {
        // 0o040755 is still a directory: the low 12 permission bits must
        // not leak into the type-bit match.
        assert_eq!(ModeInode(0o040755).file_type(), FileType::Directory);
        assert_eq!(ModeInode(0o100644).file_type(), FileType::Regular);
    }

    #[test]
    fn overridden_defaults_win() {
        struct OverridingInode;
        impl Inode for OverridingInode {
            fn stat(&self) -> Stat {
                Stat::regular(1, 0)
            }
            fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
                Err(Errno::ENOSYS)
            }
            fn lookup(&self, _name: &str) -> Result<Arc<dyn Inode>, Errno> {
                Ok(Arc::new(OverridingInode))
            }
            fn chmod(&self, mode: u32) -> Result<(), Errno> {
                if mode == 0 { Err(Errno::EINVAL) } else { Ok(()) }
            }
            fn as_any(&self) -> &dyn Any {
                self
            }
        }
        assert!(OverridingInode.lookup("x").is_ok());
        assert_eq!(OverridingInode.chmod(0), Err(Errno::EINVAL));
    }
}
