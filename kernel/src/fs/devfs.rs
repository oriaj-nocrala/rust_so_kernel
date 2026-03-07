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
//   └── kbd
//
// Each device inode delegates `open()` to `crate::drivers::open_device`.
// Inode numbers: 100 = /dev directory, 101+ = individual devices.

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

    fn root(&self) -> Arc<dyn Inode> {
        Arc::new(DevDirInode)
    }
}

// ── Directory inode ──────────────────────────────────────────────────────────

struct DevDirInode;

impl Inode for DevDirInode {
    fn stat(&self) -> Stat {
        Stat::dir(100)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        Ok(Box::new(DevDirHandle { offset: 0 }))
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
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
            n => {
                let idx = (n - 2) as usize;
                match crate::drivers::device_by_index(idx) {
                    None => Ok(None),
                    Some(path) => {
                        // path is "/dev/console" → name is "console"
                        let name = path.trim_start_matches("/dev/");
                        Ok(Some(DirEntry::new(idx as u64 + 101, FileType::CharDevice,
                                              name.as_bytes())))
                    }
                }
            }
        }
    }
}

// ── Device inode ─────────────────────────────────────────────────────────────

struct DevInode {
    path: String,
    ino:  u64,
}

impl Inode for DevInode {
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
        let dir = DevDirInode;
        let mut written: usize = 0;

        loop {
            let entry = match dir.readdir(self.offset) {
                Ok(Some(e))  => e,
                Ok(None)     => break,
                Err(e)       => return e.as_i64(),
            };
            let needed = entry.dirent64_size();
            if written + needed > buf.len() {
                break;
            }
            let next_off = self.offset as i64 + 1;
            entry.write_dirent64(next_off, &mut buf[written..written + needed]);
            written += needed;
            self.offset += 1;
        }

        written as i64
    }

    fn stat(&self) -> Option<crate::fs::types::Stat> {
        Some(Stat::dir(100))
    }

    fn name(&self) -> &str { "devfs/dir" }
}
