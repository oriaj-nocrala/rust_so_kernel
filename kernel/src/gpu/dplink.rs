// kernel/src/gpu/dplink.rs
//
// `gpu=dplink` (phase 5.6 of docs/gpu/gpu-plan.md): retrains the DP link of
// the primary head's output. The sequence is `nvgpu::dp` (nouveau's
// `nvkm_dp_train`, replayed register for register against trace-nogsp);
// this module finds the output's DP table entry at boot and runs a training
// when `/dev/dispctl` asks for one.
//
// - Boot (`setup`, IF=0, before the APs): the DP table entry of the output
//   `supervisor::setup` found, its scripts, and the board's limits. Reads
//   only.
// - `/dev/dispctl` `train <lanes> <rate>` (any CPU, IF=1): refused unless
//   the SOR is detached (a `detach` whose supervisor 3 completed) and the
//   core idle; while it runs, dispctl pushes are refused
//   (`supervisor::begin_training`). Then, like nouveau between its round-1
//   detach and round-2 attach: pad to AUX, LTTPR probe, receiver caps, the
//   configuration checked against sink and board, `DisableLT`, the
//   training, pad back. ~130 ms of busy-waits (two 20 ms condition polls,
//   a fixed 40 ms in `links`), in the caller's syscall.
// - BAR0: the training touches the SOR, its PLL, AUX channel `aux` and the
//   VGA CR ports. The vblank handler touches the interrupt registers and,
//   with no supervisor pending (the SOR is detached and no push is
//   allowed), nothing of these; flips touch only window 0's. No lock.
// - The next `attach` runs supervisor 2.2 with this link: it reads it back
//   from the SOR (`DpLink::read`) for the DP packing.

use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

use nvgpu::aux::{self, Aux};
use nvgpu::dp::{self, DpInfo, LinkConfig, Params, Report, Sor};
use nvgpu::pad::{self, PadMode};
use nvgpu::supervisor::{self as sup, Outp};
use nvgpu::vblank::HeadTiming;
use nvgpu::Mmio;

use super::supervisor::{self as svr, RequestError};
use super::Bar0;

static INFO: spin::Once<DpInfo> = spin::Once::new();

static TRAINS: AtomicU64 = AtomicU64::new(0);
static OK: AtomicU64 = AtomicU64::new(0);
static FAILED: AtomicU64 = AtomicU64::new(0);
static REFUSED: AtomicU64 = AtomicU64::new(0);
static LAST_MS: AtomicU64 = AtomicU64::new(0);

/// Boot, from `supervisor::setup` with `gpu=dplink`: the output's DP table
/// entry, logged.
#[allow(clippy::too_many_arguments)]
pub fn setup(r: &mut String, _regs: &Bar0, bios: &nvgpu::vbios::Bios, outp: &Outp, sor: u8, sublink: u8, board_nr: u8, board_bw: u8) {
    let Some(info) = dp::dpout_match(bios, outp.hasht, outp.hashm) else {
        let _ = writeln!(r, "dplink: STOP: no DP table entry for outp {:02x} ({:04x}:{:04x})", outp.dcb, outp.hasht, outp.hashm);
        return;
    };
    let rates: alloc::vec::Vec<String> = [0x1e, 0x14, 0x0a, 0x06]
        .iter()
        .map(|&bw| {
            alloc::format!(
                "{:#x}: before {:?} lnkcmp {:?}",
                bw,
                dp::rate_script(bios, info.script[0], bw),
                dp::rate_script(bios, info.lnkcmp, bw)
            )
        })
        .collect();
    let _ = writeln!(
        r,
        "dplink: outp {:02x} on SOR-{} sublink {} aux {:?} pad {:?}: DP table v{:#x} entry {:#x} flags {:#x} AfterLT {:#x} EnableSpread {:#x} DisableSpread {:#x} DisableLT {:#x}; per rate {}; board max {}x{:#x}",
        outp.dcb,
        sor,
        sublink,
        outp.aux,
        outp.pad,
        info.ver,
        info.data,
        info.flags,
        info.script[1],
        info.script[2],
        info.script[3],
        info.script[4],
        rates.join(", "),
        board_nr,
        board_bw
    );
    if outp.aux.is_none() {
        let _ = writeln!(r, "dplink: STOP: the output has no AUX channel");
        return;
    }
    INFO.call_once(|| info);
    let _ = writeln!(r, "dplink: ready: /dev/dispctl train <lanes> <rate> retrains it while detached");
}

