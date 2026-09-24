// kernel/src/memory/tlb.rs
//
// The one place a stale translation is dropped after a page-table change.
// Stage 0 of `docs/smp/smp-plan.md`.
//
// Today every function here is a plain local invalidation (`invlpg` or a
// CR3 reload) — this kernel runs on one CPU, so that is complete. With
// several CPUs it is not: another CPU running the same address space (a
// `clone` thread) or any CPU at all (a kernel mapping) keeps the old entry
// until it is told to drop it, and nothing fails visibly — it reads and
// writes a frame that now belongs to someone else. Routing every
// invalidation through here means the TLB shootdown (stage 5) changes the
// bodies below, not forty call sites scattered through `memory`.
//
// **Rule:** after changing a PTE, invalidate through this module. Never
// `x86_64::instructions::tlb::*` or `MapperFlush::flush()` directly —
// consume a `MapperFlush` with `.ignore()` and call `invalidate_page`
// (`MapperFlush` does not expose its page, so the caller passes it).
// `Cr3::write` is a switch of address space, not an invalidation, and
// stays where it is.
//
// The split that matters for stage 5 is *who else can hold the entry*:
//
//   * `invalidate_page` — a user mapping in some address space. Only CPUs
//     running that address space can cache it.
//   * `invalidate_kernel_page` — a kernel mapping (physmap, MMIO, the
//     framebuffer). Shared by every address space and possibly GLOBAL, so
//     every CPU can cache it.
//   * `invalidate_all_this_cpu` — not a PTE change at all; a step of a
//     per-CPU register procedure (the SDM's PAT reprogramming sequence)
//     that each CPU performs for itself. Never a substitute for the above.
//
// No range variants yet: every current caller changes one page at a time.
// Add them when a caller batches — with a shootdown, one IPI per range
// instead of one per page is exactly what a range variant is for.

use x86_64::VirtAddr;
use x86_64::instructions::tlb;

/// A user mapping at `addr` changed (unmapped, remapped, permissions
/// changed). A 2 MiB/1 GiB page is dropped whole by any address inside it.
#[inline]
pub fn invalidate_page(addr: VirtAddr) {
    tlb::flush(addr);
}

/// A kernel mapping at `addr` changed. `invlpg` drops the entry even when
/// it is GLOBAL, so this is complete on one CPU.
#[inline]
pub fn invalidate_kernel_page(addr: VirtAddr) {
    tlb::flush(addr);
}

/// Reload CR3 on this CPU only, for a per-CPU procedure that requires it
/// (the PAT change sequence in `memory::memtype::program_pat`). Each CPU
/// runs such a procedure itself, so there is nothing to shoot down — and
/// that is the only legitimate use.
#[inline]
pub fn invalidate_all_this_cpu() {
    tlb::flush_all();
}
