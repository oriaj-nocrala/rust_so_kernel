//! Realtek RTL8111/8168/8411 ("r8169 family", PCI `10ec:8168`) Gigabit
//! Ethernet: register map, descriptor format and the init/RX/TX logic, as
//! pure code over two seams — [`Regs`] (the MMIO window) and [`DmaMem`] (the
//! descriptor rings and packet buffers the device reads and writes). The
//! kernel adapter (`kernel/src/network/rtl8168.rs`) supplies the real ones;
//! the tests below supply a software model of the device.
//!
//! **Provenance and confidence.** There is no datasheet in the repository
//! and QEMU does not emulate this chip, so every constant here is from the
//! Linux `r8169` driver as remembered, not read from a document. What is
//! verified: the host tests (which check this code against *itself* — the
//! model encodes the same understanding) and, once someone runs it, the
//! real board. What is deliberately **not** done: the per-chip `hw_start`
//! sequences (EPHY/ERI/OCP/CSI writes) and PHY firmware/parameter tables
//! that Linux applies per `XID`. The driver takes the generic path that
//! other small drivers use (reset, rings, RxConfig/TxConfig, autoneg) and
//! reports how far it got. See `docs/net/rtl8168.md` for the bring-up
//! ladder and the list of unknowns.
//!
//! References are to Linux `drivers/net/ethernet/realtek/r8169_main.c`.

// ── Registers (`enum rtl_registers`) ────────────────────────────────────────

/// `MAC0..MAC5`: the station address (read-only without unlocking `Cfg9346`).
pub const MAC0: usize = 0x00;
/// `MAR0..MAR7`: the 64-bit multicast hash filter.
pub const MAR0: usize = 0x08;
pub const TX_DESC_START_LO: usize = 0x20;
pub const TX_DESC_START_HI: usize = 0x24;
/// High-priority TX queue (unused, but its ring registers are initialised).
pub const TX_HDESC_START_LO: usize = 0x28;
pub const TX_HDESC_START_HI: usize = 0x2C;
pub const CHIP_CMD: usize = 0x37;
pub const TX_POLL: usize = 0x38;
pub const INTR_MASK: usize = 0x3C;
pub const INTR_STATUS: usize = 0x3E;
pub const TX_CONFIG: usize = 0x40;
pub const RX_CONFIG: usize = 0x44;
pub const CFG9346: usize = 0x50;
pub const CONFIG1: usize = 0x52;
pub const CONFIG2: usize = 0x53;
pub const CONFIG3: usize = 0x54;
pub const CONFIG4: usize = 0x55;
pub const CONFIG5: usize = 0x56;
pub const PHYAR: usize = 0x60;
pub const PHY_STATUS: usize = 0x6C;
pub const RX_MAX_SIZE: usize = 0xDA;
pub const C_PLUS_CMD: usize = 0xE0;
pub const INTR_MITIGATE: usize = 0xE2;
pub const RX_DESC_START_LO: usize = 0xE4;
pub const RX_DESC_START_HI: usize = 0xE8;
pub const MAX_TX_PACKET_SIZE: usize = 0xEC;
/// `EarlySize` for `MaxTxPacketSize` (units of 128 bytes): what Linux's `r8169` leaves in the register on this
/// chip (`ethtool -d` of the RTL8168H on the AM4 board shows 0x27).
pub const EARLY_SIZE: u8 = 0x27;

/// Size of the register window the driver maps (`ethtool -d` dumps this much).
pub const REG_WINDOW: usize = 0x100;

/// `OCPDR`: the MAC OCP window (`r8168_mac_ocp_read`/`write`): one register,
/// the address in bits 30:16 (byte address / 2), `OCPAR_FLAG` for a write.
pub const OCPDR: usize = 0xB0;
pub const OCPAR_FLAG: u32 = 1 << 31;

/// The extended register interface (ERI): data at `ERIDR`, command at `ERIAR`.
pub const ERIDR: usize = 0x70;
pub const ERIAR: usize = 0x74;
pub const ERIAR_FLAG: u32 = 1 << 31;
pub const ERIAR_MASK_0001: u32 = 0x1 << 12;
pub const ERIAR_MASK_0011: u32 = 0x3 << 12;
pub const ERIAR_MASK_1111: u32 = 0xf << 12;
/// `DLLPR` / `MISC_1` / `MISC` (Linux names): power-feature bits and the RX gate.
pub const DLLPR: usize = 0xD0;
pub const DLLPR_PFM_EN: u8 = 1 << 6;
pub const DLLPR_TX_10M_PS_EN: u8 = 1 << 7;
pub const MISC: usize = 0xF0;
/// `RXDV_GATED_EN`: while set, the MAC drops everything the PHY delivers. A
/// reset leaves it set on the 8168g/h; Linux clears it in `rtl_hw_start_8168h_1`.
pub const MISC_RXDV_GATED_EN: u32 = 1 << 19;
pub const MISC_1: usize = 0xF2;
pub const MISC_1_PFM_D3COLD_EN: u8 = 1 << 6;

// `ChipCmd` bits.
pub const CMD_RESET: u8 = 0x10;
pub const CMD_RX_ENB: u8 = 0x08;
pub const CMD_TX_ENB: u8 = 0x04;
// `TxPoll` bits: tell the chip the normal-priority queue has work.
pub const TXPOLL_NPQ: u8 = 0x40;
// `Cfg9346`: configuration registers are read-only until unlocked.
pub const CFG9346_LOCK: u8 = 0x00;
pub const CFG9346_UNLOCK: u8 = 0xC0;

// `CPlusCmd` bits.
pub const CPCMD_RX_CHKSUM: u16 = 1 << 5;
/// 64-bit descriptor addresses allowed ("dual address cycle").
pub const CPCMD_PCIDAC: u16 = 1 << 4;
pub const CPCMD_PCI_MUL_RW: u16 = 1 << 3;
/// Strip the VLAN tag / VLAN offload: off, frames arrive as they are.
pub const CPCMD_RX_VLAN: u16 = 1 << 6;

// `IntrStatus`/`IntrMask` bits.
pub const INT_RX_OK: u16 = 0x0001;
pub const INT_RX_ERR: u16 = 0x0002;
pub const INT_TX_OK: u16 = 0x0004;
pub const INT_TX_ERR: u16 = 0x0008;
pub const INT_RX_OVERFLOW: u16 = 0x0010;
pub const INT_LINK_CHG: u16 = 0x0020;
pub const INT_RX_FIFO_OVER: u16 = 0x0040;
pub const INT_TX_DESC_UNAVAIL: u16 = 0x0080;
pub const INT_SW: u16 = 0x0100;
pub const INT_PCS_TIMEOUT: u16 = 0x4000;
pub const INT_SYS_ERR: u16 = 0x8000;
/// What an interrupt-driven driver unmasks: Linux's `0x2f` (RxOK, RxErr,
/// TxOK, TxErr, LinkChg) plus the two ways the chip says it lost frames.
pub const IRQ_MASK: u16 = INT_RX_OK | INT_RX_ERR | INT_TX_OK | INT_TX_ERR | INT_LINK_CHG | INT_RX_OVERFLOW | INT_RX_FIFO_OVER;

// `RxConfig` (`rtl_init_rxcfg` for the 8168 family).
pub const RXCFG_ACCEPT_ERR: u32 = 0x20;
pub const RXCFG_ACCEPT_RUNT: u32 = 0x10;
pub const RXCFG_ACCEPT_BROADCAST: u32 = 0x08;
pub const RXCFG_ACCEPT_MULTICAST: u32 = 0x04;
pub const RXCFG_ACCEPT_MY_PHYS: u32 = 0x02;
pub const RXCFG_ACCEPT_ALL_PHYS: u32 = 0x01;
/// `RX_DMA_BURST`: unlimited burst (7 << 8).
pub const RXCFG_DMA_BURST: u32 = 7 << 8;
/// `RX_EARLY_OFF`.
pub const RXCFG_EARLY_OFF: u32 = 1 << 11;
/// `RX128_INT_EN`.
pub const RXCFG_128_INT_EN: u32 = 1 << 15;
/// `RX_MULTI_EN`.
pub const RXCFG_MULTI_EN: u32 = 1 << 14;

// `TxConfig`.
/// `TX_DMA_BURST` (7 << 8, unlimited) and the inter-frame gap (3 << 24).
pub const TXCFG_DMA_BURST: u32 = 7 << 8;
pub const TXCFG_IFG: u32 = 3 << 24;
pub const TXCFG_AUTO_FIFO: u32 = 1 << 7;
/// Bits 30:20 of `TxConfig` carry the hardware revision, "XID".
pub const TXCFG_XID_SHIFT: u32 = 20;

// `PHYstatus`.
pub const PHYST_FULL_DUP: u8 = 0x01;
pub const PHYST_LINK: u8 = 0x02;
pub const PHYST_10: u8 = 0x04;
pub const PHYST_100: u8 = 0x08;
pub const PHYST_1000: u8 = 0x10;

// `PHYAR`: the MDIO access register.
pub const PHYAR_FLAG: u32 = 1 << 31;

// MII registers and bits (`linux/mii.h`).
pub const MII_BMCR: u8 = 0;
pub const MII_BMSR: u8 = 1;
pub const MII_ADVERTISE: u8 = 4;
pub const MII_CTRL1000: u8 = 9;
pub const BMCR_RESET: u16 = 0x8000;
pub const BMCR_ANENABLE: u16 = 0x1000;
pub const BMCR_ANRESTART: u16 = 0x0200;
pub const BMSR_LSTATUS: u16 = 0x0004;
pub const BMSR_ANEGCOMPLETE: u16 = 0x0020;
/// Advertise 10/100 half/full and 802.3 (`ADVERTISE_ALL | ADVERTISE_CSMA`).
pub const ADVERTISE_ALL: u16 = 0x01E1;
/// Advertise 1000BASE-T half/full (`ADVERTISE_1000FULL | ADVERTISE_1000HALF`).
pub const ADVERTISE_1000: u16 = 0x0300;

// ── Descriptors (`struct RxDesc`/`TxDesc`, 16 bytes) ────────────────────────

pub const DESC_SIZE: usize = 16;
/// The chip owns the descriptor (it will read it, or fill it).
pub const DESC_OWN: u32 = 1 << 31;
/// Last descriptor of the ring: the chip wraps to the start after it.
pub const DESC_RING_END: u32 = 1 << 30;
pub const DESC_FIRST_FRAG: u32 = 1 << 29;
pub const DESC_LAST_FRAG: u32 = 1 << 28;
/// RX status bits in `opts1`: the frame is bad.
pub const RX_RES: u32 = 1 << 21;
pub const RX_RUNT: u32 = 1 << 20;
pub const RX_CRC: u32 = 1 << 19;
/// Length field of `opts1` (RX: the frame including its 4-byte FCS).
pub const DESC_LEN_MASK: u32 = 0x3FFF;
/// The FCS the chip leaves on received frames.
pub const FCS_LEN: usize = 4;

