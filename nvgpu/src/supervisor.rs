//! Display supervisors (phase 5.4 of the plan), without GSP.
//!
//! When a core UPDATE changes what drives a head (an output resource
//! attached or detached, a new pixel clock), the display stops and raises
//! three supervisor interrupts in turn; each waits until the driver has
//! done its part and released it. The interrupt side (`0x611ec0` bit 12 →
//! CTRL_DISP `0x611c30` → ack `0x611860`) is in [`crate::vblank::service`];
//! this module is the work, one call per supervisor, as nouveau's
//! `gv100_disp_super` does it (`engine/disp/gv100.c:835-891`, the stages in
//! `engine/disp/nv50.c:1137-1362`):
//!
//! - 1: read the heads' and SORs' state (ARM = now, ASSEMBLY = after the
//!   UPDATE); 1.0: each head losing its SOR runs the output's `OffInt1`
//!   script (VBIOS IED table);
//! - 2: 2.0 `OffInt2` for the same; 2.1 the new pixel clock (VPLL, phase
//!   5.5: reported, not done); 2.2 each head getting a SOR runs `OnInt2`,
//!   programs the RG clock divider, the DP packing (audio symbols and
//!   watermark, from the link the SOR runs) and the SOR clock;
//! - 3: 3.0 `OnInt3`.
//!
//! After each: every head's work mask cleared, then the release.
//!
//! Only the heads in [`Config::owned`] are worked on; another head's bits
//! are reported and released without work. The output a SOR drives comes
//! from the pad routing (`route_get`), read once by the caller, because
//! nothing here acquires or routes outputs: this phase re-attaches the SOR
//! the GOP routed, with the link the GOP trained.

use alloc::vec::Vec;

use crate::dcb::{Dcb, Output, OUTPUT_DP, OUTPUT_TMDS};
use crate::init::{self, ScriptError, Target};
use crate::vbios::Bios;
use crate::vblank::{HeadTiming, HEADS_MAX};
use crate::Mmio;

/// CTRL_DISP status: bits 0-2 = supervisor 1-3 pending, bit 7 error
/// (`engine/disp/gv100.c:940-958`).
pub const CTRL_DISP: u32 = 0x61_1c30;
pub const CTRL_DISP_SUPERVISORS: u32 = 0x0000_0007;
pub const CTRL_DISP_ERROR: u32 = 0x0000_0080;
/// Written with the pending supervisor bits to acknowledge the interrupt
/// (`gv100.c:945`).
pub const CTRL_DISP_ACK: u32 = 0x61_1860;
/// What the error was, read (and written back) by nouveau (`gv100.c:957`).
pub const CTRL_DISP_ERROR_INFO: u32 = 0x61_1848;
/// CTRL_DISP interrupt mask and enable (`gv100.c:1196-1198`: nouveau sets
/// AWAKEN, ERROR and SUPERVISOR1-3, `0x187`, in both). This driver enables
/// the supervisors only: nothing here services AWAKEN (window wakeups), and
/// the error bit is polled with the other faults (`evo::Faults`).
pub const CTRL_DISP_MSK: u32 = 0x61_1cf0;
pub const CTRL_DISP_EN: u32 = 0x61_1db0;

/// Supervisor status, written `0x80000000` to release (`gv100.c:844,889`).
pub const SUPER_STAT: u32 = 0x61_07a8;
pub const SUPER_RELEASE: u32 = 0x8000_0000;
/// Head n's work mask, `+ n * 4` (`gv100.c:848`), cleared after the work
/// (`gv100.c:887`): bit 12 = its output changes (stages x.0, 2.2), bit 16 =
/// its pixel clock changes (2.1) (`gv100.c:858,870`).
pub const SUPER_HEAD: u32 = 0x61_07ac;
pub const HEAD_OUTPUT: u32 = 0x0000_1000;
pub const HEAD_CLOCK: u32 = 0x0001_0000;

/// SOR control, ASSEMBLY `0x680300` / ARM `+ 0x8000`, `+ sor * 0x20`
/// (`gv100.c:184-204`): bits 0-7 heads, 8-11 protocol.
const SOR_CTRL_ASY: u32 = 0x68_0300;
const SOR_CTRL_ARM: u32 = SOR_CTRL_ASY + 0x8000;
const SOR_CTRL_STRIDE: u32 = 0x20;
pub const SORS_MAX: u32 = 8;

/// Enables the supervisor interrupts (see [`CTRL_DISP_EN`]).
pub fn arm(m: &dyn Mmio) {
    m.wr32(CTRL_DISP_MSK, CTRL_DISP_SUPERVISORS);
    m.wr32(CTRL_DISP_EN, CTRL_DISP_SUPERVISORS);
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Proto {
    Lvds,
    Tmds,
    Dp,
    #[default]
    Unknown,
}

/// A SOR's control (`gv100_sor_state`, `gv100.c:184-204`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SorState {
    /// Raw protocol field (the IED configuration tables are keyed by it).
    pub proto_evo: u8,
    pub proto: Proto,
    /// 1 = sublink A, 2 = B, 3 = both (dual TMDS).
    pub link: u8,
    /// Heads it drives.
    pub head: u8,
}

