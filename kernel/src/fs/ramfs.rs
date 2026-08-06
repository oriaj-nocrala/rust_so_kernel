// kernel/src/fs/ramfs.rs
//
// Writable in-memory filesystem, mounted at /tmp.
//
// Everything else in the VFS (initramfs, devfs) is read-only; this is the
// one place a process can create/write/read files (and, now, directories)
// at runtime — mainly intended as debug scratch space: write a batch
// script here, then run it with the shell's `sh` command (see
// userspace/src/bin/shell.rs) instead of re-typing a sequence of commands
// by hand every time.
//
// Real (recursive) subdirectories, not persisted across reboots. Each
// directory's `entries` map holds `Arc<dyn Inode>` — files and
// subdirectories side by side, told apart via `Inode::file_type()` — so a
// directory can contain other directories without a parallel enum.
// Directory listings are a snapshot taken at open() time — fine for a
// scratch fs nobody expects strict live-mutation semantics from.

use alloc::{boxed::Box, collections::BTreeMap, string::String, string::ToString, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use spin::Mutex;

use crate::fs::{
    types::{DirEntry, Errno, FileType, OpenFlags, Stat},
    vfs::{Filesystem, Inode},
};
use crate::process::file::{FileHandle, FileResult};

const ROOT_INO: u64 = 1;

/// Single counter shared by every `RamDirNode`/`RamFileNode`, however
/// deeply nested — per-directory counters (the old flat-namespace design)
/// would hand out colliding inode numbers as soon as two different
/// subdirectories both allocated children.
static NEXT_INO: AtomicU64 = AtomicU64::new(ROOT_INO + 1);

fn alloc_ino() -> u64 {
    NEXT_INO.fetch_add(1, Ordering::Relaxed)
}

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

    fn root(&self) -> Result<Arc<dyn Inode>, Errno> {
        Ok(self.root.clone())
    }
}

// ── Directory inode ──────────────────────────────────────────────────────────

struct RamDirNode {
    ino: u64,
    entries: Mutex<BTreeMap<String, Arc<dyn Inode>>>,
    // Real per-inode permission storage (not a hardcoded per-filesystem
    // constant like the read-only filesystems use) — ramfs is one of only
    // two genuinely writable filesystems here (the other being ext2), so
    // `chmod` should persist for real on both. Plain `AtomicU32` (not
    // `Arc`-wrapped, unlike `RamFileNode::mode`): only path-based `chmod`
    // ever reaches a directory in this kernel (no `fchmod` on a directory
    // fd — matches `ext2::Ext2DirHandle`'s identical scope), and
    // path-based chmod always operates on the same `Arc<RamDirNode>`
    // instance already shared through the parent's `entries` map, so no
    // extra sharing mechanism is needed.
    mode: AtomicU32,
}

impl RamDirNode {
    fn new(ino: u64) -> Self {
        Self {
            ino,
            entries: Mutex::new(BTreeMap::new()),
            mode: AtomicU32::new(0o755),
        }
    }

    /// Locks `entries`, reporting the acquisition through
    /// `debug::RAMFS_ENTRIES_LOCK` (see that type's doc comment — built
    /// after this exact lock was found stuck taken forever during the
    /// 2026-08-05 hang hunt, RIP parked in `SpinMutex::lock`'s `_mm_pause`
    /// inlined into `mkdir` below; the root cause was DF-corrupted trapframe
    /// copies abandoning the syscall that held it, fixed in `tss.rs`'s FMASK
    /// and the two entry stubs' `cld`). `op` names the calling method
    /// (`"mkdir"`, `"symlink"`, ...) so `/proc/kdebug` can say *who* holds
    /// it, which `LockDiag`'s file:line can't (every op locks from the same
    /// inlined site). Returns a guard that reports the matching release on
    /// `Drop`, so call sites read exactly like the plain guard they replaced
    /// — `self.entries.lock()` → `self.lock_entries(op)`.
    ///
    /// The acquire is recorded *after* the lock is actually taken, never
    /// before: recording first would name a spinning-but-not-yet-holding
    /// caller as the holder (one of the defective instruments this hunt
    /// produced — see docs/hang-hunt-bug2-findings.md). The pid, on the
    /// other hand, is read *before*: `current_pid_safe` takes the
    /// `SCHEDULER` lock and ends with an unconditional `sti`, neither of
    /// which may happen inside this critical section.
    fn lock_entries(&self, op: &'static str) -> TrackedEntriesGuard<'_> {
        let pid = crate::process::scheduler::current_pid_safe().map(|p| p as u64).unwrap_or(u64::MAX);
        let guard = self.entries.lock();
        crate::debug::RAMFS_ENTRIES_LOCK.record_acquire(pid, op);
        TrackedEntriesGuard { guard }
    }
}

/// RAII wrapper around `RamDirNode::entries`'s real `MutexGuard` that
/// records the matching release for `debug::RAMFS_ENTRIES_LOCK` on `Drop`
/// — see `RamDirNode::lock_entries`. `Deref`/`DerefMut` straight through to
/// the `BTreeMap` so callers use it exactly like the plain guard they used
/// to hold.
struct TrackedEntriesGuard<'a> {
    guard: spin::MutexGuard<'a, BTreeMap<String, Arc<dyn Inode>>>,
}

