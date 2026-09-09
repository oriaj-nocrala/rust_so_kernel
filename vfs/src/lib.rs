//! `vfs` — host-testable core of the kernel's virtual filesystem layer.
//!
//! The `kernel` crate itself cannot run `cargo test` on the host (see
//! `CLAUDE.md`'s "QEMU integration tests" section: `-Z build-std` plus a
//! double build of the `kernel` bin target collides on lang items in
//! `core`), so VFS logic that can be made to speak in plain types instead of
//! this kernel's concrete globals gets moved out here, where a plain
//! `cargo test` reaches it — the same extraction shape `hal`/`ext2`/`mm`
//! already went through.
//!
//! Right now this crate holds [`types`] (`Errno`, `FileType`, `OpenFlags`,
//! `Stat`, `DirEntry` — the plain data types every VFS trait and adapter
//! speaks in), [`file`] (`FileError`, `FileResult`, `compute_seek`, and
//! the `FileHandle` trait — the coupling point between processes, drivers,
//! and filesystems), [`inode`] (`Inode`, `Filesystem` — the traits every
//! concrete filesystem implements), [`dirent`] (the shared
//! `getdents64_via_readdir`/`getdents64_from_snapshot` packing helpers),
//! [`path`] (`normalize_path`, the `..`/`.`-collapsing helper; and
//! `split_parent`, crate-private), [`mount`] (`MountTable` — longest-
//! prefix-match path resolution, symlink following with an `ELOOP` guard,
//! and the mutating VFS ops) and [`ramfs`] (`RamFs` — the one writable
//! in-memory filesystem, `/tmp`'s implementation, behind a
//! `DirLockObserver` seam so its directory-lock diagnostic can be injected
//! by the kernel without this crate calling into the scheduler or
//! `kernel::debug` directly). See `docs/fs/vfs-extraction-plan.md` for the
//! full six-step migration this crate came out of.
#![no_std]

extern crate alloc;

pub mod dirent;
pub mod file;
pub mod inode;
pub mod mount;
pub mod path;
pub mod ramfs;
pub mod types;
