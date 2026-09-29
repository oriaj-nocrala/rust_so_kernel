//! The HP on HDMI (phase 5.8 of the plan): a second head, SOR-0 running
//! TMDS, and the encoder's HDMI mode, all as nouveau does it on this GPU
//! (`trace-nogsp`, `fixtures/hdmi-enable.txt`, `modeset-push.txt`).
//!
//! What the GOP leaves unlit and this module brings up, beside the parts
//! earlier phases have (supervisors, VPLL: `supervisor`, `pll`):
//! - the SOR's power (`nv50_sor_power`, `engine/disp/nv50.c:203-228`), read
//!   first: the GOP may have left it on;
//! - the pad → SOR route (`gm200_sor_route_set`, `gm200.c:98-114`): the
//!   HDMI output has none at boot (`super: outp 07: no route`);
//! - the encoder: HDMI control, general control and audio clock
//!   regeneration packets (`gv100_sor_hdmi_ctrl`, `gv100.c:146-172`), the
//!   SCDC scrambling state (`gm200_sor_hdmi_scdc`, `gm200.c:71-88`: the HP
//!   is HDMI 1.x, so off) and the AVI infoframe (`gv100.c:124-142`);
//! - the core-channel methods that put head `n` on the SOR and the window
//!   state that scans a surface out on it ([`head_methods`],
//!   [`window_methods`]): the values of nouveau's round-2 push, replayed.
//!
//! Audio (the ELD and the HDA device entry, `0x616528`) is not ported.

use alloc::vec::Vec;

use crate::evo::{self, Chan};
use crate::mode::{self, Mode};
use crate::Mmio;

// ---------------------------------------------------------------------------
// The SOR and its route
// ---------------------------------------------------------------------------

/// SOR power (`nv50_sor_power`, `nv50.c:211-228`): `0x61c004 + sor * 0x800`,
/// bit 31 = a change is pending, bit 0 = normal power up; `0x61c030` bit 28
/// = the power sequencer is busy.
pub const SOR_POWER: u32 = 0x61_c004;
pub const SOR_POWER_STATUS: u32 = 0x61_c030;
const SOR_STRIDE: u32 = 0x800;

/// Polls every 10 µs for up to 2 s, nouveau's bound (`nv50.c:205,225`).
fn poll(m: &dyn Mmio, mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..200_000 {
        if done() {
            return true;
        }
        m.udelay(10);
    }
    done()
}

/// Whether the SOR's normal power bit is on.
pub fn sor_powered(m: &dyn Mmio, sor: u8) -> bool {
    m.rd32(SOR_POWER + sor as u32 * SOR_STRIDE) & 1 != 0
}

/// `nv50_sor_power(sor, normal = true, pu = true, ..)`: wait for the last
/// change, set bit 0, wait again, wait for the sequencer. `false` if a wait
/// ran out (nouveau ignores it; this driver reports it).
pub fn sor_power_up(m: &dyn Mmio, sor: u8) -> bool {
    let soff = sor as u32 * SOR_STRIDE;
    let idle = |m: &dyn Mmio| m.rd32(SOR_POWER + soff) & 0x8000_0000 == 0;
    let mut ok = poll(m, || idle(m));
    m.mask(SOR_POWER + soff, 0x8000_0001, 0x8000_0001);
    ok &= poll(m, || idle(m));
    ok &= poll(m, || m.rd32(SOR_POWER_STATUS + soff) & 0x1000_0000 == 0);
    ok
}

/// The pad-to-SOR route registers: `0x612308 + ffs(or) * 0x100` for
/// sublink A, `0x612388 + ..` for B (`gm200.c:98-114`).
const ROUTE_A: u32 = 0x61_2308;
const ROUTE_B: u32 = 0x61_2388;

/// `gm200_sor_route_set` for output `or` (its DCB OR mask) with sublinks
/// `sublinks` (the DCB's `sorconf.link`: bit 0 = A, bit 1 = B), routed to
/// `sor` (`None` = unrouted) whose assigned link is `sor_link`
/// (`ior->asy.link`, 2 = B). `gm200_sor_route_get` reads it back.
pub fn route_set(m: &dyn Mmio, or: u8, sublinks: u8, sor: Option<u8>, sor_link: u8) {
    let moff = or.trailing_zeros() * 0x100;
    let sor = sor.map_or(0, |s| s as u32 + 1);
    let mut link = if sor != 0 { (sor_link == 2) as u32 } else { 0 };
    if sublinks & 1 != 0 {
        m.mask(ROUTE_A + moff, 0x1f, link << 4 | sor);
        link += 1;
    }
    if sublinks & 2 != 0 {
        m.mask(ROUTE_B + moff, 0x1f, link << 4 | sor);
    }
}

