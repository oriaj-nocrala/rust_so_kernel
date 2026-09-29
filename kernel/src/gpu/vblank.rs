// kernel/src/gpu/vblank.rs
//
// Phase 3 of docs/gpu/gpu-plan.md: the display's vblank interrupt, by MSI,
// on the heads the firmware (GOP) lit — no modeset, no core channel, no
// GSP. The register logic is `nvgpu::vblank` (host-tested against the
// trace); this module reports the heads, arms the interrupt, and runs the
// handler.
//
// The MSI goes to CPU 0 and its work is global: one sequence number (the
// primary head's vblanks), per-head counters, and a poll wakeup for
// `/dev/vblank` (`drivers/dev_vblank.rs`). After the boot BAR0 has two
// users: this handler, which runs on one CPU and never overlaps itself (the
// GPU sends no second MSI before it rearms), and page flips (`scanout.rs`),
// which touch only window 0's channel registers, none of the handler's.

use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use nvgpu::vblank::{self as vb, HeadTiming};
use nvgpu::Mmio;

use super::Bar0;

struct State {
    regs: Bar0,
    vector: u8,
    /// The head whose vblanks `seq()` counts: the lowest lit one.
    primary: u32,
    /// Heads with the vblank interrupt enabled.
    heads: u8,
}

static STATE: spin::Once<State> = spin::Once::new();
/// The MSI is enabled in the device: vblanks will arrive.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Vblanks of the primary head, and the `ktime_get` of the last one.
static SEQ: AtomicU64 = AtomicU64::new(0);
static LAST_NS: AtomicU64 = AtomicU64::new(0);
static PER_HEAD: [AtomicU64; vb::HEADS_MAX as usize] = [const { AtomicU64::new(0) }; vb::HEADS_MAX as usize];
static SPURIOUS: AtomicU64 = AtomicU64::new(0);
static BLOCKED: AtomicU64 = AtomicU64::new(0);
static GONE: AtomicU64 = AtomicU64::new(0);
/// Last non-zero "not serviced here" bits, for diagnosis.
static DISP_OTHER: AtomicU32 = AtomicU32::new(0);
static HEAD_OTHER: AtomicU32 = AtomicU32::new(0);

/// The interrupt is armed and the MSI enabled (`/dev/vblank` opens only
/// then).
pub fn armed() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// Vblanks of the primary head so far.
pub fn seq() -> u64 {
    SEQ.load(Ordering::Acquire)
}

/// `ktime_get()` (ns) of the primary head's last vblank.
pub fn last_ns() -> u64 {
    LAST_NS.load(Ordering::Acquire)
}

/// MSI handler: ISR context, IF=0, CPU 0.
fn on_msi(_vector: u8) {
    let Some(s) = STATE.get() else { return };
    let r = vb::service_with(&s.regs, super::supervisor::enabled(), &super::intr::extras());
    if r.extra != 0 {
        super::intr::on_serviced(r.extra);
    }
    // Supervisors first: the display is stopped until they are released.
    if r.supervisor != 0 {
        super::supervisor::on_pending(&s.regs, r.supervisor);
    }
    if let Some(info) = r.ctrl_disp_error {
        super::supervisor::note_error(info);
    }
    if r.spurious {
        SPURIOUS.fetch_add(1, Ordering::Relaxed);
    }
    if r.blocked {
        BLOCKED.fetch_add(1, Ordering::Relaxed);
    }
    if r.gone {
        GONE.fetch_add(1, Ordering::Relaxed);
    }
    if r.disp_other != 0 {
        DISP_OTHER.store(r.disp_other, Ordering::Relaxed);
    }
    if r.head_other != 0 {
        HEAD_OTHER.store(r.head_other, Ordering::Relaxed);
    }
    for (head, count) in PER_HEAD.iter().enumerate() {
        if r.vblank & (1 << head) != 0 {
            count.fetch_add(1, Ordering::Relaxed);
        }
    }
    if r.vblank & (1 << s.primary) != 0 {
        // Timestamp before the sequence number: a reader that sees the new
        // number (Acquire) sees this vblank's time.
        LAST_NS.store(crate::time::ktime_get(), Ordering::Release);
        let n = SEQ.fetch_add(1, Ordering::Release) + 1;
        // Phase 6d: GSP-RM raises no interrupt for what it queues (`gpu_intr`), so its status queue is
        // served from here, ten times a second, when no process is in the middle of an RPC.
        if n % 6 == 0 {
            super::gsp::poll_events_isr();
        }
        crate::process::syscall::poll_wakeup_for_input(crate::drivers::evdev::QUEUE_VBLANK);
    }
}

