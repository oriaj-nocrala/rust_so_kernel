//! DisplayPort AUX channel (GM200+ registers): native DPCD reads and
//! I2C-over-AUX (the EDID).
//!
//! Two layers, each a port of what ran in `trace-nogsp`:
//! - [`Aux::xfer`]: one AUX transaction, `gm200_i2c_aux_xfer` with
//!   `retry = false` (`nvkm/subdev/i2c/auxgm200.c:75-166`; nouveau's DRM
//!   side passes `false`, `nouveau_connector.c:1242`);
//! - the retry policy on top: DRM's DP helper (`drm/display/drm_dp_helper.c`),
//!   native reads (`drm_dp_dpcd_access`, `:590-647`) and I2C messages
//!   (`drm_dp_i2c_do_msg`, `:1990-2098`, `drm_dp_i2c_xfer`, `:2147-2225`).
//!
//! Paths under `drivers/gpu/drm/` in the pinned Linux v7.2.2 (nouveau files
//! relative to `nouveau/`). Both are MIT-style licensed.

use crate::Mmio;

/// AUX request types (`include/drm/display/drm_dp.h:87-92`).
pub const I2C_WRITE: u8 = 0x0;
pub const I2C_READ: u8 = 0x1;
pub const I2C_MOT: u8 = 0x4;
pub const NATIVE_WRITE: u8 = 0x8;
pub const NATIVE_READ: u8 = 0x9;

/// Reply codes (`drm_dp.h:94-101`): native in bits 0-1, I2C in bits 2-3.
const NATIVE_REPLY_MASK: u8 = 0x3;
const NATIVE_REPLY_ACK: u8 = 0x0;
#[cfg_attr(not(test), allow(dead_code))]
const NATIVE_REPLY_NACK: u8 = 0x1;
const NATIVE_REPLY_DEFER: u8 = 0x2;
const I2C_REPLY_MASK: u8 = 0xc;
const I2C_REPLY_ACK: u8 = 0x0;
#[cfg_attr(not(test), allow(dead_code))]
const I2C_REPLY_NACK: u8 = 0x4;
const I2C_REPLY_DEFER: u8 = 0x8;

/// `AUX_RETRY_INTERVAL` (`drm_dp_helper.c:562`), µs.
const RETRY_INTERVAL_US: u32 = 500;

/// DPCD addresses (`drm_dp.h:107,114,116,172,174,1190,1696`).
pub const DPCD_REV: u32 = 0x000;
pub const DP13_DPCD_REV: u32 = 0x2200;
pub const RECEIVER_CAP_SIZE: usize = 0xf;
const TRAINING_AUX_RD_INTERVAL: usize = 0x00e;
/// `drm_dp.h:594`.
const DP_TRAINING_PATTERN_SET: u32 = 0x102;
const EXTENDED_RECEIVER_CAP_FIELD_PRESENT: u8 = 1 << 7;

/// The EDID's I2C address and block size (`include/drm/drm_edid.h:38-39`).
pub const DDC_ADDR: u8 = 0x50;
pub const EDID_LENGTH: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuxError {
    /// The channel did not become idle or did not grant the request
    /// (`auxgm200.c:48-72`, `-EBUSY`).
    Busy(u32),
    /// Status bit 28 clear: nothing on the other end (`auxgm200.c:93-98`).
    NoSink,
    /// The transaction never completed (`auxgm200.c:127-135`).
    Timeout(u32),
    /// The sink did not reply (`auxgm200.c:146`, `-ETIMEDOUT`).
    ReplyTimeout(u32),
    /// Status error bits (`auxgm200.c:148`, `-EIO`).
    Io(u32),
    /// NACK, or a reply code that is neither ACK, NACK nor DEFER.
    Nack(u8),
    /// Still deferred after all retries.
    Defer,
    /// A native read returned fewer bytes than asked (`-EPROTO`).
    Short(u8),
}

/// One completed transaction: the raw reply code and how many bytes the
/// sink returned (`stat & 0x1f`) or were sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reply {
    pub code: u8,
    pub len: usize,
}

/// One AUX channel.
pub struct Aux<'a, M: Mmio + ?Sized> {
    m: &'a M,
    base: u32,
    /// The auto-DPCD register as the first transaction found it, so the
    /// caller can put its bit back ([`Aux::autodpcd_found`]).
    autodpcd: core::cell::Cell<Option<u32>>,
}

