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
//   on. Refused (EAGAIN) while the core channel has not finished the last
//   one. Under `PUSH_AT`, the only user of the core push buffer after boot.
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
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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

/// What `setup` found: the head and SOR `/dev/dispctl` detaches and
/// re-attaches, and the SOR control value the GOP left.
struct Disp {
    regs: Bar0,
    head: u32,
    sor: u8,
    ctrl: u32,
}

static DISP: spin::Once<Disp> = spin::Once::new();
/// The supervisor state (kept from supervisor 1 to 3). Taken by the MSI
/// handler; `render_kdebug` does not take it.
static SUPER: IrqMutex<Option<Supervisor>, KernelIrq> = IrqMutex::new(None);
/// Where the core channel's next push goes (bytes).
static PUSH_AT: IrqMutex<u32, KernelIrq> = IrqMutex::new(0);
static ENABLED: AtomicBool = AtomicBool::new(false);

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

/// Boot, `gpu=super`, once `evo::bring_up` ended OK.
pub fn setup(r: &mut String, regs: &Bar0) {
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

    SUPER.with(|s| *s = Some(Supervisor::new(Config { heads, sors, owned: 1 << head, routes })));
    PUSH_AT.with(|p| *p = put);
    DISP.call_once(|| Disp { regs: Bar0 { base: regs.base, len: regs.len }, head, sor, ctrl });
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
    for e in &rep.events {
        match e {
            Event::Script { result: Err(_), .. } => {
                SCRIPT_ERRORS.fetch_add(1, Ordering::Relaxed);
            }
            Event::ClockNotPorted { .. } | Event::AttachNotPorted { .. } | Event::NoOutput { .. } | Event::NoScript { .. } | Event::DpFailed { .. } => {
                NOT_DONE.fetch_add(1, Ordering::Relaxed);
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestError {
    NotReady,
    /// The core channel has not finished the previous push.
    Busy,
    Chan(evo::ChanError),
}

/// `/dev/dispctl`: pushes `SOR_SET_CONTROL(sor)` (0, or the GOP's value)
/// and UPDATE on the core channel. Returns the value pushed.
pub fn request(cmd: Cmd) -> Result<u32, RequestError> {
    let d = DISP.get().filter(|_| ready()).ok_or(RequestError::NotReady)?;
    let (buf, _) = super::evo::core_push().ok_or(RequestError::NotReady)?;
    let value = match cmd {
        Cmd::Detach => 0,
        Cmd::Attach => d.ctrl,
    };
    REQUESTS.fetch_add(1, Ordering::Relaxed);
    let res = PUSH_AT.with(|at| {
        let regs = &d.regs;
        let get = regs.rd32(evo::CORE.get());
        if get != *at || !evo::CORE.idle(regs) {
            return Err(RequestError::Busy);
        }
        let before = *at;
        let mut p = Push::new(buf, *at);
        if p.room_words() < 8 {
            evo::wind(regs, evo::CORE, &mut p).map_err(RequestError::Chan)?;
        }
        p.mthd(sor_set_control(d.sor), &[value]).and_then(|_| evo::push_update(&mut p)).map_err(RequestError::Chan)?;
        evo::submit(regs, evo::CORE, &p);
        *at = p.put_bytes();
        Ok((before, *at))
    });
    let line = match res {
        Ok((a, b)) => alloc::format!(
            "dispctl: {:?} at {} ms: SOR_SET_CONTROL({}) = {:#x} + UPDATE pushed (core put {:#x} -> {:#x})",
            cmd,
            crate::time::ktime_get() / 1_000_000,
            d.sor,
            value,
            a,
            b
        ),
        Err(e) => {
            REFUSED.fetch_add(1, Ordering::Relaxed);
            alloc::format!("dispctl: {:?} refused: {:?}", cmd, e)
        }
    };
    crate::serial_println!("{}", line);
    log(line);
    res.map(|_| value)
}

/// One status line (`/dev/dispctl` read, the job's evidence).
pub fn status() -> String {
    let Some(d) = DISP.get() else { return String::from("dispctl: not set up (needs gpu=super)\n") };
    let r = &d.regs;
    alloc::format!(
        "dispctl: head {} SOR-{} armed control {:#x} (GOP {:#x}) core put {:#x} get {:#x} idle {} vblank seq {}\n",
        d.head,
        d.sor,
        r.rd32(evo::CORE.armed_base() + sor_set_control(d.sor)),
        d.ctrl,
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
        "gpu_super: enabled=1 serviced={},{},{} script_errors={} not_done={} ctrl_disp_errors={} requests={} refused={} work_us last={} max={}",
        SERVICED[0].load(Ordering::Relaxed),
        SERVICED[1].load(Ordering::Relaxed),
        SERVICED[2].load(Ordering::Relaxed),
        SCRIPT_ERRORS.load(Ordering::Relaxed),
        NOT_DONE.load(Ordering::Relaxed),
        CTRL_DISP_ERRORS.load(Ordering::Relaxed),
        REQUESTS.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
        LAST_US.load(Ordering::Relaxed),
        MAX_US.load(Ordering::Relaxed),
    )
}
