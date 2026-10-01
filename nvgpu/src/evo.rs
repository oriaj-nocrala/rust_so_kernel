//! Display channels without GSP (phase 5.2 of the plan): instance memory,
//! the core channel and window 0 brought up over the display the GOP left
//! lit, and one UPDATE per channel that changes nothing on screen.
//!
//! The pieces, all nouveau's non-GSP path (`engine/disp/gv100.c`,
//! `tu102.c`) as `trace-nogsp` shows it (`nvgpu/fixtures/modeset-1-*`,
//! `modeset-2-*`):
//! - **instance memory**: 64 KiB of VRAM holding the RAMHT (handle →
//!   object hash, one table for every channel) and the context DMA objects
//!   the handles name. nouveau reaches it through BAR2's VM; here it is
//!   written through the PRAMIN window of BAR0 ([`Pramin`]), which nouveau
//!   also uses on this GPU (`subdev/instmem/nv50.c:398-401`, the trace's
//!   `0x1700 = 0x1ffe0`);
//! - **push buffers** in host memory (target HOST, as the trace's
//!   `0x610b24 = 0x00fff1c3`), filled by [`Push`] and handed to the channel
//!   by moving its PUT pointer;
//! - **channel bring-up** ([`init_channel`]) and the evidence to decide
//!   whether it went well ([`Faults`], [`gate`]).
//!
//! Where the GOP left the display: the core's ARMED state has head 0 at
//! 1080p60 on SOR 1, and window 0 scans the GOP framebuffer (phase 5.1).
//! A channel's ASSEMBLY state starts as a copy of its ARMED state (the
//! round-1 dump, `modeset-core-round1.txt`, differs only in the methods
//! nouveau pushed), so an UPDATE that pushes nothing about heads or SORs
//! latches the same mode. [`gate`] checks exactly that before any UPDATE.

use alloc::vec::Vec;

use crate::Mmio;

// ---------------------------------------------------------------------------
// VRAM and the PRAMIN window
// ---------------------------------------------------------------------------

/// VRAM size in MiB (`subdev/fb/ga102.c:31`; the trace reads `0x2000` =
/// 8 GiB).
pub const FB_VRAM_MIB: u32 = 0x11_83a4;

pub fn vram_size(m: &dyn Mmio) -> u64 {
    (m.rd32(FB_VRAM_MIB) as u64) << 20
}

/// The PRAMIN window: BAR0 `0x700000`, 1 MiB, onto VRAM at `0x1700 << 16`
/// (`subdev/instmem/nv50.c:60-69,398-401`).
pub const PRAMIN_BASE_REG: u32 = 0x00_1700;
pub const PRAMIN_WINDOW: u32 = 0x70_0000;
pub const PRAMIN_SIZE: u64 = 0x10_0000;

/// VRAM through the PRAMIN window. Moves the window as needed and puts the
/// firmware's base back on [`Pramin::restore`] (the VBIOS shadow does the
/// same, `subdev/bios/shadowramin.c:49`).
pub struct Pramin<'a> {
    m: &'a dyn Mmio,
    saved: u32,
    base: Option<u64>,
}

impl<'a> Pramin<'a> {
    pub fn new(m: &'a dyn Mmio) -> Self {
        Pramin { m, saved: m.rd32(PRAMIN_BASE_REG), base: None }
    }

    fn window(&mut self, vram: u64) -> u32 {
        assert!(vram % 4 == 0);
        let base = vram & !(PRAMIN_SIZE - 1);
        if self.base != Some(base) {
            self.m.wr32(PRAMIN_BASE_REG, (base >> 16) as u32);
            self.base = Some(base);
        }
        PRAMIN_WINDOW + (vram - base) as u32
    }

    pub fn wr32(&mut self, vram: u64, value: u32) {
        let o = self.window(vram);
        self.m.wr32(o, value);
    }

    pub fn rd32(&mut self, vram: u64) -> u32 {
        let o = self.window(vram);
        self.m.rd32(o)
    }

    /// The base register value found before the first move.
    pub fn saved(&self) -> u32 {
        self.saved
    }

    pub fn restore(self) {
        if self.base.is_some() {
            self.m.wr32(PRAMIN_BASE_REG, self.saved);
        }
    }
}

// ---------------------------------------------------------------------------
// Instance memory: RAMHT and context DMA objects
// ---------------------------------------------------------------------------

/// Where this driver puts the display's instance memory (RAMHT and context DMAs), 64 KiB aligned. nouveau's address on the target is `0x1ffc90000`
/// (`0x610014 = 0x1ffc9` in `modeset-1-disp-init.txt`), at the very top of VRAM: **after GSP-RM boots that range is its reserved region and the display's
/// context DMA lookups from it hang** (Ryzen #198-#201: the core stood in `CHNSTATUS_CORE.STG1_STATE = CTX_DMA_LOOKUP` for any handle; at `gpu=hdmi`,
/// before the GSP, the same push resolved: #202). So it lives low, between the boot's carve-outs (below 176 MiB) and the user heap (1 GiB).
pub const INST_VRAM: u64 = 0x1000_0000;
pub const INST_SIZE: u32 = 0x1_0000;

/// RAMHT: 0x2000 bytes (`engine/disp/tu102.c:221`) at the start of the
/// instance memory (`engine/disp/nv50.c:1641`: its parent is
/// `disp->inst`), 8 bytes per entry → 1024 entries, 10 hash bits.
pub const RAMHT_OFFSET: u32 = 0x0000;
pub const RAMHT_SIZE: u32 = 0x2000;
const RAMHT_BITS: u32 = 10;
/// nouveau's context DMAs follow the RAMHT (`modeset-2-inst.txt`: the
/// first at `0x2000`, 0x20 apart).
pub const DMAOBJ_OFFSET: u32 = 0x2000;

/// `nvkm_ramht_hash` (`core/ramht.c:26-38`): fold the handle in 10-bit
/// pieces, then the channel id above bit 6.
pub fn ramht_hash(chid: u32, handle: u32) -> u32 {
    let mut hash = 0;
    let mut h = handle;
    while h != 0 {
        hash ^= h & ((1 << RAMHT_BITS) - 1);
        h >>= RAMHT_BITS;
    }
    hash ^ (chid << (RAMHT_BITS - 4))
}

/// The RAMHT entry for `handle` on user channel `chid`, naming the object
/// at `obj` (an offset into the instance memory): `(offset, [handle,
/// context])`. Context = `chid << 25 | 0x40` (`engine/disp/gv100.c:356-357`)
/// with the object's offset shifted left 9 (`addr = -9`, `core/ramht.c:88`).
/// Collisions probe linearly (`core/ramht.c:45-55`); this driver inserts a
/// few handles and [`Ramht`] checks for them.
pub fn ramht_entry(chid: u32, handle: u32, obj: u32) -> (u32, [u32; 2]) {
    let co = ramht_hash(chid, handle);
    (RAMHT_OFFSET + co * 8, [handle, chid << 25 | 0x40 | obj << 9])
}

/// A GV100+ context DMA: a range of memory a method can name by handle
/// (`engine/dma/usergv100.c:37-58`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CtxDma {
    pub flags0: u32,
    pub start: u64,
    /// Inclusive.
    pub limit: u64,
}

/// `flags0` bits (`engine/dma/usergv100.c:104-114`).
pub const CTXDMA_PAGE: u32 = 0x40;
pub const CTXDMA_RW: u32 = 0x04;
pub const CTXDMA_VRAM: u32 = 0x01;

impl CtxDma {
    /// The five words at the object (`usergv100.c:50-54`): flags, then
    /// start and limit in 256-byte units, low word first.
    pub fn words(&self) -> [u32; 5] {
        let (s, l) = (self.start >> 8, self.limit >> 8);
        [self.flags0, s as u32, (s >> 32) as u32, l as u32, (l >> 32) as u32]
    }
}

