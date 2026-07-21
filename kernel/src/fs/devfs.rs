// kernel/src/fs/devfs.rs
//
// Device filesystem — exposes the driver registry as a VFS namespace.
//
// LAYOUT
// ──────
//   /dev/   (DevDirInode)
//   ├── console
//   ├── null
//   ├── zero
//   ├── fb
//   ├── kbd
//   └── input/   (InputDirInode — one hardcoded level of nesting)
//       └── event0
//
// Each device inode delegates `open()` to `crate::drivers::open_device`.
// Inode numbers: 100 = /dev directory, 101+ = individual devices.
//
// `crate::drivers::DEVICES` entries are just path strings — nothing stops
// registering one with a "/" in it (e.g. "/dev/input/event0", matching the
// real Linux evdev layout). But `fs::vfs::resolve` walks a path one
// component at a time via `Inode::lookup`, and this filesystem is
// otherwise flat: `DevDirInode::lookup("input")` would try
// `has_device("/dev/input")` (not a device — ENOENT) unless "input" is
// special-cased as a subdirectory first. `InputDirInode` is that one
// hardcoded case, not a general nested-devfs mechanism — add another if a
// second nested device ever shows up.

use alloc::{boxed::Box, string::String, sync::Arc};

use crate::fs::{
    types::{DirEntry, Errno, FileType, OpenFlags, Stat},
    vfs::{Filesystem, Inode},
};
use crate::process::file::{FileError, FileHandle, FileResult};

// ── Filesystem ───────────────────────────────────────────────────────────────

pub struct DevFs;

impl Filesystem for DevFs {
    fn name(&self) -> &str { "devfs" }

    fn root(&self) -> Result<Arc<dyn Inode>, Errno> {
        Ok(Arc::new(DevDirInode))
    }
}

/// Fixed inode number for the synthetic `/dev/input` directory — outside
/// the `101..` range individual devices use (`device_index() + 101`),
/// since it isn't itself a `DEVICES` entry.
const INPUT_DIR_INO: u64 = 100_000;

// ── Directory inode ──────────────────────────────────────────────────────────

struct DevDirInode;

impl Inode for DevDirInode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        Stat::dir(100)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        Ok(Box::new(DevDirHandle { offset: 0 }))
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        if name == "input" {
            return Ok(Arc::new(InputDirInode));
        }
        let path = alloc::format!("/dev/{}", name);
        if crate::drivers::has_device(&path) {
            // Inode number: hash the device index for stability
            let ino = crate::drivers::device_index(&path)
                .map(|i| i as u64 + 101)
                .unwrap_or(101);
            Ok(Arc::new(DevInode { path: String::from(path), ino }))
        } else {
            Err(Errno::ENOENT)
        }
    }

    fn readdir(&self, offset: u64) -> Result<Option<DirEntry>, Errno> {
        match offset {
            0 => Ok(Some(DirEntry::new(100, FileType::Directory, b"."))),
            1 => Ok(Some(DirEntry::new(100, FileType::Directory, b".."))),
            // Synthetic "input" subdirectory entry — not itself a
            // registered device, see InputDirInode.
            2 => Ok(Some(DirEntry::new(INPUT_DIR_INO, FileType::Directory, b"input"))),
            n => {
                // Walk DEVICES skipping "/dev/input/*" entries (those are
                // listed under InputDirInode, not flatly here), counting
                // only the ones actually surfaced at this level — offset
                // numbering must stay contiguous (no gaps/blanks) or the
                // getdents64 loop in DevDirHandle stops at the first one.
                let mut idx = 0usize;
                let mut count = 3u64; // offsets 0,1,2 already consumed above
                loop {
                    match crate::drivers::device_by_index(idx) {
                        None => return Ok(None),
                        Some(path) if path.starts_with("/dev/input/") => {
                            idx += 1;
                        }
                        Some(path) => {
                            if count == n {
                                let name = path.trim_start_matches("/dev/");
                                return Ok(Some(DirEntry::new(idx as u64 + 101, FileType::CharDevice,
                                                              name.as_bytes())));
                            }
                            count += 1;
                            idx += 1;
                        }
                    }
                }
            }
        }
    }
}

// ── /dev/input subdirectory ───────────────────────────────────────────────────

struct InputDirInode;

impl Inode for InputDirInode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        Stat::dir(INPUT_DIR_INO)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        Ok(Box::new(InputDirHandle { offset: 0 }))
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        let path = alloc::format!("/dev/input/{}", name);
        if crate::drivers::has_device(&path) {
            let ino = crate::drivers::device_index(&path)
                .map(|i| i as u64 + 101)
                .unwrap_or(101);
            Ok(Arc::new(DevInode { path: String::from(path), ino }))
        } else {
            Err(Errno::ENOENT)
        }
    }

    fn readdir(&self, offset: u64) -> Result<Option<DirEntry>, Errno> {
        match offset {
            0 => Ok(Some(DirEntry::new(INPUT_DIR_INO, FileType::Directory, b"."))),
            1 => Ok(Some(DirEntry::new(100, FileType::Directory, b".."))),
            n => {
                // Same skip-non-matching-entries shape as DevDirInode's
                // readdir, scoped to the "/dev/input/" prefix instead.
                let mut idx = 0usize;
                let mut count = 2u64;
                loop {
                    match crate::drivers::device_by_index(idx) {
                        None => return Ok(None),
                        Some(path) if !path.starts_with("/dev/input/") => {
                            idx += 1;
                        }
                        Some(path) => {
                            if count == n {
                                let name = path.trim_start_matches("/dev/input/");
                                return Ok(Some(DirEntry::new(idx as u64 + 101, FileType::CharDevice,
                                                              name.as_bytes())));
                            }
                            count += 1;
                            idx += 1;
                        }
                    }
                }
            }
        }
    }
}

struct InputDirHandle {
    offset: u64,
}

impl FileHandle for InputDirHandle {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument)
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument)
    }

    fn getdents64(&mut self, buf: &mut [u8]) -> i64 {
        crate::fs::vfs::getdents64_via_readdir(&InputDirInode, &mut self.offset, buf)
    }

    fn stat(&self) -> Option<crate::fs::types::Stat> {
        Some(Stat::dir(INPUT_DIR_INO))
    }

    fn name(&self) -> &str { "devfs/input-dir" }
}

// ── Device inode ─────────────────────────────────────────────────────────────

struct DevInode {
    path: String,
    ino:  u64,
}

impl Inode for DevInode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        Stat::chardev(self.ino)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        crate::drivers::open_device(&self.path)
            .ok_or(Errno::ENOENT)
    }
}

// ── Directory handle ─────────────────────────────────────────────────────────

struct DevDirHandle {
    offset: u64,
}

impl FileHandle for DevDirHandle {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument)
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument)
    }

    fn getdents64(&mut self, buf: &mut [u8]) -> i64 {
        crate::fs::vfs::getdents64_via_readdir(&DevDirInode, &mut self.offset, buf)
    }

    fn stat(&self) -> Option<crate::fs::types::Stat> {
        Some(Stat::dir(100))
    }

    fn name(&self) -> &str { "devfs/dir" }
}
