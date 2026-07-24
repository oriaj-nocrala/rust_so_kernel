//! `ext2` — host-testable core of the ext2 filesystem driver.
//!
//! Extracted out of `kernel/src/fs/ext2.rs` (see
//! `docs/fs/ext2-extraction-plan.md` for the full plan and rationale) so
//! this filesystem's bitmap/inode/superblock logic can be exercised with a
//! plain `cargo test` instead of only inside QEMU — same motivation as the
//! `hal` crate (see its own doc comment), and this crate depends on `hal`
//! for the `BlockDevice`/`SECTOR_SIZE` seam rather than re-inventing one.
//!
//! `#![no_std]` except under `cfg(test)`, same idiom `hal` uses. Links
//! `alloc` (`Vec`, `Box`, `String`), available both on the bare-metal
//! kernel target (`-Z build-std`) and on the host (a normal sysroot
//! component).
//!
//! ## Scope (as of this extraction pass)
//!
//! Migration steps 1-4 from the extraction plan:
//!
//! 1. **On-disk structs + parsing** — [`Superblock`], [`BlockGroupDesc`],
//!    [`RawInode`], [`dirent`]'s pure record format. No behavior change:
//!    this is the exact byte-slicing that used to live inline in
//!    `Ext2Fs::mount()`/`read_bgd`/the `RawInode` impl in the kernel.
//! 2. **Block/inode allocation and freeing** — [`Ext2Core::alloc_block`]/
//!    [`Ext2Core::free_block`]/[`Ext2Core::alloc_inode`]/
//!    [`Ext2Core::free_inode`], plus the free-count bookkeeping in the
//!    block group descriptor and superblock ([`Ext2Core::adjust_bgd_counts`]/
//!    [`Ext2Core::adjust_sb_counts`]) their bitmap scan
//!    ([`bitmap::find_first_free_bit`]) relies on.
//! 3. **Inode-table read/write + indirect block addressing + file
//!    byte-range read/write** — [`Ext2Core::inode_location`]/
//!    [`Ext2Core::read_inode`]/[`Ext2Core::write_inode`],
//!    [`Ext2Core::block_for_index`]/[`Ext2Core::block_for_index_alloc`]
//!    (direct, singly-, doubly-, and triply-indirect pointers),
//!    [`Ext2Core::read_file_range`]/[`Ext2Core::write_file_range`], and the
//!    shared block-tree walk [`Ext2Core::visit_inode_blocks`] (used by both
//!    freeing a file's blocks and the mount-time orphan scan, still
//!    unmigrated — see below).
//! 4. **Directory operations + symlink fast/slow target read+write** —
//!    [`Ext2Core::read_dir_entries`]/[`Ext2Core::add_dir_entry`]/
//!    [`Ext2Core::remove_dir_entry`]/[`Ext2Core::set_dotdot`] (module
//!    [`dir`], speaking in the raw on-disk `file_type` byte, never the
//!    kernel's `fs::types::FileType`) and [`Ext2Core::read_symlink_target`]/
//!    [`Ext2Core::write_symlink_target`].
//!
//! Steps 5-6 (`mount`'s own repair passes `reconcile_free_counts`/
//! `reclaim_orphans`, and turning what's left of `kernel::fs::ext2` into a
//! pure VFS adapter) deliberately have **not** moved yet and still live in
//! `kernel::fs::ext2` — see the plan document for why each is its own
//! step, verified green independently.
//!
//! ## Error type and locking (design decisions carried over from the plan)
//!
//! This crate defines its own [`Ext2Error`] rather than depending on the
//! kernel's `fs::types::Errno` — moving `Errno` itself would touch half the
//! kernel for no real benefit. The kernel adapter
//! (`kernel/src/fs/ext2.rs`) implements `From<Ext2Error> for Errno`.
//!
//! [`Ext2Core`]'s coarse cross-operation locking (`EXT2_LOCK` in the
//! kernel adapter — the invariant that every mutating VFS operation runs
//! under one lock, and read-only paths like `lookup`/`readdir` never take
//! it, because they're already called *from inside* a lock-held mutating
//! method and `spin::Mutex` isn't reentrant) stays entirely in the kernel,
//! not here — see that module's doc comment. Every method below takes
//! `&self`, including the ones that write to the block device: real
//! mutation happens on the far side of `hal::block::BlockDevice`, whose
//! own `read_sectors`/`write_sectors` are `&self` methods (mirroring how
//! `hal::PortIo`'s `outb`/`outw`/`outl` are `&self` too — hardware/device
//! *mutation* by design flows through a shared reference in this seam
//! style, with an external lock providing the actual exclusion, not the
//! Rust type system). This crate does not yet attempt the harder
//! `&mut self`-for-mutations split the extraction plan's design notes
//! describe as the eventual goal ("the borrow checker enforces it") —
//! doing that soundly needs the kernel's global mount state itself
//! restructured (today: `Once<Ext2Fs>`, permanently immutable once
//! published), which is bigger surgery than steps 1-2's "no behavior
//! change" scope allows. Revisit once step 5/6 replace that global with
//! something `EXT2_LOCK` can hand out an exclusive borrow from directly.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod bgd;
pub mod bitmap;
pub mod dir;
pub mod dirent;
pub mod error;
pub mod inode;
pub mod superblock;
#[cfg(test)]
mod test_support;
pub mod volume;

pub use bgd::{bgd_location, BlockGroupDesc};
pub use dir::DirEntry;
pub use error::Ext2Error;
pub use inode::RawInode;
pub use superblock::{Superblock, EXT2_MAGIC, FEATURE_INCOMPAT_FILETYPE, ROOT_INO};
pub use volume::Ext2Core;
