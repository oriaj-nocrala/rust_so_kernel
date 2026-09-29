// kernel/src/gpu/supervisor.rs
//
// `gpu=super` (phase 5.4 of docs/gpu/gpu-plan.md): the display's supervisor
// interrupts, and `/dev/dispctl` to exercise them. The work is
// `nvgpu::supervisor` (host-tested against both rounds of nouveau's
// modeset); this module owns its state, runs it from the vblank MSI handler
// and pushes the core UPDATEs that ask for it.
//
// Boot (IF=0, before the APs, after the channels are up and before vblank
// is armed): find the primary head (the lowest lit), the SOR the GOP
// attached to it and the output routed to that SOR (pad routing, as nouveau
// reads it at init), log the DP link the GOP trained and the IED scripts a
// detach/attach would run, then enable the supervisor interrupts (CTRL_DISP
// MSK/EN = supervisors only).
//
// Runtime:
// - `/dev/dispctl` `detach` / `attach` (any CPU, IF=1): one core push,
//   `SOR_SET_CONTROL(sor)` = 0 or the GOP's value, then UPDATE; not waited
//   on. With `gpu=vpll` (phase 5.5) also `clock <kHz>`:
//   `HEAD_SET_PIXEL_CLOCK_FREQUENCY(_MAX)(head)`, then UPDATE; supervisor
//   2.1 programs the VPLL. Bounded to the current raster at >= 48 Hz (the
//   ASUS's floor) and to what the DP link carries (the GOP's, or the last
//   one `dplink.rs` trained). With `gpu=modes` (phase 5.7), `Cmd::Mode`
//   (from `modeset.rs`): a whole mode's head methods and the attach in one
//   UPDATE. Refused (EAGAIN) while the core channel has not finished the
//   last one or a link training runs. Under `PUSH_AT`, the only user of
//   the core push buffer after boot.
// - Link training (`gpu=dplink`, `dplink.rs`) needs the SOR detached: the
//   ISR records at each supervisor 3 whether the head's SOR drives nothing
//   (`LINK_FREE`); a request that can attach it clears that, and both
//   checks happen under `PUSH_AT`, so no attach is pushed while a training
//   runs (`TRAINING`) and no training starts while one is pending.
// - The display then raises supervisors 1, 2, 3, each by MSI to CPU 0:
//   `vblank::on_msi` acknowledges it (`nvgpu::vblank::service`) and calls
//   `on_pending`, which does the work and releases the display. Global
//   work, ISR context. It touches head/SOR registers and the supervisor
//   ones; page flips touch only window 0's, the dispctl push only the core
//   PUT/GET: BAR0 needs no lock.
// - Each supervisor is logged (klog, lock-free) and kept in `LOG` for
//   /proc/gpu; counters in /proc/kdebug (`gpu_super:`).

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use diag::IrqMutex;
use nvgpu::evo::{self, Push};
use nvgpu::supervisor::{self as sup, Config, Event, Outp, Proto, SorState, Supervisor};
use nvgpu::vblank::{HeadTiming, HEADS_MAX};
use nvgpu::Mmio;

use crate::allocator::KernelIrq;

use super::Bar0;

/// `SOR_SET_CONTROL(a)` (`clc67d.h:299`).
fn sor_set_control(sor: u8) -> u32 {
    0x300 + sor as u32 * 0x20
}

/// `HEAD_SET_PIXEL_CLOCK_FREQUENCY(a)` and `_MAX(a)` (`clc67d.h:691,737`):
/// hertz in bits 0-30; nouveau pushes both with the mode's clock
/// (`dispnv50/headc57d.c:232-236`).
fn head_pixel_clock(head: u32) -> (u32, u32) {
    (0x200c + head * 0x400, 0x2028 + head * 0x400)
}

/// Lowest refresh `clock` may ask for: the ASUS VG279Q3A's range starts at
/// 48 Hz (EDID, "Hechos medidos" of the plan).
const MIN_REFRESH_HZ: u64 = 48;

