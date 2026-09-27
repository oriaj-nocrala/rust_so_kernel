//! DisplayPort link training (phase 5.6 of the plan), without GSP.
//!
//! What nouveau does to (re)train the link of a DP output whose SOR is not
//! driving a head, ported from the pinned Linux v7.2.2 (paths under
//! `drivers/gpu/drm/nouveau/`):
//! - the DP table of the VBIOS (BIT 'd', `nvkm/subdev/bios/dp.c`): per
//!   output, the scripts that power the link down (`DisableLT`), set the
//!   spread (`EnableSpread`/`DisableSpread`), prepare the SOR for a rate
//!   (`BeforeLinkTraining` and the `lnkcmp` list, both indexed by link
//!   rate on Ampere) and finish (`AfterLinkTraining`), plus the drive
//!   table (voltage swing / pre-emphasis → the SOR's lane registers);
//! - `nvkm_dp_train` with `retrain = false` (`nvkm/engine/disp/dp.c:
//!   312-526`): scripts, `ga102_sor_dp_links` (`ga102.c:32-70`),
//!   `g94_sor_dp_power` (`g94.c:101-118`), then clock recovery (TPS1)
//!   and channel equalisation (TPS2/3/4) over AUX, adjusting each lane's
//!   drive to what the sink asks (`gm200_sor_dp_drive`, `gm200.c:33-55`;
//!   `gm107_sor_dp_pattern`, `gm107.c:31-53`);
//! - the sink power-up DRM does first (`nouveau_dp.c:423-429`).
//!
//! All of it runs against the [`Mmio`] seam and is checked against
//! `trace-nogsp` register for register (`fixtures/dp-train.txt`). Not
//! ported (refused before touching anything): LTTPRs, the post-LT adjust
//! of DP 1.3 sinks without TPS4 (`nouveau_dp.c:321-400`), eDP rate
//! tables, MST.

use alloc::vec::Vec;

use crate::aux::{Aux, AuxError, RECEIVER_CAP_SIZE};
use crate::init::{self, ScriptError, Stats, Target};
use crate::vbios::Bios;
use crate::Mmio;

// DPCD fields (`nvkm/engine/disp/dp.h:12-74`).
pub const DPCD_REV: usize = 0x00;
pub const DPCD_MAX_LINK_RATE: usize = 0x01;
pub const DPCD_RC02: usize = 0x02;
pub const RC02_ENHANCED_FRAME_CAP: u8 = 0x80;
pub const RC02_TPS3_SUPPORTED: u8 = 0x40;
pub const RC02_MAX_LANE_COUNT: u8 = 0x1f;
/// `DP_POST_LT_ADJ_REQ_SUPPORTED` (`include/drm/display/drm_dp.h:118`).
pub const RC02_POST_LT_ADJ_REQ_SUPPORTED: u8 = 0x20;
pub const DPCD_RC03: usize = 0x03;
pub const RC03_TPS4_SUPPORTED: u8 = 0x80;
pub const RC03_MAX_DOWNSPREAD: u8 = 0x01;
pub const DPCD_RC0E: usize = 0x0e;
pub const RC0E_AUX_RD_INTERVAL: u8 = 0x7f;
const LC00_LINK_BW_SET: u32 = 0x100;
const LC01_ENHANCED_FRAME_EN: u8 = 0x80;
const LC02: u32 = 0x102;
const LC02_TRAINING_PATTERN_SET: u8 = 0x0f;
const LC02_SCRAMBLING_DISABLE: u8 = 0x20;
const LC03: u32 = 0x103;
const LC03_MAX_SWING_REACHED: u8 = 0x04;
const LC03_VOLTAGE_SWING_SET: u8 = 0x03;
const LC0F: u32 = 0x10f;
const LC0F_LANE0_MAX_POST_CURSOR2_REACHED: u8 = 0x04;
const LS02: u32 = 0x202;
const LS02_LANE0_SYMBOL_LOCKED: u8 = 0x04;
const LS02_LANE0_CHANNEL_EQ_DONE: u8 = 0x02;
const LS02_LANE0_CR_DONE: u8 = 0x01;
const LS04_INTERLANE_ALIGN_DONE: u8 = 0x01;
const LS06: u32 = 0x206;
/// `DPCD_LS0C` (`dp.h:83`): post-cursor2 adjust requests.
const LS0C: u32 = 0x20c;
/// `DP_SET_POWER` (`drm_dp.h:1014-1017`).
pub const SET_POWER: u32 = 0x600;
const SET_POWER_MASK: u8 = 0x3;
const SET_POWER_D0: u8 = 0x1;
/// `DP_LT_TUNABLE_PHY_REPEATER_FIELD_DATA_STRUCTURE_REV` (`drm_dp.h:1508`):
/// nouveau counts LTTPRs only if this reads >= 0x14 (`nouveau_dp.c:45-57`).
pub const LTTPR_REV: u32 = 0xf0000;

// ---------------------------------------------------------------------------
// The VBIOS DP table (`nvkm/subdev/bios/dp.c`)
// ---------------------------------------------------------------------------

/// `nvbios_dp_table` (`dp.c:28-57`): BIT 'd' version 1 points at it.
fn dp_table(bios: &Bios) -> Option<(u32, u8, u8, u8, u8)> {
    let d = bios.bit_entry(b'd').filter(|d| d.version == 1 && d.length >= 2)?;
    let data = bios.rd16(d.offset as u32) as u32;
    if data == 0 {
        return None;
    }
    let ver = bios.rd08(data);
    matches!(ver, 0x20 | 0x21 | 0x30 | 0x40 | 0x41 | 0x42).then(|| (data, ver, bios.rd08(data + 1), bios.rd08(data + 2), bios.rd08(data + 3)))
}

