// kernel/src/fs/types.rs
//
// Shared VFS types: Stat, Errno, DirEntry, FileType, OpenFlags.
//
// No dependencies on other kernel modules — safe to import from anywhere.

// ── Errno ────────────────────────────────────────────────────────────────────

/// POSIX error numbers, compatible with Linux x86-64 ABI.
///
/// Syscall handlers return `errno.as_i64()` (a negative value) on failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Errno(pub i32);

#[allow(dead_code)]
impl Errno {
    pub const EPERM:   Self = Self(1);
    pub const ENOENT:  Self = Self(2);
    pub const EIO:     Self = Self(5);
    pub const EBADF:   Self = Self(9);
    pub const ENOMEM:  Self = Self(12);
    pub const EFAULT:  Self = Self(14);
    pub const EBUSY:   Self = Self(16);
    pub const EEXIST:  Self = Self(17);
    pub const ENOTDIR: Self = Self(20);
    pub const EISDIR:  Self = Self(21);
    pub const EINVAL:  Self = Self(22);
    pub const ENOSPC:  Self = Self(28);
    pub const EROFS:   Self = Self(30);
    pub const EPIPE:   Self = Self(32);
    pub const ERANGE:  Self = Self(34);
    pub const ENOSYS:  Self = Self(38);
    pub const ENOTEMPTY: Self = Self(39);

    /// Syscall return value for this error (negative i64).
    #[inline]
    pub fn as_i64(self) -> i64 {
        -(self.0 as i64)
    }
}

// ── File type ────────────────────────────────────────────────────────────────

/// Inode type — determines which operations are valid on a node.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileType {
    Regular,
    Directory,
    CharDevice,
    BlockDevice,
    Symlink,
}

impl FileType {
    /// `d_type` constant for `linux_dirent64`.
    pub fn as_dt_type(self) -> u8 {
        match self {
            Self::Regular     => 8,  // DT_REG
            Self::Directory   => 4,  // DT_DIR
            Self::CharDevice  => 2,  // DT_CHR
            Self::BlockDevice => 6,  // DT_BLK
            Self::Symlink     => 10, // DT_LNK
        }
    }

    /// `st_mode` bits for `Stat`.
    pub fn as_mode_bits(self) -> u32 {
        match self {
            Self::Regular     => 0o100000,
            Self::Directory   => 0o040000,
            Self::CharDevice  => 0o020000,
            Self::BlockDevice => 0o060000,
            Self::Symlink     => 0o120000,
        }
    }
}

// ── Open flags ───────────────────────────────────────────────────────────────

/// O_* flags passed to `open()` / `sys_open()`.
#[derive(Clone, Copy)]
pub struct OpenFlags(pub i32);

#[allow(dead_code)]
impl OpenFlags {
    pub const RDONLY:    Self = Self(0);
    pub const WRONLY:    Self = Self(1);
    pub const RDWR:      Self = Self(2);
    pub const CREAT:     Self = Self(0o100);
    pub const TRUNC:     Self = Self(0o1000);
    pub const APPEND:    Self = Self(0o2000);
    pub const DIRECTORY: Self = Self(0o200000);

    /// True if the file is opened for writing.
    #[inline]
    pub fn is_write(self) -> bool {
        self.0 & 3 != 0 // O_WRONLY or O_RDWR
    }

    /// True if O_DIRECTORY is set (open must be a directory).
    #[inline]
    pub fn is_directory(self) -> bool {
        self.0 & 0o200000 != 0
    }
}

// ── Stat ─────────────────────────────────────────────────────────────────────

/// `struct stat` as defined by the Linux x86-64 ABI (144 bytes).
///
/// Must match the layout expected by glibc / mlibc's `sys/stat.h`.
#[repr(C)]
pub struct Stat {
    pub st_dev:        u64,   // Device ID of containing filesystem
    pub st_ino:        u64,   // Inode number
    pub st_nlink:      u64,   // Number of hard links
    pub st_mode:       u32,   // File type + permissions
    pub st_uid:        u32,   // Owner user ID
    pub st_gid:        u32,   // Owner group ID
    _pad0:             u32,
    pub st_rdev:       u64,   // Device ID (for special files)
    pub st_size:       i64,   // Total size in bytes
    pub st_blksize:    i64,   // Preferred I/O block size
    pub st_blocks:     i64,   // Number of 512-byte blocks
    pub st_atime:      u64,   // Access time (seconds)
    pub st_atime_nsec: u64,   // Access time (nanoseconds)
    pub st_mtime:      u64,   // Modification time (seconds)
    pub st_mtime_nsec: u64,   // Modification time (nanoseconds)
    pub st_ctime:      u64,   // Status change time (seconds)
    pub st_ctime_nsec: u64,   // Status change time (nanoseconds)
    _reserved:         [i64; 3],
}

