// kernel/src/gpu/hdmi.rs
//
// `gpu=hdmi` (phase 5.8 of docs/gpu/gpu-plan.md): lights the HP on HDMI
// (DCB output 07, head 1, SOR-0, window 2) with a picture of the kernel's
// own, while the ASUS keeps running on head 0. The register work is
// `nvgpu::hdmi` (replayed against nouveau's trace), the supervisor work
// `nvgpu::supervisor` (TMDS attach, likewise); this module owns the state,
// the push buffer, the picture in VRAM and the order of the steps.
//
// - Boot (`setup`, IF=0, before the APs, after `modeset::setup`): finds the
//   connected HDMI connector `gpu=disp` probed and its EDID, the DCB output,
//   a free head and SOR, the HP's preferred mode, checks the VPLL can make
//   its clock; allocates window 2's push buffer, maps 16 MiB of BAR1 at
//   `SURF_VRAM` write-combined and paints the test picture there (nothing
//   scans it out yet). Reads only, besides that picture. Logged as `hdmi:`.
// - `/dev/dispctl` `hdmi on` (`on`, any CPU, IF=1 like `modeset::set`, since
//   the supervisors it waits for come by MSI on CPU 0): SOR power, window 2's
//   channel, route + HDMI encoder registers, one core push (head 1's mode,
//   SOR-0 TMDS, window 2 as its owner) + UPDATE, wait for the supervisors
//   (VPLL1, the TMDS scripts), window 2's surface push + UPDATE, then
//   verify: ARMED state, faults, the refresh of head 1 measured by the raster.
//   A failure detaches again. `hdmi off` reverses it.
// - Head 0, the window 0 flips and the compositor are not touched. The
//   supervisors are shared (they release for every head); `BUSY` keeps this
//   from overlapping `modeset::set`.

use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use nvgpu::display::{Path, Status};
use nvgpu::edid::Edid;
use nvgpu::evo::{self, Chan, Faults, Push};
use nvgpu::hdmi as nh;
use nvgpu::mode::Mode;
use nvgpu::supervisor::{Outp, SorState};
use nvgpu::vblank::{self as vb, HeadTiming};
use nvgpu::Mmio;

use font8x8::legacy::BASIC_LEGACY;

use crate::framebuffer::{GLYPH_H, GLYPH_W};

use super::evo::PushBuf;
use super::supervisor::{self as svr, RequestError};
use super::Bar0;

/// The surface: 16 MiB of VRAM (= BAR1 offset) after `scanout.rs`'s two; the
/// picture at its start, the LUTs and notifier area further up
/// (`nvgpu::hdmi`).
const SURF_VRAM: u64 = nh::SLOT_VRAM;
const SURF_MAX: u64 = nh::SLOT_LEN;
/// Bound on each wait for the supervisors or a latch.
const WAIT_MS: u64 = 1000;

struct State {
    outp: Outp,
    sor: u8,
    head: u32,
    window: Chan,
    mode: Mode,
    /// `HDMI-A-1`.
    conn: String,
    monitor: String,
    push: &'static PushBuf,
    /// The picture's pitch (bytes).
    pitch: u32,
}

static STATE: spin::Once<State> = spin::Once::new();
static BUSY: AtomicBool = AtomicBool::new(false);
static ON: AtomicBool = AtomicBool::new(false);
/// Window 2's channel is up (`init_channel` runs once).
static CHAN_UP: AtomicBool = AtomicBool::new(false);
/// Where window 2's next push goes (bytes).
static PUT: AtomicU32 = AtomicU32::new(0);

static ONS: AtomicU64 = AtomicU64::new(0);
static OK: AtomicU64 = AtomicU64::new(0);
static FAILED: AtomicU64 = AtomicU64::new(0);
static LAST_MS: AtomicU64 = AtomicU64::new(0);
/// Measured refresh of head 1 after the last `on`, mHz.
static REFRESH_MHZ: AtomicU64 = AtomicU64::new(0);

