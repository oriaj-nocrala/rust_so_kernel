// kernel/src/gpu/scanout.rs
//
// `gpu=scanout` (phase 5.3 of docs/gpu/gpu-plan.md): window 0 scans out
// two buffers of this kernel's own in VRAM, at the mode the GOP left, and
// the framebuffer flips between them (`Framebuffer::attach_flip`,
// `present`). The channel work is `nvgpu::evo::flip` (host-tested); this
// module places and maps the buffers, measures the first flips at boot and
// implements `framebuffer::Scanout` for the running system.
//
// Boot (IF=0, before the APs, after `evo::bring_up` and before vblank is
// armed): check that window 0's surface is the framebuffer's geometry, map
// the buffers WC, attach (the shadow is copied into both, then a flip to
// buffer 0), and time the flip against the raster and the head's LOADV and
// VBLANK status bits: that says when the ARMED offset changes and whether
// "ARMED = requested" is a flip's completion. Then the GOP framebuffer is
// painted red: from now on it is not scanned out, so the screen must not
// change. Two more flips (to 1 and back to 0) prove both buffers.
//
// Afterwards `flip`/`flip_done` run under `FRAMEBUFFER` (`/dev/fb0`'s
// `FBIO_FLUSH`, any CPU, IF=1): they touch window 0's PUT/GET and its
// ARMED offset, registers the vblank MSI handler never touches, so the two
// share BAR0 without a lock.

use alloc::boxed::Box;
use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

use nvgpu::evo::{self, Chan, Faults, FlipError, Push};
use nvgpu::vblank as vb;
use nvgpu::Mmio;

use super::evo::PushBuf;
use super::Bar0;

/// The two scanout buffers, as VRAM offsets (= BAR1 offsets: BAR1 maps
/// VRAM from 0 without a VM, phase 5.1). The GOP framebuffer is at 0
/// (8.4 MiB at pitch 8192), the instance memory at `evo::INST_VRAM` (near
/// 8 GiB), and nouveau put its own first surface at 2 MiB
/// (`modeset-push.txt`, `SET_OFFSET = 0x2000`): nothing the firmware keeps
/// lives this low.
const BUF_VRAM: [u64; 2] = [16 << 20, 32 << 20];
/// Room for each buffer.
const BUF_MAX: u64 = 16 << 20;

const W0: Chan = evo::window(0);

static SUBMITTED: AtomicU64 = AtomicU64::new(0);
static LATCHED: AtomicU64 = AtomicU64::new(0);
/// `flip_done` answered "not yet" (a caller got `Busy`).
static NOT_YET: AtomicU64 = AtomicU64::new(0);
static REFUSED: AtomicU64 = AtomicU64::new(0);
/// From submitting a flip to the first `flip_done` that saw it done (an
/// upper bound: nobody asks sooner than the next present).
static LAT_LAST_US: AtomicU64 = AtomicU64::new(0);
static LAT_MAX_US: AtomicU64 = AtomicU64::new(0);
static LAT_SUM_US: AtomicU64 = AtomicU64::new(0);
/// Vblanks (primary head) between the submit and the done, summed and the
/// largest: 1 each when every flip lands on the next vblank.
static SEQ_GAP_SUM: AtomicU64 = AtomicU64::new(0);
static SEQ_GAP_MAX: AtomicU64 = AtomicU64::new(0);
/// Buffer scanned out after the last completed flip.
static SHOWN: AtomicU64 = AtomicU64::new(u64::MAX);

/// `/proc/kdebug` line.
pub fn render_kdebug() -> String {
    if SHOWN.load(Ordering::Relaxed) == u64::MAX {
        return String::from("gpu_flip: off");
    }
    let latched = LATCHED.load(Ordering::Relaxed);
    alloc::format!(
        "gpu_flip: submitted={} latched={} not_yet={} refused={} shown={} latency_us last={} max={} avg={} vblanks_per_flip sum={} max={}",
        SUBMITTED.load(Ordering::Relaxed),
        latched,
        NOT_YET.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
        SHOWN.load(Ordering::Relaxed),
        LAT_LAST_US.load(Ordering::Relaxed),
        LAT_MAX_US.load(Ordering::Relaxed),
        LAT_SUM_US.load(Ordering::Relaxed) / latched.max(1),
        SEQ_GAP_SUM.load(Ordering::Relaxed),
        SEQ_GAP_MAX.load(Ordering::Relaxed),
    )
}

