// vfs/src/ramfs.rs
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
use crate::lock::Mutex;

use crate::types::{DirEntry, Errno, FileType, OpenFlags, Stat};
use crate::inode::{Filesystem, Inode};
use crate::dirent::getdents64_from_snapshot;
use crate::file::{FileHandle, FileError, FileResult, compute_seek};

const ROOT_INO: u64 = 1;

/// Single counter shared by every `RamDirNode`/`RamFileNode`, however
/// deeply nested — per-directory counters (the old flat-namespace design)
/// would hand out colliding inode numbers as soon as two different
/// subdirectories both allocated children.
static NEXT_INO: AtomicU64 = AtomicU64::new(ROOT_INO + 1);

fn alloc_ino() -> u64 {
    NEXT_INO.fetch_add(1, Ordering::Relaxed)
}

// ── Directory-lock observer seam ────────────────────────────────────────────

/// Observer of `RamDirNode`'s directory-entries lock. Same seam shape as
/// `hal::PortIo` / `mm::PhysMap`: this crate can't call the kernel's
/// scheduler or its `kernel::debug` tracing module, so the kernel injects
/// an implementation that does, and host tests use the no-op below.
///
/// The call order is a real invariant, paid for with a real bug (see
/// `RamDirNode::lock_entries`'s doc comment) — implementations must not
/// reorder what they do relative to when the caller invokes each method,
/// only what each method itself does.
pub trait DirLockObserver: Send + Sync {
    /// Pid of the process about to attempt the lock. Always invoked
    /// *before* the lock is taken — see `RamDirNode::lock_entries`.
    fn current_pid(&self) -> u64;
    /// The lock has just been acquired for real. Always invoked *after*,
    /// never before — see `RamDirNode::lock_entries`.
    fn record_acquire(&self, pid: u64, op: &'static str);
    /// The lock has just been released (called from the guard's `Drop`).
    fn record_release(&self);
}

/// Default observer for host tests (and anywhere else that doesn't need
/// the kernel's `/proc/kdebug` diagnostic): does nothing.
pub struct NoopDirLockObserver;

impl DirLockObserver for NoopDirLockObserver {
    fn current_pid(&self) -> u64 { u64::MAX }
    fn record_acquire(&self, _pid: u64, _op: &'static str) {}
    fn record_release(&self) {}
}

pub static NOOP_DIR_LOCK_OBSERVER: NoopDirLockObserver = NoopDirLockObserver;

// ── Filesystem ───────────────────────────────────────────────────────────────

pub struct RamFs {
    root: Arc<RamDirNode>,
}

impl RamFs {
    /// Build a `RamFs` with the no-op observer — no `/proc/kdebug`
    /// `ramfs_entries_lock` diagnostic, but otherwise fully functional.
    /// Used by host tests; the real kernel mount uses `with_observer`
    /// instead (`kernel/src/fs/ramfs.rs::new`) so the diagnostic stays
    /// wired up.
    pub fn new() -> Self {
        Self { root: Arc::new(RamDirNode::new(ROOT_INO, &NOOP_DIR_LOCK_OBSERVER)) }
    }

    /// Build a `RamFs` whose every directory reports lock acquire/release
    /// through `observer` — see `DirLockObserver`.
    pub fn with_observer(observer: &'static dyn DirLockObserver) -> Self {
        Self { root: Arc::new(RamDirNode::new(ROOT_INO, observer)) }
    }
}

impl Default for RamFs {
    fn default() -> Self {
        Self::new()
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
    // See `DirLockObserver`. A `&'static dyn` fat pointer (`Copy`), not an
    // `Arc`, so wiring it through costs nothing per node.
    observer: &'static dyn DirLockObserver,
}

impl RamDirNode {
    fn new(ino: u64, observer: &'static dyn DirLockObserver) -> Self {
        Self {
            ino,
            entries: Mutex::new(BTreeMap::new()),
            mode: AtomicU32::new(0o755),
            observer,
        }
    }

    /// Locks `entries`, reporting the acquisition through the node's
    /// `DirLockObserver` (see that trait's doc comment — built after this
    /// exact lock was found stuck taken forever during the 2026-08-05 hang
    /// hunt, RIP parked in `SpinMutex::lock`'s `_mm_pause` inlined into
    /// `mkdir` below; the root cause was DF-corrupted trapframe copies
    /// abandoning the syscall that held it, fixed in `tss.rs`'s FMASK and
    /// the two entry stubs' `cld`). `op` names the calling method
    /// (`"mkdir"`, `"symlink"`, ...) so the kernel's `/proc/kdebug` can say
    /// *who* holds it, which a plain file:line tracker can't (every op
    /// locks from the same inlined site). Returns a guard that reports the
    /// matching release on `Drop`, so call sites read exactly like the
    /// plain guard they replaced — `self.entries.lock()` → `self.lock_entries(op)`.
    ///
    /// The acquire is recorded *after* the lock is actually taken, never
    /// before: recording first would name a spinning-but-not-yet-holding
    /// caller as the holder (one of the defective instruments this hunt
    /// produced — see docs/hang-hunt-bug2-findings.md). The pid, on the
    /// other hand, is read *before*: the kernel's real observer
    /// (`current_pid_safe`) takes the `SCHEDULER` lock and ends with an
    /// unconditional `sti`, neither of which may happen inside this
    /// critical section.
    fn lock_entries(&self, op: &'static str) -> TrackedEntriesGuard<'_> {
        let pid = self.observer.current_pid();
        let guard = self.entries.lock();
        self.observer.record_acquire(pid, op);
        TrackedEntriesGuard { guard, observer: self.observer }
    }
}

/// RAII wrapper around `RamDirNode::entries`'s real `MutexGuard` that
/// records the matching release for the node's `DirLockObserver` on
/// `Drop` — see `RamDirNode::lock_entries`. `Deref`/`DerefMut` straight
/// through to the `BTreeMap` so callers use it exactly like the plain
/// guard they used to hold.
struct TrackedEntriesGuard<'a> {
    guard: crate::lock::MutexGuard<'a, BTreeMap<String, Arc<dyn Inode>>>,
    observer: &'static dyn DirLockObserver,
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
        self.observer.record_release();
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
        let node = Arc::new(RamDirNode::new(alloc_ino(), self.observer));
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

