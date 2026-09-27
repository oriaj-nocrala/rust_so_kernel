//! Bit-banged I2C (GF119+ port registers): the HDMI connector's DDC.
//!
//! The line control is `gf119_i2c_bus` (`nvkm/subdev/i2c/busgf119.c:32-94`);
//! the algorithm is nouveau's own bit-banger (`nvkm/subdev/i2c/bit.c:27-196`,
//! MIT). Paths relative to `drivers/gpu/drm/nouveau/` in the pinned Linux
//! v7.2.2.
//!
//! In `trace-nogsp` Linux drove this port with its generic `i2c-algo-bit`
//! instead (nouveau's own is behind `CONFIG_NOUVEAU_I2C_INTERNAL`,
//! `bus.c:214`), so the access sequence differs from the trace; the register
//! bits do not. They were checked against the trace by decoding its port-5
//! accesses back into I2C (`scripts/gpu-trace.py i2c trace-nogsp 5`), which
//! yields the HP 2309's EDID byte for byte.

use crate::Mmio;

/// Timings in ns (`bit.c:27-29`).
const T_TIMEOUT: u32 = 2_200_000;
const T_RISEFALL: u32 = 1000;
const T_HOLD: u32 = 5000;

/// Line bits of the port register (`busgf119.c:32-62`): drive SCL/SDA in
/// bits 0/1, sensed levels in bits 4/5.
const DRIVE_SCL: u32 = 0x01;
const DRIVE_SDA: u32 = 0x02;
const SENSE_SCL: u32 = 0x10;
const SENSE_SDA: u32 = 0x20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum I2cError {
    /// SCL never went high: a slave stretching the clock forever, or no
    /// pull-up (`bit.c:61-72`, `-EBUSY`/`-ETIMEDOUT`).
    Stuck,
    /// A byte was not acknowledged (`bit.c:141-155`, `-EIO`): nothing at
    /// that address, e.g. no monitor.
    Nack,
}

/// One message: a write of `buf` or a read into it.
pub struct Msg<'b> {
    pub addr: u8,
    pub read: bool,
    pub buf: &'b mut [u8],
}

/// A bit-banged port.
pub struct Bus<'a, M: Mmio> {
    m: &'a M,
    reg: u32,
}

impl<'a, M: Mmio> Bus<'a, M> {
    /// Port `drive` of the CCB (`busgf119.c:93`).
    pub fn new(m: &'a M, drive: u8) -> Self {
        Bus { m, reg: port_reg(drive) }
    }

    /// `gf119_i2c_bus_init` (`busgf119.c:64-70`): both lines released.
    pub fn init(&self) {
        self.m.wr32(self.reg, 0x0000_0007);
    }

    fn drive_scl(&self, s: bool) {
        self.m.mask(self.reg, DRIVE_SCL, if s { DRIVE_SCL } else { 0 });
    }
    fn drive_sda(&self, s: bool) {
        self.m.mask(self.reg, DRIVE_SDA, if s { DRIVE_SDA } else { 0 });
    }
    fn sense_scl(&self) -> bool {
        self.m.rd32(self.reg) & SENSE_SCL != 0
    }
    fn sense_sda(&self) -> bool {
        self.m.rd32(self.reg) & SENSE_SDA != 0
    }
    /// `nvkm_i2c_delay` (`bit.c:55-59`).
    fn delay(&self, ns: u32) {
        self.m.udelay((ns + 500) / 1000);
    }

    /// `nvkm_i2c_raise_scl` (`bit.c:61-72`).
    fn raise_scl(&self) -> bool {
        let mut timeout = T_TIMEOUT / T_RISEFALL;
        self.drive_scl(true);
        loop {
            self.delay(T_RISEFALL);
            if self.sense_scl() {
                return true;
            }
            timeout -= 1;
            if timeout == 0 {
                return false;
            }
        }
    }

    /// `i2c_start` (`bit.c:74-92`).
    fn start(&self) -> Result<(), I2cError> {
        let mut r = Ok(());
        if !self.sense_scl() || !self.sense_sda() {
            self.drive_scl(false);
            self.drive_sda(true);
            if !self.raise_scl() {
                r = Err(I2cError::Stuck);
            }
        }
        self.drive_sda(false);
        self.delay(T_HOLD);
        self.drive_scl(false);
        self.delay(T_HOLD);
        r
    }

    /// `i2c_stop` (`bit.c:94-105`).
    fn stop(&self) {
        self.drive_scl(false);
        self.drive_sda(false);
        self.delay(T_RISEFALL);
        self.drive_scl(true);
        self.delay(T_HOLD);
        self.drive_sda(true);
        self.delay(T_HOLD);
    }

    /// `i2c_bitw` (`bit.c:107-120`).
    fn bitw(&self, sda: bool) -> Result<(), I2cError> {
        self.drive_sda(sda);
        self.delay(T_RISEFALL);
        if !self.raise_scl() {
            return Err(I2cError::Stuck);
        }
        self.delay(T_HOLD);
        self.drive_scl(false);
        self.delay(T_HOLD);
        Ok(())
    }

