// kernel/src/fs/vfs.rs
//
// Virtual File System adapter.
//
// `Inode`/`Filesystem`, the getdents64 packing helpers, and — as of
// `docs/fs/vfs-extraction-plan.md` step 4 — the mount table itself
// (longest-prefix-match path resolution, symlink following, and the
// mutating VFS ops) all live in the host-testable `vfs` crate now
// (`vfs::inode`, `vfs::dirent`, `vfs::mount::MountTable`, `vfs::path`).
// This file is the thin kernel-side adapter: it owns the single global
// `MOUNTS: MountTable` instance and re-exposes every operation as a free
// function with the exact same signature it always had, so every
// `crate::fs::vfs::{...}` call site elsewhere in the kernel (`fs/devfs.rs`,
// `fs/initramfs.rs`, `fs/procfs.rs`, `fs/ramfs.rs`, `fs/ext2.rs`,
// `hw_tests.rs`, `process/syscall/*.rs`) needed no changes at all — same
// pattern as `EXT2: Once<Ext2Fs>` in `kernel/src/fs/ext2.rs`.
use alloc::{boxed::Box, sync::Arc, vec::Vec};

use crate::fs::types::{Errno, OpenFlags, Stat};
use crate::process::file::FileHandle;
use vfs::mount::MountTable;

// `Inode`/`Filesystem` and the getdents64 packing helpers live in the
// host-testable `vfs` crate (`vfs::inode`/`vfs::dirent`). This re-export
// leaves every `use crate::fs::vfs::{Inode, Filesystem}` (and the
// `getdents64_*` call sites) in `fs/devfs.rs`, `fs/initramfs.rs`,
// `fs/procfs.rs`, `fs/ramfs.rs`, `fs/ext2.rs` untouched.
pub use vfs::dirent::{getdents64_from_snapshot, getdents64_via_readdir};
pub use vfs::inode::{Filesystem, Inode};
pub use vfs::path::normalize_path;

// ── Mount table ──────────────────────────────────────────────────────────────

/// Global mount table. `MountTable::new()` is a `const fn` (both
/// `Mutex::new` and `Vec::new` are const), so this can be a plain `static`
/// initialized at compile time — no `spin::Once` indirection needed, unlike
/// `EXT2: Once<Ext2Fs>` (whose `Ext2Fs` construction genuinely needs to run
/// at mount time, against a real disk).
static MOUNTS: MountTable = MountTable::new();

/// Mount `fs` at `prefix`.
pub fn mount(prefix: &'static str, fs: Arc<dyn Filesystem>) {
    MOUNTS.mount(prefix, fs)
}

/// Names of filesystems mounted exactly one path component below `parent`.
/// See `MountTable::direct_children`'s doc comment for the full rationale
/// (this is what makes `dev`/`tmp`/`mnt`/`proc` show up in `ls /`).
pub fn direct_children(parent: &str) -> Vec<&'static str> {
    MOUNTS.direct_children(parent)
}

/// Resolve an absolute path to its inode, following symlinks — including
/// one at the final path component. See `MountTable::resolve`.
pub fn resolve(path: &str) -> Result<Arc<dyn Inode>, Errno> {
    MOUNTS.resolve(path)
}

/// Like `resolve`, but never follows a symlink at the final path component.
/// See `MountTable::resolve_no_follow`.
pub fn resolve_no_follow(path: &str) -> Result<Arc<dyn Inode>, Errno> {
    MOUNTS.resolve_no_follow(path)
}

/// Resolve `path` and open it, returning an FD-ready `FileHandle`.
pub fn open(path: &str, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
    MOUNTS.open(path, flags)
}

/// Resolve `path` and return its metadata.
pub fn stat(path: &str) -> Result<Stat, Errno> {
    MOUNTS.stat(path)
}

/// Create a new directory at `path`.
pub fn mkdir(path: &str) -> Result<(), Errno> {
    MOUNTS.mkdir(path)
}

/// Create a symlink at `path` pointing at `target`.
pub fn symlink(target: &str, path: &str) -> Result<(), Errno> {
    MOUNTS.symlink(target, path)
}

/// Create an AF_UNIX socket node at `path` (what `bind()` does with a
/// pathname address).
pub fn mksocket(path: &str) -> Result<(), Errno> {
    MOUNTS.mksocket(path)
}

/// Remove the file at `path` (fails with `EISDIR` on directories).
pub fn unlink(path: &str) -> Result<(), Errno> {
    MOUNTS.unlink(path)
}

/// Remove the empty directory at `path`.
pub fn rmdir(path: &str) -> Result<(), Errno> {
    MOUNTS.rmdir(path)
}

/// Move/rename `old_path` to `new_path`.
pub fn rename(old_path: &str, new_path: &str) -> Result<(), Errno> {
    MOUNTS.rename(old_path, new_path)
}