/// An output's DP table entry (`struct nvbios_dpout`,
/// `include/nvkm/subdev/bios/dp.h:8-14`). Versions 0x40-0x42 only (this
/// board's is 0x42); older layouts are not parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DpInfo {
    /// Where the entry is, the table version and the entry header length.
    pub data: u32,
    pub ver: u8,
    pub hdr: u8,
    pub flags: u8,
    /// 0 BeforeLinkTraining (a rate-indexed list on Ampere, `dp.c:38-42,
    /// 386-397`), 1 AfterLinkTraining, 2 EnableSpread, 3 DisableSpread,
    /// 4 DisableLT (`dp.c:434-470,528-537`).
    pub script: [u16; 5],
    /// Rate-indexed list of the scripts that set the SOR up for a rate.
    pub lnkcmp: u16,
}

/// `nvbios_dpout_match` (`dp.c:137-150`) with `nvbios_dpout_parse`
/// (`dp.c:59-135`): the first entry of type `hasht` whose mask covers
/// `hashm` (both from the DCB output, `dcb.c:108-118`).
pub fn dpout_match(bios: &Bios, hasht: u16, hashm: u16) -> Option<DpInfo> {
    let (table, ver, hdr, len, cnt) = dp_table(bios)?;
    if !(0x40..=0x42).contains(&ver) {
        return None;
    }
    let ohdr = bios.rd08(table + 4);
    for idx in 0..cnt as u32 {
        let data = bios.rd16(table + hdr as u32 + idx * len as u32) as u32;
        if data == 0 {
            continue;
        }
        let kind = bios.rd16(data);
        let mask = bios.rd16(data + 2);
        if kind == hasht && mask & hashm == hashm {
            return Some(DpInfo {
                data,
                ver,
                hdr: ohdr,
                flags: bios.rd08(data + 4),
                script: [bios.rd16(data + 5), bios.rd16(data + 7), bios.rd16(data + 0x0b), bios.rd16(data + 0x0d), bios.rd16(data + 0x0f)],
                lnkcmp: bios.rd16(data + 9),
            });
        }
    }
    None
}

/// One drive setting for a lane (`struct nvbios_dpcfg`,
/// `include/nvkm/subdev/bios/dp.h:25-30`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DpCfg {
    pub pc: u8,
    pub dc: u8,
    pub pe: u8,
    pub tx_pu: u8,
}

/// `nvbios_dpcfg_match` (`dp.c:206-232`) for versions 0x40-0x42: entry
/// `pc * 10 + {0, 4, 7, 9}[vs] + pe`, offset by the output's byte 0x11
/// (x40 on 0x40/0x41 with a long header, x10 on 0x42), in the table that
/// follows the output pointers (`nvbios_dpcfg_entry`, `dp.c:152-168`).
pub fn dpcfg_match(bios: &Bios, info: &DpInfo, pc: u8, vs: u8, pe: u8) -> Option<DpCfg> {
    const VSOFF: [u32; 4] = [0, 4, 7, 9];
    let mut idx = pc as u32 * 10 + VSOFF[(vs & 3) as usize] + pe as u32;
    if (0x40..=0x41).contains(&info.ver) && info.hdr >= 0x12 {
        idx += bios.rd08(info.data + 0x11) as u32 * 40;
    } else if info.ver >= 0x42 {
        idx += bios.rd08(info.data + 0x11) as u32 * 10;
    }
    let (table, _, hdr, len, cnt) = dp_table(bios)?;
    let base = hdr as u32 + len as u32 * cnt as u32;
    let elen = bios.rd08(table + 6) as u32;
    let ecnt = bios.rd08(table + 7) as u32 * bios.rd08(table + 5) as u32;
    if idx >= ecnt {
        return None;
    }
    let e = table + base + idx * elen;
    Some(match info.ver {
        0x42 => DpCfg { pc: 0, dc: bios.rd08(e), pe: bios.rd08(e + 1), tx_pu: bios.rd08(e + 2) },
        _ => DpCfg { pc: bios.rd08(e), dc: bios.rd08(e + 1), pe: bios.rd08(e + 2), tx_pu: bios.rd08(e + 3) },
    })
}

/// A rate-indexed script list (`dp.c:386-390,405-408`, version >= 0x30):
/// 3-byte entries `(rate, script)`, highest rate first; the first whose rate
/// the link's does not exceed. Bounded (nouveau is not): a list is VBIOS
/// data.
pub fn rate_script(bios: &Bios, list: u16, bw: u8) -> Option<u16> {
    let mut at = list as u32;
    if at == 0 {
        return None;
    }
    for _ in 0..16 {
        let rate = bios.rd08(at);
        if bw >= rate {
            return Some(bios.rd16(at + 1));
        }
        at += 3;
    }
    None
}

// ---------------------------------------------------------------------------
// Training
// ---------------------------------------------------------------------------

/// The SOR clock select for a link rate (`ga102_sor_dp_links`,
/// `ga102.c:41-53`); `None` = a rate GA102 cannot run.
pub fn clksor(bw: u8) -> Option<u32> {
    Some(match bw {
        0x06 => 0x0000_0000,
        0x0a => 0x0004_0000,
        0x14 => 0x0008_0000,
        0x1e => 0x000c_0000,
        0x08 => 0x0010_0000,
        0x09 => 0x0014_0000,
        0x0c => 0x0018_0000,
        0x10 => 0x001c_0000,
        _ => return None,
    })
}

/// A link configuration: lanes and rate (DPCD units of 0.27 Gb/s).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkConfig {
    pub nr: u8,
    pub bw: u8,
}

/// Why a configuration is refused before anything is touched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// Not 1, 2 or 4 lanes, or more than the sink or the board has.
    Lanes { nr: u8, sink: u8, board: u8 },
    /// Not a standard rate (RBR/HBR/HBR2/HBR3), or above the sink's or the
    /// board's maximum.
    Rate { bw: u8, sink: u8, board: u8 },
    /// The sink wants the post-LT adjust sequence (`nouveau_dp.c:327-329`),
    /// not ported.
    PostLtAdjust,
}