/// What `setup` found: the head and SOR `/dev/dispctl` detaches and
/// re-attaches, and the SOR control value the GOP left.
pub(super) struct Disp {
    pub(super) regs: Bar0,
    pub(super) head: u32,
    pub(super) sor: u8,
    /// The sublink the SOR runs (1 = A, 2 = B) and the DP output routed
    /// to it.
    pub(super) sublink: u8,
    pub(super) outp: Outp,
    /// The DCB's limits for that output (`dpconf`: lanes, rate).
    pub(super) board_nr: u8,
    pub(super) board_bw: u8,
    pub(super) ctrl: u32,
    /// `gpu=vpll`: 2.1 programs VPLLs and `clock` is accepted.
    clocks: bool,
    /// `gpu=dplink`: `train` is accepted.
    pub(super) train: bool,
    /// The pixel clock the GOP set (Hz). `clock` accepts the current
    /// raster at >= 48 Hz up to `MAX_KHZ`.
    gop_hz: u32,
}

/// What the DP link carries (kHz of pixel clock): the GOP's link at boot,
/// then the last one `dplink.rs` trained.
static MAX_KHZ: AtomicU32 = AtomicU32::new(0);
/// Set by the ISR at each supervisor 3: the primary head's SOR drives no
/// head any more (a detach completed). Cleared by requests that may attach.
static LINK_FREE: AtomicBool = AtomicBool::new(false);
/// A link training runs: dispctl pushes are refused.
static TRAINING: AtomicBool = AtomicBool::new(false);

static DISP: spin::Once<Disp> = spin::Once::new();
/// The supervisor state (kept from supervisor 1 to 3). Taken by the MSI
/// handler; `render_kdebug` does not take it.
static SUPER: IrqMutex<Option<Supervisor>, KernelIrq> = IrqMutex::new(None);
/// Where the core channel's next push goes (bytes).
static PUSH_AT: IrqMutex<u32, KernelIrq> = IrqMutex::new(0);
static ENABLED: AtomicBool = AtomicBool::new(false);
/// The core's ASSEMBLY window interlock flags are non-zero (`push_core`).
static WINDOW_INTERLOCK: AtomicBool = AtomicBool::new(false);

/// Recent supervisor reports and dispctl requests, for /proc/gpu.
static LOG: IrqMutex<VecDeque<String>, KernelIrq> = IrqMutex::new(VecDeque::new());
const LOG_LINES: usize = 64;

static SERVICED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static SCRIPT_ERRORS: AtomicU64 = AtomicU64::new(0);
/// Events this phase reports without doing the work (2.1 clock, non-DP
/// attach, no output/script).
static NOT_DONE: AtomicU64 = AtomicU64::new(0);
static CTRL_DISP_ERRORS: AtomicU64 = AtomicU64::new(0);
static REQUESTS: AtomicU64 = AtomicU64::new(0);
static REFUSED: AtomicU64 = AtomicU64::new(0);
/// VPLLs programmed by 2.1 (phase 5.5).
static CLOCKS_SET: AtomicU64 = AtomicU64::new(0);
static LAST_US: AtomicU64 = AtomicU64::new(0);
static MAX_US: AtomicU64 = AtomicU64::new(0);

fn log(line: String) {
    LOG.with(|l| {
        if l.len() == LOG_LINES {
            l.pop_front();
        }
        l.push_back(line);
    });
}

/// The supervisor interrupts are on: `vblank::on_msi` services them.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// `/dev/dispctl` can be used: supervisors serviced and vblank's MSI
/// armed (it carries them).
pub fn ready() -> bool {
    enabled() && super::vblank::armed()
}

pub(super) fn disp() -> Option<&'static Disp> {
    DISP.get().filter(|_| ready())
}

/// What `setup` found, before vblank is armed (boot).
pub(super) fn disp_at_boot() -> Option<&'static Disp> {
    DISP.get()
}

/// The pixel rate (kHz) a DP link's payload carries at `depth` bits per
/// pixel: lanes x rate x 8 bits per symbol / bpp (the units of nouveau's
/// `link_kbps`, `nv50.c:1177`).
pub(super) fn link_max_khz(l: &sup::DpLink, depth: u32) -> u32 {
    (l.nr as u64 * l.bw as u64 * 27_000 * 8 / depth.max(1) as u64) as u32
}

/// `clock`'s upper bound follows a newly trained link.
pub(super) fn set_max_khz(khz: u32) {
    MAX_KHZ.store(khz, Ordering::Relaxed);
}