impl SorState {
    pub fn from_ctrl(ctrl: u32) -> SorState {
        let proto_evo = ((ctrl & 0xf00) >> 8) as u8;
        let (proto, link) = match proto_evo {
            0 => (Proto::Lvds, 1),
            1 => (Proto::Tmds, 1),
            2 => (Proto::Tmds, 2),
            5 => (Proto::Tmds, 3),
            8 => (Proto::Dp, 1),
            9 => (Proto::Dp, 2),
            _ => (Proto::Unknown, 0),
        };
        SorState { proto_evo, proto, link, head: (ctrl & 0xff) as u8 }
    }

    pub fn read(m: &dyn Mmio, sor: u32, armed: bool) -> SorState {
        let base = if armed { SOR_CTRL_ARM } else { SOR_CTRL_ASY };
        SorState::from_ctrl(m.rd32(base + sor * SOR_CTRL_STRIDE))
    }
}

/// What the VBIOS tables and scripts need of a DCB output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outp {
    pub dcb: u8,
    pub kind: u8,
    /// DCB OR mask and link: the pad, not the SOR (GM200+ route any SOR
    /// to it, `gm200.c:98-139`).
    pub or: u8,
    pub link: u8,
    pub hasht: u16,
    pub hashm: u16,
    /// Connector type (`GENERIC_CONDITION` 0 asks for eDP).
    pub conn: Option<u8>,
}

impl Outp {
    pub fn new(o: &Output, dcb: &Dcb) -> Outp {
        Outp {
            dcb: o.index,
            kind: o.kind,
            or: o.or,
            link: o.link,
            hasht: o.hasht(),
            hashm: o.hashm(),
            conn: dcb.connector(o.connector).map(|c| c.kind),
        }
    }

    /// The SOR protocol this output runs.
    fn proto(&self) -> Proto {
        match self.kind {
            OUTPUT_DP => Proto::Dp,
            OUTPUT_TMDS => Proto::Tmds,
            0x3 => Proto::Lvds,
            _ => Proto::Unknown,
        }
    }
}

/// `gm200_sor_route_get` (`gm200.c:116-139`): the SOR routed to `o`'s pad,
/// from `0x612308 + pad_sublink * 0x80` (bits 0-3 = SOR + 1, 0 = none).
pub fn route_get(m: &dyn Mmio, o: &Outp) -> Option<u8> {
    if o.or == 0 {
        return None;
    }
    let sublinks = o.link;
    let mut sor = [0u32; 2];
    let mut lnk = [0u32; 2];
    let pad = o.or.trailing_zeros() * 2;
    for s in 0..2 {
        if sublinks & (1 << s) != 0 {
            let data = m.rd32(0x61_2308 + (pad + s as u32) * 0x80);
            lnk[s] = (data & 0x10) >> 4;
            sor[s] = data & 0xf;
            if sor[s] == 0 {
                return None;
            }
        }
    }
    if sublinks == 3 && (sor[0] != sor[1] || lnk[0] != 0 || lnk[1] == 0) {
        return None;
    }
    let s = if sublinks & 1 != 0 { sor[0] } else { sor[1] };
    s.checked_sub(1).map(|s| s as u8)
}

/// The DP link a SOR runs, from its registers: nouveau keeps these from its
/// own link training (`dp.c`), this driver reads what the GOP trained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DpLink {
    /// Link rate, DPCD units of 0.27 Gb/s (0x14 = HBR2).
    pub bw: u8,
    pub nr: u8,
    pub ef: bool,
    pub mst: bool,
}

impl DpLink {
    /// Inverse of `ga102_sor_dp_links` (`ga102.c:34-70`): clock select in
    /// `0x612300 + sor * 0x800` bits 18-22, lanes (bits 16-19), enhanced
    /// framing (bit 14) and MST (bit 30) in `0x61c10c + sor * 0x800 (+ 0x80
    /// for sublink B)`. `None` if the clock select is not a DP rate.
    pub fn read(m: &dyn Mmio, sor: u32, link: u8) -> Option<DpLink> {
        let soff = sor * 0x800;
        let loff = soff + if link == 2 { 0x80 } else { 0 };
        let clksor = m.rd32(0x61_2300 + soff);
        let dpctrl = m.rd32(0x61_c10c + loff);
        let bw = match clksor & 0x007c_0000 {
            0x0000_0000 => 0x06,
            0x0004_0000 => 0x0a,
            0x0008_0000 => 0x14,
            0x000c_0000 => 0x1e,
            0x0010_0000 => 0x08,
            0x0014_0000 => 0x09,
            0x0018_0000 => 0x0c,
            0x001c_0000 => 0x10,
            _ => return None,
        };
        let lanes = (dpctrl >> 16) & 0xf;
        Some(DpLink { bw, nr: lanes.count_ones() as u8, ef: dpctrl & 0x4000 != 0, mst: dpctrl & 0x4000_0000 != 0 })
    }
}

