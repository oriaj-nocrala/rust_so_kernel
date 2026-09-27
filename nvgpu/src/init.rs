//! VBIOS init scripts ("devinit" opcodes): the interpreter nouveau runs for
//! the display's IED scripts at each supervisor (`subdev/bios/init.c`).
//!
//! Only the opcodes the target board's DP scripts use are ported (measured
//! by decoding them from the VBIOS: `OffInt1/2`, `OnInt2/3` of the output
//! the GOP lit, phase 5.4 of the plan). Any other opcode, executed or
//! skipped, stops the script with [`ScriptError::Unsupported`]: nouveau
//! also stops at an opcode it does not know (`init.c:2302-2318`), and an
//! opcode's length is only known by porting it. HDMI's scripts need more
//! (conditions from tables, polls): phase 5.8.
//!
//! Semantics kept from nouveau: a script runs until `DONE`; `execute` is a
//! small flag set where bit 1 = "skip" (conditions set it, `NOT` flips it,
//! `RESUME` clears it) and skipped opcodes are still parsed
//! (`init.c:32-57`); a sub-script (`SUB_DIRECT`) is entered only when
//! executing.

use crate::vbios::Bios;
use crate::Mmio;

/// What a script runs against (`struct nvbios_init`,
/// `include/nvkm/subdev/bios/init.h:5-19`): the output resource (SOR), its
/// sublink, the head, and the output's connector type.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Target {
    pub or: Option<u8>,
    /// Sublink: 1 = A, 2 = B, 0 = none (`init.c:73-82`).
    pub link: u8,
    pub head: Option<u8>,
    /// `enum dcb_connector_type` of the output's connector (`init_conn`,
    /// `init.c:115-140`).
    pub conn: Option<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScriptError {
    /// An opcode this port has not ported (nouveau may know it).
    Unsupported { offset: u32, opcode: u8 },
    /// A `GENERIC_CONDITION` that needs the DP output table or an AUX read
    /// (`init.c:815-835`), executed.
    UnsupportedCondition { offset: u32, cond: u8 },
    /// The script needs the OR, its link or the head and the target has
    /// none. nouveau logs "script needs OR!!" and goes on with 0
    /// (`init.c:61-94`); here it stops instead of writing the wrong unit.
    NeedsTarget { offset: u32, what: &'static str },
    /// A register outside BAR0's 16 MiB (nouveau warns "unknown bits",
    /// `init.c:175-176`, and goes on).
    BadRegister { offset: u32, reg: u32 },
    /// Sub-scripts nested deeper than [`MAX_NESTED`].
    TooDeep { offset: u32 },
    /// More than [`MAX_STEPS`] opcodes: a loop, or not a script.
    TooLong,
}

/// What a script did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub opcodes: u32,
    pub writes: u32,
}

/// Bounds nouveau does not have: a script is VBIOS data, and this runs in
/// an interrupt handler.
pub const MAX_NESTED: u32 = 8;
pub const MAX_STEPS: u32 = 4096;

/// `DCB_CONNECTOR_eDP` (`include/nvkm/subdev/bios/conn.h:21`).
pub const CONNECTOR_EDP: u8 = 0x47;

/// Runs the script at `offset` (`nvbios_init` with `execute = 1`,
/// `include/nvkm/subdev/bios/init.h:21-33`). An offset of 0 is an empty
/// script. On error the writes made so far stay made, as in nouveau.
pub fn run(m: &dyn Mmio, bios: &Bios, target: Target, offset: u32) -> Result<Stats, ScriptError> {
    let mut e = Exec { m, bios, t: target, offset, at: offset, execute: 1, nested: 0, stats: Stats::default() };
    e.exec()?;
    Ok(e.stats)
}

struct Exec<'a> {
    m: &'a dyn Mmio,
    bios: &'a Bios,
    t: Target,
    offset: u32,
    /// Where the opcode being run starts (for errors).
    at: u32,
    execute: u8,
    nested: u32,
    stats: Stats,
}