// ---------------------------------------------------------------------------
// The encoder
// ---------------------------------------------------------------------------

/// Packet-window and rekey values nouveau's `nv50_hdmi_enable` computes
/// (`dispnv50/disp.c:786-795`): rekey is a constant, the window is what
/// the horizontal blanking leaves after it and 18 (tegra's constant), in
/// units of 32.
pub const REKEY: u32 = 56;

pub fn max_ac_packet(mode: &Mode) -> u32 {
    (mode.htotal as u32 - mode.hdisplay as u32).saturating_sub(REKEY + 18) / 32
}

/// `gv100_sor_hdmi_ctrl` with `enable` (`gv100.c:146-172`): the general
/// control packet (`0x6f00c0/cc`), audio clock regeneration (`0x6f0080`)
/// and the head's HDMI control word (`0x6165c0`).
pub fn ctrl_enable(m: &dyn Mmio, head: u32, max_ac_packet: u32, rekey: u32) {
    let (hoff, hdmi) = (head * 0x800, head * 0x400);
    let ctrl = 0x4000_0000 | max_ac_packet << 16 | rekey;
    m.mask(0x6f_00c0 + hdmi, 1, 0);
    m.wr32(0x6f_00cc + hdmi, 0x10);
    m.mask(0x6f_00c0 + hdmi, 1, 1);
    m.wr32(0x6f_0080 + hdmi, 0x8200_0000);
    m.mask(0x61_65c0 + hoff, 0x401f_007f, ctrl);
}

/// `gm200_sor_hdmi_scdc` for a sink without scrambling: the SOR's SCDC
/// bits cleared (`gm200.c:71-88`). Returns `tmds.high_speed`
/// (`khz > 340000`), which selects the SOR clock divider.
pub fn scdc_off(m: &dyn Mmio, sor: u8, khz: u32) -> bool {
    m.mask(0x61_c5bc + sor as u32 * SOR_STRIDE, 0x3, 0);
    khz > 340_000
}

/// An infoframe as the registers hold it (`pack_hdmi_infoframe`,
/// `engine/disp/hdmi.c:5-84`): bytes 0-2 the header, then two sub-packs
/// of 7 bytes each.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Packed {
    pub header: u32,
    pub sub0_low: u32,
    pub sub0_high: u32,
    pub sub1_low: u32,
    pub sub1_high: u32,
}

pub fn pack(raw: &[u8; 17]) -> Packed {
    let w = |a: usize, n: usize| (0..n).fold(0u32, |v, i| v | (raw[a + i] as u32) << (8 * i));
    Packed { header: w(0, 3), sub0_low: w(3, 4), sub0_high: w(7, 3), sub1_low: w(10, 4), sub1_high: w(14, 3) }
}

/// HDMI's video identification codes (CTA-861) for the modes `mode`
/// matches by size, total and clock, with the picture aspect ratio's code
/// (1 = 4:3, 2 = 16:9): `(vic, aspect)`, `(0, 0)` for any other mode.
pub fn vic(mode: &Mode) -> (u8, u8) {
    const TABLE: [(u16, u16, u16, u16, u32, u8, u8); 5] = [
        // hdisplay, vdisplay, htotal, vtotal, kHz, VIC, aspect
        (640, 480, 800, 525, 25_200, 1, 1),
        (1280, 720, 1650, 750, 74_250, 4, 2),
        (1280, 720, 1980, 750, 74_250, 19, 2),
        (1920, 1080, 2200, 1125, 148_500, 16, 2),
        (1920, 1080, 2640, 1125, 148_500, 31, 2),
    ];
    TABLE
        .iter()
        .find(|t| {
            (t.0, t.1, t.2, t.3) == (mode.hdisplay, mode.vdisplay, mode.htotal, mode.vtotal)
                // within 0.5 % (59.94 Hz variants use the 1000/1001 clock)
                && (mode.clock_khz as u64 * 1000).abs_diff(t.4 as u64 * 1000) * 200 <= t.4 as u64 * 1000
        })
        .map_or((0, 0), |t| (t.5, t.6))
}