/// Supervisors 3 serviced so far: one per completed core UPDATE that
/// changed what drives a head (`modeset.rs` waits on it).
pub(super) fn supers_done() -> u64 {
    SERVICED[2].load(Ordering::Acquire)
}

/// The core channel has fetched everything pushed: a `request` now is not
/// refused as busy (unless a training starts meanwhile).
pub(super) fn core_idle() -> bool {
    let Some(d) = disp() else { return false };
    PUSH_AT.with(|at| d.regs.rd32(evo::CORE.get()) == *at && evo::CORE.idle(&d.regs)) && !TRAINING.load(Ordering::Acquire)
}

/// The primary head's SOR drives nothing (a detach completed).
pub(super) fn link_free() -> bool {
    LINK_FREE.load(Ordering::Acquire)
}

/// The lowest pixel clock (kHz) `clock` takes on raster `t`: 48 Hz.
fn min_khz(t: &HeadTiming) -> u32 {
    (t.htotal as u64 * t.vtotal as u64 * MIN_REFRESH_HZ).div_ceil(1000) as u32
}

/// Starts a link training if the SOR is detached and nothing is pending:
/// then no dispctl push happens until [`end_training`].
pub(super) fn begin_training() -> Result<(), RequestError> {
    let d = disp().ok_or(RequestError::NotReady)?;
    PUSH_AT.with(|at| {
        let idle = d.regs.rd32(evo::CORE.get()) == *at && evo::CORE.idle(&d.regs);
        if TRAINING.load(Ordering::Acquire) || !LINK_FREE.load(Ordering::Acquire) || !idle {
            return Err(RequestError::Busy);
        }
        TRAINING.store(true, Ordering::Release);
        Ok(())
    })
}

pub(super) fn end_training() {
    TRAINING.store(false, Ordering::Release);
}

pub(super) fn log_line(line: String) {
    log(line);
}