fn report(line: String) {
    crate::serial_println!("{}", line);
    svr::log_line(line);
}

fn elapsed_ms(t0: u64) -> u64 {
    crate::cpu::tsc::read().wrapping_sub(t0) / (crate::cpu::tsc::freq_hz() / 1000).max(1)
}

fn wait(done: impl Fn() -> bool) -> bool {
    let t0 = crate::cpu::tsc::read();
    while !done() {
        if elapsed_ms(t0) >= WAIT_MS {
            return done();
        }
        crate::memory::tlb::service_pending();
        core::hint::spin_loop();
    }
    true
}

/// Text on the picture: `BASIC_LEGACY` glyphs at `scale`, white on the
/// panel's colour, straight into the mapped surface.
fn draw_text(virt: u64, pitch: u32, x: u32, y: u32, text: &str, scale: u32) {
    for (i, b) in text.bytes().enumerate() {
        let glyph = BASIC_LEGACY.get(b as usize).copied().unwrap_or(BASIC_LEGACY[b'?' as usize]);
        for (row, bits) in glyph.iter().enumerate().take(GLYPH_H) {
            for col in 0..GLYPH_W {
                let px = if (bits >> col) & 1 != 0 { 0x00ff_ffff } else { nvgpu::pattern::PANEL_BG };
                for sy in 0..scale {
                    for sx in 0..scale {
                        let (dx, dy) = (x + (i * GLYPH_W + col) as u32 * scale + sx, y + row as u32 * scale + sy);
                        // SAFETY: inside the mapped surface (checked by the
                        // caller: the text stays in the panel).
                        unsafe { core::ptr::write_volatile((virt + dy as u64 * pitch as u64 + dx as u64 * 4) as *mut u32, px) };
                    }
                }
            }
        }
    }
}

