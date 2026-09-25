//! The global allocator for every Rust program here (phase 2.3 of
//! `docs/gui/gui-plan.md`): until now none of them used the heap.
//!
//! Two paths, both over anonymous `mmap`:
//!
//! - **Small** (up to [`MAX_SMALL`] bytes after rounding): power-of-two size
//!   classes, each with an intrusive free list, carved out of 1 MiB chunks
//!   with a bump pointer. Chunks are never returned. Memory is demand-paged,
//!   so the tail of a chunk that is never handed out costs address space,
//!   not RAM.
//! - **Large**: one mapping per allocation, `munmap`ped on free.
//!
//! Shaped by the kernel's `mmap`, not by a general-purpose allocator's:
//!
//! - **A process has at most 64 VMAs** (`memory/vma.rs`). That is why the
//!   small path goes up to 64 KiB and chunks are 1 MiB: a program with
//!   thousands of small objects uses a handful of VMAs, and only
//!   allocations above 64 KiB — framebuffers, big `Vec`s — take one each.
//! - **`munmap` needs the exact VMA** — address *and* length. The length
//!   given back is recomputed by [`large_len`] from the `Layout`, the same
//!   rounding the `mmap` used.
//! - **A request of 2 MiB or more becomes a 2 MiB-page VMA** whose length
//!   the kernel rounds up to 2 MiB; `large_len` rounds the same way, so the
//!   `munmap` matches. Chunks stay at 1 MiB so they never do.
//! - **`mmap` only guarantees 4 KiB alignment.** Small blocks of class `c`
//!   sit at multiples of `c` inside a page-aligned chunk, so they are
//!   aligned to `min(c, 4096)`; an alignment above 4096 is refused (null,
//!   i.e. `handle_alloc_error`) rather than faked.
//!
//! One spin lock over everything. Rust programs here are single-threaded
//! today; the lock makes a future thread correct, and yields while it
//! waits, since the holder may have been preempted.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::syscall;

const MIN_CLASS_SHIFT: usize = 4; // 16 bytes: room for the free-list link
const MAX_CLASS_SHIFT: usize = 16; // 64 KiB
const NUM_CLASSES: usize = MAX_CLASS_SHIFT - MIN_CLASS_SHIFT + 1;
/// Largest request served from a size class.
pub const MAX_SMALL: usize = 1 << MAX_CLASS_SHIFT;
/// One `mmap` refills every class. Below 2 MiB so it stays 4 KiB pages.
const CHUNK: usize = 1 << 20;
const PAGE: usize = 4096;
const HUGE: usize = 2 << 20;

struct FreeNode {
    next: *mut FreeNode,
}

struct State {
    free: [*mut FreeNode; NUM_CLASSES],
    /// Bump region of the current chunk: `[cur, end)`.
    cur: usize,
    end: usize,
}

pub struct Heap {
    lock: AtomicBool,
    state: UnsafeCell<State>,
}

// Every access to `state` is under `lock`.
unsafe impl Sync for Heap {}

/// Counters for tests and `/proc`-style reporting. Updated under the lock,
/// read without it (approximate while another thread allocates).
pub struct Stats {
    pub chunks: AtomicUsize,
    pub large_live: AtomicUsize,
    pub large_bytes: AtomicUsize,
    pub small_live: AtomicUsize,
}

pub static STATS: Stats = Stats {
    chunks: AtomicUsize::new(0),
    large_live: AtomicUsize::new(0),
    large_bytes: AtomicUsize::new(0),
    small_live: AtomicUsize::new(0),
};

/// Size class index for a layout, or `None` for the large path.
fn class_of(layout: &Layout) -> Option<usize> {
    let need = layout.size().max(layout.align()).max(1 << MIN_CLASS_SHIFT);
    if need > MAX_SMALL {
        return None;
    }
    let shift = need.next_power_of_two().trailing_zeros() as usize;
    Some(shift - MIN_CLASS_SHIFT)
}