/// Boot, `gpu=super`, once `evo::bring_up` ended OK.
pub fn setup(r: &mut String, regs: &Bar0, clocks: bool, train: bool) {
    let Some((bios, dcb)) = super::VBIOS.get() else {
        let _ = writeln!(r, "super: STOP: no VBIOS/DCB (see vbios:/dcb:)");
        return;
    };
    let Some((_, put)) = super::evo::core_push() else {
        let _ = writeln!(r, "super: STOP: the core channel is not up (see chan:)");
        return;
    };
    let hs = regs.rd32(evo::DISP_HEAD_SOR_MASK);
    let (heads, sors) = ((hs & 0xff) as u8, ((hs >> 8) & 0xff) as u8);
    let _ = writeln!(r, "super: heads {:#04x} sors {:#04x}", heads, sors);

    let mut routes = Vec::new();
    for o in dcb.outputs.iter().filter(|o| o.kind == nvgpu::dcb::OUTPUT_DP || o.kind == nvgpu::dcb::OUTPUT_TMDS) {
        let outp = Outp::new(o, dcb);
        match sup::route_get(regs, &outp) {
            Some(sor) => {
                let _ = writeln!(r, "super: outp {:02x} type {:02x} hash {:04x}:{:04x} conn {:?} on SOR-{}", o.index, o.kind, outp.hasht, outp.hashm, outp.conn, sor);
                routes.push((sor, outp));
            }
            None => {
                let _ = writeln!(r, "super: outp {:02x} type {:02x}: no route", o.index, o.kind);
            }
        }
    }

    let Some(head) = (0..HEADS_MAX).find(|&h| heads & (1 << h) != 0 && HeadTiming::read_armed(regs, h).active()) else {
        let _ = writeln!(r, "super: STOP: no head is lit");
        return;
    };
    let Some(sor) = (0..sup::SORS_MAX).find(|&s| sors & (1 << s) != 0 && SorState::read(regs, s, true).head & (1 << head) != 0) else {
        let _ = writeln!(r, "super: STOP: no SOR drives head {}", head);
        return;
    };
    let sor = sor as u8;
    let ctrl = regs.rd32(evo::CORE.armed_base() + sor_set_control(sor));
    let st = SorState::from_ctrl(ctrl);
    let Some(outp) = routes.iter().find(|(s, o)| *s == sor && o.kind == nvgpu::dcb::OUTPUT_DP).map(|(_, o)| *o) else {
        let _ = writeln!(r, "super: STOP: SOR-{} (control {:#x}) has no routed DP output; only a DP attach is ported", sor, ctrl);
        return;
    };
    if st.proto != Proto::Dp {
        let _ = writeln!(r, "super: STOP: SOR-{} runs {:?} (control {:#x}); only a DP attach is ported", sor, st.proto, ctrl);
        return;
    }
    let t = HeadTiming::read_armed(regs, head);
    let link = sup::DpLink::read(regs, sor as u32, st.link);
    let _ = writeln!(
        r,
        "super: head {} ({}x{} {} Hz pixclk, depth {}) on SOR-{} control {:#x} sublink {} outp {:02x}; the GOP's DP link {:?}, packing now h {:#x} v {:#x} watermark {:#x}, nouveau's formula gives {:?}",
        head,
        t.visible().0,
        t.visible().1,
        t.hz,
        t.depth_bits(),
        sor,
        ctrl,
        st.link,
        outp.dcb,
        link,
        regs.rd32(0x61_6568 + head * 0x800) & 0xffff,
        regs.rd32(0x61_656c + head * 0x800) & 0xff_ffff,
        regs.rd32(0x61_6550 + head * 0x800) & 0x3f,
        link.and_then(|l| sup::dp_config(&t, &l))
    );
    // The scripts a detach/attach will run, from the tables (no access).
    let l = if outp.link == 0 { 0 } else { outp.link.trailing_zeros() as u16 + 1 };
    match sup::iedt_match(bios, outp.hasht, (0x100 << head) | (l << 6) | outp.or as u16) {
        Some(iedt) => {
            let on = iedt.ocfg(bios, st.proto_evo, if st.link == 3 { 1 } else { 0 });
            let khz = t.hz / 1000;
            let _ = writeln!(
                r,
                "super: IED OffInt1 {:#x} OffInt2 {:#x} OnInt2 {:?} OnInt3 {:?} (at {} kHz)",
                iedt.script[1],
                iedt.script[2],
                on.and_then(|c| sup::oclk_match(bios, c[0], khz)),
                on.and_then(|c| sup::oclk_match(bios, c[1], khz)),
                khz
            );
        }
        None => {
            let _ = writeln!(r, "super: no IED entry for outp {:02x} on head {}", outp.dcb, head);
        }
    }

    // `clock` bounds: the raster at MIN_REFRESH_HZ, and the pixel rate the
    // link's payload carries (lanes x rate x 8 bits per symbol / bpp; the
    // units of nouveau's `link_kbps`, `nv50.c:1177`).
    let min_khz = min_khz(&t);
    let max_khz = link.map_or(0, |l| link_max_khz(&l, t.depth_bits()));
    if clocks {
        match nvgpu::pll::parse(bios, nvgpu::pll::PLL_VPLL0 + head as u8) {
            Ok(l) => {
                let _ = writeln!(
                    r,
                    "vpll: VPLL{} limits {:?}; the GOP's {} kHz gives {:?}; clock accepts {}..={} kHz",
                    head,
                    l,
                    t.hz / 1000,
                    nvgpu::pll::calc(&l, t.hz / 1000),
                    min_khz,
                    max_khz
                );
            }
            Err(e) => {
                let _ = writeln!(r, "vpll: VPLL{}: no limits ({:?}); 2.1 will fail", head, e);
            }
        }
        // What the GOP left in VPLL<head> (reads only): the registers
        // `ga100_devinit_pll_set` writes and the rest of its 0x40 block.
        let _ = write!(r, "vpll: GOP VPLL{} e9c0+h*4 {:#x}, ef00+h*0x40..:", head, regs.rd32(0xe9c0 + head * 4));
        for i in 0..16 {
            let _ = write!(r, " {:#x}", regs.rd32(0xef00 + head * 0x40 + i * 4));
        }
        let _ = writeln!(r);
    }

    SUPER.with(|s| *s = Some(Supervisor::new(Config { heads, sors, owned: 1 << head, routes, clocks })));
    PUSH_AT.with(|p| *p = put);
    let (board_nr, board_bw) = dcb.outputs.iter().find(|o| o.index == outp.dcb).map_or((0, 0), |o| (o.dp_link_nr, o.dp_link_bw));
    if train {
        super::dplink::setup(r, regs, bios, &outp, sor, st.link, board_nr, board_bw);
    }
    MAX_KHZ.store(max_khz, Ordering::Relaxed);
    DISP.call_once(|| Disp {
        regs: Bar0 { base: regs.base, len: regs.len },
        head,
        sor,
        sublink: st.link,
        outp,
        board_nr,
        board_bw,
        ctrl,
        clocks,
        train,
        gop_hz: t.hz,
    });
    sup::arm(regs);
    ENABLED.store(true, Ordering::Release);
    let _ = writeln!(
        r,
        "super: ready: supervisor interrupts on (CTRL_DISP MSK {:#x} EN {:#x}, status {:#x}); /dev/dispctl detaches SOR-{} from head {} and puts back {:#x}; core push at {:#x}",
        regs.rd32(sup::CTRL_DISP_MSK),
        regs.rd32(sup::CTRL_DISP_EN),
        regs.rd32(sup::CTRL_DISP),
        sor,
        head,
        ctrl,
        put
    );
}