impl<'a> core::ops::Deref for TrackedEntriesGuard<'a> {
    type Target = BTreeMap<String, Arc<dyn Inode>>;
    fn deref(&self) -> &Self::Target { &self.guard }
}

impl<'a> core::ops::DerefMut for TrackedEntriesGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target { &mut self.guard }
}

impl<'a> Drop for TrackedEntriesGuard<'a> {
    fn drop(&mut self) {
        crate::debug::RAMFS_ENTRIES_LOCK.record_release();
    }
}

impl Inode for RamDirNode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        // Real link count (2 + subdirectory count), same "report the
        // actual state, not a fixed default" fix applied to permission
        // bits below and to ext2's own `stat()`.
        let nlink = 2 + self.lock_entries("stat").values()
            .filter(|v| v.file_type() == FileType::Directory)
            .count() as u64;
        Stat::dir(self.ino).with_perm_bits(self.mode.load(Ordering::Relaxed)).with_nlink(nlink)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        // Snapshot: this handle's getdents64 walks a fixed Vec, not the live
        // map, so files created after opendir() won't retroactively appear.
        let entries = self.lock_entries("open");
        let mut snapshot: Vec<DirEntry> = Vec::with_capacity(entries.len() + 2);
        snapshot.push(DirEntry::new(self.ino, FileType::Directory, b"."));
        snapshot.push(DirEntry::new(self.ino, FileType::Directory, b".."));
        for (name, node) in entries.iter() {
            snapshot.push(DirEntry::new(node.stat().st_ino, node.file_type(), name.as_bytes()));
        }
        Ok(Box::new(RamDirHandle { snapshot, offset: 0 }))
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        self.lock_entries("lookup").get(name).cloned().ok_or(Errno::ENOENT)
    }

    fn readdir(&self, offset: u64) -> Result<Option<DirEntry>, Errno> {
        match offset {
            0 => Ok(Some(DirEntry::new(self.ino, FileType::Directory, b"."))),
            1 => Ok(Some(DirEntry::new(self.ino, FileType::Directory, b".."))),
            n => {
                let idx = (n - 2) as usize;
                let entries = self.lock_entries("readdir");
                match entries.iter().nth(idx) {
                    Some((name, node)) => Ok(Some(DirEntry::new(node.stat().st_ino, node.file_type(), name.as_bytes()))),
                    None => Ok(None),
                }
            }
        }
    }

    fn create(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        let mut entries = self.lock_entries("create");
        if let Some(existing) = entries.get(name) {
            if existing.file_type() == FileType::Directory {
                return Err(Errno::EISDIR);
            }
            return Ok(existing.clone());
        }
        let node = Arc::new(RamFileNode {
            ino: alloc_ino(),
            data: Arc::new(Mutex::new(Vec::new())),
            mode: Arc::new(AtomicU32::new(0o644)),
        });
        entries.insert(name.to_string(), node.clone() as Arc<dyn Inode>);
        Ok(node as Arc<dyn Inode>)
    }

    fn mkdir(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        let mut entries = self.lock_entries("mkdir");
        if entries.contains_key(name) {
            return Err(Errno::EEXIST);
        }
        let node = Arc::new(RamDirNode::new(alloc_ino()));
        entries.insert(name.to_string(), node.clone() as Arc<dyn Inode>);
        Ok(node as Arc<dyn Inode>)
    }

    fn unlink(&self, name: &str) -> Result<(), Errno> {
        let mut entries = self.lock_entries("unlink");
        match entries.get(name) {
            None => Err(Errno::ENOENT),
            Some(node) if node.file_type() == FileType::Directory => Err(Errno::EISDIR),
            Some(_) => { entries.remove(name); Ok(()) }
        }
    }

    fn rmdir(&self, name: &str) -> Result<(), Errno> {
        let mut entries = self.lock_entries("rmdir");
        let node = match entries.get(name) {
            None => return Err(Errno::ENOENT),
            Some(node) => node,
        };
        if node.file_type() != FileType::Directory {
            return Err(Errno::ENOTDIR);
        }
        // offset 2 is the first entry past "." and ".." — Ok(None) there
        // means the directory holds nothing else.
        if node.readdir(2)?.is_some() {
            return Err(Errno::ENOTEMPTY);
        }
        entries.remove(name);
        Ok(())
    }

    fn take_child(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        self.lock_entries("take_child").remove(name).ok_or(Errno::ENOENT)
    }

    fn insert_child(&self, name: &str, node: Arc<dyn Inode>) -> Result<(), Errno> {
        let mut entries = self.lock_entries("insert_child");
        if entries.contains_key(name) {
            return Err(Errno::EEXIST);
        }
        entries.insert(name.to_string(), node);
        Ok(())
    }

    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn Inode>, Errno> {
        let mut entries = self.lock_entries("symlink");
        if entries.contains_key(name) {
            return Err(Errno::EEXIST);
        }
        let node = Arc::new(RamSymlinkNode { ino: alloc_ino(), target: target.to_string() });
        entries.insert(name.to_string(), node.clone() as Arc<dyn Inode>);
        Ok(node as Arc<dyn Inode>)
    }

    fn chmod(&self, mode: u32) -> Result<(), Errno> {
        self.mode.store(mode & 0o7777, Ordering::Relaxed);
        Ok(())
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
        crate::fs::vfs::getdents64_from_snapshot(&self.snapshot, &mut self.offset, buf)
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
    // `Arc`-wrapped (unlike `RamDirNode::mode`): a `RamFileHandle` opened
    // from this node needs to share the same storage so `fchmod` (which
    // only ever sees the handle, never this `Inode`) and path-based
    // `chmod`/`stat()` (which only ever see this `Inode`, never a live
    // handle) agree on one true value — same reasoning as `data` itself
    // being `Arc`-shared below.
    mode: Arc<AtomicU32>,
}