/// The AVI infoframe DRM builds for a CEA mode on an RGB sink that has
/// underscan (`drm_hdmi_avi_infoframe_from_display_mode`, then
/// `_quant_range` with a sink that cannot select it): RGB, active format
/// information valid and "same as picture", underscan, the picture aspect,
/// the VIC. Type 0x82, version 2, length 13, checksum such that all 17
/// bytes sum to 0. Equal to the bytes of the trace for 1080p60.
pub fn avi_infoframe(mode: &Mode) -> [u8; 17] {
    let (vic, aspect) = vic(mode);
    let mut f = [0u8; 17];
    f[0] = 0x82;
    f[1] = 2;
    f[2] = 13;
    f[4] = 0x10 | 0x02; // PB1: A0 = 1, S = 2 (underscan)
    f[5] = aspect << 4 | 0x08; // PB2: M, R = 8 (same as the picture)
    f[7] = vic; // PB4
    f[3] = 0u8.wrapping_sub(f.iter().fold(0u8, |s, b| s.wrapping_add(*b)));
    f
}

/// `gv100_sor_hdmi_infoframe_avi` (`gv100.c:124-142`): off, the five
/// words, on. `None` = off only.
pub fn write_avi(m: &dyn Mmio, head: u32, frame: Option<&[u8; 17]>) {
    let hdmi = head * 0x400;
    m.mask(0x6f_0000 + hdmi, 1, 0);
    let Some(f) = frame else { return };
    let p = pack(f);
    m.wr32(0x6f_0008 + hdmi, p.header);
    m.wr32(0x6f_000c + hdmi, p.sub0_low);
    m.wr32(0x6f_0010 + hdmi, p.sub0_high);
    m.wr32(0x6f_0014 + hdmi, p.sub1_low);
    m.wr32(0x6f_0018 + hdmi, p.sub1_high);
    m.mask(0x6f_0000 + hdmi, 1, 1);
}

/// `gv100_sor_hdmi_infoframe_vsi` with no frame (`gv100.c:100-121`): the
/// vendor infoframe off. A 1080p60 mode has none to send.
pub fn vsi_off(m: &dyn Mmio, head: u32) {
    m.mask(0x6f_0100 + head * 0x400, 0x0001_0001, 0);
}

/// The whole encoder enable in nouveau's order (`nv50_hdmi_enable` through
/// `nvkm_uoutp_mthd_hdmi`, `uoutp.c:243-274`, then the two infoframes):
/// ctrl, SCDC, AVI, VSI. Returns `tmds.high_speed`.
pub fn enable(m: &dyn Mmio, head: u32, sor: u8, mode: &Mode) -> bool {
    ctrl_enable(m, head, max_ac_packet(mode), REKEY);
    let high_speed = scdc_off(m, sor, mode.clock_khz);
    write_avi(m, head, Some(&avi_infoframe(mode)));
    vsi_off(m, head);
    high_speed
}

/// The encoder disable (`nvkm_uoutp_mthd_hdmi`, `uoutp.c:260-265`): AVI
/// off, VSI off, then the control word (`gv100.c:154-161`).
pub fn disable(m: &dyn Mmio, head: u32) {
    let (hoff, hdmi) = (head * 0x800, head * 0x400);
    write_avi(m, head, None);
    vsi_off(m, head);
    m.mask(0x61_65c0 + hoff, 0x4000_0000, 0);
    m.mask(0x6f_0100 + hdmi, 1, 0);
    m.mask(0x6f_00c0 + hdmi, 1, 0);
    m.mask(0x6f_0000 + hdmi, 1, 0);
}

// ---------------------------------------------------------------------------
// Core and window methods
// ---------------------------------------------------------------------------

/// `SOR_SET_CONTROL(sor)`: protocol in bits 8-11, the heads it drives in
/// bits 0-7 (`clc67d.h:299-`, `gv100_sor_state`, `gv100.c:184-204`).
pub const fn sor_control(proto_evo: u32, head: u32) -> u32 {
    proto_evo << 8 | 1 << head
}
/// `SINGLE_TMDS_A` (`gv100_sor_state` case 1).
pub const PROTO_TMDS_A: u32 = 1;