/// What `nouveau_dp_train` would allow (`nouveau_dp.c:88-145`: lanes and
/// rate capped by the sink and the DCB's `dpconf`; the standard rate list
/// when the sink has no rate table) for `cfg`.
pub fn check_config(dpcd: &[u8; RECEIVER_CAP_SIZE], board_nr: u8, board_bw: u8, cfg: LinkConfig) -> Result<(), ConfigError> {
    let sink_nr = dpcd[DPCD_RC02] & RC02_MAX_LANE_COUNT;
    if !matches!(cfg.nr, 1 | 2 | 4) || cfg.nr > sink_nr || cfg.nr > board_nr {
        return Err(ConfigError::Lanes { nr: cfg.nr, sink: sink_nr, board: board_nr });
    }
    let sink_bw = dpcd[DPCD_MAX_LINK_RATE];
    if !matches!(cfg.bw, 0x06 | 0x0a | 0x14 | 0x1e) || cfg.bw > sink_bw || cfg.bw > board_bw {
        return Err(ConfigError::Rate { bw: cfg.bw, sink: sink_bw, board: board_bw });
    }
    if dpcd[DPCD_RC02] & RC02_POST_LT_ADJ_REQ_SUPPORTED != 0 && dpcd[DPCD_RC03] & RC03_TPS4_SUPPORTED == 0 {
        return Err(ConfigError::PostLtAdjust);
    }
    Ok(())
}

/// Which DP-table script ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Script {
    DisableLt,
    EnableSpread,
    DisableSpread,
    BeforeLinkTraining,
    LinkRate,
    AfterLinkTraining,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrainError {
    /// A script stopped (unported opcode, missing target...): training is
    /// abandoned there, the SOR half set up.
    Script { script: Script, addr: u16, error: ScriptError },
    /// No `BeforeLinkTraining`/`lnkcmp` entry for the rate.
    NoRateScript { bw: u8 },
    /// `ga102_sor_dp_links` refuses the rate.
    Rate { bw: u8 },
    /// An AUX access the sequence cannot go on without.
    Aux { step: &'static str, error: AuxError },
    /// Clock recovery or channel equalisation never completed.
    ClockRecovery,
    ChannelEq,
}

/// What a training did (the kernel logs it).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub scripts: Vec<(Script, u16, Stats)>,
    /// `DP_SET_POWER` as read, and whether it was written to D0.
    pub sink_power: Option<u8>,
    pub sink_woken: bool,
    /// `0x61c034` reads until the SOR's lane power-up finished; `false`
    /// if it never did (nouveau goes on).
    pub power_polls: u32,
    pub power_done: bool,
    /// Loop iterations of clock recovery and channel equalisation, and the
    /// pattern the latter used (2, 3 or 4).
    pub cr_tries: u32,
    pub eq_tries: u32,
    pub eq_pattern: u8,
    /// The last lane status (`0x202..0x207`) and lane settings written
    /// (`0x103..0x106`).
    pub stat: [u8; 6],
    pub conf: [u8; 4],
    /// The last AUX error the training loops broke on (nouveau breaks out
    /// silently).
    pub aux_error: Option<AuxError>,
    pub result: Option<Result<(), TrainError>>,
}

/// The SOR the output is routed to and the sublink it runs (1 = A, 2 = B).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sor {
    pub id: u8,
    pub link: u8,
}

impl Sor {
    /// `nv50_ior_base` / `nv50_sor_link` (`ior.h:110-128`).
    fn soff(&self) -> u32 {
        self.id as u32 * 0x800
    }
    fn loff(&self) -> u32 {
        self.soff() + if self.link == 2 { 0x80 } else { 0 }
    }
}

/// Everything a training needs.
pub struct Params<'a> {
    pub bios: &'a Bios,
    pub info: &'a DpInfo,
    pub sor: Sor,
    /// For the scripts: the output's connector type and AUX channel (the
    /// OR and link come from `sor`; nouveau passes no head).
    pub conn: Option<u8>,
    pub aux: u8,
    /// The sink's receiver capabilities (`drm_dp_read_dpcd_caps`).
    pub dpcd: [u8; RECEIVER_CAP_SIZE],
    pub link: LinkConfig,
}

impl Params<'_> {
    fn target(&self) -> Target {
        Target { or: Some(self.sor.id), link: self.sor.link, head: None, conn: self.conn, aux: Some(self.aux) }
    }
}

fn run_script(m: &dyn Mmio, p: &Params, r: &mut Report, script: Script, addr: u16) -> Result<(), TrainError> {
    match init::run(m, p.bios, p.target(), addr as u32) {
        Ok(st) => {
            r.scripts.push((script, addr, st));
            Ok(())
        }
        Err(error) => Err(TrainError::Script { script, addr, error }),
    }
}

/// `nvkm_dp_disable` (`dp.c:528-537`), what nouveau's release of the
/// output runs: `DisableLT`, on the link the SOR runs now.
pub fn disable(m: &dyn Mmio, p: &Params, r: &mut Report) -> Result<(), TrainError> {
    run_script(m, p, r, Script::DisableLt, p.info.script[4])
}

/// Sink power-up (`nouveau_dp.c:423-429`, DRM's AUX policy), then
/// `nvkm_dp_train` with `retrain = false` for `p.link`. Always returns the
/// report; `r.result` says how it ended.
pub fn train(m: &dyn Mmio, p: &Params, r: &mut Report) {
    let res = train_(m, p, r);
    r.result = Some(res);
}