/// Boot, `gpu=hdmi`.
pub fn setup(r: &mut String, regs: &Bar0, bar1: Option<(u64, u64)>) {
    let Some(d) = svr::disp_at_boot() else {
        let _ = writeln!(r, "hdmi: STOP: the supervisors are not set up (see super:)");
        return;
    };
    let Some((bios, dcb)) = super::VBIOS.get() else {
        let _ = writeln!(r, "hdmi: STOP: no VBIOS/DCB");
        return;
    };
    let Some(p) = super::PROBES.get().and_then(|ps| {
        ps.iter().find(|p| matches!(p.conn.path, Some(Path::I2c { .. })) && p.status == Status::Connected && !p.edid.is_empty())
    }) else {
        let _ = writeln!(r, "hdmi: STOP: no connected HDMI connector with an EDID (see displays:)");
        return;
    };
    let Ok(edid) = Edid::parse(&p.edid) else {
        let _ = writeln!(r, "hdmi: STOP: {}'s EDID does not parse", p.conn.name);
        return;
    };
    let Some(o) = dcb.outputs.iter().find(|o| o.kind == nvgpu::dcb::OUTPUT_TMDS && o.connector == p.conn.index) else {
        let _ = writeln!(r, "hdmi: STOP: no TMDS output for {}", p.conn.name);
        return;
    };
    let outp = Outp::new(o, dcb);
    let hs = regs.rd32(evo::DISP_HEAD_SOR_MASK);
    let (heads, sors) = ((hs & 0xff) as u8, ((hs >> 8) & 0xff) as u8);
    let head = nh::HP_HEAD;
    let window = nh::window_chan(head);
    let wndws = regs.rd32(evo::DISP_WNDW_MASK);
    if heads & (1 << head) == 0 || wndws & (1 << (window.user - 1)) == 0 {
        let _ = writeln!(r, "hdmi: STOP: head {} or window {} does not exist (heads {:#x} windows {:#x})", head, window.user - 1, heads, wndws);
        return;
    }
    let lit = HeadTiming::read_armed(regs, head);
    if lit.active() {
        let _ = writeln!(r, "hdmi: STOP: head {} is already scanning out ({:?}): the GOP lit the HP", head, lit);
        return;
    }
    // A SOR nothing drives (ARMED), other than the DP one.
    let Some(sor) = (0..nvgpu::supervisor::SORS_MAX)
        .find(|&s| sors & (1 << s) != 0 && s != d.sor as u32 && SorState::read(regs, s, true).head == 0)
        .map(|s| s as u8)
    else {
        let _ = writeln!(r, "hdmi: STOP: no free SOR (mask {:#x}, the DP one is {})", sors, d.sor);
        return;
    };
    let Some(mode) = nh::preferred(&edid) else {
        let _ = writeln!(r, "hdmi: STOP: {} has no progressive detailed timing", p.conn.name);
        return;
    };
    let (w, h) = mode.size();
    let pitch = w as u32 * 4;
    if pitch as u64 * h as u64 > SURF_MAX {
        let _ = writeln!(r, "hdmi: STOP: {}x{} does not fit {} bytes", w, h, SURF_MAX);
        return;
    }
    let vpll = nvgpu::pll::parse(bios, nvgpu::pll::PLL_VPLL0 + head as u8).map(|l| nvgpu::pll::calc(&l, mode.clock_khz));
    let _ = writeln!(
        r,
        "hdmi: {} '{}' outp {:02x} (hash {:04x}:{:04x} conn {:?}) -> head {} window {} SOR-{} (power {:#x}); mode {}x{} {} kHz {}.{:03} Hz total {}x{}; vpll {:?}; VIC/aspect {:?}, max_ac_packet {}",
        p.conn.name,
        edid.name.as_deref().unwrap_or("?"),
        outp.dcb,
        outp.hasht,
        outp.hashm,
        outp.conn,
        head,
        window.user - 1,
        sor,
        regs.rd32(nh::SOR_POWER + sor as u32 * 0x800),
        w,
        h,
        mode.clock_khz,
        mode.refresh_mhz() / 1000,
        mode.refresh_mhz() % 1000,
        mode.htotal,
        mode.vtotal,
        vpll,
        nh::vic(&mode),
        nh::max_ac_packet(&mode)
    );
    if !matches!(vpll, Ok(Ok(_))) {
        let _ = writeln!(r, "hdmi: STOP: no VPLL coefficients for {} kHz", mode.clock_khz);
        return;
    }
    let Some((bar1_phys, bar1_len)) = bar1 else {
        let _ = writeln!(r, "hdmi: STOP: no BAR1");
        return;
    };
    if bar1_len < SURF_VRAM + SURF_MAX {
        let _ = writeln!(r, "hdmi: STOP: BAR1 is {:#x} bytes, the surface needs {:#x}", bar1_len, SURF_VRAM + SURF_MAX);
        return;
    }
    let push = match super::evo::alloc_push() {
        Ok(b) => alloc::boxed::Box::leak(alloc::boxed::Box::new(b)),
        Err(e) => {
            let _ = writeln!(r, "hdmi: STOP: push buffer: {:?}", e);
            return;
        }
    };
    // SAFETY: BAR1 is the VRAM aperture; [SURF_VRAM, +SURF_MAX) is VRAM
    // nothing else uses (scanout.rs uses [16, 48) MiB, the GOP 0.., the
    // instance memory the top).
    let Some(virt) = (unsafe { crate::memory::mmio::map(x86_64::PhysAddr::new(bar1_phys + SURF_VRAM), SURF_MAX as usize) }) else {
        let _ = writeln!(r, "hdmi: STOP: cannot map the surface");
        return;
    };
    match crate::memory::memtype::set_pat_index_range(virt.as_u64(), SURF_MAX, hal::memtype::PAT_WC_INDEX) {
        Ok(_) => {}
        Err(e) => {
            let _ = writeln!(r, "hdmi: surface not WC ({}): slower, still correct", e);
        }
    }
    // The picture, row by row.
    let t0 = crate::cpu::tsc::read();
    let virt = virt.as_u64();
    for y in 0..h as u32 {
        for x in 0..w as u32 {
            // SAFETY: inside the mapping (`pitch * h <= SURF_MAX`).
            unsafe { core::ptr::write_volatile((virt + y as u64 * pitch as u64 + x as u64 * 4) as *mut u32, nvgpu::pattern::pixel(x, y, w as u32, h as u32)) };
        }
    }
    let (px, py, pw, ph) = nvgpu::pattern::panel(w as u32, h as u32);
    let mut line = String::new();
    let _ = write!(line, "{} {}x{} {} Hz", p.conn.name, w, h, mode.refresh_hz());
    if pw >= 8 * GLYPH_W as u32 * 12 && ph >= 12 * GLYPH_H as u32 {
        draw_text(virt, pitch, px + 40, py + 30, "constanos", 10);
        draw_text(virt, pitch, px + 40, py + 30 + 12 * GLYPH_H as u32, &line, 4);
        draw_text(virt, pitch, px + 40, py + 30 + 12 * GLYPH_H as u32 + 6 * GLYPH_H as u32, "GA106 own driver, no GSP: head 1 / SOR-0 / window 2", 3);
    }
    // The LUTs nouveau always programs (identity) and a zeroed notifier area.
    for (vram, bytes) in [(nh::ILUT_VRAM, nvgpu::lut::ilut_identity()), (nh::OLUT_VRAM, nvgpu::lut::olut_identity())] {
        // SAFETY: inside the mapping (`SLOT_VRAM + 0x00e0_0000 + 0x4000 + BYTES <= SLOT_LEN`).
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), (virt + (vram - SURF_VRAM)) as *mut u8, bytes.len()) };
    }
    // SAFETY: the notifier area is inside the mapping.
    unsafe { core::ptr::write_bytes((virt + (nh::NTFY_VRAM - SURF_VRAM)) as *mut u8, 0, nh::NTFY_LEN as usize) };
    // SAFETY: a fence.
    unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)) };
    let _ = writeln!(r, "hdmi: LUTs at VRAM {:#x} and {:#x}, notifier at {:#x}", nh::ILUT_VRAM, nh::OLUT_VRAM, nh::NTFY_VRAM);
    let _ = writeln!(r, "hdmi: picture painted at VRAM {:#x} (mapped {:#x}) in {} ms", SURF_VRAM, virt, elapsed_ms(t0));
    STATE.call_once(|| State {
        outp,
        sor,
        head,
        window,
        mode,
        conn: p.conn.name.clone(),
        monitor: edid.name.clone().unwrap_or_default(),
        push,
        pitch,
    });
    let _ = writeln!(r, "hdmi: ready: /dev/dispctl `hdmi on` / `hdmi off`");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdmiError {
    NotReady,
    Busy,
    /// Already on / already off.
    State,
    /// Something failed (details in /proc/gpu); it was switched off again.
    Failed,
}