    fn mksocket(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        let mut entries = self.lock_entries("mksocket");
        if entries.contains_key(name) {
            return Err(Errno::EEXIST);
        }
        let node = Arc::new(RamSocketNode { ino: alloc_ino() });
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
        Err(FileError::InvalidArgument) // directories use getdents64
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument)
    }

    fn getdents64(&mut self, buf: &mut [u8]) -> i64 {
        getdents64_from_snapshot(&self.snapshot, &mut self.offset, buf)
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

/// A bound AF_UNIX socket's name in the filesystem.
///
/// Holds nothing: the socket it names lives in the kernel's socket table,
/// reachable only by `connect()`/`sendto()` with this path. The node exists
/// so `ls -l` shows the socket, `stat()` reports `S_IFSOCK`, and `unlink()`
/// can remove the name — exactly the role the same node plays in Linux.
struct RamSocketNode {
    ino: u64,
}

impl Inode for RamSocketNode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        Stat::socket(self.ino)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        // Linux answers ENXIO for open() on a socket node: there is no file
        // behind the name, and a socket is not something open() can produce.
        Err(Errno::ENXIO)
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
        // `data` before `offset`, like `read`/`write`: the handle's two
        // locks are shared by every dup of it, and taking them in opposite
        // orders is an ABBA deadlock the moment two CPUs run a read and a
        // seek on one open file (stage 6 of `docs/smp/smp-plan.md`).
        let size = self.data.lock().len() as i64;
        let mut cur = self.offset.lock();
        let new_pos = compute_seek(*cur as i64, size, offset, whence)?;
        *cur = new_pos as usize;
        Ok(new_pos)
    }

    fn chmod(&mut self, mode: u32) -> FileResult<()> {
        self.mode.store(mode & 0o7777, Ordering::Relaxed);
        Ok(())
    }

    fn name(&self) -> &str { "ramfs" }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mount::MountTable;
    use alloc::vec;

    // `NEXT_INO` is a single counter shared by the whole test binary (every
    // test in this module constructs `RamFs` instances that all draw from
    // the same static), so tests below only ever assert inode numbers are
    // *distinct* from each other / from `ROOT_INO` — never a concrete value.

    fn new_fs() -> RamFs {
        RamFs::new()
    }

    fn root(fs: &RamFs) -> Arc<dyn Inode> {
        fs.root().expect("root always succeeds for ramfs")
    }

    // ── Files: create/open/read/write ───────────────────────────────────

    #[test]
    fn create_open_write_read_roundtrip() {
        let fs = new_fs();
        let r = root(&fs);
        let file = r.create("f.txt").expect("create");
        assert_ne!(file.stat().st_ino, ROOT_INO);

        let mut h = file.open(OpenFlags::RDWR).expect("open");
        assert_eq!(h.write(b"hello").unwrap(), 5);
        assert_eq!(h.seek(0, 0).unwrap(), 0); // SEEK_SET back to start
        let mut buf = [0u8; 5];
        assert_eq!(h.read(&mut buf).unwrap(), 5);
        assert_eq!(&buf, b"hello");
    }

    #[test]
    fn read_advances_offset() {
        let fs = new_fs();
        let r = root(&fs);
        let file = r.create("f").unwrap();
        let mut h = file.open(OpenFlags::RDWR).unwrap();
        h.write(b"abcdef").unwrap();
        h.seek(0, 0).unwrap();

        let mut buf = [0u8; 3];
        assert_eq!(h.read(&mut buf).unwrap(), 3);
        assert_eq!(&buf, b"abc");
        assert_eq!(h.read(&mut buf).unwrap(), 3);
        assert_eq!(&buf, b"def");
    }

