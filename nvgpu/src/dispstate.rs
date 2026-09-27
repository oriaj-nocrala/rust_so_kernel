//! The display state the firmware (GOP) left, read without writing
//! (phase 5.1 of the plan).
//!
//! Every display channel has two copies of its method state in BAR0:
//! ASSEMBLY (what the pushed methods built so far) and ARMED (what the last
//! UPDATE latched, i.e. what scans out). nouveau dumps both at each
//! supervisor 1 (`engine/disp/nv50.c:451-510`, `nv50_disp_chan_mthd`); the
//! method lists and addresses below are its GA102 ones (`gv100.c`). Only
//! ARMED is read here: with the GOP the channels are not running, and the
//! ARMED core state is what the 5.2 channels must repeat.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::Mmio;

/// Core channel method state: ASSEMBLY at `0x680000 + method`
/// (`engine/disp/gv100.c:615`), ARMED `+0x8000` (`.prev`, `gv100.c:711`).
pub const CORE_ARMED: u32 = 0x68_0000 + 0x8000;
/// Window w: ASSEMBLY at `0x690000 + method + w * 0x1000`
/// (`gv100.c:422`, `.addr` at `:510`), ARMED `+0x800` (`.prev`, `:511`).
pub const WNDW_ARMED: u32 = 0x69_0000 + 0x800;
pub const WNDW_STRIDE: u32 = 0x1000;

/// `gv100_disp_core_mthd_base` (`gv100.c:610-624`).
const CORE_BASE: [u32; 7] = [0x0200, 0x0208, 0x020c, 0x0210, 0x0214, 0x0218, 0x021c];
/// `gv100_disp_core_mthd_sor`, 4 SORs 0x20 apart (`gv100.c:626-637`, `:714`).
const CORE_SOR: [u32; 4] = [0x0300, 0x0304, 0x0308, 0x030c];
/// `gv100_disp_core_mthd_wndw`, 8 windows 0x80 apart (`gv100.c:639-651`, `:715`).
const CORE_WNDW: [u32; 5] = [0x1000, 0x1004, 0x1008, 0x100c, 0x1010];
/// `gv100_disp_core_mthd_head`, 4 heads 0x400 apart (`gv100.c:653-705`, `:716`).
const CORE_HEAD: [u32; 45] = [
    0x2000, 0x2004, 0x2008, 0x200c, 0x2014, 0x2018, 0x201c, 0x2020, 0x2028, 0x202c, 0x2030, 0x2038,
    0x203c, 0x2048, 0x204c, 0x2050, 0x2054, 0x2058, 0x205c, 0x2060, 0x2064, 0x2068, 0x206c, 0x2070,
    0x2074, 0x2078, 0x207c, 0x2080, 0x2088, 0x2090, 0x209c, 0x20a0, 0x20a4, 0x20a8, 0x20ac, 0x2180,
    0x2184, 0x218c, 0x2194, 0x2198, 0x219c, 0x21a0, 0x21a4, 0x2214, 0x2218,
];
/// `gv100_disp_wndw_mthd_base` (`gv100.c:417-505`).
const WNDW: [u32; 80] = [
    0x0200, 0x020c, 0x0210, 0x0214, 0x0218, 0x021c, 0x0220, 0x0224, 0x0228, 0x022c, 0x0230, 0x0234,
    0x0238, 0x0240, 0x0244, 0x0248, 0x024c, 0x0250, 0x0254, 0x0260, 0x0264, 0x0268, 0x026c, 0x0270,
    0x0274, 0x0280, 0x0284, 0x0288, 0x028c, 0x0290, 0x0298, 0x029c, 0x02a0, 0x02a4, 0x02a8, 0x02ac,
    0x02b0, 0x02b4, 0x02b8, 0x02bc, 0x02c0, 0x02c4, 0x02c8, 0x02cc, 0x02d0, 0x02d4, 0x02d8, 0x02dc,
    0x02e0, 0x02e4, 0x02e8, 0x02ec, 0x02f0, 0x02f4, 0x02f8, 0x02fc, 0x0300, 0x0304, 0x0308, 0x0310,
    0x0314, 0x0318, 0x031c, 0x0320, 0x0324, 0x0328, 0x032c, 0x033c, 0x0340, 0x0344, 0x0348, 0x034c,
    0x0350, 0x0354, 0x0358, 0x0364, 0x0368, 0x036c, 0x0370, 0x0374,
];

/// The core methods nouveau dumps, in its order (base, SOR 0-3, window
/// 0-7, head 0-3).
pub fn core_methods() -> impl Iterator<Item = u32> {
    let sor = (0..4).flat_map(|i| CORE_SOR.iter().map(move |m| m + i * 0x20));
    let wndw = (0..8).flat_map(|i| CORE_WNDW.iter().map(move |m| m + i * 0x80));
    let head = (0..4).flat_map(|i| CORE_HEAD.iter().map(move |m| m + i * 0x400));
    CORE_BASE.iter().copied().chain(sor).chain(wndw).chain(head)
}