/// What the chip wrote into an RX descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxStatus {
    /// Still the chip's.
    Owned,
    /// A whole good frame of this many bytes (FCS stripped).
    Frame(usize),
    /// A bad or fragmented frame to drop.
    Bad,
}

/// Interprets an RX descriptor's `opts1`.
pub fn rx_status(opts1: u32) -> RxStatus {
    if opts1 & DESC_OWN != 0 {
        return RxStatus::Owned;
    }
    let whole = opts1 & (DESC_FIRST_FRAG | DESC_LAST_FRAG) == (DESC_FIRST_FRAG | DESC_LAST_FRAG);
    let len = (opts1 & DESC_LEN_MASK) as usize;
    if !whole || opts1 & (RX_RES | RX_RUNT | RX_CRC) != 0 || len < FCS_LEN {
        return RxStatus::Bad;
    }
    RxStatus::Frame(len - FCS_LEN)
}

/// `opts1` that hands an RX buffer of `buf_len` bytes to the chip.
pub fn rx_arm(buf_len: usize, last: bool) -> u32 {
    DESC_OWN | if last { DESC_RING_END } else { 0 } | (buf_len as u32 & DESC_LEN_MASK)
}

/// `opts1` that hands a TX frame of `len` bytes to the chip.
pub fn tx_arm(len: usize, last: bool) -> u32 {
    DESC_OWN | DESC_FIRST_FRAG | DESC_LAST_FRAG | if last { DESC_RING_END } else { 0 } | (len as u32 & DESC_LEN_MASK)
}

/// Shortest frame without its FCS (the chip appends the FCS, but does not
/// pad: short frames are padded here).
pub const MIN_FRAME: usize = 60;

// ── Hardware revision ───────────────────────────────────────────────────────

/// What the `XID` in `TxConfig` says about the chip. Only used to log and to
/// tell the user what to compare against Linux's `dmesg`; the init path is
/// the same for all of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// RTL8168H / RTL8111H (Linux `RTL_GIGA_MAC_VER_45`/`46`): `0x540`, `0x541`.
    Rtl8168H,
    /// RTL8168G / RTL8111G (`VER_40`..`42`): `0x4c0`, `0x4c1`, `0x509`.
    Rtl8168G,
    /// Anything else with an `XID` this table does not know.
    Unknown,
}

/// The `XID` (11 bits) of a `TxConfig` value, masked like Linux does (`0x7cf`).
/// Linux's `ether_crc`: CRC-32 (poly 0x04c11db7), bits taken LSB first, no
/// final inversion. The chip's multicast hash is its top 6 bits.
pub fn ether_crc(addr: &[u8; 6]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in addr {
        let mut data = byte;
        for _ in 0..8 {
            let carry = (crc >> 31) as u8 ^ (data & 1) != 0;
            crc <<= 1;
            if carry {
                crc ^= 0x04C1_1DB7;
            }
            data >>= 1;
        }
    }
    crc
}

/// The `MAR0`/`MAR0+4` words (`rtl_set_rx_mode`, 8168 family: the two halves
/// swapped and byte-reversed) that pass exactly the groups in `groups`.
pub fn multicast_filter(groups: &[[u8; 6]]) -> [u32; 2] {
    let mut f = [0u32; 2];
    for g in groups {
        let bit = ether_crc(g) >> 26;
        f[(bit >> 5) as usize] |= 1 << (bit & 31);
    }
    [f[1].swap_bytes(), f[0].swap_bytes()]
}

pub fn xid(tx_config: u32) -> u16 {
    ((tx_config >> TXCFG_XID_SHIFT) & 0x7CF) as u16
}

pub fn family(xid: u16) -> Family {
    match xid {
        0x540 | 0x541 => Family::Rtl8168H,
        0x4C0 | 0x4C1 | 0x509 => Family::Rtl8168G,
        _ => Family::Unknown,
    }
}

// ── Seams ───────────────────────────────────────────────────────────────────

/// The register window, as the device sees it. Production: volatile MMIO.
pub trait Regs {
    fn r8(&self, off: usize) -> u8;
    fn r16(&self, off: usize) -> u16;
    fn r32(&self, off: usize) -> u32;
    fn w8(&self, off: usize, val: u8);
    fn w16(&self, off: usize, val: u16);
    fn w32(&self, off: usize, val: u32);
}

/// One DMA arena (descriptor rings and packet buffers) addressed by byte
/// offset; `bus_addr(0)` is what the device is told. Production: a `DmaBuf`
/// with volatile accesses.
pub trait DmaMem {
    fn read(&self, off: usize, buf: &mut [u8]);
    fn write(&self, off: usize, data: &[u8]);
    fn bus_addr(&self, off: usize) -> u64;
}

// ── Layout of the arena ─────────────────────────────────────────────────────

/// Slots per ring, and bytes per packet buffer.
pub const SLOTS: usize = 64;
pub const BUF_SIZE: usize = 2048;
const TX_RING_OFF: usize = 0;
const RX_RING_OFF: usize = SLOTS * DESC_SIZE;
const TX_BUFS_OFF: usize = 4096;
const RX_BUFS_OFF: usize = TX_BUFS_OFF + SLOTS * BUF_SIZE;
/// Bytes the arena needs (the adapter rounds up to a power of two).
pub const ARENA_BYTES: usize = RX_BUFS_OFF + SLOTS * BUF_SIZE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitError {
    /// `ChipCmd` still shows the reset bit after the bounded wait.
    ResetTimeout,
    /// An all-ones read: the device is not answering (powered down, or
    /// the BAR is not decoded).
    NoDevice,
}