    #[test]
    fn read_at_eof_returns_zero() {
        let fs = new_fs();
        let r = root(&fs);
        let file = r.create("f").unwrap();
        let mut h = file.open(OpenFlags::RDWR).unwrap();
        h.write(b"abc").unwrap();
        h.seek(0, 2).unwrap(); // SEEK_END, at EOF already
        let mut buf = [0u8; 4];
        assert_eq!(h.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn write_in_the_middle_overwrites() {
        let fs = new_fs();
        let r = root(&fs);
        let file = r.create("f").unwrap();
        let mut h = file.open(OpenFlags::RDWR).unwrap();
        h.write(b"aaaaaa").unwrap();
        h.seek(2, 0).unwrap();
        h.write(b"XX").unwrap();
        h.seek(0, 0).unwrap();
        let mut buf = [0u8; 6];
        h.read(&mut buf).unwrap();
        assert_eq!(&buf, b"aaXXaa");
    }

    #[test]
    fn write_past_end_extends_with_resize() {
        let fs = new_fs();
        let r = root(&fs);
        let file = r.create("f").unwrap();
        let mut h = file.open(OpenFlags::RDWR).unwrap();
        h.write(b"ab").unwrap();
        h.seek(5, 0).unwrap();
        h.write(b"Z").unwrap();
        assert_eq!(file.stat().st_size, 6);
        h.seek(0, 0).unwrap();
        let mut buf = [0u8; 6];
        h.read(&mut buf).unwrap();
        assert_eq!(&buf, b"ab\0\0\0Z");
    }

    #[test]
    fn o_trunc_empties_on_open() {
        let fs = new_fs();
        let r = root(&fs);
        let file = r.create("f").unwrap();
        let mut h = file.open(OpenFlags::RDWR).unwrap();
        h.write(b"stale data").unwrap();
        drop(h);

        let h2 = file.open(OpenFlags(OpenFlags::RDWR.0 | OpenFlags::TRUNC.0)).unwrap();
        assert_eq!(h2.stat().unwrap().st_size, 0);
    }

    #[test]
    fn o_append_positions_at_end() {
        let fs = new_fs();
        let r = root(&fs);
        let file = r.create("f").unwrap();
        let mut h = file.open(OpenFlags::RDWR).unwrap();
        h.write(b"12345").unwrap();
        drop(h);

        let mut h2 = file.open(OpenFlags(OpenFlags::RDWR.0 | OpenFlags::APPEND.0)).unwrap();
        h2.write(b"67").unwrap();
        // Re-open plain RDWR and read back the full content.
        let mut h3 = file.open(OpenFlags::RDWR).unwrap();
        let mut buf = [0u8; 7];
        h3.read(&mut buf).unwrap();
        assert_eq!(&buf, b"1234567");
    }

    #[test]
    fn stat_size_reflects_content() {
        let fs = new_fs();
        let r = root(&fs);
        let file = r.create("f").unwrap();
        let mut h = file.open(OpenFlags::RDWR).unwrap();
        h.write(b"0123456789").unwrap();
        assert_eq!(file.stat().st_size, 10);
        assert_eq!(h.stat().unwrap().st_size, 10);
    }

    #[test]
    fn seek_set_cur_end() {
        let fs = new_fs();
        let r = root(&fs);
        let file = r.create("f").unwrap();
        let mut h = file.open(OpenFlags::RDWR).unwrap();
        h.write(b"0123456789").unwrap();

        assert_eq!(h.seek(3, 0).unwrap(), 3); // SEEK_SET
        assert_eq!(h.seek(2, 1).unwrap(), 5); // SEEK_CUR
        assert_eq!(h.seek(-1, 2).unwrap(), 9); // SEEK_END
    }

    #[test]
    fn dup_shares_the_offset() {
        // POSIX dup() semantics: two fds on the same open file description
        // share one true seek position. This is exactly what
        // `RamFileHandle::offset` being `Arc<Mutex<usize>>` (not a plain
        // usize) exists to provide.
        let fs = new_fs();
        let r = root(&fs);
        let file = r.create("f").unwrap();
        let mut h1 = file.open(OpenFlags::RDWR).unwrap();
        h1.write(b"abcdef").unwrap();
        h1.seek(0, 0).unwrap();

        let mut h2 = h1.dup().expect("ramfs file handles are dup-able");

        let mut buf = [0u8; 3];
        assert_eq!(h1.read(&mut buf).unwrap(), 3);
        assert_eq!(&buf, b"abc");
        // Reading through h2 continues from where h1 left off.
        assert_eq!(h2.read(&mut buf).unwrap(), 3);
        assert_eq!(&buf, b"def");
    }

    // ── Directories ──────────────────────────────────────────────────────

    #[test]
    fn mkdir_nested_directory_inside_directory() {
        let fs = new_fs();
        let r = root(&fs);
        let a = r.mkdir("a").expect("mkdir a");
        let b = a.mkdir("b").expect("mkdir a/b");
        assert_eq!(b.file_type(), FileType::Directory);
        assert_ne!(a.stat().st_ino, b.stat().st_ino);
        assert_ne!(a.stat().st_ino, ROOT_INO);
    }

    #[test]
    fn create_on_existing_name_returns_the_existing_node() {
        let fs = new_fs();
        let r = root(&fs);
        let f1 = r.create("f").unwrap();
        let f2 = r.create("f").unwrap();
        assert_eq!(f1.stat().st_ino, f2.stat().st_ino);
    }

    #[test]
    fn create_on_a_directory_name_is_eisdir() {
        let fs = new_fs();
        let r = root(&fs);
        r.mkdir("d").unwrap();
        assert_eq!(r.create("d").err(), Some(Errno::EISDIR));
    }

    #[test]
    fn mkdir_on_occupied_name_is_eexist() {
        let fs = new_fs();
        let r = root(&fs);
        r.mkdir("d").unwrap();
        assert_eq!(r.mkdir("d").err(), Some(Errno::EEXIST));
        r.create("f").unwrap();
        assert_eq!(r.mkdir("f").err(), Some(Errno::EEXIST));
    }

    #[test]
    fn lookup_missing_is_enoent() {
        let fs = new_fs();
        let r = root(&fs);
        assert_eq!(r.lookup("nope").err(), Some(Errno::ENOENT));
    }

    #[test]
    fn unlink_a_directory_is_eisdir() {
        let fs = new_fs();
        let r = root(&fs);
        r.mkdir("d").unwrap();
        assert_eq!(r.unlink("d").err(), Some(Errno::EISDIR));
    }

    #[test]
    fn rmdir_a_non_directory_is_enotdir() {
        let fs = new_fs();
        let r = root(&fs);
        r.create("f").unwrap();
        assert_eq!(r.rmdir("f").err(), Some(Errno::ENOTDIR));
    }

    #[test]
    fn rmdir_non_empty_is_enotempty() {
        let fs = new_fs();
        let r = root(&fs);
        let d = r.mkdir("d").unwrap();
        d.create("child").unwrap();
        assert_eq!(r.rmdir("d").err(), Some(Errno::ENOTEMPTY));
    }

    #[test]
    fn rmdir_empty_succeeds() {
        let fs = new_fs();
        let r = root(&fs);
        r.mkdir("d").unwrap();
        assert!(r.rmdir("d").is_ok());
        assert_eq!(r.lookup("d").err(), Some(Errno::ENOENT));
    }

    #[test]
    fn readdir_dot_and_dotdot_then_real_entries_then_none() {
        let fs = new_fs();
        let r = root(&fs);
        r.create("a").unwrap();
        r.create("b").unwrap();

        let e0 = r.readdir(0).unwrap().expect("offset 0 is '.'");
        assert_eq!(&e0.name[..e0.name_len], b".");
        let e1 = r.readdir(1).unwrap().expect("offset 1 is '..'");
        assert_eq!(&e1.name[..e1.name_len], b"..");

        // Offsets 2 and 3 are the two real entries (BTreeMap keeps them
        // sorted: "a" then "b").
        let e2 = r.readdir(2).unwrap().expect("offset 2 is a real entry");
        assert_eq!(&e2.name[..e2.name_len], b"a");
        let e3 = r.readdir(3).unwrap().expect("offset 3 is a real entry");
        assert_eq!(&e3.name[..e3.name_len], b"b");

        assert!(r.readdir(4).unwrap().is_none());
    }

    // ── nlink ────────────────────────────────────────────────────────────

    #[test]
    fn dir_nlink_is_2_plus_subdirectory_count() {
        let fs = new_fs();
        let r = root(&fs);
        assert_eq!(r.stat().st_nlink, 2); // empty: 2 + 0

        r.create("file1").unwrap();
        r.create("file2").unwrap();
        assert_eq!(r.stat().st_nlink, 2, "plain files don't count");

        r.mkdir("sub1").unwrap();
        assert_eq!(r.stat().st_nlink, 3);
        r.mkdir("sub2").unwrap();
        assert_eq!(r.stat().st_nlink, 4);
    }

    // ── chmod ────────────────────────────────────────────────────────────

    #[test]
    fn chmod_on_file_persists_and_is_visible_in_stat() {
        let fs = new_fs();
        let r = root(&fs);
        let f = r.create("f").unwrap();
        f.chmod(0o600).unwrap();
        assert_eq!(f.stat().st_mode & 0o7777, 0o600);
    }

    #[test]
    fn chmod_on_directory_persists_and_is_visible_in_stat() {
        let fs = new_fs();
        let r = root(&fs);
        let d = r.mkdir("d").unwrap();
        d.chmod(0o700).unwrap();
        assert_eq!(d.stat().st_mode & 0o7777, 0o700);
    }

    #[test]
    fn fchmod_and_inode_chmod_share_state() {
        let fs = new_fs();
        let r = root(&fs);
        let f = r.create("f").unwrap();
        let mut h = f.open(OpenFlags::RDWR).unwrap();

        // chmod via the inode is visible through the already-open handle.
        f.chmod(0o640).unwrap();
        assert_eq!(h.stat().unwrap().st_mode & 0o7777, 0o640);

        // fchmod via the handle is visible through the inode.
        h.chmod(0o444).unwrap();
        assert_eq!(f.stat().st_mode & 0o7777, 0o444);
    }

    #[test]
    fn chmod_only_stores_low_12_bits() {
        let fs = new_fs();
        let r = root(&fs);
        let f = r.create("f").unwrap();
        f.chmod(0xFFFF_FFFF).unwrap();
        assert_eq!(f.stat().st_mode & 0o7777, 0o7777);
        assert_eq!(f.stat().st_mode & !0o7777, FileType::Regular.as_mode_bits());
    }

    // ── Symlinks ─────────────────────────────────────────────────────────

    #[test]
    fn symlink_creates_the_node() {
        let fs = new_fs();
        let r = root(&fs);
        let link = r.symlink("link", "target.txt").unwrap();
        assert_eq!(link.file_type(), FileType::Symlink);
    }

    #[test]
    fn readlink_returns_target_verbatim() {
        let fs = new_fs();
        let r = root(&fs);
        r.symlink("rel", "sibling.txt").unwrap();
        assert_eq!(r.lookup("rel").unwrap().readlink().unwrap(), "sibling.txt");

        // Dangling target: legal, stored verbatim, no existence check.
        r.symlink("dangling", "/nowhere/at/all").unwrap();
        assert_eq!(r.lookup("dangling").unwrap().readlink().unwrap(), "/nowhere/at/all");
    }

    #[test]
    fn symlink_open_is_einval() {
        let fs = new_fs();
        let r = root(&fs);
        let link = r.symlink("link", "x").unwrap();
        assert_eq!(link.open(OpenFlags::RDONLY).err(), Some(Errno::EINVAL));
    }

    #[test]
    fn symlink_on_occupied_name_is_eexist() {
        let fs = new_fs();
        let r = root(&fs);
        r.create("taken").unwrap();
        assert_eq!(r.symlink("taken", "x").err(), Some(Errno::EEXIST));
    }

    // ── socket nodes (bind) ─────────────────────────────────────────────

    #[test]
    fn mksocket_creates_a_socket_typed_node() {
        let fs = new_fs();
        let r = root(&fs);
        let s = r.mksocket("sock").unwrap();
        assert_eq!(s.file_type(), FileType::Socket);
        assert_eq!(s.stat().st_mode & 0o170000, 0o140000, "S_IFSOCK");
        assert_eq!(s.stat().st_size, 0);
    }

    #[test]
    fn a_socket_node_cannot_be_opened() {
        let fs = new_fs();
        let r = root(&fs);
        let s = r.mksocket("sock").unwrap();
        assert_eq!(s.open(OpenFlags::RDONLY).err(), Some(Errno::ENXIO));
    }

    #[test]
    fn binding_over_an_existing_name_is_eexist() {
        // What makes a second bind() to the same path fail even after the
        // first socket has died: the node outlives it until unlinked.
        let fs = new_fs();
        let r = root(&fs);
        r.mksocket("sock").unwrap();
        assert_eq!(r.mksocket("sock").err(), Some(Errno::EEXIST));
        assert_eq!(r.create("sock").err(), None, "create() still finds the name taken");
    }

    #[test]
    fn a_socket_node_is_removed_by_unlink_like_any_other_name() {
        let fs = new_fs();
        let r = root(&fs);
        r.mksocket("sock").unwrap();
        r.unlink("sock").unwrap();
        assert_eq!(r.lookup("sock").err(), Some(Errno::ENOENT));
        assert!(r.mksocket("sock").is_ok(), "the name is free again");
    }

    #[test]
    fn a_socket_node_is_listed_by_readdir_as_dt_sock() {
        let fs = new_fs();
        let r = root(&fs);
        r.mksocket("sock").unwrap();
        let entry = (0..)
            .map_while(|i| r.readdir(i).ok().flatten())
            .find(|e| &e.name[..e.name_len] == b"sock")
            .expect("listed");
        assert_eq!(entry.kind.as_dt_type(), 12, "DT_SOCK");
    }

    // ── take_child / insert_child (rename primitives) ───────────────────

    #[test]
    fn take_child_removes_and_returns() {
        let fs = new_fs();
        let r = root(&fs);
        let f = r.create("f").unwrap();
        let taken = r.take_child("f").unwrap();
        assert_eq!(taken.stat().st_ino, f.stat().st_ino);
        assert_eq!(r.lookup("f").err(), Some(Errno::ENOENT));
    }

    #[test]
    fn take_child_missing_is_enoent() {
        let fs = new_fs();
        let r = root(&fs);
        assert_eq!(r.take_child("nope").err(), Some(Errno::ENOENT));
    }

    #[test]
    fn insert_child_on_occupied_name_is_eexist() {
        let fs = new_fs();
        let r = root(&fs);
        r.create("taken").unwrap();
        let f = r.create("other").unwrap();
        assert_eq!(r.insert_child("taken", f).err(), Some(Errno::EEXIST));
    }

    #[test]
    fn non_empty_directory_can_be_moved_via_take_child_insert_child() {
        // Unlike rmdir, take_child/insert_child never checks emptiness —
        // real POSIX rename() allows moving a non-empty directory.
        let fs = new_fs();
        let r = root(&fs);
        let src = r.mkdir("src").unwrap();
        src.create("inner.txt").unwrap();

        let taken = r.take_child("src").unwrap();
        let dst_dir = r.mkdir("dst").unwrap();
        dst_dir.insert_child("moved", taken).unwrap();

        let moved = dst_dir.lookup("moved").unwrap();
        assert_eq!(moved.file_type(), FileType::Directory);
        assert!(moved.lookup("inner.txt").is_ok(), "contents survive the move");
    }

    // ── Directory handle getdents64 snapshot semantics ──────────────────

    #[test]
    fn dir_handle_getdents64_snapshot_excludes_files_created_after_open() {
        // Deliberate, documented behavior (see the module doc comment) —
        // not a bug: a directory listing is a snapshot taken at open()
        // time, so files created afterward don't retroactively appear in
        // an already-open handle's listing.
        let fs = new_fs();
        let r = root(&fs);
        r.create("before").unwrap();

        let mut handle = r.open(OpenFlags::DIRECTORY).unwrap();
        r.create("after").unwrap(); // created after the snapshot was taken

        let mut buf = vec![0u8; 4096];
        let n = handle.getdents64(&mut buf);
        assert!(n > 0);
        let text = core::str::from_utf8(&buf[..n as usize]).unwrap_or("");
        // A crude but sufficient check: the snapshot bytes contain "before"
        // but not "after" anywhere in the packed names.
        assert!(contains_name(&buf[..n as usize], b"before"));
        assert!(!contains_name(&buf[..n as usize], b"after"));
        let _ = text;
    }

    /// Scan packed `linux_dirent64` records in `buf` for one whose name
    /// exactly matches `name` — used by the snapshot test above instead of
    /// a raw substring search, since `contains_name` must not accidentally
    /// match "after" as a substring of some longer, unrelated name.
    fn contains_name(buf: &[u8], name: &[u8]) -> bool {
        let mut pos = 0usize;
        while pos + 19 <= buf.len() {
            let reclen = u16::from_le_bytes(buf[pos + 16..pos + 18].try_into().unwrap()) as usize;
            if reclen == 0 || pos + reclen > buf.len() {
                break;
            }
            // Name runs from byte 19 up to (but not including) the null
            // terminator; scan for it rather than assuming a fixed length.
            let name_start = pos + 19;
            let mut name_end = name_start;
            while name_end < pos + reclen && buf[name_end] != 0 {
                name_end += 1;
            }
            if &buf[name_start..name_end] == name {
                return true;
            }
            pos += reclen;
        }
        false
    }

    // ── The DirLockObserver seam ─────────────────────────────────────────

    /// Records every observer call, in order, for the invariant test below.
    struct RecordingObserver {
        calls: Mutex<Vec<Call>>,
    }

    #[derive(Debug, PartialEq, Eq, Clone)]
    enum Call {
        CurrentPid,
        Acquire(&'static str),
        Release,
    }

    impl DirLockObserver for RecordingObserver {
        fn current_pid(&self) -> u64 {
            self.calls.lock().push(Call::CurrentPid);
            42
        }
        fn record_acquire(&self, pid: u64, op: &'static str) {
            assert_eq!(pid, 42, "the pid recorded must be the one current_pid() returned");
            self.calls.lock().push(Call::Acquire(op));
        }
        fn record_release(&self) {
            self.calls.lock().push(Call::Release);
        }
    }

    #[test]
    fn lock_entries_calls_observer_in_the_exact_required_order() {
        // What this actually pins down: current_pid() is called before
        // record_acquire(), record_acquire() receives the correct `op`,
        // and record_release() fires exactly once, after both, from the
        // guard's Drop. That's a real invariant and worth keeping.
        //
        // What it does NOT pin down, despite `lock_entries`'s doc comment
        // describing it as a hard rule: that record_acquire() actually runs
        // *after* the real `self.entries.lock()` call has returned the
        // lock. Verified empirically — moving
        // `self.observer.record_acquire(pid, op)` to *before*
        // `let guard = self.entries.lock();` in `lock_entries` (i.e.
        // deliberately violating that half of the invariant) still passes
        // all 150 tests in this crate, this one included. The observer
        // only ever sees the relative order of its own three calls, never
        // their position relative to the real lock acquisition; without a
        // second thread actually contending for `entries`, "recorded after
        // the lock" and "recorded before the lock" produce byte-identical
        // call sequences from a single thread's point of view. Today, that
        // second half of the invariant is held up only by reading
        // `lock_entries`'s source and its doc comment — not by this test,
        // not by anything that runs.
        //
        // This gap matters enough to write down, not just leave silently
        // true-in-practice: this repo already paid once for an instrument
        // that claimed to guard more than it measured (see
        // `docs/hang-hunt-bug2-findings.md`, and `lock_entries`'s own doc
        // comment, which explains why recording the acquire too early
        // would misattribute a still-spinning caller as the lock holder).
        // A test that *looks* like it closes that exact gap but doesn't is
        // precisely the failure mode that hunt was about. Closing it for
        // real needs a second thread genuinely contending for `entries` so
        // the observer's acquire-vs-lock ordering becomes actually
        // observable — see
        // `record_acquire_never_fires_while_another_thread_holds_the_same_entries_lock`
        // immediately below, which is the test that actually closes this
        // gap using two real host threads and a `Barrier` to force genuine
        // contention on the same `RamDirNode`'s `entries` lock.
        let observer: &'static RecordingObserver =
            Box::leak(Box::new(RecordingObserver { calls: Mutex::new(Vec::new()) }));
        let dir = RamDirNode::new(alloc_ino(), observer);

        dir.mkdir("child").expect("mkdir");

        let calls = observer.calls.lock();
        assert_eq!(
            &calls[..],
            &[Call::CurrentPid, Call::Acquire("mkdir"), Call::Release],
            "lock_entries must call current_pid, then record_acquire, then record_release, in that order"
        );
    }

    /// Observer used only by the contention test below. Pure atomics — no
    /// `Mutex` of its own, so this instrument doesn't introduce a second
    /// lock that could itself skew the timing it's trying to measure.
    struct ContendingObserver {
        /// Threads currently between `record_acquire` and `record_release`.
        inside: core::sync::atomic::AtomicUsize,
        /// Set once, if ever, `inside` is observed to exceed 1 — i.e. two
        /// threads both believe they hold the (single, non-reentrant)
        /// `entries` lock at the same time.
        overlap_seen: core::sync::atomic::AtomicBool,
        /// Only the first thread to reach `record_acquire` does the bounded
        /// wait below; the second just proceeds.
        first: core::sync::atomic::AtomicBool,
        /// Rendezvous point *before* either thread calls `entries.lock()`
        /// (see `current_pid`'s doc comment) — this is the real
        /// synchronization mechanism, not the bounded wait in
        /// `record_acquire`.
        gate: std::sync::Barrier,
    }

    impl DirLockObserver for ContendingObserver {
        fn current_pid(&self) -> u64 {
            // `current_pid()` is always called before `entries.lock()` in
            // `lock_entries` (both the real, correct version and the
            // saboteur below preserve that much). Blocking here on a
            // 2-count barrier guarantees both threads are released
            // together, right before they actually race for the real
            // `spin::Mutex` — genuine contention, not a hope-it-happens
            // race. This is synchronization, not the observation budget
            // below.
            self.gate.wait();
            7
        }

        fn record_acquire(&self, _pid: u64, _op: &'static str) {
            use core::sync::atomic::Ordering::SeqCst;
            let n = self.inside.fetch_add(1, SeqCst) + 1;
            if n > 1 {
                self.overlap_seen.store(true, SeqCst);
            }
            if self.first.swap(false, SeqCst) {
                // Only the first thread to arrive here waits, and only to
                // give the *correct* implementation's real mutual
                // exclusion a chance to be violated observably by the
                // saboteur. This bound is not flaky: in the sabotaged
                // implementation, the other thread only has to travel from
                // the barrier (which both threads just left together) to
                // its own `record_acquire` call — a handful of atomic ops,
                // nanoseconds. 500ms is roughly six orders of magnitude
                // more than that needs, and the wait exits immediately
                // (via `yield_now`, not a sleep) the moment overlap is
                // actually observed, so the common (correct) case pays
                // ~nothing. The barrier above is what makes the race
                // deterministic; this wait is purely an observation
                // window, not the synchronization.
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
                while std::time::Instant::now() < deadline {
                    if self.inside.load(SeqCst) > 1 {
                        self.overlap_seen.store(true, SeqCst);
                        break;
                    }
                    std::thread::yield_now();
                }
            }
        }

        fn record_release(&self) {
            self.inside.fetch_sub(1, core::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn record_acquire_never_fires_while_another_thread_holds_the_same_entries_lock() {
        // Family B: a contention probe using real host threads. This
        // models host-level contention (two `std::thread`s racing a
        // `spin::Mutex` on the host scheduler), not the kernel's own
        // single-core execution model — the kernel never actually runs two
        // CPUs through `lock_entries` on the same `RamDirNode`
        // concurrently the way this test forces. What it fixes that
        // `lock_entries_calls_observer_in_the_exact_required_order` does
        // not: that half of `lock_entries`'s invariant is entirely about
        // *timing relative to a real lock acquisition under contention* —
        // a property that is, by construction, unobservable from a single
        // thread with no contender. This test manufactures a real
        // contender (via the `Barrier` in `current_pid`, not a sleep) so
        // that "record_acquire fired while another thread was already
        // inside" becomes something that can actually be measured, not
        // just asserted in prose.
        use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};

        let observer: &'static ContendingObserver = Box::leak(Box::new(ContendingObserver {
            inside: AtomicUsize::new(0),
            overlap_seen: AtomicBool::new(false),
            first: AtomicBool::new(true),
            gate: std::sync::Barrier::new(2),
        }));
        let dir: &'static RamDirNode = Box::leak(Box::new(RamDirNode::new(alloc_ino(), observer)));

        let (tx, rx) = std::sync::mpsc::channel::<bool>();

        let tx_a = tx.clone();
        std::thread::spawn(move || {
            let ok = dir.mkdir("a").is_ok();
            tx_a.send(ok).expect("send from thread a");
        });
        let tx_b = tx.clone();
        std::thread::spawn(move || {
            let ok = dir.mkdir("b").is_ok();
            tx_b.send(ok).expect("send from thread b");
        });
        drop(tx);

        // Watchdog: never `join()` bare here. If `lock_entries` deadlocks
        // (e.g. a saboteur that also breaks real mutual exclusion in a way
        // that wedges the mutex), a bare `join()` would hang this test
        // forever instead of failing it.
        let ok_a = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap_or_else(|_| {
            panic!("watchdog: un hilo no terminó en 10s — lock_entries probablemente se autobloqueó")
        });
        let ok_b = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap_or_else(|_| {
            panic!("watchdog: un hilo no terminó en 10s — lock_entries probablemente se autobloqueó")
        });

        assert!(ok_a, "thread a's mkdir(\"a\") must succeed");
        assert!(ok_b, "thread b's mkdir(\"b\") must succeed");
        assert_eq!(
            observer.inside.load(SeqCst),
            0,
            "both critical sections must have released by the time both threads reported back"
        );
        assert!(
            !observer.overlap_seen.load(SeqCst),
            "two threads recorded an acquire simultaneously on the SAME RamDirNode's entries lock — \
             impossible if record_acquire is only ever called after the caller actually holds the \
             mutex, since spin::Mutex guarantees mutual exclusion. This means the instrument itself \
             (or lock_entries) named a still-spinning thread as the lock holder — exactly the \
             misattribution failure mode described in lock_entries's own doc comment and in \
             docs/hang-hunt-bug2-findings.md."
        );
    }

    #[test]
    fn observer_propagates_to_subdirectories_created_by_mkdir() {
        // If `mkdir` forgot to pass `self.observer` through to the child
        // `RamDirNode`, an operation on the child would silently use no
        // observer at all (a compile error, since there's no default) or
        // — if someone "fixed" that by defaulting to the noop observer —
        // would silently lose the diagnostic. Assert the child really
        // reports through the same observer as the parent.
        let observer: &'static RecordingObserver =
            Box::leak(Box::new(RecordingObserver { calls: Mutex::new(Vec::new()) }));
        let parent = RamDirNode::new(alloc_ino(), observer);

        let child = parent.mkdir("sub").expect("mkdir");
        observer.calls.lock().clear(); // only care about the child's own op now

        child.lookup("nonexistent").ok(); // any op that locks entries

        let calls = observer.calls.lock();
        assert_eq!(
            &calls[..],
            &[Call::CurrentPid, Call::Acquire("lookup"), Call::Release],
            "a child directory created by mkdir must report through the same observer as its parent"
        );
    }

    // ── End-to-end via a real MountTable ─────────────────────────────────

    #[test]
    fn mounted_ramfs_mkdir_write_read_by_absolute_path() {
        let table = MountTable::new();
        table.mount("/tmp", Arc::new(RamFs::new()));

        table.mkdir("/tmp/a").expect("mkdir /tmp/a");
        let mut h = table.open("/tmp/a/f.txt", OpenFlags(OpenFlags::CREAT.0 | OpenFlags::RDWR.0))
            .expect("create+open /tmp/a/f.txt");
        h.write(b"payload").unwrap();
        drop(h);

        let mut h2 = table.open("/tmp/a/f.txt", OpenFlags::RDONLY).expect("re-open");
        let mut buf = [0u8; 7];
        h2.read(&mut buf).unwrap();
        assert_eq!(&buf, b"payload");
    }

    #[test]
    fn mounted_ramfs_relative_symlink_resolves_to_sibling_via_table() {
        let table = MountTable::new();
        table.mount("/tmp", Arc::new(RamFs::new()));

        table.mkdir("/tmp/a").unwrap();
        table.open("/tmp/a/sibling.txt", OpenFlags(OpenFlags::CREAT.0 | OpenFlags::RDWR.0))
            .unwrap()
            .write(b"sibling-data").unwrap();
        table.symlink("sibling.txt", "/tmp/a/link").expect("relative symlink");

        let followed = table.resolve("/tmp/a/link").expect("resolve follows the symlink");
        assert_eq!(followed.file_type(), FileType::Regular);

        let not_followed = table.resolve_no_follow("/tmp/a/link").expect("resolve_no_follow");
        assert_eq!(not_followed.file_type(), FileType::Symlink);
        assert_eq!(not_followed.readlink().unwrap(), "sibling.txt");
    }

    #[test]
    fn mounted_ramfs_rename_moves_file_and_old_path_no_longer_resolves() {
        let table = MountTable::new();
        table.mount("/tmp", Arc::new(RamFs::new()));

        table.mkdir("/tmp/a").unwrap();
        table.open("/tmp/a/f", OpenFlags(OpenFlags::CREAT.0 | OpenFlags::RDWR.0))
            .unwrap()
            .write(b"data").unwrap();

        table.rename("/tmp/a/f", "/tmp/a/g").expect("rename");

        assert_eq!(table.resolve("/tmp/a/f").err(), Some(Errno::ENOENT));
        let mut h = table.open("/tmp/a/g", OpenFlags::RDONLY).expect("renamed file opens");
        let mut buf = [0u8; 4];
        h.read(&mut buf).unwrap();
        assert_eq!(&buf, b"data");
    }

    #[test]
    fn mounted_ramfs_symlink_chain_too_long_is_eloop() {
        let table = MountTable::new();
        table.mount("/tmp", Arc::new(RamFs::new()));

        table.symlink("/tmp/link1", "/tmp/link0").unwrap();
        table.symlink("/tmp/link2", "/tmp/link1").unwrap();
        table.symlink("/tmp/link3", "/tmp/link2").unwrap();
        table.symlink("/tmp/link4", "/tmp/link3").unwrap();
        table.symlink("/tmp/link5", "/tmp/link4").unwrap();
        table.symlink("/tmp/link6", "/tmp/link5").unwrap();
        table.symlink("/tmp/link7", "/tmp/link6").unwrap();
        table.symlink("/tmp/link8", "/tmp/link7").unwrap();
        table.symlink("/tmp/link9", "/tmp/link8").unwrap();
        // link0 -> link1 -> ... -> link9 -> (nothing): 9 hops, one more
        // than MAX_SYMLINK_HOPS (8) — must be ELOOP, not a hang.
        assert_eq!(table.resolve("/tmp/link0").err(), Some(Errno::ELOOP));
    }
}
