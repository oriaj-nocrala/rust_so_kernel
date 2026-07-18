// kernel/src/fs/procfs.rs
//
// Minimal /proc — currently just /proc/meminfo, generated fresh on every
// open() (real Linux regenerates it on every read() of the same fd; this
// kernel's flat single-open-then-read usage never notices the difference).
// Exists so real Unix tools/scripts (busybox `free`, `cat /proc/meminfo`,
// anything doing `awk '/MemTotal/'`) work here the same way they would on
// a real system, instead of only via this kernel's own custom
// `meminfo_kb` syscall (#402, debug-only, not a real ABI).
//
// LAYOUT
// ──────
//   /proc/   (ProcDirInode)
//   └── meminfo
//
// Inode numbers: 200 = /proc directory, 201 = meminfo.

use alloc::{boxed::Box, format, string::String, sync::Arc, vec::Vec};

use crate::fs::{
    types::{DirEntry, Errno, FileType, OpenFlags, Stat},
    vfs::{Filesystem, Inode},
};
use crate::process::file::{FileError, FileHandle, FileResult};

// ── Filesystem ───────────────────────────────────────────────────────────────

pub struct ProcFs;

impl Filesystem for ProcFs {
    fn name(&self) -> &str { "procfs" }

    fn root(&self) -> Arc<dyn Inode> {
        Arc::new(ProcDirInode)
    }
}

/// Renders `/proc/meminfo` content as of right now — `MemTotal`/`MemFree`
/// only (no `MemAvailable`/`Buffers`/`Cached`: this kernel has no page
/// cache or reclaimable memory concept to report). Matches real
/// `/proc/meminfo`'s `"%-13s%8lu kB\n"` shape closely enough for tools
/// that grep/awk specific field names, which is the only thing that
/// actually matters for compatibility.
fn render_meminfo() -> String {
    let buddy = crate::allocator::buddy_allocator::BUDDY.lock();
    let total_kb = buddy.total_bytes() / 1024;
    let free_kb = buddy.free_bytes() / 1024;
    format!(
        "MemTotal:       {:>8} kB\nMemFree:        {:>8} kB\nMemAvailable:   {:>8} kB\n",
        total_kb, free_kb, free_kb
    )
}

// ── Directory inode ──────────────────────────────────────────────────────────

struct ProcDirInode;

impl Inode for ProcDirInode {
    fn stat(&self) -> Stat {
        Stat::dir(200)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        Ok(Box::new(ProcDirHandle { offset: 0 }))
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        match name {
            "meminfo" => Ok(Arc::new(MeminfoInode)),
            _ => Err(Errno::ENOENT),
        }
    }

    fn readdir(&self, offset: u64) -> Result<Option<DirEntry>, Errno> {
        match offset {
            0 => Ok(Some(DirEntry::new(200, FileType::Directory, b"."))),
            1 => Ok(Some(DirEntry::new(200, FileType::Directory, b".."))),
            2 => Ok(Some(DirEntry::new(201, FileType::Regular, b"meminfo"))),
            _ => Ok(None),
        }
    }
}

// ── meminfo file inode ───────────────────────────────────────────────────────

struct MeminfoInode;

impl Inode for MeminfoInode {
    fn stat(&self) -> Stat {
        Stat::regular(201, render_meminfo().len() as i64)
    }

    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        if flags.is_write() {
            return Err(Errno::EROFS);
        }
        Ok(Box::new(ProcFile { data: render_meminfo().into_bytes(), offset: 0 }))
    }
}

// ── Open file handle ─────────────────────────────────────────────────────────

/// Read-only handle over a snapshot generated at `open()` time.
struct ProcFile {
    data:   Vec<u8>,
    offset: usize,
}

impl FileHandle for ProcFile {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        let remaining = &self.data[self.offset..];
        if remaining.is_empty() {
            return Ok(0); // EOF
        }
        let n = buf.len().min(remaining.len());
        buf[..n].copy_from_slice(&remaining[..n]);
        self.offset += n;
        Ok(n)
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::NotSupported)
    }

    fn stat(&self) -> Option<crate::fs::types::Stat> {
        Some(Stat::regular(201, self.data.len() as i64))
    }

    fn name(&self) -> &str { "procfs/meminfo" }
}

/// Directory handle: keeps a readdir cursor and serves `getdents64`.
struct ProcDirHandle {
    offset: u64,
}

impl FileHandle for ProcDirHandle {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument) // directories use getdents64
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument)
    }

    fn getdents64(&mut self, buf: &mut [u8]) -> i64 {
        let dir = ProcDirInode;
        let mut written: usize = 0;

        loop {
            let entry = match dir.readdir(self.offset) {
                Ok(Some(e)) => e,
                Ok(None)    => break,
                Err(e)      => return e.as_i64(),
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
        Some(Stat::dir(200))
    }

    fn name(&self) -> &str { "procfs/dir" }
}