fn fail(what: &str, why: String) -> HdmiError {
    FAILED.fetch_add(1, Ordering::Relaxed);
    report(alloc::format!("hdmi: {} FAILED: {}", what, why));
    HdmiError::Failed
}

/// Runs `f` with IF=1 (the supervisors come by MSI on CPU 0; a syscall may
/// enter with IF=0 there, `modeset::set`), holding `BUSY`.
fn locked<R>(f: impl FnOnce() -> Result<R, HdmiError>) -> Result<R, HdmiError> {
    if BUSY.swap(true, Ordering::Acquire) {
        return Err(HdmiError::Busy);
    }
    let was_on = x86_64::instructions::interrupts::are_enabled();
    if !was_on {
        x86_64::instructions::interrupts::enable();
    }
    let t0 = crate::cpu::tsc::read();
    let res = f();
    LAST_MS.store(elapsed_ms(t0), Ordering::Relaxed);
    BUSY.store(false, Ordering::Release);
    if !was_on {
        x86_64::instructions::interrupts::disable();
    }
    res
}

fn new_faults(regs: &Bar0, start: &Faults) -> Option<Faults> {
    // Supervisors are expected here: only errors and exceptions count.
    let n = Faults::read(regs).new_since(start);
    (n.ctrl_disp & 0x80 != 0 || n.exc_other != 0 || n.exc_win != 0 || n.exc_winim != 0).then_some(n)
}

