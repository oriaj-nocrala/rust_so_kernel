//! The display engine's hardware cursor (docs/gpu/hw-cursor-plan.md): one cursor channel per head (class C67A, a PIO channel: no push buffer),
//! positioned by two register writes, its image and enable set through the core channel.
//!
//! Sources: `clc67a.h` (cursor channel), `clc67d.h` / `clc37d.h` (core methods), nouveau `nvkm/engine/disp/gv100.c:555-608` (channel life cycle),
//! `dispnv50/headc37d.c:104-152` (the core methods nouveau pushes), `dispnv50/cursc37a.c:28-46` (position, update).

use crate::evo::{self, Chan};
use crate::Mmio;
use alloc::vec::Vec;

/// `gv100_disp_curs = { .ctrl = 73, .user = 73 }` (`gv100.c:604-608`): head `h`'s cursor channel is `73 + h` (`chan.c:206-207`).
pub const CHID_BASE: u32 = 73;

/// The cursor channel of `head`: the control register is `0x6104e0 + chid * 4` (= `0x610604` for head 0), its status `0x610664 + (chid - 1) * 4`
/// (`0x610784`), its exception slot `0x611020 + chid * 12` (`0x61138c`) and its user region `0x690000 + (chid - 1) * 0x1000` (`0x6d8000`), all the
/// generic [`Chan`] formulas (`gv100.c:336,340-350,379`).
pub const fn chan(head: u32) -> Chan {
    Chan { ctrl: CHID_BASE + head, user: CHID_BASE + head }
}

/// The channel is idle when bits 18:16 of its status are 4 (`gv100_disp_curs_idle`, `gv100.c:555-565`: mask `0x70000`, unlike a window's `0xf0000`).
pub const IDLE_MASK: u32 = 0x0007_0000;
pub const IDLE: u32 = 0x0004_0000;

/// The control register's bits: bit 0 = allocated, bit 4 = disable request (`gv100_disp_curs_init` writes 1; `fini` sets bit 4, waits idle, clears bit 0:
/// `gv100.c:577-593`).
pub const CTRL_ENABLE: u32 = 0x1;
pub const CTRL_DISABLE: u32 = 0x10;

/// User region: `FREE` (room for methods, bits 5:0), `UPDATE`, `SET_CURSOR_HOT_SPOT_POINT_OUT(0)` (`clc67a.h:44,46,108`). `UPDATE` carries RELEASE_ELV
/// (bit 0); nouveau writes 1 (`cursc37a.c:34`).
pub const USER_FREE: u32 = 0x008;
pub const USER_UPDATE: u32 = 0x200;
pub const USER_POINT: u32 = 0x208;
pub const UPDATE_RELEASE_ELV: u32 = 1;

/// Core methods, head `h` (`clc67d.h:745,855-863,877`): usage bounds, context DMA, offset, control, composition.
pub const fn core_usage_bounds(h: u32) -> u32 {
    0x2030 + h * 0x400
}
pub const fn core_context_dma(h: u32) -> u32 {
    0x2088 + h * 0x400
}
pub const fn core_offset(h: u32) -> u32 {
    0x2090 + h * 0x400
}
/// The second ("right eye", stereo) slot of the context DMA and the offset (`clc67d.h:855-858`: the `(a, b)` index adds `b * 4`), and
/// `HEAD_SET_PRESENT_CONTROL_CURSOR` (`clc67d.h:859`, MONO = 0). nouveau pushes only slot 0; NVIDIA's own driver pushes the present control and BOTH slots
/// ("HW will just ignore this if it is not in stereo cursor mode", `nvkms-evo3.c:6520-6544`). Ryzen #204/#205: enabling with slot 1 left at 0 raised
/// INVALID_STATE (code 0x43).
pub const fn core_context_dma_right(h: u32) -> u32 {
    0x208c + h * 0x400
}
pub const fn core_offset_right(h: u32) -> u32 {
    0x2094 + h * 0x400
}
pub const fn core_present_control(h: u32) -> u32 {
    0x2098 + h * 0x400
}
pub const fn core_control(h: u32) -> u32 {
    0x209c + h * 0x400
}
pub const fn core_composition(h: u32) -> u32 {
    0x20a0 + h * 0x400
}

