//! Hybrid I2C/AUX pads (GM200+): a pad is shared by a DP AUX channel and a
//! bit-banged I2C port and must be switched to the mode in use.
//!
//! Port of `gm200_i2c_pad_mode` (`nvkm/subdev/i2c/padgm200.c:28-50`,
//! paths relative to `drivers/gpu/drm/nouveau/` in the pinned Linux v7.2.2).
//! The pad index is the CCB entry's `share` (`i2c/base.c:288-291`).

use crate::Mmio;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PadMode {
    I2c,
    Aux,
}

/// Mode register (`padgm200.c:40,44`).
pub fn mode_reg(pad: u8) -> u32 {
    0x00d970 + pad as u32 * 0x50
}
/// Power register; bit 0 set = pad off (`padgm200.c:37,41,45`).
pub fn power_reg(pad: u8) -> u32 {
    0x00d97c + pad as u32 * 0x50
}

const MODE_MASK: u32 = 0x0000_c003;

/// What [`acquire`] found, to put back with [`release`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadSaved {
    pub pad: u8,
    mode: u32,
    power: u32,
    set: u32,
}

/// `gm200_i2c_pad_mode(pad, mode)`: the same two read-modify-writes, in the
/// same order (the traces show R/W of the mode register, then of the power
/// register).
pub fn acquire(m: &impl Mmio, pad: u8, mode: PadMode) -> PadSaved {
    let set = match mode {
        PadMode::I2c => 0x0000_c001,
        PadMode::Aux => 0x0000_0002,
    };
    let old_mode = m.mask(mode_reg(pad), MODE_MASK, set);
    let old_power = m.mask(power_reg(pad), 1, 0);
    PadSaved { pad, mode: old_mode, power: old_power, set }
}

/// Puts the pad back as [`acquire`] found it. Nouveau's release powers the
/// pad off (`pad.c:67-73` with mode OFF, `padgm200.c:37`), which on the
/// target board is also what it found (`trace-nogsp`: power bit 1 before
/// every acquire); restoring the value read keeps that true on any board.
/// The mode field is written back only if acquire changed it.
pub fn release(m: &impl Mmio, s: PadSaved) {
    m.mask(power_reg(s.pad), 1, s.power & 1);
    if s.mode & MODE_MASK != s.set {
        m.mask(mode_reg(s.pad), MODE_MASK, s.mode & MODE_MASK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmio::testing::TableMmio;

    /// The HDMI pad (hybrid 2) in `trace-nogsp`: mode already I2C
    /// (`0xe3a1`), power off (1). Acquire and release write exactly what
    /// nouveau wrote (lines "W da10 e3a1, W da1c 0 ... W da1c 1").
    #[test]
    fn hdmi_pad_like_the_trace() {
        let m = TableMmio::new(&[(0xda10, 0xe3a1), (0xda1c, 1)]);
        let s = acquire(&m, 2, PadMode::I2c);
        release(&m, s);
        assert_eq!(*m.writes.borrow(), [(0xda10, 0xe3a1), (0xda1c, 0), (0xda1c, 1)]);
        assert_eq!(*m.reads.borrow(), [0xda10, 0xda1c, 0xda1c]);
    }

    /// The DP pad (hybrid 3): `0x23a2` is already AUX mode.
    #[test]
    fn dp_pad_and_restore_of_a_changed_mode() {
        let m = TableMmio::new(&[(0xda60, 0x23a2), (0xda6c, 1)]);
        let s = acquire(&m, 3, PadMode::Aux);
        release(&m, s);
        assert_eq!(*m.writes.borrow(), [(0xda60, 0x23a2), (0xda6c, 0), (0xda6c, 1)]);
        // A pad found in I2C mode and powered: switched to AUX, then put
        // back (the TableMmio does not latch writes, so each mask reads the
        // original value).
        let m = TableMmio::new(&[(0xda60, 0xe3a1), (0xda6c, 0)]);
        let s = acquire(&m, 3, PadMode::Aux);
        release(&m, s);
        assert_eq!(*m.writes.borrow(), [(0xda60, 0x23a2), (0xda6c, 0), (0xda6c, 0), (0xda60, 0xe3a1)]);
    }
}