impl<'a, M: Mmio + ?Sized> Aux<'a, M> {
    pub fn new(m: &'a M, ch: u8) -> Self {
        Aux { m, base: ch as u32 * 0x50, autodpcd: core::cell::Cell::new(None) }
    }

    fn wdata(&self, i: u32) -> u32 {
        0x00d930 + self.base + i * 4
    }
    fn rdata(&self, i: u32) -> u32 {
        0x00d940 + self.base + i * 4
    }
    fn addr_reg(&self) -> u32 {
        0x00d950 + self.base
    }
    pub fn ctrl_reg(&self) -> u32 {
        0x00d954 + self.base
    }
    pub fn stat_reg(&self) -> u32 {
        0x00d958 + self.base
    }
    /// `gm200_aux_autodpcd` (`i2c/gm200.c:30-34`).
    pub fn autodpcd_reg(&self) -> u32 {
        0x00d968 + self.base
    }

    /// The auto-DPCD register before this channel's first transaction.
    pub fn autodpcd_found(&self) -> Option<u32> {
        self.autodpcd.get()
    }

    /// `gm200_i2c_aux_fini` (`auxgm200.c:32-37`).
    fn fini(&self) {
        self.m.mask(self.ctrl_reg(), 0x0071_0000, 0);
    }

    /// `gm200_i2c_aux_init` (`auxgm200.c:39-73`), `unksel = 1`.
    fn init(&self) -> Result<(), AuxError> {
        const UREQ: u32 = 0x0010_0000;
        const UREP: u32 = 0x0100_0000;
        // "wait up to 1ms for any previous transaction to be done"; nouveau's
        // `if (!timeout--)` allows 1001 reads.
        let mut ctrl = 0;
        for i in 0..=1000 {
            ctrl = self.m.rd32(self.ctrl_reg());
            self.m.udelay(1);
            if ctrl & 0x0701_0000 == 0 {
                break;
            }
            if i == 1000 {
                return Err(AuxError::Busy(ctrl));
            }
        }
        let _ = ctrl;
        // "set some magic, and wait up to 1ms for it to appear"
        self.m.mask(self.ctrl_reg(), 0x0070_0000, UREQ);
        for i in 0..=1000 {
            let ctrl = self.m.rd32(self.ctrl_reg());
            self.m.udelay(1);
            if ctrl & 0x0700_0000 == UREP {
                return Ok(());
            }
            if i == 1000 {
                self.fini();
                return Err(AuxError::Busy(ctrl));
            }
        }
        unreachable!()
    }

    /// Whether a sink answers on this channel: status bit 28, the check
    /// `xfer` makes before each transaction (`auxgm200.c:93-94`). Takes the
    /// channel like `xfer` does, so it has the same side effects (none that
    /// outlive it).
    pub fn sink_present(&self) -> Result<bool, AuxError> {
        if let Err(e) = self.init() {
            self.fini();
            return Err(e);
        }
        let stat = self.m.rd32(self.stat_reg());
        self.fini();
        Ok(stat & 0x1000_0000 != 0)
    }

    /// One transaction (`gm200_i2c_aux_xfer`, `auxgm200.c:75-166`, one
    /// attempt). `buf` is the payload (≤ 16 bytes; empty = address only):
    /// sent for writes, filled for reads.
    pub fn xfer(&self, request: u8, addr: u32, buf: &mut [u8]) -> Result<Reply, AuxError> {
        self.xfer_retry(request, addr, buf, false)
    }

