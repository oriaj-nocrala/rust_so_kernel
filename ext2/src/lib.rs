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
//! ## Scope — extraction complete (all 6 plan steps)
//!
//! Every step in `docs/fs/ext2-extraction-plan.md` has landed. What used to
//! be ~2,200 lines of bitmap/inode/directory logic inline in
//! `kernel/src/fs/ext2.rs`, only ever exercisable by booting QEMU, now lives
//! here as `no_std` + `alloc` code with 89 host-run tests (`cd ext2 && cargo
//! test`, no QEMU, no hardware):
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
//!    ([`bitmap::find_first_free_bit`]) relies on. (The kernel adapter keeps
//!    its own, older, actively-used copy of this exact allocation logic
//!    rather than calling these — see `kernel/src/fs/ext2.rs`'s "ext2 core
//!    crate" module doc comment for why that duplication was left alone in
//!    the step 6 cleanup rather than folded together.)
//! 3. **Inode-table read/write + indirect block addressing + file
//!    byte-range read/write** — [`Ext2Core::inode_location`]/
//!    [`Ext2Core::read_inode`]/[`Ext2Core::write_inode`],
//!    [`Ext2Core::block_for_index`]/[`Ext2Core::block_for_index_alloc`]
//!    (direct, singly-, doubly-, and triply-indirect pointers),
//!    [`Ext2Core::read_file_range`]/[`Ext2Core::write_file_range`], and the
//!    shared block-tree walk [`Ext2Core::visit_inode_blocks`] (used by both
//!    freeing a file's blocks and the mount-time orphan scan).
//! 4. **Directory operations + symlink fast/slow target read+write** —
//!    [`Ext2Core::read_dir_entries`]/[`Ext2Core::add_dir_entry`]/
//!    [`Ext2Core::remove_dir_entry`]/[`Ext2Core::set_dotdot`] (module
//!    [`dir`], speaking in the raw on-disk `file_type` byte, never the
//!    kernel's `fs::types::FileType`) and [`Ext2Core::read_symlink_target`]/
//!    [`Ext2Core::write_symlink_target`].
//! 5. **Mount-time consistency repair** — [`Ext2Core::reconcile_free_counts`]/
//!    [`Ext2Core::reclaim_orphans`] (module [`repair`], plus the private
//!    recursive `mark_reachable` step and the test-inspection accessors
//!    `inode_used`/`block_used`/`inode_mode`/`sb_free_counts`/
//!    `bgd_free_counts`/`true_free_counts_group0`). Same walk order, same
//!    on-disk bytes — see `repair`'s module doc comment for the ordering
//!    invariant that must never change, and for how these two methods
//!    report what they found/fixed through their return values instead of
//!    tracing directly (this crate can't call the kernel's `ktrace!`). A
//!    real `e2fsck -fn` (built at test time via `mke2fs`, see `repair`'s
//!    own test module) confirms `reconcile_free_counts`'s repair is
//!    genuinely clean; the same oracle also surfaced a real, pre-existing
//!    limitation of `reclaim_orphans` deliberately *not* fixed here (out of
//!    scope for a no-behavior-change extraction): it only ever clears the
//!    orphan's bitmap bit and never zeroes/stamps `i_dtime` on the
//!    reclaimed inode's own record the way `unlink`/`rmdir` do for a normal
//!    deletion, so a real `e2fsck -fn` afterward still finds the
//!    well-formed-looking leftover record and reports a disconnected
//!    directory needing reconnection to `lost+found` (exit code 4) — see
//!    the "e2fsck oracle" comment block in `repair.rs`'s test module for
//!    the full story of how this was found and why fixing it belongs to a
//!    separate change, not this migration.
//! 6. **The kernel adapter is now a pure VFS shim.** `kernel/src/fs/ext2.rs`
//!    implements `Filesystem`/`Inode`/`FileHandle` over [`Ext2Core`] and
//!    nothing else — every wrapper method left there either wraps a core
//!    method 1:1 (converting `Ext2Error`↔`Errno` and the core's raw
//!    `file_type: u8`↔the kernel's `fs::types::FileType` at the boundary)
//!    or is genuinely kernel-only state (`EXT2: Once<Ext2Fs>`,
//!    `EXT2_LOCK`). The hand-built test disk images
//!    ([`testimg::build_minimal_image`]/[`testimg::build_image_with_orphans`])
//!    that used to be duplicated — once in the kernel (`#[cfg(test)]`,
//!    consumed by `kernel/src/hw_tests.rs`'s QEMU integration tests) and
//!    once in this crate's own `test_support` (consumed by `repair`'s host
//!    tests) — now live in [`testimg`] as the single source both sides
//!    import.
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
//! restructured (today, still: `Once<Ext2Fs>`, permanently immutable once
//! published, unchanged by any of the 6 extraction steps above), which is
//! bigger surgery than a "no behavior change" scope allows. A real
//! follow-up, not something this extraction attempted — revisit once
//! something can hand `EXT2_LOCK` out as an exclusive borrow directly.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod bgd;
pub mod bitmap;
pub mod dir;
pub mod dirent;
pub mod error;
pub mod inode;
pub mod repair;
pub mod superblock;
#[cfg(test)]
mod test_support;
pub mod testimg;
pub mod volume;

pub use bgd::{bgd_location, BlockGroupDesc};
pub use dir::DirEntry;
pub use error::Ext2Error;
pub use inode::RawInode;
pub use repair::ReconcileReport;
pub use superblock::{Superblock, EXT2_MAGIC, FEATURE_INCOMPAT_FILETYPE, ROOT_INO};
pub use volume::Ext2Core;