/// The head's usage bounds as nouveau sets them (`headc57d.c:240`, `hdmi.rs HEAD_USAGE_BOUNDS`): the GOP leaves `0x1110`, which is the same without the
/// cursor field (bits 2:0 = 0 = none; 4 = 256x256).
pub const USAGE_BOUNDS: u32 = 0x1114;

/// `HEAD_SET_CONTROL_CURSOR` fields (`clc67d.h:864-876`, DE_GAMMA `clc37d.h:848`).
pub const CONTROL_ENABLE: u32 = 1 << 31;
pub const FORMAT_A8R8G8B8: u32 = 0xcf;
pub const FORMAT_A1R5G5B5: u32 = 0xe9;

/// A cursor image's size: 32, 64, 128 or 256 pixels square (`SIZE` bits 9:8). `None` for anything else.
pub fn size_code(px: u32) -> Option<u32> {
    match px {
        32 => Some(0),
        64 => Some(1),
        128 => Some(2),
        256 => Some(3),
        _ => None,
    }
}

/// `HEAD_SET_CONTROL_CURSOR`: enabled, ARGB8888, `size`, hot spot, no de-gamma (`headc37d.c:123-131`).
pub fn control(size: u32, hot_x: u32, hot_y: u32) -> Option<u32> {
    if hot_x > 0xff || hot_y > 0xff {
        return None;
    }
    Some(CONTROL_ENABLE | FORMAT_A8R8G8B8 | size_code(size)? << 8 | hot_x << 12 | hot_y << 20)
}

/// `HEAD_SET_CONTROL_CURSOR_COMPOSITION`: K1 = 0xff, cursor factor K1, viewport factor NEG_K1_TIMES_SRC, blend (`headc37d.c:133-141`; `clc67d.h:878-888`):
/// premultiplied alpha, `K1 * src + (1 - K1 * alpha_src) * dst`.
pub const COMPOSITION: u32 = 0xff | 2 << 8 | 7 << 12;

/// The core methods that turn the cursor of `head` on with the image at `vram` (256-byte aligned) through the context DMA `handle`: usage bounds first, then
/// what NVIDIA's `EvoSetCursorImageC3` pushes (`nvkms-evo3.c:6546-6625`): present control MONO, the context DMA and the offset in BOTH slots, control,
/// composition. `None` if the image's size or hot spot is not valid or the address is not aligned.
pub fn core_methods_set(head: u32, handle: u32, vram: u64, size: u32, hot_x: u32, hot_y: u32) -> Option<Vec<(u32, u32)>> {
    if vram & 0xff != 0 || (vram >> 8) > u32::MAX as u64 {
        return None;
    }
    Some(alloc::vec![
        (core_usage_bounds(head), USAGE_BOUNDS),
        (core_present_control(head), 0),
        (core_context_dma(head), handle),
        (core_context_dma_right(head), handle),
        (core_offset(head), (vram >> 8) as u32),
        (core_offset_right(head), (vram >> 8) as u32),
        (core_control(head), control(size, hot_x, hot_y)?),
        (core_composition(head), COMPOSITION),
    ])
}

/// `HEAD_SET_CONTEXT_DMA_OLUT(h)` (`hdmi.rs`, `clc67d.h`): the head's output LUT context DMA, which resolves on the core for head 1. A control for the cursor's
/// lookup hanging on head 0 (Ryzen #198-#200): the same lookup, another method.
pub const fn core_olut_context_dma(h: u32) -> u32 {
    0x2288 + h * 0x400
}

/// The core's `SET_INTERLOCK_FLAGS` bit that makes its next UPDATE wait for head `h`'s cursor channel (`clc67d.h:153-160`: `INTERLOCK_WITH_CURSOR(i)` = bit i).
/// nouveau's core UPDATE carries it whenever a cursor changes (`corec37d_update`), and the cursor channel then gets its own UPDATE (`cursc37a_update`). A window
/// state change without its interlock raised INVALID_STATE too (Ryzen #89), so the cursor's enable (INVALID_STATE code 0x43, #204-#206) may be the same.
pub const fn core_interlock_with_cursor(h: u32) -> u32 {
    1 << h
}

