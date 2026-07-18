// kernel/src/fs/initramfs.rs
//
// In-memory filesystem backed by ELF binaries embedded at compile time.
//
// LAYOUT
// ──────
//   /   (InitramfsDirInode)
//   ├── shell
//   ├── uname
//   └── …   (one entry per PROGRAMS registry entry)
//
// All files are read-only.  Writes return EROFS.
// Inode numbers: 1 = root directory, 2+ = files (index + 2).

use alloc::{boxed::Box, sync::Arc};
use spin::Mutex;

use crate::fs::{
    types::{DirEntry, Errno, FileType, OpenFlags, Stat},
    vfs::{Filesystem, Inode},
};
use crate::process::{
    file::{FileError, FileHandle, FileResult},
    user_programs::{ProgramSource, list_programs},
};

// ── Filesystem ───────────────────────────────────────────────────────────────

pub struct InitramfsFs;

impl Filesystem for InitramfsFs {
    fn name(&self) -> &str { "initramfs" }

    fn root(&self) -> Arc<dyn Inode> {
        Arc::new(InitramfsDirInode)
    }
}

/// Return raw ELF bytes for `name`, or `None` if not found.
///
/// Used by `sys_exec` to feed the ELF loader without opening an FD.
pub fn bytes(name: &str) -> Option<&'static [u8]> {
    for (prog_name, source) in list_programs() {
        if *prog_name == name {
            if let ProgramSource::Elf(b) = source {
                return Some(b);
            }
        }
    }
    None
}

/// Open a file by name as a `FileHandle` (for `sys_open("/bin/<name>")`).
pub fn open(name: &str) -> Option<Box<dyn FileHandle>> {
    let data = bytes(name)?;
    Some(Box::new(RamFile::new(data)))
}

// ── Directory inode ──────────────────────────────────────────────────────────

struct InitramfsDirInode;

impl Inode for InitramfsDirInode {
    fn stat(&self) -> Stat {
        Stat::dir(1)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        Ok(Box::new(InitramfsDirHandle { offset: 0 }))
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        for (i, (prog_name, source)) in list_programs().iter().enumerate() {
            if *prog_name == name {
                if let ProgramSource::Elf(data) = source {
                    let ino = (i as u64) + 2;
                    return Ok(Arc::new(InitramfsFileInode { ino, data }));
                }
            }
        }
        Err(Errno::ENOENT)
    }

    fn readdir(&self, offset: u64) -> Result<Option<DirEntry>, Errno> {
        match offset {
            0 => Ok(Some(DirEntry::new(1, FileType::Directory, b"."))),
            1 => Ok(Some(DirEntry::new(1, FileType::Directory, b".."))),
            n => {
                let idx = (n - 2) as usize;
                let programs = list_programs();
                if idx >= programs.len() {
                    return Ok(None);
                }
                let (name, _) = &programs[idx];
                let ino = idx as u64 + 2;
                Ok(Some(DirEntry::new(ino, FileType::Regular, name.as_bytes())))
            }
        }
    }
}

// ── File inode ───────────────────────────────────────────────────────────────

struct InitramfsFileInode {
    ino:  u64,
    data: &'static [u8],
}

impl Inode for InitramfsFileInode {
    fn stat(&self) -> Stat {
        Stat::regular(self.ino, self.data.len() as i64)
    }

    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        if flags.is_write() {
            return Err(Errno::EROFS);
        }
        Ok(Box::new(RamFile::new(self.data)))
    }
}

// ── Open file handles ────────────────────────────────────────────────────────

/// Seekable read-only file handle over a static byte slice.
struct RamFile {
    data:   &'static [u8],
    // Arc'd so dup()/dup2() can share one true "open file description"
    // position between two fds (POSIX dup() semantics) — see ramfs.rs's
    // RamFileHandle, which has the exact same reasoning.
    offset: Arc<Mutex<usize>>,
}

impl RamFile {
    fn new(data: &'static [u8]) -> Self {
        Self { data, offset: Arc::new(Mutex::new(0)) }
    }
}

impl FileHandle for RamFile {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        let mut offset = self.offset.lock();
        let remaining = &self.data[*offset..];
        if remaining.is_empty() {
            return Ok(0); // EOF
        }
        let n = buf.len().min(remaining.len());
        buf[..n].copy_from_slice(&remaining[..n]);
        *offset += n;
        Ok(n)
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::NotSupported)
    }

    fn stat(&self) -> Option<crate::fs::types::Stat> {
        Some(Stat::regular(0, self.data.len() as i64))
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(RamFile { data: self.data, offset: self.offset.clone() }))
    }

    fn name(&self) -> &str { "initramfs" }
}

/// Directory handle: keeps a readdir cursor and serves `getdents64`.
struct InitramfsDirHandle {
    offset: u64,
}

impl FileHandle for InitramfsDirHandle {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument) // directories use getdents64
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument)
    }

    fn getdents64(&mut self, buf: &mut [u8]) -> i64 {
        let dir = InitramfsDirInode;
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
        Some(Stat::dir(1))
    }

    fn name(&self) -> &str { "initramfs/dir" }
}