/// `NV50_DISP_HANDLE_WNDW_CTX(0)` (`dispnv50/handles.h:13`): nouveau's
/// handle for a window surface of kind 0 (pitch linear).
pub const HANDLE_WNDW_CTX: u32 = 0xfb00_0000;

/// All of VRAM, as nouveau's surface context DMA has it (`0x2240`: flags 5,
/// limit `0x1ffffffff` in `modeset-2-inst.txt`).
pub fn vram_ctxdma(vram: u64) -> CtxDma {
    CtxDma { flags0: CTXDMA_RW | CTXDMA_VRAM, start: 0, limit: vram - 1 }
}

/// What this driver writes into its instance memory: one context DMA per
/// `(chid, handle)`, each 0x20 from [`DMAOBJ_OFFSET`], and their RAMHT
/// entries. Everything else is zero.
pub struct Ramht {
    pub objects: Vec<(u32, u32, CtxDma)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstError {
    /// Two handles hash to the same slot; this driver does not probe.
    Collision { slot: u32 },
    /// A word read back through PRAMIN is not what was written.
    Readback { vram: u64, wrote: u32, read: u32 },
}

impl Ramht {
    /// `(offset into the instance memory, value)` for every non-zero word.
    pub fn words(&self) -> Result<Vec<(u32, u32)>, InstError> {
        let mut out = Vec::new();
        let mut slots: Vec<u32> = Vec::new();
        for (i, (chid, handle, dma)) in self.objects.iter().enumerate() {
            let obj = DMAOBJ_OFFSET + i as u32 * 0x20;
            for (j, w) in dma.words().iter().enumerate() {
                if *w != 0 {
                    out.push((obj + j as u32 * 4, *w));
                }
            }
            let (slot, e) = ramht_entry(*chid, *handle, obj);
            if slots.contains(&slot) {
                return Err(InstError::Collision { slot });
            }
            slots.push(slot);
            out.push((slot, e[0]));
            out.push((slot + 4, e[1]));
        }
        Ok(out)
    }

    /// Zeroes the instance memory at `vram` and writes the table, then
    /// reads every written word back.
    pub fn write(&self, p: &mut Pramin, vram: u64) -> Result<(), InstError> {
        let words = self.words()?;
        for o in (0..INST_SIZE).step_by(4) {
            p.wr32(vram + o as u64, 0);
        }
        for (o, v) in &words {
            p.wr32(vram + *o as u64, *v);
        }
        for (o, v) in &words {
            let r = p.rd32(vram + *o as u64);
            if r != *v {
                return Err(InstError::Readback { vram: vram + *o as u64, wrote: *v, read: r });
            }
        }
        Ok(())
    }
}

/// `tu102_disp_init`'s instance-memory part (`engine/disp/tu102.c:167-178`):
/// the unnamed enable bit 0 of `0x610078`, then target (1 = VRAM) | valid
/// (8) and the address >> 16.
pub const DISP_INIT: u32 = 0x61_0078;
pub const DISP_INST_TARGET: u32 = 0x61_0010;
pub const DISP_INST_ADDR: u32 = 0x61_0014;
const INST_TARGET_VRAM: u32 = 0x1;
const INST_VALID: u32 = 0x8;

pub fn set_instance(m: &dyn Mmio, vram: u64) {
    m.mask(DISP_INIT, 0x1, 0x1);
    m.wr32(DISP_INST_TARGET, INST_VALID | INST_TARGET_VRAM);
    m.wr32(DISP_INST_ADDR, (vram >> 16) as u32);
}

/// Display ownership (`engine/disp/tu102.c:116-123`): bit 1 set = the VBIOS
/// still owns it. Clear on the target (the trace writes nothing there).
pub const DISP_OWNER: u32 = 0x62_54e8;
pub const DISP_OWNER_VBIOS: u32 = 0x2;

/// Which heads, SORs and windows exist (`engine/disp/gv100.c:238,324,550`).
pub const DISP_HEAD_SOR_MASK: u32 = 0x61_0060;
pub const DISP_WNDW_MASK: u32 = 0x61_0064;

/// The capability registers `tu102_disp_init` copies from the hardware
/// into the CAPS area (`engine/disp/tu102.c:125-164`), as `(kind, index,
/// from, to)` with `kind` the enable mask the copy sets first (0 = none).
/// Returned as a list so the adapter can report how many the GOP had
/// already set.
pub fn caps_copies(m: &dyn Mmio) -> Vec<CapCopy> {
    let hs = m.rd32(DISP_HEAD_SOR_MASK);
    let heads = hs & 0xff;
    let sors = (hs >> 8) & 0xff;
    let wndws = m.rd32(DISP_WNDW_MASK);
    let mut v = Vec::new();
    // SOR (tu102.c:130-134): `for (i = 0; i < sor.nr; i++)`, nr = the
    // highest SOR + 1.
    for i in 0..(32 - sors.leading_zeros()) {
        v.push(CapCopy { enable: Some((0x64_0000, 0x100 << i)), from: 0x61_c000 + i * 0x800, to: 0x64_0144 + i * 8 });
    }
    // Heads (tu102.c:137-149): RG, then 5 POSTCOMP words.
    for id in 0..8 {
        if heads & (1 << id) == 0 {
            continue;
        }
        v.push(CapCopy { enable: None, from: 0x61_6300 + id * 0x800, to: 0x64_0048 + id * 0x20 });
        for j in 0..5 {
            v.push(CapCopy { enable: None, from: 0x61_6140 + id * 0x800 + j * 4, to: 0x64_0680 + id * 0x20 + j * 4 });
        }
    }
    // Windows (tu102.c:152-158): 6 words each; the trailing `0x64000c`
    // bit 8 is set after each window's copy by the adapter's order below.
    for i in 0..(32 - wndws.leading_zeros()) {
        for j in 0..6 {
            v.push(CapCopy {
                enable: if j == 0 { Some((0x64_0004, 1 << i)) } else { None },
                from: 0x63_0100 + i * 0x800 + j * 4,
                to: 0x64_0780 + i * 0x20 + j * 4,
            });
        }
    }
    // IHUB (tu102.c:161-164).
    for i in 0..3 {
        v.push(CapCopy { enable: None, from: 0x62_e000 + i * 4, to: 0x64_0010 + i * 4 });
    }
    v
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapCopy {
    pub enable: Option<(u32, u32)>,
    pub from: u32,
    pub to: u32,
}

/// Lock pin capabilities, written as a constant (`tu102.c:126-127`, "XXX").
pub const CAPS_LOCK_PIN: (u32, u32) = (0x64_0008, 0x21);
/// Set after each window's capabilities (`tu102.c:158`).
pub const CAPS_WNDW_VALID: (u32, u32) = (0x64_000c, 0x100);

/// Runs the copies in nouveau's order; returns how many CAPS words changed
/// value (0 = the GOP had already done it).
pub fn copy_caps(m: &dyn Mmio) -> u32 {
    let mut changed = 0;
    m.wr32(CAPS_LOCK_PIN.0, CAPS_LOCK_PIN.1);
    let copies = caps_copies(m);
    for c in &copies {
        if let Some((reg, bit)) = c.enable {
            m.mask(reg, bit, bit);
        }
        let v = m.rd32(c.from);
        if m.rd32(c.to) != v {
            changed += 1;
        }
        m.wr32(c.to, v);
        // The last word of a window's block (to = 0x640794 + i * 0x20).
        if (0x64_0780..0x64_0880).contains(&c.to) && (c.to - 0x64_0780) % 0x20 == 0x14 {
            m.mask(CAPS_WNDW_VALID.0, CAPS_WNDW_VALID.1, CAPS_WNDW_VALID.1);
        }
    }
    changed
}

// ---------------------------------------------------------------------------
// Channels
// ---------------------------------------------------------------------------

/// A display channel: `ctrl` indexes the control registers, `user` the
/// method area and the RAMHT (`gv100.c:540-541,789-790`: core 0/0, window
/// n (n+1)/(n+1)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chan {
    pub ctrl: u32,
    pub user: u32,
}

pub const CORE: Chan = Chan { ctrl: 0, user: 0 };

pub const fn window(n: u32) -> Chan {
    Chan { ctrl: 1 + n, user: 1 + n }
}

impl Chan {
    pub fn is_core(&self) -> bool {
        self.ctrl == 0
    }