    /// `gm200_i2c_aux_xfer` with its `retry` flag: when set, a DEFER reply
    /// (native or I2C), a reply timeout or an error is retried up to 32 more
    /// times, 400 µs apart, reusing the loaded payload (`auxgm200.c:115-145`).
    /// nvkm's own accesses (`nvkm_rdaux`/`nvkm_wraux`: link training, VBIOS
    /// scripts) set it; DRM's never do.
    pub fn xfer_retry(&self, request: u8, addr: u32, buf: &mut [u8], retry: bool) -> Result<Reply, AuxError> {
        assert!(buf.len() <= 16);
        let size = buf.len() as u32;
        if let Err(e) = self.init() {
            self.fini(); // `goto out` (`auxgm200.c:90-91,163-164`)
            return Err(e);
        }
        let stat = self.m.rd32(self.stat_reg());
        if stat & 0x1000_0000 == 0 {
            self.fini();
            return Err(AuxError::NoSink);
        }
        // Nouveau's wrapper passes `false` whatever it is asked
        // (`i2c/auxch.h:7-11`), before and after the transfer; the caller
        // restores the bit when it is done with the channel.
        let found = self.m.mask(self.autodpcd_reg(), 0x0001_0000, 0);
        if self.autodpcd.get().is_none() {
            self.autodpcd.set(Some(found));
        }

        if request & 1 == 0 {
            let mut x = [0u8; 16];
            x[..buf.len()].copy_from_slice(buf);
            for i in 0..4 {
                let w = u32::from_le_bytes(x[i * 4..i * 4 + 4].try_into().unwrap());
                self.m.wr32(self.wdata(i as u32), w);
            }
        }

        let mut ctrl = self.m.rd32(self.ctrl_reg());
        ctrl &= !0x0001_f1ff;
        ctrl |= (request as u32) << 12;
        ctrl |= if size > 0 { size - 1 } else { 0x0000_0100 };
        self.m.wr32(self.addr_reg(), addr);

        let mut retries = 0;
        let (stat, err) = loop {
            // Reset, delay if a retry, then request (`auxgm200.c:118-125`).
            self.m.wr32(self.ctrl_reg(), 0x8000_0000 | ctrl);
            self.m.wr32(self.ctrl_reg(), ctrl);
            if retries > 0 {
                self.m.udelay(400);
            }
            self.m.wr32(self.ctrl_reg(), 0x0001_0000 | ctrl);
            // "wait up to 2ms for it to complete".
            for i in 0..=2000 {
                ctrl = self.m.rd32(self.ctrl_reg());
                self.m.udelay(1);
                if ctrl & 0x0001_0000 == 0 {
                    break;
                }
                if i == 2000 {
                    self.m.mask(self.autodpcd_reg(), 0x0001_0000, 0);
                    self.fini();
                    return Err(AuxError::Timeout(ctrl));
                }
            }

            // Read and acknowledge the status (`nvkm_mask(.., 0, 0)`).
            let stat = self.m.mask(self.stat_reg(), 0, 0);
            let mut err = None;
            let mut again = matches!(stat & 0x000f_0000, 0x0008_0000 | 0x0002_0000);
            if stat & 0x0000_0100 != 0 {
                err = Some(AuxError::ReplyTimeout(stat));
                again = true;
            }
            if stat & 0x0000_0e00 != 0 {
                err = Some(AuxError::Io(stat));
                again = true;
            }
            // `while (ret && retry && retries++ < 32)`.
            if !(again && retry && retries < 32) {
                break (stat, err);
            }
            retries += 1;
        };
        let mut len = buf.len();
        if request & 1 != 0 {
            let mut x = [0u8; 16];
            for i in 0..4 {
                x[i * 4..i * 4 + 4].copy_from_slice(&self.m.rd32(self.rdata(i as u32)).to_le_bytes());
            }
            buf.copy_from_slice(&x[..buf.len()]);
            len = (stat & 0x1f) as usize;
        }
        self.m.mask(self.autodpcd_reg(), 0x0001_0000, 0);
        self.fini();
        match err {
            Some(e) => Err(e),
            None => Ok(Reply { code: ((stat & 0x000f_0000) >> 16) as u8, len }),
        }
    }

    /// `nvkm_rdaux` / `nvkm_wraux` (`include/nvkm/subdev/i2c.h:157-179`):
    /// one native transaction with the hardware retry on, no DRM policy on
    /// top. nvkm takes any reply code but ACK (0) as a failure; a short
    /// read is only a `WARN_ON` there, an error here.
    pub fn nvkm_read(&self, addr: u32, buf: &mut [u8]) -> Result<(), AuxError> {
        let r = self.xfer_retry(NATIVE_READ, addr, buf, true)?;
        match r.code {
            0 if r.len == buf.len() => Ok(()),
            0 => Err(AuxError::Short(r.len as u8)),
            2 | 8 => Err(AuxError::Defer),
            c => Err(AuxError::Nack(c)),
        }
    }

    pub fn nvkm_write(&self, addr: u32, data: &[u8]) -> Result<(), AuxError> {
        let mut buf = [0u8; 16];
        buf[..data.len()].copy_from_slice(data);
        let r = self.xfer_retry(NATIVE_WRITE, addr, &mut buf[..data.len()], true)?;
        match r.code {
            0 => Ok(()),
            2 | 8 => Err(AuxError::Defer),
            c => Err(AuxError::Nack(c)),
        }
    }

