//! Falcon microcontrollers as the GA10x GSP has them (phase 4a): reset, DMA
//! loading of IMEM/DMEM from host memory, boot and mailboxes. Pure, over
//! [`Mmio`]; polls are bounded by microseconds of [`Mmio::udelay`] and
//! nothing blocks or logs.
//!
//! Scope: the *DMA* flavour, which is what `ga102_gsp_flcn` uses for both
//! the GSP and (later) SEC2 on the GA106 (`nvkm/subdev/gsp/ga102.c:136-150`).
//! The PIO loaders of older chips (`gm200_flcn_*_pio`) are not ported: the
//! target has no use for them. Paths are relative to
//! `drivers/gpu/drm/nouveau/nvkm/falcon/` in Linux v7.2.2.
//!
//! Oracle: `fixtures/fwsec-frts.txt`, nouveau's FWSEC-FRTS run on the GSP
//! falcon (`trace-gsp`, 8,340 to 8,576 s), replayed in the tests below.

use crate::Mmio;

/// A falcon: its BAR0 base and the offset of the second register bank
/// (`nvkm_falcon.addr`, `.addr2`). The GSP is at `0x110000` with the bank at
/// `+0x1000` (`ga102.c:141`; the base is what nouveau finds in the TOP table,
/// measured in `trace-gsp`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Falcon {
    pub base: u32,
    pub addr2: u32,
}

pub const GSP: Falcon = Falcon { base: 0x11_0000, addr2: 0x1000 };

// Register offsets, relative to the falcon's base.
const INTR_CLEAR: u32 = 0x014; // gm200.c:mask in disable `wr32(0x014, ~0)`
const MAILBOX0: u32 = 0x040; // gm200.c fw_boot
const MAILBOX1: u32 = 0x044;
const IRQMASK_CTRL: u32 = 0x048; // gm200.c disable: mask 0x048 bits 1:0
const CHIP_ID: u32 = 0x084; // gm200.c enable: PMC_BOOT_0 copy
const CPUCTL: u32 = 0x100; // gm200.c fw_boot: 0x2 = start, bit 4 = halted
const BOOTVEC: u32 = 0x104;
const HWCFG: u32 = 0x108; // base.c oneinit
const DMACTL: u32 = 0x10c;
const DMATRFBASE: u32 = 0x110; // ga102.c dma_init
const DMATRFMOFFS: u32 = 0x114; // ga102.c dma_xfer
const DMATRFCMD: u32 = 0x118;
const DMATRFFBOFFS: u32 = 0x11c;
const DMATRFBASE1: u32 = 0x128;
const HWCFG1: u32 = 0x12c; // base.c oneinit
const SCRUB: u32 = 0x0f4; // ga102.c reset_prep / wait_mem_scrubbing
const ENGINE_RESET: u32 = 0x3c0; // gp102.c reset_eng
const DMAIDX: u32 = 0x600; // ga102_flcn_fw_load
const DMACTL2: u32 = 0x624;
const SELECT: u32 = 0x668; // + addr2, ga102.c select
const RISCV_RESET: u32 = 0x1668; // ga102_gsp_reset, bits 8, 4, 0
// Offsets from `addr2` used when booting a signed image (`ga102.c:130-138`).
const SIGN_DMEM: u32 = 0x210;
const SIGN_ENGINE: u32 = 0x19c;
const SIGN_UCODE: u32 = 0x198;
const SIGN_GO: u32 = 0x180;

/// The device's chip-id register, copied into every falcon on enable
/// (`gm200.c:enable`, `nvkm_rd32(device, 0x000000)`).
const PMC_BOOT_0: u32 = 0x00_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FalconError {
    /// A bounded wait ended (`-ETIMEDOUT` in nouveau).
    Timeout,
    /// The DMA length must be a non-zero multiple of 256 (`base.c:dma_wr`).
    BadLength,
    /// The falcon halted, but its mailbox 0 is not the expected value.
    Mailbox { mbox0: u32, mbox1: u32 },
    /// The falcon never halted within the time limit.
    BootTimeout { mbox0: u32, mbox1: u32 },
}

/// Which falcon memory a DMA transfer targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mem {
    Imem,
    Dmem,
}

/// What `nvkm_falcon_oneinit` reads (`base.c:206-232`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Info {
    pub version: u32,
    /// Security level (`falcon->secret`).
    pub secret: u32,
    pub code_ports: u32,
    pub data_ports: u32,
    /// IMEM and DMEM sizes in bytes.
    pub code_limit: u32,
    pub data_limit: u32,
}

