//! `vfs` — host-testable core of the kernel's virtual filesystem layer.
//!
//! Extracted out of `kernel/src/fs/vfs.rs` and `kernel/src/fs/ramfs.rs` (see
//! `docs/fs/vfs-extraction-plan.md` for the full six-step migration this
//! crate came out of) for the same reason `hal`, `ext2`, and `mm` exist:
//! `kernel` itself cannot run `cargo test` on the host (see `CLAUDE.md`'s
//! "QEMU integration tests" section — `-Z build-std` plus a double build of
//! the `kernel` bin target collides on lang items in `core`), so logic that
//! can be made to speak in plain types instead of this kernel's concrete
//! globals gets moved out here, where a plain `cargo test` reaches it. The
//! difference from the `ext2` precedent: there, only the byte-level core
//! moved and the VFS-facing traits stayed in the kernel; here *the traits
//! themselves* (`Inode`, `Filesystem`, `FileHandle`) are what needed testing
//! most (path resolution, symlink following, mount-table lookup), so they
//! move too — same reasoning that took `hal::PortIo`/`PhysMem` out as bare
//! seam traits rather than leaving them kernel-side.
//!
//! ## What lives here
//!
//! - [`types`] — `Errno`, `FileType`, `OpenFlags`, `Stat`, `DirEntry`: the
//!   plain data types every VFS trait and adapter speaks in. Depends on
//!   nothing else in this crate.
//! - [`file`] — `FileError`, `FileResult`, `compute_seek`, and the
//!   `FileHandle` trait: the coupling point between processes, drivers, and
//!   filesystems (an open file description, not a directory entry).
//! - [`inode`] — the `Inode` and `Filesystem` traits every concrete
//!   filesystem (ramfs here; devfs/initramfs/procfs/ext2 in the kernel)
//!   implements.
//! - [`dirent`] — `getdents64_via_readdir`/`getdents64_from_snapshot`, the
//!   packing helpers shared by every `Inode::open()` directory handle that
//!   needs to serve `getdents64` in `linux_dirent64` wire format.
//! - [`path`] — `normalize_path` (the `.`/`..`-collapsing helper used before
//!   every resolution) and `split_parent` (crate-private).
//! - [`mount`] — `MountTable`: longest-prefix-match path resolution across
//!   every mounted filesystem, symlink following with an `ELOOP` guard, and
//!   the mutating VFS ops (`mkdir`/`symlink`/`unlink`/`rmdir`/`rename`, with
//!   rollback on a failed rename). A plain struct with methods, not a
//!   global — see "What stays in the kernel" below.
//! - [`ramfs`] — `RamFs`, the one writable in-memory filesystem (`/tmp`'s
//!   implementation), behind a [`ramfs::DirLockObserver`] seam so its
//!   directory-lock diagnostic can be injected by the kernel without this
//!   crate calling into the scheduler or `kernel::debug` directly.
//!
//! ## What stays in the kernel, and why
//!
//! - **The mount table's global state.** `MountTable` here is a plain
//!   struct; `static MOUNTS: MountTable` lives in `kernel/src/fs/vfs.rs`,
//!   which re-exposes every operation as a free function with an unchanged
//!   signature — the exact same shape `static EXT2: Once<Ext2Fs>` took after
//!   the `ext2` extraction. No call site elsewhere in the kernel
//!   (`fs/devfs.rs`, `fs/initramfs.rs`, `fs/procfs.rs`, `fs/ext2.rs`,
//!   `process/syscall/*.rs`) needed to change.
//! - **`FileDescriptorTable`.** `kernel/src/process/file.rs` re-exports
//!   [`file::FileHandle`]/[`file::FileError`]/[`file::compute_seek`] from
//!   here, but keeps the fd table itself — it calls `crate::drivers` (to
//!   construct device handles) and `serial_println!`, neither of which this
//!   crate can name.
//! - **Every filesystem except ramfs.** `devfs` needs the kernel's device
//!   driver registry, `initramfs` needs the embedded ELF blobs baked into
//!   the kernel binary, `procfs` needs live scheduler state
//!   (`scheduler::all_pids()`, per-pid `Process` snapshots), and `ext2`'s
//!   VFS adapter (`kernel/src/fs/ext2.rs`) wraps the standalone `ext2`
//!   crate's `Ext2Core` plus its own global `EXT2: Once<Ext2Fs>` +
//!   `EXT2_LOCK`. None of these can be made to speak in plain types the way
//!   ramfs could — see `docs/fs/vfs-extraction-plan.md`'s "Fuera de
//!   alcance".
//!
//! ## Two seams/invariants a future reader has to know
//!
//! 1. **[`mount::MountTable::find`] locks and releases the mount-entries
//!    list *inside itself*; nothing else in `mount.rs` holds that guard
//!    while calling into `Inode`/`Filesystem`.** `fs::initramfs`'s root
//!    directory calls back into `direct_children("/")` — which re-locks the
//!    same table — from *inside* `resolve_inner`'s own walk
//!    (`RootDirInode::lookup`/`::readdir`). `spin::Mutex` isn't reentrant,
//!    so holding the lock across that call would self-deadlock the first
//!    `ls /` a real boot ever does. `mount::tests::
//!    direct_children_callback_from_lookup_does_not_self_deadlock` hangs if
//!    this is ever broken — see `MountTable::find`'s own doc comment and
//!    `docs/fs/vfs-extraction-plan.md`'s decision #3.
//! 2. **[`ramfs::DirLockObserver`]** replaces `RamDirNode::lock_entries`'s
//!    direct calls into the kernel scheduler and `kernel::debug` — same seam
//!    shape as `hal::PortIo`/`mm::PhysMap`. Its call order is a real,
//!    paid-for invariant: `current_pid()` fires *before* the real
//!    `entries.lock()` (the kernel's implementation takes the `SCHEDULER`
//!    lock and ends with an unconditional `sti`, neither of which may
//!    happen inside the directory-lock critical section), and
//!    `record_acquire()` fires *after* the lock is actually held (recording
//!    it earlier would misattribute a still-spinning caller as the lock
//!    holder — one of the defective diagnostics from the 2026-08-05 hang
//!    hunt, see `docs/hang-hunt-bug2-findings.md`). `ramfs::tests::
//!    lock_entries_calls_observer_in_the_exact_required_order` pins down the
//!    *first* half of that (call order between the three observer methods)
//!    but not the second (observer-call-vs-real-lock timing) — see that
//!    test's own comment for exactly what was verified and what wasn't.
//!
//! ## `Errno` moving here forced a newtype in `kernel/src/fs/ext2.rs`
//!
//! Once [`types::Errno`] lived in this crate instead of the kernel, the
//! kernel's `impl From<ext2::Ext2Error> for Errno` became an impl of a
//! foreign trait for a foreign type (`E0117` — the orphan rule: neither
//! `Ext2Error` nor `Errno` is local to the kernel crate anymore). Fixed with
//! a local newtype, `struct ExtErr(ext2::Ext2Error)`, and `.map_err(ExtErr)?`
//! at call sites in `kernel/src/fs/ext2.rs`. Giving the `ext2` crate a
//! dependency on `vfs` instead (so it could implement `From<Ext2Error> for
//! Errno` itself) was deliberately rejected: it would contradict the `ext2`
//! extraction's own decision #1 ("the core doesn't know `Errno`",
//! `docs/fs/ext2-extraction-plan.md`) and invert the dependency direction
//! between the two crates for no real benefit — the newtype is a few lines
//! in the adapter that already exists to do exactly this kind of
//! type-boundary conversion.
#![no_std]

extern crate alloc;

// Los tests de host de este crate necesitan hilos reales (`std::thread`) para
// hacer observable la contención sobre un lock — sin un segundo hilo
// compitiendo de verdad, el orden entre `record_acquire` y la adquisición
// real del `spin::Mutex` no es observable desde el observador. El harness de
// `cargo test` enlaza std en el host aunque el crate sea `#![no_std]`.
#[cfg(test)] extern crate std;

pub mod dirent;
pub mod file;
pub mod inode;
pub mod mount;
pub mod path;
pub mod ramfs;
pub mod types;