    /// `drm_dp_dpcd_access` for a read (`drm_dp_helper.c:590-647`): up to
    /// 32 attempts, 500 µs apart unless the last one timed out; the error
    /// returned is the first one.
    pub fn native_read(&self, addr: u32, buf: &mut [u8]) -> Result<(), AuxError> {
        self.dpcd_access(NATIVE_READ, addr, buf)
    }

    /// `drm_dp_dpcd_read` (`drm_dp_helper.c:748-767`): DRM first probes
    /// `DP_TRAINING_PATTERN_SET` with a 1-byte read (`drm_dp_dpcd_probe`,
    /// `:667-679`; some sinks wake up on it), then reads. [`Aux::native_read`]
    /// is the access alone (what the phase 2 probe replays).
    pub fn drm_read(&self, addr: u32, buf: &mut [u8]) -> Result<(), AuxError> {
        let mut b = [0u8];
        self.native_read(DP_TRAINING_PATTERN_SET, &mut b)?;
        self.native_read(addr, buf)
    }

    /// `drm_dp_dpcd_access` for a write (same policy); `drm_dp_dpcd_write`
    /// does not probe (`:786-805`).
    pub fn native_write(&self, addr: u32, data: &[u8]) -> Result<(), AuxError> {
        let mut buf = [0u8; 16];
        buf[..data.len()].copy_from_slice(data);
        self.dpcd_access(NATIVE_WRITE, addr, &mut buf[..data.len()])
    }

    fn dpcd_access(&self, request: u8, addr: u32, buf: &mut [u8]) -> Result<(), AuxError> {
        let mut first = None;
        let mut last: Option<AuxError> = None;
        for _ in 0..32 {
            if matches!(last, Some(e) if !matches!(e, AuxError::ReplyTimeout(_))) {
                self.m.udelay(RETRY_INTERVAL_US);
            }
            let e = match self.xfer(request, addr, buf) {
                Ok(r) if r.code & NATIVE_REPLY_MASK == NATIVE_REPLY_ACK => {
                    if r.len == buf.len() {
                        return Ok(());
                    }
                    AuxError::Short(r.len as u8)
                }
                Ok(r) if r.code & NATIVE_REPLY_MASK == NATIVE_REPLY_DEFER => AuxError::Defer,
                Ok(r) => AuxError::Nack(r.code),
                Err(e) => e,
            };
            first.get_or_insert(e);
            last = Some(e);
        }
        Err(first.unwrap())
    }

    /// The receiver capabilities, 15 bytes, with the DP 1.3 extended copy
    /// at 0x2200 when the sink has one (`drm_dp_read_dpcd_caps` and
    /// `drm_dp_read_extended_dpcd_caps`, `drm_dp_helper.c:1188-1259`).
    pub fn read_dpcd_caps(&self) -> Result<[u8; RECEIVER_CAP_SIZE], AuxError> {
        let mut dpcd = [0u8; RECEIVER_CAP_SIZE];
        self.native_read(DPCD_REV, &mut dpcd)?;
        if dpcd[TRAINING_AUX_RD_INTERVAL] & EXTENDED_RECEIVER_CAP_FIELD_PRESENT != 0 {
            let mut ext = [0u8; RECEIVER_CAP_SIZE];
            self.native_read(DP13_DPCD_REV, &mut ext)?;
            if dpcd[0] <= ext[0] {
                dpcd = ext;
            }
        }
        Ok(dpcd)
    }

    /// `drm_dp_i2c_do_msg` (`drm_dp_helper.c:1990-2098`): one I2C-over-AUX
    /// message with DEFER retries (the spec's minimum of 7, plus up to 7
    /// more for I2C defers). Returns the bytes transferred.
    fn i2c_msg(&self, request: u8, addr: u8, buf: &mut [u8]) -> Result<usize, AuxError> {
        let (mut retry, mut defer_i2c) = (0, 0);
        while retry < 7 + defer_i2c {
            retry += 1;
            let r = match self.xfer(request, addr as u32, buf) {
                Ok(r) => r,
                Err(AuxError::Busy(_)) => continue,
                Err(e) => return Err(e),
            };
            match r.code & NATIVE_REPLY_MASK {
                NATIVE_REPLY_ACK => {}
                NATIVE_REPLY_DEFER => {
                    self.m.udelay(RETRY_INTERVAL_US);
                    continue;
                }
                _ => return Err(AuxError::Nack(r.code)),
            }
            match r.code & I2C_REPLY_MASK {
                I2C_REPLY_ACK => return Ok(r.len),
                I2C_REPLY_DEFER => {
                    if defer_i2c < 7 {
                        defer_i2c += 1;
                    }
                    self.m.udelay(RETRY_INTERVAL_US);
                    continue;
                }
                _ => return Err(AuxError::Nack(r.code)),
            }
        }
        Err(AuxError::Defer)
    }