pub const fn sor_set_control(sor: u32) -> u32 {
    0x300 + sor * 0x20
}

/// `HEAD_SET_CONTROL_OUTPUT_RESOURCE` of a head nothing configured before:
/// what nouveau's round-2 push holds for both heads at 1080p60 with
/// positive syncs (`modeset-push.txt`: `0xfc000040`).
pub const HEAD_OR_DEFAULT: u32 = 0xfc00_0040;
/// `HEAD_SET_HEAD_USAGE_BOUNDS` (`clc67d.h:745`) as nouveau sets it.
pub const HEAD_USAGE_BOUNDS: u32 = 0x1114;
const HEAD_STRIDE: u32 = 0x400;
/// `WINDOW_SET_WINDOW_FORMAT_USAGE_BOUNDS(w)`, `_ROTATED_`, `_USAGE_BOUNDS`
/// (`clc67d.h:364,416,471`) and the values of nouveau's init push.
const WINDOW_STRIDE: u32 = 0x80;
const WNDW_FORMAT_USAGE: u32 = 0xf;
const WNDW_USAGE: u32 = 0x0011_7fff;

/// The core-channel methods that light head `head` at `mode` on `sor`
/// (TMDS sublink A) for the output DCB entry `dcb_index`, with `window`
/// (its own, `window >> 1 == head`) scanning it out: nouveau's round-2
/// push for head 1, method for method (`modeset-push.txt`), less the head's
/// output LUT. Without UPDATE: the caller adds it.
pub fn head_methods(head: u32, sor: u32, dcb_index: u8, window: u32, mode: &Mode) -> Vec<(u32, u32)> {
    let hd = head * HEAD_STRIDE;
    let (w, h) = mode.size();
    let size = (h as u32) << 16 | w as u32;
    let mut v = alloc::vec![
        (0x2020 + hd, 1u32 << dcb_index),
        (sor_set_control(sor), sor_control(PROTO_TMDS_A, head)),
        (0x204c + hd, size),
        (0x2058 + hd, size),
    ];
    v.extend(mode.methods(head, HEAD_OR_DEFAULT));
    v.extend([
        (0x2030 + hd, HEAD_USAGE_BOUNDS),
        (0x2018 + hd, 0x10), // HEAD_SET_DITHER_CONTROL (`clc67d.h:705`)
        (0x2000 + hd, 0),    // HEAD_SET_PROCAMP (`clc67d.h:489`)
    ]);
    let wo = window * WINDOW_STRIDE;
    v.extend([
        (0x1004 + wo, WNDW_FORMAT_USAGE),
        (0x1008 + wo, 0),
        (0x1010 + wo, WNDW_USAGE),
        (0x1000 + wo, head), // WINDOW_SET_CONTROL: the owner (`clc67d.h:351`)
    ]);
    v
}

/// The core methods that take the head off its SOR (`nvkm` round 1 of the
/// trace: `SOR_SET_CONTROL(sor) = 0`, the display id cleared).
pub fn head_off_methods(head: u32, sor: u32) -> Vec<(u32, u32)> {
    alloc::vec![(0x2020 + head * HEAD_STRIDE, 0), (sor_set_control(sor), 0)]
}

