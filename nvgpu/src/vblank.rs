//! Vblank on the GA106 display, without GSP (phase 3 of the plan).
//!
//! Three parts, all from nouveau's non-GSP path as the `trace-nogsp`
//! oracle shows it:
//! - [`HeadTiming`]: what a head is scanning out (the ARM copy of its
//!   state: totals, blanking, pixel clock), so the driver can tell which
//!   heads the firmware lit and at what rate;
//! - [`arm`]: the interrupt tree (VFN) and the display's head-timing
//!   interrupt enables, so a vblank reaches the CPU as an MSI;
//! - [`service`]: one pass of nouveau's interrupt handler (`nvkm_intr` +
//!   `gv100_disp_intr`), register for register as the trace does it.
//!
//! Interrupt tree (Turing+): a top register says which leaf pairs have
//! something; each leaf has a status (write 1 to clear), an allow and a
//! block register; "unarm"/"rearm" gate the whole tree. The display is leaf
//! 4, bit 26. After each MSI the GPU sends no other until the MSI rearm
//! register is written.

use crate::Mmio;

/// `nvkm_vfn`'s register base on GA100+ (`subdev/vfn/ga100.c:51`).
pub const VFN_PRIV: u32 = 0xb8_0000;
/// Leaf status, write 1 to clear ("reset", `subdev/vfn/tu102.c:33`).
pub const VFN_LEAF_STAT: u32 = VFN_PRIV + 0x1000;
/// Leaf allow (`subdev/vfn/tu102.c:41`).
pub const VFN_LEAF_ALLOW: u32 = VFN_PRIV + 0x1200;
/// Leaf block (`subdev/vfn/tu102.c:49`).
pub const VFN_LEAF_BLOCK: u32 = VFN_PRIV + 0x1400;
/// Top-level pending: bit `leaf / 2` (`subdev/vfn/tu102.c:73-78`).
pub const VFN_TOP: u32 = VFN_PRIV + 0x1600;
/// Re-enable the top-level sources (`subdev/vfn/tu102.c:57`).
pub const VFN_REARM: u32 = VFN_PRIV + 0x1608;
/// Disable the top-level sources (`subdev/vfn/tu102.c:65`).
pub const VFN_UNARM: u32 = VFN_PRIV + 0x1610;
/// Both take `0xf` (`subdev/vfn/tu102.c:57,65`).
const VFN_ALL_TOP: u32 = 0x0000_000f;
/// Leaves the tree has (`subdev/vfn/tu102.c:76`: `leaf < 8`).
pub const VFN_LEAVES: u32 = 8;

/// The display's place in the tree (`subdev/vfn/ga100.c:30`).
pub const DISP_LEAF: u32 = 4;
pub const DISP_BIT: u32 = 0x0400_0000;

/// MSI rearm: the PCI config mirror at `0x088000` (`subdev/pci/gp100.c:34`)
/// + `0x0704`, written 0 (`subdev/pci/gp100.c:29`).
pub const PCI_MSI_REARM: u32 = 0x08_8704;

/// `NV_PMC_BOOT_0`; all ones means the GPU fell off the bus
/// (`core/intr.c:191`).
const PMC_BOOT_0: u32 = 0x00_0000;

/// Display interrupt summary: bit n = head n's timing interrupt; 0x200
/// window exceptions, 0x400 window-IM, 0x800 other, 0x1000 CTRL_DISP
/// (supervisor) (`engine/disp/gv100.c:1082-1110`).
pub const DISP_INTR: u32 = 0x61_1ec0;
const DISP_INTR_HEADS: u32 = 0x0000_00ff;

/// CTRL_DISP interrupt enable (AWAKEN, ERROR, SUPERVISOR1-3), which
/// nouveau sets to `0x187` (`engine/disp/gv100.c:1198`). Only read here:
/// what the firmware left.
pub const CTRL_DISP_EN: u32 = 0x61_1db0;

