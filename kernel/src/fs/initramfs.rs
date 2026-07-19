// kernel/src/fs/initramfs.rs
//
// In-memory filesystem backed by ELF binaries embedded at compile time.
//
// LAYOUT
// ──────
//   /
//   ├── bin/                (real subdirectory, owned by this filesystem)
//   │   ├── shell
//   │   ├── uname
//   │   └── …                (one entry per PROGRAMS registry entry)
//   ├── dev/                 (empty placeholder — real content lives behind
//   ├── tmp/                  the /dev, /tmp, /mnt, /proc mounts; traversal
//   ├── mnt/                  into them is redirected there by the VFS
//   └── proc/                 mount table before ever reaching this inode)
//
// The placeholders under root aren't hardcoded: `RootDirInode` asks
// `vfs::direct_children("/")` for every *other* mount and lists it, mirroring
// how a real Linux rootfs has actual empty directories that mounts overlay
// (see that function's doc comment). This is what makes `ls /` show `bin`,
// `dev`, `tmp`, `proc`, etc. instead of just `bin`.
//
// BusyBox applets (vi, grep, sed, ...) do NOT get a symlink here — this
// filesystem is compile-time-baked and read-only, so a symlink under /bin
// could only ever be a synthetic, computed-on-the-fly stand-in for a real
// one. Real symlinks belong on a writable mount: `init::processes`'s PID 1
// runs actual `busybox --install -s /tmp/bin` at boot (real `symlink(2)`,
// see `ramfs::RamDirNode::symlink`), the same mechanism a real Linux install
// uses (one multi-call binary + real symlinks + argv[0] dispatch) — nothing
// synthetic, no kernel-side awareness of the applet list required.
//
// All files are read-only.  Writes return EROFS.
// Inode numbers: 1 = root dir, 2 = /bin dir, 3+ = files (registry index + 3),
// 100+ = mount placeholder dirs (index into `direct_children`, cosmetic only).

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

const ROOT_INO: u64 = 1;
const BIN_INO: u64 = 2;
const MOUNT_PLACEHOLDER_INO_BASE: u64 = 100;
const BUSYBOX_APPLET_INO_BASE: u64 = 1000;

// ── Filesystem ───────────────────────────────────────────────────────────────

pub struct InitramfsFs;

impl Filesystem for InitramfsFs {
    fn name(&self) -> &str { "initramfs" }

    fn root(&self) -> Arc<dyn Inode> {
        Arc::new(RootDirInode)
    }
}

// ── Root directory: contains only "bin" ─────────────────────────────────────

struct RootDirInode;

impl Inode for RootDirInode {
    fn stat(&self) -> Stat {
        Stat::dir(ROOT_INO)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        Ok(Box::new(DirHandle { kind: DirKind::Root, offset: 0 }))
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        if name == "bin" {
            return Ok(Arc::new(BinDirInode));
        }
        let children = crate::fs::vfs::direct_children("/");
        match children.iter().position(|&n| n == name) {
            Some(idx) => Ok(Arc::new(MountPointDirInode {
                ino: MOUNT_PLACEHOLDER_INO_BASE + idx as u64,
            })),
            None => Err(Errno::ENOENT),
        }
    }

    fn readdir(&self, offset: u64) -> Result<Option<DirEntry>, Errno> {
        match offset {
            0 => Ok(Some(DirEntry::new(ROOT_INO, FileType::Directory, b"."))),
            1 => Ok(Some(DirEntry::new(ROOT_INO, FileType::Directory, b".."))),
            2 => Ok(Some(DirEntry::new(BIN_INO, FileType::Directory, b"bin"))),
            n => {
                let idx = (n - 3) as usize;
                let children = crate::fs::vfs::direct_children("/");
                if idx >= children.len() {
                    return Ok(None);
                }
                let ino = MOUNT_PLACEHOLDER_INO_BASE + idx as u64;
                Ok(Some(DirEntry::new(ino, FileType::Directory, children[idx].as_bytes())))
            }
        }
    }
}