    /// The method area: PUT at +0, GET at +4 (`clc67d.h:76-79`,
    /// `clc67e.h:49-52`), ASSEMBLY state at +method (`gv100.c:336,737`).
    pub fn user_base(&self) -> u32 {
        if self.is_core() {
            0x68_0000
        } else {
            0x69_0000 + (self.user - 1) * 0x1000
        }
    }

    /// ARMED state: core `+0x8000` (`gv100.c:711`), window `+0x800`
    /// (`gv100.c:511`).
    pub fn armed_base(&self) -> u32 {
        self.user_base() + if self.is_core() { 0x8000 } else { 0x800 }
    }

    pub fn put(&self) -> u32 {
        self.user_base()
    }

    pub fn get(&self) -> u32 {
        self.user_base() + 4
    }

    /// Control: `0x6104e0 + ctrl * 4` (`gv100.c:379,768-772`).
    pub fn control(&self) -> u32 {
        0x61_04e0 + self.ctrl * 4
    }

    /// Push buffer registers: `0x610b20 + ctrl * 0x10` (address high),
    /// `+4` (low | target), `+8` valid, `+0xc` (`gv100.c:381-384`).
    pub fn push_regs(&self) -> u32 {
        0x61_0b20 + self.ctrl * 0x10
    }

    /// Status: core `0x610630`, idle when bits 16-20 are `0xb`
    /// (`gv100.c:722-731`); windows `0x610664 + (ctrl - 1) * 4`, idle when
    /// bits 16-19 are `4` (`gv100.c:340-350`).
    pub fn status(&self) -> (u32, u32, u32) {
        if self.is_core() {
            (0x61_0630, 0x001f_0000, 0x000b_0000)
        } else {
            (0x61_0664 + (self.ctrl - 1) * 4, 0x000f_0000, 0x0004_0000)
        }
    }

    /// Exception slot: `0x611020 + chid * 12` (stat, data, code), chid =
    /// the core 0 and windows 1-8 map straight (`gv100.c:894-908`).
    pub fn exception(&self) -> u32 {
        0x61_1020 + self.ctrl * 12
    }

    pub fn idle(&self, m: &dyn Mmio) -> bool {
        let (reg, mask, want) = self.status();
        m.rd32(reg) & mask == want
    }
}

/// Push target HOST (`engine/disp/nv50.c:694-701`: 1 VRAM, 2 NCOH, 3 HOST;
/// the trace's push is 3).
const PUSH_TARGET_HOST: u32 = 0x3;

/// What the core/window init writes into the push registers: `(high,
/// low)` of `target | bus >> 8` (`engine/disp/nv50.c:684-702`).
pub fn push_pointer(bus: u64) -> (u32, u32) {
    let v = PUSH_TARGET_HOST as u64 | (bus >> 8);
    ((v >> 32) as u32, v as u32)
}

/// Where the push buffer lives. It must stay put, and be readable by the
/// GPU, for as long as the channel runs.
pub trait PushMem {
    /// Bus address, 4 KiB aligned, under 40 bits (`dispnv50/disp.c:242-250`).
    fn bus_addr(&self) -> u64;
    fn len_words(&self) -> usize;
    fn write(&self, word: usize, value: u32);
    /// Makes the words written so far visible to the device before a PUT
    /// write (a store fence on x86).
    fn flush(&self);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChanError {
    /// The control register already has the channel's enable bits: someone
    /// (not the GOP, whose value is `0x21`) runs it.
    AlreadyRunning { control: u32 },
    BadPushAddress { bus: u64 },
    /// The status register never reached idle.
    NotIdle { status: u32 },
    /// GET never reached PUT.
    Stalled { put: u32, get: u32 },
    PushFull,
}

/// Channel control bits nouveau writes (`gv100.c:768-772`): `0x10` first
/// (with a mask), then `0x13`.
const CTL_PUT_ENABLE: u32 = 0x10;
const CTL_RUN: u32 = 0x13;

/// Polls every 10 µs for up to 2 s, nouveau's bound (`gv100.c:725,344`).
const POLL_US: u32 = 10;
const POLL_TRIES: u32 = 200_000;

pub fn wait<F: Fn() -> bool>(m: &dyn Mmio, f: F) -> bool {
    for _ in 0..POLL_TRIES {
        if f() {
            return true;
        }
        m.udelay(POLL_US);
    }
    f()
}

/// `gv100_disp_core_init` / `gv100_disp_dmac_init` (`gv100.c:760-773`,
/// `:373-389`) with PUT = 0: the push buffer's address, the enable bit,
/// PUT, run, idle. Refuses a channel whose control register already has
/// the run bits (`0x13`).
pub fn init_channel(m: &dyn Mmio, chan: Chan, push: &dyn PushMem) -> Result<(), ChanError> {
    let ctl = m.rd32(chan.control());
    if ctl & 0x3 == 0x3 {
        return Err(ChanError::AlreadyRunning { control: ctl });
    }
    let bus = push.bus_addr();
    if bus % 0x1000 != 0 || bus >> 40 != 0 || push.len_words() < 1024 {
        return Err(ChanError::BadPushAddress { bus });
    }
    let (hi, lo) = push_pointer(bus);
    let r = chan.push_regs();
    m.wr32(r + 4, lo);
    m.wr32(r, hi);
    m.wr32(r + 8, 0x1);
    m.wr32(r + 0xc, 0x40);
    m.mask(chan.control(), CTL_PUT_ENABLE, CTL_PUT_ENABLE);
    m.wr32(chan.put(), 0);
    m.wr32(chan.control(), CTL_RUN);
    if !wait(m, || chan.idle(m)) {
        return Err(ChanError::NotIdle { status: m.rd32(chan.status().0) });
    }
    Ok(())
}

/// A push buffer being filled from `at`. It wraps only through [`wind`]
/// (a JUMP back to word 0), which [`flip`] does when the end is near.
pub struct Push<'a> {
    mem: &'a dyn PushMem,
    at: usize,
}

impl<'a> Push<'a> {
    pub fn new(mem: &'a dyn PushMem, at_bytes: u32) -> Self {
        Push { mem, at: at_bytes as usize / 4 }
    }

    /// One incrementing-method header (`clc67d.h:59-66`: opcode 0, count
    /// 27:18, offset 13:2 — i.e. the method's byte address) and its data.
    pub fn mthd(&mut self, method: u32, data: &[u32]) -> Result<(), ChanError> {
        if self.at + 1 + data.len() > self.mem.len_words() {
            return Err(ChanError::PushFull);
        }
        self.mem.write(self.at, (data.len() as u32) << 18 | (method & 0x3ffc));
        for (i, d) in data.iter().enumerate() {
            self.mem.write(self.at + 1 + i, *d);
        }
        self.at += 1 + data.len();
        Ok(())
    }

    pub fn put_bytes(&self) -> u32 {
        (self.at * 4) as u32
    }