/// What the DP packing registers get for a mode on a link.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DpConfig {
    /// Audio symbols per hblank / vblank.
    pub h: u16,
    pub v: u32,
    pub watermark: u8,
}

/// `nv50_disp_super_2_2_dp` (`nv50.c:1156-1260`) for a SOR without
/// `activesym` (GA102's, `ga102.c:73-83`): no TU search, `bestTU = 64`.
/// `None` when the mode or the link make it divide by zero or go negative.
pub fn dp_config(t: &HeadTiming, link: &DpLink) -> Option<DpConfig> {
    let khz = (t.hz / 1000) as i64;
    let link_kbps = link.bw as i64 * 27000;
    let symbol = 100_000i64;
    let nr = link.nr as i64;
    let ef = link.ef as i64;
    let depth = t.depth_bits() as i64;
    if khz == 0 || nr == 0 {
        return None;
    }
    let h = (t.hblanke as i64 + t.htotal as i64 - t.hblanks as i64 - 7) * link_kbps / khz - 3 * ef - 12 / nr;
    let v = (t.vblanks as i64 - t.vblanke as i64 - 25) * link_kbps / khz - (36 / nr + 3) - 1;
    let link_data_rate = (khz * depth / 8) / nr;
    let link_ratio = link_data_rate * symbol / link_kbps;
    let best_tu = 64;
    let unk = (symbol - link_ratio) * best_tu * link_ratio / symbol / symbol + 6;
    if h < 0 || v < 0 || !(0..=0x3f).contains(&unk) {
        return None;
    }
    Some(DpConfig { h: h as u16, v: v as u32, watermark: unk as u8 })
}

// ---------------------------------------------------------------------------
// VBIOS IED tables (`subdev/bios/disp.c`)
// ---------------------------------------------------------------------------

/// An output's IED entry (`nvbios_outp_parse`, `disp.c:66-85`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Iedt {
    /// Where the entry starts (its configuration list follows).
    pub data: u32,
    /// `script[1]` = OffInt1, `script[2]` = OffInt2 (`nv50.c:1084-1107`).
    pub script: [u16; 3],
    hdr: u8,
    cnt: u8,
}

/// `nvbios_outp_match` (`disp.c:88-100`) over the display table of BIT 'U'
/// (`disp.c:27-52`): the first entry of type `hasht` whose mask covers
/// `hashm`.
pub fn iedt_match(bios: &Bios, hasht: u16, hashm: u16) -> Option<Iedt> {
    let u = bios.bit_entry(b'U').filter(|u| u.version == 1)?;
    let table = bios.rd16(u.offset as u32) as u32;
    if table == 0 || !(0x20..=0x22).contains(&bios.rd08(table)) {
        return None;
    }
    let ver = bios.rd08(table);
    let hdr = bios.rd08(table + 1) as u32;
    let len = bios.rd08(table + 2) as u32;
    let cnt = bios.rd08(table + 3) as u32;
    let sub = bios.rd08(table + 4);
    if len < 2 || sub < 0x0a {
        return None;
    }
    for idx in 0..cnt {
        let data = bios.rd16(table + hdr + idx * len) as u32;
        if data == 0 {
            continue;
        }
        let kind = bios.rd16(data);
        let mut mask = bios.rd32(data + 2) as u16;
        if ver <= 0x20 {
            mask |= 0x00c0;
        }
        if kind == hasht && mask & hashm == hashm {
            let script = [bios.rd16(data + 6), bios.rd16(data + 8), if sub >= 0x0c { bios.rd16(data + 10) } else { 0 }];
            return Some(Iedt { data, script, hdr: sub, cnt: bios.rd08(data + 5) });
        }
    }
    None
}

impl Iedt {
    /// `nvbios_ocfg_match` (`disp.c:131-142`): the `clkcmp` lists (OnInt2,
    /// OnInt3) for protocol `proto_evo` and `flags`.
    pub fn ocfg(&self, bios: &Bios, proto_evo: u8, flags: u8) -> Option<[u16; 2]> {
        (0..self.cnt as u32).find_map(|idx| {
            let e = self.data + self.hdr as u32 + idx * 6;
            let proto = bios.rd08(e);
            ((proto == proto_evo || proto == 0xff) && bios.rd08(e + 1) == flags).then(|| [bios.rd16(e + 2), bios.rd16(e + 4)])
        })
    }
}

