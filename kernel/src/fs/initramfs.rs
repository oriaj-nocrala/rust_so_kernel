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
//   ├── etc/                (static configuration files, `ETC_FILES`)
//   │   ├── localtime, passwd, group
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
// see `vfs::ramfs::RamDirNode::symlink`), the same mechanism a real Linux install
// uses (one multi-call binary + real symlinks + argv[0] dispatch) — nothing
// synthetic, no kernel-side awareness of the applet list required.
//
// All files are read-only.  Writes return EROFS.
// Inode numbers: 1 = root dir, 2 = /bin dir, 3+ = files (registry index + 3),
// 50 = /etc, 60+ = its files, 100+ = mount placeholder dirs (index into
// `direct_children`, cosmetic only).

use alloc::{boxed::Box, sync::Arc};
use crate::sync::Mutex;

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
const ETC_INO: u64 = 50;
const ETC_FILE_INO_BASE: u64 = 60;
const MOUNT_PLACEHOLDER_INO_BASE: u64 = 100;

/// The files of `/etc`: what a libc or a portable program expects to find
/// there, as data compiled into the kernel.
const ETC_FILES: &[(&str, &[u8])] = &[
    // mlibc's `localtime`/`tzset` read the zone from `/etc/localtime` and
    // ignore `TZ`; without the file they panic (`uptime`, `date`, anything
    // formatting local time died). The clock is UTC (`rtc` reads it as
    // UTC and nothing here has a zone), so this is the zone file for UTC:
    // TZif version 1, no transitions, one type (offset 0, not DST, "UTC").
    ("localtime", &UTC_TZIF),
    // The one user and group there are: every process runs as uid/gid 0
    // (no permission model). With them, `getpwuid`/`getgrgid` answer, so
    // `id`, `ps`, `ls -l` and `find -user` say `root` instead of `0` or
    // nothing. Home is `/tmp` — the root filesystem is read-only and there
    // is no `/root`; `/` would make ash's prompt call every path `~/...` —
    // and the shell `/tmp/bin/sh`, BusyBox's, which is what exists.
    ("passwd", b"root:x:0:0:root:/tmp:/tmp/bin/sh\n"),
    ("group", b"root:x:0:\n"),
];

const UTC_TZIF: [u8; 54] = {
    let mut b = [0u8; 54];
    // Magic; version 0 (v1); 15 reserved bytes.
    b[0] = b'T'; b[1] = b'Z'; b[2] = b'i'; b[3] = b'f';
    // Six big-endian counts at 20..44: isutcnt, isstdcnt, leapcnt, timecnt
    // (all 0), typecnt = 1, charcnt = 4.
    b[39] = 1;
    b[43] = 4;
    // ttinfo at 44: utoff 0 (4 bytes), isdst 0, desigidx 0. Then "UTC\0".
    b[50] = b'U'; b[51] = b'T'; b[52] = b'C';
    b
};
const BUSYBOX_APPLET_INO_BASE: u64 = 1000;

// ── Filesystem ───────────────────────────────────────────────────────────────

pub struct InitramfsFs;

impl Filesystem for InitramfsFs {
    fn name(&self) -> &str { "initramfs" }

    fn root(&self) -> Result<Arc<dyn Inode>, Errno> {
        Ok(Arc::new(RootDirInode))
    }
}

// ── Root directory: "bin", "etc" and the other mounts ─────────────────────

struct RootDirInode;