/// The window's surface state, as nouveau's first flip pushes it
/// (`modeset-push.txt`, `wndw2`): a `w`x`h` pitch-linear XRGB8888 surface
/// at VRAM `offset` named by context DMA `handle`, no scaling, no LUT, no
/// notifier. `interlock` = the windows (bit mask) whose UPDATEs latch
/// together with the core's: nouveau's first UPDATE of a newly owned window
/// interlocks with the core (`SET_INTERLOCK_FLAGS = 1`, `SET_WINDOW_
/// INTERLOCK_FLAGS = 5` for windows 0 and 2) and the core then UPDATEs with
/// the same window mask ([`core_interlock_methods`]); Ryzen boot #89: an
/// independent UPDATE of the new window raised INVALID_STATE (exception
/// slot `0x5080 0x1 0x2d`). 0 = no interlock (a flip of a running window).
/// Without UPDATE.
pub fn window_methods(w: u16, h: u16, pitch: u32, handle: u32, offset: u64, interlock: u32) -> Vec<(u32, u32)> {
    let size = (h as u32) << 16 | w as u32;
    alloc::vec![
        (evo::WNDW_SET_PRESENT_CONTROL, evo::PRESENT_CONTROL_VSYNC),
        (0x224, size),      // SET_SIZE
        (0x228, 0),         // SET_STORAGE: pitch, block height 0
        (0x22c, 0xcf),      // SET_PARAMS: the format nouveau uses
        (0x230, pitch >> 6), // SET_PLANAR_STORAGE(0): the pitch in 64-byte units
        (evo::WNDW_SET_CONTEXT_DMA_ISO0, handle),
        (evo::WNDW_SET_OFFSET0, (offset >> 8) as u32),
        (0x290, 0), // SET_POINT_IN(0)
        (0x298, size), // SET_SIZE_IN
        (0x2a4, size), // SET_SIZE_OUT
        (0x2ec, 0xff0), // SET_COMPOSITION_CONTROL
        (0x2f0, 0xff),  // SET_COMPOSITION_CONSTANT_ALPHA
        (0x2f4, 0x7722), // SET_COMPOSITION_FACTOR_SELECT
        (0x2f8, 0xffff_0000), // SET_KEY_ALPHA
        (0x2fc, 0xffff_0000), // SET_KEY_RED_CR
        (0x300, 0xffff_0000), // SET_KEY_GREEN_Y
        (0x304, 0xffff_0000), // SET_KEY_BLUE_CB
        (evo::WNDW_SET_INTERLOCK_FLAGS, (interlock != 0) as u32),
        (evo::WNDW_SET_WINDOW_INTERLOCK_FLAGS, interlock),
    ]
}

/// The core methods (before its UPDATE) that latch together with windows
/// `windows` (`modeset-push.txt`: `SET_INTERLOCK_FLAGS = 0`,
/// `SET_WINDOW_INTERLOCK_FLAGS = 5`).
pub fn core_interlock_methods(windows: u32) -> Vec<(u32, u32)> {
    alloc::vec![(evo::CORE_SET_INTERLOCK_FLAGS, 0), (evo::CORE_SET_WINDOW_INTERLOCK_FLAGS, windows)]
}

/// VRAM the HP's head uses, a 16 MiB slot of BAR1 after `scanout.rs`'s two
/// buffers: the picture at its start, then (well above it) the LUTs and the
/// notifier area.
pub const SLOT_VRAM: u64 = 48 << 20;
pub const SLOT_LEN: u64 = 16 << 20;
pub const ILUT_VRAM: u64 = SLOT_VRAM + 0x00e0_0000;
pub const OLUT_VRAM: u64 = SLOT_VRAM + 0x00e0_4000;
pub const NTFY_VRAM: u64 = SLOT_VRAM + 0x00f0_0000;
pub const NTFY_LEN: u64 = 0x1000;
/// nouveau's handles for the notifier area and the LUT memory
/// (`dispnv50/handles.h`, `modeset-2-inst.txt`).
pub const HANDLE_SYNC: u32 = 0xf000_0000;
pub const HANDLE_LUT: u32 = 0xf000_0001;

/// The RAMHT entries this driver adds for the HP: the core's LUT handle
/// (the head's output LUT), and window `head_window(HP_HEAD)`'s surface,
/// LUT and notifier handles. `(chid, handle, object)`; the LUT object is
/// all of VRAM with nouveau's flags (`0x45`), the notifier a 4 KiB area.
pub fn ramht_objects(vram: u64) -> Vec<(u32, u32, evo::CtxDma)> {
    use evo::{CtxDma, CTXDMA_PAGE, CTXDMA_RW, CTXDMA_VRAM};
    let w = window_chan(HP_HEAD).user;
    let all = CtxDma { flags0: CTXDMA_PAGE | CTXDMA_RW | CTXDMA_VRAM, start: 0, limit: vram - 1 };
    let sync = CtxDma { flags0: CTXDMA_PAGE | CTXDMA_RW | CTXDMA_VRAM, start: NTFY_VRAM, limit: NTFY_VRAM + NTFY_LEN - 1 };
    alloc::vec![
        (w, evo::HANDLE_WNDW_CTX, evo::vram_ctxdma(vram)),
        (w, HANDLE_LUT, all),
        (w, HANDLE_SYNC, sync),
        (evo::CORE.user, HANDLE_LUT, all),
    ]
}