/// The window methods nouveau dumps, in its order.
pub fn window_methods() -> impl Iterator<Item = u32> {
    WNDW.iter().copied()
}

/// ARMED state as `(method, value)` in dump order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub core: Vec<(u32, u32)>,
    pub window0: Vec<(u32, u32)>,
}

impl Snapshot {
    /// Reads only; the registers are plain state (nouveau reads them at any
    /// supervisor, and phase 3 already reads the head part on the target).
    pub fn read_armed(m: &dyn Mmio) -> Snapshot {
        Snapshot {
            core: core_methods().map(|x| (x, m.rd32(CORE_ARMED + x))).collect(),
            window0: window_methods().map(|x| (x, m.rd32(WNDW_ARMED + x))).collect(),
        }
    }

    fn core(&self, mthd: u32) -> u32 {
        self.core.iter().find(|(m, _)| *m == mthd).map_or(0, |(_, v)| *v)
    }

    fn window0(&self, mthd: u32) -> u32 {
        self.window0.iter().find(|(m, _)| *m == mthd).map_or(0, |(_, v)| *v)
    }

    /// Head `h` as the core state has it (`clc67d.h` field layouts).
    pub fn head(&self, h: u32) -> HeadState {
        let b = h * 0x400;
        let raster = self.core(0x2064 + b); // HEAD_SET_RASTER_SIZE, clc67d.h:826-828
        let view = self.core(0x204c + b); // HEAD_SET_VIEWPORT_SIZE_IN, clc67d.h:817-819
        HeadState {
            raster_w: raster & 0x7fff,
            raster_h: (raster >> 16) & 0x7fff,
            view_w: view & 0x7fff,
            view_h: (view >> 16) & 0x7fff,
            // HEAD_SET_PIXEL_CLOCK_FREQUENCY_HERTZ 30:0, clc67d.h:691-692
            pixel_hz: self.core(0x200c + b) & 0x7fff_ffff,
        }
    }

    /// Which heads SOR `s` drives (`SOR_SET_CONTROL_OWNER_MASK` 7:0,
    /// `clc67d.h:299-300`) and its protocol (11:8, `clc67d.h:310-315`:
    /// 1 = TMDS A, 8 = DP A, 9 = DP B).
    pub fn sor_control(&self, s: u32) -> (u8, u8) {
        let v = self.core(0x300 + s * 0x20);
        ((v & 0xff) as u8, ((v >> 8) & 0xf) as u8)
    }

    /// The head window `w` belongs to (`WINDOW_SET_CONTROL_OWNER` 3:0,
    /// `clc67d.h:351-352`; 0xf = none).
    pub fn window_owner(&self, w: u32) -> u8 {
        (self.core(0x1000 + w * 0x80) & 0xf) as u8
    }