/// The parts of `struct nvkm_falcon_fw` a DMA boot uses (`ga102.c:130-142`).
/// Offsets `*_base_img` are into the image the DMA reads; `*_base` are
/// addresses in the falcon's memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LoadParams {
    pub imem_base_img: u32,
    pub imem_base: u32,
    pub imem_size: u32,
    pub dmem_base_img: u32,
    pub dmem_base: u32,
    pub dmem_size: u32,
    /// Where the signature was patched in the DMEM image (`fw->dmem_sign`).
    pub dmem_sign: u32,
    pub boot_addr: u32,
    pub engine_id: u32,
    pub ucode_id: u32,
}

/// Microseconds, the time base of every bound below (nouveau's
/// `nvkm_msec(dev, N, ...)` is `N * 1000`).
const MS: u32 = 1000;

impl Falcon {
    pub(crate) fn rd(&self, m: &impl Mmio, r: u32) -> u32 {
        m.rd32(self.base + r)
    }
    pub(crate) fn wr(&self, m: &impl Mmio, r: u32, v: u32) {
        m.wr32(self.base + r, v)
    }
    fn mask(&self, m: &impl Mmio, r: u32, mask: u32, v: u32) -> u32 {
        m.mask(self.base + r, mask, v)
    }

    /// Poll `r` until `done(value)`, at most `us` microseconds (one read per
    /// microsecond). nouveau's `nvkm_msec` returns the elapsed time, and a
    /// value that satisfies the condition on the last try still counts.
    fn poll(&self, m: &impl Mmio, r: u32, us: u32, done: impl Fn(u32) -> bool) -> bool {
        let mut left = us;
        loop {
            if done(self.rd(m, r)) {
                return true;
            }
            if left == 0 {
                return false;
            }
            m.udelay(1);
            left -= 1;
        }
    }

    /// `nvkm_falcon_oneinit` (`base.c:206-232`), reads only.
    pub fn probe(&self, m: &impl Mmio) -> Info {
        let cfg1 = self.rd(m, HWCFG1);
        let cfg = self.rd(m, HWCFG);
        Info {
            version: cfg1 & 0xf,
            secret: (cfg1 >> 4) & 0x3,
            code_ports: (cfg1 >> 8) & 0xf,
            data_ports: (cfg1 >> 12) & 0xf,
            code_limit: (cfg & 0x1ff) << 8,
            data_limit: (cfg & 0x3fe00) >> 1,
        }
    }

    /// `ga102_flcn_select` (`ga102.c:96-109`): leave the RISC-V core's
    /// selection so the falcon core answers (bit 4 of `addr2+0x668`).
    pub fn select(&self, m: &impl Mmio) -> Result<(), FalconError> {
        let r = self.addr2 + SELECT;
        if self.rd(m, r) & 0x10 != 0 {
            self.wr(m, r, 0);
            if !self.poll(m, r, 10 * MS, |v| v & 1 != 0) {
                return Err(FalconError::Timeout);
            }
        }
        Ok(())
    }

    /// `ga102_flcn_reset_prep` (`ga102.c:88-99`): one read, then up to 150 us
    /// for bit 31. Running out of time is not an error (`_warn = false`).
    fn reset_prep(&self, m: &impl Mmio) {
        self.rd(m, SCRUB);
        self.poll(m, SCRUB, 150, |v| v & 0x8000_0000 != 0);
    }

    /// `ga102_flcn_reset_wait_mem_scrubbing` (`ga102.c:73-85`): a no-op mask
    /// write of the mailbox, then up to 20 ms for the memory scrub (bit 12)
    /// to end.
    fn wait_mem_scrubbing(&self, m: &impl Mmio) -> Result<(), FalconError> {
        self.mask(m, MAILBOX0, 0, 0);
        if self.poll(m, SCRUB, 20 * MS, |v| v & 0x1000 == 0) {
            Ok(())
        } else {
            Err(FalconError::Timeout)
        }
    }

    /// `gp102_flcn_reset_eng` (`gp102.c:65-79`): pulse the engine reset.
    fn reset_eng(&self, m: &impl Mmio) -> Result<(), FalconError> {
        self.reset_prep(m);
        self.mask(m, ENGINE_RESET, 1, 1);
        m.udelay(10);
        self.mask(m, ENGINE_RESET, 1, 0);
        self.wait_mem_scrubbing(m)
    }

    /// `gm200_flcn_disable` (`gm200.c`, no PMC reset on the GA10x GSP).
    pub fn disable(&self, m: &impl Mmio) -> Result<(), FalconError> {
        self.select(m)?;
        self.mask(m, IRQMASK_CTRL, 3, 0);
        self.wr(m, INTR_CLEAR, 0xffff_ffff);
        self.reset_eng(m)
    }

