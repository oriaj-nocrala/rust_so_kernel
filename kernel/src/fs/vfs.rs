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

    /// Read this inode's symlink target — a path string, either absolute
    /// or relative to the symlink's own containing directory. Only
    /// meaningful on `Symlink`-type inodes (see `file_type`); the default
    /// matches `readlink(2)` on a non-symlink.
    fn readlink(&self) -> Result<alloc::string::String, Errno> {
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

/// Names of filesystems mounted exactly one path component below `parent`
/// (e.g. `direct_children("/")` → `["dev", "tmp", "proc", ...]`).
///
/// On real Linux, `/proc`, `/dev`, etc. show up in `ls /` because they're
/// real, pre-existing empty directories in the root filesystem that a mount
/// later overlays — traversal redirects into the mount, but the parent
/// directory's own listing is what makes the mountpoint visible at all.
/// This is the equivalent for our synthetic root: `fs::initramfs`'s root
/// directory calls this to list every other mount as a real (if empty from
/// its perspective — actual traversal never reaches them, since a longer,
/// more specific mount prefix always wins in `resolve_inner`) subdirectory
/// entry, instead of only ever showing its own "bin".
pub fn direct_children(parent: &str) -> Vec<&'static str> {
    let table = mounts().lock();
    table.iter().filter_map(|e| {
        if e.prefix == parent {
            return None; // don't list the mount itself as its own child
        }
        let rel = if parent == "/" {
            e.prefix.strip_prefix('/')?
        } else {
            e.prefix.strip_prefix(parent)?.strip_prefix('/')?
        };
        if rel.is_empty() || rel.contains('/') {
            None // not a direct child: either self ("") or nested deeper
        } else {
            Some(rel)
        }
    }).collect()
}

// ── Path resolution ──────────────────────────────────────────────────────────

/// Symlink chains longer than this are rejected with `ELOOP`, same spirit
/// as real Linux's (much larger) `MAXSYMLINKS` — this kernel only ever
/// produces short, deliberately-built chains (procfs), so a small bound
/// is enough to catch a real cycle without being a meaningful limitation.
const MAX_SYMLINK_HOPS: u32 = 8;

/// Resolve an absolute path to its inode, following symlinks — including
/// one at the final path component (matches `open()`/`stat()` semantics).
/// Use `resolve_no_follow` for `lstat`/`readlink`, which must see the
/// symlink itself rather than whatever it points to.
///
/// # Errors
/// - `EINVAL`  — path does not start with `/`
/// - `ENOENT`  — no mount found or a path component doesn't exist
/// - `ENOTDIR` — a non-terminal component is not a directory
/// - `ELOOP`   — more than `MAX_SYMLINK_HOPS` symlinks chained together
pub fn resolve(path: &str) -> Result<Arc<dyn Inode>, Errno> {
    resolve_inner(path, true, MAX_SYMLINK_HOPS)
}

/// Like `resolve`, but never follows a symlink at the *final* path
/// component — intermediate components are still always followed (real
/// `lstat(2)`/`readlink(2)` behavior: `/a/link/b` still requires `link` to
/// be a real, followable directory, only the leaf is left alone).
pub fn resolve_no_follow(path: &str) -> Result<Arc<dyn Inode>, Errno> {
    resolve_inner(path, false, MAX_SYMLINK_HOPS)
}

