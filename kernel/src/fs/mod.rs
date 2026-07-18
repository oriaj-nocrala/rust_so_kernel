// kernel/src/fs/mod.rs
//
// VFS public API.
//
// MODULES
//   types      — Stat, Errno, DirEntry, FileType, OpenFlags
//   vfs        — Inode + Filesystem traits, MountTable, path resolution
//   initramfs  — /bin/*  backed by embedded ELF bytes
//   devfs      — /dev/*  backed by the driver registry
//   ramfs      — /tmp/*  writable, in-memory scratch space
//   ext2       — /mnt/*  read-only, backed by the ATA disk (persists across reboots)
//   procfs     — /proc/* read-only, generated on open() (currently just meminfo)
//
// MOUNT LAYOUT (after init())
//   /dev   → DevFs
//   /bin   → InitramfsFs  (mounted at /bin so sys_exec uses "/bin/<name>")
//   /tmp   → RamFs        (writable scratch — e.g. shell `write`/`sh` scripts)
//   /mnt   → Ext2Fs        (read-only; only mounted if the ATA disk is present)
//   /proc  → ProcFs        (read-only, synthetic — /proc/meminfo)
//   /      → InitramfsFs  (fallback root, also serves plain-name exec)

pub mod devfs;
pub mod ext2;
pub mod initramfs;
pub mod procfs;
pub mod ramfs;
pub mod types;
pub mod vfs;

pub use types::{DirEntry, Errno, FileType, OpenFlags, Stat};

use alloc::sync::Arc;

/// Initialise the VFS: register all built-in filesystems.
///
/// Must be called once, after the memory allocator is ready, before any
/// process opens a file.
pub fn init() {
    // /dev — character devices from the driver registry
    vfs::mount("/dev", Arc::new(devfs::DevFs));
    // /bin — user-space ELF binaries from initramfs
    vfs::mount("/bin", Arc::new(initramfs::InitramfsFs));
    // /tmp — writable scratch space (ramfs)
    vfs::mount("/tmp", Arc::new(ramfs::RamFs::new()));
    // /mnt — real disk, read-only ext2 (best-effort: no disk / bad image just
    // means no /mnt, not a boot failure).
    match ext2::init() {
        Ok(()) => {
            vfs::mount("/mnt", Arc::new(ext2::Ext2FsHandle));
            crate::serial_println!("ext2: mounted /mnt");
        }
        Err(e) => crate::serial_println!("ext2: not mounted ({})", e),
    }
    // /proc — synthetic, read-only (meminfo today)
    vfs::mount("/proc", Arc::new(procfs::ProcFs));
    // /   — root (fallback; also exposes binaries without /bin prefix)
    vfs::mount("/", Arc::new(initramfs::InitramfsFs));
}

/// Open a file by absolute path.
///
/// Delegates to the VFS mount table.  Used by `sys_open`.
pub fn open(path: &str, flags: OpenFlags) -> Result<alloc::boxed::Box<dyn crate::process::file::FileHandle>, Errno> {
    vfs::open(path, flags)
}

/// Stat a file by absolute path.  Used by `sys_stat` / `sys_lstat`.
pub fn stat(path: &str) -> Result<Stat, Errno> {
    vfs::stat(path)
}
