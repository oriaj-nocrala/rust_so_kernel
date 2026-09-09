// kernel/src/fs/ramfs.rs
//
// The writable in-memory filesystem (`/tmp`) itself now lives in the
// host-testable `vfs` crate (`vfs::ramfs::RamFs`) — see
// `docs/fs/vfs-extraction-plan.md`, step 5. All that's left here is the
// `DirLockObserver` that connects `RamFs`'s directory-entries lock to this
// kernel's `/proc/kdebug` diagnostic (`crate::debug::RAMFS_ENTRIES_LOCK`):
// `vfs` can't call `crate::process::scheduler` or `crate::debug` itself,
// so it takes an injected observer instead — same seam shape as
// `hal::PortIo`/`mm::PhysMap`.

pub use vfs::ramfs::RamFs;

struct KernelDirLockObserver;

impl vfs::ramfs::DirLockObserver for KernelDirLockObserver {
    fn current_pid(&self) -> u64 {
        crate::process::scheduler::current_pid_safe().map(|p| p as u64).unwrap_or(u64::MAX)
    }

    fn record_acquire(&self, pid: u64, op: &'static str) {
        crate::debug::RAMFS_ENTRIES_LOCK.record_acquire(pid, op)
    }

    fn record_release(&self) {
        crate::debug::RAMFS_ENTRIES_LOCK.record_release()
    }
}

static KERNEL_DIR_LOCK_OBSERVER: KernelDirLockObserver = KernelDirLockObserver;

/// Construct the `RamFs` mounted at `/tmp`, with the kernel's lock
/// diagnostic wired in. Deliberately not just `RamFs::new()` — that
/// constructor uses `vfs::ramfs::NoopDirLockObserver`, which would build
/// and mount just fine but silently drop the `ramfs_entries_lock` line
/// `/proc/kdebug` reports (see `kernel/src/fs/mod.rs::init`, which must
/// call this function, not `RamFs::new()`, for that reason).
pub fn new() -> RamFs {
    RamFs::with_observer(&KERNEL_DIR_LOCK_OBSERVER)
}
