// kernel/src/gpu/intr.rs
//
// Phase 6d of docs/gpu/gpu-plan.md: interrupt sources of the GPU besides the display, found by
// looking at which VFN leaf bit a piece of work raises (`nvgpu::vblank::new_vectors`), and the
// counters the MSI handler (`vblank.rs`) keeps for them.
//
// Two sources are watched: the copy engine's non-stall interrupt (a copy launched with
// INTERRUPT_TYPE NON_BLOCKING) and GSP-RM's (a reply queued in the status queue). A vector is
// only "known" when exactly one new bit appeared; the handler acknowledges the known ones and
// the tree's `allow` gets them at `vblank::setup`.

use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use nvgpu::vblank as vb;

use super::Bar0;

/// No vector known yet (past the tree, so `nvgpu::vblank` ignores it).
const UNKNOWN: u32 = 0xffff;

/// One interrupt source.
pub struct Source {
    name: &'static str,
    vector: AtomicU32,
    count: AtomicU64,
    /// TSC of the last interrupt (the handler stamps it).
    last_tsc: AtomicU64,
}

impl Source {
    const fn new(name: &'static str) -> Self {
        Source { name, vector: AtomicU32::new(UNKNOWN), count: AtomicU64::new(0), last_tsc: AtomicU64::new(0) }
    }

    pub fn vector(&self) -> Option<u32> {
        Some(self.vector.load(Ordering::Relaxed)).filter(|&v| v != UNKNOWN)
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Acquire)
    }

    pub fn last_tsc(&self) -> u64 {
        self.last_tsc.load(Ordering::Acquire)
    }

    /// The handler saw this source pending (ISR context).
    pub fn hit(&self) {
        self.last_tsc.store(crate::cpu::tsc::read(), Ordering::Release);
        self.count.fetch_add(1, Ordering::Release);
    }
}

pub static CE: Source = Source::new("ce");
pub static GSP: Source = Source::new("gsp");

/// The vectors the handler acknowledges, in the order `Serviced::extra`'s bits use: [CE, GSP].
pub fn extras() -> [u32; 2] {
    [CE.vector.load(Ordering::Relaxed), GSP.vector.load(Ordering::Relaxed)]
}

/// The handler's part: acknowledge what `service_with` reported.
pub fn on_serviced(extra: u32) {
    if extra & 1 != 0 {
        CE.hit();
    }
    if extra & 2 != 0 {
        GSP.hit();
    }
}

/// Run `action` with the leaf status cleared before it and read after: the vectors it raised.
/// Reports on `r`; the source's vector is set when exactly one appeared. The tree's own gates
/// (unarm/allow/block) are not touched: the status latches whether or not a source is allowed.
pub fn discover(r: &mut String, regs: &Bar0, src: &Source, action: &mut dyn FnMut()) -> Option<u32> {
    vb::clear_all_leaves(regs);
    let before = vb::leaf_stats(regs);
    action();
    let after = vb::leaf_stats(regs);
    let new = vb::new_vectors(&before, &after);
    let _ = writeln!(r, "intr: {}: leaf status before {:x?}, after {:x?}: new vectors {:?}", src.name, before, after, new);
    vb::clear_all_leaves(regs);
    if let [v] = new[..] {
        src.vector.store(v, Ordering::Relaxed);
        let (leaf, bit) = vb::vector_leaf(v);
        let _ = writeln!(r, "intr: {}: vector {} (leaf {} bit {:#x})", src.name, v, leaf, bit);
        Some(v)
    } else {
        let _ = writeln!(r, "intr: {}: no single vector", src.name);
        None
    }
}

/// `/proc/kdebug` line.
pub fn render_kdebug() -> String {
    let f = |s: &Source| match s.vector() {
        Some(v) => alloc::format!("{}", v),
        None => String::from("none"),
    };
    alloc::format!("gpu_intr: ce_vector={} ce_count={} gsp_vector={} gsp_count={}", f(&CE), CE.count(), f(&GSP), GSP.count())
}