/// Frames per second of `head` by counting the raster's vline wrapping,
/// over ~300 ms (mHz). The instrument: the same reading on head 0 is
/// logged beside it.
fn measure_mhz(regs: &Bar0, heads: [u32; 2]) -> [u64; 2] {
    let t0 = crate::cpu::tsc::read();
    let limit = crate::cpu::tsc::freq_hz() * 3 / 10;
    let mut last = [vb::scan_position(regs, heads[0]).0, vb::scan_position(regs, heads[1]).0];
    let mut frames = [0u64; 2];
    let mut t_end = t0;
    while crate::cpu::tsc::read().wrapping_sub(t0) < limit {
        for i in 0..2 {
            let v = vb::scan_position(regs, heads[i]).0;
            if v < last[i] {
                frames[i] += 1;
            }
            last[i] = v;
        }
        t_end = crate::cpu::tsc::read();
        core::hint::spin_loop();
    }
    let ns = (t_end.wrapping_sub(t0)) * 1_000_000 / (crate::cpu::tsc::freq_hz() / 1000).max(1);
    // frames / (ns * 1e-9) Hz = frames * 1e12 / ns mHz... in mHz: * 1e3.
    [frames[0] * 1_000_000_000_000 / ns.max(1), frames[1] * 1_000_000_000_000 / ns.max(1)]
}