impl Inode for RootDirInode {
    fn as_any(&self) -> &dyn core::any::Any { self }

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
        if name == "etc" {
            return Ok(Arc::new(EtcDirInode));
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
            3 => Ok(Some(DirEntry::new(ETC_INO, FileType::Directory, b"etc"))),
            n => {
                let idx = (n - 4) as usize;
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
    fn as_any(&self) -> &dyn core::any::Any { self }

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
    fn as_any(&self) -> &dyn core::any::Any { self }

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
                    return Ok(Arc::new(InitramfsFileInode { ino, data, executable: true }));
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

// ── /etc directory: `ETC_FILES` ──────────────────────────────────────────────

struct EtcDirInode;

impl Inode for EtcDirInode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        Stat::dir(ETC_INO)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        Ok(Box::new(DirHandle { kind: DirKind::Etc, offset: 0 }))
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        ETC_FILES.iter().position(|(n, _)| *n == name)
            .map(|i| Arc::new(InitramfsFileInode {
                ino: ETC_FILE_INO_BASE + i as u64,
                data: ETC_FILES[i].1,
                executable: false,
            }) as Arc<dyn Inode>)
            .ok_or(Errno::ENOENT)
    }

    fn readdir(&self, offset: u64) -> Result<Option<DirEntry>, Errno> {
        match offset {
            0 => Ok(Some(DirEntry::new(ETC_INO, FileType::Directory, b"."))),
            1 => Ok(Some(DirEntry::new(ROOT_INO, FileType::Directory, b".."))),
            n => Ok(ETC_FILES.get((n - 2) as usize).map(|(name, _)| {
                DirEntry::new(ETC_FILE_INO_BASE + n - 2, FileType::Regular, name.as_bytes())
            })),
        }
    }
}

// ── File inode ───────────────────────────────────────────────────────────────

struct InitramfsFileInode {
    ino:  u64,
    data: &'static [u8],
    /// A program (`/bin`, mode 0555) or data (`/etc`, 0444).
    executable: bool,
}

impl InitramfsFileInode {
    fn stat_of(ino: u64, len: usize, executable: bool) -> Stat {
        if executable { Stat::executable(ino, len as i64) } else { Stat::regular(ino, len as i64) }
    }
}

impl Inode for InitramfsFileInode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        InitramfsFileInode::stat_of(self.ino, self.data.len(), self.executable)
    }

    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        if flags.is_write() {
            return Err(Errno::EROFS);
        }
        Ok(Box::new(RamFile::new(self.data, self.ino, self.executable)))
    }
}

// ── Open file handles ────────────────────────────────────────────────────────

/// Seekable read-only file handle over a static byte slice.
struct RamFile {
    data:   &'static [u8],
    /// What `fstat` reports: its inode's number and kind.
    ino:        u64,
    executable: bool,
    // Arc'd so dup()/dup2() can share one true "open file description"
    // position between two fds (POSIX dup() semantics) — see ramfs.rs's
    // RamFileHandle, which has the exact same reasoning.
    offset: Arc<Mutex<usize>>,
}

impl RamFile {
    fn new(data: &'static [u8], ino: u64, executable: bool) -> Self {
        Self { data, ino, executable, offset: Arc::new(Mutex::new(0)) }
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
        Some(InitramfsFileInode::stat_of(self.ino, self.data.len(), self.executable))
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(RamFile {
            data: self.data,
            ino: self.ino,
            executable: self.executable,
            offset: self.offset.clone(),
        }))
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
/// by `RootDirInode`, `BinDirInode`, `EtcDirInode` and every
/// `MountPointDirInode` (only their `readdir` differs).
enum DirKind {
    Root,
    Bin,
    Etc,
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
        match self.kind {
            DirKind::Root => crate::fs::vfs::getdents64_via_readdir(&RootDirInode, &mut self.offset, buf),
            DirKind::Bin => crate::fs::vfs::getdents64_via_readdir(&BinDirInode, &mut self.offset, buf),
            DirKind::Etc => crate::fs::vfs::getdents64_via_readdir(&EtcDirInode, &mut self.offset, buf),
            DirKind::MountPoint(ino) => crate::fs::vfs::getdents64_via_readdir(&MountPointDirInode { ino }, &mut self.offset, buf),
        }
    }

    fn stat(&self) -> Option<crate::fs::types::Stat> {
        match self.kind {
            DirKind::Root => Some(Stat::dir(ROOT_INO)),
            DirKind::Bin => Some(Stat::dir(BIN_INO)),
            DirKind::Etc => Some(Stat::dir(ETC_INO)),
            DirKind::MountPoint(ino) => Some(Stat::dir(ino)),
        }
    }

    fn name(&self) -> &str {
        match self.kind {
            DirKind::Root => "initramfs/root",
            DirKind::Bin => "initramfs/bin",
            DirKind::Etc => "initramfs/etc",
            DirKind::MountPoint(_) => "initramfs/mountpoint",
        }
    }
}
