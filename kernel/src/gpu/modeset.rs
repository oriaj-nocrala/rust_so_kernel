// kernel/src/gpu/modeset.rs
//
// `gpu=modes` (phase 5.7 of docs/gpu/gpu-plan.md): sets the primary head to
// another mode of the same size, e.g. the ASUS VG279Q3A's 1920x1080 at
// 180 Hz. The modes are `nvgpu::mode`'s (the monitor's EDID detailed
// timings, else CVT-RB2 inside its range); the pieces that change a mode
// are the earlier subphases': supervisors (5.4), VPLL (5.5), DP link
// training (5.6).
//
// - Boot (`setup`, IF=0, before the APs): the EDID and DPCD `gpu=disp` read
//   from the primary head's output (same AUX channel), its modes, the size
//   the head scans out now (the only one accepted: window 0, the
//   framebuffer, the console and the compositor keep theirs). Logged as
//   `modes:` lines. Reads only.
// - `/dev/dispctl` `mode WxH@Hz` (`set`, any CPU, IF=1, synchronous):
//   everything is checked before the first push (the mode, its size, the
//   VPLL coefficients, a DP link that carries it and the packing on it).
//   If the current link cannot carry the mode, nouveau's order: `detach`
//   and wait for a new supervisor 3 (the SOR drives nothing), retrain the
//   link to the sink's and board's maximum (`dplink::train`). Then (or
//   directly, with the SOR still attached, when the link carries it: the
//   hardware runs 2.0/2.1/2.2 for a raster/clock change by itself, as
//   `clock` showed in 5.5) push the mode's head methods + the SOR control
//   + UPDATE (`Cmd::Mode`); wait for supervisor 3 (2.1 programs the VPLL,
//   2.2 the packing), and compare the ARMED raster and clock with the
//   mode's. Waits are bounded (TSC); the supervisors run in the vblank ISR
//   on CPU 0, so `set` runs with IF=1 (see there). Avoiding the detach
//   saves a blink: while detached the head raises no vblank (Ryzen #83,
//   #84: the sequence stood still, pending flips never completed).
// - A failure after the detach tries to put back what was there (retrain
//   the old link if it was changed, attach at the old mode) and reports
//   EIO. Only one `set` at a time (`BUSY`).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use nvgpu::aux::RECEIVER_CAP_SIZE;
use nvgpu::display::Path;
use nvgpu::edid::Edid;
use nvgpu::mode::{self, Mode, ModeError};
use nvgpu::supervisor::{self as sup, DpLink};
use nvgpu::vblank::HeadTiming;
use nvgpu::Mmio;

use diag::IrqMutex;

use crate::allocator::KernelIrq;

use super::supervisor::{self as svr, Cmd, RequestError};
use super::Bar0;

struct State {
    edid: Edid,
    dpcd: [u8; RECEIVER_CAP_SIZE],
    /// The head's visible size at boot: the only one `set` accepts.
    size: (u16, u16),
    /// The connector's name (`DP-3`).
    name: String,
    /// The mode the GOP set, read back from the head at boot: what a
    /// failed `set` puts back before any other succeeded.
    gop: Mode,
}

static STATE: spin::Once<State> = spin::Once::new();
/// The mode the last successful `set` put on the head (`None`: the GOP's).
static CURRENT: IrqMutex<Option<Mode>, KernelIrq> = IrqMutex::new(None);
static BUSY: AtomicBool = AtomicBool::new(false);

static SETS: AtomicU64 = AtomicU64::new(0);
static OK: AtomicU64 = AtomicU64::new(0);
static FAILED: AtomicU64 = AtomicU64::new(0);
static REFUSED: AtomicU64 = AtomicU64::new(0);
static RETRAINS: AtomicU64 = AtomicU64::new(0);
static LAST_MS: AtomicU64 = AtomicU64::new(0);

/// Refresh rates the boot log lists as CVT-RB2 (those inside the range).
const CVT_LISTED: [u32; 8] = [48, 60, 75, 100, 120, 144, 165, 180];
/// Bound on each wait for the supervisors (~30 ms, Ryzen #77).
const WAIT_MS: u64 = 1000;

/// `HEAD_SET_CONTROL_OUTPUT_RESOURCE(head)` in the core's ARMED state.
fn or_armed(regs: &Bar0, head: u32) -> u32 {
    regs.rd32(nvgpu::evo::CORE.armed_base() + mode::HEAD_SET_CONTROL_OUTPUT_RESOURCE + head * 0x400)
}