/// `/dev/dispctl` `hdmi on`.
pub fn on() -> Result<(), HdmiError> {
    let st = STATE.get().ok_or(HdmiError::NotReady)?;
    let regs = svr::regs().ok_or(HdmiError::NotReady)?;
    locked(|| {
        if ON.load(Ordering::Acquire) {
            return Err(HdmiError::State);
        }
        ONS.fetch_add(1, Ordering::Relaxed);
        let t0 = crate::cpu::tsc::read();
        let start = Faults::read(regs);
        let (w, h) = st.mode.size();
        let t = st.mode.timing(0);
        report(alloc::format!("hdmi: on: {} '{}' {}x{} {} kHz", st.conn, st.monitor, w, h, st.mode.clock_khz));

        // 1. SOR power.
        if !nh::sor_powered(regs, st.sor) && !nh::sor_power_up(regs, st.sor) {
            report(alloc::format!("hdmi: SOR-{} power sequencer did not settle (status {:#x})", st.sor, regs.rd32(nh::SOR_POWER_STATUS + st.sor as u32 * 0x800)));
        }
        // 2. Window 2's channel, once.
        if !CHAN_UP.load(Ordering::Acquire) {
            if let Err(e) = evo::init_channel(regs, st.window, st.push) {
                return Err(fail("on", alloc::format!("window {} channel: {:?}", st.window.user - 1, e)));
            }
            CHAN_UP.store(true, Ordering::Release);
            report(alloc::format!("hdmi: window {} up: ctl {:#x} status {:#x}", st.window.user - 1, regs.rd32(st.window.control()), regs.rd32(st.window.status().0)));
        }
        // 3. Route and encoder.
        nh::route_set(regs, st.outp.or, st.outp.link, Some(st.sor), st.outp.link);
        let high = nh::enable(regs, st.head, st.sor, &st.mode);
        report(alloc::format!(
            "hdmi: route {:#x}, encoder: ctrl {:#x} scdc {:#x} avi {:#x} high_speed {}",
            regs.rd32(0x61_2408),
            regs.rd32(0x61_65c0 + st.head * 0x800),
            regs.rd32(0x61_c5bc + st.sor as u32 * 0x800),
            regs.rd32(0x6f_0000 + st.head * 0x400),
            high
        ));
        // 4. The head, the SOR and the window's owner in one core UPDATE.
        svr::own_head(st.head, st.sor, st.outp);
        let methods = nh::head_methods(st.head, st.sor as u32, st.outp.dcb, nh::head_window(st.head), &st.mode);
        wait(svr::core_idle);
        let before = svr::supers_done();
        if let Err(e) = svr::push_core("HdmiOn", &methods, true) {
            svr::disown_head(st.head, st.sor);
            return Err(match e {
                RequestError::Busy => HdmiError::Busy,
                e => fail("on", alloc::format!("core push: {:?}", e)),
            });
        }
        let attached = wait(|| {
            svr::supers_done() > before
                && svr::core_idle()
                && SorState::read(regs, st.sor as u32, true).head & (1 << st.head) != 0
                && HeadTiming::read_armed(regs, st.head).hz == st.mode.clock_khz * 1000
        });
        let now = HeadTiming::read_armed(regs, st.head);
        let want = HeadTiming { depth_code: now.depth_code, ..t };
        report(alloc::format!(
            "hdmi: core UPDATE {} after {} ms: SOR-{} ARMED {:#x} head {} ARMED {:?} (want {:?}); vpll1 {:#x} {:#x}",
            if attached { "latched" } else { "NOT latched" },
            elapsed_ms(t0),
            st.sor,
            regs.rd32(evo::CORE.armed_base() + nh::sor_set_control(st.sor as u32)),
            st.head,
            now,
            want,
            regs.rd32(0xef18 + st.head * 0x40),
            regs.rd32(0xef04 + st.head * 0x40)
        ));
        if !attached || now != want {
            return Err(off_after(st, regs, "the head did not take the mode"));
        }
        if let Some(n) = new_faults(regs, &start) {
            return Err(off_after(st, regs, &alloc::format!("new faults after the core UPDATE: {:?}", n)));
        }
        // 5. The surface.
        let mut put = PUT.load(Ordering::Relaxed);
        let mut push = Push::new(st.push, put);
        if push.room_words() < 64 {
            if let Err(e) = evo::wind(regs, st.window, &mut push) {
                return Err(off_after(st, regs, &alloc::format!("window push wind: {:?}", e)));
            }
        }
        // Is head 1's raster running? (The window's UPDATE latches at its vblank.)
        let v0 = vb::scan_position(regs, st.head);
        let tv = crate::cpu::tsc::read();
        wait(|| elapsed_ms(tv) >= 10);
        let v1 = vb::scan_position(regs, st.head);
        report(alloc::format!("hdmi: head {} raster before the window push: {:?} -> {:?} after 10 ms", st.head, v0, v1));
        // nouveau's first UPDATE of a newly owned window is interlocked with
        // the core's (Ryzen #89: an independent one raised INVALID_STATE):
        // the window's state and UPDATE with `SET_INTERLOCK_FLAGS = 1` and
        // its window bit, not waited on; then the core UPDATE with the same
        // window bit, which releases both.
        let wbit = 1u32 << (st.window.user - 1);
        let mut wm = nh::window_methods(w, h, st.pitch, evo::HANDLE_WNDW_CTX, SURF_VRAM, wbit);
        wm.extend(nh::window_lut_methods());
        let res = wm.iter().try_for_each(|&(m, v)| push.mthd(m, &[v])).and_then(|_| evo::push_update(&mut push));
        if let Err(e) = res {
            return Err(off_after(st, regs, &alloc::format!("window push: {:?}", e)));
        }
        evo::submit(regs, st.window, &push);
        put = push.put_bytes();
        PUT.store(put, Ordering::Relaxed);
        wait(svr::core_idle);
        let mut core_methods = nh::olut_methods(st.head).to_vec();
        core_methods.extend(nh::core_interlock_methods(wbit));
        if let Err(e) = svr::push_core("HdmiWindow", &core_methods, true) {
            window_diag(st, regs, &wm, &start);
            return Err(off_after(st, regs, &alloc::format!("core interlock push: {:?}", e)));
        }
        let done = wait(|| regs.rd32(st.window.get()) == put && st.window.idle(regs) && svr::core_idle());
        if !done {
            window_diag(st, regs, &wm, &start);
            return Err(off_after(st, regs, &alloc::format!("window {} did not go idle (status {:#x})", st.window.user - 1, regs.rd32(st.window.status().0))));
        }
        let origin = (SURF_VRAM >> 8) as u32;
        let latched = evo::wait(regs, || {
            regs.rd32(st.window.armed_base() + evo::WNDW_SET_OFFSET0) == origin
                && regs.rd32(st.window.armed_base() + evo::WNDW_SET_CONTEXT_DMA_ISO0) == evo::HANDLE_WNDW_CTX
        });
        report(alloc::format!(
            "hdmi: window {} UPDATE {}: ARMED offset {:#x} ctxdma {:#x} size {:#x}, put {:#x}",
            st.window.user - 1,
            if latched { "latched" } else { "NOT latched" },
            regs.rd32(st.window.armed_base() + evo::WNDW_SET_OFFSET0),
            regs.rd32(st.window.armed_base() + evo::WNDW_SET_CONTEXT_DMA_ISO0),
            regs.rd32(st.window.armed_base() + 0x224),
            put
        ));
        if !latched {
            return Err(off_after(st, regs, "the window's UPDATE did not latch"));
        }
        // 6. Evidence: faults, and the refresh by the raster.
        let t50 = crate::cpu::tsc::read();
        wait(|| elapsed_ms(t50) >= 50);
        let faults = new_faults(regs, &start);
        let m = measure_mhz(regs, [st.head, svr::disp().map_or(0, |d| d.head)]);
        REFRESH_MHZ.store(m[0], Ordering::Relaxed);
        report(alloc::format!(
            "hdmi: head {} raster {}.{:03} Hz (nominal {}.{:03}), head 0 by the same instrument {}.{:03} Hz; faults {:?}; exception slot window {:#x}",
            st.head,
            m[0] / 1000,
            m[0] % 1000,
            st.mode.refresh_mhz() / 1000,
            st.mode.refresh_mhz() % 1000,
            m[1] / 1000,
            m[1] % 1000,
            faults,
            regs.rd32(st.window.exception())
        ));
        if let Some(n) = faults {
            return Err(off_after(st, regs, &alloc::format!("new faults: {:?}", n)));
        }
        ON.store(true, Ordering::Release);
        OK.fetch_add(1, Ordering::Relaxed);
        report(alloc::format!("hdmi: on OK in {} ms", elapsed_ms(t0)));
        Ok(())
    })
}