fn train_(m: &dyn Mmio, p: &Params, r: &mut Report) -> Result<(), TrainError> {
    let aux = Aux::new(m, p.aux);
    let mut pwr = [0u8];
    if aux.drm_read(SET_POWER, &mut pwr).is_ok() {
        r.sink_power = Some(pwr[0]);
        if pwr[0] & SET_POWER_MASK != SET_POWER_D0 {
            let v = (pwr[0] & !SET_POWER_MASK) | SET_POWER_D0;
            aux.native_write(SET_POWER, &[v]).map_err(|error| TrainError::Aux { step: "sink power", error })?;
            r.sink_woken = true;
        }
    }

    // `nvkm_dp_train_init` (`dp.c:444-470`); on Ampere BeforeLinkTraining
    // runs later, per rate.
    if p.dpcd[DPCD_RC03] & RC03_MAX_DOWNSPREAD != 0 {
        run_script(m, p, r, Script::EnableSpread, p.info.script[2])?;
    } else {
        run_script(m, p, r, Script::DisableSpread, p.info.script[3])?;
    }
    let res = train_links(m, p, r, &aux);
    // `nvkm_dp_train_fini` (`dp.c:433-442`), whatever happened.
    let fini = run_script(m, p, r, Script::AfterLinkTraining, p.info.script[1]);
    res.and(fini)
}

/// `nvkm_dp_train_links` (`dp.c:368-431`). GA106 is chipset 0x176, so
/// neither TPS3 nor TPS4 is masked off (`dp.c:381-384`).
fn train_links(m: &dyn Mmio, p: &Params, r: &mut Report, aux: &Aux<'_, dyn Mmio + '_>) -> Result<(), TrainError> {
    let bw = p.link.bw;
    if p.info.script[0] != 0 {
        let s = rate_script(p.bios, p.info.script[0], bw).ok_or(TrainError::NoRateScript { bw })?;
        run_script(m, p, r, Script::BeforeLinkTraining, s)?;
    }
    if p.info.lnkcmp != 0 {
        let s = rate_script(p.bios, p.info.lnkcmp, bw).ok_or(TrainError::NoRateScript { bw })?;
        run_script(m, p, r, Script::LinkRate, s)?;
    }
    let ef = p.dpcd[DPCD_RC02] & RC02_ENHANCED_FRAME_CAP != 0;
    links(m, p.sor, p.link, ef)?;
    power(m, p.sor, p.link.nr, r);
    train_link(m, p, r, aux, ef)
}

/// `ga102_sor_dp_links` (`ga102.c:32-70`).
fn links(m: &dyn Mmio, sor: Sor, link: LinkConfig, ef: bool) -> Result<(), TrainError> {
    let clk = clksor(link.bw).ok_or(TrainError::Rate { bw: link.bw })?;
    let mut dpctrl = ((1u32 << link.nr) - 1) << 16;
    if ef {
        dpctrl |= 0x0000_4000;
    }
    m.mask(0x61_2300 + sor.soff(), 0x007c_0000, clk);
    // "XXX": a fixed 40 ms.
    m.udelay(40_000);
    m.mask(0x61_2300 + sor.soff(), 0x0003_0000, 0x0001_0000);
    m.mask(0x61_c10c + sor.loff(), 0x0000_0003, 0x0000_0001);
    m.mask(0x61_c10c + sor.loff(), 0x401f_4000, dpctrl);
    Ok(())
}

/// `g94_sor_dp_power` (`g94.c:101-118`): lanes `0..nr` (GA102's lane map
/// is the identity, `ga102.c:74`), then wait up to 2 s for the SOR.
fn power(m: &dyn Mmio, sor: Sor, nr: u8, r: &mut Report) {
    m.mask(0x61_c130 + sor.loff(), 0x0000_000f, (1u32 << nr) - 1);
    m.mask(0x61_c034 + sor.soff(), 0x8000_0000, 0x8000_0000);
    for i in 0..200_000 {
        r.power_polls = i + 1;
        if m.rd32(0x61_c034 + sor.soff()) & 0x8000_0000 == 0 {
            r.power_done = true;
            return;
        }
        m.udelay(10);
    }
}

/// `gm107_sor_dp_pattern` (`gm107.c:31-53`).
fn sor_pattern(m: &dyn Mmio, sor: Sor, pattern: u8) {
    let data = match pattern {
        0 => 0x1010_1010,
        1 => 0x0101_0101,
        2 => 0x0202_0202,
        3 => 0x0303_0303,
        _ => 0x1b1b_1b1b,
    };
    let reg = if sor.link & 1 != 0 { 0x61_c110 } else { 0x61_c12c };
    m.mask(reg + sor.soff(), 0x1f1f_1f1f, data);
}

/// `gm200_sor_dp_drive` (`gm200.c:33-55`) for lane `ln`.
fn sor_drive(m: &dyn Mmio, sor: Sor, ln: u32, c: DpCfg) {
    let loff = sor.loff();
    let shift = ln * 8;
    let pu = (c.tx_pu & 0x0f) as u32;
    let d0 = m.rd32(0x61_c118 + loff) & !(0xff << shift);
    let d1 = m.rd32(0x61_c120 + loff) & !(0xff << shift);
    let mut d2 = m.rd32(0x61_c130 + loff);
    if (d2 & 0x0000_0f00) < (pu << 8) || ln == 0 {
        d2 = (d2 & !0x0000_0f00) | (pu << 8);
    }
    m.wr32(0x61_c118 + loff, d0 | ((c.dc as u32) << shift));
    m.wr32(0x61_c120 + loff, d1 | ((c.pe as u32) << shift));
    m.wr32(0x61_c130 + loff, d2);
    let d3 = m.rd32(0x61_c13c + loff) & !(0xff << shift);
    m.wr32(0x61_c13c + loff, d3 | ((c.pc as u32) << shift));
}

/// `struct lt_state` (`dp.c:78-89`), without repeaters.
struct Lt<'a> {
    m: &'a (dyn Mmio + 'a),
    p: &'a Params<'a>,
    aux: &'a Aux<'a, dyn Mmio + 'a>,
    stat: [u8; 6],
    conf: [u8; 4],
    pc2: bool,
    pc2stat: u8,
    pc2conf: [u8; 2],
}