/// `setup` found the output's DP table entry: `train` can run.
pub fn ready() -> bool {
    INFO.get().is_some()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrainError {
    /// No `gpu=dplink`, or setup stopped.
    NotReady,
    /// Not detached, a push pending, or another training running.
    Busy,
    /// Refused before touching anything (the configuration, an LTTPR, the
    /// sink not answering).
    Refused,
    /// Ran and failed (details in the log).
    Failed,
}

/// `/dev/dispctl` `train`.
pub fn train(nr: u8, bw: u8) -> Result<(), TrainError> {
    let d = svr::disp().filter(|d| d.train).ok_or(TrainError::NotReady)?;
    let info = INFO.get().ok_or(TrainError::NotReady)?;
    let (bios, _) = super::VBIOS.get().ok_or(TrainError::NotReady)?;
    if let Err(e) = svr::begin_training() {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        report(alloc::format!("dplink: train {}x{:#x} refused: {:?} (needs the SOR detached and the core idle)", nr, bw, e));
        return Err(match e {
            RequestError::Busy => TrainError::Busy,
            _ => TrainError::NotReady,
        });
    }
    let t0 = crate::cpu::tsc::read();
    let res = run(d, bios, info, LinkConfig { nr, bw });
    svr::end_training();
    let ms = crate::cpu::tsc::read().wrapping_sub(t0) / (crate::cpu::tsc::freq_hz() / 1000).max(1);
    LAST_MS.store(ms, Ordering::Relaxed);
    let (line, out) = match res {
        Err(why) => {
            REFUSED.fetch_add(1, Ordering::Relaxed);
            (alloc::format!("dplink: train {}x{:#x} refused: {}", nr, bw, why), Err(TrainError::Refused))
        }
        Ok(r) => {
            TRAINS.fetch_add(1, Ordering::Relaxed);
            let ok = r.result == Some(Ok(()));
            if ok {
                OK.fetch_add(1, Ordering::Relaxed);
            } else {
                FAILED.fetch_add(1, Ordering::Relaxed);
            }
            let link = sup::DpLink::read(&d.regs, d.sor as u32, d.sublink);
            let t = HeadTiming::read_armed(&d.regs, d.head);
            if let (true, Some(l)) = (ok, link) {
                svr::set_max_khz(svr::link_max_khz(&l, t.depth_bits()));
            }
            let mut s = String::new();
            let _ = write!(
                s,
                "dplink: train {}x{:#x} {} in {} ms: {:?}; sink power {:?} woken {}; lane power {} after {} polls; CR {} rounds, EQ {} rounds (TPS{}); status {:02x?} lanes {:02x?}; aux error {:?}; SOR link now {:?}; scripts",
                nr,
                bw,
                if ok { "OK" } else { "FAILED" },
                ms,
                r.result,
                r.sink_power,
                r.sink_woken,
                if r.power_done { "up" } else { "NOT up" },
                r.power_polls,
                r.cr_tries,
                r.eq_tries,
                r.eq_pattern,
                r.stat,
                r.conf,
                r.aux_error,
                link
            );
            for (script, addr, st) in &r.scripts {
                let _ = write!(s, " {:?}@{:#x}({} ops, {} writes, {} aux, {} aux errors)", script, addr, st.opcodes, st.writes, st.aux, st.aux_errors);
            }
            (s, if ok { Ok(()) } else { Err(TrainError::Failed) })
        }
    };
    report(line);
    out
}

fn report(line: String) {
    crate::serial_println!("{}", line);
    svr::log_line(line);
}

/// The training proper, pad held. `Err` = refused before any write to the
/// SOR (the reason).
fn run(d: &svr::Disp, bios: &nvgpu::vbios::Bios, info: &DpInfo, link: LinkConfig) -> Result<Report, String> {
    let regs = &d.regs;
    let ch = d.outp.aux.ok_or_else(|| String::from("no AUX channel"))?;
    let saved = d.outp.pad.map(|n| pad::acquire(regs, n, PadMode::Aux));
    let a = Aux::new(regs, ch);
    let res = (|| {
        // LTTPRs (`nouveau_dp_probe_lttpr`, `nouveau_dp.c:45-57`: one
        // transaction, a repeater only if it answers >= 0x14).
        let mut rev = [0u8];
        if let Ok(r) = a.xfer(aux::NATIVE_READ, dp::LTTPR_REV, &mut rev) {
            if r.code == 0 && r.len == 1 && rev[0] >= 0x14 {
                return Err(alloc::format!("an LTTPR answers (rev {:#x}): not ported", rev[0]));
            }
        }
        let dpcd = a.read_dpcd_caps().map_err(|e| alloc::format!("receiver caps: {:?}", e))?;
        dp::check_config(&dpcd, d.board_nr, d.board_bw, link).map_err(|e| alloc::format!("{:?} (sink caps {:02x?})", e, dpcd))?;
        let p = Params { bios, info, sor: Sor { id: d.sor, link: d.sublink }, conn: d.outp.conn, aux: ch, dpcd, link };
        let mut r = Report::default();
        if let Err(e) = dp::disable(regs, &p, &mut r) {
            r.result = Some(Err(e));
            return Ok(r);
        }
        dp::train(regs, &p, &mut r);
        Ok(r)
    })();
    if let Some(v) = a.autodpcd_found() {
        regs.mask(a.autodpcd_reg(), 0x0001_0000, v & 0x0001_0000);
    }
    if let Some(s) = saved {
        pad::release(regs, s);
    }
    res
}

/// `/proc/kdebug` line.
pub fn render_kdebug() -> String {
    if INFO.get().is_none() {
        return String::from("gpu_dplink: off");
    }
    alloc::format!(
        "gpu_dplink: enabled=1 trains={} ok={} failed={} refused={} last_ms={}",
        TRAINS.load(Ordering::Relaxed),
        OK.load(Ordering::Relaxed),
        FAILED.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
        LAST_MS.load(Ordering::Relaxed)
    )
}