impl Inode for RamFileNode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        Stat::regular_writable(self.ino, self.data.lock().len() as i64)
            .with_perm_bits(self.mode.load(Ordering::Relaxed))
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
        Ok(Box::new(RamFileHandle {
            ino: self.ino,
            data: self.data.clone(),
            offset: Arc::new(Mutex::new(offset)),
            mode: self.mode.clone(),
        }))
    }

    fn chmod(&self, mode: u32) -> Result<(), Errno> {
        self.mode.store(mode & 0o7777, Ordering::Relaxed);
        Ok(())
    }
}

// ── Symlink inode ────────────────────────────────────────────────────────────

/// A real, persisted (for this boot — ramfs isn't disk-backed) symlink,
/// created via `symlink(2)` (`RamDirNode::symlink`). Unlike the synthetic
/// symlink inodes elsewhere in this VFS (`fs::procfs::SelfInode`,
/// `fs::initramfs::BusyboxAppletInode`) which compute their target on the
/// fly, this one just stores whatever string `symlink()` was called with —
/// same as a real filesystem's symlink.
struct RamSymlinkNode {
    ino:    u64,
    target: String,
}

impl Inode for RamSymlinkNode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        Stat::symlink(self.ino, self.target.len() as i64)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        // Real Unix: open() without O_NOFOLLOW follows the symlink, so this
        // is unreachable through normal traversal — same reasoning as
        // procfs::SelfInode::open().
        Err(Errno::EINVAL)
    }

    fn readlink(&self) -> Result<String, Errno> {
        Ok(self.target.clone())
    }
}

// ── Open file handle ─────────────────────────────────────────────────────────

struct RamFileHandle {
    ino:    u64,
    data:   Arc<Mutex<Vec<u8>>>,
    // Arc'd (not a plain usize) so dup()/dup2() can share one true "open
    // file description" position between two fds, matching POSIX dup()
    // semantics — reading through either fd advances both.
    offset: Arc<Mutex<usize>>,
    // Shared with the owning `RamFileNode` (and every other open handle on
    // it) — see that struct's doc comment on this same field.
    mode: Arc<AtomicU32>,
}

impl FileHandle for RamFileHandle {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        let data = self.data.lock();
        let mut offset = self.offset.lock();
        if *offset >= data.len() {
            return Ok(0); // EOF
        }
        let n = buf.len().min(data.len() - *offset);
        buf[..n].copy_from_slice(&data[*offset..*offset + n]);
        *offset += n;
        Ok(n)
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        let mut data = self.data.lock();
        let mut offset = self.offset.lock();
        let end = *offset + buf.len();
        if data.len() < end {
            data.resize(end, 0);
        }
        data[*offset..end].copy_from_slice(buf);
        *offset = end;
        Ok(buf.len())
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::regular_writable(self.ino, self.data.lock().len() as i64)
            .with_perm_bits(self.mode.load(Ordering::Relaxed)))
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(RamFileHandle {
            ino: self.ino,
            data: self.data.clone(),
            offset: self.offset.clone(),
            mode: self.mode.clone(),
        }))
    }

    fn seek(&mut self, offset: i64, whence: i32) -> FileResult<i64> {
        let mut cur = self.offset.lock();
        let size = self.data.lock().len() as i64;
        let new_pos = crate::process::file::compute_seek(*cur as i64, size, offset, whence)?;
        *cur = new_pos as usize;
        Ok(new_pos)
    }

    fn chmod(&mut self, mode: u32) -> FileResult<()> {
        self.mode.store(mode & 0o7777, Ordering::Relaxed);
        Ok(())
    }

    fn name(&self) -> &str { "ramfs" }
}
