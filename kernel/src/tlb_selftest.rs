// kernel/src/tlb_selftest.rs
//
// Stage 5 of `docs/smp/smp-plan.md`, "done when": a CPU that reads a page in
// a loop never sees the old translation after another CPU changes it. The
// APs run no processes yet, so the reader is sent to an inert AP through
// its mailbox (`smp::run_on`) and the BSP plays the writer.
//
// One implementation, two callers: the QEMU integration test
// (`hw_tests::tlb_shootdown_*`) and `kdebug tlbtest` (syscall 403, cmd 3),
// which is how it runs on the Ryzen's 24 CPUs rather than QEMU's model.
//
// ── What each part proves ───────────────────────────────────────────────
//
// Three frames, frame i holding `value(i)`. Round r maps the page to frame
// r % 3. The reader loads the round number, then the page: after round r
// is published the only legal values are frame r's and — if the writer has
// already remapped for r+1 — frame r+1's. Frame r-1's (= r+2's) is the old
// translation: counted as stale. The writer starts round r+1 only after
// the reader has read round r a few times, so its TLB really holds frame r
// when the change comes; without a shootdown it would keep reading it.
//
//   * user scope — a scratch address space loaded on the AP, its leaf
//     repointed with `OwnedPageTable::replace_frame` (one store: the reader
//     must never find it not-present, which `unmap_and_remap` would let
//     it). Only CPUs with that table loaded are told.
//   * kernel scope — a 4 KiB kernel mapping (`mmio::map`, present in every
//     address space) whose leaf is rewritten and dropped with
//     `invalidate_kernel_page`, the path guard pages and MMIO take.
//   * mutual — the BSP and an AP shoot each other down, both with IF=0,
//     thousands of times. Only the servicing inside `shoot`'s spin for the
//     sender slot keeps this from deadlocking.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{Page, PageTableFlags, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

use crate::memory::page_table_manager::{OwnedPageTable, USER_MMAP_BASE};
use crate::memory::tlb;

const MAGIC: u64 = 0x544c_4253_0000_0000; // "TLBS"
/// Reads the reader makes in a round before it lets the writer move on.
const READS_PER_ACK: u32 = 32;
/// Shootdowns each side of the mutual part sends.
const HAMMER: u32 = 2000;
const WAIT_NS: u64 = 2_000_000_000;
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

fn value(i: u32) -> u64 {
    MAGIC | (i % 3) as u64
}

// ── Shared with the reader on the AP ────────────────────────────────────
static TARGET_VA: AtomicU64 = AtomicU64::new(0);
/// Page table the reader loads first; 0 = stay on its own.
static TARGET_CR3: AtomicU64 = AtomicU64::new(0);
static ROUND: AtomicU32 = AtomicU32::new(0);
/// `r + 1` once the reader has read round `r` `READS_PER_ACK` times.
static ACK: AtomicU32 = AtomicU32::new(0);
static STOP: AtomicBool = AtomicBool::new(false);
static STALE: AtomicU64 = AtomicU64::new(0);
static READS: AtomicU64 = AtomicU64::new(0);

/// Only its address matters: the mutual part invalidates it.
static HAMMER_TARGET: u64 = 0;

pub struct Report {
    pub aps: u32,
    pub rounds: u32,
    pub user_reads: u64,
    pub kernel_reads: u64,
    /// Reads that returned the old translation. Must be 0.
    pub stale: u64,
    pub hammer: u32,
}

impl core::fmt::Display for Report {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "tlb_selftest: {} ({} APs x {} rounds, reads user {} kernel {}, stale {}, mutual {}x2)",
            if self.stale == 0 { "PASS" } else { "FAIL" },
            self.aps, self.rounds, self.user_reads, self.kernel_reads, self.stale, self.hammer
        )
    }
}

fn reader(_cpu: usize) {
    let cr3 = TARGET_CR3.load(Ordering::Acquire);
    let (own, _) = Cr3::read();
    if cr3 != 0 {
        // SAFETY: the scratch table copies every kernel mapping.
        unsafe { tlb::switch_to(PhysFrame::containing_address(PhysAddr::new(cr3))) };
    }
    let p = TARGET_VA.load(Ordering::Acquire) as *const u64;
    let (mut last, mut n) = (u32::MAX, 0u32);
    while !STOP.load(Ordering::Acquire) {
        let r = ROUND.load(Ordering::Acquire);
        // SAFETY: mapped for as long as the writer has not seen us stop.
        let v = unsafe { core::ptr::read_volatile(p) };
        if v != value(r) && v != value(r + 1) {
            STALE.fetch_add(1, Ordering::Relaxed);
        }
        READS.fetch_add(1, Ordering::Relaxed);
        if r != last {
            (last, n) = (r, 0);
        }
        n += 1;
        if n == READS_PER_ACK {
            ACK.store(r + 1, Ordering::Release);
        }
    }
    if cr3 != 0 {
        // SAFETY: back to the table this CPU was running.
        unsafe { tlb::switch_to(own) };
    }
}