    /// Window 0's surface (`clc67e.h:126-182`; units from nouveau's
    /// `wndwc57e.c:60-65`: pitch >> 6, offset >> 8).
    pub fn surface0(&self) -> Surface {
        let size = self.window0(0x224);
        Surface {
            width: size & 0xffff,
            height: size >> 16,
            format: (self.window0(0x22c) & 0xff) as u8,
            block_height: (self.window0(0x228) & 0xf) as u8,
            pitch: (self.window0(0x230) & 0x1fff) * 64,
            ctxdma: self.window0(0x240),
            offset: (self.window0(0x260) as u64) << 8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeadState {
    pub raster_w: u32,
    pub raster_h: u32,
    pub view_w: u32,
    pub view_h: u32,
    pub pixel_hz: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Surface {
    pub width: u32,
    pub height: u32,
    /// `SET_PARAMS_FORMAT`: 0xcf = A8R8G8B8, 0xe6 = X8R8G8B8
    /// (`clc67e.h:144-145`).
    pub format: u8,
    pub block_height: u8,
    /// Bytes per line, if the surface is pitch-linear.
    pub pitch: u32,
    /// `SET_CONTEXT_DMA_ISO(0)` handle.
    pub ctxdma: u32,
    /// `SET_OFFSET(0)` in bytes, within the context DMA.
    pub offset: u64,
}

fn format_name(f: u8) -> &'static str {
    match f {
        0xcf => "A8R8G8B8",
        0xe6 => "X8R8G8B8",
        0xd5 => "A8B8G8R8",
        0xf9 => "X8B8G8R8",
        0xdf => "A2R10G10B10",
        0xd1 => "A2B10G10R10",
        0xe8 => "R5G6B5",
        _ => "?",
    }
}

/// `/proc/dispstate`: a summary, then every ARMED method as
/// `core|wndw0 method value` (the same method column as
/// `nvgpu/fixtures/modeset-core-round1.txt`, to diff on the host).
pub fn render(s: &Snapshot) -> String {
    let mut r = String::new();
    for h in 0..4 {
        let hs = s.head(h);
        if hs.pixel_hz == 0 && hs.raster_w == 0 {
            continue;
        }
        let _ = writeln!(
            r,
            "head{}: raster {}x{} viewport {}x{} pixel clock {} Hz",
            h, hs.raster_w, hs.raster_h, hs.view_w, hs.view_h, hs.pixel_hz
        );
    }
    for sor in 0..4 {
        let (owner, proto) = s.sor_control(sor);
        if owner != 0 {
            let _ = writeln!(r, "sor{}: owner heads {:#04x} protocol {:#x}", sor, owner, proto);
        }
    }
    let _ = write!(r, "window owners:");
    for w in 0..8 {
        let _ = write!(r, " {}", s.window_owner(w));
    }
    let _ = writeln!(r);
    let f = s.surface0();
    let _ = writeln!(
        r,
        "window0: {}x{} format {:#04x} {} block_height {} pitch {} ctxdma {:#010x} offset {:#x}",
        f.width,
        f.height,
        f.format,
        format_name(f.format),
        f.block_height,
        f.pitch,
        f.ctxdma,
        f.offset
    );
    for (m, v) in &s.core {
        let _ = writeln!(r, "core {:04x} {:08x}", m, v);
    }
    for (m, v) in &s.window0 {
        let _ = writeln!(r, "wndw0 {:04x} {:08x}", m, v);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmio::testing::TableMmio;

    const ROUND1: &str = include_str!("../fixtures/modeset-core-round1.txt");
    const PUSH: &str = include_str!("../fixtures/modeset-push.txt");

    /// (method, armed) rows of a `gpu-trace.py core` fixture.
    fn armed_column(text: &str) -> Vec<(u32, u32)> {
        text.lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                (u32::from_str_radix(f[0], 16).unwrap(), u32::from_str_radix(f[1], 16).unwrap())
            })
            .collect()
    }

    /// (A) The method list is nouveau's: reading a BAR0 that holds the
    /// round-1 dump's ARMED column gives that column back, row for row, and
    /// reads nothing but the ARMED copies.
    #[test]
    fn core_list_matches_nouveaus_dump() {
        let armed = armed_column(ROUND1);
        assert_eq!(armed.len(), 243);
        let regs: Vec<(u32, u32)> = armed.iter().map(|(m, v)| (CORE_ARMED + m, *v)).collect();
        let mmio = TableMmio::new(&regs);
        let s = Snapshot::read_armed(&mmio);
        assert_eq!(s.core, armed);
        assert!(mmio.writes.borrow().is_empty());
        let reads = mmio.reads.borrow();
        assert!(reads.iter().all(|&o| (CORE_ARMED..CORE_ARMED + 0x4000).contains(&o)
            || (WNDW_ARMED..WNDW_ARMED + 0x400).contains(&o)));
    }

    /// (A) The GOP's head 0, as the trace saw it before nouveau's first
    /// update: 1080p60 at 148.5 MHz on SOR 1 (DP), window 0 on head 0.
    #[test]
    fn decodes_the_gops_head0() {
        let armed = armed_column(ROUND1);
        let s = Snapshot { core: armed, window0: Vec::new() };
        assert_eq!(
            s.head(0),
            HeadState { raster_w: 2200, raster_h: 1125, view_w: 1920, view_h: 1080, pixel_hz: 148_500_000 }
        );
        assert_eq!(s.sor_control(1), (0x01, 0x9));
        assert_eq!(s.window_owner(0), 0);
    }

    /// (A) Window 0's surface fields, from the methods nouveau pushed for
    /// its first flip (`modeset-push.txt`): 1920x1080 A8R8G8B8, pitch
    /// 7680, 2 MiB into its context DMA.
    #[test]
    fn decodes_window0_surface_from_the_first_flip() {
        let window0: Vec<(u32, u32)> = PUSH
            .lines()
            .filter(|l| l.split_whitespace().nth(1) == Some("wndw0"))
            .map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                (u32::from_str_radix(f[2], 16).unwrap(), u32::from_str_radix(f[3], 16).unwrap())
            })
            .collect();
        assert!(!window0.is_empty());
        let s = Snapshot { core: Vec::new(), window0 };
        assert_eq!(
            s.surface0(),
            Surface {
                width: 1920,
                height: 1080,
                format: 0xcf,
                block_height: 0,
                pitch: 7680,
                ctxdma: 0xfb00_0000,
                offset: 0x20_0000
            }
        );
        let text = render(&s);
        assert!(text.contains("window0: 1920x1080 format 0xcf A8R8G8B8 block_height 0 pitch 7680"), "{text}");
    }
}
