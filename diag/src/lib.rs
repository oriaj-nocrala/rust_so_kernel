//! `diag` — host-testable always-on diagnostic instruments.
//!
//! Extracted out of `kernel/src/debug.rs` (see `CLAUDE.md`'s "Runtime
//! Tracing & Counters" section for the pre-extraction story) following the
//! exact precedent set by the `hal`/`ext2`/`mm`/`vfs` extractions: logic that
//! can be made to speak in plain types instead of this kernel's concrete
//! globals moves out here, where a plain `cargo test` reaches it, instead of
//! staying trapped inside `kernel` (which cannot run `cargo test` on the
//! host at all — see the "QEMU integration tests" section of `CLAUDE.md` for
//! why: `-Z build-std` plus a double build of the `kernel` bin target
//! collides lang items in `core`).
//!
//! This crate holds the four **always-on diagnostic structs** that render
//! into `/proc/kdebug` and the panic-time snapshot — `LockDiag`,
//! `DirLockDiag`, `TfRewindDiag`, `IfViolationDiag`. Each grew out of a real,
//! hours-to-days-long bug hunt (see their individual doc comments, moved
//! here verbatim); each is "almost pure": plain atomics plus an `unsafe`
//! pointer-and-length reconstruction of a `&'static str` captured earlier
//! from `core::panic::Location`/a `&'static str` argument, with range guards
//! and string fallbacks nobody had ever exercised with a test before this
//! extraction. That gap — real logic, including `unsafe`, with zero tests —
//! is the entire reason this crate exists: `diag`'s own test suite is the
//! actual deliverable, not just a side effect of moving files around.
//!
//! `#![cfg_attr(not(test), no_std)]`, same idiom `hal`/`ext2`/`mm`/`vfs`
//! already use — under `cfg(test)` this becomes a full `std` crate (host
//! unit tests link against `std`'s allocator for their own scaffolding via
//! `extern crate alloc`), but the shipped, non-test build stays `no_std`.
//! Unlike `mm` (which links no `alloc` at all, since it *is* the kernel's
//! global allocator and any internal allocation there would recurse), this
//! crate links `alloc` — same as `hal`/`ext2` — because `render()` builds a
//! `alloc::string::String` via `format!`, exactly like the pre-extraction
//! code did.
//!
//! ## The one seam this extraction needed
//!
//! Three methods in the pre-extraction code called straight into
//! `crate::serial_println_raw!`, which this crate cannot name. Resolved with
//! the same discipline the `mm` extraction established (see that crate's
//! doc comment: "recoverable conditions come back as data that the kernel
//! adapter prints"):
//!
//! - **[`tfrewind::TfRewindDiag::record`]** used to print immediately and
//!   unconditionally on every detected rewind. Here it only records and
//!   returns a [`tfrewind::RewindEvent`] describing what happened —
//!   `kernel/src/debug.rs::tf_record` is the thin adapter that calls
//!   `record`, then reproduces the *exact* pre-extraction
//!   `serial_println_raw!` line from the returned event.
//! - **`DirLockDiag::print_panic_line`** and **`TfRewindDiag::print_panic_line`**
//!   (allocation-free variants for the panic handler, which must not
//!   allocate) are gone entirely — replaced by plain, allocation-free
//!   accessors (`acquires()`, `releases()`, `outstanding()`, `last_pid()`,
//!   `last_op()`/`last_site()`/`last_old_seq()`/`last_new_seq()`, `count()`,
//!   `last_file()`, `last_line()`) that `kernel/src/debug.rs::
//!   print_panic_snapshot` calls directly, formatting and printing itself
//!   with `serial_println_raw!` exactly as before. `LockDiag` and
//!   `IfViolationDiag` never had a `print_panic_line` of their own —
//!   `print_panic_snapshot` already read their fields directly — so they
//!   gained the same style of accessor instead of exposing their private
//!   fields.
//!
//! No output changes anywhere: every `render()`/panic-snapshot line this
//! crate's callers produce is byte-for-byte identical to what
//! `kernel/src/debug.rs` produced before this extraction.
//!
//! ## What did NOT move
//!
//! The `static` instances of these four types (`SCHEDULER_LOCK`,
//! `RAMFS_ENTRIES_LOCK`, `TF_REWIND`, the four `COW_IF_VIOLATIONS_*`), the
//! `Subsystem`/`TRACE_MASK`/`ktrace!` tracing machinery, every permanent
//! lifecycle counter (`forks_total`, `switches_total`, ...), `render_report`,
//! `print_panic_snapshot`, and `subsystem_bit_by_name` all stay in
//! `kernel/src/debug.rs` — exactly like `EXT2: Once<Ext2Fs>` stayed in the
//! kernel's `fs::ext2` adapter after that extraction, and `BUDDY`/
//! `SLAB_ALLOCATOR` stayed in `kernel/src/allocator/mod.rs` after `mm`'s.
//! This crate only defines the diagnostic *types*; it owns no global state.
#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod dirlock;
pub mod fbstat;
pub mod ifviolation;
pub mod irqmutex;
pub mod lock;
pub mod tfrewind;
pub mod tracked;

pub use dirlock::DirLockDiag;
pub use fbstat::OpStat;
pub use ifviolation::IfViolationDiag;
pub use irqmutex::{IrqControl, IrqMutex};
pub use lock::LockDiag;
pub use tracked::{LockObserver, TrackedGuard, TrackedMutex};
pub use tfrewind::{RewindEvent, TfRewindDiag};