/// `/proc/kdebug` line(s).
pub fn render_kdebug() -> String {
    let Some(s) = STATE.get() else { return String::from("gpu_vblank: off") };
    let mut out = String::new();
    let _ = write!(
        out,
        "gpu_vblank: enabled={} primary_head={} seq={} last_ns={} msi={} vector={:#x} spurious={} blocked={} gone={} disp_other={:#x} head_other={:#x}",
        armed() as u8,
        s.primary,
        seq(),
        last_ns(),
        crate::interrupts::msi::count(s.vector),
        s.vector,
        SPURIOUS.load(Ordering::Relaxed),
        BLOCKED.load(Ordering::Relaxed),
        GONE.load(Ordering::Relaxed),
        DISP_OTHER.load(Ordering::Relaxed),
        HEAD_OTHER.load(Ordering::Relaxed),
    );
    for head in 0..vb::HEADS_MAX {
        if s.heads & (1 << head) != 0 {
            let _ = write!(out, "\ngpu_vblank_head{}: {}", head, PER_HEAD[head as usize].load(Ordering::Relaxed));
        }
    }
    out
}

fn ms(m: u64) -> u64 {
    crate::cpu::tsc::freq_hz() / 1000 * m
}

/// Plan B's instrument, read-only: frames counted from the raster position
/// wrapping (vline going down) over `window_ms`, and the vline range seen.
fn raster_rate(regs: &Bar0, head: u32, window_ms: u64) -> (u64, u16, u16, u64) {
    let t0 = crate::cpu::tsc::read();
    let (mut last, _) = vb::scan_position(regs, head);
    let (mut lo, mut hi) = (last, last);
    let mut wraps = 0u64;
    let mut first_wrap = None;
    let mut last_wrap = t0;
    while crate::cpu::tsc::read().wrapping_sub(t0) < ms(window_ms) {
        let (v, _) = vb::scan_position(regs, head);
        if v < last {
            let now = crate::cpu::tsc::read();
            if first_wrap.is_none() {
                first_wrap = Some(now);
            } else {
                wraps += 1;
            }
            last_wrap = now;
        }
        lo = lo.min(v);
        hi = hi.max(v);
        last = v;
        crate::memory::tlb::service_pending();
    }
    // Rate between the first and the last wrap: whole frames only.
    let span = last_wrap.wrapping_sub(first_wrap.unwrap_or(last_wrap));
    let mhz = if span == 0 { 0 } else { wraps * crate::cpu::tsc::freq_hz() * 1000 / span };
    (wraps, lo, hi, mhz)
}