    /// Words that still fit before the end, keeping one for a JUMP.
    pub fn room_words(&self) -> usize {
        self.mem.len_words().saturating_sub(self.at + 1)
    }
}

/// A JUMP to byte offset `to` of the push buffer (`clc67e.h:36-45`:
/// opcode 31:29 = 1, offset 11:2; `nvif/push507c.h:19-24`).
pub fn jump_header(to: u32) -> u32 {
    1 << 29 | (to & 0xffc)
}

/// Back to the start of the push buffer, as `nv50_dmac_wind` and the kick
/// after it do (`dispnv50/disp.c:169-209`): a JUMP to 0 where the next
/// method would go, then PUT = 0; GET follows the jump. Only once the
/// channel has fetched everything (GET = PUT, not 0): a PUT equal to GET
/// would be ignored.
pub fn wind(m: &dyn Mmio, chan: Chan, push: &mut Push) -> Result<(), ChanError> {
    let put = push.put_bytes();
    if put == 0 {
        return Ok(());
    }
    let get = m.rd32(chan.get());
    if get != put {
        return Err(ChanError::Stalled { put, get });
    }
    push.mem.write(push.at, jump_header(0));
    push.at = 0;
    push.mem.flush();
    m.wr32(chan.put(), 0);
    if !wait(m, || m.rd32(chan.get()) == 0) {
        return Err(ChanError::Stalled { put: 0, get: m.rd32(chan.get()) });
    }
    Ok(())
}

/// Hands the pushed words to the channel without waiting for it.
pub fn submit(m: &dyn Mmio, chan: Chan, push: &Push) {
    push.mem.flush();
    m.wr32(chan.put(), push.put_bytes());
}

/// Hands the pushed words to the channel and waits until it has fetched
/// them all and gone idle.
pub fn kick(m: &dyn Mmio, chan: Chan, push: &Push) -> Result<(), ChanError> {
    submit(m, chan, push);
    let put = push.put_bytes();
    if !wait(m, || m.rd32(chan.get()) == put) {
        return Err(ChanError::Stalled { put, get: m.rd32(chan.get()) });
    }
    if !wait(m, || chan.idle(m)) {
        return Err(ChanError::NotIdle { status: m.rd32(chan.status().0) });
    }
    // The exception slot is not checked here: it is not zero at rest (the
    // GOP leaves `0x80` in the core's and window 0's, Ryzen boot #70).
    // Exceptions are judged by `Faults::new_since`.
    Ok(())
}

// Methods (`clc67d.h`, `clc67e.h`).
pub const CORE_UPDATE: u32 = 0x200; // clc67d.h:80
pub const CORE_SET_INTERLOCK_FLAGS: u32 = 0x218; // clc67d.h:153
pub const CORE_SET_WINDOW_INTERLOCK_FLAGS: u32 = 0x21c; // clc67d.h:185
pub const WNDW_UPDATE: u32 = 0x200; // clc67e.h:53
pub const WNDW_SET_CONTEXT_DMA_ISO0: u32 = 0x240; // clc67e.h:179
pub const WNDW_SET_INTERLOCK_FLAGS: u32 = 0x370; // clc67e.h:324
pub const WNDW_SET_WINDOW_INTERLOCK_FLAGS: u32 = 0x374; // clc67e.h:356
/// `UPDATE_RELEASE_ELV_TRUE` (`clc67d.h:89-91`, `clc67e.h:54-56`), the
/// value nouveau pushes (`modeset-push.txt`).
pub const UPDATE_RELEASE_ELV: u32 = 0x1;

/// The core push of this phase: no interlock (the GOP's ARMED flags would
/// make the UPDATE wait for window 0 and a cursor), then UPDATE. Nothing
/// about heads, SORs or windows: they stay what ASSEMBLY inherited.
pub fn push_core_update(p: &mut Push) -> Result<(), ChanError> {
    p.mthd(CORE_SET_INTERLOCK_FLAGS, &[0])?;
    p.mthd(CORE_SET_WINDOW_INTERLOCK_FLAGS, &[0])?;
    push_update(p)
}

/// The window's state push, without UPDATE: every method whose ASSEMBLY
/// differs from ARMED gets its ARMED value (`restore`: `(method, armed)`),
/// then the same surface named by this driver's context DMA instead of the
/// GOP's (its handle `0x45564144` is not in this RAMHT), and no interlock.
///
/// Unlike the core, a window channel comes up with a reset ASSEMBLY (Ryzen
/// boot #73: size 0, format `0xe9`, pitch 0), so an UPDATE alone would
/// latch an empty surface. nouveau's first flip pushes the same kind of
/// state (`modeset-push.txt`: SetSize, SetParams, SetPlanarStorage, …).
pub fn push_window_state(p: &mut Push, restore: &[(u32, u32)], handle: u32) -> Result<(), ChanError> {
    for (m, v) in restore {
        if !WNDW_PUSHED.contains(m) {
            p.mthd(*m, &[*v])?;
        }
    }
    p.mthd(WNDW_SET_CONTEXT_DMA_ISO0, &[handle])?;
    p.mthd(WNDW_SET_INTERLOCK_FLAGS, &[0])?;
    p.mthd(WNDW_SET_WINDOW_INTERLOCK_FLAGS, &[0])
}

/// UPDATE with `RELEASE_ELV` (core and window alike).
pub fn push_update(p: &mut Push) -> Result<(), ChanError> {
    p.mthd(CORE_UPDATE, &[UPDATE_RELEASE_ELV])
}

// ---------------------------------------------------------------------------
// Page flips (phase 5.3)
// ---------------------------------------------------------------------------

pub const WNDW_SET_OFFSET0: u32 = 0x260; // clc67e.h:181
pub const WNDW_SET_PRESENT_CONTROL: u32 = 0x308; // clc67e.h:279
/// `MIN_PRESENT_INTERVAL` 1, `BEGIN_MODE_NON_TEARING` (`clc67e.h:280-282`):
/// nouveau's value in its first flip (`modeset-push.txt`). The GOP leaves
/// 0 (interval 0) in ARMED (Ryzen boot #73).
pub const PRESENT_CONTROL_VSYNC: u32 = 0x1;
/// Words one flip pushes: three one-word methods.
const FLIP_WORDS: usize = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlipError {
    /// `SET_OFFSET` holds the address >> 8 (`clc67e.h:181-182`, and the
    /// trace's `0x2000` for 2 MiB), and a context DMA spans 40 bits.
    Misaligned { vram: u64 },
    /// The channel has not fetched the previous flip yet.
    Busy { put: u32, get: u32 },
    Chan(ChanError),
}

impl From<ChanError> for FlipError {
    fn from(e: ChanError) -> Self {
        FlipError::Chan(e)
    }
}

/// Queues a flip of `chan` (a window) to the surface at `vram`, with every
/// other surface method as ASSEMBLY already holds it: `SET_PRESENT_CONTROL`
/// (non-tearing: the new offset latches at a vblank), `SET_OFFSET(0)` and
/// UPDATE. Does not wait: [`flip_latched`] says when it took. Returns the
/// `SET_OFFSET` value pushed. Refuses while the previous push is unfetched,
/// which also keeps GET = PUT for [`wind`].
pub fn flip(m: &dyn Mmio, chan: Chan, push: &mut Push, vram: u64) -> Result<u32, FlipError> {
    if vram % 256 != 0 || vram >> 40 != 0 {
        return Err(FlipError::Misaligned { vram });
    }
    let (put, get) = (push.put_bytes(), m.rd32(chan.get()));
    if put != get {
        return Err(FlipError::Busy { put, get });
    }
    if push.room_words() < FLIP_WORDS {
        wind(m, chan, push)?;
    }
    let origin = (vram >> 8) as u32;
    push.mthd(WNDW_SET_PRESENT_CONTROL, &[PRESENT_CONTROL_VSYNC])?;
    push.mthd(WNDW_SET_OFFSET0, &[origin])?;
    push_update(push)?;
    submit(m, chan, push);
    Ok(origin)
}

/// The flip that pushed `origin` was fetched (GET = PUT) and the window's
/// ARMED offset is `origin`. Not a completion on its own: ARMED changes
/// within microseconds of the UPDATE, while the new surface loads (LOADV)
/// at the next vblank (Ryzen boot #76); the caller also waits for a vblank.
pub fn flip_latched(m: &dyn Mmio, chan: Chan, push: &Push, origin: u32) -> bool {
    m.rd32(chan.get()) == push.put_bytes() && m.rd32(chan.armed_base() + WNDW_SET_OFFSET0) == origin
}

// ---------------------------------------------------------------------------
// Evidence
// ---------------------------------------------------------------------------

/// Everything that says a channel or the display complained, read without
/// acknowledging anything (the interrupts behind them stay disabled: this
/// phase polls).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Faults {
    /// `0x611ec0` (`gv100.c:1082`).
    pub disp_intr: u32,
    /// CTRL_DISP status `0x611c30`: bits 0-2 supervisors 1-3, 7 error,
    /// 8 awaken (`gv100.c:939-980`).
    pub ctrl_disp: u32,
    /// EXC_OTHER `0x611854` (bit 0 core), EXC_WIN `0x61184c`, EXC_WINIM
    /// `0x611850` (`gv100.c:987,1016,1036`).
    pub exc_other: u32,
    pub exc_win: u32,
    pub exc_winim: u32,
    /// Exception slots of the core and window 0 (`0x611020 + chid * 12`).
    pub core_exc: u32,
    pub wndw0_exc: u32,
}