fn describe(m: &Mode) -> String {
    let mhz = m.refresh_mhz();
    alloc::format!(
        "{}x{}@{}.{:03} {:?} clock {} kHz h {}/{}/{}/{} v {}/{}/{}/{} {}H{}V",
        m.hdisplay,
        m.vdisplay,
        mhz / 1000,
        mhz % 1000,
        m.source,
        m.clock_khz,
        m.hdisplay,
        m.hsync_start,
        m.hsync_end,
        m.htotal,
        m.vdisplay,
        m.vsync_start,
        m.vsync_end,
        m.vtotal,
        if m.nhsync { '-' } else { '+' },
        if m.nvsync { '-' } else { '+' },
    )
}

/// Boot, `gpu=modes`, after `supervisor::setup`.
pub fn setup(r: &mut String, regs: &Bar0) {
    let Some(d) = svr::disp_at_boot() else {
        let _ = writeln!(r, "modes: STOP: the supervisors are not set up (see super:)");
        return;
    };
    if !super::dplink::ready() {
        let _ = writeln!(r, "modes: STOP: no DP link training (see dplink:)");
        return;
    }
    let probe = super::PROBES.get().and_then(|ps| {
        ps.iter().find(|p| matches!(p.conn.path, Some(Path::Aux { ch, .. }) if Some(ch) == d.outp.aux))
    });
    let Some(p) = probe else {
        let _ = writeln!(r, "modes: STOP: no connector probed on AUX {:?}", d.outp.aux);
        return;
    };
    let (Some(dpcd), Ok(edid)) = (p.dpcd, Edid::parse(&p.edid)) else {
        let _ = writeln!(r, "modes: STOP: {} has no DPCD or no valid EDID ({} bytes)", p.conn.name, p.edid.len());
        return;
    };
    let t = HeadTiming::read_armed(regs, d.head);
    let size = t.visible();
    let link = DpLink::read(regs, d.sor as u32, d.sublink);
    let max = nvgpu::dp::max_config(&dpcd, d.board_nr, d.board_bw);
    let _ = writeln!(
        r,
        "modes: {} '{}' range {:?}; head {} {}x{} now; link now {:?}, max {:?}",
        p.conn.name,
        edid.name.as_deref().unwrap_or("?"),
        edid.range,
        d.head,
        size.0,
        size.1,
        link,
        max
    );
    let vpll = super::VBIOS.get().and_then(|(bios, _)| nvgpu::pll::parse(bios, nvgpu::pll::PLL_VPLL0 + d.head as u8).ok());
    let mut list: Vec<Mode> = mode::edid_modes(&edid);
    for hz in CVT_LISTED {
        if let Ok(m) = mode::select(&edid, size.0, size.1, hz) {
            if m.source == mode::Source::Cvt {
                list.push(m);
            }
        }
    }
    for m in &list {
        let fits = if m.size() != size { "other size: not accepted" } else { "" };
        let _ = writeln!(
            r,
            "modes: {} {}; vpll {:?}; packing on {:?}: {:?}",
            describe(m),
            fits,
            vpll.map(|l| nvgpu::pll::calc(&l, m.clock_khz)),
            max,
            max.and_then(|c| sup::dp_config(&m.timing(t.depth_code), &DpLink { bw: c.bw, nr: c.nr, ef: true, mst: false }))
        );
    }
    let gop = Mode::from_head(&t, or_armed(regs, d.head));
    let _ = writeln!(r, "modes: the GOP's: {}", describe(&gop));
    STATE.call_once(|| State { edid, dpcd, size, name: p.conn.name.clone(), gop });
    let _ = writeln!(r, "modes: ready: /dev/dispctl mode WxH@Hz (size {}x{})", size.0, size.1);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetError {
    NotReady,
    /// Another `set`, a push pending, or a training.
    Busy,
    /// Refused before touching anything (syntax, no such mode, other size,
    /// no VPLL coefficients, no link carries it).
    Invalid,
    /// Something failed after the detach (details in /proc/gpu).
    Failed,
}

fn report(line: String) {
    crate::serial_println!("{}", line);
    svr::log_line(line);
}

fn elapsed_ms(t0: u64) -> u64 {
    crate::cpu::tsc::read().wrapping_sub(t0) / (crate::cpu::tsc::freq_hz() / 1000).max(1)
}

/// Busy-waits (bounded) until `done`; the supervisors that make it true run
/// in the vblank ISR on CPU 0, so this needs IF=1 if it runs there.
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

/// Pushes a dispctl command once the core channel has fetched the
/// previous push (bounded wait; each refused `request` would be logged).
fn push(cmd: Cmd) -> Result<(), RequestError> {
    wait(svr::core_idle);
    svr::request(cmd).map(|_| ())
}

/// `/dev/dispctl` `mode WxH@Hz`.
pub fn set(req: &str) -> Result<(), SetError> {
    if BUSY.swap(true, Ordering::Acquire) {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        return Err(SetError::Busy);
    }
    // The supervisors this waits for arrive by MSI on CPU 0. A syscall
    // enters with IF=0 (`IA32_FMASK`) and this one may still be at IF=0 on
    // CPU 0: then no MSI is taken until it returns and every wait times out
    // (Ryzen #84: "IF=0"; #83's "1 s late" was the timeout). Nothing here
    // holds a lock or a `cpu_id()` across the waits, so run with IF=1 and
    // put the caller's state back.
    let was_on = x86_64::instructions::interrupts::are_enabled();
    if !was_on {
        x86_64::instructions::interrupts::enable();
    }
    let t0 = crate::cpu::tsc::read();
    let res = set_locked(req);
    LAST_MS.store(elapsed_ms(t0), Ordering::Relaxed);
    BUSY.store(false, Ordering::Release);
    if !was_on {
        x86_64::instructions::interrupts::disable();
    }
    res
}

fn refuse(req: &str, why: String) -> SetError {
    REFUSED.fetch_add(1, Ordering::Relaxed);
    report(alloc::format!("mode: {} refused: {}", req, why));
    SetError::Invalid
}

fn set_locked(req: &str) -> Result<(), SetError> {
    let st = STATE.get().ok_or(SetError::NotReady)?;
    let d = svr::disp().ok_or(SetError::NotReady)?;
    let (bios, _) = super::VBIOS.get().ok_or(SetError::NotReady)?;
    let regs = &d.regs;

    // Everything checked before the first push.
    let (w, h, hz) = mode::parse_request(req).map_err(|_| refuse(req, String::from("not WxH@Hz")))?;
    let m = mode::select(&st.edid, w, h, hz).map_err(|e| match e {
        ModeError::OutOfRange => refuse(req, alloc::format!("not an EDID mode, and CVT-RB2 is outside {:?}", st.edid.range)),
        ModeError::Syntax => refuse(req, String::from("not WxH@Hz")),
    })?;
    if m.size() != st.size {
        return Err(refuse(req, alloc::format!("{}: a size change is not ported (window, framebuffer, console)", describe(&m))));
    }
    let limits = nvgpu::pll::parse(bios, nvgpu::pll::PLL_VPLL0 + d.head as u8).map_err(|e| refuse(req, alloc::format!("no VPLL limits: {:?}", e)))?;
    let coeffs = nvgpu::pll::calc(&limits, m.clock_khz).map_err(|e| refuse(req, alloc::format!("{}: VPLL {:?}", describe(&m), e)))?;
    let armed = HeadTiming::read_armed(regs, d.head);
    let t = m.timing(armed.depth_code);
    let old_link = DpLink::read(regs, d.sor as u32, d.sublink).ok_or_else(|| refuse(req, String::from("the SOR runs no DP link")))?;
    let carries = |l: &DpLink| svr::link_max_khz(l, t.depth_bits()) >= m.clock_khz && sup::dp_config(&t, l).is_some();
    let retrain = if carries(&old_link) {
        None
    } else {
        let c = nvgpu::dp::max_config(&st.dpcd, d.board_nr, d.board_bw).ok_or_else(|| refuse(req, String::from("no link the sink and board run")))?;
        let l = DpLink { bw: c.bw, nr: c.nr, ef: old_link.ef, mst: false };
        if !carries(&l) {
            return Err(refuse(req, alloc::format!("{}: not even {}x{:#x} carries it", describe(&m), c.nr, c.bw)));
        }
        Some(c)
    };
    let old_methods = CURRENT.with(|c| *c).unwrap_or(st.gop).methods(d.head, or_armed(regs, d.head));
    let methods = m.methods(d.head, or_armed(regs, d.head));
    SETS.fetch_add(1, Ordering::Relaxed);
    report(alloc::format!(
        "mode: {} = {}; vpll {:?}; link {}x{:#x}{}",
        req,
        describe(&m),
        coeffs,
        old_link.nr,
        old_link.bw,
        match retrain {
            Some(c) => alloc::format!(" -> retrain {}x{:#x}", c.nr, c.bw),
            None => String::from(" (carries it)"),
        }
    ));

    // 1. Only to retrain: detach, and wait for its supervisor 3 (a new one:
    // `LINK_FREE` may be left over from an earlier detach).
    let t0 = crate::cpu::tsc::read();
    if retrain.is_some() {
        let before = svr::supers_done();
        match push(Cmd::Detach) {
            Ok(()) => {}
            Err(RequestError::Busy) => return Err(SetError::Busy),
            Err(e) => return Err(fail(req, alloc::format!("detach: {:?}", e), None)),
        }
        if !wait(|| svr::supers_done() > before && svr::link_free()) {
            let if_on = x86_64::instructions::interrupts::are_enabled();
            return Err(fail(req, alloc::format!("the SOR is not free {} ms after detach (IF={})", WAIT_MS, if_on as u8), Some((None, old_methods))));
        }
    }
    let detached_ms = elapsed_ms(t0);

    // 2. The link, if the mode needs a faster one.
    if let Some(c) = retrain {
        RETRAINS.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = super::dplink::train(c.nr, c.bw) {
            return Err(fail(req, alloc::format!("train {}x{:#x}: {:?}", c.nr, c.bw, e), Some((Some(old_link), old_methods))));
        }
    }
    let trained_ms = elapsed_ms(t0);

    // 3. The mode and the attach in one UPDATE; wait for supervisor 3.
    let before = svr::supers_done();
    // The mode already ARMED and attached (e.g. 60 Hz asked at 60 Hz): the
    // UPDATE changes nothing and raises no supervisor.
    let unchanged = retrain.is_none() && HeadTiming::read_armed(regs, d.head) == t && regs.rd32(nvgpu::evo::CORE.armed_base() + 0x300 + d.sor as u32 * 0x20) == d.ctrl;
    if let Err(e) = push(Cmd::Mode(methods)) {
        return Err(fail(req, alloc::format!("mode push: {:?}", e), Some((retrain.map(|_| old_link), old_methods))));
    }
    let clock_hz = m.clock_khz * 1000;
    let attached = wait(|| (unchanged || svr::supers_done() > before) && HeadTiming::read_armed(regs, d.head).hz == clock_hz && svr::core_idle());
    let now = HeadTiming::read_armed(regs, d.head);
    if !attached || now != t {
        return Err(fail(req, alloc::format!("ARMED after the attach is {:?}, not {:?}", now, t), None));
    }
    CURRENT.with(|c| *c = Some(m));
    OK.fetch_add(1, Ordering::Relaxed);
    report(alloc::format!(
        "mode: {} OK in {} ms (detached {} ms, trained {} ms); ARMED {:?}; link {:?}; vpll {:#x} {:#x}",
        req,
        elapsed_ms(t0),
        detached_ms,
        trained_ms,
        now,
        DpLink::read(regs, d.sor as u32, d.sublink),
        regs.rd32(0xef18 + d.head * 0x40),
        regs.rd32(0xef04 + d.head * 0x40)
    ));
    Ok(())
}

/// What `fail` puts back: the link to retrain first (if it was changed)
/// and the old mode's head methods.
type Restore = (Option<DpLink>, [(u32, u32); 9]);

fn fail(req: &str, why: String, restore: Option<Restore>) -> SetError {
    FAILED.fetch_add(1, Ordering::Relaxed);
    report(alloc::format!("mode: {} FAILED: {}", req, why));
    if let Some((link, methods)) = restore {
        if let Some(l) = link {
            let r = super::dplink::train(l.nr, l.bw);
            report(alloc::format!("mode: restore: train {}x{:#x}: {:?}", l.nr, l.bw, r));
        }
        let r = push(Cmd::Mode(methods));
        report(alloc::format!("mode: restore: old mode + attach: {:?}", r));
    }
    SetError::Failed
}

/// `/dev/dispctl` read, after the supervisor's status line.
pub fn status() -> String {
    let Some(st) = STATE.get() else { return String::new() };
    let cur = CURRENT.with(|c| *c);
    alloc::format!(
        "mode: {} {} (size {}x{})\n",
        st.name,
        cur.map_or(String::from("the GOP's"), |m| describe(&m)),
        st.size.0,
        st.size.1
    )
}

/// `/proc/kdebug` line.
pub fn render_kdebug() -> String {
    if STATE.get().is_none() {
        return String::from("gpu_mode: off");
    }
    let cur = CURRENT.with(|c| *c);
    alloc::format!(
        "gpu_mode: enabled=1 sets={} ok={} failed={} refused={} retrains={} last_ms={} current_mhz={}",
        SETS.load(Ordering::Relaxed),
        OK.load(Ordering::Relaxed),
        FAILED.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
        RETRAINS.load(Ordering::Relaxed),
        LAST_MS.load(Ordering::Relaxed),
        cur.map_or(0, |m| m.refresh_mhz())
    )
}