/// The exact length a large allocation is mapped (and unmapped) with.
pub fn large_len(size: usize) -> usize {
    let len = (size + PAGE - 1) & !(PAGE - 1);
    if len >= HUGE {
        (len + HUGE - 1) & !(HUGE - 1)
    } else {
        len
    }
}

impl Heap {
    pub const fn new() -> Self {
        Heap {
            lock: AtomicBool::new(false),
            state: UnsafeCell::new(State {
                free: [ptr::null_mut(); NUM_CLASSES],
                cur: 0,
                end: 0,
            }),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        while self
            .lock
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            syscall::yield_now();
        }
        let r = f(unsafe { &mut *self.state.get() });
        self.lock.store(false, Ordering::Release);
        r
    }
}

impl State {
    fn alloc_small(&mut self, class: usize) -> *mut u8 {
        let head = self.free[class];
        if !head.is_null() {
            self.free[class] = unsafe { (*head).next };
            return head as *mut u8;
        }
        let size = 1usize << (class + MIN_CLASS_SHIFT);
        let mut start = (self.cur + size - 1) & !(size - 1);
        if self.cur == 0 || start + size > self.end {
            let base = syscall::mmap_anon(0, CHUNK as u64, syscall::PROT_READ | syscall::PROT_WRITE);
            if base <= 0 {
                return ptr::null_mut();
            }
            STATS.chunks.fetch_add(1, Ordering::Relaxed);
            // What was left of the old chunk is abandoned: never touched,
            // so never faulted in.
            self.cur = base as usize;
            self.end = base as usize + CHUNK;
            start = self.cur; // page-aligned: fits any class
        }
        self.cur = start + size;
        start as *mut u8
    }

    fn free_small(&mut self, p: *mut u8, class: usize) {
        let node = p as *mut FreeNode;
        unsafe { (*node).next = self.free[class] };
        self.free[class] = node;
    }
}

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.align() > PAGE {
            return ptr::null_mut();
        }
        match class_of(&layout) {
            Some(class) => {
                let p = self.with(|s| s.alloc_small(class));
                if !p.is_null() {
                    STATS.small_live.fetch_add(1, Ordering::Relaxed);
                }
                p
            }
            None => {
                let len = large_len(layout.size());
                let r = syscall::mmap_anon(0, len as u64, syscall::PROT_READ | syscall::PROT_WRITE);
                if r <= 0 {
                    return ptr::null_mut();
                }
                STATS.large_live.fetch_add(1, Ordering::Relaxed);
                STATS.large_bytes.fetch_add(len, Ordering::Relaxed);
                r as *mut u8
            }
        }
    }

    // Anonymous mappings are zero-filled, so a fresh large block needs no
    // memset; a small one may be a recycled block.
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = self.alloc(layout);
        if !p.is_null() && class_of(&layout).is_some() {
            ptr::write_bytes(p, 0, layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        match class_of(&layout) {
            Some(class) => {
                self.with(|s| s.free_small(p, class));
                STATS.small_live.fetch_sub(1, Ordering::Relaxed);
            }
            None => {
                let len = large_len(layout.size());
                // A failure here means the layout does not match the
                // allocation: leaking beats unmapping the wrong range.
                if syscall::munmap(p as u64, len as u64) == 0 {
                    STATS.large_live.fetch_sub(1, Ordering::Relaxed);
                    STATS.large_bytes.fetch_sub(len, Ordering::Relaxed);
                }
            }
        }
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_layout = Layout::from_size_align_unchecked(new_size, layout.align());
        // Same block still fits: nothing to move.
        match (class_of(&layout), class_of(&new_layout)) {
            (Some(a), Some(b)) if a == b => return p,
            (None, None) if large_len(layout.size()) == large_len(new_size) => return p,
            _ => {}
        }
        let q = self.alloc(new_layout);
        if !q.is_null() {
            ptr::copy_nonoverlapping(p, q, layout.size().min(new_size));
            self.dealloc(p, layout);
        }
        q
    }
}

#[global_allocator]
pub static HEAP: Heap = Heap::new();