    /// `gm200_flcn_enable`: reset, select, wait for the scrub, publish the
    /// chip id.
    pub fn enable(&self, m: &impl Mmio) -> Result<(), FalconError> {
        self.reset_eng(m)?;
        self.select(m)?;
        self.wait_mem_scrubbing(m)?;
        self.wr(m, CHIP_ID, m.rd32(PMC_BOOT_0));
        Ok(())
    }

    /// `nvkm_falcon_reset` (`base.c`): disable, then enable.
    pub fn reset(&self, m: &impl Mmio) -> Result<(), FalconError> {
        self.disable(m)?;
        self.enable(m)
    }

    /// `ga102_gsp_reset` (`subdev/gsp/ga102.c:28-39`): engine reset, then
    /// bits 8, 4 and 0 of `0x1668`, which leaves the GSP in RISC-V mode.
    pub fn gsp_reset(&self, m: &impl Mmio) -> Result<(), FalconError> {
        self.reset_eng(m)?;
        self.mask(m, RISCV_RESET, 0x111, 0x111);
        Ok(())
    }

    /// `nvkm_falcon_dma_wr` with `ga102_flcn_dma` (`base.c:50-109`,
    /// `ga102.c:31-65`): copy `len` bytes, 256 at a time, from host memory at
    /// `dma_addr` (the image's bus address) to the falcon's `mem` at
    /// `mem_base`. `dma_base` is the offset in the image where the data
    /// starts; the DMEM path moves the base address itself and transfers
    /// from offset 0.
    pub fn dma_wr(
        &self,
        m: &impl Mmio,
        dma_addr: u64,
        dma_base: u32,
        mem: Mem,
        mem_base: u32,
        len: u32,
        sec: bool,
    ) -> Result<(), FalconError> {
        const XFER: u32 = 256;
        if len == 0 || len % XFER != 0 {
            return Err(FalconError::BadLength);
        }
        let (dma_addr, dma_start) = match mem {
            Mem::Dmem => (dma_addr + dma_base as u64, dma_base),
            Mem::Imem => (dma_addr, 0),
        };
        // ga102_flcn_dma_init
        let mut cmd = (XFER.ilog2() - 2) << 8;
        if mem == Mem::Imem {
            cmd |= 0x10;
        }
        if sec {
            cmd |= 0x4;
        }
        self.wr(m, DMATRFBASE, (dma_addr >> 8) as u32);
        self.wr(m, DMATRFBASE1, 0);

        let (mut dst, mut src, mut left) = (mem_base, dma_base, len);
        while left >= XFER {
            // ga102_flcn_dma_xfer, then wait for `done` (bit 1, 2 s).
            self.wr(m, DMATRFMOFFS, dst);
            self.wr(m, DMATRFFBOFFS, src - dma_start);
            self.wr(m, DMATRFCMD, cmd);
            if !self.poll(m, DMATRFCMD, 2000 * MS, |v| v & 2 != 0) {
                return Err(FalconError::Timeout);
            }
            src += XFER;
            dst += XFER;
            left -= XFER;
        }
        Ok(())
    }

    /// `ga102_flcn_fw_load` (`ga102.c:117-141`): DMA context, then IMEM
    /// (secure) and DMEM from the image at `dma_addr`.
    pub fn load(&self, m: &impl Mmio, dma_addr: u64, p: &LoadParams) -> Result<(), FalconError> {
        self.mask(m, DMACTL2, 0x80, 0x80);
        self.wr(m, DMACTL, 0);
        self.mask(m, DMAIDX, 0x0001_0007, (1 << 2) | 1);
        self.dma_wr(m, dma_addr, p.imem_base_img, Mem::Imem, p.imem_base, p.imem_size, true)?;
        self.dma_wr(m, dma_addr, p.dmem_base_img, Mem::Dmem, p.dmem_base, p.dmem_size, false)
    }