/// From `vblank::on_msi` (ISR, CPU 0) with the supervisors it
/// acknowledged: the work and the release.
pub fn on_pending(regs: &Bar0, pending: u32) {
    let Some((bios, _)) = super::VBIOS.get() else { return };
    let t0 = crate::cpu::tsc::read();
    let Some(rep) = SUPER.with(|s| s.as_mut().map(|s| s.service(regs, bios, pending))) else { return };
    let us = crate::cpu::tsc::read().wrapping_sub(t0) / (crate::cpu::tsc::freq_hz() / 1_000_000).max(1);
    LAST_US.store(us, Ordering::Relaxed);
    MAX_US.fetch_max(us, Ordering::Relaxed);
    if (1..=3).contains(&rep.stage) {
        SERVICED[rep.stage as usize - 1].fetch_add(1, Ordering::Relaxed);
    }
    if rep.stage == 3 {
        if let Some(d) = DISP.get() {
            let free = SUPER.with(|s| s.as_ref().map_or(false, |s| s.sor_state(d.sor).1.head == 0));
            LINK_FREE.store(free, Ordering::Release);
        }
    }
    for e in &rep.events {
        match e {
            Event::Script { result: Err(_), .. } => {
                SCRIPT_ERRORS.fetch_add(1, Ordering::Relaxed);
            }
            Event::ClockNotPorted { .. } | Event::ClockFailed { .. } | Event::AttachNotPorted { .. } | Event::NoOutput { .. } | Event::NoScript { .. } | Event::DpFailed { .. } => {
                NOT_DONE.fetch_add(1, Ordering::Relaxed);
            }
            Event::Clock { .. } => {
                CLOCKS_SET.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }
    let mut line = String::new();
    let _ = write!(line, "super: {} at {} ms: stat {:#x} masks", rep.stage, crate::time::ktime_get() / 1_000_000, rep.stat);
    for m in &rep.masks[..4] {
        let _ = write!(line, " {:#x}", m);
    }
    let _ = write!(line, ", {} us:", us);
    for e in &rep.events {
        let _ = write!(line, " {:?};", e);
    }
    crate::serial_println_raw!("{}", line);
    log(line);
}

/// A CTRL_DISP error seen with a supervisor interrupt (`0x611848`).
pub fn note_error(info: u32) {
    CTRL_DISP_ERRORS.fetch_add(1, Ordering::Relaxed);
    crate::serial_println_raw!("super: CTRL_DISP error, info {:#x}", info);
    log(alloc::format!("super: CTRL_DISP error, info {:#x}", info));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cmd {
    Detach,
    Attach,
    /// Pixel clock of the primary head, kHz (`gpu=vpll`); 0 = the GOP's.
    Clock(u32),
    /// A mode's head methods (`nvgpu::mode::Mode::methods`), then the
    /// GOP's SOR control (`gpu=modes`, from `modeset.rs`).
    Mode([(u32, u32); 9]),
}

impl Cmd {
    /// For the log (a mode's methods are logged with the push).
    fn name(&self) -> String {
        match self {
            Cmd::Mode(_) => String::from("Mode"),
            c => alloc::format!("{:?}", c),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestError {
    NotReady,
    /// `clock` without `gpu=vpll`, or out of bounds.
    Invalid,
    /// The core channel has not finished the previous push.
    Busy,
    Chan(evo::ChanError),
}

/// `/dev/dispctl`: pushes `SOR_SET_CONTROL(sor)` (0, or the GOP's value),
/// the head's pixel clock or a whole mode, and UPDATE on the core channel.
/// Returns the last value pushed.
pub fn request(cmd: Cmd) -> Result<u32, RequestError> {
    let d = DISP.get().filter(|_| ready()).ok_or(RequestError::NotReady)?;
    let mut methods: Vec<(u32, u32)> = Vec::new();
    match cmd {
        Cmd::Detach => methods.push((sor_set_control(d.sor), 0)),
        Cmd::Attach => methods.push((sor_set_control(d.sor), d.ctrl)),
        Cmd::Clock(khz) => {
            let khz = if khz == 0 { d.gop_hz / 1000 } else { khz };
            let max_khz = MAX_KHZ.load(Ordering::Relaxed);
            let min_khz = min_khz(&HeadTiming::read_armed(&d.regs, d.head));
            if !d.clocks || khz < min_khz || khz > max_khz {
                REFUSED.fetch_add(1, Ordering::Relaxed);
                log(alloc::format!("dispctl: {:?} refused: needs gpu=vpll and {}..={} kHz", cmd, min_khz, max_khz));
                return Err(RequestError::Invalid);
            }
            let (f, max) = head_pixel_clock(d.head);
            methods.extend([(f, khz * 1000), (max, khz * 1000)]);
        }
        Cmd::Mode(head) => {
            methods.extend(head);
            methods.push((sor_set_control(d.sor), d.ctrl));
        }
    }
    let value = methods.last().map_or(0, |&(_, v)| v);
    push_core(&cmd.name(), &methods, cmd != Cmd::Detach).map(|_| value)
}

/// Pushes `methods` and UPDATE on the core channel, not waited on: the one
/// place the core push buffer is written after boot. `may_attach` clears
/// [`LINK_FREE`] (anything but a detach may attach the SOR again).
/// `Busy` while the core has not fetched the last push or a link training
/// runs; logged either way.
pub(super) fn push_core(name: &str, methods: &[(u32, u32)], may_attach: bool) -> Result<(), RequestError> {
    // The core's interlock flags persist in its ASSEMBLY state: a push that
    // set a window interlock (`hdmi.rs`, nouveau's first UPDATE of a new
    // window) leaves every later core UPDATE waiting for that window
    // (Ryzen #91: `hdmi off` never latched). The next push clears them.
    let mut with_flags;
    let methods = match methods.iter().rev().find(|m| m.0 == evo::CORE_SET_WINDOW_INTERLOCK_FLAGS) {
        Some(&(_, v)) => {
            WINDOW_INTERLOCK.store(v != 0, Ordering::Release);
            methods
        }
        None if WINDOW_INTERLOCK.swap(false, Ordering::AcqRel) => {
            with_flags = alloc::vec![(evo::CORE_SET_INTERLOCK_FLAGS, 0), (evo::CORE_SET_WINDOW_INTERLOCK_FLAGS, 0)];
            with_flags.extend_from_slice(methods);
            &with_flags[..]
        }
        None => methods,
    };
    let d = DISP.get().filter(|_| ready()).ok_or(RequestError::NotReady)?;
    let (buf, _) = super::evo::core_push().ok_or(RequestError::NotReady)?;
    REQUESTS.fetch_add(1, Ordering::Relaxed);
    let res = PUSH_AT.with(|at| {
        let regs = &d.regs;
        let get = regs.rd32(evo::CORE.get());
        if get != *at || !evo::CORE.idle(regs) || TRAINING.load(Ordering::Acquire) {
            return Err(RequestError::Busy);
        }
        if may_attach {
            LINK_FREE.store(false, Ordering::Release);
        }
        let before = *at;
        let mut p = Push::new(buf, *at);
        if p.room_words() < 2 * methods.len() + 8 {
            evo::wind(regs, evo::CORE, &mut p).map_err(RequestError::Chan)?;
        }
        for &(mth, v) in methods {
            p.mthd(mth, &[v]).map_err(RequestError::Chan)?;
        }
        evo::push_update(&mut p).map_err(RequestError::Chan)?;
        evo::submit(regs, evo::CORE, &p);
        *at = p.put_bytes();
        Ok((before, *at))
    });
    let line = match res {
        Ok((a, b)) => alloc::format!(
            "dispctl: {} at {} ms: methods {:x?} + UPDATE pushed (core put {:#x} -> {:#x})",
            name,
            crate::time::ktime_get() / 1_000_000,
            methods,
            a,
            b
        ),
        Err(e) => {
            REFUSED.fetch_add(1, Ordering::Relaxed);
            alloc::format!("dispctl: {} refused: {:?}", name, e)
        }
    };
    crate::serial_println!("{}", line);
    log(line);
    res.map(|_| ())
}

/// The supervisors also work on `head`, whose SOR `sor` drives `outp`
/// (`hdmi.rs`): its stage work runs from now on (`Config::owned`,
/// `Config::routes`).
pub(super) fn own_head(head: u32, sor: u8, outp: Outp) {
    SUPER.with(|s| {
        if let Some(s) = s.as_mut() {
            s.cfg.owned |= 1 << head;
            s.cfg.routes.retain(|(r, _)| *r != sor);
            s.cfg.routes.push((sor, outp));
        }
    });
}

/// Back to what [`own_head`] changed.
pub(super) fn disown_head(head: u32, sor: u8) {
    SUPER.with(|s| {
        if let Some(s) = s.as_mut() {
            s.cfg.owned &= !(1 << head);
            s.cfg.routes.retain(|(r, _)| *r != sor);
        }
    });
}

/// The BAR0 window `setup` kept, for the modules that run on top of it.
pub(super) fn regs() -> Option<&'static Bar0> {
    disp().map(|d| &d.regs)
}

/// One status line (`/dev/dispctl` read, the job's evidence).
pub fn status() -> String {
    let Some(d) = DISP.get() else { return String::from("dispctl: not set up (needs gpu=super)\n") };
    let r = &d.regs;
    alloc::format!(
        "dispctl: head {} SOR-{} armed control {:#x} (GOP {:#x}) link {} free {} armed pixclk {} (GOP {}) max {} vpll {:#x} {:#x} core put {:#x} get {:#x} idle {} vblank seq {}\n",
        d.head,
        d.sor,
        r.rd32(evo::CORE.armed_base() + sor_set_control(d.sor)),
        d.ctrl,
        sup::DpLink::read(r, d.sor as u32, d.sublink).map_or(String::from("?"), |l| alloc::format!("{}x{:#x}{}", l.nr, l.bw, if l.ef { "ef" } else { "" })),
        LINK_FREE.load(Ordering::Relaxed) as u8,
        r.rd32(evo::CORE.armed_base() + head_pixel_clock(d.head).0),
        d.gop_hz,
        MAX_KHZ.load(Ordering::Relaxed),
        r.rd32(0xef18 + d.head * 0x40),
        r.rd32(0xef04 + d.head * 0x40),
        r.rd32(evo::CORE.put()),
        r.rd32(evo::CORE.get()),
        evo::CORE.idle(r) as u8,
        super::vblank::seq()
    )
}

/// The runtime log for /proc/gpu.
pub fn render_log() -> String {
    let mut out = String::new();
    LOG.with(|l| {
        for line in l.iter() {
            out.push_str(line);
            out.push('\n');
        }
    });
    out
}

/// `/proc/kdebug` line.
pub fn render_kdebug() -> String {
    if !enabled() {
        return String::from("gpu_super: off");
    }
    alloc::format!(
        "gpu_super: enabled=1 serviced={},{},{} script_errors={} not_done={} ctrl_disp_errors={} requests={} refused={} clocks_set={} work_us last={} max={}",
        SERVICED[0].load(Ordering::Relaxed),
        SERVICED[1].load(Ordering::Relaxed),
        SERVICED[2].load(Ordering::Relaxed),
        SCRIPT_ERRORS.load(Ordering::Relaxed),
        NOT_DONE.load(Ordering::Relaxed),
        CTRL_DISP_ERRORS.load(Ordering::Relaxed),
        REQUESTS.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
        CLOCKS_SET.load(Ordering::Relaxed),
        LAST_US.load(Ordering::Relaxed),
        MAX_US.load(Ordering::Relaxed),
    )
}