/// Only the cursor channel's UPDATE (`cursc37a_update`), after a core push that interlocked with it.
pub fn update(m: &dyn Mmio, head: u32) -> Result<(), CursorError> {
    let base = chan(head).user_base();
    if !evo::wait(m, || m.rd32(base + USER_FREE) & 0x3f >= 1) {
        return Err(CursorError::NoRoom(m.rd32(base + USER_FREE)));
    }
    m.wr32(base + USER_UPDATE, UPDATE_RELEASE_ELV);
    Ok(())
}

/// The cursor channel's own `SET_INTERLOCK_FLAGS` (`clc67a.h:80-97`): bit 16 = `INTERLOCK_WITH_CORE`. A window's first update is interlocked from BOTH sides (its
/// `SET_INTERLOCK_FLAGS = 1`, the core's window bit: the HDMI window, Ryzen #89); nouveau's cursor path only flags the core's side, and then the core stood
/// in WAIT_FOR_UPD for ever (Ryzen #207/#208).
pub const USER_INTERLOCK_FLAGS: u32 = 0x204;
pub const INTERLOCK_WITH_CORE: u32 = 1 << 16;

/// The cursor channel's side of an interlocked core update: its flags (with the core), the position, UPDATE. `clear` = flags back to 0 afterwards.
pub fn update_interlocked(m: &dyn Mmio, head: u32, x: i32, y: i32) -> Result<(), CursorError> {
    let base = chan(head).user_base();
    if !evo::wait(m, || m.rd32(base + USER_FREE) & 0x3f >= 3) {
        return Err(CursorError::NoRoom(m.rd32(base + USER_FREE)));
    }
    m.wr32(base + USER_INTERLOCK_FLAGS, INTERLOCK_WITH_CORE);
    m.wr32(base + USER_POINT, point(x, y));
    m.wr32(base + USER_UPDATE, UPDATE_RELEASE_ELV);
    Ok(())
}

pub fn clear_interlock(m: &dyn Mmio, head: u32) {
    m.wr32(chan(head).user_base() + USER_INTERLOCK_FLAGS, 0);
}

/// What `hdmi::head_methods` pushes for head 1 and nobody pushes for head 0 (whose state is the GOP's): the head's procamp and dither control
/// (`clc67d.h:489,705`) and the usage bounds of its window (`clc67d.h:364,416,471`; head `h` scans out window `2h`). Ryzen #212: the cursor enables on head 1,
/// which this driver programs whole, and raises INVALID_STATE (code 0x43) on head 0.
pub const fn core_procamp(h: u32) -> u32 {
    0x2000 + h * 0x400
}
pub const fn core_dither(h: u32) -> u32 {
    0x2018 + h * 0x400
}
/// The head's output LUT methods (`hdmi::olut_methods`: `HEAD_SET_OLUT_CONTROL`, `_FP_NORM_SCALE`, `CONTEXT_DMA_OLUT`, `OFFSET_OLUT`, `headc57d.c:120-128`):
/// head 1 has them, head 0 (the GOP's) has no output LUT while its usage bounds say OLUT_ALLOWED.
pub const fn core_olut(h: u32) -> [u32; 4] {
    [0x2280 + h * 0x400, 0x2284 + h * 0x400, 0x2288 + h * 0x400, 0x228c + h * 0x400]
}
pub const fn window_usage_format(h: u32) -> u32 {
    0x1004 + 2 * h * 0x80
}
pub const fn window_usage_rotated(h: u32) -> u32 {
    0x1008 + 2 * h * 0x80
}
pub const fn window_usage(h: u32) -> u32 {
    0x1010 + 2 * h * 0x80
}

/// The only core methods `/dev/dispctl cursor raw` may push for `head`: the five the cursor uses and the OLUT context DMA control (a debugging ladder, one
/// method per push).
pub fn core_method_allowed(head: u32, method: u32) -> bool {
    [
        core_usage_bounds(head),
        core_context_dma(head),
        core_context_dma_right(head),
        core_offset(head),
        core_offset_right(head),
        core_present_control(head),
        core_control(head),
        core_composition(head),
        core_olut_context_dma(head),
        evo::CORE_SET_INTERLOCK_FLAGS,
        core_olut(head)[0],
        core_olut(head)[1],
        core_olut(head)[3],
        core_procamp(head),
        core_dither(head),
        window_usage_format(head),
        window_usage_rotated(head),
        window_usage(head),
    ]
    .contains(&method)
}