    /// `i2c_bitr` (`bit.c:122-139`).
    fn bitr(&self) -> Result<bool, I2cError> {
        self.drive_sda(true);
        self.delay(T_RISEFALL);
        if !self.raise_scl() {
            return Err(I2cError::Stuck);
        }
        self.delay(T_HOLD);
        let sda = self.sense_sda();
        self.drive_scl(false);
        self.delay(T_HOLD);
        Ok(sda)
    }

    /// `nvkm_i2c_get_byte` (`bit.c:141-155`): ACK every byte but the last.
    fn get_byte(&self, last: bool) -> Result<u8, I2cError> {
        let mut byte = 0u8;
        for i in (0..8).rev() {
            if self.bitr()? {
                byte |= 1 << i;
            }
        }
        self.bitw(last)?;
        Ok(byte)
    }

    /// `nvkm_i2c_put_byte` (`bit.c:157-171`).
    fn put_byte(&self, byte: u8) -> Result<(), I2cError> {
        for i in (0..8).rev() {
            self.bitw(byte & (1 << i) != 0)?;
        }
        if self.bitr()? {
            return Err(I2cError::Nack);
        }
        Ok(())
    }

    /// `nvkm_i2c_bit_xfer` (`bit.c:173-196`): a START (repeated between
    /// messages) and the address before each message, one STOP at the end,
    /// always.
    pub fn xfer(&self, msgs: &mut [Msg]) -> Result<(), I2cError> {
        let mut r = Ok(());
        for msg in msgs.iter_mut() {
            r = self.start();
            if r.is_ok() {
                r = self.put_byte(msg.addr << 1 | msg.read as u8);
            }
            let n = msg.buf.len();
            for i in 0..n {
                if r.is_err() {
                    break;
                }
                if msg.read {
                    match self.get_byte(i + 1 == n) {
                        Ok(b) => msg.buf[i] = b,
                        Err(e) => r = Err(e),
                    }
                } else {
                    r = self.put_byte(msg.buf[i]);
                }
            }
            if r.is_err() {
                break;
            }
        }
        self.stop();
        r
    }

    /// One 128-byte EDID block: write the offset, read the block, the two
    /// messages DRM's EDID reader sends (`trace-nogsp`, port 5: "wr 1 [00]",
    /// "rd 128"). Blocks 0 and 1 only (no E-DDC segment pointer, as in the
    /// trace).
    pub fn read_edid_block(&self, block: u8, out: &mut [u8; 128]) -> Result<(), I2cError> {
        assert!(block < 2);
        let mut off = [block * 128];
        self.xfer(&mut [
            Msg { addr: crate::aux::DDC_ADDR, read: false, buf: &mut off },
            Msg { addr: crate::aux::DDC_ADDR, read: true, buf: out },
        ])
    }
}