fn resolve_inner(path: &str, follow_final: bool, hops_left: u32) -> Result<Arc<dyn Inode>, Errno> {
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
    let mount_prefix = entry.prefix; // `&'static str` — cheap to keep past `drop(table)` below

    let mut node: Arc<dyn Inode> = entry.fs.root()?;
    drop(table); // don't hold the mount table locked across a recursive resolve()

    let components: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    let last_idx = components.len().checked_sub(1);

    for (i, component) in components.iter().enumerate() {
        match *component {
            "."  => { /* stay at current directory */ }
            ".." => {
                // Every caller normalizes `..`/`.` away before a path
                // reaches this function (`resolve_path` in the syscall
                // layer, and this function's own relative-symlink-target
                // handling above, both go through `normalize_path` first
                // — verified against every `vfs::resolve*`/`vfs::open`/
                // `fs::stat` call site in the kernel). A raw `..` showing
                // up here anyway means some caller skipped that step; fail
                // loudly with `EINVAL` instead of silently resolving the
                // wrong file the way a no-op here used to.
                return Err(Errno::EINVAL);
            }
            name => {
                node = node.lookup(name)?;
                let is_final = Some(i) == last_idx;
                if node.file_type() == FileType::Symlink && (!is_final || follow_final) {
                    if hops_left == 0 {
                        return Err(Errno::ELOOP);
                    }
                    let target = node.readlink()?;
                    // A relative target resolves against the symlink's own
                    // containing directory (real symlink(2)/readlink(2)
                    // semantics), not root — matters now that ext2's real
                    // `symlink()` can produce genuinely relative targets
                    // (e.g. `symlink("realfile.txt", "/mnt/link")`, the
                    // common case for a real `ln -s`). Reconstruct that
                    // directory from the mount prefix plus every path
                    // component consumed so far (everything before this
                    // one, i.e. `components[..i]`) — `Inode` itself has no
                    // notion of "my containing directory" to ask for
                    // directly.
                    let abs_target = if target.starts_with('/') {
                        target
                    } else {
                        let mut dir_path = alloc::string::String::from(mount_prefix);
                        for c in &components[..i] {
                            dir_path.push('/');
                            dir_path.push_str(c);
                        }
                        normalize_path(&dir_path, &target)
                    };
                    node = resolve_inner(&abs_target, follow_final, hops_left - 1)?;
                }
            }
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

/// Turn a possibly-relative `path` into a clean, normalized absolute path,
/// resolving `.`/`..` components lexically against `cwd` (or against `path`
/// itself if it's already absolute — `/a/b/../c` still needs collapsing,
/// since `resolve()`'s own `..` handling is a no-op placeholder).
///
/// Purely string-based: doesn't touch the filesystem, so it can't tell
/// `../` past `/` from `../` past a real directory — both just get dropped,
/// matching how a shell's `..` behaves at the true root.
pub fn normalize_path(cwd: &str, path: &str) -> alloc::string::String {
    let mut stack: Vec<&str> = if path.starts_with('/') {
        Vec::new()
    } else {
        cwd.split('/').filter(|s| !s.is_empty()).collect()
    };

    for component in path.split('/').filter(|s| !s.is_empty()) {
        match component {
            "."  => {}
            ".." => { stack.pop(); }
            name => stack.push(name),
        }
    }

    if stack.is_empty() {
        alloc::string::String::from("/")
    } else {
        let mut out = alloc::string::String::from("/");
        out.push_str(&stack.join("/"));
        out
    }
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

/// Create a symlink at `path` pointing at `target`. `path`'s parent
/// directory is resolved (and must exist and be writable); `target` is
/// stored as-is, unresolved — matches real `symlink(2)`.
pub fn symlink(target: &str, path: &str) -> Result<(), Errno> {
    let (dir_path, leaf) = split_parent(path)?;
    resolve(dir_path)?.symlink(leaf, target)?;
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

// ── Shared getdents64 helpers ───────────────────────────────────────────────
//
// Every directory `FileHandle` in this VFS packs `DirEntry`s into
// `linux_dirent64` records the same way — only *where the entries come
// from* differs, which is why this is two helpers, not one. Before these
// existed, seven directory handles (devfs x2, initramfs, procfs x2, ramfs,
// ext2) each hand-rolled an identical packing loop.

/// Walk `dir.readdir(offset)` one entry at a time, packing each into `buf`
/// as a `linux_dirent64` record, until either the directory is exhausted
/// or the next entry wouldn't fit. For directory handles backed by a
/// cheap-to-call-repeatedly `Inode::readdir` (devfs, initramfs, procfs) —
/// see `getdents64_from_snapshot` for handles that pre-collect their
/// listing into a `Vec<DirEntry>` at `open()` time instead (ramfs, ext2).
pub fn getdents64_via_readdir(dir: &dyn Inode, offset: &mut u64, buf: &mut [u8]) -> i64 {
    let mut written: usize = 0;
    loop {
        let entry = match dir.readdir(*offset) {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => return e.as_i64(),
        };
        let needed = entry.dirent64_size();
        if written + needed > buf.len() {
            break;
        }
        let next_off = *offset as i64 + 1;
        entry.write_dirent64(next_off, &mut buf[written..written + needed]);
        written += needed;
        *offset += 1;
    }
    written as i64
}

/// Same packing loop as `getdents64_via_readdir`, indexed by position
/// through an already-collected `Vec<DirEntry>` snapshot instead of
/// re-querying `readdir()` per entry.
pub fn getdents64_from_snapshot(entries: &[crate::fs::types::DirEntry], offset: &mut usize, buf: &mut [u8]) -> i64 {
    let mut written: usize = 0;
    while *offset < entries.len() {
        let entry = &entries[*offset];
        let needed = entry.dirent64_size();
        if written + needed > buf.len() {
            break;
        }
        let next_off = *offset as i64 + 1;
        entry.write_dirent64(next_off, &mut buf[written..written + needed]);
        written += needed;
        *offset += 1;
    }
    written as i64
}