/// `gpu=vblank`: report the heads, arm the interrupt. Runs at boot with
/// IF=0, before the APs are released; the first MSI is taken once the boot
/// enables interrupts.
pub fn setup(r: &mut String, regs: &Bar0, bdf: (u8, u8, u8)) {
    let present = vb::head_mask(regs);
    let _ = writeln!(r, "vblank: heads present {:#04x}", present);
    let mut lit = 0u8;
    for head in 0..vb::HEADS_MAX {
        if present & (1 << head) == 0 {
            continue;
        }
        let t = HeadTiming::read_armed(regs, head);
        if t.active() {
            lit |= 1 << head;
            let (w, h) = t.visible();
            let mhz = t.refresh_mhz();
            let _ = writeln!(
                r,
                "vblank: head {} lit: {}x{} total {}x{} pixclk {} Hz refresh {}.{:03} Hz depth {} (blank h {}-{} v {}-{}, sync end h {} v {})",
                head, w, h, t.htotal, t.vtotal, t.hz, mhz / 1000, mhz % 1000, t.depth_code,
                t.hblanke, t.hblanks, t.vblanke, t.vblanks, t.hsynce, t.vsynce
            );
        } else {
            let _ = writeln!(r, "vblank: head {} dark (clock {} total {}x{})", head, t.hz, t.htotal, t.vtotal);
        }
    }
    // What the firmware left in the interrupt registers: evidence for the
    // "can vblank run without the core channel" question of the plan.
    let _ = write!(r, "vblank: before arm: top {:#x} disp_intr {:#x} ctrl_disp_en {:#x} leaves", regs.rd32(vb::VFN_TOP), regs.rd32(vb::DISP_INTR), regs.rd32(vb::CTRL_DISP_EN));
    for leaf in 0..vb::VFN_LEAVES {
        let _ = write!(r, " {:#x}", regs.rd32(vb::VFN_LEAF_STAT + leaf * 4));
    }
    let _ = writeln!(r);
    for head in 0..vb::HEADS_MAX {
        if present & (1 << head) != 0 {
            let _ = writeln!(
                r,
                "vblank: before arm: head {} stat {:#x} msk {:#x} en {:#x}",
                head,
                regs.rd32(vb::HEAD_TIMING_STAT + head * 4),
                regs.rd32(vb::HEAD_TIMING_MSK + head * 4),
                regs.rd32(vb::HEAD_TIMING_EN + head * 4)
            );
        }
    }
    if lit == 0 {
        let _ = writeln!(r, "vblank: no head is lit; not arming");
        return;
    }
    let primary = lit.trailing_zeros();

    // Plan B, measured whatever happens next.
    let (frames, lo, hi, mhz) = raster_rate(regs, primary, 200);
    let _ = writeln!(r, "vblank: raster head {}: {} frames in 200 ms, vline {}..{}, {}.{:03} Hz", primary, frames, lo, hi, mhz / 1000, mhz % 1000);

    // Does the status bit latch with the firmware's setup? Clear it (write
    // 1, as the handler does), wait two frames, look.
    let stat = vb::HEAD_TIMING_STAT + primary * 4;
    regs.wr32(stat, vb::HEAD_TIMING_VBLANK);
    let after_clear = regs.rd32(stat);
    regs.udelay(40_000);
    let later = regs.rd32(stat);
    let _ = writeln!(r, "vblank: head {} status after clear {:#x}, 40 ms later {:#x} (bit 2 = vblank latched)", primary, after_clear, later);

    let Some(vector) = crate::interrupts::msi::alloc(on_msi) else {
        let _ = writeln!(r, "vblank: no free MSI vector");
        return;
    };
    STATE.call_once(|| State { regs: Bar0 { base: regs.base, len: regs.len }, vector, primary, heads: lit });
    vb::arm_with(regs, present, lit, &super::intr::extras());
    let (b, d, f) = bdf;
    crate::pci::update_command(b, d, f, hal::pcicfg::COMMAND_MASTER, 0);
    // CPU 0: the boot runs on it (the BSP), and the work is global.
    match crate::pci::enable_msi(b, d, f, crate::interrupts::apic::this_lapic_id(), vector) {
        Ok(()) => {
            ENABLED.store(true, Ordering::Release);
            let _ = writeln!(
                r,
                "vblank: armed heads {:#04x}, primary head {}, MSI vector {:#x} to CPU 0, command {:#06x}",
                lit, primary, vector, crate::pci::command(b, d, f)
            );
        }
        Err(e) => {
            // The tree is armed but nothing will arrive: `/dev/vblank`
            // stays closed, /proc/kdebug shows the zero count.
            let _ = writeln!(r, "vblank: MSI not enabled: {}", e);
        }
    }
}