fn hammer(_cpu: usize) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        for _ in 0..HAMMER {
            tlb::invalidate_kernel_page(VirtAddr::new(&HAMMER_TARGET as *const u64 as u64));
        }
    });
}

/// Spins until `cond`, answering shootdowns meanwhile (the caller may have
/// IF=0, as the test boot always does).
fn wait(cond: impl Fn() -> bool, what: &'static str) -> Result<(), &'static str> {
    let start = crate::cpu::tsc::read();
    let limit = crate::cpu::tsc::freq_hz().saturating_mul(WAIT_NS) / 1_000_000_000;
    let mut spins = 0u64;
    while !cond() {
        x86_64::instructions::interrupts::without_interrupts(tlb::service_pending);
        spins += 1;
        let waited = crate::cpu::tsc::read().wrapping_sub(start);
        if (limit != 0 && waited > limit) || spins > 2_000_000_000 {
            return Err(what);
        }
        core::hint::spin_loop();
    }
    Ok(())
}

/// Runs the reader on `cpu` against `va` (in `cr3`, 0 = the AP's own)
/// while `remap(r)` moves the page to frame r % 3 for each round. The page
/// must already map frame 0. `Err` leaves the AP possibly still reading:
/// the caller must then leak what it mapped.
fn drive(cpu: usize, cr3: u64, va: u64, rounds: u32, mut remap: impl FnMut(u32)) -> Result<u64, &'static str> {
    TARGET_VA.store(va, Ordering::Relaxed);
    TARGET_CR3.store(cr3, Ordering::Relaxed);
    ROUND.store(0, Ordering::Relaxed);
    ACK.store(0, Ordering::Relaxed);
    READS.store(0, Ordering::Relaxed);
    STOP.store(false, Ordering::Release);
    if !crate::smp::run_on(cpu, reader) {
        return Err("AP mailbox busy or AP offline");
    }
    wait(|| ACK.load(Ordering::Acquire) >= 1, "reader never started")?;
    for r in 1..=rounds {
        remap(r);
        ROUND.store(r, Ordering::Release);
        wait(|| ACK.load(Ordering::Acquire) >= r + 1, "reader stopped acknowledging rounds")?;
    }
    STOP.store(true, Ordering::Release);
    wait(|| !crate::smp::ap_busy(cpu), "reader never returned")?;
    Ok(READS.load(Ordering::Relaxed))
}

fn alloc_frames() -> Result<[PhysFrame<Size4KiB>; 3], &'static str> {
    let off = crate::memory::physical_memory_offset().as_u64();
    let mut out = [PhysFrame::containing_address(PhysAddr::new(0)); 3];
    for (i, f) in out.iter_mut().enumerate() {
        // SAFETY: a fresh frame, written through the physical window.
        let pa = unsafe { crate::allocator::phys_alloc(12) }.ok_or("out of frames")?;
        unsafe {
            core::ptr::write_bytes((off + pa.as_u64()) as *mut u8, 0, 4096);
            core::ptr::write_volatile((off + pa.as_u64()) as *mut u64, value(i as u32));
            // The kernel part reads these through an uncached alias: push the
            // write-back line out first.
            core::arch::asm!("clflush [{}]; mfence", in(reg) off + pa.as_u64(), options(nostack));
        }
        *f = PhysFrame::containing_address(pa);
    }
    Ok(out)
}