/// Head n's timing status, write 1 to clear: bit 0-1 LAST_DATA/LOADV, bit 2
/// VBLANK (`engine/disp/gv100.c:1057-1073`).
pub const HEAD_TIMING_STAT: u32 = 0x61_1800;
const HEAD_TIMING_LOADV: u32 = 0x0000_0003;
pub const HEAD_TIMING_VBLANK: u32 = 0x0000_0004;
/// Head n's timing interrupt mask (MSK) and enable (EN), 4 bytes apart per
/// head (`engine/disp/gv100.c:1216-1217`; vblank_get sets EN bit 2,
/// `:253`).
pub const HEAD_TIMING_MSK: u32 = 0x61_1cc0;
pub const HEAD_TIMING_EN: u32 = 0x61_1d80;

/// Heads the display has: bits 0-7 (`engine/disp/gv100.c:324`).
pub const DISP_HEADS: u32 = 0x61_0060;
/// Most heads a mask can name (`engine/disp/gv100.c:1087`: 8 bits).
pub const HEADS_MAX: u32 = 8;

/// Raster position: vline in `0x616330`, hline in `0x616334`, 0x800 per
/// head; reading vline latches hline (`engine/disp/gv100.c:259-263`).
pub const HEAD_RG_VLINE: u32 = 0x61_6330;
pub const HEAD_RG_HLINE: u32 = 0x61_6334;
const HEAD_RG_STRIDE: u32 = 0x800;

/// Head state: `0x682000 + 0x8000` for the ARM copy (what scans out now)
/// `+ head * 0x400` (`engine/disp/gv100.c:270`).
const HEAD_STATE_ARM: u32 = 0x68_2000 + 0x8000;
const HEAD_STATE_STRIDE: u32 = 0x400;
/// `engine/disp/gv100.c:273-287`.
const HS_DEPTH: u32 = 0x004;
const HS_HZ: u32 = 0x00c;
const HS_TOTAL: u32 = 0x064;
const HS_SYNCE: u32 = 0x068;
const HS_BLANKE: u32 = 0x06c;
const HS_BLANKS: u32 = 0x070;

/// Which heads exist (`DISP_HEADS` bits 0-7).
pub fn head_mask(m: &dyn Mmio) -> u8 {
    (m.rd32(DISP_HEADS) & 0xff) as u8
}

/// A head's scanout timing, from its ARM state (`gv100_head_state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeadTiming {
    pub htotal: u16,
    pub vtotal: u16,
    pub hsynce: u16,
    pub vsynce: u16,
    pub hblanke: u16,
    pub vblanke: u16,
    pub hblanks: u16,
    pub vblanks: u16,
    /// Pixel clock in Hz.
    pub hz: u32,
    /// Bits 4-7 of the depth register: 5 = 30 bpp, 4 = 24, 1 = 18
    /// (`engine/disp/gv100.c:288-293`).
    pub depth_code: u8,
}

impl HeadTiming {
    pub fn read_armed(m: &dyn Mmio, head: u32) -> HeadTiming {
        let base = HEAD_STATE_ARM + head * HEAD_STATE_STRIDE;
        let hi = |v: u32| (v >> 16) as u16;
        let lo = |v: u32| (v & 0xffff) as u16;
        let total = m.rd32(base + HS_TOTAL);
        let synce = m.rd32(base + HS_SYNCE);
        let blanke = m.rd32(base + HS_BLANKE);
        let blanks = m.rd32(base + HS_BLANKS);
        HeadTiming {
            vtotal: hi(total),
            htotal: lo(total),
            vsynce: hi(synce),
            hsynce: lo(synce),
            vblanke: hi(blanke),
            hblanke: lo(blanke),
            vblanks: hi(blanks),
            hblanks: lo(blanks),
            hz: m.rd32(base + HS_HZ),
            depth_code: ((m.rd32(base + HS_DEPTH) >> 4) & 0xf) as u8,
        }
    }

    /// Scanning something out: a clock and a raster.
    pub fn active(&self) -> bool {
        self.hz != 0 && self.htotal != 0 && self.vtotal != 0
    }

    /// Refresh rate in millihertz (0 when inactive).
    pub fn refresh_mhz(&self) -> u64 {
        if !self.active() {
            return 0;
        }
        self.hz as u64 * 1000 / (self.htotal as u64 * self.vtotal as u64)
    }

