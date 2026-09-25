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
//   ├── input/   (InputDirInode — one hardcoded level of nesting)
//   │   └── event0
//   └── pts/     (PtsDirInode — the live pseudo-terminal slaves, ipc/pty.rs)
//       └── 0
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

/// Same, for `/dev/pts`.
const PTS_DIR_INO: u64 = 100_001;

/// `O_NOCTTY` (this port's `fcntl.h`).
const O_NOCTTY: i32 = 0o400;

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
        if name == "pts" {
            return Ok(Arc::new(PtsDirInode));
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
            3 => Ok(Some(DirEntry::new(PTS_DIR_INO, FileType::Directory, b"pts"))),
            n => {
                // Walk DEVICES skipping "/dev/input/*" entries (those are
                // listed under InputDirInode, not flatly here), counting
                // only the ones actually surfaced at this level — offset
                // numbering must stay contiguous (no gaps/blanks) or the
                // getdents64 loop in DevDirHandle stops at the first one.
                let mut idx = 0usize;
                let mut count = 4u64; // offsets 0..=3 already consumed above
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

// ── /dev/pts subdirectory ─────────────────────────────────────────────────────

/// The slaves of the pseudo-terminals whose master is open, named by
/// number — what Linux's devpts shows. Not `DEVICES` entries: they come and
/// go with `/dev/ptmx` opens.
struct PtsDirInode;

impl Inode for PtsDirInode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        Stat::dir(PTS_DIR_INO)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        Ok(Box::new(PtsDirHandle { offset: 0 }))
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        let index: usize = name.parse().map_err(|_| Errno::ENOENT)?;
        if !crate::ipc::pty::exists(index) || alloc::format!("{}", index) != name {
            return Err(Errno::ENOENT);
        }
        Ok(Arc::new(PtsInode { index }))
    }

    fn readdir(&self, offset: u64) -> Result<Option<DirEntry>, Errno> {
        match offset {
            0 => Ok(Some(DirEntry::new(PTS_DIR_INO, FileType::Directory, b"."))),
            1 => Ok(Some(DirEntry::new(100, FileType::Directory, b".."))),
            n => {
                let live = crate::ipc::pty::live_indices();
                Ok(live.get((n - 2) as usize).map(|&i| {
                    let name = alloc::format!("{}", i);
                    DirEntry::new(crate::ipc::pty::PTS_INO_BASE + i as u64, FileType::CharDevice, name.as_bytes())
                }))
            }
        }
    }
}

struct PtsDirHandle {
    offset: u64,
}

impl FileHandle for PtsDirHandle {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument)
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument)
    }

    fn getdents64(&mut self, buf: &mut [u8]) -> i64 {
        crate::fs::vfs::getdents64_via_readdir(&PtsDirInode, &mut self.offset, buf)
    }

    fn stat(&self) -> Option<crate::fs::types::Stat> {
        Some(Stat::dir(PTS_DIR_INO))
    }

    fn name(&self) -> &str { "devfs/pts-dir" }
}

struct PtsInode {
    index: usize,
}

impl Inode for PtsInode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        Stat::chardev(crate::ipc::pty::PTS_INO_BASE + self.index as u64)
    }

    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        crate::ipc::pty::open_slave(self.index, flags.0 & O_NOCTTY != 0)
    }
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