/// The window's notifier and input LUT methods (nouveau's wndw2 push:
/// `SET_CONTEXT_DMA_NOTIFIER`, `SET_NOTIFIER_CONTROL`, `SET_ILUT_CONTROL`,
/// `SET_CONTEXT_DMA_ILUT`, `SET_OFFSET_ILUT`).
pub fn window_lut_methods() -> [(u32, u32); 5] {
    [
        (0x21c, HANDLE_SYNC),
        (0x220, 0xe0),
        (0x440, crate::lut::ILUT_CONTROL),
        (0x444, HANDLE_LUT),
        (0x448, (ILUT_VRAM >> 8) as u32),
    ]
}

/// The head's output LUT methods, in the core's third push
/// (`HEAD_SET_OLUT_CONTROL` .. `HEAD_SET_OFFSET_OLUT`, `headc57d.c:120-128`).
pub fn olut_methods(head: u32) -> [(u32, u32); 4] {
    let hd = head * HEAD_STRIDE;
    [
        (0x2280 + hd, crate::lut::OLUT_CONTROL),
        (0x2284 + hd, 0xffff_ffff),
        (0x2288 + hd, HANDLE_LUT),
        (0x228c + hd, (OLUT_VRAM >> 8) as u32),
    ]
}

/// The head nouveau gave the HP (`modeset-push.txt`: `HEAD_SET_DISPLAY_ID(1,
/// 0) = 0x80`): the GOP lights head 0 only.
pub const HP_HEAD: u32 = 1;

/// The window that scans out head `head`: nouveau gives each head two
/// (`WINDOW_SET_CONTROL(i)` = `i >> 1` in the trace); this driver uses the
/// first.
pub const fn head_window(head: u32) -> u32 {
    head * 2
}

/// The window's channel.
pub fn window_chan(head: u32) -> Chan {
    evo::window(head_window(head))
}