const _: () = assert!(
    core::mem::size_of::<Stat>() == 144,
    "Stat must be 144 bytes (Linux x86-64 ABI)"
);

impl Stat {
    /// Construct a directory stat.
    pub fn dir(ino: u64) -> Self {
        Self {
            st_dev: 1, st_ino: ino, st_nlink: 2,
            st_mode: FileType::Directory.as_mode_bits() | 0o755,
            st_uid: 0, st_gid: 0, _pad0: 0, st_rdev: 0,
            st_size: 0, st_blksize: 4096, st_blocks: 0,
            st_atime: 0, st_atime_nsec: 0,
            st_mtime: 0, st_mtime_nsec: 0,
            st_ctime: 0, st_ctime_nsec: 0,
            _reserved: [0; 3],
        }
    }

    /// Construct a regular-file stat.
    pub fn regular(ino: u64, size: i64) -> Self {
        Self {
            st_dev: 1, st_ino: ino, st_nlink: 1,
            st_mode: FileType::Regular.as_mode_bits() | 0o444,
            st_uid: 0, st_gid: 0, _pad0: 0, st_rdev: 0,
            st_size: size, st_blksize: 4096,
            st_blocks: (size + 511) / 512,
            st_atime: 0, st_atime_nsec: 0,
            st_mtime: 0, st_mtime_nsec: 0,
            st_ctime: 0, st_ctime_nsec: 0,
            _reserved: [0; 3],
        }
    }

    /// Construct a character-device stat.
    pub fn chardev(ino: u64) -> Self {
        Self {
            st_dev: 1, st_ino: ino, st_nlink: 1,
            st_mode: FileType::CharDevice.as_mode_bits() | 0o666,
            st_uid: 0, st_gid: 0, _pad0: 0, st_rdev: 0,
            st_size: 0, st_blksize: 4096, st_blocks: 0,
            st_atime: 0, st_atime_nsec: 0,
            st_mtime: 0, st_mtime_nsec: 0,
            st_ctime: 0, st_ctime_nsec: 0,
            _reserved: [0; 3],
        }
    }
}

// ── DirEntry ─────────────────────────────────────────────────────────────────

/// An in-kernel directory entry, produced by `Inode::readdir`.
///
/// Converted to `linux_dirent64` on-the-fly by `getdents64()`.
pub struct DirEntry {
    pub ino:      u64,
    pub kind:     FileType,
    pub name:     [u8; 256],  // null-terminated, valid up to name_len
    pub name_len: usize,
}

impl DirEntry {
    /// Construct from a byte-slice name (truncated to 255 chars).
    pub fn new(ino: u64, kind: FileType, name: &[u8]) -> Self {
        let mut entry = Self { ino, kind, name: [0u8; 256], name_len: 0 };
        let len = name.len().min(255);
        entry.name[..len].copy_from_slice(&name[..len]);
        entry.name_len = len;
        entry
    }

    /// Size of the corresponding `linux_dirent64` record (8-byte aligned).
    ///
    /// Layout: ino(8) + off(8) + reclen(2) + type(1) + name(len+1) → aligned.
    pub fn dirent64_size(&self) -> usize {
        let raw = 19 + self.name_len + 1; // +1 for null terminator
        (raw + 7) & !7
    }

    /// Serialize as `linux_dirent64` into `buf`.
    ///
    /// `next_off` is the opaque offset to the *next* entry (written into
    /// `d_off`).  `buf` must be at least `dirent64_size()` bytes.
    pub fn write_dirent64(&self, next_off: i64, buf: &mut [u8]) {
        let reclen = self.dirent64_size() as u16;
        buf[0..8].copy_from_slice(&self.ino.to_le_bytes());
        buf[8..16].copy_from_slice(&next_off.to_le_bytes());
        buf[16..18].copy_from_slice(&reclen.to_le_bytes());
        buf[18] = self.kind.as_dt_type();
        buf[19..19 + self.name_len].copy_from_slice(&self.name[..self.name_len]);
        buf[19 + self.name_len] = 0; // null terminator
        for b in buf[20 + self.name_len..reclen as usize].iter_mut() {
            *b = 0; // zero padding
        }
    }
}