/// The port register of CCB drive `drive` (`busgf119.c:93`).
pub fn port_reg(drive: u8) -> u32 {
    0x00d014 + drive as u32 * 0x20
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use core::cell::{Cell, RefCell};

    /// An open-drain bus with a 256-byte DDC EEPROM at 0x50 on it,
    /// behind the port register. The line level is our drive AND the
    /// slave's (the slave only ever pulls SDA). The slave acts on edges of
    /// the line levels, like a real one.
    pub struct DdcSim {
        pub reg: u32,
        drive: Cell<u32>,
        rom: Option<[u8; 256]>,
        st: RefCell<Slave>,
        pub scl_stuck_low: bool,
        pub accesses: Cell<usize>,
    }

    #[derive(Default)]
    struct Slave {
        scl: bool,
        sda: bool,
        state: State,
        shift: u8,
        bits: u8,
        ptr: u8,
        pull: bool,
        first_write: bool,
    }

    #[derive(Default, PartialEq, Clone, Copy)]
    enum State {
        #[default]
        Idle,
        Addr,
        AckAddr { read: bool },
        Write,
        AckWrite,
        Read,
        AckRead,
    }

    impl DdcSim {
        pub fn new(drive: u8, rom: Option<[u8; 256]>) -> Self {
            DdcSim {
                reg: port_reg(drive),
                drive: Cell::new(0x7),
                rom,
                st: RefCell::new(Slave { scl: true, sda: true, ..Default::default() }),
                scl_stuck_low: false,
                accesses: Cell::new(0),
            }
        }

        fn levels(&self) -> (bool, bool) {
            let s = self.st.borrow();
            let d = self.drive.get();
            (d & DRIVE_SCL != 0 && !self.scl_stuck_low, d & DRIVE_SDA != 0 && !s.pull)
        }

        fn step(&self) {
            let (scl, sda) = self.levels();
            let mut s = self.st.borrow_mut();
            let (pscl, psda) = (s.scl, s.sda);
            s.scl = scl;
            s.sda = sda;
            let Some(rom) = self.rom else { return };
            if pscl && scl && psda && !sda {
                // START
                s.state = State::Addr;
                s.shift = 0;
                s.bits = 0;
                s.pull = false;
                return;
            }
            if pscl && scl && !psda && sda {
                s.state = State::Idle; // STOP
                s.pull = false;
                return;
            }
            if !pscl && scl {
                // rising: sample
                match s.state {
                    State::Addr | State::Write => {
                        s.shift = s.shift << 1 | sda as u8;
                        s.bits += 1;
                    }
                    State::AckRead => {
                        // master ACK (low) → next byte; NACK → done
                        s.state = if sda { State::Idle } else { State::Read };
                        s.bits = 0;
                    }
                    _ => {}
                }
            } else if pscl && !scl {
                // falling: drive
                match s.state {
                    State::Addr if s.bits == 8 => {
                        let (a, read) = (s.shift >> 1, s.shift & 1 != 0);
                        if a == crate::aux::DDC_ADDR {
                            s.pull = true;
                            s.state = State::AckAddr { read };
                        } else {
                            s.state = State::Idle;
                        }
                    }
                    State::AckAddr { read } => {
                        s.pull = false;
                        s.bits = 0;
                        s.shift = 0;
                        if read {
                            s.state = State::Read;
                            let b = rom[s.ptr as usize];
                            s.pull = b & 0x80 == 0;
                            s.bits = 1;
                        } else {
                            s.state = State::Write;
                            s.first_write = true;
                        }
                    }
                    State::Write if s.bits == 8 => {
                        if s.first_write {
                            s.ptr = s.shift;
                            s.first_write = false;
                        }
                        s.pull = true;
                        s.state = State::AckWrite;
                    }
                    State::AckWrite => {
                        s.pull = false;
                        s.bits = 0;
                        s.shift = 0;
                        s.state = State::Write;
                    }
                    State::Read => {
                        if s.bits == 8 {
                            s.pull = false; // release for the master's ACK
                            s.ptr = s.ptr.wrapping_add(1);
                            s.state = State::AckRead;
                        } else {
                            let b = rom[s.ptr as usize];
                            s.pull = b & (0x80 >> s.bits) == 0;
                            s.bits += 1;
                        }
                    }
                    State::AckRead => {}
                    _ => {}
                }
                // First bit of the next byte after a master ACK.
                if s.state == State::Read && s.bits == 0 {
                    let b = rom[s.ptr as usize];
                    s.pull = b & 0x80 == 0;
                    s.bits = 1;
                }
            }
        }
    }

    impl Mmio for DdcSim {
        fn rd32(&self, o: u32) -> u32 {
            self.accesses.set(self.accesses.get() + 1);
            assert_eq!(o, self.reg, "only the port register");
            let (scl, sda) = self.levels();
            self.drive.get() | (scl as u32) << 4 | (sda as u32) << 5
        }
        fn wr32(&self, o: u32, v: u32) {
            self.accesses.set(self.accesses.get() + 1);
            assert_eq!(o, self.reg, "only the port register");
            self.drive.set(v & 0x7);
            self.step();
        }
        fn udelay(&self, _: u32) {}
    }

    const HP: &[u8] = include_bytes!("../fixtures/edid-hp-2309.bin");

    #[test]
    fn edid_from_a_simulated_ddc_eeprom() {
        let m = DdcSim::new(5, Some(HP.try_into().unwrap()));
        assert_eq!(m.reg, 0xd0b4, "port 5 is the one nouveau used (trace-nogsp)");
        let bus = Bus::new(&m, 5);
        bus.init();
        let mut edid = [0u8; 256];
        let (b0, b1) = edid.split_at_mut(128);
        bus.read_edid_block(0, b0.try_into().unwrap()).unwrap();
        bus.read_edid_block(1, b1.try_into().unwrap()).unwrap();
        assert_eq!(&edid[..], HP);
    }

    #[test]
    fn nothing_connected_is_a_nack() {
        let m = DdcSim::new(5, None);
        let bus = Bus::new(&m, 5);
        assert_eq!(bus.read_edid_block(0, &mut [0; 128]), Err(I2cError::Nack));
        // Released at the end: both lines high.
        assert_eq!(m.rd32(m.reg) & 0x33, 0x33);
    }

    #[test]
    fn stuck_clock_is_bounded() {
        let mut m = DdcSim::new(5, Some([0; 256]));
        m.scl_stuck_low = true;
        let bus = Bus::new(&m, 5);
        assert_eq!(bus.read_edid_block(0, &mut [0; 128]), Err(I2cError::Stuck));
        // One raise_scl timeout (2200 polls) per failing step, not forever.
        assert!(m.accesses.get() < 20_000, "{}", m.accesses.get());
    }
}