    /// `drm_dp_i2c_xfer` (`drm_dp_helper.c:2147-2225`) for one write of
    /// `wr` then one read into `rd`, both to `addr`, without STOP between
    /// them: a bare address packet opens each message, payloads go in ≤16
    /// byte chunks (smaller if the sink replies short, `drm_dp_i2c_drain_msg`
    /// `:2114-2136`), all with MOT; a bare read without MOT closes.
    pub fn i2c_write_read(&self, addr: u8, wr: &[u8], rd: &mut [u8]) -> Result<(), AuxError> {
        let r = self.i2c_write_read_inner(addr, wr, rd);
        // Close out even after an error (`:2216-2223`); the last message
        // was the read.
        let _ = self.i2c_msg(I2C_READ, addr, &mut []);
        r
    }

    fn i2c_write_read_inner(&self, addr: u8, wr: &[u8], rd: &mut [u8]) -> Result<(), AuxError> {
        self.i2c_msg(I2C_WRITE | I2C_MOT, addr, &mut [])?;
        let mut chunk = [0u8; 16];
        let mut j = 0;
        let mut size = 16;
        while j < wr.len() {
            let n = size.min(wr.len() - j);
            chunk[..n].copy_from_slice(&wr[j..j + n]);
            let got = self.i2c_drain(I2C_WRITE | I2C_MOT, addr, &mut chunk[..n])?;
            size = got;
            j += n;
        }
        self.i2c_msg(I2C_READ | I2C_MOT, addr, &mut [])?;
        let mut j = 0;
        let mut size = 16;
        while j < rd.len() {
            let n = size.min(rd.len() - j);
            let got = self.i2c_drain(I2C_READ | I2C_MOT, addr, &mut rd[j..j + n])?;
            size = got;
            j += n;
        }
        Ok(())
    }

    /// `drm_dp_i2c_drain_msg`: repeats until `buf` is done; returns the
    /// smallest partial size seen (the next chunk size).
    fn i2c_drain(&self, request: u8, addr: u8, buf: &mut [u8]) -> Result<usize, AuxError> {
        let mut ret = buf.len();
        let mut off = 0;
        while off < buf.len() {
            let n = self.i2c_msg(request, addr, &mut buf[off..])?;
            if n == 0 {
                return Err(AuxError::Short(0));
            }
            let n = n.min(buf.len() - off);
            if n < buf.len() - off && n < ret {
                ret = n;
            }
            off += n;
        }
        Ok(ret)
    }