/// Why the window did not go idle: its state registers, the exception slot,
/// the display's fault bits (all polled, never acknowledged) and which of the
/// pushed methods its ASSEMBLY holds.
fn window_diag(st: &State, regs: &Bar0, pushed: &[(u32, u32)], start: &Faults) {
    let c = st.window;
    let x = c.exception();
    report(alloc::format!(
        "hdmi: window {} diag: ctl {:#x} status {:#x} put {:#x} get {:#x}; exception slot {:#x} {:#x} {:#x}; faults now {:?} (before {:?}); DISP_INTR {:#x}",
        c.user - 1,
        regs.rd32(c.control()),
        regs.rd32(c.status().0),
        regs.rd32(c.put()),
        regs.rd32(c.get()),
        regs.rd32(x),
        regs.rd32(x + 4),
        regs.rd32(x + 8),
        Faults::read(regs),
        start,
        regs.rd32(vb::DISP_INTR)
    ));
    let mut line = String::from("hdmi: window ASSEMBLY vs pushed:");
    for &(m, v) in pushed {
        let a = regs.rd32(c.user_base() + m);
        if a != v {
            let _ = write!(line, " {:03x} pushed {:#x} has {:#x};", m, v, a);
        }
    }
    let _ = write!(line, " | ARMED offset {:#x} ctxdma {:#x} size {:#x}", regs.rd32(c.armed_base() + 0x260), regs.rd32(c.armed_base() + 0x240), regs.rd32(c.armed_base() + 0x224));
    report(line);
    let core = evo::CORE;
    report(alloc::format!(
        "hdmi: core ARMED window usage bounds(2) {:#x} {:#x} {:#x}, WINDOW_SET_CONTROL(2) {:#x}, head {} usage {:#x}, core exception slot {:#x}",
        regs.rd32(core.armed_base() + 0x1104),
        regs.rd32(core.armed_base() + 0x1108),
        regs.rd32(core.armed_base() + 0x1110),
        regs.rd32(core.armed_base() + 0x1100),
        st.head,
        regs.rd32(core.armed_base() + 0x2030 + st.head * 0x400),
        regs.rd32(core.exception())
    ));
}