    /// Visible size: blank start minus blank end (the width is computed
    /// the same way in `dispnv50/disp.c:1624`). Checked against the trace's
    /// 1920x1080 in the tests.
    pub fn visible(&self) -> (u16, u16) {
        (self.hblanks.saturating_sub(self.hblanke), self.vblanks.saturating_sub(self.vblanke))
    }
}

/// The raster position of `head`: (vline, hline).
pub fn scan_position(m: &dyn Mmio, head: u32) -> (u16, u16) {
    let v = m.rd32(HEAD_RG_VLINE + head * HEAD_RG_STRIDE) & 0xffff;
    let h = m.rd32(HEAD_RG_HLINE + head * HEAD_RG_STRIDE) & 0xffff;
    (v as u16, h as u16)
}

/// Programs the tree and the display so that a vblank on any head of
/// `enable` raises the display's leaf bit, and only that bit. Leaves the
/// tree armed; the MSI itself is the PCI side's.
///
/// The order is nouveau's (`core/intr.c`): unarm (`:178`), block every
/// leaf and allow only what a handler exists for (`:329-331`), reset the
/// bit before allowing it (`:95-97`), rearm (`:335`). nouveau allows GPIO,
/// I2C and PRIVRING on leaf 4 too (`0x44200000` in the trace); this driver
/// services only the display, so only its bit is allowed. The display side
/// is `gv100_disp_init`'s MSK (`engine/disp/gv100.c:1216`, for every head)
/// and `gv100_head_vblank_get`'s EN (`:253`, for the heads wanted).
pub fn arm(m: &dyn Mmio, present: u8, enable: u8) {
    m.wr32(VFN_UNARM, VFN_ALL_TOP);
    for leaf in 0..VFN_LEAVES {
        m.wr32(VFN_LEAF_BLOCK + leaf * 4, 0xffff_ffff);
    }
    m.wr32(VFN_LEAF_STAT + DISP_LEAF * 4, DISP_BIT);
    m.wr32(VFN_LEAF_ALLOW + DISP_LEAF * 4, DISP_BIT);
    for head in 0..HEADS_MAX {
        if present & (1 << head) != 0 {
            m.wr32(HEAD_TIMING_MSK + head * 4, HEAD_TIMING_VBLANK);
        }
    }
    for head in 0..HEADS_MAX {
        if enable & present & (1 << head) != 0 {
            m.mask(HEAD_TIMING_EN + head * 4, HEAD_TIMING_VBLANK, HEAD_TIMING_VBLANK);
        }
    }
    m.wr32(PCI_MSI_REARM, 0);
    m.wr32(VFN_REARM, VFN_ALL_TOP);
}

/// What one pass of [`service`] found.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Serviced {
    /// Heads whose vblank bit was set (and is now acknowledged).
    pub vblank: u8,
    /// Leaf 4's status had the display bit.
    pub display: bool,
    /// `DISP_INTR` bits this driver does not service (not enabled by it;
    /// reported, left alone).
    pub disp_other: u32,
    /// Head timing bits other than LOADV/VBLANK, acknowledged
    /// (nouveau warns "head %08x").
    pub head_other: u32,
    /// Nothing handled but leaves had bits: those bits were blocked, as
    /// nouveau does against interrupt storms (`core/intr.c:211-220`).
    pub blocked: bool,
    /// `PMC_BOOT_0` read all ones: the GPU is off the bus.
    pub gone: bool,
    /// No leaf had anything (a spurious MSI).
    pub spurious: bool,
}