    /// `ga102_flcn_fw_boot` + `gm200_flcn_fw_boot` (`ga102.c:130-138`,
    /// `gm200.c:fw_boot`): tell the boot ROM where the signature is and which
    /// engine and ucode it is for, write the mailboxes, start, and wait for
    /// the falcon to halt (2 s). Returns the mailboxes; `mbox0` must be
    /// `mbox0_ok`. `mbox0`/`mbox1` are the values the image is started with
    /// (`None` for mailbox 1 leaves it alone; mailbox 0 defaults to
    /// `0xcafebeef` when `None`).
    pub fn boot(
        &self,
        m: &impl Mmio,
        p: &LoadParams,
        mbox0: Option<u32>,
        mbox1: Option<u32>,
        mbox0_ok: u32,
        irqsclr: u32,
    ) -> Result<(u32, u32), FalconError> {
        let a2 = self.addr2;
        self.wr(m, a2 + SIGN_DMEM, p.dmem_sign);
        self.wr(m, a2 + SIGN_ENGINE, p.engine_id);
        self.wr(m, a2 + SIGN_UCODE, p.ucode_id);
        self.wr(m, a2 + SIGN_GO, 1);

        self.wr(m, MAILBOX0, mbox0.unwrap_or(0xcafe_beef));
        if let Some(v) = mbox1 {
            self.wr(m, MAILBOX1, v);
        }
        self.wr(m, BOOTVEC, p.boot_addr);
        self.wr(m, CPUCTL, 2);
        let halted = self.poll(m, CPUCTL, 2000 * MS, |v| v & 0x10 != 0);
        let (r0, r1) = (self.rd(m, MAILBOX0), self.rd(m, MAILBOX1));
        if irqsclr != 0 {
            self.mask(m, 0x004, 0xffff_ffff, irqsclr);
        }
        if !halted {
            return Err(FalconError::BootTimeout { mbox0: r0, mbox1: r1 });
        }
        if r0 != mbox0_ok {
            return Err(FalconError::Mailbox { mbox0: r0, mbox1: r1 });
        }
        Ok((r0, r1))
    }

    /// `ga102_flcn_riscv_active` (`ga102.c:28-31`): the RISC-V core runs
    /// (bit 7 of `addr2 + 0x388`).
    pub fn riscv_active(&self, m: &impl Mmio) -> bool {
        self.rd(m, self.addr2 + 0x388) & 0x80 != 0
    }