/// `nvbios_oclk_match` (`disp.c:144-154`): the script for the first
/// threshold (10 kHz units) the clock reaches.
pub fn oclk_match(bios: &Bios, mut cmp: u16, khz: u32) -> Option<u16> {
    // The list ends at a 0 threshold, which every clock reaches; the bound
    // is against a table with none.
    for _ in 0..64 {
        if cmp == 0 {
            return None;
        }
        if khz / 10 >= bios.rd16(cmp as u32) as u32 {
            return Some(bios.rd16(cmp as u32 + 2));
        }
        cmp += 4;
    }
    None
}

// ---------------------------------------------------------------------------
// The supervisor work
// ---------------------------------------------------------------------------

/// What the caller knows about the display, read once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Heads and SORs present (`0x610060` bits 0-7 and 8-15,
    /// `gv100.c:234-240,320-326`).
    pub heads: u8,
    pub sors: u8,
    /// Heads this driver works on.
    pub owned: u8,
    /// `(sor, output)`: the output routed to each SOR (`route_get`).
    pub routes: Vec<(u8, Outp)>,
}

/// One thing a supervisor did or could not do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A head this driver does not own had work: released untouched.
    Foreign { head: u8, mask: u32 },
    /// x.0: no SOR drives the head now (`nv50.c:1124-1135`).
    NothingAttached { head: u8, stage: u8 },
    /// 2.2/3.0: no SOR will drive it (`nv50.c:1110-1122`).
    NothingToAttach { head: u8, stage: u8 },
    /// The SOR has no routed output of its protocol (nouveau's
    /// `ior->arm/asy.outp` is NULL: "nothing (to) attach(ed)").
    NoOutput { head: u8, stage: u8, sor: u8 },
    /// The VBIOS has no IED script for this output/protocol/clock
    /// (`nv50.c:1014-1070`: nouveau logs "missing IEDT" and goes on).
    NoScript { head: u8, stage: u8, sor: u8 },
    /// An IED script ran (`addr` 0 = empty).
    Script { head: u8, stage: u8, sor: u8, addr: u16, result: Result<init::Stats, ScriptError> },
    /// 2.1: the head's pixel clock changes. VPLL programming is phase 5.5:
    /// not done.
    ClockNotPorted { head: u8, khz: u32 },
    /// 2.2 on a protocol other than DP: phase 5.8, nothing done after the
    /// script.
    AttachNotPorted { head: u8, sor: u8, proto: Proto },
    /// 2.2: DP packing programmed for this link.
    Dp { head: u8, sor: u8, link: DpLink, config: DpConfig },
    /// 2.2: the DP link or the mode could not be read or computed.
    DpFailed { head: u8, sor: u8, link: Option<DpLink> },
    /// 2.2: RG clock divider and SOR clock programmed.
    Clocks { head: u8, sor: u8 },
}

/// One supervisor's work.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// 1, 2 or 3 (0: the pending bits named none).
    pub stage: u8,
    /// `SUPER_STAT` and each present head's work mask, as read.
    pub stat: u32,
    pub masks: [u32; HEADS_MAX as usize],
    pub events: Vec<Event>,
}

/// The state kept from supervisor 1 to 3.
pub struct Supervisor {
    pub cfg: Config,
    head_arm: [HeadTiming; HEADS_MAX as usize],
    head_asy: [HeadTiming; HEADS_MAX as usize],
    sor_arm: [SorState; SORS_MAX as usize],
    sor_asy: [SorState; SORS_MAX as usize],
}

const NO_TIMING: HeadTiming =
    HeadTiming { htotal: 0, vtotal: 0, hsynce: 0, vsynce: 0, hblanke: 0, vblanke: 0, hblanks: 0, vblanks: 0, hz: 0, depth_code: 0 };

fn bits(mask: u8, max: u32) -> impl Iterator<Item = u32> {
    (0..max).filter(move |i| mask & (1 << i) != 0)
}

impl Supervisor {
    pub fn new(cfg: Config) -> Supervisor {
        Supervisor {
            cfg,
            head_arm: [NO_TIMING; HEADS_MAX as usize],
            head_asy: [NO_TIMING; HEADS_MAX as usize],
            sor_arm: [SorState::default(); SORS_MAX as usize],
            sor_asy: [SorState::default(); SORS_MAX as usize],
        }
    }

    /// The SOR state kept from the last supervisor 1 (ARM, ASSEMBLY).
    pub fn sor_state(&self, sor: u8) -> (SorState, SorState) {
        (self.sor_arm[sor as usize], self.sor_asy[sor as usize])
    }

