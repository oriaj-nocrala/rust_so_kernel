// kernel/src/fs/ramfs.rs
//
// Writable in-memory filesystem, mounted at /tmp.
//
// Everything else in the VFS (initramfs, devfs) is read-only; this is the
// one place a process can create/write/read files at runtime — mainly
// intended as debug scratch space: write a batch script here, then run it
// with the shell's `sh` command (see userspace/src/bin/shell.rs) instead of
// re-typing a sequence of commands by hand every time.
//
// Flat namespace (no subdirectories), not persisted across reboots.
// Directory listings are a snapshot taken at open() time — fine for a
// scratch fs nobody expects strict live-mutation semantics from.

use alloc::{boxed::Box, collections::BTreeMap, string::String, string::ToString, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;

use crate::fs::{
    types::{DirEntry, Errno, FileType, OpenFlags, Stat},
    vfs::{Filesystem, Inode},
};
use crate::process::file::{FileHandle, FileResult};

const ROOT_INO: u64 = 1;

// ── Filesystem ───────────────────────────────────────────────────────────────

pub struct RamFs {
    root: Arc<RamDirNode>,
}

impl RamFs {
    pub fn new() -> Self {
        Self { root: Arc::new(RamDirNode::new(ROOT_INO)) }
    }
}

impl Filesystem for RamFs {
    fn name(&self) -> &str { "ramfs" }

    fn root(&self) -> Arc<dyn Inode> {
        self.root.clone()
    }
}

// ── Directory inode ──────────────────────────────────────────────────────────

struct RamDirNode {
    ino: u64,
    entries: Mutex<BTreeMap<String, Arc<RamFileNode>>>,
    next_ino: AtomicU64,
}

impl RamDirNode {
    fn new(ino: u64) -> Self {
        Self {
            ino,
            entries: Mutex::new(BTreeMap::new()),
            next_ino: AtomicU64::new(ino + 1),
        }
    }
}

impl Inode for RamDirNode {
    fn stat(&self) -> Stat {
        Stat::dir(self.ino)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        // Snapshot: this handle's getdents64 walks a fixed Vec, not the live
        // map, so files created after opendir() won't retroactively appear.
        let entries = self.entries.lock();
        let mut snapshot: Vec<DirEntry> = Vec::with_capacity(entries.len() + 2);
        snapshot.push(DirEntry::new(self.ino, FileType::Directory, b"."));
        snapshot.push(DirEntry::new(self.ino, FileType::Directory, b".."));
        for (name, node) in entries.iter() {
            snapshot.push(DirEntry::new(node.ino, FileType::Regular, name.as_bytes()));
        }
        Ok(Box::new(RamDirHandle { snapshot, offset: 0 }))
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        self.entries.lock().get(name)
            .cloned()
            .map(|node| node as Arc<dyn Inode>)
            .ok_or(Errno::ENOENT)
    }

    fn readdir(&self, offset: u64) -> Result<Option<DirEntry>, Errno> {
        match offset {
            0 => Ok(Some(DirEntry::new(self.ino, FileType::Directory, b"."))),
            1 => Ok(Some(DirEntry::new(self.ino, FileType::Directory, b".."))),
            n => {
                let idx = (n - 2) as usize;
                let entries = self.entries.lock();
                match entries.iter().nth(idx) {
                    Some((name, node)) => Ok(Some(DirEntry::new(node.ino, FileType::Regular, name.as_bytes()))),
                    None => Ok(None),
                }
            }
        }
    }

    fn create(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        let mut entries = self.entries.lock();
        if let Some(existing) = entries.get(name) {
            return Ok(existing.clone() as Arc<dyn Inode>);
        }
        let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
        let node = Arc::new(RamFileNode { ino, data: Arc::new(Mutex::new(Vec::new())) });
        entries.insert(name.to_string(), node.clone());
        Ok(node as Arc<dyn Inode>)
    }
}

/// Directory handle: serves `getdents64` off the open-time snapshot.
struct RamDirHandle {
    snapshot: Vec<DirEntry>,
    offset: usize,
}

impl FileHandle for RamDirHandle {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Err(crate::process::file::FileError::InvalidArgument) // directories use getdents64
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(crate::process::file::FileError::InvalidArgument)
    }

    fn getdents64(&mut self, buf: &mut [u8]) -> i64 {
        let mut written: usize = 0;
        while self.offset < self.snapshot.len() {
            let entry = &self.snapshot[self.offset];
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

    fn stat(&self) -> Option<Stat> {
        Some(Stat::dir(ROOT_INO))
    }

    fn name(&self) -> &str { "ramfs/dir" }
}

// ── File inode ───────────────────────────────────────────────────────────────

struct RamFileNode {
    ino:  u64,
    data: Arc<Mutex<Vec<u8>>>,
}

impl Inode for RamFileNode {
    fn stat(&self) -> Stat {
        Stat::regular(self.ino, self.data.lock().len() as i64)
    }

    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        if flags.0 & OpenFlags::TRUNC.0 != 0 {
            self.data.lock().clear();
        }
        let offset = if flags.0 & OpenFlags::APPEND.0 != 0 {
            self.data.lock().len()
        } else {
            0
        };
        Ok(Box::new(RamFileHandle { ino: self.ino, data: self.data.clone(), offset }))
    }
}

// ── Open file handle ─────────────────────────────────────────────────────────

struct RamFileHandle {
    ino:    u64,
    data:   Arc<Mutex<Vec<u8>>>,
    offset: usize,
}

impl FileHandle for RamFileHandle {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        let data = self.data.lock();
        if self.offset >= data.len() {
            return Ok(0); // EOF
        }
        let n = buf.len().min(data.len() - self.offset);
        buf[..n].copy_from_slice(&data[self.offset..self.offset + n]);
        self.offset += n;
        Ok(n)
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        let mut data = self.data.lock();
        let end = self.offset + buf.len();
        if data.len() < end {
            data.resize(end, 0);
        }
        data[self.offset..end].copy_from_slice(buf);
        self.offset = end;
        Ok(buf.len())
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::regular(self.ino, self.data.lock().len() as i64))
    }

    fn name(&self) -> &str { "ramfs" }
}