    /// Write mailboxes 0 and 1 (`nvkm_falcon_wr32(0x040/0x044)`): the boot
    /// arguments of the next stage (`tu102.c:412-413` puts the LibOS
    /// address there).
    pub fn set_mailboxes(&self, m: &impl Mmio, mbox0: u32, mbox1: u32) {
        self.wr(m, MAILBOX0, mbox0);
        self.wr(m, MAILBOX1, mbox1);
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::mmio::testing::ReplayMmio;
    use alloc::vec::Vec;
    use core::cell::{Cell, RefCell};

    /// A small model of a GA10x falcon: the registers the sequences poll
    /// behave as in the trace (scrub bit 12 stays for `scrub_reads` reads
    /// after an engine reset, bit 31 of `0xf4` after `prep_reads`, the DMA
    /// says done after `dma_reads` reads of the command register, the CPU
    /// halts `halt_reads` reads after the start). Everything else is a plain
    /// register file. `never_*` keeps a condition from ever being met.
    pub struct Sim {
        pub base: u32,
        pub regs: RefCell<Vec<(u32, u32)>>,
        pub writes: RefCell<Vec<(u32, u32)>>,
        pub reads_of: RefCell<Vec<u32>>,
        pub scrub_reads: Cell<u32>,
        pub scrub_left: Cell<u32>,
        pub prep_reads: u32,
        pub prep_seen: Cell<u32>,
        pub dma_reads: u32,
        pub dma_seen: Cell<u32>,
        pub halt_reads: u32,
        pub halt_seen: Cell<Option<u32>>,
        pub never_scrubbed: bool,
        pub never_dma: bool,
        pub never_halt: bool,
        pub select_bit: Cell<bool>,
        pub select_never_acks: bool,
        /// (writes so far, microseconds) of every udelay.
        pub delays: RefCell<Vec<(usize, u32)>>,
    }

    impl Sim {
        pub fn new(base: u32) -> Self {
            Sim {
                base,
                regs: RefCell::new(Vec::new()),
                writes: RefCell::new(Vec::new()),
                reads_of: RefCell::new(Vec::new()),
                scrub_reads: Cell::new(3),
                scrub_left: Cell::new(0),
                prep_reads: 5,
                prep_seen: Cell::new(0),
                dma_reads: 1,
                dma_seen: Cell::new(0),
                halt_reads: 4,
                halt_seen: Cell::new(None),
                never_scrubbed: false,
                never_dma: false,
                never_halt: false,
                select_bit: Cell::new(false),
                select_never_acks: false,
                delays: RefCell::new(Vec::new()),
            }
        }
        fn get(&self, o: u32) -> u32 {
            self.regs.borrow().iter().rev().find(|(r, _)| *r == o).map_or(0, |(_, v)| *v)
        }
        pub fn set(&self, o: u32, v: u32) {
            self.regs.borrow_mut().push((o, v));
        }
        pub fn count_reads(&self, o: u32) -> usize {
            self.reads_of.borrow().iter().filter(|r| **r == o).count()
        }
    }

    impl Mmio for Sim {
        fn rd32(&self, o: u32) -> u32 {
            self.reads_of.borrow_mut().push(o);
            let r = o.wrapping_sub(self.base);
            match r {
                0x0f4 => {
                    if self.never_scrubbed {
                        return 0x77f7;
                    }
                    let seen = self.prep_seen.get();
                    self.prep_seen.set(seen + 1);
                    let mut v = if seen >= self.prep_reads { 0x8000_47f7 } else { 0x47f7 };
                    if self.scrub_left.get() > 0 {
                        self.scrub_left.set(self.scrub_left.get() - 1);
                        v |= 0x1000;
                    }
                    v
                }
                0x118 => {
                    if self.never_dma {
                        return 0x600;
                    }
                    let n = self.dma_seen.get() + 1;
                    self.dma_seen.set(n);
                    if n > self.dma_reads {
                        0x602
                    } else {
                        0x600
                    }
                }
                0x100 => {
                    if self.never_halt {
                        return 0;
                    }
                    match self.halt_seen.get() {
                        None => 0,
                        Some(n) => {
                            self.halt_seen.set(Some(n + 1));
                            if n >= self.halt_reads {
                                0x10
                            } else {
                                0
                            }
                        }
                    }
                }
                x if x == 0x1000 + 0x668 => {
                    let mut v = self.get(o);
                    if self.select_bit.get() {
                        v |= 0x10;
                        if !self.select_never_acks {
                            // the write of 0 was seen: bit 0 is set
                        }
                    }
                    v
                }
                _ => self.get(o),
            }
        }
        fn wr32(&self, o: u32, v: u32) {
            self.writes.borrow_mut().push((o, v));
            let r = o.wrapping_sub(self.base);
            match r {
                0x3c0 => {
                    if v & 1 == 0 {
                        self.scrub_left.set(self.scrub_reads.get());
                        self.prep_seen.set(0);
                    }
                }
                0x118 => self.dma_seen.set(0),
                0x100 if v == 2 => self.halt_seen.set(Some(0)),
                x if x == 0x1000 + 0x668 => {
                    self.select_bit.set(false);
                    self.set(o, if self.select_never_acks { 0 } else { 1 });
                    return;
                }
                _ => {}
            }
            self.set(o, v);
        }
        fn udelay(&self, us: u32) {
            self.delays.borrow_mut().push((self.writes.borrow().len(), us));
        }
    }

    fn wr_at(s: &Sim, r: u32) -> Vec<u32> {
        s.writes.borrow().iter().filter(|(o, _)| *o == s.base + r).map(|(_, v)| *v).collect()
    }

    #[test]
    fn probe_decodes_what_the_trace_read() {
        // trace-gsp: 0x11012c = 0xca101136, 0x110108 = 0x80420100.
        let s = Sim::new(GSP.base);
        s.set(GSP.base + 0x12c, 0xca10_1136);
        s.set(GSP.base + 0x108, 0x8042_0100);
        let i = GSP.probe(&s);
        assert_eq!(
            i,
            Info { version: 6, secret: 3, code_ports: 1, data_ports: 1, code_limit: 0x1_0000, data_limit: 0x1_0000 }
        );
    }

    #[test]
    fn select_only_acts_when_the_riscv_bit_is_set() {
        let s = Sim::new(GSP.base);
        GSP.select(&s).unwrap();
        assert!(s.writes.borrow().is_empty(), "bit 4 clear: nothing written");
        s.select_bit.set(true);
        GSP.select(&s).unwrap();
        assert_eq!(*s.writes.borrow(), [(GSP.base + 0x1668, 0)]);
    }

    #[test]
    fn select_times_out_if_bit0_never_comes() {
        let mut s = Sim::new(GSP.base);
        s.select_never_acks = true;
        s.select_bit.set(true);
        assert_eq!(GSP.select(&s), Err(FalconError::Timeout));
        // bounded: 10 ms of one-microsecond polls (+ the first read + the check)
        assert!(s.count_reads(GSP.base + 0x1668) <= 10_003);
    }

    #[test]
    fn reset_eng_waits_for_the_scrub_and_pulses_the_reset() {
        let s = Sim::new(GSP.base);
        s.scrub_reads.set(7);
        GSP.reset_eng(&s).unwrap();
        assert_eq!(wr_at(&s, 0x3c0), [1, 0]);
        assert_eq!(wr_at(&s, 0x040), [0], "the mask of mailbox 0 writes it back");
        // scrub finished only after 7 reads with bit 12
        assert!(s.count_reads(GSP.base + 0x0f4) >= 1 + 1 + 8, "prep read + poll + scrub polls");
    }

    #[test]
    fn scrub_that_never_ends_is_an_error_within_the_bound() {
        let mut s = Sim::new(GSP.base);
        s.never_scrubbed = true;
        assert_eq!(GSP.reset_eng(&s), Err(FalconError::Timeout));
        let n = s.count_reads(GSP.base + 0x0f4);
        // prep: 1 read + 151 polls (150 us); scrub: 20 001 polls (20 ms)
        assert_eq!(n, 1 + 151 + 20_001);
    }

    #[test]
    fn prep_stops_polling_when_bit_31_comes() {
        let s = Sim::new(GSP.base);
        s.scrub_reads.set(0);
        GSP.reset_eng(&s).unwrap();
        // 1 discarded read + 5 polls (4 without bit 31, then the one with it) + 1 scrub poll.
        assert_eq!(s.count_reads(GSP.base + 0x0f4), 1 + 5 + 1);
    }

    #[test]
    fn prep_running_out_of_time_is_not_an_error() {
        let mut s = Sim::new(GSP.base);
        s.prep_reads = u32::MAX; // bit 31 never
        GSP.reset_eng(&s).unwrap();
    }

    #[test]
    fn enable_publishes_the_chip_id() {
        let s = Sim::new(GSP.base);
        s.set(0, 0xb760_00a1);
        GSP.enable(&s).unwrap();
        assert_eq!(wr_at(&s, 0x084), [0xb760_00a1]);
    }

    #[test]
    fn disable_masks_irqs_clears_them_and_resets() {
        let s = Sim::new(GSP.base);
        s.set(GSP.base + 0x048, 0xff);
        GSP.disable(&s).unwrap();
        assert_eq!(wr_at(&s, 0x048), [0xfc]);
        assert_eq!(wr_at(&s, 0x014), [0xffff_ffff]);
        assert_eq!(wr_at(&s, 0x3c0), [1, 0]);
    }

    #[test]
    fn gsp_reset_sets_the_riscv_bits_only_after_the_reset() {
        let s = Sim::new(GSP.base);
        s.set(GSP.base + 0x1668, 0x2);
        GSP.gsp_reset(&s).unwrap();
        let w = s.writes.borrow();
        let last = w.last().unwrap();
        assert_eq!(*last, (GSP.base + 0x1668, 0x113));
        assert!(w.iter().position(|x| x.0 == GSP.base + 0x3c0).unwrap() < w.len() - 1);
    }

    #[test]
    fn dma_command_words_and_addresses() {
        let s = Sim::new(GSP.base);
        // IMEM, secure: 3 transfers from image offset 0x200, into 0x100.
        GSP.dma_wr(&s, 0x1_2345_6700, 0x200, Mem::Imem, 0x100, 0x300, true).unwrap();
        assert_eq!(wr_at(&s, 0x110), [(0x1_2345_6700u64 >> 8) as u32]);
        assert_eq!(wr_at(&s, 0x128), [0]);
        assert_eq!(wr_at(&s, 0x114), [0x100, 0x200, 0x300]);
        assert_eq!(wr_at(&s, 0x11c), [0x200, 0x300, 0x400]);
        assert_eq!(wr_at(&s, 0x118), [0x614, 0x614, 0x614]);
        // DMEM, not secure: the base moves by the image offset, transfers start at 0.
        let s = Sim::new(GSP.base);
        GSP.dma_wr(&s, 0x1000, 0x500, Mem::Dmem, 0, 0x200, false).unwrap();
        assert_eq!(wr_at(&s, 0x110), [0x15]);
        assert_eq!(wr_at(&s, 0x114), [0, 0x100]);
        assert_eq!(wr_at(&s, 0x11c), [0, 0x100]);
        assert_eq!(wr_at(&s, 0x118), [0x600, 0x600]);
    }

    #[test]
    fn dma_refuses_bad_lengths_and_writes_nothing() {
        let s = Sim::new(GSP.base);
        for len in [0u32, 1, 255, 257, 0x1ff] {
            assert_eq!(GSP.dma_wr(&s, 0, 0, Mem::Imem, 0, len, false), Err(FalconError::BadLength));
        }
        assert!(s.writes.borrow().is_empty());
    }

    #[test]
    fn dma_that_never_completes_times_out_after_2s() {
        let mut s = Sim::new(GSP.base);
        s.never_dma = true;
        assert_eq!(GSP.dma_wr(&s, 0, 0, Mem::Imem, 0, 0x100, false), Err(FalconError::Timeout));
        assert_eq!(wr_at(&s, 0x114).len(), 1, "stops at the first transfer");
        let n = s.count_reads(GSP.base + 0x118);
        assert!((2_000_000..=2_000_002).contains(&n), "{n}");
    }

    fn params() -> LoadParams {
        LoadParams {
            imem_base_img: 0,
            imem_base: 0,
            imem_size: 0x200,
            dmem_base_img: 0x200,
            dmem_base: 0x40,
            dmem_size: 0x100,
            dmem_sign: 0x5a4,
            boot_addr: 0x80,
            engine_id: 0x400,
            ucode_id: 9,
        }
    }

    #[test]
    fn boot_writes_the_signature_hints_then_starts() {
        let s = Sim::new(GSP.base);
        s.set(GSP.base + 0x040, 0); // the image leaves 0 in mailbox 0
        let r = GSP.boot(&s, &params(), Some(0), None, 0, 0).unwrap();
        assert_eq!(r, (0, 0));
        assert_eq!(wr_at(&s, 0x1000 + 0x210), [0x5a4]);
        assert_eq!(wr_at(&s, 0x1000 + 0x19c), [0x400]);
        assert_eq!(wr_at(&s, 0x1000 + 0x198), [9]);
        assert_eq!(wr_at(&s, 0x1000 + 0x180), [1]);
        assert_eq!(wr_at(&s, 0x104), [0x80]);
        assert_eq!(wr_at(&s, 0x100), [2]);
        assert!(wr_at(&s, 0x044).is_empty(), "mailbox 1 untouched");
        // order: hints, mailbox, boot vector, start
        let w = s.writes.borrow();
        let idx = |o: u32| w.iter().position(|x| x.0 == GSP.base + o).unwrap();
        assert!(idx(0x1000 + 0x180) < idx(0x040) && idx(0x040) < idx(0x104) && idx(0x104) < idx(0x100));
    }

    #[test]
    fn boot_default_mailbox_and_mailbox1_and_irq_clear() {
        let s = Sim::new(GSP.base);
        s.set(GSP.base + 0x004, 0x1234);
        // The sim's mailbox holds what was written: the 0xcafebeef default.
        let r = GSP.boot(&s, &params(), None, Some(7), 0xcafe_beef, 0x55).unwrap();
        assert_eq!(r, (0xcafe_beef, 7));
        assert_eq!(wr_at(&s, 0x040), [0xcafe_beef]);
        assert_eq!(wr_at(&s, 0x044), [7]);
        assert_eq!(wr_at(&s, 0x004), [0x55]);
    }

    #[test]
    fn boot_reports_a_bad_mailbox_and_a_timeout() {
        let s = Sim::new(GSP.base);
        assert_eq!(
            GSP.boot(&s, &params(), Some(0x1234), Some(0x77), 0, 0),
            Err(FalconError::Mailbox { mbox0: 0x1234, mbox1: 0x77 })
        );
        let mut s = Sim::new(GSP.base);
        s.never_halt = true;
        assert_eq!(
            GSP.boot(&s, &params(), Some(1), None, 1, 0),
            Err(FalconError::BootTimeout { mbox0: 1, mbox1: 0 })
        );
        let n = s.count_reads(GSP.base + 0x100);
        assert!((2_000_000..=2_000_002).contains(&n), "{n}");
    }

    #[test]
    fn probe_masks_are_the_documented_widths() {
        // version 4 bits, secret 2, ports 4+4; limits 9 and 8 bits.
        let s = Sim::new(GSP.base);
        s.set(GSP.base + 0x12c, 0xffff_ffff);
        s.set(GSP.base + 0x108, 0xffff_ffff);
        let i = GSP.probe(&s);
        assert_eq!((i.version, i.secret, i.code_ports, i.data_ports), (0xf, 3, 0xf, 0xf));
        assert_eq!((i.code_limit, i.data_limit), (0x1ff << 8, 0x3fe00 >> 1));
    }

    #[test]
    fn engine_reset_is_held_for_10us_between_its_two_writes() {
        let s = Sim::new(GSP.base);
        GSP.reset_eng(&s).unwrap();
        let w = s.writes.borrow();
        let first = w.iter().position(|x| *x == (GSP.base + 0x3c0, 1)).unwrap();
        let second = w.iter().position(|x| *x == (GSP.base + 0x3c0, 0)).unwrap();
        assert!(s.delays.borrow().iter().any(|&(n, us)| us == 10 && n > first && n <= second), "{:?}", s.delays.borrow());
    }

    #[test]
    fn dma_index_keeps_the_bits_outside_its_mask() {
        // DMAIDX = 0x600: bits 16 and 2:0 are ours (`(0 << 16) | (1 << 2) | 1`), the rest stays.
        let s = Sim::new(GSP.base);
        s.set(GSP.base + 0x600, 0x0001_0117);
        s.set(GSP.base + 0x624, 0x0000_0100);
        GSP.load(&s, 0x1000, &params()).unwrap();
        assert_eq!(wr_at(&s, 0x600), [0x115]);
        assert_eq!(wr_at(&s, 0x624), [0x180]);
        assert_eq!(wr_at(&s, 0x10c), [0]);
    }

    // ---- replay of nouveau's FWSEC-FRTS on the GSP falcon ----------------

    pub const FRTS_FIXTURE: &str = include_str!("../fixtures/fwsec-frts.txt");

    /// The image's bus address in the trace (`W 0x110110 0x00f7ee00`).
    pub const TRACE_DMA_ADDR: u64 = 0xf7ee_0000;
    /// The LibOS address the trace wrote to the mailboxes (run-specific).
    pub const TRACE_LIBOS: u64 = 0xf7fd_f000;

    /// `LoadParams` of the trace's FWSEC image: IMEM 0xe100 bytes at falcon
    /// address 0, DMEM 0x800 at 0 from image offset 0xe100 (the DMA base
    /// `0x00f7eee1 << 8` = image + 0xe100), signature at DMEM 0x5a4, engine
    /// 0x400, ucode 9.
    pub fn trace_params() -> LoadParams {
        LoadParams {
            imem_base_img: 0,
            imem_base: 0,
            imem_size: 0xe100,
            dmem_base_img: 0xe100,
            dmem_base: 0,
            dmem_size: 0x800,
            dmem_sign: 0x5a4,
            boot_addr: 0,
            engine_id: 0x400,
            ucode_id: 9,
        }
    }

    /// A replay of the trace where the polled status registers (which the
    /// fixture leaves out) come from a fixed idle state: no scrub, DMA done,
    /// halted.
    pub fn frts_replay() -> ReplayMmio {
        replay_of(FRTS_FIXTURE)
    }

    /// [`frts_replay`] over another text (a fixture with a value changed).
    pub fn replay_of(text: &str) -> ReplayMmio {
        let mut m = ReplayMmio::from_extract(text);
        m.fallback = alloc::vec![(0x11_00f4, 0x47f7), (0x11_0100, 0x10), (0x11_0118, 0x602)];
        m
    }

    fn fmt(w: &[(u32, u32)]) -> alloc::string::String {
        w.iter().map(|(o, v)| alloc::format!("W {o:#08x} {v:#010x}\n")).collect()
    }

    /// Everything nouveau's `nvkm_falcon_fw_boot` does on the falcon, in
    /// order, with the polled registers served idle. The signature fuse read
    /// (`0x8241e0`) belongs to the caller but sits in the queue, so it is
    /// taken here.
    fn run_boot(m: &ReplayMmio) {
        GSP.probe(m);
        m.rd32(0x82_41e0);
        GSP.reset(m).unwrap();
        GSP.load(m, TRACE_DMA_ADDR, &trace_params()).unwrap();
        // nvkm_gsp_fwsec_boot: mailbox 0 = 0, no mailbox 1, expects 0.
        GSP.boot(m, &trace_params(), Some(0), None, 0, 0).unwrap();
    }

    #[test]
    fn reset_load_and_boot_replay_the_trace() {
        let m = frts_replay();
        run_boot(&m);
        // The trace's writes up to and including the start (CPUCTL = 2).
        let end = m.expected_writes.iter().position(|w| *w == (0x11_0100, 2)).unwrap() + 1;
        let ours = m.writes.borrow().clone();
        assert_eq!(fmt(&ours), fmt(&m.expected_writes[..end]));
        // 225 IMEM + 8 DMEM transfers.
        assert_eq!(ours.iter().filter(|(o, _)| *o == 0x11_0118).count(), 233);
        // Every mailbox read up to the boot was consumed; the one left of
        // 0x110040 is ga102_gsp_reset's, after it.
        assert_eq!((m.unread(0x11_0040), m.unread(0x11_0044)), (1, 0));
    }

    #[test]
    fn gsp_reset_and_libos_mailboxes_replay_the_tail_of_the_trace() {
        let m = frts_replay();
        run_boot(&m);
        // (the FRTS verify reads are other registers: fwsec.rs owns them)
        let before = m.writes.borrow().len();
        GSP.gsp_reset(&m).unwrap();
        GSP.set_mailboxes(&m, TRACE_LIBOS as u32, (TRACE_LIBOS >> 32) as u32);
        let ours = m.writes.borrow()[before..].to_vec();
        let n = m.expected_writes.len();
        assert_eq!(fmt(&ours), fmt(&m.expected_writes[n - ours.len()..]));
        assert_eq!(ours.len(), 6);
        assert_eq!(m.unread(0x11_1668), 0);
    }
}