/// The head's mode as `mode::select` would have it for an EDID's first
/// detailed timing (the HP's 1080p60 at 148.5 MHz).
pub fn preferred(edid: &crate::edid::Edid) -> Option<Mode> {
    mode::edid_modes(edid).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edid::Edid;
    use crate::mmio::testing::ReplayMmio;
    use alloc::string::String;

    const ENABLE: &str = include_str!("../fixtures/hdmi-enable.txt");
    const PUSH: &str = include_str!("../fixtures/modeset-push.txt");
    const HP: &[u8] = include_bytes!("../fixtures/edid-hp-2309.bin");

    fn hp_mode() -> Mode {
        preferred(&Edid::parse(HP).unwrap()).unwrap()
    }

    fn fmt(w: &[(u32, u32)]) -> String {
        w.iter().map(|(o, v)| alloc::format!("W {o:#08x} {v:#010x}\n")).collect()
    }

    #[test]
    fn the_hps_preferred_mode_is_cea_1080p60() {
        let m = hp_mode();
        assert_eq!((m.size(), m.htotal, m.vtotal, m.clock_khz), ((1920, 1080), 2200, 1125, 148_500));
        assert_eq!(vic(&m), (16, 2));
        assert_eq!(max_ac_packet(&m), 6);
    }

    #[test]
    fn avi_infoframe_is_the_traces() {
        let f = avi_infoframe(&hp_mode());
        assert_eq!(f.iter().fold(0u8, |s, b| s.wrapping_add(*b)), 0);
        assert_eq!(
            pack(&f),
            Packed { header: 0x000d_0282, sub0_low: 0x0028_1225, sub0_high: 0x10, sub1_low: 0, sub1_high: 0 }
        );
        // A mode with no VIC: no code, no aspect, the checksum still holds.
        let m = Mode { clock_khz: 100_000, ..hp_mode() };
        let f = avi_infoframe(&m);
        assert_eq!((f[5], f[7]), (0x08, 0));
        assert_eq!(f.iter().fold(0u8, |s, b| s.wrapping_add(*b)), 0);
    }

    /// The encoder enable replays the trace access for access: route,
    /// GCP/ACR/ctrl, SCDC, AVI, VSI.
    #[test]
    fn enable_replays_the_trace() {
        let m = ReplayMmio::from_extract(ENABLE);
        route_set(&m, 2, 1, Some(0), 1);
        let high = enable(&m, 1, 0, &hp_mode());
        assert!(!high);
        assert_eq!(fmt(&m.writes.borrow()), fmt(&m.expected_writes));
        for r in [0x61_2408, 0x6f_04c0, 0x61_6dc0, 0x61_c5bc, 0x6f_0400, 0x6f_0500] {
            assert_eq!(m.unread(r), 0, "{r:#x}");
        }
    }

    #[test]
    fn disable_clears_what_enable_set() {
        let m = ReplayMmio::from_extract("");
        let mut fb = alloc::vec::Vec::new();
        for r in [0x6f_0400, 0x6f_0500, 0x61_6dc0, 0x6f_04c0] {
            fb.push((r, 0xffff_ffff));
        }
        let mut m = m;
        m.fallback = fb;
        disable(&m, 1);
        let w = m.writes.borrow();
        // AVI off, VSI off, ctrl bit 30 off, then the three enables.
        assert_eq!(w[0], (0x6f_0400, 0xffff_fffe));
        assert_eq!(w[1], (0x6f_0500, 0xfffe_fffe));
        assert_eq!(w[2], (0x61_6dc0, 0xbfff_ffff));
        assert!(w[3..].iter().all(|(_, v)| v & 1 == 0));
    }

    #[test]
    fn route_set_writes_the_sublinks_route() {
        let zero = || {
            let mut m = ReplayMmio::from_extract("");
            m.fallback = alloc::vec![(0x61_2408, 0), (0x61_2488, 0)];
            m
        };
        // HDMI: pad or 2 (ffs 2 -> 0x100), sublink A only, SOR-0.
        let m = zero();
        route_set(&m, 2, 1, Some(0), 1);
        assert_eq!(m.writes.borrow().as_slice(), &[(0x61_2408, 0x1)]);
        // DP-3: sublink B, SOR-1 running link 2: link bit 4 set.
        let m = zero();
        route_set(&m, 2, 2, Some(1), 2);
        assert_eq!(m.writes.borrow().as_slice(), &[(0x61_2488, 0x12)]);
        // Sublink A of a SOR running link B (dual-link pad bookkeeping):
        // A gets link bit 4, and B the incremented link number.
        let m = zero();
        route_set(&m, 2, 3, Some(1), 2);
        assert_eq!(m.writes.borrow().as_slice(), &[(0x61_2408, 0x12), (0x61_2488, 0x22)]);
        // Unrouted.
        let m = zero();
        route_set(&m, 2, 1, None, 1);
        assert_eq!(m.writes.borrow().as_slice(), &[(0x61_2408, 0)]);
    }

    /// `(time, channel, method, value)` of every method line of the push
    /// fixture.
    fn push_rows() -> Vec<(String, String, u32, u32)> {
        PUSH.lines()
            .filter(|l| !l.starts_with('#') && !l.starts_with("=="))
            .map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                (f[0].into(), f[1].into(), u32::from_str_radix(f[2], 16).unwrap(), u32::from_str_radix(f[3], 16).unwrap())
            })
            .collect()
    }

    fn sorted(mut v: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
        v.sort();
        v
    }

    /// Head 1 as nouveau's round-2 core push has it (everything about head
    /// 1, SOR-0's control, window 2's owner) and the init push's usage
    /// bounds for window 2.
    #[test]
    fn head_methods_are_nouveaus_for_head_1() {
        let rows = push_rows();
        let mut want: Vec<(u32, u32)> = rows
            .iter()
            .filter(|(t, c, m, _)| {
                c == "core"
                    && ((t == "11.918881" && ((0x2400..0x2800).contains(m) || *m == 0x300 || *m == 0x1100))
                        || (t == "11.395777" && [0x1104, 0x1108, 0x1110].contains(m)))
            })
            .map(|r| (r.2, r.3))
            .collect();
        // The trace's head 1 raster is the same 1080p60; its methods are
        // in `want` twice only if the fixture repeats them.
        want.dedup();
        let got = head_methods(1, 0, 7, head_window(1), &hp_mode());
        assert_eq!(sorted(got), sorted(want));
    }

    /// Window 2's surface push, less the notifier and the ILUT, with the
    /// trace's interlock (windows 0 and 2 = mask 5) and `SET_OFFSET`.
    #[test]
    fn window_methods_are_nouveaus_for_window_2() {
        let rows = push_rows();
        let want: Vec<(u32, u32)> = rows
            .iter()
            .filter(|(_, c, m, _)| {
                c == "wndw2" && ![0x21c, 0x220, 0x440, 0x444, 0x448, 0x200].contains(m)
            })
            .map(|r| (r.2, r.3))
            .collect();
        let got = window_methods(1920, 1080, 7680, evo::HANDLE_WNDW_CTX, 0x20_0000, 5);
        assert_eq!(sorted(got), sorted(want));
        // Without an interlock a flip's window pushes 0/0.
        let free = window_methods(1920, 1080, 7680, evo::HANDLE_WNDW_CTX, 0x20_0000, 0);
        assert!(free.contains(&(0x370, 0)) && free.contains(&(0x374, 0)));
        // The core's side, as the trace's third push.
        let core: Vec<(u32, u32)> = rows.iter().filter(|(t, c, m, _)| c == "core" && t == "11.964732" && [0x218, 0x21c].contains(m)).map(|r| (r.2, r.3)).collect();
        assert_eq!(core_interlock_methods(5), core);
    }

    /// The notifier, ILUT and OLUT methods are the trace's, except the
    /// LUT offsets (nouveau's allocations, not this driver's).
    #[test]
    fn lut_methods_are_nouveaus() {
        let rows = push_rows();
        let find = |c: &str, m: u32| rows.iter().find(|r| r.1 == c && r.2 == m && (c != "core" || r.0 == "11.964732")).map(|r| r.3).unwrap();
        for (m, v) in window_lut_methods() {
            if m == 0x448 {
                assert_eq!(v, (ILUT_VRAM >> 8) as u32);
                continue;
            }
            assert_eq!(v, find("wndw2", m), "{m:#x}");
        }
        for (m, v) in olut_methods(1) {
            if m == 0x228c + 0x400 {
                continue;
            }
            assert_eq!(v, find("core", m), "{m:#x}");
        }
    }

    /// The RAMHT with the HP's entries has no collisions and names the
    /// objects the way nouveau's does (same slots for the same
    /// `(chid, handle)`).
    #[test]
    fn ramht_with_the_hps_entries() {
        let mut objects = alloc::vec![(evo::window(0).user, evo::HANDLE_WNDW_CTX, evo::vram_ctxdma(8 << 30))];
        objects.extend(ramht_objects(8 << 30));
        let t = evo::Ramht { objects };
        let w = t.words().unwrap();
        // The slots the trace's `modeset-2-inst.txt` has for chid 3's
        // handles (nouveau hashes the same way).
        for (chid, handle) in [(3, HANDLE_SYNC), (3, HANDLE_LUT), (3, evo::HANDLE_WNDW_CTX), (0, HANDLE_LUT)] {
            let (slot, _) = evo::ramht_entry(chid, handle, 0);
            assert!(w.iter().any(|(o, v)| *o == slot && *v == handle), "chid {chid} handle {handle:#x}");
        }
        // Two objects for the notifier (4 KiB) and the surface (flags 5),
        // one object per handle: 5 objects.
        assert_eq!(w.iter().filter(|(o, _)| *o >= evo::DMAOBJ_OFFSET && (*o - evo::DMAOBJ_OFFSET) % 0x20 == 0).count(), 5);
    }

    #[test]
    fn head_off_matches_round_1() {
        // Round 1: SOR_SET_CONTROL(1) = 0 and HEAD_SET_DISPLAY_ID(0,0) = 0.
        assert_eq!(head_off_methods(0, 1), alloc::vec![(0x2020, 0), (0x320, 0)]);
    }

    #[test]
    fn the_windows_channel_is_the_traces() {
        assert_eq!(window_chan(1), evo::window(2));
        assert_eq!(window_chan(1).user_base(), 0x69_2000);
    }
}