/// A failed `on`: switch it off again, report.
fn off_after(st: &State, regs: &Bar0, why: &str) -> HdmiError {
    let e = fail("on", String::from(why));
    let _ = off_locked(st, regs);
    e
}

/// `/dev/dispctl` `hdmi off`.
pub fn off() -> Result<(), HdmiError> {
    let st = STATE.get().ok_or(HdmiError::NotReady)?;
    let regs = svr::regs().ok_or(HdmiError::NotReady)?;
    locked(|| {
        if !ON.load(Ordering::Acquire) {
            return Err(HdmiError::State);
        }
        off_locked(st, regs)
    })
}

/// The encoder off, the SOR detached and the head's display id cleared in
/// one core UPDATE (nouveau's round 1 for head 0), then wait for the
/// supervisors' OffInt scripts.
fn off_locked(st: &State, regs: &Bar0) -> Result<(), HdmiError> {
    let t0 = crate::cpu::tsc::read();
    nh::disable(regs, st.head);
    let methods = nh::head_off_methods(st.head, st.sor as u32);
    wait(svr::core_idle);
    let before = svr::supers_done();
    if let Err(e) = svr::push_core("HdmiOff", &methods, false) {
        report(alloc::format!("hdmi: off: core push refused: {:?}", e));
        return Err(HdmiError::Failed);
    }
    let done = wait(|| svr::supers_done() > before && svr::core_idle() && SorState::read(regs, st.sor as u32, true).head == 0);
    svr::disown_head(st.head, st.sor);
    ON.store(false, Ordering::Release);
    report(alloc::format!(
        "hdmi: off {} in {} ms: SOR-{} ARMED {:#x}",
        if done { "OK" } else { "NOT confirmed" },
        elapsed_ms(t0),
        st.sor,
        regs.rd32(evo::CORE.armed_base() + nh::sor_set_control(st.sor as u32))
    ));
    if done {
        Ok(())
    } else {
        Err(HdmiError::Failed)
    }
}

/// `/dev/dispctl` read, after the mode line.
pub fn status() -> String {
    let Some(st) = STATE.get() else { return String::new() };
    alloc::format!(
        "hdmi: {} '{}' head {} SOR-{} window {} {} mode {}x{} {} kHz\n",
        st.conn,
        st.monitor,
        st.head,
        st.sor,
        st.window.user - 1,
        if ON.load(Ordering::Relaxed) { "on" } else { "off" },
        st.mode.size().0,
        st.mode.size().1,
        st.mode.clock_khz
    )
}

/// `/proc/kdebug` line.
pub fn render_kdebug() -> String {
    if STATE.get().is_none() {
        return String::from("gpu_hdmi: off");
    }
    alloc::format!(
        "gpu_hdmi: enabled=1 on={} ons={} ok={} failed={} last_ms={} refresh_mhz={}",
        ON.load(Ordering::Relaxed) as u8,
        ONS.load(Ordering::Relaxed),
        OK.load(Ordering::Relaxed),
        FAILED.load(Ordering::Relaxed),
        LAST_MS.load(Ordering::Relaxed),
        REFRESH_MHZ.load(Ordering::Relaxed)
    )
}
