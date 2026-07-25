//! `mm` — host-testable physical (buddy) and heap (slab) allocators.
//!
//! Extracted out of `kernel/src/allocator/{buddy_allocator,slab}.rs` (see
//! `CLAUDE.md`'s "Memory Subsystem" section for the pre-extraction story)
//! following the exact precedent set by the `ext2` crate extraction
//! (`docs/fs/ext2-extraction-plan.md`, and see `ext2::repair`'s module doc
//! comment in particular): pure allocator logic moves here so it can be
//! exercised with a plain `cargo test` instead of only inside QEMU; the
//! kernel keeps only what genuinely has to live there.
//!
//! `#![cfg_attr(not(test), no_std)]`, same idiom `hal`/`ext2` already use —
//! under `cfg(test)` this becomes a full `std` crate (host unit tests get a
//! real allocator for their own scaffolding), but the shipped, non-test
//! build is `no_std` with **no `alloc` dependency at all** (unlike `hal`/
//! `ext2`, which both link `alloc`). That's not a style choice: the slab
//! allocator in here *is* the kernel's `#[global_allocator]`. If its own
//! internal bookkeeping ever allocated, that would recurse into itself the
//! first time anything in the kernel touched the heap — boot would never
//! get past its first `Vec`/`Box`/`String`. Both allocators are built
//! entirely out of fixed-size arrays and intrusive (in-place) linked lists
//! for exactly this reason, same as the pre-extraction code.
//!
//! ## Two seams replace direct kernel calls
//!
//! The pre-extraction code called straight into `crate::memory::
//! physical_memory_offset()` (to turn a physical address into something it
//! could dereference) and `crate::serial_println_raw!` (debug/error
//! logging, including a `loop { hlt }` on detecting a double-free). Neither
//! can survive the move as-is — this crate cannot name anything in
//! `kernel::`, and logging in a `no_std`+no-`alloc` crate can format lazily
//! but has nowhere to *send* the result:
//!
//! - **[`PhysMap`]** replaces the physical-memory-offset call — same shape
//!   as `hal::PhysMem` (see that crate's doc comment), just returning a raw
//!   pointer instead of copying bytes, since both allocators need to
//!   dereference in place (write link pointers, walk free lists), not just
//!   read. The kernel's implementation (`KernelPhysMap` in
//!   `kernel/src/allocator/mod.rs`) wraps `physical_memory_offset()`; host
//!   tests back it with a small on-stack/static buffer.
//! - **Logging moves to the kernel adapter, which now reports by value
//!   instead of printing directly** — the same discipline
//!   `ext2::repair::reconcile_free_counts`/`reclaim_orphans` established
//!   (see that module's doc comment): recoverable conditions come back as
//!   data ([`buddy::PhantomEvent`], [`slab::AllocEvent`]/
//!   [`slab::DeallocEvent`]) that `kernel/src/allocator/mod.rs` matches on
//!   and prints with `serial_println_raw!`, using the *exact* pre-
//!   extraction format strings (verified against the original file, see
//!   that module's comments) — no output changes, only where the
//!   `serial_println_raw!` call physically lives. The one genuinely
//!   *unrecoverable* condition (a double-free caught by the bitmap) used to
//!   print then spin forever (`loop { hlt }`) specifically because
//!   panicking while the caller's `BUDDY` lock is held looked unsafe from
//!   inside the pre-extraction file; `kernel/src/panic.rs`'s handler is
//!   verified to touch neither `BUDDY` nor the heap (it only does
//!   lock-free serial writes and a `try_lock` on the framebuffer), so a
//!   plain `panic!` here is safe and is what this crate does now — the
//!   same call `ext2` makes for its own hard errors (`Ext2Error`), per this
//!   extraction's explicit instructions.
//!
//! `slab`'s large-object path additionally needs **[`FrameSource`]** — the
//! two-function `phys_alloc`/`phys_free` facade
//! (`kernel/src/allocator/mod.rs`) that has always been the sole boundary
//! between the slab and buddy allocators stays exactly that: a boundary,
//! not a direct call from `slab.rs` into `buddy.rs` now that both live in
//! the same crate. `KernelFrameSource` (kernel side) forwards straight
//! through to `phys_alloc`/`phys_free`, which still wrap the one global
//! `BUDDY`.
//!
//! ## What did NOT move
//!
//! `static BUDDY: Mutex<buddy::BuddyAllocator>`, `static SLAB_ALLOCATOR:
//! Mutex<slab::SlabAllocator>`, the `#[global_allocator]` registration, and
//! the `phys_alloc`/`phys_free` facade all stay in
//! `kernel/src/allocator/mod.rs` — exactly like `EXT2: Once<Ext2Fs>` stayed
//! in the kernel's `fs::ext2` adapter after that extraction. This crate
//! only defines the allocator *types*; it owns no global state and drives
//! no hardware.
//!
//! ## Address type
//!
//! Addresses are `x86_64::PhysAddr`/`x86_64::VirtAddr`, not raw `u64`.
//! Unlike `hal`/`ext2` (which depend on nothing beyond `spin`/`hal`), this
//! crate pulls in the `x86_64` crate — pinned to the exact version
//! (`=0.15.4`) the kernel's `Cargo.lock` already resolves to, because
//! `0.15.5` fails to build against this repo's pinned nightly (`Step`
//! trait shape mismatch in `x86_64`'s `paging::Page`/`PageTable` `Step`
//! impls — verified directly, not assumed). `x86_64::PhysAddr` itself has
//! no bare-metal-only code path (no inline asm, no port I/O), so it builds
//! and runs identically on the host; keeping it (instead of downgrading
//! every address in this crate to a bare `u64`, which `hal`/`ext2` use
//! instead) was chosen because nearly every call site moving into this
//! crate already had a `PhysAddr` in hand (`frame.start_address()`,
//! `phys_base`, ...) — converting those to/from `u64` at every boundary
//! would have meant touching far more of `kernel/src/memory/
//! page_table_manager.rs` and `kernel/src/init/processes.rs` than this
//! mechanical move should.
#![cfg_attr(not(test), no_std)]