impl Lt<'_> {
    fn nr(&self) -> usize {
        self.p.link.nr as usize
    }

    /// `nvkm_dp_train_sense` (`dp.c:91-129`).
    fn sense(&mut self, pc: bool, delay: u32) -> Result<(), AuxError> {
        self.m.udelay(delay);
        self.aux.nvkm_read(LS02, &mut self.stat[0..3])?;
        self.aux.nvkm_read(LS06, &mut self.stat[4..6])?;
        if pc {
            let mut b = [0u8];
            self.pc2stat = if self.aux.nvkm_read(LS0C, &mut b).is_ok() { b[0] } else { 0 };
        }
        Ok(())
    }

    /// `nvkm_dp_train_drive` (`dp.c:131-200`): what each lane asks for,
    /// capped (the "max reached" flags go to the sink), programmed from the
    /// VBIOS drive table and written to the sink. `pc2conf` accumulates
    /// across calls, as in nouveau.
    fn drive(&mut self, pc: bool) -> Result<(), AuxError> {
        for i in 0..self.nr() {
            let lane = (self.stat[4 + (i >> 1)] >> ((i & 1) * 4)) & 0xf;
            let mut lpc2 = (self.pc2stat >> (i * 2)) & 0x3;
            let mut lpre = (lane & 0x0c) >> 2;
            let mut lvsw = lane & 0x03;
            let mut hivs = 3 - lpre;
            let (hipe, hipc) = (3, 3);
            if lpc2 >= hipc {
                lpc2 = hipc | LC0F_LANE0_MAX_POST_CURSOR2_REACHED;
            }
            if lpre >= hipe {
                lpre = hipe | LC03_MAX_SWING_REACHED; // "yes."
                hivs = 3 - (lpre & 3);
                lvsw = hivs;
            } else if lvsw >= hivs {
                lvsw = hivs | LC03_MAX_SWING_REACHED;
            }
            self.conf[i] = (lpre << 3) | lvsw;
            self.pc2conf[i >> 1] |= lpc2 << ((i & 1) * 4);
            if let Some(c) = dpcfg_match(self.p.bios, self.p.info, lpc2 & 3, lvsw & 3, lpre & 3) {
                sor_drive(self.m, self.p.sor, i as u32, c);
            }
        }
        self.aux.nvkm_write(LC03, &self.conf)?;
        if pc {
            self.aux.nvkm_write(LC0F, &self.pc2conf)?;
        }
        Ok(())
    }

    /// `nvkm_dp_train_pattern` (`dp.c:202-226`): the SOR's and the sink's.
    /// A failed read of the sink's byte counts as 0 (nouveau's is
    /// uninitialised), a failed write is ignored, as there.
    fn pattern(&mut self, pattern: u8) {
        sor_pattern(self.m, self.p.sor, pattern);
        let mut b = [0u8];
        if self.aux.nvkm_read(LC02, &mut b).is_err() {
            b[0] = 0;
        }
        let mut tp = b[0] & !LC02_TRAINING_PATTERN_SET;
        tp |= if pattern != 4 { pattern } else { 7 };
        if pattern != 0 {
            tp |= LC02_SCRAMBLING_DISABLE;
        } else {
            tp &= !LC02_SCRAMBLING_DISABLE;
        }
        let _ = self.aux.nvkm_write(LC02, &[tp]);
    }

    fn lane(&self, i: usize) -> u8 {
        (self.stat[i >> 1] >> ((i & 1) * 4)) & 0xf
    }

    /// `nvkm_dp_train_cr` (`dp.c:275-310`).
    fn cr(&mut self, r: &mut Report) -> bool {
        let (mut cr_done, mut abort) = (false, false);
        let mut voltage = self.conf[0] & LC03_VOLTAGE_SWING_SET;
        let mut tries = 0;
        self.pattern(1);
        let dpcd = &self.p.dpcd;
        let usec = if dpcd[DPCD_REV] < 0x14 { (dpcd[DPCD_RC0E] & RC0E_AUX_RD_INTERVAL) as u32 * 4000 } else { 0 };
        loop {
            r.cr_tries += 1;
            if let Err(e) = self.drive(false).and_then(|_| self.sense(false, if usec != 0 { usec } else { 100 })) {
                r.aux_error = Some(e);
                break;
            }
            cr_done = true;
            for i in 0..self.nr() {
                if self.lane(i) & LS02_LANE0_CR_DONE == 0 {
                    cr_done = false;
                    if self.conf[i] & LC03_MAX_SWING_REACHED != 0 {
                        abort = true;
                    }
                    break;
                }
            }
            if self.conf[0] & LC03_VOLTAGE_SWING_SET != voltage {
                voltage = self.conf[0] & LC03_VOLTAGE_SWING_SET;
                tries = 0;
            }
            tries += 1;
            if cr_done || abort || tries >= 5 {
                break;
            }
        }
        cr_done
    }

    /// `nvkm_dp_train_eq` (`dp.c:228-273`).
    fn eq(&mut self, r: &mut Report) -> bool {
        let (mut eq_done, mut cr_done) = (false, true);
        let dpcd = self.p.dpcd;
        let pattern = if dpcd[DPCD_REV] >= 0x14 && dpcd[DPCD_RC03] & RC03_TPS4_SUPPORTED != 0 {
            4
        } else if dpcd[DPCD_REV] >= 0x12 && dpcd[DPCD_RC02] & RC02_TPS3_SUPPORTED != 0 {
            3
        } else {
            2
        };
        r.eq_pattern = pattern;
        self.pattern(pattern);
        let usec = (dpcd[DPCD_RC0E] & RC0E_AUX_RD_INTERVAL) as u32 * 4000;
        let mut tries = 0;
        loop {
            r.eq_tries += 1;
            let step = if tries > 0 { self.drive(self.pc2) } else { Ok(()) };
            if let Err(e) = step.and_then(|_| self.sense(self.pc2, if usec != 0 { usec } else { 400 })) {
                r.aux_error = Some(e);
                break;
            }
            eq_done = self.stat[2] & LS04_INTERLANE_ALIGN_DONE != 0;
            let mut i = 0;
            while i < self.nr() && eq_done {
                let lane = self.lane(i);
                if lane & LS02_LANE0_CR_DONE == 0 {
                    cr_done = false;
                }
                if lane & LS02_LANE0_CHANNEL_EQ_DONE == 0 || lane & LS02_LANE0_SYMBOL_LOCKED == 0 {
                    eq_done = false;
                }
                i += 1;
            }
            tries += 1;
            if eq_done || !cr_done || tries > 5 {
                break;
            }
        }
        eq_done
    }
}