    /// Runs the supervisor named by `pending` (the CTRL_DISP bits the
    /// interrupt acknowledged; the lowest wins, as in `gv100.c:852-881`)
    /// and releases it. Always releases, whatever failed: a supervisor left
    /// waiting stops the display.
    pub fn service(&mut self, m: &dyn Mmio, bios: &Bios, pending: u32) -> Report {
        let mut r = Report { stat: m.rd32(SUPER_STAT), ..Default::default() };
        for head in bits(self.cfg.heads, HEADS_MAX) {
            r.masks[head as usize] = m.rd32(SUPER_HEAD + head * 4);
        }
        let (heads, owned) = (self.cfg.heads, self.cfg.owned);
        let work = |r: &Report, bit: u32| {
            bits(heads & owned, HEADS_MAX).filter(|&h| r.masks[h as usize] & bit != 0).collect::<Vec<u32>>()
        };
        let foreign = |r: &mut Report, cfg: &Config| {
            for head in bits(cfg.heads & !cfg.owned, HEADS_MAX) {
                let mask = r.masks[head as usize];
                if mask & (HEAD_OUTPUT | HEAD_CLOCK) != 0 {
                    r.events.push(Event::Foreign { head: head as u8, mask });
                }
            }
        };
        if pending & 1 != 0 {
            r.stage = 1;
            self.read_states(m);
            foreign(&mut r, &self.cfg);
            for head in work(&r, HEAD_OUTPUT) {
                self.ied_off(m, bios, &mut r, head, 1);
            }
        } else if pending & 2 != 0 {
            r.stage = 2;
            foreign(&mut r, &self.cfg);
            for head in work(&r, HEAD_OUTPUT) {
                self.ied_off(m, bios, &mut r, head, 2);
            }
            for head in work(&r, HEAD_CLOCK) {
                let khz = self.head_asy[head as usize].hz / 1000;
                if khz != 0 {
                    r.events.push(Event::ClockNotPorted { head: head as u8, khz });
                }
            }
            for head in work(&r, HEAD_OUTPUT) {
                self.super_2_2(m, bios, &mut r, head);
            }
        } else if pending & 4 != 0 {
            r.stage = 3;
            foreign(&mut r, &self.cfg);
            for head in work(&r, HEAD_OUTPUT) {
                // 3.0 (`nv50.c:1137-1154`); GA102's SOR has no `war_3`.
                if let Some(sor) = self.ior_asy(head) {
                    let khz = self.head_asy[head as usize].hz / 1000;
                    self.ied_on(m, bios, &mut r, head, sor, 1, khz);
                } else {
                    r.events.push(Event::NothingToAttach { head: head as u8, stage: 30 });
                }
            }
        }
        for head in bits(self.cfg.heads, HEADS_MAX) {
            m.wr32(SUPER_HEAD + head * 4, 0);
        }
        m.wr32(SUPER_STAT, SUPER_RELEASE);
        r
    }

    /// `nv50_disp_super_1` (`nv50.c:1347-1362`).
    fn read_states(&mut self, m: &dyn Mmio) {
        for head in bits(self.cfg.heads, HEADS_MAX) {
            self.head_arm[head as usize] = HeadTiming::read(m, head, true);
            self.head_asy[head as usize] = HeadTiming::read(m, head, false);
        }
        for sor in bits(self.cfg.sors, SORS_MAX) {
            self.sor_arm[sor as usize] = SorState::read(m, sor, true);
            self.sor_asy[sor as usize] = SorState::read(m, sor, false);
        }
    }

    /// `nv50_disp_super_ior_arm` / `_asy` (`nv50.c:1109-1135`).
    fn ior_arm(&self, head: u32) -> Option<u8> {
        bits(self.cfg.sors, SORS_MAX).find(|&s| self.sor_arm[s as usize].head & (1 << head) != 0).map(|s| s as u8)
    }
    fn ior_asy(&self, head: u32) -> Option<u8> {
        bits(self.cfg.sors, SORS_MAX).find(|&s| self.sor_asy[s as usize].head & (1 << head) != 0).map(|s| s as u8)
    }

    /// The output routed to `sor`, if it runs `state`'s protocol.
    fn outp(&self, sor: u8, state: &SorState) -> Option<Outp> {
        self.cfg.routes.iter().find(|(s, o)| *s == sor && o.proto() == state.proto).map(|(_, o)| *o)
    }

    /// `nv50_disp_super_iedt` (`nv50.c:1013-1026`).
    fn iedt(bios: &Bios, head: u32, o: &Outp) -> Option<Iedt> {
        let l = if o.link == 0 { 0 } else { o.link.trailing_zeros() as u16 + 1 };
        iedt_match(bios, o.hasht, (0x100 << head) | (l << 6) | o.or as u16)
    }

