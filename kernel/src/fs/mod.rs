// kernel/src/fs/mod.rs
//
// Virtual File System (VFS) — Phase 1: initramfs + devfs
//
// Path resolution rules:
//   /dev/*  → driver registry (crate::drivers)
//   /bin/*  → initramfs (embedded ELF bytes)
//
// Future mounts (ext2, tmpfs) will be added here without touching syscalls.

pub mod initramfs;

use alloc::boxed::Box;
use crate::process::file::FileHandle;

/// Open a file by absolute path.
///
/// Returns a `FileHandle` ready for read/write, or `None` if not found.
/// This is the single entry point used by `sys_open`.
pub fn open(path: &str) -> Option<Box<dyn FileHandle>> {
    if path.starts_with("/dev/") {
        return crate::drivers::open_device(path);
    }

    if let Some(name) = path.strip_prefix("/bin/") {
        return initramfs::open(name);
    }

    None
}

/// Return raw bytes for a file — used by `sys_exec` to feed the ELF loader.
///
/// Returns `&'static [u8]` because initramfs data is embedded at compile time.
/// For future on-disk filesystems this will change to an owned buffer.
pub fn read_bytes(path: &str) -> Option<&'static [u8]> {
    if let Some(name) = path.strip_prefix("/bin/") {
        return initramfs::bytes(name);
    }
    None
}