/// `nvkm_dp_train_link` (`dp.c:312-366`) with no LTTPR and no rate table.
fn train_link(m: &dyn Mmio, p: &Params, r: &mut Report, aux: &Aux<'_, dyn Mmio + '_>, ef: bool) -> Result<(), TrainError> {
    let mut sink = [p.link.bw, p.link.nr];
    if ef {
        sink[1] |= LC01_ENHANCED_FRAME_EN;
    }
    aux.nvkm_write(LC00_LINK_BW_SET, &sink).map_err(|error| TrainError::Aux { step: "link config", error })?;
    let mut lt = Lt {
        m,
        p,
        aux,
        stat: [0; 6],
        conf: [0; 4],
        pc2: p.dpcd[DPCD_RC02] & RC02_TPS3_SUPPORTED != 0,
        pc2stat: 0,
        pc2conf: [0; 2],
    };
    let res = if !lt.cr(r) {
        Err(TrainError::ClockRecovery)
    } else if !lt.eq(r) {
        Err(TrainError::ChannelEq)
    } else {
        Ok(())
    };
    lt.pattern(0);
    r.stat = lt.stat;
    r.conf = lt.conf;
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmio::testing::ReplayMmio;
    use crate::vbios::tests::oracle_vbios;
    use alloc::string::String;
    use alloc::vec;

    const TRAIN: &str = include_str!("../fixtures/dp-train.txt");

    /// The ASUS's receiver capabilities as nouveau read them (0x2200 copy,
    /// `aux-ch3-dpcd-edid.txt`): DPCD 1.4, HBR2, 4 lanes, enhanced framing,
    /// TPS3, downspread, no TPS4, no post-LT adjust, no AUX read interval.
    const ASUS_DPCD: [u8; RECEIVER_CAP_SIZE] = [0x14, 0x14, 0xc4, 0x01, 0x01, 0x00, 0x01, 0xc0, 0x02, 0x00, 0x06, 0x00, 0x00, 0x00, 0x80];

    fn bios() -> Option<Bios> {
        match oracle_vbios() {
            Some(d) => Some(Bios::new(d).unwrap()),
            None => {
                std::eprintln!("SKIP: no oracle VBIOS");
                None
            }
        }
    }

    /// DP-3 (the ASUS): DCB output 4, type 6, hashm 0x0f82 (heads 0xf,
    /// sublink B, pad 2).
    fn dp3(bios: &Bios) -> DpInfo {
        dpout_match(bios, 0x0006, 0x0f82).expect("DP-3 has a DP table entry")
    }

    #[test]
    fn dp_table_entry_of_dp3() {
        let Some(bios) = bios() else { return };
        let info = dp3(&bios);
        assert_eq!((info.data, info.ver, info.hdr, info.flags), (0x6e96, 0x42, 0x13, 0x02));
        assert_eq!(info.script, [0x6ef1, 0x67c6, 0x733c, 0x732b, 0x734d]);
        assert_eq!(info.lnkcmp, 0x6ec1);
        // Rate lists: HBR2 and the GOP's HBR.
        assert_eq!(rate_script(&bios, info.script[0], 0x14), Some(0x6f99));
        assert_eq!(rate_script(&bios, info.lnkcmp, 0x14), Some(0x7292));
        assert_eq!(rate_script(&bios, info.lnkcmp, 0x0a), Some(0x72c5));
        assert_eq!(rate_script(&bios, info.lnkcmp, 0x1e), Some(0x7281));
        // The drive table entries the trace programmed (vs 0, 2, 3 at pe 0).
        assert_eq!(dpcfg_match(&bios, &info, 0, 0, 0), Some(DpCfg { pc: 0, dc: 0x15, pe: 0x01, tx_pu: 0x02 }));
        assert_eq!(dpcfg_match(&bios, &info, 0, 2, 0), Some(DpCfg { pc: 0, dc: 0x28, pe: 0x01, tx_pu: 0x04 }));
        assert_eq!(dpcfg_match(&bios, &info, 0, 3, 0), Some(DpCfg { pc: 0, dc: 0x35, pe: 0x01, tx_pu: 0x06 }));
        assert_eq!(dpcfg_match(&bios, &info, 3, 3, 3), None);
    }

    /// The whole of `trace-nogsp` from nouveau's release of DP-3 to the end
    /// of its training at 4x HBR2: DisableLT, sink power, EnableSpread,
    /// BeforeLinkTraining + lnkcmp for HBR2, links, lane power, TPS1 (two
    /// rounds: swing 0 then 2), TPS3 (two rounds: swing 3 reaches the
    /// maximum), pattern off, AfterLinkTraining. Every write must be the
    /// trace's, in order, and every read the trace made consumed.
    /// Equal write sequences, or a panic showing where they part.
    fn assert_same_writes(ours: &[(u32, u32)], trace: &[(u32, u32)]) {
        let Some(i) = (0..ours.len().max(trace.len())).find(|&i| ours.get(i) != trace.get(i)) else { return };
        let show = |w: &[(u32, u32)]| {
            w.iter().enumerate().skip(i.saturating_sub(4)).take(10).map(|(j, (o, v))| alloc::format!("  {j:4} W {o:#08x} {v:#010x}\n")).collect::<String>()
        };
        panic!("writes part at {i} (ours {}, trace {}):\nours:\n{}trace:\n{}", ours.len(), trace.len(), show(ours), show(trace));
    }

    #[test]
    fn training_replays_the_trace() {
        let Some(bios) = bios() else { return };
        let info = dp3(&bios);
        let m = ReplayMmio::from_extract(TRAIN);
        let p = Params {
            bios: &bios,
            info: &info,
            sor: Sor { id: 1, link: 2 },
            conn: Some(0x46),
            aux: 3,
            dpcd: ASUS_DPCD,
            link: LinkConfig { nr: 4, bw: 0x14 },
        };
        let mut r = Report::default();
        disable(&m, &p, &mut r).unwrap();
        train(&m, &p, &mut r);
        assert_same_writes(&m.writes.borrow(), &m.expected_writes);
        assert_eq!(r.result, Some(Ok(())));
        for reg in [0xda44, 0xda58, 0x61c834, 0x612488, 0x61c9c4, 0x61c998, 0x61c9b0, 0x6013d5] {
            assert_eq!(m.unread(reg), 0, "reads of {reg:#x} left");
        }
        assert_eq!(r.sink_power, Some(2));
        assert!(r.sink_woken);
        assert!(r.power_done);
        assert_eq!((r.cr_tries, r.eq_tries, r.eq_pattern), (2, 2, 3));
        assert_eq!(r.stat, [0x77, 0x77, 0x81, 0x00, 0x33, 0x33]);
        assert_eq!(r.conf, [0x07; 4]);
        let names: Vec<(Script, u16)> = r.scripts.iter().map(|(s, a, _)| (*s, *a)).collect();
        assert_eq!(
            names,
            vec![
                (Script::DisableLt, 0x734d),
                (Script::EnableSpread, 0x733c),
                (Script::BeforeLinkTraining, 0x6f99),
                (Script::LinkRate, 0x7292),
                (Script::AfterLinkTraining, 0x67c6),
            ]
        );
        // The ASSR condition's DPCD read (0x0d) and the spread's 0x107.
        assert!(r.scripts.iter().all(|(_, _, st)| st.aux_errors == 0));
        assert_eq!(r.scripts.iter().map(|(_, _, st)| st.aux).sum::<u32>(), 2 + 1 + 2);
    }

    #[test]
    fn configs_the_sink_or_board_cannot_run_are_refused() {
        let ok = LinkConfig { nr: 4, bw: 0x14 };
        assert_eq!(check_config(&ASUS_DPCD, 4, 0x1e, ok), Ok(()));
        assert_eq!(check_config(&ASUS_DPCD, 4, 0x1e, LinkConfig { nr: 2, bw: 0x0a }), Ok(()));
        assert_eq!(check_config(&ASUS_DPCD, 4, 0x1e, LinkConfig { nr: 3, bw: 0x14 }), Err(ConfigError::Lanes { nr: 3, sink: 4, board: 4 }));
        assert_eq!(check_config(&ASUS_DPCD, 2, 0x1e, ok), Err(ConfigError::Lanes { nr: 4, sink: 4, board: 2 }));
        // HBR3 is above the ASUS's HBR2; 0x0b is not a rate.
        assert_eq!(check_config(&ASUS_DPCD, 4, 0x1e, LinkConfig { nr: 4, bw: 0x1e }), Err(ConfigError::Rate { bw: 0x1e, sink: 0x14, board: 0x1e }));
        assert_eq!(check_config(&ASUS_DPCD, 4, 0x1e, LinkConfig { nr: 4, bw: 0x0b }), Err(ConfigError::Rate { bw: 0x0b, sink: 0x14, board: 0x1e }));
        let mut post = ASUS_DPCD;
        post[DPCD_RC02] |= RC02_POST_LT_ADJ_REQ_SUPPORTED;
        assert_eq!(check_config(&post, 4, 0x1e, ok), Err(ConfigError::PostLtAdjust));
        post[DPCD_RC03] |= RC03_TPS4_SUPPORTED;
        assert_eq!(check_config(&post, 4, 0x1e, ok), Ok(()));
    }

    /// A DP sink on AUX channel 3 (`0xda20..0xda5c`, `aux.rs`) behind a
    /// register file: native reads and
    /// writes of a DPCD array (always ACK), lane status and adjust
    /// requests fixed by the test; every other register reads back what
    /// was written (0x61c834's "busy" bit clears at once).
    struct SinkSim {
        regs: core::cell::RefCell<alloc::collections::BTreeMap<u32, u32>>,
        dpcd: core::cell::RefCell<[u8; 0x700]>,
        writes: core::cell::RefCell<Vec<(u32, u32)>>,
    }

    impl SinkSim {
        fn new(status: [u8; 3], adjust: [u8; 2]) -> SinkSim {
            let mut dpcd = [0u8; 0x700];
            dpcd[..RECEIVER_CAP_SIZE].copy_from_slice(&ASUS_DPCD);
            dpcd[0x202..0x205].copy_from_slice(&status);
            dpcd[0x206..0x208].copy_from_slice(&adjust);
            dpcd[0x600] = 1;
            // Condition 4 (PLL lock) and 8 (link ready) hold.
            let regs = [(0x61_2488, 0x80), (0x61_c9c4, 1), (0xda44, 0x9000), (0xda48, 0x1000_0000)].into_iter().collect();
            SinkSim { regs: core::cell::RefCell::new(regs), dpcd: core::cell::RefCell::new(dpcd), writes: Default::default() }
        }
        fn reg(&self, o: u32) -> u32 {
            *self.regs.borrow().get(&o).unwrap_or(&0)
        }
    }

    impl Mmio for SinkSim {
        fn rd32(&self, o: u32) -> u32 {
            self.reg(o)
        }
        fn wr32(&self, o: u32, v: u32) {
            self.writes.borrow_mut().push((o, v));
            let mut v = v;
            if o == 0x61_c834 {
                v &= !0x8000_0000;
            }
            if o == 0xda44 {
                // Channel request → granted; transaction → done at once.
                let g = if v & 0x0070_0000 != 0 { 0x0100_0000 } else { 0 };
                if v & 0x0001_0000 != 0 {
                    let (kind, addr) = ((v >> 12) & 0xf, self.reg(0xda40) as usize);
                    let size = if v & 0x100 != 0 { 0 } else { (v & 0xf) as usize + 1 };
                    let mut d = self.dpcd.borrow_mut();
                    if kind == 0x8 {
                        for i in 0..size {
                            d[addr + i] = (self.reg(0xda20 + (i as u32 / 4) * 4) >> ((i % 4) * 8)) as u8;
                        }
                    } else if kind == 0x9 {
                        let mut x = [0u8; 16];
                        x[..size].copy_from_slice(&d[addr..addr + size]);
                        for i in 0..4u32 {
                            let w = u32::from_le_bytes(x[i as usize * 4..i as usize * 4 + 4].try_into().unwrap());
                            self.regs.borrow_mut().insert(0xda30 + i * 4, w);
                        }
                    }
                    self.regs.borrow_mut().insert(0xda48, 0x1000_0000 | size as u32);
                }
                v = (v & !0x0701_0000) | g;
            }
            self.regs.borrow_mut().insert(o, v);
        }
        fn udelay(&self, _: u32) {}
    }

    fn params<'a>(bios: &'a Bios, info: &'a DpInfo) -> Params<'a> {
        Params { bios, info, sor: Sor { id: 1, link: 2 }, conn: Some(0x46), aux: 3, dpcd: ASUS_DPCD, link: LinkConfig { nr: 4, bw: 0x14 } }
    }

    /// A sink that never reports clock recovery and keeps asking for swing
    /// 1: the first round drives swing 0 (no status read yet), the second
    /// swing 1 (a change: the count restarts), then 4 more at it; nouveau
    /// gives up, and still turns the pattern off and runs AfterLinkTraining.
    #[test]
    fn clock_recovery_that_never_locks_gives_up_and_cleans_up() {
        let Some(bios) = bios() else { return };
        let info = dp3(&bios);
        let m = SinkSim::new([0, 0, 0], [0x11, 0x11]);
        let mut r = Report::default();
        train(&m, &params(&bios, &info), &mut r);
        assert_eq!(r.result, Some(Err(TrainError::ClockRecovery)));
        assert_eq!(r.aux_error, None);
        assert_eq!(r.cr_tries, 6);
        assert_eq!(r.eq_tries, 0);
        assert_eq!(r.conf, [0x01; 4]);
        assert_eq!(r.scripts.last().map(|s| s.0), Some(Script::AfterLinkTraining));
        // SOR and sink patterns off at the end (0x10 per lane, TPS 0).
        assert_eq!(m.reg(0x61c92c) & 0x1f1f_1f1f, 0x1010_1010);
        assert_eq!(m.dpcd.borrow()[0x102] & 0x2f, 0);
        // The sink got the link configuration asked for.
        assert_eq!(&m.dpcd.borrow()[0x100..0x102], &[0x14, 0x84]);
    }

    /// Asked for the maximum swing without clock recovery: the second round
    /// drives it with "max swing reached" flagged to the sink, and nouveau
    /// stops there.
    #[test]
    fn clock_recovery_stops_at_the_maximum_swing() {
        let Some(bios) = bios() else { return };
        let info = dp3(&bios);
        let m = SinkSim::new([0, 0, 0], [0x33, 0x33]);
        let mut r = Report::default();
        train(&m, &params(&bios, &info), &mut r);
        assert_eq!(r.result, Some(Err(TrainError::ClockRecovery)));
        assert_eq!(r.cr_tries, 2);
        assert_eq!(r.conf, [0x07; 4]);
        assert_eq!(&m.dpcd.borrow()[0x103..0x107], &[0x07; 4]);
    }

    /// Post-cursor2 requests (`0x20c`, read with TPS3) are capped and
    /// or-ed into `pc2conf` across rounds, never cleared, as nouveau keeps
    /// it in `lt_state` (`dp.c:164`).
    #[test]
    fn post_cursor2_settings_accumulate_across_rounds() {
        let Some(bios) = bios() else { return };
        let info = dp3(&bios);
        let m = SinkSim::new([0, 0, 0], [0, 0]);
        let p = params(&bios, &info);
        let aux = Aux::new(&m as &dyn Mmio, 3);
        let mut lt = Lt { m: &m, p: &p, aux: &aux, stat: [0; 6], conf: [0; 4], pc2: true, pc2stat: 0x01, pc2conf: [0; 2] };
        lt.drive(true).unwrap();
        assert_eq!(&m.dpcd.borrow()[0x10f..0x111], &[0x01, 0x00]);
        // Lane 0 asks for 2, lane 1 for 3 (the maximum: flagged).
        lt.pc2stat = 0x0e;
        lt.drive(true).unwrap();
        assert_eq!(&m.dpcd.borrow()[0x10f..0x111], &[0x01 | 0x02 | (0x07 << 4), 0x00]);
    }

    /// A sink locked from the start: one CR round, one EQ round, at swing
    /// 0; the SOR's lanes get entry 0 of the drive table.
    #[test]
    fn a_sink_that_locks_at_once_trains_in_one_round_each() {
        let Some(bios) = bios() else { return };
        let info = dp3(&bios);
        let m = SinkSim::new([0x77, 0x77, 0x81], [0, 0]);
        let mut r = Report::default();
        train(&m, &params(&bios, &info), &mut r);
        assert_eq!(r.result, Some(Ok(())));
        assert_eq!((r.cr_tries, r.eq_tries), (1, 1));
        assert_eq!(m.reg(0x61c998), 0x1515_1515);
        assert_eq!(m.reg(0x61c98c) & 0x401f_4003, 0x000f_4001);
        assert_eq!(m.reg(0x612b00) & 0x007c_0000, 0x0008_0000);
    }
}