impl Exec<'_> {
    /// `init_exec` (`init.c:32-36`).
    fn on(&self) -> bool {
        self.execute == 1 || (self.execute & 5) == 5
    }

    /// `init_exec_set` (`init.c:38-43`).
    fn set(&mut self, exec: bool) {
        if exec {
            self.execute &= 0xfd;
        } else {
            self.execute |= 0x02;
        }
    }

    fn need(&self, v: Option<u8>, what: &'static str) -> Result<u32, ScriptError> {
        v.map(u32::from).ok_or(ScriptError::NeedsTarget { offset: self.at, what })
    }

    /// `init_nvreg` (`init.c:143-178`): bit 31 selects the head's copy
    /// (+0x800 per head), bit 30 the OR's (+0x800 per OR) and, with it, bit
    /// 29 the sublink's (+0x80 for B). Only an executed access needs the
    /// target (`init_or`/`init_head`/`init_link` return 0 otherwise).
    fn nvreg(&self, reg: u32) -> Result<u32, ScriptError> {
        let mut reg = reg & !3;
        if reg & 0x8000_0000 != 0 {
            let head = if self.on() { self.need(self.t.head, "head")? } else { 0 };
            reg = reg.wrapping_add(head * 0x800) & !0x8000_0000;
        }
        if reg & 0x4000_0000 != 0 {
            let or = if self.on() { self.need(self.t.or, "OR")? } else { 0 };
            reg = reg.wrapping_add(or * 0x800) & !0x4000_0000;
            if reg & 0x2000_0000 != 0 {
                let link = if self.on() {
                    if self.t.link == 0 {
                        return Err(ScriptError::NeedsTarget { offset: self.at, what: "OR link" });
                    }
                    (self.t.link == 2) as u32
                } else {
                    0
                };
                reg = reg.wrapping_add(link * 0x80) & !0x2000_0000;
            }
        }
        if self.on() && reg & !0x00ff_fffc != 0 {
            return Err(ScriptError::BadRegister { offset: self.at, reg });
        }
        Ok(reg)
    }

    /// `init_rd32` (`init.c:180-188`).
    fn rd32(&self, reg: u32) -> Result<u32, ScriptError> {
        let reg = self.nvreg(reg)?;
        Ok(if self.on() { self.m.rd32(reg) } else { 0 })
    }

    /// `init_wr32` (`init.c:190-197`).
    fn wr32(&mut self, reg: u32, val: u32) -> Result<(), ScriptError> {
        let reg = self.nvreg(reg)?;
        if self.on() {
            self.m.wr32(reg, val);
            self.stats.writes += 1;
        }
        Ok(())
    }

    /// `init_mask` (`init.c:199-210`): clears `mask`, ors `val`.
    fn mask(&mut self, reg: u32, mask: u32, val: u32) -> Result<(), ScriptError> {
        let reg = self.nvreg(reg)?;
        if self.on() {
            self.m.mask(reg, mask, val);
            self.stats.writes += 1;
        }
        Ok(())
    }

    fn rd08(&self, at: u32) -> u8 {
        self.bios.rd08(self.offset + at)
    }
    fn rd16(&self, at: u32) -> u16 {
        self.bios.rd16(self.offset + at)
    }
    fn rd32b(&self, at: u32) -> u32 {
        self.bios.rd32(self.offset + at)
    }

    /// `nvbios_exec` (`init.c:2302-2318`).
    fn exec(&mut self) -> Result<(), ScriptError> {
        self.nested += 1;
        if self.nested > MAX_NESTED {
            return Err(ScriptError::TooDeep { offset: self.offset });
        }
        while self.offset != 0 {
            self.stats.opcodes += 1;
            if self.stats.opcodes > MAX_STEPS {
                return Err(ScriptError::TooLong);
            }
            self.at = self.offset;
            let opcode = self.rd08(0);
            match opcode {
                0x38 => self.not(),
                0x3a => self.generic_condition()?,
                0x5b => self.sub_direct()?,
                0x5f => self.copy_nv_reg()?,
                0x6e => self.nv_reg()?,
                0x71 => self.offset = 0, // INIT_DONE, `init.c:610-615`
                0x72 => self.resume(),
                0x74 => self.time(),
                0x7a => self.zm_reg()?,
                _ => return Err(ScriptError::Unsupported { offset: self.offset, opcode }),
            }
        }
        self.nested -= 1;
        Ok(())
    }

    /// INIT_NOT, 0x38 (`init.c:767-773`).
    fn not(&mut self) {
        self.offset += 1;
        self.execute ^= 0x02;
    }

    /// INIT_GENERIC_CONDITION, 0x3a (`init.c:796-845`).
    fn generic_condition(&mut self) -> Result<(), ScriptError> {
        let (cond, size) = (self.rd08(1), self.rd08(2));
        let at = self.at;
        self.offset += 3;
        match cond {
            // CONDITION_ID_INT_DP: only an eDP connector. With no connector
            // nouveau reads 0xff, not eDP.
            0 => {
                if self.t.conn != Some(CONNECTOR_EDP) {
                    self.set(false);
                }
            }
            // USE_SPPLL0/1 (DP output table), ASSR_SUPPORT (AUX read).
            // Skipped, they could only set "skip", which is already set.
            1 | 2 | 5 => {
                if self.on() {
                    return Err(ScriptError::UnsupportedCondition { offset: at, cond });
                }
            }
            // CONDITION_ID_NO_PANEL_SEQ_DELAYS: always "skip".
            7 => self.set(false),
            // Unknown: nouveau warns and skips its `size` bytes.
            _ => self.offset += size as u32,
        }
        Ok(())
    }

    /// INIT_SUB_DIRECT, 0x5b (`init.c:1344-1363`).
    fn sub_direct(&mut self) -> Result<(), ScriptError> {
        let addr = self.rd16(1) as u32;
        if self.on() {
            let save = self.offset;
            self.offset = addr;
            self.exec()?;
            self.offset = save;
        }
        self.offset += 3;
        Ok(())
    }

    /// INIT_COPY_NV_REG, 0x5f (`init.c:1415-1434`), with `init_shift`
    /// (`init.c:531-537`).
    fn copy_nv_reg(&mut self) -> Result<(), ScriptError> {
        let sreg = self.rd32b(1);
        let shift = self.rd08(5);
        let smask = self.rd32b(6);
        let sxor = self.rd32b(10);
        let dreg = self.rd32b(14);
        let dmask = self.rd32b(18);
        self.offset += 22;
        let src = self.rd32(sreg)?;
        let data = if shift < 0x80 { src.checked_shr(shift as u32).unwrap_or(0) } else { src.checked_shl(0x100 - shift as u32).unwrap_or(0) };
        self.mask(dreg, !dmask, (data & smask) ^ sxor)
    }

    /// INIT_NV_REG, 0x6e (`init.c:1709-1720`).
    fn nv_reg(&mut self) -> Result<(), ScriptError> {
        let (reg, mask, data) = (self.rd32b(1), self.rd32b(5), self.rd32b(9));
        self.offset += 13;
        self.mask(reg, !mask, data)
    }

    /// INIT_RESUME, 0x72 (`init.c:1751-1757`).
    fn resume(&mut self) {
        self.offset += 1;
        self.set(true);
    }

    /// INIT_TIME, 0x74 (`init.c:1781-1795`): µs below 1000, else whole ms
    /// rounded as nouveau does.
    fn time(&mut self) {
        let usec = self.rd16(1) as u32;
        self.offset += 3;
        if self.on() {
            self.m.udelay(if usec < 1000 { usec } else { (usec + 900) / 1000 * 1000 });
        }
    }

    /// INIT_ZM_REG, 0x7a (`init.c:1892-1905`).
    fn zm_reg(&mut self) -> Result<(), ScriptError> {
        let (addr, mut data) = (self.rd32b(1), self.rd32b(5));
        self.offset += 9;
        if addr == 0x000200 {
            data |= 1;
        }
        self.wr32(addr, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmio::testing::TableMmio;
    use alloc::vec;
    use alloc::vec::Vec;

    /// A VBIOS image holding each `(offset, script)`.
    fn bios_with(scripts: &[(usize, &[u8])]) -> Bios {
        let mut d = crate::vbios::tests::tiny_image();
        for (at, s) in scripts {
            d[*at..*at + s.len()].copy_from_slice(s);
        }
        Bios::new(d).expect("test image")
    }

    fn le(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }

    fn nv_reg(reg: u32, mask: u32, data: u32) -> Vec<u8> {
        [&[0x6e][..], &le(reg), &le(mask), &le(data)].concat()
    }

    #[test]
    fn nv_reg_selects_the_head_and_or_copies() {
        let s = [nv_reg(0x8061_6540, 0xffff_fffe, 0), nv_reg(0x6061_c10c, 0xffff_fffe, 1), vec![0x71]].concat();
        let bios = bios_with(&[(0x100, &s)]);
        let m = TableMmio::new(&[(0x616d40, 0x34101), (0x61c98c, 0x4000)]);
        let t = Target { or: Some(1), link: 2, head: Some(1), conn: None };
        let st = run(&m, &bios, t, 0x100).unwrap();
        assert_eq!(*m.writes.borrow(), vec![(0x616d40, 0x34100), (0x61c98c, 0x4001)]);
        assert_eq!(st, Stats { opcodes: 3, writes: 2 });
    }

    #[test]
    fn conditions_skip_until_resume_and_not_flips() {
        // cond 7 → skip; NOT → execute; write A; cond 0 (not eDP) → skip;
        // write B (skipped); RESUME; write C.
        let s = [
            vec![0x3a, 7, 1, 0x38],
            nv_reg(0x21234, 0xffff_ffff, 0),
            vec![0x3a, 0, 57],
            nv_reg(0x2121c, 0xffff_ffff, 0),
            vec![0x72],
            [&[0x7a][..], &le(0xd604), &le(0)].concat(),
            vec![0x71],
        ]
        .concat();
        let bios = bios_with(&[(0x100, &s)]);
        let m = TableMmio::new(&[(0x21234, 0), (0x2121c, 0)]);
        let dp = Target { or: Some(1), link: 2, head: Some(0), conn: Some(0x46) };
        run(&m, &bios, dp, 0x100).unwrap();
        assert_eq!(*m.writes.borrow(), vec![(0x21234, 0), (0xd604, 0)]);
        // The same script on an eDP connector runs the middle write too.
        let m = TableMmio::new(&[(0x21234, 0), (0x2121c, 0)]);
        run(&m, &bios, Target { conn: Some(CONNECTOR_EDP), ..dp }, 0x100).unwrap();
        assert_eq!(*m.writes.borrow(), vec![(0x21234, 0), (0x2121c, 0), (0xd604, 0)]);
    }

    #[test]
    fn a_skipped_sub_is_not_entered_and_unknown_opcodes_stop() {
        // cond 7 → skip; SUB_DIRECT to a script with an unported opcode
        // (skipped, so never parsed); RESUME; SUB_DIRECT to it again → error.
        let s = [vec![0x3a, 7, 1, 0x5b, 0x00, 0x02, 0x72, 0x5b, 0x00, 0x02, 0x71]].concat();
        let bios = bios_with(&[(0x100, &s), (0x200, &[0x52, 0xe8, 0xdf, 0x00, 0x71])]);
        let m = TableMmio::new(&[]);
        let t = Target { or: Some(0), link: 1, head: Some(0), conn: None };
        assert_eq!(run(&m, &bios, t, 0x100), Err(ScriptError::Unsupported { offset: 0x200, opcode: 0x52 }));
        assert!(m.writes.borrow().is_empty());
    }

    #[test]
    fn copy_nv_reg_masks_and_shifts() {
        // dst 0x616540 keeps 0xbf00bffe, gets (src & 0x40ff4001): the DP
        // OnInt2 sub-script of the target board (VBIOS 0x67e9).
        let s = [
            &[0x5f][..],
            &le(0x6061_c10c),
            &[0x00],
            &le(0x40ff_4001),
            &le(0),
            &le(0x8061_6540),
            &le(0xbf00_bffe),
            &[0x71],
        ]
        .concat();
        let bios = bios_with(&[(0x100, &s)]);
        let m = TableMmio::new(&[(0x61c98c, 0x000f_4001), (0x616540, 0x0003_4100)]);
        run(&m, &bios, Target { or: Some(1), link: 2, head: Some(0), conn: Some(0x46) }, 0x100).unwrap();
        assert_eq!(*m.writes.borrow(), vec![(0x616540, 0x000f_4101)]);
    }

    #[test]
    fn missing_target_and_runaway_scripts_are_errors() {
        let s = [nv_reg(0x4061_c080, 0, 0), vec![0x71]].concat();
        let bios = bios_with(&[(0x100, &s), (0x300, &[0x5b, 0x00, 0x03])]);
        let m = TableMmio::new(&[]);
        assert_eq!(run(&m, &bios, Target::default(), 0x100), Err(ScriptError::NeedsTarget { offset: 0x100, what: "OR" }));
        assert_eq!(run(&m, &bios, Target::default(), 0x300), Err(ScriptError::TooDeep { offset: 0x300 }));
        assert_eq!(run(&m, &bios, Target::default(), 0), Ok(Stats::default()));
    }
}