    /// One 128-byte EDID block over I2C-over-AUX: offset `block * 128`,
    /// then 128 bytes — the two messages DRM's EDID reader sends
    /// (`trace-nogsp`, AUX channel 3, transactions 15-38). Blocks 0 and 1
    /// only: higher ones need the E-DDC segment pointer, which the trace
    /// never uses.
    pub fn read_edid_block(&self, block: u8, out: &mut [u8; EDID_LENGTH]) -> Result<(), AuxError> {
        assert!(block < 2);
        self.i2c_write_read(DDC_ADDR, &[block * EDID_LENGTH as u8], out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmio::testing::{ReplayMmio, TableMmio};

    const EXTRACT: &str = include_str!("../fixtures/aux-ch3-dpcd-edid.txt");
    const ASUS: &[u8] = include_bytes!("../fixtures/edid-asus-vg279q3a.bin");

    /// Replays what nouveau + DRM did on channel 3 (the ASUS on DP):
    /// DPCD caps (with a real DEFER on 0x2200), then EDID blocks 0 and 1.
    /// The EDID must come out byte-identical to sysfs's, every read the
    /// trace made must be consumed, and the data/address/control writes
    /// must be the trace's, in order.
    #[test]
    fn replay_dpcd_and_edid_from_the_trace() {
        let m = ReplayMmio::from_extract(EXTRACT);
        let aux = Aux::new(&m, 3);

        let dpcd = aux.read_dpcd_caps().unwrap();
        // 0x2200 says DPCD 1.4; HBR2 (0x14); 4 lanes + enhanced framing.
        assert_eq!(&dpcd[..3], &[0x14, 0x14, 0xc4]);

        let mut edid = [0u8; 256];
        let (b0, b1) = edid.split_at_mut(128);
        aux.read_edid_block(0, b0.try_into().unwrap()).unwrap();
        aux.read_edid_block(1, b1.try_into().unwrap()).unwrap();
        assert_eq!(&edid[..], ASUS);

        for reg in [0xda44, 0xda48, 0xda58, 0xda30, 0xda34, 0xda38, 0xda3c] {
            assert_eq!(m.unread(reg), 0, "reads of {reg:#x} left over");
        }
        let (ours, trace) = m.write_diff(&[0xda20, 0xda24, 0xda28, 0xda2c, 0xda40, 0xda44, 0xda48, 0xda58]);
        assert_eq!(ours, trace);
    }

    /// Channel 0 that grants every request and completes every
    /// transaction at once, with a fixed status.
    struct AuxSim {
        ctrl: core::cell::Cell<u32>,
        stat: u32,
        writes: core::cell::RefCell<alloc::vec::Vec<(u32, u32)>>,
    }
    impl AuxSim {
        fn new(stat: u32) -> Self {
            AuxSim { ctrl: core::cell::Cell::new(0x9000), stat, writes: Default::default() }
        }
        fn tries(&self) -> usize {
            self.writes.borrow().iter().filter(|(o, v)| *o == 0xd954 && v & 0x0001_0000 != 0).count()
        }
    }
    impl Mmio for AuxSim {
        fn rd32(&self, o: u32) -> u32 {
            match o {
                0xd954 => self.ctrl.get(),
                0xd958 => self.stat,
                _ => 0,
            }
        }
        fn wr32(&self, o: u32, v: u32) {
            self.writes.borrow_mut().push((o, v));
            if o == 0xd954 {
                // Request bit → granted; pending bit → done at once.
                let g = if v & 0x0070_0000 != 0 { 0x0100_0000 } else { 0 };
                self.ctrl.set((v & !0x0701_0000) | g);
            }
        }
        fn udelay(&self, _: u32) {}
    }

    #[test]
    fn no_sink_is_reported_and_releases_the_channel() {
        let m = AuxSim::new(0);
        let aux = Aux::new(&m, 0);
        assert_eq!(aux.xfer(NATIVE_READ, 0, &mut [0; 1]), Err(AuxError::NoSink));
        // The last write is the fini (request bits cleared).
        assert_eq!(m.writes.borrow().last().map(|w| (w.0, w.1 & 0x0071_0000)), Some((0xd954, 0)));
        assert_eq!(aux.sink_present(), Ok(false));
        assert_eq!(Aux::new(&AuxSim::new(0x1000_0000), 0).sink_present(), Ok(true));
    }

    #[test]
    fn busy_and_timeouts_are_bounded() {
        // Never idle.
        let m = TableMmio::new(&[(0xd954, 0x0001_0000)]);
        assert!(matches!(Aux::new(&m, 0).xfer(NATIVE_READ, 0, &mut [0; 1]), Err(AuxError::Busy(_))));
        assert!(m.reads.borrow().len() <= 1001 + 2);
        // Idle but the grant never appears.
        let m = TableMmio::new(&[(0xd954, 0)]);
        assert!(matches!(Aux::new(&m, 0).xfer(NATIVE_READ, 0, &mut [0; 1]), Err(AuxError::Busy(_))));
        // Idle, granted, sink present, but the transaction never
        // completes: the pending bit (16) stays set.
        let mut m = ReplayMmio::from_extract("R 0xd954 0x9000\nR 0xd954 0x9000\nR 0xd954 0x1109000\nR 0xd954 0x1109000\nR 0xd954 0x1119000\n");
        m.fallback = alloc::vec![(0xd958, 0x1000_0000)];
        assert_eq!(Aux::new(&m, 0).xfer(NATIVE_READ, 0, &mut [0; 1]), Err(AuxError::Timeout(0x0111_9000)));
        // ... and still released: autodpcd and fini written last.
        let w = m.writes.borrow();
        assert_eq!(w[w.len() - 2].0, 0xd968);
        assert_eq!(w[w.len() - 1], (0xd954, 0x0111_9000 & !0x0071_0000));
    }

    #[test]
    fn nack_and_defer_policy() {
        // A native read that is always deferred gives up after 32 tries.
        let m = AuxSim::new(0x1002_0000);
        assert_eq!(Aux::new(&m, 0).native_read(0, &mut [0; 1]), Err(AuxError::Defer));
        assert_eq!(m.tries(), 32);
        // An ACK with fewer bytes than asked is a short read (retried too).
        let m = AuxSim::new(0x1000_0001);
        assert_eq!(Aux::new(&m, 0).native_read(0, &mut [0; 2]), Err(AuxError::Short(1)));
        // An I2C NACK ends at once.
        let m = AuxSim::new(0x1000_0000 | (I2C_REPLY_NACK as u32) << 16);
        assert_eq!(Aux::new(&m, 0).i2c_msg(I2C_READ, 0x50, &mut [0; 1]), Err(AuxError::Nack(4)));
        assert_eq!(m.tries(), 1);
        // A native NACK on I2C too.
        let m = AuxSim::new(0x1000_0000 | (NATIVE_REPLY_NACK as u32) << 16);
        assert_eq!(Aux::new(&m, 0).i2c_msg(I2C_READ, 0x50, &mut [0; 1]), Err(AuxError::Nack(1)));
        // I2C DEFER forever: 7 + 7 attempts.
        let m = AuxSim::new(0x1008_0000);
        assert_eq!(Aux::new(&m, 0).i2c_msg(I2C_READ, 0x50, &mut [0; 1]), Err(AuxError::Defer));
        assert_eq!(m.tries(), 14);
        // A read ACKed with zero bytes never loops.
        let m = AuxSim::new(0x1000_0000);
        assert_eq!(Aux::new(&m, 0).i2c_drain(I2C_READ, 0x50, &mut [0; 4]), Err(AuxError::Short(0)));
        // The EDID read closes the I2C transaction even after a NACK.
        let m = AuxSim::new(0x1004_0000);
        assert_eq!(Aux::new(&m, 0).read_edid_block(0, &mut [0; 128]), Err(AuxError::Nack(4)));
        let last_req = m.writes.borrow().iter().rev().find(|(o, v)| *o == 0xd954 && v & 0x0001_0000 != 0).unwrap().1;
        assert_eq!((last_req >> 12) & 0xf, I2C_READ as u32);
        assert_eq!(last_req & 0x100, 0x100, "address only");
    }

    /// nvkm's accesses (`retry = true`): the hardware loop re-sends a
    /// deferred transaction 32 more times without re-arming the channel;
    /// DRM's (`retry = false`) send it once per call.
    #[test]
    fn nvkm_retry_resends_deferred_transactions() {
        let m = AuxSim::new(0x1002_0000);
        assert_eq!(Aux::new(&m, 0).nvkm_read(0x202, &mut [0; 3]), Err(AuxError::Defer));
        assert_eq!(m.tries(), 33);
        // Each one reset first (bit 31), as in `auxgm200.c:118-119`.
        assert_eq!(m.writes.borrow().iter().filter(|(o, v)| *o == 0xd954 && v & 0x8000_0000 != 0).count(), 33);
        let m = AuxSim::new(0x1002_0000);
        assert_eq!(Aux::new(&m, 0).xfer(NATIVE_READ, 0x202, &mut [0; 3]).map(|r| r.code), Ok(2));
        assert_eq!(m.tries(), 1);
        // A NACK is not retried by the hardware loop, and is a failure.
        let m = AuxSim::new(0x1001_0000);
        assert_eq!(Aux::new(&m, 0).nvkm_write(0x103, &[0; 4]), Err(AuxError::Nack(1)));
        assert_eq!(m.tries(), 1);
        // An ACK completes a write.
        let m = AuxSim::new(0x1000_0000);
        assert_eq!(Aux::new(&m, 0).nvkm_write(0x103, &[1, 2, 3, 4]), Ok(()));
        assert_eq!(m.writes.borrow().iter().find(|(o, _)| *o == 0xd930), Some(&(0xd930, 0x0403_0201)));
    }
}