pub const CTRL_DISP_STAT: u32 = 0x61_1c30;
const CTRL_DISP_SUPERVISORS: u32 = 0x7;
const CTRL_DISP_ERROR: u32 = 0x80;

impl Faults {
    pub fn read(m: &dyn Mmio) -> Faults {
        Faults {
            disp_intr: m.rd32(0x61_1ec0),
            ctrl_disp: m.rd32(CTRL_DISP_STAT),
            exc_other: m.rd32(0x61_1854),
            exc_win: m.rd32(0x61_184c),
            exc_winim: m.rd32(0x61_1850),
            core_exc: m.rd32(CORE.exception()),
            wndw0_exc: m.rd32(window(0).exception()),
        }
    }

    /// A supervisor ran (or waits): the display thinks the mode changed.
    pub fn supervisor(&self) -> bool {
        self.ctrl_disp & CTRL_DISP_SUPERVISORS != 0
    }

    /// Anything that means a channel or the display rejected something.
    /// Awaken (bit 8) and the vblank bits of `disp_intr` are normal. The
    /// exception slots are not looked at: nouveau reads a slot only when
    /// its EXC_* bit is set (`gv100.c:987-1044`), and at rest they hold the
    /// last method (`0x80` from the GOP, `0x178` after window 0's bring-up
    /// on Ryzen boot #72, with EXC_WIN still 0).
    pub fn bad(&self) -> bool {
        self.supervisor()
            || self.ctrl_disp & CTRL_DISP_ERROR != 0
            || self.exc_other != 0
            || self.exc_win != 0
            || self.exc_winim != 0
    }

    /// What changed since `before` (the firmware may leave bits of its own).
    pub fn new_since(&self, before: &Faults) -> Faults {
        Faults {
            disp_intr: self.disp_intr & !before.disp_intr,
            ctrl_disp: self.ctrl_disp & !before.ctrl_disp,
            exc_other: self.exc_other & !before.exc_other,
            exc_win: self.exc_win & !before.exc_win,
            exc_winim: self.exc_winim & !before.exc_winim,
            core_exc: self.core_exc & !before.core_exc,
            wndw0_exc: self.wndw0_exc & !before.wndw0_exc,
        }
    }
}

/// The safety gate before an UPDATE: every `(method, assembly, armed)`
/// where the channel's ASSEMBLY state differs from its ARMED state, except
/// the methods `ignore` lists (the ones about to be pushed, and UPDATE,
/// whose ARMED value is the residue of the last trigger). Empty = an
/// UPDATE latches what scans out already.
pub fn gate(m: &dyn Mmio, chan: Chan, methods: &[u32], ignore: &[u32]) -> Vec<(u32, u32, u32)> {
    methods
        .iter()
        .filter(|x| !ignore.contains(x))
        .filter_map(|&x| {
            let a = m.rd32(chan.user_base() + x);
            let b = m.rd32(chan.armed_base() + x);
            (a != b).then_some((x, a, b))
        })
        .collect()
}

/// Methods of the core push and UPDATE, which the gate ignores.
pub const CORE_PUSHED: [u32; 3] = [CORE_UPDATE, CORE_SET_INTERLOCK_FLAGS, CORE_SET_WINDOW_INTERLOCK_FLAGS];
pub const WNDW_PUSHED: [u32; 4] =
    [WNDW_UPDATE, WNDW_SET_CONTEXT_DMA_ISO0, WNDW_SET_INTERLOCK_FLAGS, WNDW_SET_WINDOW_INTERLOCK_FLAGS];

/// Display registers the kernel reads on request (`/dev/dispctl peek <offset>`, an instrument: what the GOP left, what a channel holds): 4-aligned,
/// in the control and status block (`0x610000-0x611fff`: channel control and status, exception slots, interrupt masks) or in the channels' user and
/// state areas (`0x640000-0x6dffff`: the core's ASSEMBLY and ARMED, windows, cursors). Nothing else (not PRAMIN, not the other engines): the
/// driver already reads these ranges in its own status lines, so a read is not expected to have a side effect (an instrument, to be used with that in mind).
pub fn peek_allowed(offset: u32) -> bool {
    offset % 4 == 0 && ((0x61_0000..0x61_2000).contains(&offset) || (0x64_0000..0x6e_0000).contains(&offset))
}