    /// 1.0 / 2.0: `OffInt<id>` of the output the head loses
    /// (`nv50_disp_super_ied_off`, `nv50.c:1084-1107`).
    fn ied_off(&self, m: &dyn Mmio, bios: &Bios, r: &mut Report, head: u32, id: usize) {
        let stage = id as u8 * 10;
        let h = head as u8;
        let Some(sor) = self.ior_arm(head) else {
            r.events.push(Event::NothingAttached { head: h, stage });
            return;
        };
        let arm = self.sor_arm[sor as usize];
        let Some(o) = self.outp(sor, &arm) else {
            r.events.push(Event::NoOutput { head: h, stage, sor });
            return;
        };
        let Some(iedt) = Self::iedt(bios, head, &o) else {
            r.events.push(Event::NoScript { head: h, stage, sor });
            return;
        };
        let addr = iedt.script[id];
        let t = Target { or: Some(sor), link: arm.link, head: Some(h), conn: o.conn };
        r.events.push(Event::Script { head: h, stage, sor, addr, result: init::run(m, bios, t, addr as u32) });
    }

    /// 2.2 (id 0) / 3.0 (id 1): `OnInt<id+2>` for the pixel clock
    /// (`nv50_disp_super_ied_on`, `nv50.c:1028-1082`).
    #[allow(clippy::too_many_arguments)]
    fn ied_on(&self, m: &dyn Mmio, bios: &Bios, r: &mut Report, head: u32, sor: u8, id: usize, khz: u32) {
        let stage = if id == 0 { 22 } else { 30 };
        let h = head as u8;
        let asy = self.sor_asy[sor as usize];
        let Some(o) = self.outp(sor, &asy) else {
            r.events.push(Event::NoOutput { head: h, stage, sor });
            return;
        };
        let mut flags = 0u8;
        if asy.proto == Proto::Lvds && self.head_asy[head as usize].depth_bits() == 24 {
            flags |= 0x02;
        }
        if asy.link == 3 {
            flags |= 0x01;
        }
        let script = Self::iedt(bios, head, &o)
            .and_then(|iedt| iedt.ocfg(bios, asy.proto_evo, flags))
            .and_then(|clkcmp| oclk_match(bios, clkcmp[id], khz));
        let Some(addr) = script else {
            r.events.push(Event::NoScript { head: h, stage, sor });
            return;
        };
        let t = Target { or: Some(sor), link: asy.link, head: Some(h), conn: o.conn };
        r.events.push(Event::Script { head: h, stage, sor, addr, result: init::run(m, bios, t, addr as u32) });
    }

