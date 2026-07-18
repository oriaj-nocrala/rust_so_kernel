// kernel/src/fs/vfs.rs
//
// Virtual File System core.
//
// ABSTRACTIONS
// ────────────
//   Inode      — a file or directory in a filesystem (reference-counted).
//                Filesystems implement this trait to expose their nodes.
//   Filesystem — a mounted filesystem instance with a root Inode.
//   MountTable — ordered list of (prefix, Filesystem) pairs; resolved by
//                longest-prefix match.
//
// PATH RESOLUTION
// ───────────────
//   resolve("/dev/console")
//     1. Longest-prefix match → mount at "/dev"
//     2. rel_path = "console"
//     3. DevFs.root().lookup("console") → DevInode
//
// OPEN
// ────
//   open(path, flags) = resolve(path)?.open(flags)
//   Returns a Box<dyn FileHandle> ready for read/write in the FD table.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use spin::{Mutex, Once};

use crate::fs::types::{DirEntry, Errno, FileType, OpenFlags, Stat};
use crate::process::file::FileHandle;

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
    fn root(&self) -> Arc<dyn Inode>;
}

// ── Mount table ──────────────────────────────────────────────────────────────

struct MountEntry {
    /// Absolute path prefix (e.g. "/dev", "/").  No trailing slash.
    prefix: &'static str,
    fs:     Arc<dyn Filesystem>,
}

/// Global mount table.  Initialised lazily; entries are kept sorted by
/// descending prefix length for correct longest-prefix matching.
static MOUNTS: Once<Mutex<Vec<MountEntry>>> = Once::new();

fn mounts() -> &'static Mutex<Vec<MountEntry>> {
    MOUNTS.call_once(|| Mutex::new(Vec::new()))
}

/// Mount `fs` at `prefix`.
///
/// The table is kept sorted longest-prefix-first so that `resolve` can do a
/// simple linear scan and stop at the first match.
pub fn mount(prefix: &'static str, fs: Arc<dyn Filesystem>) {
    let mut table = mounts().lock();
    table.push(MountEntry { prefix, fs });
    table.sort_by(|a, b| b.prefix.len().cmp(&a.prefix.len()));
}

// ── Path resolution ──────────────────────────────────────────────────────────

/// Resolve an absolute path to its inode.
///
/// # Errors
/// - `EINVAL`  — path does not start with `/`
/// - `ENOENT`  — no mount found or a path component doesn't exist
/// - `ENOTDIR` — a non-terminal component is not a directory
pub fn resolve(path: &str) -> Result<Arc<dyn Inode>, Errno> {
    if !path.starts_with('/') {
        return Err(Errno::EINVAL);
    }

    let table = mounts().lock();

    // Longest-prefix match: the table is sorted so the first hit is correct.
    let entry = table.iter().find(|e| {
        path == e.prefix
            || path.starts_with(e.prefix)
                && (e.prefix == "/"
                    || path[e.prefix.len()..].starts_with('/'))
    }).ok_or(Errno::ENOENT)?;

    // Strip the mount prefix to get the path relative to this filesystem.
    let rel = if entry.prefix == "/" {
        &path[1..]
    } else {
        path[entry.prefix.len()..].trim_start_matches('/')
    };

    let mut node: Arc<dyn Inode> = entry.fs.root();

    for component in rel.split('/').filter(|s| !s.is_empty()) {
        match component {
            "."  => { /* stay at current directory */ }
            ".." => { /* parent not implemented yet — stay put */ }
            name => { node = node.lookup(name)?; }
        }
    }

    Ok(node)
}

/// Resolve `path` and open it, returning an FD-ready `FileHandle`.
///
/// If `path` doesn't exist and `O_CREAT` is set, resolves the *parent*
/// directory instead and asks it to `create()` the leaf component.
pub fn open(path: &str, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
    match resolve(path) {
        Ok(inode) => inode.open(flags),
        Err(Errno::ENOENT) if flags.0 & OpenFlags::CREAT.0 != 0 => create_and_open(path, flags),
        Err(e) => Err(e),
    }
}

fn create_and_open(path: &str, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
    let (dir_path, leaf) = split_parent(path)?;
    let dir = resolve(dir_path)?;
    let inode = dir.create(leaf)?;
    inode.open(flags)
}

/// Resolve `path` and return its metadata.
pub fn stat(path: &str) -> Result<Stat, Errno> {
    Ok(resolve(path)?.stat())
}

/// Split `path` into (parent directory path, leaf component name).
///
/// `"/tmp/sub/file"` → `("/tmp/sub", "file")`; `"/file"` → `("/", "file")`.
fn split_parent(path: &str) -> Result<(&str, &str), Errno> {
    let idx = path.rfind('/').ok_or(Errno::EINVAL)?;
    let leaf = &path[idx + 1..];
    if leaf.is_empty() {
        return Err(Errno::EINVAL);
    }
    let dir_path = if idx == 0 { "/" } else { &path[..idx] };
    Ok((dir_path, leaf))
}

/// Create a new directory at `path`.
pub fn mkdir(path: &str) -> Result<(), Errno> {
    let (dir_path, leaf) = split_parent(path)?;
    resolve(dir_path)?.mkdir(leaf)?;
    Ok(())
}

/// Remove the file at `path` (fails with `EISDIR` on directories).
pub fn unlink(path: &str) -> Result<(), Errno> {
    let (dir_path, leaf) = split_parent(path)?;
    resolve(dir_path)?.unlink(leaf)
}

/// Remove the empty directory at `path`.
pub fn rmdir(path: &str) -> Result<(), Errno> {
    let (dir_path, leaf) = split_parent(path)?;
    resolve(dir_path)?.rmdir(leaf)
}

/// Move/rename `old_path` to `new_path`. Both must resolve to directories
/// on the same mounted filesystem (no cross-filesystem support — the
/// target parent's `insert_child` will fail with `EROFS`/`ENOSYS` if not).
pub fn rename(old_path: &str, new_path: &str) -> Result<(), Errno> {
    let (old_dir, old_leaf) = split_parent(old_path)?;
    let (new_dir, new_leaf) = split_parent(new_path)?;
    let old_parent = resolve(old_dir)?;
    let new_parent = resolve(new_dir)?;

    let node = old_parent.take_child(old_leaf)?;
    if let Err(e) = new_parent.insert_child(new_leaf, node.clone()) {
        // Best-effort rollback so a failed rename doesn't just lose the file.
        let _ = old_parent.insert_child(old_leaf, node);
        return Err(e);
    }
    Ok(())
}