/// Writes ` tcp 1.2.3.4:80 > 5.6.7.8:1234 SYN|ACK seq 1 ack 2 win 3` for an
/// IPv4 TCP frame (or ` icmp type N` / ` udp a:p > b:p`); `false` (nothing
/// written) when the note is another protocol or too short to say.
fn write_decoded(out: &mut impl core::fmt::Write, n: &FrameNote) -> Result<bool, core::fmt::Error> {
    let h = &n.head;
    if (n.len as usize) < 34 || h[12] != 0x08 || h[13] != 0x00 || h[14] >> 4 != 4 {
        return Ok(false);
    }
    let ip = |o: usize| (h[o], h[o + 1], h[o + 2], h[o + 3]);
    let (s, d) = (ip(26), ip(30));
    let be16 = |o: usize| u16::from_be_bytes([h[o], h[o + 1]]);
    let be32 = |o: usize| u32::from_be_bytes([h[o], h[o + 1], h[o + 2], h[o + 3]]);
    match h[23] {
        6 if (n.len as usize) >= 54 && h[14] & 0xF == 5 => {
            let flags = h[47];
            write!(out, " tcp {}.{}.{}.{}:{} > {}.{}.{}.{}:{} ", s.0, s.1, s.2, s.3, be16(34), d.0, d.1, d.2, d.3, be16(36))?;
            let mut any = false;
            for (bit, name) in [(0x02, "SYN"), (0x10, "ACK"), (0x08, "PSH"), (0x01, "FIN"), (0x04, "RST")] {
                if flags & bit != 0 {
                    write!(out, "{}{}", if any { "|" } else { "" }, name)?;
                    any = true;
                }
            }
            write!(out, " seq {} ack {} win {}", be32(38), be32(42), be16(48))?;
            Ok(true)
        }
        6 => {
            write!(out, " tcp {}.{}.{}.{}:{} > {}.{}.{}.{}:{} (options)", s.0, s.1, s.2, s.3, be16(34), d.0, d.1, d.2, d.3, be16(36))?;
            Ok(true)
        }
        17 => {
            write!(out, " udp {}.{}.{}.{}:{} > {}.{}.{}.{}:{}", s.0, s.1, s.2, s.3, be16(34), d.0, d.1, d.2, d.3, be16(36))?;
            Ok(true)
        }
        1 => {
            write!(out, " icmp {}.{}.{}.{} > {}.{}.{}.{} type {}", s.0, s.1, s.2, s.3, d.0, d.1, d.2, d.3, h[34])?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// What the chip reports about the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Link {
    pub up: bool,
    pub mbps: u16,
    pub full_duplex: bool,
}

pub fn link_from_phy_status(st: u8) -> Link {
    let mbps = if st & PHYST_1000 != 0 {
        1000
    } else if st & PHYST_100 != 0 {
        100
    } else if st & PHYST_10 != 0 {
        10
    } else {
        0
    };
    Link { up: st & PHYST_LINK != 0, mbps, full_duplex: st & PHYST_FULL_DUP != 0 }
}

// ── The driver ──────────────────────────────────────────────────────────────

/// Bytes of a frame kept in the diagnostic log, and how many frames per direction.
/// Enough for Ethernet + IPv4 + TCP headers without options (14 + 20 + 20).
pub const NOTE_BYTES: usize = 54;
pub const NOTES: usize = 16;

/// The start of a frame seen by the driver (diagnostics: `/proc/nic`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameNote {
    pub len: u16,
    pub head: [u8; NOTE_BYTES],
}

impl FrameNote {
    fn of(frame: &[u8]) -> FrameNote {
        let mut head = [0u8; NOTE_BYTES];
        let n = frame.len().min(NOTE_BYTES);
        head[..n].copy_from_slice(&frame[..n]);
        FrameNote { len: frame.len() as u16, head }
    }
}

pub struct Rtl8168<R: Regs, D: DmaMem> {
    regs: R,
    dma: D,
    tx_head: usize,
    tx_tail: usize,
    rx_next: usize,
    pub rx_frames: u64,
    pub rx_dropped: u64,
    pub tx_frames: u64,
    pub tx_dropped: u64,
    /// Every `IntrStatus` bit seen set since init (it is acknowledged, not acted on).
    pub intr_seen: u16,
    /// The last frames sent and received, oldest overwritten (index = count % NOTES).
    pub tx_notes: [FrameNote; NOTES],
    pub rx_notes: [FrameNote; NOTES],
    /// Link state as of the last `poll_link` (set by `init_rings`).
    link_was_up: bool,
    /// Link transitions seen by `poll_link`.
    pub link_changes: u32,
}

impl<R: Regs, D: DmaMem> Rtl8168<R, D> {
    pub fn new(regs: R, dma: D) -> Self {
        Rtl8168 {
            regs,
            dma,
            tx_head: 0,
            tx_tail: 0,
            rx_next: 0,
            rx_frames: 0,
            rx_dropped: 0,
            tx_frames: 0,
            tx_dropped: 0,
            intr_seen: 0,
            tx_notes: [FrameNote { len: 0, head: [0; NOTE_BYTES] }; NOTES],
            rx_notes: [FrameNote { len: 0, head: [0; NOTE_BYTES] }; NOTES],
            link_was_up: false,
            link_changes: 0,
        }
    }

    pub fn regs(&self) -> &R {
        &self.regs
    }

    /// Read-only identification: no register is written. `None` when the
    /// window reads as all ones.
    pub fn identify(&self) -> Option<Identity> {
        let tx_config = self.regs.r32(TX_CONFIG);
        if tx_config == 0xFFFF_FFFF {
            return None;
        }
        let mut mac = [0u8; 6];
        for (i, b) in mac.iter_mut().enumerate() {
            *b = self.regs.r8(MAC0 + i);
        }
        let x = xid(tx_config);
        Some(Identity { tx_config, xid: x, family: family(x), mac, link: link_from_phy_status(self.regs.r8(PHY_STATUS)) })
    }

    /// `Some(up)` when the PHY's link state differs from the last call (or
    /// from `init_rings`): a cable pulled or plugged, a switch rebooted. Reads
    /// `PHYstatus` only, so polled operation sees it as well as `LinkChg`.
    pub fn poll_link(&mut self) -> Option<bool> {
        let up = self.link().up;
        if up == self.link_was_up {
            return None;
        }
        self.link_was_up = up;
        self.link_changes += 1;
        Some(up)
    }

    pub fn link(&self) -> Link {
        link_from_phy_status(self.regs.r8(PHY_STATUS))
    }

    /// `PHY page select` (register 0x1f), a paged read, write and modify, as
    /// Linux's `phy_{read,write,modify}_paged` (the page goes back to 0).
    fn phy_page_read(&self, page: u16, reg: u8, relax: &mut impl FnMut()) -> Option<u16> {
        self.phy_write(0x1f, page, &mut *relax).then_some(())?;
        let v = self.phy_read(reg, &mut *relax);
        self.phy_write(0x1f, 0, &mut *relax).then_some(())?;
        v
    }

    fn phy_page_write(&self, page: u16, reg: u8, value: u16, relax: &mut impl FnMut()) -> Option<()> {
        self.phy_write(0x1f, page, &mut *relax).then_some(())?;
        let ok = self.phy_write(reg, value, &mut *relax);
        self.phy_write(0x1f, 0, &mut *relax).then_some(())?;
        ok.then_some(())
    }

    fn phy_page_modify(&self, page: u16, reg: u8, clear: u16, set: u16, relax: &mut impl FnMut()) -> Option<()> {
        let v = self.phy_page_read(page, reg, relax)?;
        self.phy_page_write(page, reg, (v & !clear) | set, relax)
    }

    /// `r8168g_phy_param`: an indirect PHY parameter (page 0xa43, regs 0x13/0x14).
    fn phy_param(&self, param: u16, clear: u16, set: u16, relax: &mut impl FnMut()) -> Option<()> {
        self.phy_write(0x1f, 0x0a43, &mut *relax).then_some(())?;
        let r = (|| {
            self.phy_write(0x13, param, &mut *relax).then_some(())?;
            let v = self.phy_read(0x14, &mut *relax)?;
            self.phy_write(0x14, (v & !clear) | set, &mut *relax).then_some(())
        })();
        self.phy_write(0x1f, 0, &mut *relax).then_some(())?;
        r
    }

    /// `r8168_mac_ocp_read` / `write`.
    fn mac_ocp_read(&self, reg: u16) -> u16 {
        self.regs.w32(OCPDR, (reg as u32) << 15);
        self.regs.r32(OCPDR) as u16
    }

    fn mac_ocp_write(&self, reg: u16, data: u16) {
        self.regs.w32(OCPDR, OCPAR_FLAG | (reg as u32) << 15 | data as u32);
    }

    /// Linux's `rtl8168h_2_hw_phy_config` for this chip (XID 0x541, VER_46),
    /// without the PHY firmware patch (`rtl8168h-2.fw`) it applies first:
    /// channel-estimation and R-tune parameters, the ADC bias offset and TX
    /// LPF level read from the MAC, and the power-saving features off
    /// (PFM, 10M PLL off, ALDPS), EEE on. Linux runs it before the link comes
    /// up; without it the board's PHY dropped gigabit link ~13 s after boot
    /// and came back at 100 Mb/s. `false` on an MDIO timeout.
    pub fn phy_config_8168h(&self, mut relax: impl FnMut()) -> bool {
        let r = &mut relax;
        let mut run = || -> Option<()> {
            self.phy_param(0x808a, 0x003f, 0x000a, r)?; // CHIN EST parameter update
            self.phy_param(0x0811, 0x0000, 0x0800, r)?; // enable R-tune and PGA-retune
            self.phy_page_modify(0x0a42, 0x16, 0x0000, 0x0002, r)?;
            self.phy_page_modify(0x0a44, 0x11, 0x0000, 1 << 11, r)?; // enable gphy 10M
            // ADC bias offset from the MAC OCP.
            self.mac_ocp_write(0xdd02, 0x807d);
            let data1 = self.mac_ocp_read(0xdd02);
            let data2 = self.mac_ocp_read(0xdd00);
            let mut ioffset = (data2 >> 1) & 0x7ff8 | data2 & 0x0007;
            if data1 & (1 << 7) != 0 {
                ioffset |= 1 << 15;
            }
            if ioffset != 0xffff {
                self.phy_page_write(0x0bcf, 0x16, ioffset, r)?;
            }
            // TX LPF corner frequency level.
            let level = self.phy_page_read(0x0bcd, 0x16, r)? & 0x000f;
            let rlen = if level > 3 { level - 3 } else { 0 };
            self.phy_page_write(0x0bcd, 0x17, rlen | rlen << 4 | rlen << 8 | rlen << 12, r)?;
            self.phy_page_modify(0x0a44, 0x11, 1 << 7, 0, r)?; // disable PHY PFM mode
            self.phy_page_modify(0x0a43, 0x10, 1 << 0, 0, r)?; // disable 10M PLL off
            self.phy_page_modify(0x0a43, 0x10, 1 << 2, 0, r)?; // disable ALDPS
            self.phy_page_modify(0x0a43, 0x11, 0, 1 << 4, r) // EEE
        };
        run().is_some()
    }

    /// Software reset: `ChipCmd.Reset`, then wait for it to clear. `relax`
    /// is called between polls (the adapter spins, answering TLB shootdowns).
    pub fn reset(&self, mut relax: impl FnMut()) -> Result<(), InitError> {
        if self.regs.r32(TX_CONFIG) == 0xFFFF_FFFF {
            return Err(InitError::NoDevice);
        }
        self.regs.w8(CHIP_CMD, CMD_RESET);
        for _ in 0..1_000_000u32 {
            if self.regs.r8(CHIP_CMD) & CMD_RESET == 0 {
                return Ok(());
            }
            relax();
        }
        Err(InitError::ResetTimeout)
    }

    /// An MDIO read of PHY register `reg` (`rtl_readphy`): write the address,
    /// the chip sets `PHYAR.Flag` when the data is in. `None` on timeout.
    pub fn phy_read(&self, reg: u8, mut relax: impl FnMut()) -> Option<u16> {
        self.regs.w32(PHYAR, (reg as u32 & 0x1F) << 16);
        for _ in 0..100_000u32 {
            let v = self.regs.r32(PHYAR);
            if v & PHYAR_FLAG != 0 {
                return Some(v as u16);
            }
            relax();
        }
        None
    }

    /// An MDIO write (`rtl_writephy`): the chip clears `PHYAR.Flag` when done.
    pub fn phy_write(&self, reg: u8, value: u16, mut relax: impl FnMut()) -> bool {
        self.regs.w32(PHYAR, PHYAR_FLAG | (reg as u32 & 0x1F) << 16 | value as u32);
        for _ in 0..100_000u32 {
            if self.regs.r32(PHYAR) & PHYAR_FLAG == 0 {
                return true;
            }
            relax();
        }
        false
    }

    /// Restarts auto-negotiation advertising everything up to 1000BASE-T.
    pub fn phy_autoneg(&self, mut relax: impl FnMut()) -> bool {
        self.phy_write(MII_ADVERTISE, ADVERTISE_ALL, &mut relax)
            && self.phy_write(MII_CTRL1000, ADVERTISE_1000, &mut relax)
            && self.phy_write(MII_BMCR, BMCR_ANENABLE | BMCR_ANRESTART, &mut relax)
    }

    /// Brings the chip up: rings, TX/RX enabled, accepting unicast to
    /// `identify().mac` and broadcast, no interrupts. The caller has reset
    /// the chip. Programs `PCIDAC` when any address the chip will see is
    /// above 4 GiB.
    pub fn init_rings(&mut self) {
        // RX buffers handed to the chip, all descriptors written before it is told where they are.
        for i in 0..SLOTS {
            let buf = RX_BUFS_OFF + i * BUF_SIZE;
            self.write_desc(RX_RING_OFF, i, self.dma.bus_addr(buf), rx_arm(BUF_SIZE, i + 1 == SLOTS));
        }
        for i in 0..SLOTS {
            let buf = TX_BUFS_OFF + i * BUF_SIZE;
            self.write_desc(TX_RING_OFF, i, self.dma.bus_addr(buf), if i + 1 == SLOTS { DESC_RING_END } else { 0 });
        }
        self.tx_head = 0;
        self.tx_tail = 0;
        self.rx_next = 0;

        let r = &self.regs;
        let high = self.dma.bus_addr(0) + ARENA_BYTES as u64 > u32::MAX as u64 + 1;
        r.w8(CFG9346, CFG9346_UNLOCK);
        // Offloads off: plain frames in and out. PCIDAC only if a 64-bit address is in play.
        let mut cp = r.r16(C_PLUS_CMD) & !(CPCMD_RX_VLAN | CPCMD_RX_CHKSUM);
        if high {
            cp |= CPCMD_PCIDAC;
        }
        r.w16(C_PLUS_CMD, cp);
        r.w16(RX_MAX_SIZE, (BUF_SIZE - 1) as u16);
        r.w8(MAX_TX_PACKET_SIZE, EARLY_SIZE);
        if matches!(family(xid(r.r32(TX_CONFIG))), Family::Rtl8168H | Family::Rtl8168G) {
            self.mac_start_8168gh();
        }
        let tx = self.dma.bus_addr(TX_RING_OFF);
        let rx = self.dma.bus_addr(RX_RING_OFF);
        r.w32(TX_DESC_START_HI, (tx >> 32) as u32);
        r.w32(TX_DESC_START_LO, tx as u32);
        r.w32(RX_DESC_START_HI, (rx >> 32) as u32);
        r.w32(RX_DESC_START_LO, rx as u32);
        // The unused high-priority queue gets its registers cleared.
        r.w32(TX_HDESC_START_HI, 0);
        r.w32(TX_HDESC_START_LO, 0);
        // `AUTO_FIFO` is set by Linux on this family (`ethtool -d` shows TxConfig 0x57100f80).
        r.w32(TX_CONFIG, TXCFG_DMA_BURST | TXCFG_IFG | TXCFG_AUTO_FIFO);
        r.w8(CHIP_CMD, CMD_TX_ENB | CMD_RX_ENB);
        r.w32(RX_CONFIG, RXCFG_128_INT_EN | RXCFG_MULTI_EN | RXCFG_DMA_BURST | RXCFG_EARLY_OFF | RXCFG_ACCEPT_BROADCAST | RXCFG_ACCEPT_MY_PHYS);
        for i in 0..8 {
            r.w8(MAR0 + i, 0);
        }
        r.w16(INTR_MASK, 0); // polled
        r.w16(INTR_STATUS, 0xFFFF); // clear anything pending
        r.w8(CFG9346, CFG9346_LOCK);
        self.link_was_up = self.link().up;
    }

    /// One ERI write (`_rtl_eri_write`): data first, then the command, then
    /// wait for the chip to clear the flag (bounded; a stuck flag is ignored).
    fn eri_write(&self, addr: u32, mask: u32, val: u32) {
        let r = &self.regs;
        r.w32(ERIDR, val);
        r.w32(ERIAR, ERIAR_FLAG | mask | addr);
        for _ in 0..100_000u32 {
            if r.r32(ERIAR) & ERIAR_FLAG == 0 {
                break;
            }
        }
    }

    fn eri_read(&self, addr: u32) -> u32 {
        let r = &self.regs;
        r.w32(ERIAR, ERIAR_MASK_1111 | addr);
        for _ in 0..100_000u32 {
            if r.r32(ERIAR) & ERIAR_FLAG != 0 {
                return r.r32(ERIDR);
            }
        }
        !0
    }

    fn eri_modify(&self, addr: u32, set: u32, clear: u32) {
        let v = self.eri_read(addr);
        self.eri_write(addr, ERIAR_MASK_1111, (v & !clear) | set);
    }

    /// The MAC-side start-up Linux does for the 8168g/h (`rtl_hw_start_8168h_1`,
    /// minus the PHY/EPHY/OCP tuning): RX/TX FIFO sizes, pause thresholds, a
    /// packet-filter reset, and — the one that matters — opening the RXDV
    /// gate, without which no frame ever reaches the RX ring.
    fn mac_start_8168gh(&self) {
        let r = &self.regs;
        // rtl_set_fifo_size(0x08, 0x10, 0x02, 0x06); pause thresholds 0x38/0x48.
        self.eri_write(0xC8, ERIAR_MASK_1111, 0x08 << 16 | 0x02);
        self.eri_write(0xE8, ERIAR_MASK_1111, 0x10 << 16 | 0x06);
        self.eri_write(0xCC, ERIAR_MASK_0001, 0x38);
        self.eri_write(0xD0, ERIAR_MASK_0001, 0x48);
        // rtl_reset_packet_filter, then the 0xdc bits Linux sets.
        self.eri_modify(0xDC, 0, 1);
        self.eri_modify(0xDC, 1, 0);
        self.eri_modify(0xDC, 0x1C, 0);
        self.eri_write(0x5F0, ERIAR_MASK_0011, 0x4F87);
        r.w32(MISC, r.r32(MISC) & !MISC_RXDV_GATED_EN);
        self.eri_write(0xC0, ERIAR_MASK_0011, 0);
        self.eri_write(0xB8, ERIAR_MASK_0011, 0);
        r.w8(DLLPR, r.r8(DLLPR) & !(DLLPR_PFM_EN | DLLPR_TX_10M_PS_EN));
        r.w8(MISC_1, r.r8(MISC_1) & !MISC_1_PFM_D3COLD_EN);
        self.eri_modify(0x1B0, 0, 1 << 12);
    }

    fn write_desc(&self, ring: usize, i: usize, addr: u64, opts1: u32) {
        let mut d = [0u8; DESC_SIZE];
        d[8..16].copy_from_slice(&addr.to_le_bytes());
        // opts2 stays 0. opts1 (with OWN) goes last: the chip must not see
        // OWN before the rest of the descriptor.
        self.dma.write(ring + i * DESC_SIZE + 4, &d[4..16]);
        self.dma.write(ring + i * DESC_SIZE, &opts1.to_le_bytes());
    }

    fn read_opts1(&self, ring: usize, i: usize) -> u32 {
        let mut b = [0u8; 4];
        self.dma.read(ring + i * DESC_SIZE, &mut b);
        u32::from_le_bytes(b)
    }

    /// Copies the next received frame into `buf`; `None` when there is none.
    /// Bad frames are counted and skipped.
    pub fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        let mut acked = false;
        for _ in 0..SLOTS {
            let i = self.rx_next;
            let opts1 = self.read_opts1(RX_RING_OFF, i);
            match rx_status(opts1) {
                RxStatus::Owned => {
                    // Acknowledge, then look once more: a frame that landed
                    // between the ring check and the ack had its RxOK cleared
                    // with it, and would wait for the next event otherwise.
                    if !acked && self.ack_status() != 0 {
                        acked = true;
                        continue;
                    }
                    return None;
                }
                RxStatus::Frame(n) if n <= buf.len() && n <= BUF_SIZE => {
                    self.dma.read(RX_BUFS_OFF + i * BUF_SIZE, &mut buf[..n]);
                    self.rx_notes[(self.rx_frames as usize) % NOTES] = FrameNote::of(&buf[..n]);
                    self.rx_frames += 1;
                    self.rearm_rx(i);
                    return Some(n);
                }
                RxStatus::Frame(_) | RxStatus::Bad => {
                    self.rx_dropped += 1;
                    self.rearm_rx(i);
                }
            }
        }
        None
    }

    fn rearm_rx(&mut self, i: usize) {
        // Only opts1 changes: the buffer address in the descriptor is still ours.
        self.dma.write(RX_RING_OFF + i * DESC_SIZE + 4, &[0u8; 4]); // opts2
        self.dma.write(RX_RING_OFF + i * DESC_SIZE, &rx_arm(BUF_SIZE, i + 1 == SLOTS).to_le_bytes());
        self.rx_next = (i + 1) % SLOTS;
    }

    /// Queues `frame` for sending. `false` when the TX ring is full (the
    /// chip still owns every slot) or the frame does not fit a buffer.
    pub fn send(&mut self, frame: &[u8]) -> bool {
        self.reclaim_tx();
        if frame.is_empty() || frame.len() > BUF_SIZE {
            self.tx_dropped += 1;
            return false;
        }
        let i = self.tx_head;
        if (self.tx_head + 1) % SLOTS == self.tx_tail {
            self.tx_dropped += 1;
            return false; // full: one slot always stays free
        }
        let len = frame.len().max(MIN_FRAME);
        let off = TX_BUFS_OFF + i * BUF_SIZE;
        self.dma.write(off, frame);
        if len > frame.len() {
            self.dma.write(off + frame.len(), &[0u8; MIN_FRAME][..len - frame.len()]);
        }
        self.dma.write(TX_RING_OFF + i * DESC_SIZE + 4, &[0u8; 4]); // opts2
        // The chip must see the frame and the length before OWN.
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        self.dma.write(TX_RING_OFF + i * DESC_SIZE, &tx_arm(len, i + 1 == SLOTS).to_le_bytes());
        self.tx_head = (i + 1) % SLOTS;
        self.tx_notes[(self.tx_frames as usize) % NOTES] = FrameNote::of(frame);
        self.tx_frames += 1;
        self.regs.w8(TX_POLL, TXPOLL_NPQ);
        true
    }

    /// Receives frames for exactly these multicast groups (none: only
    /// broadcast and our own address), through the chip's 64-bit hash, so
    /// other groups that hash alike still get through: the stack must still
    /// ignore what is not for it.
    pub fn set_multicast(&mut self, groups: &[[u8; 6]]) {
        self.write_multicast(multicast_filter(groups), !groups.is_empty());
    }

    /// Receives every multicast frame (the hash fully open).
    pub fn accept_all_multicast(&mut self) {
        self.write_multicast([u32::MAX; 2], true);
    }

    fn write_multicast(&mut self, mar: [u32; 2], accept: bool) {
        let r = &self.regs;
        r.w32(MAR0 + 4, mar[1]);
        r.w32(MAR0, mar[0]);
        let cfg = r.r32(RX_CONFIG);
        let cfg = if accept { cfg | RXCFG_ACCEPT_MULTICAST } else { cfg & !RXCFG_ACCEPT_MULTICAST };
        r.w32(RX_CONFIG, cfg);
    }

    /// Unmasks the chip's interrupts (`IRQ_MASK`), after clearing whatever
    /// is pending. Call it once the function's MSI/MSI-X routes somewhere.
    pub fn enable_irq(&mut self) {
        self.regs.w16(INTR_STATUS, 0xFFFF);
        self.regs.w16(INTR_MASK, IRQ_MASK);
    }

    /// Masks every interrupt again (polled operation).
    pub fn disable_irq(&mut self) {
        self.regs.w16(INTR_MASK, 0);
    }

    /// Reads `IntrStatus`, remembers which bits were set and clears them
    /// (writing the bits back). With the mask at 0 nothing interrupts, but the
    /// chip still raises the flags: RxOK/TxOK say packets moved, RxOverflow and
    /// RxFIFOOver say frames were lost, SysErr says the PCIe side failed.
    pub fn ack_status(&mut self) -> u16 {
        let v = self.regs.r16(INTR_STATUS);
        if v != 0 && v != 0xFFFF {
            self.intr_seen |= v;
            self.regs.w16(INTR_STATUS, v);
        }
        v
    }

    /// Frees TX slots the chip is done with.
    fn reclaim_tx(&mut self) {
        while self.tx_tail != self.tx_head && self.read_opts1(TX_RING_OFF, self.tx_tail) & DESC_OWN == 0 {
            self.tx_tail = (self.tx_tail + 1) % SLOTS;
        }
    }

    /// A human-readable state dump for `/proc/nic`: counters, the registers
    /// that say whether the chip is moving packets, the ring cursors and the
    /// descriptors there, and the first bytes of the last frames each way.
    pub fn report(&self, out: &mut impl core::fmt::Write) -> core::fmt::Result {
        let r = &self.regs;
        writeln!(out, "rtl8168: rx {} (dropped {}) tx {} (dropped {}, in flight {}), link changes {}", self.rx_frames, self.rx_dropped, self.tx_frames, self.tx_dropped, self.tx_in_flight(), self.link_changes)?;
        writeln!(out, "regs: ChipCmd {:#04x} TxPoll {:#04x} IntrMask {:#06x} IntrStatus {:#06x}", r.r8(CHIP_CMD), r.r8(TX_POLL), r.r16(INTR_MASK), r.r16(INTR_STATUS))?;
        writeln!(out, "regs: TxConfig {:#010x} RxConfig {:#010x} CPlusCmd {:#06x} MaxTxPkt {:#04x} RxMaxSize {:#06x}", r.r32(TX_CONFIG), r.r32(RX_CONFIG), r.r16(C_PLUS_CMD), r.r8(MAX_TX_PACKET_SIZE), r.r16(RX_MAX_SIZE))?;
        writeln!(out, "regs: MISC {:#010x} (RXDV gate {}) DLLPR {:#04x}", r.r32(MISC), if r.r32(MISC) & MISC_RXDV_GATED_EN != 0 { "CLOSED" } else { "open" }, r.r8(DLLPR))?;
        writeln!(out, "regs: TxDesc {:#010x}:{:08x} RxDesc {:#010x}:{:08x} PHYstatus {:#04x} {:?}", r.r32(TX_DESC_START_HI), r.r32(TX_DESC_START_LO), r.r32(RX_DESC_START_HI), r.r32(RX_DESC_START_LO), r.r8(PHY_STATUS), link_from_phy_status(r.r8(PHY_STATUS)))?;
        let names: [(u16, &str); 11] = [
            (INT_RX_OK, "RxOK"), (INT_RX_ERR, "RxErr"), (INT_TX_OK, "TxOK"), (INT_TX_ERR, "TxErr"), (INT_RX_OVERFLOW, "RxOverflow"),
            (INT_LINK_CHG, "LinkChg"), (INT_RX_FIFO_OVER, "RxFIFOOver"), (INT_TX_DESC_UNAVAIL, "TxDescUnavail"), (INT_SW, "SWInt"),
            (INT_PCS_TIMEOUT, "PCSTimeout"), (INT_SYS_ERR, "SysErr"),
        ];
        write!(out, "events seen since init ({:#06x}):", self.intr_seen)?;
        for (bit, name) in names {
            if self.intr_seen & bit != 0 {
                write!(out, " {}", name)?;
            }
        }
        writeln!(out)?;
        writeln!(
            out,
            "tx: head {} tail {} head.opts1 {:#010x} tail.opts1 {:#010x}",
            self.tx_head, self.tx_tail, self.read_opts1(TX_RING_OFF, self.tx_head), self.read_opts1(TX_RING_OFF, self.tx_tail)
        )?;
        writeln!(out, "rx: next {} next.opts1 {:#010x} (OWN set = waiting for a frame)", self.rx_next, self.read_opts1(RX_RING_OFF, self.rx_next))?;
        for (what, notes, count) in [("tx", &self.tx_notes, self.tx_frames), ("rx", &self.rx_notes, self.rx_frames)] {
            let shown = (count as usize).min(NOTES);
            writeln!(out, "last {} {} frames (newest first):", shown, what)?;
            for k in 0..shown {
                let n = &notes[((count as usize) - 1 - k) % NOTES];
                write!(out, "  {:4} B:", n.len)?;
                if !write_decoded(out, n)? {
                    for b in &n.head[..(n.len as usize).min(24)] {
                        write!(out, " {:02x}", b)?;
                    }
                }
                writeln!(out)?;
            }
        }
        Ok(())
    }

    /// Slots in flight (diagnostics and tests).
    pub fn tx_in_flight(&self) -> usize {
        (self.tx_head + SLOTS - self.tx_tail) % SLOTS
    }
}