/// What `headc37d_curs_clr` pushes: disabled (format kept), context DMA 0 (`headc37d.c:104-119`).
pub fn core_methods_clear(head: u32) -> Vec<(u32, u32)> {
    alloc::vec![(core_control(head), FORMAT_A8R8G8B8), (core_context_dma(head), 0)]
}

/// The position write's value: X in bits 15:0, Y in 31:16, two's complement (`clc67a.h:109-110`; the sign handling is from nouveau passing `crtc_x/y`).
pub fn point(x: i32, y: i32) -> u32 {
    (x as u32 & 0xffff) | (y as u32 & 0xffff) << 16
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorError {
    /// The channel's control register already has bit 0: somebody (the firmware, a previous call) allocated it.
    AlreadyAllocated(u32),
    /// The status never reported idle.
    NotIdle(u32),
    /// The channel has no room for the two writes (`FREE` < 2).
    NoRoom(u32),
}

/// `gv100_disp_curs_init` (`gv100.c:587-593`): write 1 to the control register, wait for idle. The channel's interrupt (`0x611dac`) is left disabled: its
/// exceptions are read from the slot instead (this driver has no handler for it).
pub fn init(m: &dyn Mmio, head: u32) -> Result<(), CursorError> {
    let c = chan(head);
    let ctl = m.rd32(c.control());
    if ctl & CTRL_ENABLE != 0 {
        return Err(CursorError::AlreadyAllocated(ctl));
    }
    m.wr32(c.control(), CTRL_ENABLE);
    if !evo::wait(m, || m.rd32(c.status().0) & IDLE_MASK == IDLE) {
        return Err(CursorError::NotIdle(m.rd32(c.status().0)));
    }
    Ok(())
}

/// `gv100_disp_curs_intr` (`gv100.c:568-575`): the channel's interrupt enable is bit `16 + head` of `0x611dac`. nouveau sets it BEFORE it allocates the
/// channel (Ryzen oracle: `W 0x611dac 0x10001`, then `W 0x610604 1`); this driver left it off until the core's cursor context DMA lookup hung (Ryzen #198/#199).
pub const INTR_ENABLE: u32 = 0x61_1dac;
pub fn intr(m: &dyn Mmio, head: u32, enable: bool) {
    let bit = 0x0001_0000 << head;
    m.mask(INTR_ENABLE, bit, if enable { bit } else { 0 });
}

/// `gv100_disp_curs_fini` (`gv100.c:577-585`): ask to disable, wait for idle, clear the allocation bit.
pub fn fini(m: &dyn Mmio, head: u32) -> Result<(), CursorError> {
    let c = chan(head);
    m.mask(c.control(), CTRL_DISABLE, CTRL_DISABLE);
    let idle = evo::wait(m, || m.rd32(c.status().0) & IDLE_MASK == IDLE);
    let status = m.rd32(c.status().0);
    m.mask(c.control(), CTRL_ENABLE, 0);
    if idle {
        Ok(())
    } else {
        Err(CursorError::NotIdle(status))
    }
}

/// Moves the cursor: wait for room (`FREE` >= 2, `nvif_chan_wait(.., 1)` waits for one; the update needs the point and the update, so two), write the
/// hot spot's position and `UPDATE` (`cursc37a_point`, `cursc37a_update`). Takes effect when the display latches it; no interlock with the core.
pub fn move_to(m: &dyn Mmio, head: u32, x: i32, y: i32) -> Result<(), CursorError> {
    let base = chan(head).user_base();
    if !evo::wait(m, || m.rd32(base + USER_FREE) & 0x3f >= 2) {
        return Err(CursorError::NoRoom(m.rd32(base + USER_FREE)));
    }
    m.wr32(base + USER_POINT, point(x, y));
    m.wr32(base + USER_UPDATE, UPDATE_RELEASE_ELV);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use core::cell::RefCell;

    /// A cursor channel's registers: writing the control register's bit 0 makes the status idle (when `works`), writes are recorded.
    struct Sim {
        regs: RefCell<BTreeMap<u32, u32>>,
        writes: RefCell<Vec<(u32, u32)>>,
        works: bool,
        free: u32,
        idle_status: u32,
    }
    impl Sim {
        fn new(works: bool) -> Sim {
            Sim { regs: RefCell::new(BTreeMap::new()), writes: RefCell::new(Vec::new()), works, free: 4, idle_status: 0x0004_0000 }
        }
    }
    impl Mmio for Sim {
        fn rd32(&self, offset: u32) -> u32 {
            if offset == 0x6d8008 {
                return self.free;
            }
            *self.regs.borrow().get(&offset).unwrap_or(&0)
        }
        fn wr32(&self, offset: u32, value: u32) {
            self.writes.borrow_mut().push((offset, value));
            self.regs.borrow_mut().insert(offset, value);
            if offset == chan(0).control() && value & CTRL_ENABLE != 0 && self.works {
                self.regs.borrow_mut().insert(chan(0).status().0, self.idle_status);
            }
            if offset == chan(0).control() && value & CTRL_ENABLE == 0 {
                self.regs.borrow_mut().insert(chan(0).status().0, 0);
            }
        }
        fn udelay(&self, _us: u32) {}
    }

    #[test]
    fn the_channel_addresses_are_what_the_probe_read_on_the_ryzen() {
        // Ryzen #189 (docs/gpu/hw-cursor-plan.md section 11): control 0x610604, status 0x610784, exception slot 0x61138c, user region 0x6d8000.
        let c = chan(0);
        assert_eq!(c.control(), 0x610604);
        assert_eq!(c.status().0, 0x610784);
        assert_eq!(c.exception(), 0x61138c);
        assert_eq!(c.user_base(), 0x6d8000);
        assert_eq!(chan(1).control(), 0x610608);
        assert_eq!(chan(1).user_base(), 0x6d9000);
    }

    #[test]
    fn core_methods_match_what_nouveau_pushes() {
        // NVIDIA's EvoSetCursorImageC3 for head 0, ARGB8888, 64x64 (layout 1), hot spot 0: present control MONO, ctxdma and offset >> 8 in both slots, control
        // 0x800000cf | 1 << 8, composition 0x72ff.
        let m = core_methods_set(0, 0xfb00_0001, 0x38_0000, 64, 0, 0).unwrap();
        assert_eq!(
            m,
            alloc::vec![
                (0x2030, 0x1114),
                (0x2098, 0),
                (0x2088, 0xfb00_0001),
                (0x208c, 0xfb00_0001),
                (0x2090, 0x3800),
                (0x2094, 0x3800),
                (0x209c, 0x8000_01cf),
                (0x20a0, 0x72ff)
            ]
        );
        // head 1 adds 0x400 to every method; 256x256 with a hot spot
        let m = core_methods_set(1, 7, 0x100, 256, 3, 5).unwrap();
        assert_eq!(m[0], (0x2430, 0x1114));
        assert_eq!(m[1], (0x2498, 0));
        assert_eq!(m[2], (0x2488, 7));
        assert_eq!(m[3], (0x248c, 7));
        assert_eq!(m[4], (0x2490, 1));
        assert_eq!(m[5], (0x2494, 1));
        assert_eq!(m[6], (0x249c, 0x8000_0000 | 0xcf | 3 << 8 | 3 << 12 | 5 << 20));
        assert_eq!(m[7].0, 0x24a0);
    }

    #[test]
    fn invalid_images_are_refused() {
        assert!(core_methods_set(0, 1, 0x100, 48, 0, 0).is_none()); // not a size the hardware has
        assert!(core_methods_set(0, 1, 0x180, 64, 0, 0).is_none()); // not 256-byte aligned
        assert!(core_methods_set(0, 1, 0x100, 64, 256, 0).is_none()); // hot spot field is 8 bits
        assert!(core_methods_set(0, 1, 0x100, 64, 0, 256).is_none());
        assert!(core_methods_set(0, 1, 1 << 40, 64, 0, 0).is_none()); // offset >> 8 must fit
        assert!(core_methods_set(0, 1, 0xff_ffff_ff00, 64, 255, 255).is_some());
        assert_eq!(size_code(32), Some(0));
        assert_eq!(size_code(128), Some(2));
    }

    #[test]
    fn raw_pushes_are_limited_to_the_cursors_methods_of_that_head() {
        for m in [0x2030, 0x2088, 0x208c, 0x2090, 0x2094, 0x2098, 0x209c, 0x20a0, 0x2288, 0x2280, 0x2284, 0x228c, 0x2000, 0x2018, 0x1004, 0x1008, 0x1010] {
            assert!(core_method_allowed(0, m), "{m:#x}");
        }
        for m in [0x2430, 0x2488, 0x248c, 0x2490, 0x2494, 0x2498, 0x249c, 0x24a0, 0x2688, 0x2680, 0x2684, 0x268c, 0x2400, 0x2418, 0x1104, 0x1108, 0x1110] {
            assert!(core_method_allowed(1, m), "{m:#x}");
            assert!(!core_method_allowed(0, m), "{m:#x}");
        }
        for m in [0x200, 0x21c, 0x2034, 0x2084, 0x2084, 0x20a4, 0x300, 0x2290, 0x2294, 0x2080, 0x209d, 0x2004, 0x201c, 0x1000, 0x1100, 0x100c, 0x1014] {
            assert!(!core_method_allowed(0, m), "{m:#x}");
        }
    }

    #[test]
    fn clearing_matches_curs_clr() {
        assert_eq!(core_methods_clear(0), alloc::vec![(0x209c, 0xcf), (0x2088, 0)]);
        assert_eq!(core_methods_clear(1), alloc::vec![(0x249c, 0xcf), (0x2488, 0)]);
    }

    #[test]
    fn the_position_is_two_halves_in_twos_complement() {
        assert_eq!(point(0, 0), 0);
        assert_eq!(point(100, 200), 100 | 200 << 16);
        assert_eq!(point(-1, -2), 0xfffe_ffff);
        assert_eq!(point(0x1_0005, 3), 5 | 3 << 16); // only 16 bits of each
    }

    #[test]
    fn init_allocates_and_waits_for_idle() {
        let m = Sim::new(true);
        init(&m, 0).unwrap();
        assert_eq!(*m.writes.borrow(), alloc::vec![(0x610604, 1)]);
        // a channel somebody already allocated is not touched
        assert_eq!(init(&m, 0), Err(CursorError::AlreadyAllocated(1)));
        assert_eq!(m.writes.borrow().len(), 1);
        // a channel that never goes idle is reported with its status
        let bad = Sim::new(false);
        assert_eq!(init(&bad, 0), Err(CursorError::NotIdle(0)));
        // idle is bits 18:16 == 4 whatever bit 19 and above say (`gv100_disp_curs_idle`'s mask is 0x70000)
        let other_bits = Sim { idle_status: 0x000c_0000, ..Sim::new(true) };
        init(&other_bits, 0).unwrap();
        // 5 in bits 18:16 is not idle
        let busy = Sim { idle_status: 0x0005_0000, ..Sim::new(true) };
        assert_eq!(init(&busy, 0), Err(CursorError::NotIdle(0x0005_0000)));
    }

    #[test]
    fn fini_requests_disable_then_clears_the_allocation() {
        let m = Sim::new(true);
        init(&m, 0).unwrap();
        m.writes.borrow_mut().clear();
        fini(&m, 0).unwrap();
        assert_eq!(*m.writes.borrow(), alloc::vec![(0x610604, 0x11), (0x610604, 0x10)]);
        // a channel that never goes idle is reported, and still released
        let stuck = Sim { idle_status: 0x0001_0000, ..Sim::new(true) };
        init(&Sim::new(true), 0).unwrap();
        stuck.regs.borrow_mut().insert(0x610604, 1);
        stuck.regs.borrow_mut().insert(0x610784, 0x0001_0000);
        assert_eq!(fini(&stuck, 0), Err(CursorError::NotIdle(0x0001_0000)));
        assert_eq!(stuck.regs.borrow()[&0x610604], 0x10);
    }

    #[test]
    fn the_interrupt_enable_is_bit_16_plus_the_head() {
        let m = Sim::new(true);
        m.regs.borrow_mut().insert(0x611dac, 0x1);
        intr(&m, 0, true);
        assert_eq!(*m.writes.borrow(), alloc::vec![(0x611dac, 0x10001)]);
        intr(&m, 1, true);
        assert_eq!(m.regs.borrow()[&0x611dac], 0x30001);
        intr(&m, 0, false);
        assert_eq!(m.regs.borrow()[&0x611dac], 0x20001); // only head 0's bit, the core's stays
    }

    #[test]
    fn the_interlock_with_the_cursor_is_bit_head() {
        assert_eq!(core_interlock_with_cursor(0), 1);
        assert_eq!(core_interlock_with_cursor(1), 2);
        assert!(core_method_allowed(0, 0x218) && core_method_allowed(1, 0x218));
        let m = Sim::new(true);
        update(&m, 0).unwrap();
        assert_eq!(*m.writes.borrow(), alloc::vec![(0x6d8200, 1)]);
        let full = Sim { free: 0, ..Sim::new(true) };
        assert_eq!(update(&full, 0), Err(CursorError::NoRoom(0)));
        assert!(full.writes.borrow().is_empty());
    }

    #[test]
    fn the_cursor_side_of_an_interlocked_update() {
        let m = Sim::new(true);
        update_interlocked(&m, 0, 7, 9).unwrap();
        assert_eq!(*m.writes.borrow(), alloc::vec![(0x6d8204, 0x10000), (0x6d8208, point(7, 9)), (0x6d8200, 1)]);
        let tight = Sim { free: 2, ..Sim::new(true) };
        assert_eq!(update_interlocked(&tight, 0, 0, 0), Err(CursorError::NoRoom(2)));
        assert!(tight.writes.borrow().is_empty());
        let c = Sim::new(true);
        clear_interlock(&c, 1);
        assert_eq!(*c.writes.borrow(), alloc::vec![(0x6d9204, 0)]);
    }

    #[test]
    fn move_writes_the_point_then_the_update() {
        let m = Sim::new(true);
        move_to(&m, 0, 640, -3).unwrap();
        assert_eq!(*m.writes.borrow(), alloc::vec![(0x6d8208, point(640, -3)), (0x6d8200, 1)]);
        // FREE is bits 5:0: 0x20 is room for 32 methods
        let big = Sim { free: 0x20, ..Sim::new(true) };
        move_to(&big, 0, 1, 1).unwrap();
        // not enough room: nothing is written
        let full = Sim { free: 1, ..Sim::new(true) };
        assert_eq!(move_to(&full, 0, 1, 1), Err(CursorError::NoRoom(1)));
        assert!(full.writes.borrow().is_empty());
    }

    #[test]
    fn the_cursor_handle_does_not_collide_in_the_ramht() {
        use crate::evo::{vram_ctxdma, Ramht, HANDLE_WNDW_CTX};
        let table = Ramht {
            objects: alloc::vec![
                (1, HANDLE_WNDW_CTX, vram_ctxdma(8 << 30)),
                (0, HANDLE_CURSOR_CTX, vram_ctxdma(8 << 30)),
                (0, HANDLE_CURSOR_CTX_PAGED, vram_ctxdma(8 << 30)),
                (CHID_BASE, HANDLE_CURSOR_CTX_PAGED, vram_ctxdma(8 << 30)),
            ],
        };
        assert!(table.words().is_ok());
    }
}

/// The handle the core uses for the context DMA that covers VRAM (the cursor image's): any handle on channel 0 (the core); nouveau's is its own VRAM
/// context DMA's (`curs507a_prepare`). Registered in the RAMHT at boot (`kernel/src/gpu/evo.rs`).
pub const HANDLE_CURSOR_CTX: u32 = 0xfb00_0100;

/// Ryzen #198: with this handle (a context DMA of flags `0x05`, like window 0's surface) the core stood in `STG1_STATE = CTX_DMA_LOOKUP` for ever
/// (`CHNSTATUS_CORE = 0xa20c0005`, exception slot naming method `0x2088`). The core's LUT context DMA (`hdmi::HANDLE_LUT`, flags `0x45` = PAGE | RW | VRAM) does
/// resolve on the core. This is the same range with that flags value, under its own handle.
pub const HANDLE_CURSOR_CTX_PAGED: u32 = 0xfb00_0101;
