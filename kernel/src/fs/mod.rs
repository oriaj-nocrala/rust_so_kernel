// kernel/src/fs/mod.rs
//
// VFS public API.
//
// MODULES
//   types      — Stat, Errno, DirEntry, FileType, OpenFlags
//   vfs        — Inode + Filesystem traits, MountTable, path resolution
//   initramfs  — /bin/*  backed by embedded ELF bytes
//   devfs      — /dev/*  backed by the driver registry
//
// MOUNT LAYOUT (after init())
//   /dev  → DevFs
//   /bin  → InitramfsFs  (mounted at /bin so sys_exec uses "/bin/<name>")
//   /     → InitramfsFs  (fallback root, also serves plain-name exec)

pub mod devfs;
pub mod initramfs;
pub mod types;
pub mod vfs;

pub use types::{DirEntry, Errno, FileType, OpenFlags, Stat};

use alloc::sync::Arc;

/// Initialise the VFS: register all built-in filesystems.
///
/// Must be called once, after the memory allocator is ready, before any
/// process opens a file.
pub fn init() {
    // /dev — character devices from the driver registry
    vfs::mount("/dev", Arc::new(devfs::DevFs));
    // /bin — user-space ELF binaries from initramfs
    vfs::mount("/bin", Arc::new(initramfs::InitramfsFs));
    // /   — root (fallback; also exposes binaries without /bin prefix)
    vfs::mount("/", Arc::new(initramfs::InitramfsFs));
}

/// Open a file by absolute path.
///
/// Delegates to the VFS mount table.  Used by `sys_open`.
pub fn open(path: &str, flags: OpenFlags) -> Result<alloc::boxed::Box<dyn crate::process::file::FileHandle>, Errno> {
    vfs::open(path, flags)
}

/// Stat a file by absolute path.  Used by `sys_stat` / `sys_lstat`.
pub fn stat(path: &str) -> Result<Stat, Errno> {
    vfs::stat(path)
}