/// Window 0 flipping between `BUF_VRAM[0]` and `[1]`.
struct GpuScanout {
    regs: Bar0,
    mem: &'static PushBuf,
    /// Window 0's PUT, bytes: where the next push goes.
    put: u32,
    /// The `SET_OFFSET` of the flip not yet seen done, and which buffer.
    requested: Option<(u32, usize)>,
    kick_ns: u64,
    kick_seq: u64,
}

impl crate::framebuffer::Scanout for GpuScanout {
    fn flip(&mut self, buf: usize) -> Result<(), &'static str> {
        let mut push = Push::new(self.mem, self.put);
        let res = evo::flip(&self.regs, W0, &mut push, BUF_VRAM[buf]);
        self.put = push.put_bytes();
        match res {
            Ok(origin) => {
                self.requested = Some((origin, buf));
                self.kick_ns = crate::time::ktime_get();
                self.kick_seq = super::vblank::seq();
                SUBMITTED.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                REFUSED.fetch_add(1, Ordering::Relaxed);
                Err(match e {
                    FlipError::Busy { .. } => "window 0 has not fetched the previous push",
                    FlipError::Misaligned { .. } => "misaligned surface",
                    FlipError::Chan(_) => "window 0 channel stalled",
                })
            }
        }
    }

    /// Done when window 0 fetched the push, its ARMED offset is the new
    /// one and, once vblank interrupts count (after boot), a vblank has
    /// passed since the submit.
    fn flip_done(&mut self) -> bool {
        let Some((origin, buf)) = self.requested else { return true };
        let seq = super::vblank::seq();
        let counting = super::vblank::armed() && x86_64::instructions::interrupts::are_enabled();
        let push = Push::new(self.mem, self.put);
        if (counting && seq <= self.kick_seq) || !evo::flip_latched(&self.regs, W0, &push, origin) {
            NOT_YET.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.requested = None;
        let us = crate::time::ktime_get().saturating_sub(self.kick_ns) / 1000;
        LATCHED.fetch_add(1, Ordering::Relaxed);
        LAT_LAST_US.store(us, Ordering::Relaxed);
        LAT_MAX_US.fetch_max(us, Ordering::Relaxed);
        LAT_SUM_US.fetch_add(us, Ordering::Relaxed);
        if counting {
            let gap = seq - self.kick_seq;
            SEQ_GAP_SUM.fetch_add(gap, Ordering::Relaxed);
            SEQ_GAP_MAX.fetch_max(gap, Ordering::Relaxed);
        }
        SHOWN.store(buf as u64, Ordering::Relaxed);
        true
    }
}

/// When each event was first seen after a flip was submitted: TSC and the
/// raster line then.
#[derive(Default)]
struct Seen {
    fetched: Option<(u64, u16)>,
    armed: Option<(u64, u16)>,
    loadv: Option<(u64, u16)>,
    vblank: Option<(u64, u16)>,
}

fn us(t0: u64, t: u64) -> u64 {
    t.wrapping_sub(t0) * 1_000_000 / crate::cpu::tsc::freq_hz().max(1)
}

/// Polls for up to 100 ms after a flip to `origin` was submitted (at `t0`,
/// raster line `v0`) and logs when window 0 fetched it, when its ARMED
/// offset changed, and when `head`'s LOADV and VBLANK bits latched (they
/// were cleared just before the submit; nothing else clears them before
/// vblank is armed).
fn measure(r: &mut String, regs: &Bar0, what: &str, head: u32, origin: u32, t0: u64, v0: u16) -> bool {
    let stat = vb::HEAD_TIMING_STAT + head * 4;
    let mut s = Seen::default();
    let limit = crate::cpu::tsc::freq_hz() / 10;
    while crate::cpu::tsc::read().wrapping_sub(t0) < limit {
        let t = crate::cpu::tsc::read();
        let (v, _) = vb::scan_position(regs, head);
        if s.fetched.is_none() && regs.rd32(W0.get()) == regs.rd32(W0.put()) {
            s.fetched = Some((t, v));
        }
        if s.armed.is_none() && regs.rd32(W0.armed_base() + evo::WNDW_SET_OFFSET0) == origin {
            s.armed = Some((t, v));
        }
        let st = regs.rd32(stat);
        if s.loadv.is_none() && st & 0x3 != 0 {
            s.loadv = Some((t, v));
        }
        if s.vblank.is_none() && st & vb::HEAD_TIMING_VBLANK != 0 {
            s.vblank = Some((t, v));
        }
        if s.fetched.is_some() && s.armed.is_some() && s.loadv.is_some() && s.vblank.is_some() {
            break;
        }
        crate::memory::tlb::service_pending();
    }
    let _ = write!(r, "scanout: {}: submitted at vline {}", what, v0);
    for (name, e) in [("fetched", s.fetched), ("armed", s.armed), ("loadv", s.loadv), ("vblank", s.vblank)] {
        match e {
            Some((t, v)) => {
                let _ = write!(r, ", {} +{} us vline {}", name, us(t0, t), v);
            }
            None => {
                let _ = write!(r, ", {} NOT SEEN in 100 ms", name);
            }
        }
    }
    let _ = writeln!(r);
    s.armed.is_some() && s.fetched.is_some()
}

