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

use crate::fs::types::{DirEntry, Errno, OpenFlags, Stat};
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
pub fn open(path: &str, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
    resolve(path)?.open(flags)
}

/// Resolve `path` and return its metadata.
pub fn stat(path: &str) -> Result<Stat, Errno> {
    Ok(resolve(path)?.stat())
}