// ── Mount placeholder directory: empty, cosmetic only ───────────────────────
//
// Represents a *different* mount (`/dev`, `/tmp`, `/mnt`, `/proc`, ...) as
// seen from root's own listing. Real traversal into e.g. "/dev/console"
// never reaches this inode — `vfs::resolve_inner` picks the longer, more
// specific "/dev" mount prefix first — so this only ever needs to look like
// an empty directory, never actually serve one.
struct MountPointDirInode {
    ino: u64,
}

impl Inode for MountPointDirInode {
    fn stat(&self) -> Stat {
        Stat::dir(self.ino)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        Ok(Box::new(DirHandle { kind: DirKind::MountPoint(self.ino), offset: 0 }))
    }

    fn lookup(&self, _name: &str) -> Result<Arc<dyn Inode>, Errno> {
        Err(Errno::ENOENT)
    }

    fn readdir(&self, offset: u64) -> Result<Option<DirEntry>, Errno> {
        match offset {
            0 => Ok(Some(DirEntry::new(self.ino, FileType::Directory, b"."))),
            1 => Ok(Some(DirEntry::new(ROOT_INO, FileType::Directory, b".."))),
            _ => Ok(None),
        }
    }
}

// ── /bin directory: one entry per embedded ELF ──────────────────────────────

struct BinDirInode;

impl Inode for BinDirInode {
    fn stat(&self) -> Stat {
        Stat::dir(BIN_INO)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        Ok(Box::new(DirHandle { kind: DirKind::Bin, offset: 0 }))
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        for (i, (prog_name, source)) in list_programs().iter().enumerate() {
            if *prog_name == name {
                if let ProgramSource::Elf(data) = source {
                    let ino = (i as u64) + 3;
                    return Ok(Arc::new(InitramfsFileInode { ino, data }));
                }
            }
        }
        Err(Errno::ENOENT)
    }

    fn readdir(&self, offset: u64) -> Result<Option<DirEntry>, Errno> {
        match offset {
            0 => Ok(Some(DirEntry::new(BIN_INO, FileType::Directory, b"."))),
            1 => Ok(Some(DirEntry::new(ROOT_INO, FileType::Directory, b".."))),
            n => {
                let idx = (n - 2) as usize;
                let programs = list_programs();
                if idx >= programs.len() {
                    return Ok(None);
                }
                let (name, _) = &programs[idx];
                let ino = idx as u64 + 3;
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
        Stat::executable(self.ino, self.data.len() as i64)
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
        Some(Stat::executable(0, self.data.len() as i64))
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(RamFile { data: self.data, offset: self.offset.clone() }))
    }

    fn seek(&mut self, offset: i64, whence: i32) -> FileResult<i64> {
        let mut cur = self.offset.lock();
        let new_pos = crate::process::file::compute_seek(*cur as i64, self.data.len() as i64, offset, whence)?;
        *cur = new_pos as usize;
        Ok(new_pos)
    }

    fn name(&self) -> &str { "initramfs" }
}

/// Directory handle: keeps a readdir cursor and serves `getdents64`, shared
/// by `RootDirInode`, `BinDirInode` and every `MountPointDirInode` (only
/// their `readdir` differs).
enum DirKind {
    Root,
    Bin,
    MountPoint(u64),
}

struct DirHandle {
    kind:   DirKind,
    offset: u64,
}

impl FileHandle for DirHandle {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument) // directories use getdents64
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument)
    }

    fn getdents64(&mut self, buf: &mut [u8]) -> i64 {
        let mut written: usize = 0;

        loop {
            let entry = match &self.kind {
                DirKind::Root => RootDirInode.readdir(self.offset),
                DirKind::Bin => BinDirInode.readdir(self.offset),
                DirKind::MountPoint(ino) => MountPointDirInode { ino: *ino }.readdir(self.offset),
            };
            let entry = match entry {
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
        match self.kind {
            DirKind::Root => Some(Stat::dir(ROOT_INO)),
            DirKind::Bin => Some(Stat::dir(BIN_INO)),
            DirKind::MountPoint(ino) => Some(Stat::dir(ino)),
        }
    }

    fn name(&self) -> &str {
        match self.kind {
            DirKind::Root => "initramfs/root",
            DirKind::Bin => "initramfs/bin",
            DirKind::MountPoint(_) => "initramfs/mountpoint",
        }
    }
}