/// One interrupt, as `nvkm_intr` (`core/intr.c:163-229`) and
/// `gv100_disp_intr` (`engine/disp/gv100.c:1078-1110`) service it. Run it
/// once per MSI; it rearms the tree and the MSI before returning.
pub fn service(m: &dyn Mmio) -> Serviced {
    let mut out = Serviced::default();
    m.wr32(VFN_UNARM, VFN_ALL_TOP);
    m.wr32(PCI_MSI_REARM, 0);

    let top = m.rd32(VFN_TOP);
    let mut stat = [0u32; VFN_LEAVES as usize];
    for leaf in 0..VFN_LEAVES {
        if top & (1 << (leaf / 2)) != 0 {
            stat[leaf as usize] = m.rd32(VFN_LEAF_STAT + leaf * 4);
        }
    }
    if stat.iter().all(|&s| s == 0) {
        out.spurious = true;
        m.wr32(VFN_REARM, VFN_ALL_TOP);
        return out;
    }
    if m.rd32(PMC_BOOT_0) == 0xffff_ffff {
        out.gone = true;
        m.wr32(VFN_REARM, VFN_ALL_TOP);
        return out;
    }

    if stat[DISP_LEAF as usize] & DISP_BIT != 0 {
        out.display = true;
        m.wr32(VFN_LEAF_STAT + DISP_LEAF * 4, DISP_BIT);
        let intr = m.rd32(DISP_INTR);
        for head in 0..HEADS_MAX {
            if intr & (1 << head) == 0 {
                continue;
            }
            let reg = HEAD_TIMING_STAT + head * 4;
            let mut s = m.rd32(reg);
            if s & HEAD_TIMING_LOADV != 0 {
                m.wr32(reg, s & HEAD_TIMING_LOADV);
                s &= !HEAD_TIMING_LOADV;
            }
            if s & HEAD_TIMING_VBLANK != 0 {
                out.vblank |= 1 << head;
                m.wr32(reg, HEAD_TIMING_VBLANK);
                s &= !HEAD_TIMING_VBLANK;
            }
            if s != 0 {
                out.head_other |= s;
                m.wr32(reg, s);
            }
        }
        out.disp_other = intr & !DISP_INTR_HEADS;
    } else {
        for leaf in 0..VFN_LEAVES {
            if stat[leaf as usize] != 0 {
                m.wr32(VFN_LEAF_BLOCK + leaf * 4, stat[leaf as usize]);
            }
        }
        out.blocked = true;
    }

    m.wr32(VFN_REARM, VFN_ALL_TOP);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmio::testing::{ReplayMmio, TableMmio};

    const SERVICE: &str = include_str!("../fixtures/vblank-service.txt");

    /// The trace's writes minus none: `service` must write exactly what
    /// nouveau wrote for that interrupt, in the same order.
    #[test]
    fn service_replays_the_trace() {
        let m = ReplayMmio::from_extract(SERVICE);
        let s = service(&m);
        assert_eq!(s, Serviced { vblank: 0b11, display: true, ..Default::default() });
        assert_eq!(*m.writes.borrow(), m.expected_writes);
        for reg in [VFN_TOP, VFN_LEAF_STAT + 16, VFN_LEAF_STAT + 20, DISP_INTR, HEAD_TIMING_STAT, HEAD_TIMING_STAT + 4] {
            assert_eq!(m.unread(reg), 0, "{reg:#x} read a different number of times than the trace");
        }
    }

    #[test]
    fn head_timing_decodes_the_trace_mode() {
        let m = ReplayMmio::from_extract(SERVICE);
        let t = HeadTiming::read_armed(&m, 0);
        // The trace's head 0 after nouveau's modeset: CEA 1920x1080@60,
        // 148.5 MHz, 2200x1125 total.
        assert_eq!((t.htotal, t.vtotal, t.hz), (2200, 1125, 148_500_000));
        assert_eq!((t.hblanke, t.hblanks, t.vblanke, t.vblanks), (191, 2111, 40, 1120));
        assert_eq!((t.hsynce, t.vsynce), (43, 4));
        assert_eq!(t.depth_code, 4);
        assert_eq!(t.visible(), (1920, 1080));
        assert_eq!(t.refresh_mhz(), 60_000);
        assert!(t.active());
        assert_eq!(scan_position(&m, 0), (0x15, 0x5fe));
    }

    #[test]
    fn head_timing_inactive_head() {
        let zero = TableMmio::new(&[(0x68a464, 0), (0x68a468, 0), (0x68a46c, 0), (0x68a470, 0), (0x68a40c, 0), (0x68a404, 0)]);
        let t = HeadTiming::read_armed(&zero, 1);
        assert!(!t.active());
        assert_eq!(t.refresh_mhz(), 0);
    }

    #[test]
    fn arm_writes_nouveaus_sequence_for_the_display_only() {
        let m = TableMmio::new(&[(0x611d80, 0), (0x611d84, 0x10)]);
        arm(&m, 0x0f, 0b0011);
        let mut want = vec![(0xb81610, 0xf)];
        for leaf in 0..8u32 {
            want.push((0xb81400 + leaf * 4, 0xffff_ffff));
        }
        want.extend([(0xb81010, 0x0400_0000), (0xb81210, 0x0400_0000)]);
        // MSK for the four heads present, as gv100_disp_init (trace
        // 11.573196-203: 0x611cc0..cc c = 4).
        want.extend([(0x611cc0, 4), (0x611cc4, 4), (0x611cc8, 4), (0x611ccc, 4)]);
        // EN bit 2 by read-modify-write, other bits kept (trace 12.104292:
        // R 0x611d80 0, W 4).
        want.extend([(0x611d80, 4), (0x611d84, 0x14)]);
        want.extend([(0x088704, 0), (0xb81608, 0xf)]);
        assert_eq!(*m.writes.borrow(), want);
    }

    /// nouveau allows `0x44200000` on leaf 4 (trace 8.419219); this driver
    /// allows a subset of it: the display bit.
    #[test]
    fn allowed_bit_is_the_traces_display_bit() {
        const TRACE_LEAF4_ALLOW: u32 = 0x4420_0000;
        assert_eq!(DISP_BIT & TRACE_LEAF4_ALLOW, DISP_BIT);
        assert_eq!(VFN_LEAF_ALLOW + DISP_LEAF * 4, 0xb81210);
    }

    #[test]
    fn spurious_msi_rearms_and_touches_nothing_else() {
        let m = TableMmio::new(&[(0xb81600, 0)]);
        let s = service(&m);
        assert!(s.spurious);
        assert_eq!(*m.writes.borrow(), vec![(0xb81610, 0xf), (0x088704, 0), (0xb81608, 0xf)]);
    }

    #[test]
    fn foreign_bits_alone_get_blocked() {
        // PRIVRING (leaf 4 bit 30), pending at arm time in the trace
        // (8.419231: 0x40000000), with the display bit clear.
        let m = TableMmio::new(&[(0xb81600, 0x4), (0xb81010, 0x4000_0000), (0xb81014, 0), (0x000000, 0xb760_00a1)]);
        let s = service(&m);
        assert!(s.blocked && !s.display && s.vblank == 0);
        assert_eq!(
            *m.writes.borrow(),
            vec![(0xb81610, 0xf), (0x088704, 0), (0xb81410, 0x4000_0000), (0xb81608, 0xf)]
        );
    }

    #[test]
    fn gpu_off_the_bus_is_reported() {
        let m = TableMmio::new(&[(0xb81600, 0x4), (0xb81010, DISP_BIT), (0xb81014, 0), (0x000000, 0xffff_ffff)]);
        let s = service(&m);
        assert!(s.gone && s.vblank == 0);
        assert_eq!(*m.writes.borrow(), vec![(0xb81610, 0xf), (0x088704, 0), (0xb81608, 0xf)]);
    }

    #[test]
    fn unknown_head_bits_and_supervisor_are_reported() {
        let m = TableMmio::new(&[
            (0xb81600, 0x4),
            (0xb81010, DISP_BIT),
            (0xb81014, 0),
            (0x000000, 0xb760_00a1),
            (0x611ec0, 0x1004),
            (0x611808, 0x0000_0014),
        ]);
        let s = service(&m);
        assert_eq!(s.vblank, 0b100);
        assert_eq!(s.head_other, 0x10);
        assert_eq!(s.disp_other, 0x1000);
        assert_eq!(
            *m.writes.borrow(),
            vec![(0xb81610, 0xf), (0x088704, 0), (0xb81010, DISP_BIT), (0x611808, 4), (0x611808, 0x10), (0xb81608, 0xf)]
        );
    }
}