/// What `identify` read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub tx_config: u32,
    pub xid: u16,
    pub family: Family,
    pub mac: [u8; 6],
    pub link: Link,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::rc::Rc;
    use alloc::vec::Vec;
    use core::cell::RefCell;

    /// A software RTL8168: the register file, the two rings in the shared
    /// arena, and the behaviours the driver relies on (reset clears itself,
    /// `TxPoll` sends owned descriptors, received frames fill RX
    /// descriptors). It also records protocol violations.
    struct Model {
        regs: [u8; REG_WINDOW],
        arena: Vec<u8>,
        base: u64,
        wire: Vec<Vec<u8>>,
        violations: Vec<&'static str>,
        rx_cursor: usize,
        phy: [u16; 32],
        link_status: u8,
        reset_stuck: bool,
        eri: alloc::collections::BTreeMap<u32, u32>,
        /// PHY registers behind a non-zero page (register 0x1f).
        paged: alloc::collections::BTreeMap<(u16, usize), u16>,
        ocp: alloc::collections::BTreeMap<u16, u16>,
    }

    #[derive(Clone)]
    struct Dev(Rc<RefCell<Model>>);

    impl Dev {
        fn new(base: u64) -> Dev {
            let mut regs = [0u8; REG_WINDOW];
            regs[MAC0..MAC0 + 6].copy_from_slice(&[0x04, 0x92, 0x26, 0x01, 0x02, 0x03]);
            // XID 0x541 (RTL8168H) in TxConfig bits 30:20.
            regs[TX_CONFIG..TX_CONFIG + 4].copy_from_slice(&(0x541u32 << TXCFG_XID_SHIFT).to_le_bytes());
            Dev(Rc::new(RefCell::new(Model {
                regs,
                arena: alloc::vec![0u8; ARENA_BYTES],
                base,
                wire: Vec::new(),
                violations: Vec::new(),
                rx_cursor: 0,
                phy: [0; 32],
                link_status: PHYST_LINK | PHYST_FULL_DUP | PHYST_1000,
                reset_stuck: false,
                eri: Default::default(),
                paged: Default::default(),
                ocp: Default::default(),
            })))
        }

        fn violations(&self) -> Vec<&'static str> {
            self.0.borrow().violations.clone()
        }

        fn rd32(m: &Model, off: usize) -> u32 {
            u32::from_le_bytes(m.regs[off..off + 4].try_into().unwrap())
        }

        /// The chip receives `frame` (adds the FCS, as it leaves it on).
        fn inject(&self, frame: &[u8], extra: u32) -> bool {
            let mut m = self.0.borrow_mut();
            if Self::rd32(&m, MISC) & MISC_RXDV_GATED_EN != 0 {
                return false; // the gate drops it before it reaches a descriptor
            }
            let ring = (Self::rd32(&m, RX_DESC_START_LO) as u64 | (Self::rd32(&m, RX_DESC_START_HI) as u64) << 32) - m.base;
            let i = m.rx_cursor;
            let d = ring as usize + i * DESC_SIZE;
            let opts1 = u32::from_le_bytes(m.arena[d..d + 4].try_into().unwrap());
            if opts1 & DESC_OWN == 0 {
                return false; // no free buffer: the chip drops it
            }
            let addr = u64::from_le_bytes(m.arena[d + 8..d + 16].try_into().unwrap()) - m.base;
            let a = addr as usize;
            m.arena[a..a + frame.len()].copy_from_slice(frame);
            m.arena[a + frame.len()..a + frame.len() + 4].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // FCS
            let end = opts1 & DESC_RING_END;
            let new = end | DESC_FIRST_FRAG | DESC_LAST_FRAG | extra | (frame.len() + 4) as u32;
            m.arena[d..d + 4].copy_from_slice(&new.to_le_bytes());
            m.rx_cursor = (i + 1) % SLOTS;
            true
        }
    }

    impl Model {
        fn check_config_write(&mut self, off: usize) {
            // MAC address and Config registers are read-only unless `Cfg9346` is unlocked.
            let locked = self.regs[CFG9346] & 0xC0 != 0xC0;
            if locked && ((MAC0..MAC0 + 8).contains(&off) || (CONFIG1..=CONFIG5).contains(&off) || off == C_PLUS_CMD) {
                self.violations.push("config register written while Cfg9346 is locked");
            }
        }

        fn tx_poll(&mut self) {
            let lo = u32::from_le_bytes(self.regs[TX_DESC_START_LO..TX_DESC_START_LO + 4].try_into().unwrap()) as u64;
            let hi = u32::from_le_bytes(self.regs[TX_DESC_START_HI..TX_DESC_START_HI + 4].try_into().unwrap()) as u64;
            let ring = ((hi << 32 | lo) - self.base) as usize;
            if self.regs[CHIP_CMD] & CMD_TX_ENB == 0 {
                self.violations.push("TxPoll with the transmitter disabled");
                return;
            }
            for i in 0..SLOTS {
                let d = ring + i * DESC_SIZE;
                let opts1 = u32::from_le_bytes(self.arena[d..d + 4].try_into().unwrap());
                if opts1 & DESC_OWN != 0 {
                    if opts1 & (DESC_FIRST_FRAG | DESC_LAST_FRAG) != (DESC_FIRST_FRAG | DESC_LAST_FRAG) {
                        self.violations.push("TX descriptor without First+Last");
                    }
                    let len = (opts1 & DESC_LEN_MASK) as usize;
                    if len < MIN_FRAME {
                        self.violations.push("TX frame shorter than 60 bytes");
                    }
                    let addr = u64::from_le_bytes(self.arena[d + 8..d + 16].try_into().unwrap());
                    let a = (addr - self.base) as usize;
                    let frame = self.arena[a..a + len].to_vec();
                    self.wire.push(frame);
                    // Done: clear OWN, keep the rest.
                    self.arena[d..d + 4].copy_from_slice(&(opts1 & !DESC_OWN).to_le_bytes());
                }
            }
        }
    }

    impl Regs for Dev {
        fn r8(&self, off: usize) -> u8 {
            let m = self.0.borrow();
            match off {
                PHY_STATUS => m.link_status,
                _ => m.regs[off],
            }
        }
        fn r16(&self, off: usize) -> u16 {
            let m = self.0.borrow();
            u16::from_le_bytes(m.regs[off..off + 2].try_into().unwrap())
        }
        fn r32(&self, off: usize) -> u32 {
            let mut m = self.0.borrow_mut();
            if off == PHYAR {
                // MDIO: the read (flag clear) completes with the flag set; a write completes with it clear.
                let v = Self::rd32(&m, PHYAR);
                let reg = ((v >> 16) & 0x1F) as usize;
                let page = m.phy[0x1f];
                let banked = page != 0 && reg != 0x1f;
                if v & PHYAR_FLAG == 0 {
                    let val = if banked { m.paged.get(&(page, reg)).copied().unwrap_or(0) } else { m.phy[reg] };
                    return PHYAR_FLAG | (reg as u32) << 16 | val as u32;
                }
                if banked {
                    m.paged.insert((page, reg), v as u16);
                } else {
                    m.phy[reg] = v as u16;
                }
                m.regs[PHYAR..PHYAR + 4].copy_from_slice(&(v & !PHYAR_FLAG).to_le_bytes());
                return v & !PHYAR_FLAG;
            }
            Self::rd32(&m, off)
        }
        fn w8(&self, off: usize, val: u8) {
            let mut m = self.0.borrow_mut();
            m.check_config_write(off);
            match off {
                CHIP_CMD if val & CMD_RESET != 0 => {
                    if !m.reset_stuck {
                        // Reset clears itself, and everything configurable with it.
                        let keep_mac: [u8; 6] = m.regs[MAC0..MAC0 + 6].try_into().unwrap();
                        let keep_tx = m.regs[TX_CONFIG..TX_CONFIG + 4].to_vec();
                        m.regs = [0; REG_WINDOW];
                        m.regs[MAC0..MAC0 + 6].copy_from_slice(&keep_mac);
                        m.regs[TX_CONFIG..TX_CONFIG + 4].copy_from_slice(&keep_tx);
                        // The 8168g/h comes out of reset with the RXDV gate closed.
                        m.regs[MISC..MISC + 4].copy_from_slice(&MISC_RXDV_GATED_EN.to_le_bytes());
                    } else {
                        m.regs[CHIP_CMD] = CMD_RESET;
                    }
                }
                TX_POLL => {
                    m.regs[off] = val;
                    if val & TXPOLL_NPQ != 0 {
                        m.tx_poll();
                    }
                }
                _ => m.regs[off] = val,
            }
        }
        fn w16(&self, off: usize, val: u16) {
            let mut m = self.0.borrow_mut();
            m.check_config_write(off);
            m.regs[off..off + 2].copy_from_slice(&val.to_le_bytes());
        }
        fn w32(&self, off: usize, val: u32) {
            let mut m = self.0.borrow_mut();
            if off == ERIAR {
                let addr = val & 0xFFF;
                if val & ERIAR_FLAG != 0 {
                    let mask = (val >> 12) & 0xF;
                    if addr & 3 != 0 || mask == 0 {
                        m.violations.push("misaligned or empty-mask ERI write");
                    }
                    let data = Self::rd32(&m, ERIDR);
                    m.eri.insert(addr, data);
                    m.regs[ERIAR..ERIAR + 4].copy_from_slice(&(val & !ERIAR_FLAG).to_le_bytes());
                } else {
                    let data = m.eri.get(&addr).copied().unwrap_or(0);
                    m.regs[ERIDR..ERIDR + 4].copy_from_slice(&data.to_le_bytes());
                    m.regs[ERIAR..ERIAR + 4].copy_from_slice(&(val | ERIAR_FLAG).to_le_bytes());
                }
                return;
            }
            if off == OCPDR {
                let reg = ((val >> 15) & 0xFFFE) as u16;
                if val & OCPAR_FLAG != 0 {
                    m.ocp.insert(reg, val as u16);
                } else {
                    let data = m.ocp.get(&reg).copied().unwrap_or(0);
                    m.regs[OCPDR..OCPDR + 4].copy_from_slice(&(data as u32).to_le_bytes());
                }
                return;
            }
            m.regs[off..off + 4].copy_from_slice(&val.to_le_bytes());
        }
    }

    impl DmaMem for Dev {
        fn read(&self, off: usize, buf: &mut [u8]) {
            let m = self.0.borrow();
            buf.copy_from_slice(&m.arena[off..off + buf.len()]);
        }
        fn write(&self, off: usize, data: &[u8]) {
            self.0.borrow_mut().arena[off..off + data.len()].copy_from_slice(data);
        }
        fn bus_addr(&self, off: usize) -> u64 {
            self.0.borrow().base + off as u64
        }
    }

    fn up(base: u64) -> (Rtl8168<Dev, Dev>, Dev) {
        let dev = Dev::new(base);
        let mut d = Rtl8168::new(dev.clone(), dev.clone());
        d.reset(|| {}).unwrap();
        d.init_rings();
        (d, dev)
    }

    #[test]
    fn identify_reads_without_writing() {
        let dev = Dev::new(0x1000_0000);
        let d = Rtl8168::new(dev.clone(), dev.clone());
        let id = d.identify().unwrap();
        assert_eq!(id.mac, [0x04, 0x92, 0x26, 0x01, 0x02, 0x03]);
        assert_eq!(id.xid, 0x541);
        assert_eq!(id.family, Family::Rtl8168H);
        assert_eq!(id.link, Link { up: true, mbps: 1000, full_duplex: true });
        assert!(dev.violations().is_empty());
    }

    #[test]
    fn xid_table() {
        assert_eq!(xid(0x5410_0000 | 0x7000_0000 & 0), 0x541);
        assert_eq!(family(0x540), Family::Rtl8168H);
        assert_eq!(family(0x4C1), Family::Rtl8168G);
        assert_eq!(family(0x7FF & 0x7CF), Family::Unknown);
        // Bits outside the 0x7cf mask do not change the answer.
        assert_eq!(xid((0x541 | 0x030) << TXCFG_XID_SHIFT), 0x541);
    }

    #[test]
    fn dead_device_is_detected() {
        struct Dead;
        impl Regs for Dead {
            fn r8(&self, _: usize) -> u8 { 0xFF }
            fn r16(&self, _: usize) -> u16 { 0xFFFF }
            fn r32(&self, _: usize) -> u32 { 0xFFFF_FFFF }
            fn w8(&self, _: usize, _: u8) {}
            fn w16(&self, _: usize, _: u16) {}
            fn w32(&self, _: usize, _: u32) {}
        }
        struct NoMem;
        impl DmaMem for NoMem {
            fn read(&self, _: usize, _: &mut [u8]) {}
            fn write(&self, _: usize, _: &[u8]) {}
            fn bus_addr(&self, _: usize) -> u64 { 0 }
        }
        let d = Rtl8168::new(Dead, NoMem);
        assert!(d.identify().is_none());
        assert_eq!(d.reset(|| {}), Err(InitError::NoDevice));
    }

    #[test]
    fn reset_waits_for_the_bit_and_can_time_out() {
        let dev = Dev::new(0x1000_0000);
        let d = Rtl8168::new(dev.clone(), dev.clone());
        assert_eq!(d.reset(|| {}), Ok(()));
        dev.0.borrow_mut().reset_stuck = true;
        let mut polls = 0u32;
        assert_eq!(d.reset(|| polls += 1), Err(InitError::ResetTimeout));
        assert!(polls >= 1_000_000, "bounded, but not trivially short");
    }

    #[test]
    fn init_programs_rings_and_respects_the_config_lock() {
        let (_d, dev) = up(0x1000_0000);
        let m = dev.0.borrow();
        assert!(m.violations.is_empty(), "{:?}", m.violations);
        assert_eq!(Dev::rd32(&m, TX_DESC_START_LO), 0x1000_0000 + TX_RING_OFF as u32);
        assert_eq!(Dev::rd32(&m, RX_DESC_START_LO), 0x1000_0000 + RX_RING_OFF as u32);
        assert_eq!(m.regs[CHIP_CMD], CMD_TX_ENB | CMD_RX_ENB);
        assert_eq!(m.regs[CFG9346], CFG9346_LOCK, "relocked at the end");
        assert_eq!(u16::from_le_bytes([m.regs[INTR_MASK], m.regs[INTR_MASK + 1]]), 0, "polled: no interrupts");
        let rxc = Dev::rd32(&m, RX_CONFIG);
        assert!(rxc & RXCFG_ACCEPT_BROADCAST != 0 && rxc & RXCFG_ACCEPT_MY_PHYS != 0);
        assert!(rxc & RXCFG_ACCEPT_ALL_PHYS == 0, "not promiscuous");
        // All RX descriptors owned by the chip, the last one ends the ring.
        for i in 0..SLOTS {
            let d = RX_RING_OFF + i * DESC_SIZE;
            let o = u32::from_le_bytes(m.arena[d..d + 4].try_into().unwrap());
            assert!(o & DESC_OWN != 0);
            assert_eq!(o & DESC_RING_END != 0, i == SLOTS - 1);
            assert_eq!(o & DESC_LEN_MASK, BUF_SIZE as u32);
        }
        // TX descriptors start owned by the driver.
        let o = u32::from_le_bytes(m.arena[TX_RING_OFF..TX_RING_OFF + 4].try_into().unwrap());
        assert_eq!(o & DESC_OWN, 0);
        assert_eq!(m.regs[C_PLUS_CMD] as u16 & CPCMD_PCIDAC, 0, "low addresses: no DAC");
    }

    #[test]
    fn init_opens_the_rxdv_gate_and_sets_the_fifo_thresholds() {
        let (_d, dev) = up(0x1000_0000);
        let m = dev.0.borrow();
        assert!(m.violations.is_empty(), "{:?}", m.violations);
        assert_eq!(Dev::rd32(&m, MISC) & MISC_RXDV_GATED_EN, 0, "gate open");
        assert_eq!(m.eri[&0xC8], 0x08 << 16 | 0x02);
        assert_eq!(m.eri[&0xE8], 0x10 << 16 | 0x06);
        assert_eq!(m.eri[&0xCC] & 0xFF, 0x38);
        assert_eq!(m.eri[&0xD0] & 0xFF, 0x48);
        assert_eq!(m.eri[&0x5F0] & 0xFFFF, 0x4F87);
        assert_eq!(m.eri[&0xDC] & 0x1D, 0x1D, "filter reset ends with bit 0 set, plus 0x1c");
    }

    #[test]
    fn frames_are_dropped_while_the_rxdv_gate_is_closed() {
        let dev = Dev::new(0x1000_0000);
        let mut d = Rtl8168::new(dev.clone(), dev.clone());
        d.reset(|| {}).unwrap();
        assert!(!dev.inject(&[0u8; 60], 0), "closed gate: nothing arrives");
        d.init_rings();
        assert!(dev.inject(&[0u8; 60], 0), "open gate: frame arrives");
        let mut buf = [0u8; 2048];
        assert_eq!(d.recv(&mut buf), Some(60));
    }

    #[test]
    fn high_addresses_enable_pcidac() {
        let (_d, dev) = up(0x2_0000_0000);
        let m = dev.0.borrow();
        assert_eq!(Dev::rd32(&m, RX_DESC_START_HI), 2);
        assert!(u16::from_le_bytes([m.regs[C_PLUS_CMD], m.regs[C_PLUS_CMD + 1]]) & CPCMD_PCIDAC != 0);
        // An arena that straddles 4 GiB counts as high too.
        drop(m);
        let (_d2, dev2) = up(0x1_FFFF_F000);
        let m2 = dev2.0.borrow();
        assert!(u16::from_le_bytes([m2.regs[C_PLUS_CMD], m2.regs[C_PLUS_CMD + 1]]) & CPCMD_PCIDAC != 0);
    }

    #[test]
    fn send_pads_short_frames_and_rings_the_doorbell() {
        let (mut d, dev) = up(0x1000_0000);
        let arp = [0xAAu8; 42];
        assert!(d.send(&arp));
        let m = dev.0.borrow();
        assert!(m.violations.is_empty(), "{:?}", m.violations);
        assert_eq!(m.wire.len(), 1);
        assert_eq!(m.wire[0].len(), MIN_FRAME);
        assert_eq!(&m.wire[0][..42], &arp[..]);
        assert!(m.wire[0][42..].iter().all(|&b| b == 0), "padded with zeros");
    }

    #[test]
    fn tx_ring_fills_then_drains_and_wraps() {
        let (mut d, dev) = up(0x1000_0000);
        let mut sent = 0;
        // The model transmits on every doorbell, so the ring never fills; wrap it several times.
        for n in 0..(SLOTS * 3) {
            let mut f = [0u8; 100];
            f[0] = n as u8;
            assert!(d.send(&f), "frame {}", n);
            sent += 1;
        }
        assert_eq!(dev.0.borrow().wire.len(), sent);
        assert_eq!(dev.0.borrow().wire[SLOTS + 5][0], (SLOTS + 5) as u8, "order kept across the wrap");
        assert!(dev.violations().is_empty());
        // Now a chip that is not sending: the ring fills and `send` says no.
        let (mut d2, dev2) = up(0x1000_0000);
        dev2.0.borrow_mut().regs[TX_POLL] = 0;
        let mut accepted = 0;
        for _ in 0..(SLOTS * 2) {
            // Mark every queued descriptor owned and never serviced: model the chip not polling.
            // (Writing TxPoll normally services them, so stop the doorbell from doing it.)
            let mut m = dev2.0.borrow_mut();
            m.regs[CHIP_CMD] &= !CMD_TX_ENB; // the model ignores the doorbell while TX is off
            drop(m);
            if !d2.send(&[1u8; 64]) { break; }
            accepted += 1;
        }
        assert_eq!(accepted, SLOTS - 1, "one slot always stays free");
        assert_eq!(d2.tx_in_flight(), SLOTS - 1);
        assert_eq!(d2.tx_dropped, 1);
        let _ = &mut d;
    }

    #[test]
    fn oversized_and_empty_frames_are_refused() {
        let (mut d, _dev) = up(0x1000_0000);
        assert!(!d.send(&[]));
        assert!(!d.send(&[0u8; BUF_SIZE + 1]));
        assert_eq!(d.tx_dropped, 2);
    }

    #[test]
    fn recv_strips_the_fcs_and_rearms() {
        let (mut d, dev) = up(0x1000_0000);
        let mut buf = [0u8; 2048];
        assert_eq!(d.recv(&mut buf), None);
        let frame: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
        assert!(dev.inject(&frame, 0));
        assert_eq!(d.recv(&mut buf), Some(200));
        assert_eq!(&buf[..200], &frame[..]);
        assert_eq!(d.recv(&mut buf), None);
        // The descriptor went back to the chip with the full buffer size.
        let m = dev.0.borrow();
        let o = u32::from_le_bytes(m.arena[RX_RING_OFF..RX_RING_OFF + 4].try_into().unwrap());
        assert_eq!(o, rx_arm(BUF_SIZE, false));
    }

    #[test]
    fn rx_wraps_around_the_ring_many_times() {
        let (mut d, dev) = up(0x1000_0000);
        let mut buf = [0u8; 2048];
        for n in 0..(SLOTS * 4) {
            let mut f = [0u8; 80];
            f[0] = n as u8;
            f[1] = (n >> 8) as u8;
            assert!(dev.inject(&f, 0), "the chip found a free buffer for frame {}", n);
            assert_eq!(d.recv(&mut buf), Some(80));
            assert_eq!((buf[0], buf[1]), (n as u8, (n >> 8) as u8));
        }
        assert_eq!(d.rx_frames as usize, SLOTS * 4);
        assert_eq!(d.rx_dropped, 0);
    }

    #[test]
    fn bad_frames_are_dropped_and_do_not_block_good_ones() {
        let (mut d, dev) = up(0x1000_0000);
        let mut buf = [0u8; 2048];
        assert!(dev.inject(&[1u8; 70], RX_CRC));
        assert!(dev.inject(&[2u8; 70], RX_RUNT));
        assert!(dev.inject(&[3u8; 90], 0));
        assert_eq!(d.recv(&mut buf), Some(90), "two bad frames skipped, the good one delivered");
        assert_eq!(buf[0], 3);
        assert_eq!(d.rx_dropped, 2);
    }

    #[test]
    fn a_too_small_destination_drops_the_frame_instead_of_truncating() {
        let (mut d, dev) = up(0x1000_0000);
        assert!(dev.inject(&[9u8; 300], 0));
        let mut small = [0u8; 100];
        assert_eq!(d.recv(&mut small), None);
        assert_eq!(d.rx_dropped, 1);
    }

    #[test]
    fn rx_status_decoding() {
        assert_eq!(rx_status(DESC_OWN | 1500), RxStatus::Owned);
        let ok = DESC_FIRST_FRAG | DESC_LAST_FRAG | 64;
        assert_eq!(rx_status(ok), RxStatus::Frame(60));
        assert_eq!(rx_status(ok | RX_RES), RxStatus::Bad);
        assert_eq!(rx_status(DESC_FIRST_FRAG | 64), RxStatus::Bad, "a fragment, not a whole frame");
        assert_eq!(rx_status(DESC_FIRST_FRAG | DESC_LAST_FRAG | 3), RxStatus::Bad, "shorter than the FCS");
    }

    #[test]
    fn phy_access_and_autoneg() {
        let dev = Dev::new(0x1000_0000);
        let d = Rtl8168::new(dev.clone(), dev.clone());
        assert!(d.phy_write(MII_BMCR, 0x1234, || {}));
        assert_eq!(d.phy_read(MII_BMCR, || {}), Some(0x1234));
        assert!(d.phy_autoneg(|| {}));
        let m = dev.0.borrow();
        assert_eq!(m.phy[MII_ADVERTISE as usize], ADVERTISE_ALL);
        assert_eq!(m.phy[MII_CTRL1000 as usize], ADVERTISE_1000);
        assert_eq!(m.phy[MII_BMCR as usize], BMCR_ANENABLE | BMCR_ANRESTART);
    }

    #[test]
    fn link_decoding() {
        assert_eq!(link_from_phy_status(PHYST_LINK | PHYST_100 | PHYST_FULL_DUP), Link { up: true, mbps: 100, full_duplex: true });
        assert_eq!(link_from_phy_status(PHYST_LINK | PHYST_10), Link { up: true, mbps: 10, full_duplex: false });
        assert_eq!(link_from_phy_status(0), Link { up: false, mbps: 0, full_duplex: false });
    }

    #[test]
    fn descriptor_ownership_is_written_last() {
        // `write_desc` stores everything but opts1 first, so a chip that
        // fetches the descriptor between the two writes never sees OWN with stale fields.
        struct Log(RefCell<Vec<(usize, usize)>>);
        impl Regs for Log {
            fn r8(&self, _: usize) -> u8 { 0 }
            fn r16(&self, _: usize) -> u16 { 0 }
            fn r32(&self, _: usize) -> u32 { 0 }
            fn w8(&self, _: usize, _: u8) {}
            fn w16(&self, _: usize, _: u16) {}
            fn w32(&self, _: usize, _: u32) {}
        }
        impl DmaMem for Log {
            fn read(&self, _: usize, buf: &mut [u8]) { buf.fill(0); }
            fn write(&self, off: usize, data: &[u8]) { self.0.borrow_mut().push((off, data.len())); }
            fn bus_addr(&self, off: usize) -> u64 { off as u64 }
        }
        let log = Log(RefCell::new(Vec::new()));
        let d = Rtl8168::new(Log(RefCell::new(Vec::new())), log);
        d.write_desc(0, 3, 0x1234, DESC_OWN);
        let w = d.dma.0.borrow();
        assert_eq!(w.len(), 2);
        assert_eq!(w[0], (3 * DESC_SIZE + 4, 12), "opts2 + address first");
        assert_eq!(w[1], (3 * DESC_SIZE, 4), "opts1 (with OWN) last");
    }

    #[test]
    fn init_matches_what_linux_leaves_in_the_registers() {
        // `ethtool -d` of the same chip under r8169: TxConfig 0x57100f80, MaxTxPacketSize 0x27.
        let (_d, dev) = up(0x1000_0000);
        let m = dev.0.borrow();
        let tx = Dev::rd32(&m, TX_CONFIG);
        assert!(tx & TXCFG_AUTO_FIFO != 0, "AUTO_FIFO set, as Linux does");
        assert_eq!(m.regs[MAX_TX_PACKET_SIZE], 0x27);
    }

    #[test]
    fn enable_irq_unmasks_what_linux_does_and_disable_masks_all() {
        let (mut d, dev) = up(0x1000_0000);
        d.enable_irq();
        let mask = |dev: &Dev| {
            let m = dev.0.borrow();
            u16::from_le_bytes([m.regs[INTR_MASK], m.regs[INTR_MASK + 1]])
        };
        assert_eq!(mask(&dev), IRQ_MASK);
        assert_eq!(IRQ_MASK & 0x2f, 0x2f, "Linux's RxOK|RxErr|TxOK|TxErr|LinkChg");
        assert_eq!(IRQ_MASK & INT_SYS_ERR, 0);
        d.disable_irq();
        assert_eq!(mask(&dev), 0);
    }

    #[test]
    fn recv_acknowledges_the_status_once_the_ring_is_empty() {
        let (mut d, dev) = up(0x1000_0000);
        dev.0.borrow_mut().regs[INTR_STATUS..INTR_STATUS + 2].copy_from_slice(&INT_TX_OK.to_le_bytes());
        let mut buf = [0u8; 2048];
        assert_eq!(d.recv(&mut buf), None);
        assert_eq!(d.intr_seen, INT_TX_OK, "seen and acknowledged by an empty receive");
    }

    #[test]
    fn poll_link_reports_each_transition_once() {
        let (mut d, dev) = up(0x1000_0000);
        assert_eq!(d.poll_link(), None, "the state at init is the baseline");
        let was = dev.0.borrow().link_status;
        dev.0.borrow_mut().link_status = 0;
        assert_eq!(d.poll_link(), Some(false));
        assert_eq!(d.poll_link(), None, "reported once");
        dev.0.borrow_mut().link_status = was | PHYST_LINK;
        assert_eq!(d.poll_link(), Some(true));
        assert_eq!(d.link_changes, 2);
    }

    #[test]
    fn multicast_hash_matches_linux_ether_crc() {
        // 01:00:5e:00:00:01 (all hosts): CRC 0x7fa32d9b, bit 31 of the hash,
        // which the 8168's swapped layout puts in MAR0+4 as 0x80.
        let all_hosts = [0x01, 0x00, 0x5e, 0x00, 0x00, 0x01];
        assert_eq!(ether_crc(&all_hosts), 0x7fa3_2d9b);
        assert_eq!(multicast_filter(&[all_hosts]), [0, 0x80]);
        assert_eq!(multicast_filter(&[]), [0, 0]);
    }

    #[test]
    fn set_multicast_programs_the_hash_and_the_accept_bit() {
        let (mut d, dev) = up(0x1000_0000);
        let rd = |dev: &Dev| {
            let m = dev.0.borrow();
            (Dev::rd32(&m, MAR0), Dev::rd32(&m, MAR0 + 4), Dev::rd32(&m, RX_CONFIG))
        };
        assert_eq!(rd(&dev).2 & RXCFG_ACCEPT_MULTICAST, 0, "off by default");
        d.set_multicast(&[[0x01, 0x00, 0x5e, 0x00, 0x00, 0x01]]);
        assert_eq!(rd(&dev), (0, 0x80, rd(&dev).2));
        assert_ne!(rd(&dev).2 & RXCFG_ACCEPT_MULTICAST, 0);
        d.accept_all_multicast();
        assert_eq!((rd(&dev).0, rd(&dev).1), (u32::MAX, u32::MAX));
        d.set_multicast(&[]);
        assert_eq!(rd(&dev), (0, 0, rd(&dev).2));
        assert_eq!(rd(&dev).2 & RXCFG_ACCEPT_MULTICAST, 0, "other RxConfig bits untouched");
        assert_ne!(rd(&dev).2 & RXCFG_ACCEPT_BROADCAST, 0);
    }

    #[test]
    fn report_decodes_tcp_frames_for_the_proc_file() {
        use alloc::string::String;
        let (mut d, _dev) = up(0x1000_0000);
        let mut f = [0u8; 58];
        f[12] = 0x08;
        f[14] = 0x45;
        f[23] = 6;
        f[26..30].copy_from_slice(&[192, 168, 100, 8]);
        f[30..34].copy_from_slice(&[34, 223, 124, 45]);
        f[34..36].copy_from_slice(&49152u16.to_be_bytes());
        f[36..38].copy_from_slice(&80u16.to_be_bytes());
        f[38..42].copy_from_slice(&1000u32.to_be_bytes());
        f[46] = 0x70;
        f[47] = 0x02;
        f[48..50].copy_from_slice(&65535u16.to_be_bytes());
        assert!(d.send(&f));
        let mut out = String::new();
        d.report(&mut out).unwrap();
        assert!(out.contains("58 B: tcp 192.168.100.8:49152 > 34.223.124.45:80 SYN seq 1000 ack 0 win 65535"), "{}", out);
    }

    #[test]
    fn phy_config_matches_linux_8168h_2() {
        let dev = Dev::new(0x1000_0000);
        let d = Rtl8168::new(dev.clone(), dev.clone());
        {
            let mut m = dev.0.borrow_mut();
            m.ocp.insert(0xdd00, 0x0abc);
            // Reset values that the sequence must change.
            m.paged.insert((0x0a44, 0x11), 1 << 7);
            m.paged.insert((0x0a43, 0x10), 0x0005);
            m.paged.insert((0x0bcd, 0x16), 0x0007);
            m.paged.insert((0x0a43, 0x14), 0xffff);
        }
        assert!(d.phy_config_8168h(|| {}));
        let m = dev.0.borrow();
        let reg = |page: u16, r: usize| m.paged.get(&(page, r)).copied().unwrap_or(0);
        assert_eq!(reg(0x0bcf, 0x16), 0x055c, "ADC bias offset from MAC OCP 0xdd00");
        assert_eq!(reg(0x0bcd, 0x17), 0x4444, "level 7 -> rlen 4 in every nibble");
        assert_eq!(reg(0x0a44, 0x11), 1 << 11, "gphy 10M on, PFM off");
        assert_eq!(reg(0x0a43, 0x10) & 0x0005, 0, "10M PLL off and ALDPS cleared");
        assert_eq!(reg(0x0a43, 0x11) & (1 << 4), 1 << 4, "EEE");
        assert_eq!(reg(0x0a42, 0x16) & 2, 2);
        assert_eq!(m.phy[0x1f], 0, "page restored");
        assert!(m.violations.is_empty());
    }

    #[test]
    fn ack_status_accumulates_and_clears_event_bits() {
        let (mut d, dev) = up(0x1000_0000);
        dev.0.borrow_mut().regs[INTR_STATUS..INTR_STATUS + 2].copy_from_slice(&(INT_RX_OK | INT_RX_OVERFLOW).to_le_bytes());
        assert_eq!(d.ack_status(), INT_RX_OK | INT_RX_OVERFLOW);
        assert_eq!(d.intr_seen, INT_RX_OK | INT_RX_OVERFLOW);
        // The model has no write-1-to-clear: the driver wrote the bits back, which the model stores.
        dev.0.borrow_mut().regs[INTR_STATUS..INTR_STATUS + 2].copy_from_slice(&INT_SYS_ERR.to_le_bytes());
        d.ack_status();
        assert_eq!(d.intr_seen, INT_RX_OK | INT_RX_OVERFLOW | INT_SYS_ERR, "bits accumulate");
        // An all-ones read (device gone) is not an event.
        dev.0.borrow_mut().regs[INTR_STATUS..INTR_STATUS + 2].copy_from_slice(&0xFFFFu16.to_le_bytes());
        let before = d.intr_seen;
        d.ack_status();
        assert_eq!(d.intr_seen, before);
    }

    #[test]
    fn report_shows_counters_registers_and_recent_frames() {
        use alloc::string::String;
        let (mut d, dev) = up(0x1000_0000);
        assert!(d.send(&[0xAB; 60]));
        let mut f = [0u8; 100];
        f[0] = 0xFF;
        f[12] = 0x08;
        assert!(dev.inject(&f, 0));
        let mut buf = [0u8; 2048];
        assert_eq!(d.recv(&mut buf), Some(100));
        let mut out = String::new();
        d.report(&mut out).unwrap();
        assert!(out.contains("rx 1 (dropped 0) tx 1"), "{}", out);
        assert!(out.contains("ChipCmd 0x0c"), "{}", out);
        assert!(out.contains("AUTO_FIFO") || out.contains("TxConfig 0x"), "{}", out);
        assert!(out.contains("last 1 tx frames"), "{}", out);
        assert!(out.contains("60 B: ab ab ab"), "{}", out);
        assert!(out.contains("100 B: ff"), "{}", out);
    }

    #[test]
    fn frame_notes_keep_the_newest_and_wrap() {
        let (mut d, _dev) = up(0x1000_0000);
        for n in 0..(NOTES + 3) {
            let mut f = [0u8; 64];
            f[0] = n as u8;
            d.send(&f);
        }
        let newest = &d.tx_notes[(d.tx_frames as usize - 1) % NOTES];
        assert_eq!(newest.head[0], (NOTES + 2) as u8);
        assert_eq!(newest.len, 64);
    }

    /// The real chip: `ethtool -d` of the AM4 board's RTL8168H under Linux's r8169
    /// (`fixtures/rtl8168h-linux-regs.bin`, PCI rev 0x15, XID 0x541, MAC f0:2f:74:c9:80:a7,
    /// 1000 Mb/s full duplex). The only part of this module checked against the hardware.
    struct Fixture(&'static [u8]);

    impl Regs for Fixture {
        fn r8(&self, off: usize) -> u8 { self.0[off] }
        fn r16(&self, off: usize) -> u16 { u16::from_le_bytes([self.0[off], self.0[off + 1]]) }
        fn r32(&self, off: usize) -> u32 { u32::from_le_bytes(self.0[off..off + 4].try_into().unwrap()) }
        fn w8(&self, _: usize, _: u8) { panic!("a read-only fixture") }
        fn w16(&self, _: usize, _: u16) { panic!("a read-only fixture") }
        fn w32(&self, _: usize, _: u32) { panic!("a read-only fixture") }
    }

    struct NoDma;
    impl DmaMem for NoDma {
        fn read(&self, _: usize, _: &mut [u8]) {}
        fn write(&self, _: usize, _: &[u8]) {}
        fn bus_addr(&self, _: usize) -> u64 { 0 }
    }

    #[test]
    fn identify_decodes_the_real_chips_register_dump() {
        let dump: &'static [u8] = include_bytes!("../fixtures/rtl8168h-linux-regs.bin");
        assert_eq!(dump.len(), REG_WINDOW);
        let d = Rtl8168::new(Fixture(dump), NoDma);
        let id = d.identify().expect("a real dump is not all ones");
        assert_eq!(id.xid, 0x541, "Linux's dmesg says XID 541");
        assert_eq!(id.family, Family::Rtl8168H);
        assert_eq!(id.mac, [0xf0, 0x2f, 0x74, 0xc9, 0x80, 0xa7], "Linux's `ip link`");
        assert_eq!(id.link, Link { up: true, mbps: 1000, full_duplex: true }, "`ethtool`: 1000Mb/s Full");
        assert_eq!(id.tx_config, 0x5710_0f80, "XID in bits 30:20; DMA burst 7, AUTO_FIFO");
        // What this driver programs must match the registers Linux leaves in the same chip.
        let r = d.regs();
        assert_eq!(r.r32(TX_CONFIG) & TXCFG_AUTO_FIFO, TXCFG_AUTO_FIFO);
        assert_eq!(r.r8(MAX_TX_PACKET_SIZE), EARLY_SIZE);
        assert_eq!(r.r8(CHIP_CMD), CMD_RX_ENB | CMD_TX_ENB);
        let rx = r.r32(RX_CONFIG);
        assert!(rx & RXCFG_ACCEPT_BROADCAST != 0 && rx & RXCFG_ACCEPT_MY_PHYS != 0);
        assert_eq!(rx & (RXCFG_DMA_BURST | RXCFG_EARLY_OFF | RXCFG_MULTI_EN | RXCFG_128_INT_EN), RXCFG_DMA_BURST | RXCFG_EARLY_OFF | RXCFG_MULTI_EN | RXCFG_128_INT_EN, "RxConfig bits as remembered");
        assert_eq!(r.r16(INTR_MASK) & (INT_RX_OK | INT_TX_OK), INT_RX_OK | INT_TX_OK, "Linux runs interrupt-driven");
    }
}
