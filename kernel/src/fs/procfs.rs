// kernel/src/fs/procfs.rs
//
// Minimal /proc — meminfo, plus real per-process symlinks (/proc/self,
// /proc/<pid>/exe). Exists so real Unix tools/scripts (busybox `free`,
// `cat /proc/meminfo`, `ash`'s `execve("/proc/self/exe", ...)` re-exec
// trick for any applet that isn't NOFORK/NOEXEC) work here the same way
// they would on a real system, instead of leaning on kernel-specific
// syscalls or ad-hoc string special-casing.
//
// LAYOUT
// ──────
//   /proc/           (ProcDirInode)
//   ├── meminfo
//   ├── self         → symlink to /proc/<own pid>
//   └── <pid>/       (ProcPidDirInode, only for a pid that actually exists)
//       └── exe      → symlink to whatever ELF path that process is running
//
// Real Linux's /proc/<pid> has dozens of entries (cmdline, status, fd/,
// maps, ...) — only `exe` exists here, since that's the one thing
// anything in this kernel actually consumes (`ash`'s FEATURE_SH_STANDALONE
// re-exec). `readdir` on the root only lists the always-present entries
// (meminfo, self) — it does not enumerate live pids, so `ls /proc` won't
// show every process; direct lookup (`cat /proc/3/exe`, `cd /proc/3`)
// still works for any pid that's actually alive.
//
// Inode numbers: 200 = /proc directory, 201 = meminfo, 202 = self.
// Per-pid inodes are derived from the pid (see `pid_dir_ino`/`pid_exe_ino`).

use alloc::{boxed::Box, format, string::String, sync::Arc, vec::Vec};

use crate::fs::{
    types::{DirEntry, Errno, FileType, OpenFlags, Stat},
    vfs::{Filesystem, Inode},
};
use crate::process::file::{FileError, FileHandle, FileResult};

fn pid_dir_ino(pid: usize) -> u64 { 1000 + (pid as u64) * 2 }
fn pid_exe_ino(pid: usize) -> u64 { 1000 + (pid as u64) * 2 + 1 }

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
            "self" => Ok(Arc::new(SelfInode)),
            _ => {
                let pid: usize = name.parse().map_err(|_| Errno::ENOENT)?;
                if crate::process::scheduler::exe_name_for_pid(pid).is_some() {
                    Ok(Arc::new(ProcPidDirInode { pid }))
                } else {
                    Err(Errno::ENOENT)
                }
            }
        }
    }

    fn readdir(&self, offset: u64) -> Result<Option<DirEntry>, Errno> {
        match offset {
            0 => Ok(Some(DirEntry::new(200, FileType::Directory, b"."))),
            1 => Ok(Some(DirEntry::new(200, FileType::Directory, b".."))),
            2 => Ok(Some(DirEntry::new(201, FileType::Regular, b"meminfo"))),
            3 => Ok(Some(DirEntry::new(202, FileType::Symlink, b"self"))),
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

// ── self symlink inode ───────────────────────────────────────────────────────

/// `/proc/self` — always resolves to the *calling* process's own pid, not
/// a fixed target: `readlink()` (and hence `open()`/`stat()` via
/// `resolve()`'s symlink-following) re-queries the current pid every time
/// it's traversed, exactly like the real thing.
struct SelfInode;

impl Inode for SelfInode {
    fn stat(&self) -> Stat {
        Stat::symlink(202, self.readlink().map(|s| s.len()).unwrap_or(0) as i64)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        // Real Unix: open() on a symlink (without O_NOFOLLOW/O_PATH) opens
        // the target, not the link itself. Nothing in this kernel opens
        // /proc/self directly (only traverses through it, e.g.
        // /proc/self/exe), so this is unreachable in practice — EINVAL is
        // a reasonable stand-in for "not a regular file."
        Err(Errno::EINVAL)
    }

    fn readlink(&self) -> Result<String, Errno> {
        let pid = crate::process::scheduler::current_pid_safe().ok_or(Errno::ENOENT)?;
        Ok(format!("/proc/{}", pid))
    }
}

// ── /proc/<pid> directory inode ──────────────────────────────────────────────

struct ProcPidDirInode {
    pid: usize,
}

impl Inode for ProcPidDirInode {
    fn stat(&self) -> Stat {
        Stat::dir(pid_dir_ino(self.pid))
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        Ok(Box::new(ProcPidDirHandle { pid: self.pid, offset: 0 }))
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        match name {
            "exe" => Ok(Arc::new(ProcExeInode { pid: self.pid })),
            _ => Err(Errno::ENOENT),
        }
    }

    fn readdir(&self, offset: u64) -> Result<Option<DirEntry>, Errno> {
        let ino = pid_dir_ino(self.pid);
        match offset {
            0 => Ok(Some(DirEntry::new(ino, FileType::Directory, b"."))),
            1 => Ok(Some(DirEntry::new(ino, FileType::Directory, b".."))),
            2 => Ok(Some(DirEntry::new(pid_exe_ino(self.pid), FileType::Symlink, b"exe"))),
            _ => Ok(None),
        }
    }
}

struct ProcPidDirHandle {
    pid:    usize,
    offset: u64,
}

impl FileHandle for ProcPidDirHandle {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument)
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument)
    }

    fn getdents64(&mut self, buf: &mut [u8]) -> i64 {
        let dir = ProcPidDirInode { pid: self.pid };
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
        Some(Stat::dir(pid_dir_ino(self.pid)))
    }

    fn name(&self) -> &str { "procfs/pid-dir" }
}

// ── /proc/<pid>/exe symlink inode ────────────────────────────────────────────

/// The real payload this whole module was added for: a symlink whose
/// target is whatever `PROGRAMS`-registered path `pid` is currently
/// running (`Process::exe_name`, kept up to date by every successful
/// `exec()` — see `syscall::sys_exec`). `execve("/proc/self/exe", ...)`
/// resolves through here via the VFS's normal symlink-following, the same
/// mechanism any other symlink gets — no special-casing left in `sys_exec`
/// itself.
struct ProcExeInode {
    pid: usize,
}

impl Inode for ProcExeInode {
    fn stat(&self) -> Stat {
        let len = self.readlink().map(|s| s.len()).unwrap_or(0);
        Stat::symlink(pid_exe_ino(self.pid), len as i64)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        Err(Errno::EINVAL) // see SelfInode::open's doc comment
    }

    fn readlink(&self) -> Result<String, Errno> {
        match crate::process::scheduler::exe_name_for_pid(self.pid) {
            Some(name) if !name.is_empty() => Ok(name),
            _ => Err(Errno::ENOENT), // pid gone, or never exec'd a real ELF (kernel process)
        }
    }
}

// ── Open file handles ────────────────────────────────────────────────────────

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