fn user_scope(cpu: usize, rounds: u32) -> Result<u64, &'static str> {
    let frames = alloc_frames()?;
    // SAFETY: the Buddy allocator is up.
    let scratch = unsafe { OwnedPageTable::new_user()? };
    let page: Page<Size4KiB> = Page::containing_address(VirtAddr::new(USER_MMAP_BASE));
    // Kernel-only: the reader is ring 0.
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
    for f in frames {
        // SAFETY: frames we own; `unmap_page_and_free` below drops the one
        // left mapped from 1 to 0.
        unsafe { crate::memory::cow::set_ref(f, 1) };
    }
    unsafe { scratch.map_existing_frame(page, frames[0], flags)? };
    let pml4 = scratch.pml4_phys().as_u64();
    let result = drive(cpu, pml4, page.start_address().as_u64(), rounds, |r| unsafe {
        scratch
            .replace_frame(page, frames[(r % 3) as usize], flags)
            .expect("tlb_selftest: remap");
    });
    match result {
        Ok(reads) => {
            unsafe {
                scratch.unmap_page_and_free(page)?;
                for (i, f) in frames.iter().enumerate() {
                    if i as u32 != rounds % 3 {
                        crate::memory::cow::set_ref(*f, 0);
                        crate::allocator::phys_free(f.start_address(), 12);
                    }
                }
            }
            drop(scratch);
            Ok(reads)
        }
        Err(e) => {
            core::mem::forget(scratch);
            Err(e)
        }
    }
}

struct KernelTarget {
    va: u64,
    entry_phys: u64,
    flags: u64,
    frames: [u64; 3],
}
static KTARGET: spin::Once<Result<KernelTarget, &'static str>> = spin::Once::new();

/// Sets up the kernel-scope target: three frames and one `mmio::map` page
/// for them, kept for the whole uptime. At boot (`smp::start_aps`), because
/// `mmio::map` is boot-only; nothing if there are no APs.
pub fn prepare() {
    KTARGET.call_once(|| {
        let frames = alloc_frames()?;
        // SAFETY: frames we own, never freed while this mapping lives.
        let va = unsafe { crate::memory::mmio::map(frames[0].start_address(), 4096) }
            .ok_or("mmio::map failed")?;
        let leaf = crate::memory::memtype::leaf_for(va).ok_or("no leaf for the mmio page")?;
        if leaf.page_size != 0x1000 {
            return Err("mmio page is not a 4K leaf");
        }
        Ok(KernelTarget {
            va: va.as_u64(),
            entry_phys: leaf.entry_phys,
            flags: leaf.entry & !ADDR_MASK,
            frames: frames.map(|f| f.start_address().as_u64()),
        })
    });
}

fn kernel_scope(cpu: usize, rounds: u32) -> Result<u64, &'static str> {
    let t = KTARGET.get().ok_or("prepare() never ran")?.as_ref().map_err(|e| *e)?;
    let off = crate::memory::physical_memory_offset().as_u64();
    let set = |i: u32| unsafe {
        // SAFETY: the live leaf `prepare` found; only its frame changes.
        core::ptr::write_volatile((off + t.entry_phys) as *mut u64, t.flags | t.frames[(i % 3) as usize]);
        tlb::invalidate_kernel_page(VirtAddr::new(t.va));
    };
    set(0);
    let reads = drive(cpu, 0, t.va, rounds, |r| set(r))?;
    set(0);
    Ok(reads)
}

fn mutual(cpu: usize) -> Result<(), &'static str> {
    if !crate::smp::run_on(cpu, hammer) {
        return Err("AP mailbox busy or AP offline");
    }
    hammer(0);
    wait(|| !crate::smp::ap_busy(cpu), "mutual shootdown never finished")
}

/// Runs all three parts against up to `max_aps` online APs (the mutual
/// part against the first). `Err` is a harness failure — an AP that never
/// answered — not a stale read: those are `Report::stale`.
///
/// Call with IF=0: the caller is the writer and must not move to another
/// CPU mid-test. Since APs run processes (stage 7 of docs/smp/smp-plan.md)
/// the readers are the APs that are idle right now, never the caller's own
/// CPU — its reader would wait for a writer that is itself.
pub fn run(rounds: u32, max_aps: usize) -> Result<Report, &'static str> {
    let me = crate::cpu::cpu_id();
    let aps: alloc::vec::Vec<usize> = (1..crate::cpu::MAX_CPUS)
        .filter(|&c| c != me && crate::smp::is_online_ap(c) && crate::process::scheduler::cpu_is_idle(c))
        .take(max_aps)
        .collect();
    if aps.is_empty() {
        return Err("no online AP");
    }
    STALE.store(0, Ordering::Relaxed);
    let mut report = Report { aps: aps.len() as u32, rounds, user_reads: 0, kernel_reads: 0, stale: 0, hammer: HAMMER };
    for &cpu in &aps {
        report.user_reads += user_scope(cpu, rounds)?;
        report.kernel_reads += kernel_scope(cpu, rounds)?;
    }
    mutual(aps[0])?;
    report.stale = STALE.load(Ordering::Relaxed);
    crate::serial_println!("{}", report);
    Ok(report)
}