    /// 2.2 (`nv50_disp_super_2_2`, `nv50.c:1262-1305`).
    fn super_2_2(&self, m: &dyn Mmio, bios: &Bios, r: &mut Report, head: u32) {
        let h = head as u8;
        let Some(sor) = self.ior_asy(head) else {
            r.events.push(Event::NothingToAttach { head: h, stage: 22 });
            return;
        };
        let asy = self.sor_asy[sor as usize];
        let t = self.head_asy[head as usize];
        self.ied_on(m, bios, r, head, sor, 0, t.hz / 1000);
        if asy.proto != Proto::Dp {
            r.events.push(Event::AttachNotPorted { head: h, sor, proto: asy.proto });
            return;
        }
        // RG clock divider (`gf119_head_rgclk`, `gf119.c:416-420`): a GV100+
        // SOR state never sets `rgdiv`, so 0 (`nv50.c:126` is the PIOR's).
        m.mask(0x61_2200 + head * 0x800, 0x0000_000f, 0);
        let link = DpLink::read(m, sor as u32, asy.link);
        match link.filter(|l| !l.mst).and_then(|l| dp_config(&t, &l).map(|c| (l, c))) {
            Some((link, c)) => {
                // `gv100_sor_dp_audio_sym` / `_watermark` (`gv100.c:53-70`).
                let hoff = head * 0x800;
                m.mask(0x61_6568 + hoff, 0x0000_ffff, c.h as u32);
                m.mask(0x61_656c + hoff, 0x00ff_ffff, c.v);
                m.mask(0x61_6550 + hoff, 0x0c00_003f, 0x0800_0000 | c.watermark as u32);
                r.events.push(Event::Dp { head: h, sor, link, config: c });
            }
            None => r.events.push(Event::DpFailed { head: h, sor, link }),
        }
        // `ga102_sor_clock` (`ga102.c:104-116`): DP, so no TMDS divider.
        m.wr32(0x00_ec08 + sor as u32 * 0x10, 0);
        m.wr32(0x00_ec04 + sor as u32 * 0x10, 0);
        r.events.push(Event::Clocks { head: h, sor });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmio::testing::{ReplayMmio, TableMmio};
    use crate::vbios::tests::oracle_vbios;
    use alloc::string::String;
    use alloc::vec;

    const ROUND1: &str = include_str!("../fixtures/super-round1.txt");
    const ROUND2: &str = include_str!("../fixtures/super-round2.txt");

    /// DCB output 04 of the target board: DP-3, the ASUS (nouveau's dmesg:
    /// "outp 04:0006:0f82: type 06 loc 0 or 2 link 2 con 2", "on SOR-1 link
    /// 2"; connector 2 is type 0x46, DP).
    fn dp3() -> Outp {
        Outp { dcb: 4, kind: OUTPUT_DP, or: 2, link: 2, hasht: 0x0006, hashm: 0x0f82, conn: Some(0x46) }
    }

    fn cfg() -> Config {
        Config { heads: 0x0f, sors: 0x0f, owned: 0x01, routes: vec![(1, dp3())] }
    }

    fn bios() -> Option<Bios> {
        oracle_vbios().map(|rom| Bios::new(rom).unwrap())
    }

    /// Writes the fixture expects from the heads this driver owns: every
    /// write of the trace minus head 1's (HDMI on SOR-0 in round 2: VPLL1,
    /// SOR-0 registers, head 1's RG clock and IED register), which nouveau
    /// did and this phase does not.
    fn owned_writes(m: &ReplayMmio) -> Vec<(u32, u32)> {
        let head1 = |o: u32| {
            (0x61_c000..0x61_c800).contains(&o)
                || [0xef40, 0xef44, 0xef58, 0xe9c4, 0x61_2300, 0x61_2408, 0x61_2a00, 0xec08, 0xec04, 0x61_6d40].contains(&o)
        };
        m.expected_writes.iter().copied().filter(|(o, _)| !head1(*o)).collect()
    }

    fn fmt(w: &[(u32, u32)]) -> String {
        w.iter().map(|(o, v)| alloc::format!("W {o:#08x} {v:#010x}\n")).collect()
    }

    #[test]
    fn round1_detach_replays_the_trace() {
        let Some(bios) = bios() else { return };
        let m = ReplayMmio::from_extract(ROUND1);
        let mut s = Supervisor::new(cfg());
        let r1 = s.service(&m, &bios, 1);
        assert_eq!((r1.stage, r1.masks[0]), (1, 0x1100));
        // OffInt1 of DP-3 is an empty script (VBIOS 0x78f9: DONE).
        assert_eq!(r1.events, vec![Event::Script { head: 0, stage: 10, sor: 1, addr: 0x78f9, result: Ok(init::Stats { opcodes: 1, writes: 0 }) }]);
        assert_eq!(s.sor_state(1), (SorState::from_ctrl(0x901), SorState::from_ctrl(0)));
        let r2 = s.service(&m, &bios, 2);
        // OffInt2 (0x7355): clear 0x616540 bit 0, the 0x21234 no-op the
        // trace shows, 0xd604 = 0; then 2.2 has nothing to attach.
        assert_eq!(
            r2.events,
            vec![
                Event::Script { head: 0, stage: 20, sor: 1, addr: 0x7355, result: Ok(init::Stats { opcodes: 8, writes: 3 }) },
                Event::NothingToAttach { head: 0, stage: 22 },
            ]
        );
        let r3 = s.service(&m, &bios, 4);
        assert_eq!(r3.events, vec![Event::NothingToAttach { head: 0, stage: 30 }]);
        assert_eq!(fmt(&m.writes.borrow()), fmt(&m.expected_writes));
    }

    #[test]
    fn round2_attach_replays_the_trace_for_head0() {
        let Some(bios) = bios() else { return };
        let mut m = ReplayMmio::from_extract(ROUND2);
        // The link nouveau trained just before (modeset-4-link.txt: last
        // 0x612b00 write), read here instead of remembered.
        m.fallback = vec![(0x61_2b00, 0x0309_f040)];
        let mut s = Supervisor::new(cfg());
        let r1 = s.service(&m, &bios, 1);
        assert_eq!(r1.masks[..2], [0x1100, 0x11100]);
        assert_eq!(
            r1.events,
            vec![Event::Foreign { head: 1, mask: 0x11100 }, Event::NothingAttached { head: 0, stage: 10 }]
        );
        let r2 = s.service(&m, &bios, 2);
        let link = DpLink { bw: 0x14, nr: 4, ef: true, mst: false };
        assert_eq!(
            r2.events,
            vec![
                Event::Foreign { head: 1, mask: 0x11100 },
                Event::NothingAttached { head: 0, stage: 20 },
                Event::Script { head: 0, stage: 22, sor: 1, addr: 0x71b9, result: Ok(init::Stats { opcodes: 15, writes: 1 }) },
                Event::Dp { head: 0, sor: 1, link, config: DpConfig { h: 0x3da, v: 0xeef, watermark: 0x10 } },
                Event::Clocks { head: 0, sor: 1 },
            ]
        );
        let r3 = s.service(&m, &bios, 4);
        assert_eq!(
            r3.events,
            vec![
                Event::Foreign { head: 1, mask: 0x11100 },
                Event::Script { head: 0, stage: 30, sor: 1, addr: 0x6821, result: Ok(init::Stats { opcodes: 1, writes: 0 }) },
            ]
        );
        assert_eq!(fmt(&m.writes.borrow()), fmt(&owned_writes(&m)));
    }

    #[test]
    fn a_failing_script_still_releases() {
        // Bits for head 0, a VBIOS with no display table: the work finds
        // nothing, the release happens anyway.
        let bios = Bios::new(crate::vbios::tests::tiny_image()).unwrap();
        let m = TableMmio::new(&[(SUPER_STAT, 0x10), (SUPER_HEAD, 0x1000), (SUPER_HEAD + 4, 0), (SUPER_HEAD + 8, 0), (SUPER_HEAD + 12, 0)]);
        let mut s = Supervisor::new(cfg());
        s.sor_arm[1] = SorState::from_ctrl(0x901);
        let r = s.service(&m, &bios, 2);
        assert_eq!(r.events[0], Event::NoScript { head: 0, stage: 20, sor: 1 });
        let w = m.writes.borrow();
        assert_eq!(w[w.len() - 5..], [(SUPER_HEAD, 0), (SUPER_HEAD + 4, 0), (SUPER_HEAD + 8, 0), (SUPER_HEAD + 12, 0), (SUPER_STAT, SUPER_RELEASE)]);
    }

    #[test]
    fn iedt_lookup_on_the_oracle_vbios() {
        let Some(bios) = bios() else { return };
        // DP-3 on head 0: m = 0x100 | ffs(link 2) << 6 | or 2 = 0x182.
        let iedt = iedt_match(&bios, 0x0006, 0x0182).unwrap();
        assert_eq!(iedt.script, [0x78f8, 0x78f9, 0x7355]);
        // Protocol 9 (DP, sublink B), flags 0: OnInt2 / OnInt3 at any clock.
        let clk = iedt.ocfg(&bios, 9, 0).unwrap();
        assert_eq!(oclk_match(&bios, clk[0], 148_500), Some(0x71b9));
        assert_eq!(oclk_match(&bios, clk[1], 148_500), Some(0x6821));
        // The HP on SOR-0 (outp 07, TMDS, sublink A) on head 1, as round 2.
        let hdmi = iedt_match(&bios, 0x0002, 0x0242).unwrap();
        let clk = hdmi.ocfg(&bios, 1, 0).unwrap();
        assert_eq!(oclk_match(&bios, clk[0], 148_500), Some(0x5f10));
    }

    #[test]
    fn route_get_reads_the_pad_routing() {
        // trace-nogsp at nouveau's init: 0x612488 = 0x1912 (pad 1 sublink
        // B → SOR-1, link bit set), the others unrouted ("no route").
        let m = TableMmio::new(&[(0x61_2408, 0x800), (0x61_2488, 0x1912), (0x61_2508, 0x800), (0x61_2588, 0x800)]);
        assert_eq!(route_get(&m, &dp3()), Some(1));
        let hdmi = Outp { dcb: 7, kind: OUTPUT_TMDS, or: 2, link: 1, hasht: 0x0002, hashm: 0x0f42, conn: Some(0x61) };
        assert_eq!(route_get(&m, &Outp { or: 1, ..hdmi }), None);
    }

    #[test]
    fn dp_link_and_config_match_the_trace() {
        let m = TableMmio::new(&[(0x61_2b00, 0x0309_f040), (0x61_c98c, 0x000f_4001)]);
        let link = DpLink::read(&m, 1, 2).unwrap();
        assert_eq!(link, DpLink { bw: 0x14, nr: 4, ef: true, mst: false });
        // CEA 1080p60 as the GOP and nouveau set it (fixtures).
        let t = HeadTiming { htotal: 2200, vtotal: 1125, hsynce: 43, vsynce: 4, hblanke: 191, vblanke: 40, hblanks: 2111, vblanks: 1120, hz: 148_500_000, depth_code: 4 };
        assert_eq!(dp_config(&t, &link), Some(DpConfig { h: 0x3da, v: 0xeef, watermark: 0x10 }));
        assert_eq!(dp_config(&HeadTiming { hz: 0, ..t }, &link), None);
    }

    #[test]
    fn sor_state_decodes_the_control() {
        assert_eq!(SorState::from_ctrl(0x901), SorState { proto_evo: 9, proto: Proto::Dp, link: 2, head: 1 });
        assert_eq!(SorState::from_ctrl(0x102), SorState { proto_evo: 1, proto: Proto::Tmds, link: 1, head: 2 });
        assert_eq!(SorState::from_ctrl(0x100).head, 0);
    }
}