pub mod buddy;
pub mod slab;

/// Physical-memory mapping seam — turns a physical address into a raw
/// pointer this crate can dereference, without knowing *how* physical
/// memory is mapped. Same idea as `hal::PhysMem`, but returning a pointer
/// (for in-place linked-list writes) rather than copying bytes into a
/// caller buffer. The kernel's implementation
/// (`kernel/src/allocator/mod.rs::KernelPhysMap`) wraps
/// `physical_memory_offset()`; host tests back it with a small buffer.
pub trait PhysMap {
    fn virt_for(&self, pa: x86_64::PhysAddr) -> *mut u8;
}

/// Physical-frame source seam — the boundary `slab`'s large-object path and
/// per-cache page expansion use to reach the buddy allocator, instead of
/// `slab.rs` calling `buddy.rs` directly now that both live in this crate.
/// Preserves the pre-extraction `phys_alloc`/`phys_free` facade
/// (`kernel/src/allocator/mod.rs`) as the one real slab↔buddy boundary —
/// see this module's doc comment.
pub trait FrameSource {
    /// Allocate 2^order bytes of physical memory. `None` on OOM.
    ///
    /// # Safety
    /// Same contract as the underlying buddy allocator: `order` must be in
    /// its supported range.
    unsafe fn alloc_order(&self, order: usize) -> Option<x86_64::PhysAddr>;

    /// Free a block previously returned by `alloc_order` with the same
    /// order.
    ///
    /// # Safety
    /// `addr`/`order` must match a prior `alloc_order` call exactly, and
    /// the block must not already be freed.
    unsafe fn free_order(&self, addr: x86_64::PhysAddr, order: usize);
}