/// `gpu=scanout`: see the header. `put` is window 0's PUT after
/// `evo::bring_up`.
pub fn setup(r: &mut String, regs: &Bar0, bar1: Option<(u64, u64)>, put: u32) {
    let t_start = crate::cpu::tsc::read();
    let Some(mem) = super::evo::window_push() else {
        let _ = writeln!(r, "scanout: STOP: no window 0 push buffer");
        return;
    };
    let Some((bar1_phys, bar1_len)) = bar1 else {
        let _ = writeln!(r, "scanout: STOP: no BAR1");
        return;
    };
    if bar1_len < BUF_VRAM[1] + BUF_MAX {
        let _ = writeln!(r, "scanout: STOP: BAR1 is {:#x} bytes, the buffers need {:#x}", bar1_len, BUF_VRAM[1] + BUF_MAX);
        return;
    }
    let Some((w, h, pitch, len)) = crate::framebuffer::FRAMEBUFFER.lock().as_ref().map(|fb| {
        let (w, h) = fb.dimensions();
        (w, h, fb.stride() * fb.bytes_per_pixel(), fb.byte_len())
    }) else {
        let _ = writeln!(r, "scanout: STOP: no framebuffer");
        return;
    };
    // Window 0 must already scan out this framebuffer's layout: only the
    // offset changes.
    let surf = nvgpu::dispstate::Snapshot::read_armed(regs).surface0();
    let _ = writeln!(
        r,
        "scanout: window 0 ARMED {}x{} pitch {} format {:#x} block height {} offset {:#x}; framebuffer {}x{} pitch {} ({} bytes)",
        surf.width, surf.height, surf.pitch, surf.format, surf.block_height, surf.offset, w, h, pitch, len
    );
    if (surf.width as usize, surf.height as usize, surf.pitch as usize) != (w, h, pitch) || surf.block_height != 0 {
        let _ = writeln!(r, "scanout: STOP: window 0's surface is not the framebuffer's layout");
        return;
    }
    if len as u64 > BUF_MAX {
        let _ = writeln!(r, "scanout: STOP: a {}-byte screen does not fit a {:#x}-byte buffer", len, BUF_MAX);
        return;
    }

    // --- The buffers: one WC mapping of BAR1 [16 MiB, 48 MiB).
    let map_len = BUF_VRAM[1] + BUF_MAX - BUF_VRAM[0];
    // SAFETY: BAR1 is the VRAM aperture; this range is VRAM nothing else
    // uses (`BUF_VRAM`).
    let Some(virt) = (unsafe { crate::memory::mmio::map(x86_64::PhysAddr::new(bar1_phys + BUF_VRAM[0]), map_len as usize) }) else {
        let _ = writeln!(r, "scanout: STOP: cannot map {:#x} bytes of BAR1", map_len);
        return;
    };
    match crate::memory::memtype::set_pat_index_range(virt.as_u64(), map_len, hal::memtype::PAT_WC_INDEX) {
        Ok(_) => {
            let _ = writeln!(r, "scanout: buffers at VRAM {:#x} and {:#x}, mapped WC at {:#x}", BUF_VRAM[0], BUF_VRAM[1], virt.as_u64());
        }
        Err(e) => {
            // Still correct, only slower (UC writes, ~0.4 GB/s).
            let _ = writeln!(r, "scanout: buffers mapped at {:#x} but not WC: {}", virt.as_u64(), e);
        }
    }
    let buf = |i: usize| -> &'static mut [u8] {
        // SAFETY: inside the mapping above, `len` bytes each, disjoint
        // (`BUF_MAX` apart), mapped for the rest of the boot and handed
        // only to the framebuffer.
        unsafe { core::slice::from_raw_parts_mut((virt.as_u64() + BUF_VRAM[i] - BUF_VRAM[0]) as *mut u8, len) }
    };

    // --- The head whose raster and status bits time the flips.
    let head = (0..vb::HEADS_MAX).find(|&h| vb::head_mask(regs) & (1 << h) != 0 && vb::HeadTiming::read_armed(regs, h).active());
    let Some(head) = head else {
        let _ = writeln!(r, "scanout: STOP: no lit head");
        return;
    };
    let start = Faults::read(regs);
    let stat = vb::HEAD_TIMING_STAT + head * 4;
    let clear = |regs: &Bar0| regs.wr32(stat, 0x3 | vb::HEAD_TIMING_VBLANK);

    // --- Attach: shadow → both buffers, flip to buffer 0.
    let scanout = GpuScanout { regs: Bar0 { base: regs.base, len: regs.len }, mem, put, requested: None, kick_ns: 0, kick_seq: 0 };
    clear(regs);
    let t_copy = crate::cpu::tsc::read();
    let attached = crate::framebuffer::FRAMEBUFFER
        .lock()
        .as_mut()
        .map(|fb| fb.attach_flip([buf(0), buf(1)], Box::new(scanout)));
    let t0 = crate::cpu::tsc::read();
    let (v0, _) = vb::scan_position(regs, head);
    match attached {
        Some(Ok(())) => {
            let _ = writeln!(r, "scanout: shadow copied to both buffers in {} us, flip to buffer 0 submitted", us(t_copy, t0));
        }
        Some(Err(e)) => {
            let _ = writeln!(r, "scanout: STOP: attach: {}", e);
            return;
        }
        None => {
            let _ = writeln!(r, "scanout: STOP: no framebuffer");
            return;
        }
    }
    let origin0 = (BUF_VRAM[0] >> 8) as u32;
    let took = measure(r, regs, "flip GOP -> buffer 0", head, origin0, t0, v0);
    let settled = crate::framebuffer::FRAMEBUFFER.lock().as_mut().is_some_and(|fb| fb.flip_settled());
    let now = Faults::read(regs);
    let new = now.new_since(&start);
    if !took || !settled || new.bad() {
        let _ = writeln!(
            r,
            "scanout: STOP: first flip: took {} settled {} new faults {:?} (exception slot wndw0 {:#x})",
            took, settled, new, now.wndw0_exc
        );
        return;
    }

    // --- Proof: the GOP framebuffer is no longer scanned out.
    let gop = crate::framebuffer::FRAMEBUFFER.lock().as_ref().map(|fb| (fb.virt_addr(), fb.byte_len()));
    if let Some((gop_virt, gop_len)) = gop {
        let t = crate::cpu::tsc::read();
        // SAFETY: the GOP framebuffer's mapping (`byte_len` bytes, WC or
        // UC). Since `attach_flip` the framebuffer writes only its flip
        // buffers, so nothing else touches this memory any more.
        unsafe {
            let p = gop_virt as *mut u32;
            for i in 0..gop_len / 4 {
                core::ptr::write_volatile(p.add(i), 0x0040_0000);
            }
            core::arch::asm!("sfence", options(nostack, preserves_flags));
        }
        let _ = writeln!(r, "scanout: GOP framebuffer painted dark red in {} us (a red screen would mean window 0 still scans it)", us(t, crate::cpu::tsc::read()));
    }

    // --- Both buffers, by two flips with nothing new to copy.
    for (to, what) in [(1usize, "flip buffer 0 -> 1"), (0, "flip buffer 1 -> 0")] {
        clear(regs);
        let t = crate::cpu::tsc::read();
        let (v, _) = vb::scan_position(regs, head);
        let res = crate::framebuffer::FRAMEBUFFER.lock().as_mut().map(|fb| fb.present(&[]));
        if res != Some(Ok(())) {
            let _ = writeln!(r, "scanout: STOP: {}: {:?}", what, res);
            return;
        }
        if !measure(r, regs, what, head, (BUF_VRAM[to] >> 8) as u32, t, v) {
            let _ = writeln!(r, "scanout: STOP: {} did not take", what);
            return;
        }
        let settled = crate::framebuffer::FRAMEBUFFER.lock().as_mut().is_some_and(|fb| fb.flip_settled());
        if !settled {
            let _ = writeln!(r, "scanout: STOP: {} not settled", what);
            return;
        }
    }
    clear(regs);
    let new = Faults::read(regs).new_since(&start);
    if new.bad() {
        let _ = writeln!(r, "scanout: STOP: new faults {:?}", new);
        return;
    }
    let _ = writeln!(
        r,
        "scanout: OK: scanning out buffer {} of this kernel's (VRAM {:#x}), three flips latched, no faults, {} ms",
        SHOWN.load(Ordering::Relaxed),
        BUF_VRAM[0],
        super::ms_since(t_start)
    );
}