/// Registers that read `0xbadf5xxx` do not exist (PRI error); window 0's
/// dump has 22 of them on the target. The gate skips them.
pub fn is_pri_error(v: u32) -> bool {
    v & 0xffff_f000 == 0xbadf_5000
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use core::cell::RefCell;

    const INST: &str = include_str!("../fixtures/modeset-2-inst.txt");
    const CHAN_INIT: &str = include_str!("../fixtures/modeset-2-chan-init.txt");
    const DISP_INIT_FX: &str = include_str!("../fixtures/modeset-1-disp-init.txt");

    /// `(offset, value)` rows of `modeset-2-inst.txt`.
    fn inst_rows() -> Vec<(u32, u32)> {
        INST.lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                (u32::from_str_radix(f[1].trim_start_matches("0x"), 16).unwrap(), u32::from_str_radix(f[2], 16).unwrap())
            })
            .collect()
    }

    /// `(register, value)` of a `gpu-trace.py disp` fixture, optionally one
    /// class only.
    fn disp_rows(text: &str, class: Option<&str>) -> Vec<(u32, u32)> {
        text.lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| l.split_whitespace().collect::<Vec<_>>())
            .filter(|f| class.map_or(true, |c| f[1] == c))
            .map(|f| (u32::from_str_radix(f[2], 16).unwrap(), u32::from_str_radix(f[3], 16).unwrap()))
            .collect()
    }

    /// (A) The hash and the context word are nouveau's: every RAMHT entry
    /// the trace wrote (for the core and 8 windows, handles `0xf0000000`,
    /// `0xf0000001` and `0xfb000000`) is at the slot and holds the context
    /// `ramht_entry` computes from the object offset it names.
    #[test]
    fn ramht_entries_match_the_trace() {
        let rows = inst_rows();
        let handles: Vec<(u32, u32)> = rows.iter().filter(|(o, _)| *o < DMAOBJ_OFFSET && o % 8 == 0).copied().collect();
        assert_eq!(handles.len(), 20);
        for (slot, handle) in handles {
            let ctx = rows.iter().find(|(o, _)| *o == slot + 4).unwrap().1;
            let chid = ctx >> 25;
            let obj = (ctx & 0x01ff_ffbf) >> 9;
            assert_eq!(ramht_entry(chid, handle, obj), (slot, [handle, ctx]), "handle {handle:#x} chid {chid}");
        }
    }

    /// (A) Context DMA words: the sync object (4 KiB of VRAM at
    /// `0x62000`, flags `0x45`), the whole-VRAM one (`0x45`) and the
    /// surface one (`0x05`, the one this driver uses) at `0x2240`.
    #[test]
    fn ctxdma_words_match_the_trace() {
        let rows = inst_rows();
        let at = |base: u32| -> [u32; 5] {
            core::array::from_fn(|i| rows.iter().rev().find(|(o, _)| *o == base + i as u32 * 4).map_or(0, |r| r.1))
        };
        let sync = CtxDma { flags0: CTXDMA_PAGE | CTXDMA_RW | CTXDMA_VRAM, start: 0x62000, limit: 0x62fff };
        assert_eq!(sync.words(), at(0x2000));
        let all = CtxDma { flags0: CTXDMA_PAGE | CTXDMA_RW | CTXDMA_VRAM, start: 0, limit: (8 << 30) - 1 };
        assert_eq!(all.words(), at(0x2020));
        assert_eq!(vram_ctxdma(8 << 30).words(), at(0x2240));
    }

    /// (A) This driver's table: window 0's surface handle lands where
    /// nouveau's did (`0x1f98`), naming the object at `0x2000`.
    #[test]
    fn this_drivers_table() {
        let t = Ramht { objects: alloc::vec![(window(0).user, HANDLE_WNDW_CTX, vram_ctxdma(8 << 30))] };
        let w = t.words().unwrap();
        assert_eq!(
            w,
            alloc::vec![(0x2000, 0x5), (0x200c, 0x01ff_ffff), (0x1f98, 0xfb00_0000), (0x1f9c, 0x0240_0040)]
        );
        // Same handle twice on one channel: same slot, refused.
        let t2 = Ramht { objects: alloc::vec![(1, HANDLE_WNDW_CTX, vram_ctxdma(1 << 30)); 2] };
        assert_eq!(t2.words(), Err(InstError::Collision { slot: 0x1f98 }));
    }

    /// A tiny display engine: registers in a map; PRAMIN backed by a VRAM
    /// map; a PUT write executes the pushed methods into ASSEMBLY and GET
    /// follows; UPDATE copies ASSEMBLY to ARMED for every method written so
    /// far (enough to see what an UPDATE latches).
    struct Sim {
        regs: RefCell<BTreeMap<u32, u32>>,
        vram: RefCell<BTreeMap<u64, u32>>,
        push: RefCell<BTreeMap<u32, Vec<u32>>>, // by channel user base
        writes: RefCell<Vec<(u32, u32)>>,
        updates: RefCell<Vec<u32>>,
        /// GET does not follow PUT (a channel that has not fetched yet).
        stall: core::cell::Cell<bool>,
    }

    impl Sim {
        fn new() -> Sim {
            let mut r = BTreeMap::new();
            r.insert(PRAMIN_BASE_REG, 0x1ffe0);
            r.insert(0x61_04e0, 0x21);
            r.insert(0x61_04e4, 0x0);
            r.insert(0x61_0630, 0x200b_0000);
            r.insert(0x61_0664, 0x0004_0000);
            Sim {
                regs: RefCell::new(r),
                vram: RefCell::new(BTreeMap::new()),
                push: RefCell::new(BTreeMap::new()),
                writes: RefCell::new(Vec::new()),
                updates: RefCell::new(Vec::new()),
                stall: core::cell::Cell::new(false),
            }
        }
        fn set(&self, o: u32, v: u32) {
            self.regs.borrow_mut().insert(o, v);
        }
        fn val(&self, o: u32) -> u32 {
            *self.regs.borrow().get(&o).unwrap_or(&0)
        }
    }

    impl Mmio for Sim {
        fn rd32(&self, o: u32) -> u32 {
            if (PRAMIN_WINDOW..PRAMIN_WINDOW + PRAMIN_SIZE as u32).contains(&o) {
                let a = ((self.val(PRAMIN_BASE_REG) as u64) << 16) + (o - PRAMIN_WINDOW) as u64;
                return *self.vram.borrow().get(&a).unwrap_or(&0);
            }
            self.val(o)
        }
        fn wr32(&self, o: u32, v: u32) {
            self.writes.borrow_mut().push((o, v));
            if (PRAMIN_WINDOW..PRAMIN_WINDOW + PRAMIN_SIZE as u32).contains(&o) {
                let a = ((self.val(PRAMIN_BASE_REG) as u64) << 16) + (o - PRAMIN_WINDOW) as u64;
                self.vram.borrow_mut().insert(a, v);
                return;
            }
            let chans = [CORE, window(0)];
            if let Some(c) = chans.iter().find(|c| c.put() == o) {
                let (base, armed) = (c.user_base(), c.armed_base());
                if self.stall.get() {
                    self.set(c.put(), v);
                    return;
                }
                let words = self.push.borrow().get(&base).cloned().unwrap_or_default();
                let mut i = (self.val(c.get()) / 4) as usize;
                while i != (v / 4) as usize {
                    let h = words[i];
                    if h >> 29 == 1 {
                        i = (h & 0xffc) as usize / 4; // JUMP
                        continue;
                    }
                    let (n, m) = ((h >> 18) as usize, h & 0x3ffc);
                    for k in 0..n {
                        let mm = m + 4 * k as u32;
                        self.set(base + mm, words[i + 1 + k]);
                        if mm == 0x200 {
                            self.updates.borrow_mut().push(base);
                            let asm: Vec<(u32, u32)> = self
                                .regs
                                .borrow()
                                .range(base + 0x200..base + 0x800)
                                .map(|(a, b)| (*a - base, *b))
                                .collect();
                            for (mm, vv) in asm {
                                self.set(armed + mm, vv);
                            }
                        }
                    }
                    i += 1 + n;
                }
                self.set(c.put(), v);
                self.set(c.get(), v);
                return;
            }
            self.set(o, v);
        }
        fn udelay(&self, _us: u32) {}
    }

    struct Mem<'a> {
        sim: &'a Sim,
        base: u32,
        bus: u64,
    }

    impl PushMem for Mem<'_> {
        fn bus_addr(&self) -> u64 {
            self.bus
        }
        fn len_words(&self) -> usize {
            1024
        }
        fn write(&self, w: usize, v: u32) {
            let mut p = self.sim.push.borrow_mut();
            let buf = p.entry(self.base).or_insert_with(|| alloc::vec![0; 1024]);
            buf[w] = v;
        }
        fn flush(&self) {}
    }

    /// (A) Core bring-up writes what nouveau wrote (`modeset-2-chan-init.txt`
    /// core lines minus the interrupt enable, which this phase leaves off),
    /// in the same order, for a push at the trace's bus address.
    #[test]
    fn core_init_writes_the_trace() {
        let sim = Sim::new();
        let mem = Mem { sim: &sim, base: CORE.user_base(), bus: 0xfff1_c000 };
        init_channel(&sim, CORE, &mem).unwrap();
        // Up to the run write; the interrupt enable (`0x611dac`) is left off.
        let rows = disp_rows(CHAN_INIT, None);
        let end = rows.iter().position(|r| *r == (0x61_04e0, CTL_RUN)).unwrap();
        let trace: Vec<(u32, u32)> = rows[..=end].iter().filter(|(o, _)| *o != 0x61_1dac).copied().collect();
        assert_eq!(*sim.writes.borrow(), trace);
    }

    /// (A) Window 0 likewise (its push at the trace's address `0xfff1b000`).
    #[test]
    fn window0_init_writes_the_trace() {
        let sim = Sim::new();
        sim.set(0x61_04e4, 0x1); // what nouveau's mask read (it wrote 0x11)
        let mem = Mem { sim: &sim, base: window(0).user_base(), bus: 0xfff1_b000 };
        init_channel(&sim, window(0), &mem).unwrap();
        let rows = disp_rows(CHAN_INIT, None);
        let start = rows.iter().position(|(o, _)| *o == 0x61_0b34).unwrap();
        let end = rows.iter().position(|r| *r == (0x61_04e4, CTL_RUN)).unwrap();
        assert_eq!(*sim.writes.borrow(), rows[start..=end].to_vec());
    }

    /// (A) A channel already running (control `0x13`) is refused before
    /// any write.
    #[test]
    fn refuses_a_running_channel() {
        let sim = Sim::new();
        sim.set(0x61_04e0, 0x13);
        let mem = Mem { sim: &sim, base: CORE.user_base(), bus: 0x1000 };
        assert_eq!(init_channel(&sim, CORE, &mem), Err(ChanError::AlreadyRunning { control: 0x13 }));
        assert!(sim.writes.borrow().is_empty());
    }

    /// (A) Instance-memory setup is the trace's (`modeset-1-disp-init.txt`,
    /// classes `init`/`inst`).
    #[test]
    fn instance_registers_match_the_trace() {
        let sim = Sim::new();
        // nouveau's own address (the trace's): the function is the trace's whatever INST_VRAM is
        set_instance(&sim, 0x1_ffc9_0000);
        let mut trace = disp_rows(DISP_INIT_FX, Some("init"));
        trace.extend(disp_rows(DISP_INIT_FX, Some("inst")));
        assert_eq!(*sim.writes.borrow(), trace);
    }

    /// (A) The capability copy writes nouveau's registers in nouveau's
    /// order (`modeset-1-disp-init.txt`, class `caps`), for the target's
    /// 4 SORs, 4 heads and 8 windows.
    #[test]
    fn caps_copy_matches_the_trace() {
        let sim = Sim::new();
        sim.set(DISP_HEAD_SOR_MASK, 0x0f0f);
        sim.set(DISP_WNDW_MASK, 0xff);
        sim.set(0x64_0000, 0xf0f);
        sim.set(0x64_0004, 0xff);
        sim.set(0x64_000c, 0x100);
        // The hardware side answers what the trace wrote to the CAPS side.
        let trace = disp_rows(DISP_INIT_FX, Some("caps"));
        for c in caps_copies(&sim) {
            let v = trace.iter().find(|(o, _)| *o == c.to).unwrap().1;
            sim.set(c.from, v);
        }
        let nonzero = caps_copies(&sim).iter().filter(|c| sim.val(c.from) != 0).count();
        assert_eq!(copy_caps(&sim) as usize, nonzero);
        let ours: Vec<(u32, u32)> = sim.writes.borrow().iter().filter(|(o, _)| *o >= 0x64_0000).copied().collect();
        assert_eq!(ours, trace);
        // A second run changes nothing.
        assert_eq!(copy_caps(&sim), 0);
    }

    /// (A) PRAMIN: the window moves to the 1 MiB block, the table lands in
    /// VRAM, the firmware's base is put back.
    #[test]
    fn instance_memory_through_pramin() {
        let sim = Sim::new();
        let t = Ramht { objects: alloc::vec![(1, HANDLE_WNDW_CTX, vram_ctxdma(8 << 30))] };
        let mut p = Pramin::new(&sim);
        t.write(&mut p, INST_VRAM).unwrap();
        assert_eq!(sim.val(PRAMIN_BASE_REG), ((INST_VRAM >> 16) & !0xf) as u32);
        p.restore();
        assert_eq!(sim.val(PRAMIN_BASE_REG), 0x1ffe0);
        let v = sim.vram.borrow();
        assert_eq!(v.get(&(INST_VRAM + 0x1f98)), Some(&0xfb00_0000));
        assert_eq!(v.get(&(INST_VRAM + 0x2000)), Some(&0x5));
        assert_eq!(v.get(&(INST_VRAM + 0xfffc)), Some(&0));
    }

    /// (A) Push headers: `0x40208` = one word for method `0x208`, as the
    /// 5.1 capture decoded them.
    #[test]
    fn push_header_format() {
        let sim = Sim::new();
        let mem = Mem { sim: &sim, base: 0, bus: 0 };
        let mut p = Push::new(&mem, 0);
        p.mthd(0x208, &[0xf000_0000]).unwrap();
        p.mthd(0x1004, &[1, 2, 3]).unwrap();
        assert_eq!(&sim.push.borrow()[&0][..6], &[0x40208, 0xf000_0000, 0xc1004, 1, 2, 3]);
        assert_eq!(p.put_bytes(), 24);
    }

    /// (B) The whole phase on the simulator: ASSEMBLY = ARMED except the
    /// GOP's interlock flags, the gate passes, both UPDATEs latch, and the
    /// ARMED state afterwards differs from before only in what was pushed.
    #[test]
    fn core_then_window_update_latches_only_what_was_pushed() {
        let sim = Sim::new();
        let core_state = [(0x218, 0x10000), (0x21c, 1), (0x320, 0x901), (0x2064, 0x0465_0898)];
        for (m, v) in core_state {
            sim.set(CORE.user_base() + m, v);
            sim.set(CORE.armed_base() + m, v);
        }
        let w0 = window(0);
        for (m, v) in [(0x240, 0x4556_4144), (0x230, 0x80), (0x370, 1), (0x374, 1)] {
            sim.set(w0.user_base() + m, v);
            sim.set(w0.armed_base() + m, v);
        }
        let methods: Vec<u32> = core_state.iter().map(|x| x.0).collect();
        let cm = Mem { sim: &sim, base: CORE.user_base(), bus: 0x10_0000 };
        let wm = Mem { sim: &sim, base: w0.user_base(), bus: 0x10_1000 };
        init_channel(&sim, CORE, &cm).unwrap();
        assert!(gate(&sim, CORE, &methods, &CORE_PUSHED).is_empty());
        let mut p = Push::new(&cm, 0);
        push_core_update(&mut p).unwrap();
        kick(&sim, CORE, &p).unwrap();
        assert_eq!(sim.val(CORE.armed_base() + 0x320), 0x901);
        assert_eq!(sim.val(CORE.armed_base() + 0x21c), 0);

        init_channel(&sim, w0, &wm).unwrap();
        // A window channel comes up with a reset ASSEMBLY (boot #73).
        sim.set(w0.user_base() + 0x230, 0);
        sim.set(w0.user_base() + 0x22c, 0xe9);
        sim.set(w0.armed_base() + 0x22c, 0xcf);
        let wm_methods = [0x22c, 0x230, 0x240, 0x370, 0x374];
        let differ = gate(&sim, w0, &wm_methods, &WNDW_PUSHED);
        assert_eq!(differ, alloc::vec![(0x22c, 0xe9, 0xcf), (0x230, 0, 0x80)]);
        let restore: Vec<(u32, u32)> = differ.iter().map(|d| (d.0, d.2)).collect();
        let mut p = Push::new(&wm, 0);
        push_window_state(&mut p, &restore, HANDLE_WNDW_CTX).unwrap();
        kick(&sim, w0, &p).unwrap();
        // Nothing latched yet; ASSEMBLY now matches ARMED.
        assert_eq!(sim.val(w0.armed_base() + 0x240), 0x4556_4144);
        assert!(gate(&sim, w0, &wm_methods, &WNDW_PUSHED).is_empty());
        push_update(&mut p).unwrap();
        kick(&sim, w0, &p).unwrap();
        assert_eq!(sim.val(w0.armed_base() + 0x240), HANDLE_WNDW_CTX);
        assert_eq!(sim.val(w0.armed_base() + 0x230), 0x80);
        assert_eq!(sim.val(w0.armed_base() + 0x22c), 0xcf);
        assert_eq!(*sim.updates.borrow(), alloc::vec![CORE.user_base(), w0.user_base()]);
    }

    /// (B) The gate catches an ASSEMBLY that would change the mode (say
    /// the channel came up with SOR 1 detached): the difference is
    /// reported and must stop the UPDATE.
    #[test]
    fn gate_reports_a_mode_change() {
        let sim = Sim::new();
        sim.set(CORE.armed_base() + 0x320, 0x901);
        sim.set(CORE.user_base() + 0x320, 0);
        sim.set(CORE.armed_base() + 0x200, 0x9155_9218);
        assert_eq!(gate(&sim, CORE, &[0x200, 0x320], &CORE_PUSHED), alloc::vec![(0x320, 0, 0x901)]);
    }

    /// (A) A stalled channel (GET never follows) is reported, not waited
    /// on forever.
    #[test]
    fn stalled_kick_is_bounded() {
        let m = crate::mmio::testing::TableMmio::new(&[(0x68_0004, 0)]);
        struct Nop;
        impl PushMem for Nop {
            fn bus_addr(&self) -> u64 {
                0
            }
            fn len_words(&self) -> usize {
                1024
            }
            fn write(&self, _: usize, _: u32) {}
            fn flush(&self) {}
        }
        let mut p = Push::new(&Nop, 0);
        push_core_update(&mut p).unwrap();
        assert_eq!(kick(&m, CORE, &p), Err(ChanError::Stalled { put: 24, get: 0 }));
    }

    /// (A) The firmware's resting `0x80` in the exception slot (boot #70)
    /// does not fail a kick, and is not a new fault.
    #[test]
    fn resting_exception_slot_is_not_a_fault() {
        let sim = Sim::new();
        sim.set(CORE.exception(), 0x80);
        let mem = Mem { sim: &sim, base: CORE.user_base(), bus: 0x1000 };
        init_channel(&sim, CORE, &mem).unwrap();
        let before = Faults::read(&sim);
        let mut p = Push::new(&mem, 0);
        push_core_update(&mut p).unwrap();
        assert_eq!(kick(&sim, CORE, &p), Ok(()));
        assert!(!Faults::read(&sim).new_since(&before).bad());
        // Window 0's slot after its bring-up (boot #72): still no fault.
        let after = Faults { wndw0_exc: 0x178, ..before };
        assert!(!after.new_since(&before).bad());
        assert!(Faults { exc_win: 1, ..before }.new_since(&before).bad());
    }

    /// Window 0 up on the simulator, its push at `at` bytes.
    fn window0_up<'a>(sim: &'a Sim, wm: &'a Mem<'a>) {
        let w0 = window(0);
        init_channel(sim, w0, wm).unwrap();
        sim.set(w0.armed_base() + WNDW_SET_OFFSET0, 0);
    }

    /// (A) A flip pushes nouveau's surface methods (present control 1, the
    /// offset >> 8, UPDATE with RELEASE_ELV), and once fetched the ARMED
    /// offset is the new one.
    #[test]
    fn flip_pushes_offset_and_update() {
        let sim = Sim::new();
        let w0 = window(0);
        let wm = Mem { sim: &sim, base: w0.user_base(), bus: 0x10_1000 };
        window0_up(&sim, &wm);
        let mut p = Push::new(&wm, 0x94);
        sim.set(w0.get(), 0x94);
        let origin = flip(&sim, w0, &mut p, 0x0100_0000).unwrap();
        assert_eq!(origin, 0x1_0000);
        assert_eq!(
            &sim.push.borrow()[&w0.user_base()][0x94 / 4..0x94 / 4 + 6],
            &[0x40308, 1, 0x40260, 0x1_0000, 0x40200, 1]
        );
        assert!(flip_latched(&sim, w0, &p, origin));
        assert_eq!(sim.val(w0.armed_base() + WNDW_SET_OFFSET0), 0x1_0000);
        assert!(!flip_latched(&sim, w0, &p, 0x2_0000));
    }

    /// (A) A second flip before the channel fetched the first is refused
    /// and pushes nothing; a misaligned surface is refused.
    #[test]
    fn flip_refuses_busy_and_misaligned() {
        let sim = Sim::new();
        let w0 = window(0);
        let wm = Mem { sim: &sim, base: w0.user_base(), bus: 0x10_1000 };
        window0_up(&sim, &wm);
        let mut p = Push::new(&wm, 0);
        assert_eq!(flip(&sim, w0, &mut p, 0x80), Err(FlipError::Misaligned { vram: 0x80 }));
        sim.stall.set(true);
        let origin = flip(&sim, w0, &mut p, 0x0100_0000).unwrap();
        assert!(!flip_latched(&sim, w0, &p, origin));
        let put = p.put_bytes();
        assert_eq!(flip(&sim, w0, &mut p, 0x0200_0000), Err(FlipError::Busy { put, get: 0 }));
        assert_eq!(p.put_bytes(), put);
        sim.stall.set(false);
        sim.set(w0.put(), 0);
        sim.wr32(w0.put(), put); // the channel catches up
        assert!(flip_latched(&sim, w0, &p, origin));
    }

    /// (B) Hundreds of flips run past the end of the 4 KiB buffer: each
    /// wrap is a JUMP to 0 followed by PUT = 0, and every flip latches its
    /// own offset, alternating like a double buffer.
    #[test]
    fn flips_wrap_the_push_buffer() {
        let sim = Sim::new();
        let w0 = window(0);
        let wm = Mem { sim: &sim, base: w0.user_base(), bus: 0x10_1000 };
        window0_up(&sim, &wm);
        let start = 0x94;
        sim.set(w0.get(), start);
        let mut p = Push::new(&wm, start);
        let mut wraps = 0;
        for i in 0..700u64 {
            let before = p.put_bytes();
            let vram = 0x0100_0000 * (1 + i % 2);
            let origin = flip(&sim, w0, &mut p, vram).unwrap();
            if p.put_bytes() < before {
                wraps += 1;
                assert_eq!(sim.push.borrow()[&w0.user_base()][before as usize / 4], jump_header(0));
            }
            assert!(flip_latched(&sim, w0, &p, origin), "flip {i}");
            assert!(p.put_bytes() as usize <= 4096 - 4);
        }
        assert_eq!(wraps, 4);
        // The wrap's PUT = 0 is visible in the write log.
        assert!(sim.writes.borrow().iter().any(|&(o, v)| o == w0.put() && v == 0));
    }

    #[test]
    fn faults_and_pri_errors() {
        let f = Faults { ctrl_disp: 0x100, disp_intr: 0x1, ..Default::default() };
        assert!(!f.bad());
        let g = Faults { ctrl_disp: 0x101, ..Default::default() };
        assert!(g.supervisor() && g.bad());
        assert_eq!(g.new_since(&f).ctrl_disp, 0x1);
        assert!(is_pri_error(0xbadf_5040) && !is_pri_error(0xbadf_1100));
    }

    #[test]
    fn peek_reads_only_the_display_control_block_and_the_channel_areas() {
        // allowed: the cursor channel's control and status, the interrupt mask, the core's ARMED head usage bounds, a cursor user region
        for ok in [0x61_0604, 0x61_0784, 0x61_1dac, 0x61_138c, 0x68_8000 + 0x2030, 0x6d_8008] {
            assert!(peek_allowed(ok), "{ok:#x}");
        }
        // refused: unaligned, PRAMIN, the other BAR0 engines, and the edges just outside
        for bad in [0x61_0605, 0x70_0000, 0x10_0000, 0x60_fffc, 0x61_2000, 0x63_fffc, 0x6e_0000] {
            assert!(!peek_allowed(bad), "{bad:#x}");
        }
        // the edges inside
        assert!(peek_allowed(0x61_0000) && peek_allowed(0x61_1ffc) && peek_allowed(0x64_0000) && peek_allowed(0x6d_fffc));
    }
}
