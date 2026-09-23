//! xHCI (USB 3 host controller) register layout, TRB encoding, ring
//! arithmetic and device-context construction — pure logic, host-tested.
//!
//! The split against `kernel/src/usb/xhci.rs` follows this crate's usual
//! "decide, don't do" line (see `hal::ac97::plan_fill`,
//! `hal::keyboard::KeyDecoder::process`): everything here is arithmetic
//! over plain integers and `[u32]` slices — *what bytes go where* — while
//! the kernel side owns the parts that only exist on real hardware: the
//! MMIO window, DMA-capable allocation, the doorbell writes, and the
//! waiting.
//!
//! That line is worth drawing sharply for this particular controller. An
//! xHCI bring-up failure is close to unobservable — a wrong bit in a
//! device context produces no fault, no log, just a Transfer Event that
//! never arrives — and on the machine this driver was written for (a
//! physical AM4 box with a USB-only keyboard and no serial capture at all)
//! there is nothing to read but the screen. Every field this module
//! computes is therefore asserted against the specification's own layout
//! in `cargo test`, where a mistake costs a second instead of a reboot.
//!
//! References are to the xHCI 1.2 specification: §5 for the register
//! interface, §6.2 for contexts, §6.4 for TRBs, §4.11.2 for the rings.

// ── Capability registers (§5.3) ──────────────────────────────────────────────

pub const CAP_CAPLENGTH: usize = 0x00;
/// `HCIVERSION` is a **16-bit** register at offset 2, i.e. the upper half
/// of the dword at offset 0 — see [`hciversion`]. A 32-bit read at this
/// offset is misaligned, which on x86 merely works but which Rust's
/// `read_volatile` treats as undefined behaviour and (in a debug build)
/// panics on.
pub const CAP_HCIVERSION: usize = 0x02;
pub const CAP_HCSPARAMS1: usize = 0x04;
pub const CAP_HCSPARAMS2: usize = 0x08;
pub const CAP_HCCPARAMS1: usize = 0x10;
pub const CAP_DBOFF: usize = 0x14;
pub const CAP_RTSOFF: usize = 0x18;

/// Interface version out of the dword at [`CAP_CAPLENGTH`], whose low
/// byte is CAPLENGTH and whose high half is HCIVERSION.
pub fn hciversion(caplength_dword: u32) -> u16 {
    (caplength_dword >> 16) as u16
}

/// `HCSPARAMS1` → (MaxSlots, MaxPorts).
pub fn hcsparams1_slots(v: u32) -> u8 {
    (v & 0xFF) as u8
}
pub fn hcsparams1_ports(v: u32) -> u8 {
    ((v >> 24) & 0xFF) as u8
}

/// `HCSPARAMS2` → Max Scratchpad Buffers, a 10-bit field the spec splits
/// across two non-adjacent ranges (hi = bits 25:21, lo = bits 31:27).
/// Reading only the low half — an easy misread of the register diagram —
/// under-allocates on any controller that wants more than 31 buffers, and
/// the controller then DMAs into memory that was never handed to it.
pub fn hcsparams2_max_scratchpad(v: u32) -> u32 {
    let hi = (v >> 21) & 0x1F;
    let lo = (v >> 27) & 0x1F;
    (hi << 5) | lo
}

/// `HCCPARAMS1` bit 2 (CSZ): contexts are 64 bytes instead of 32.
pub fn hccparams1_context_size(v: u32) -> usize {
    if v & (1 << 2) != 0 { 64 } else { 32 }
}
/// `HCCPARAMS1` bit 0 (AC64): 64-bit addressing capable.
pub fn hccparams1_ac64(v: u32) -> bool {
    v & 1 != 0
}
/// `HCCPARAMS1` bits 31:16 — offset of the first extended capability, in
/// **dwords** from the capability base. Zero means there are none.
pub fn hccparams1_xecp_offset(v: u32) -> usize {
    ((v >> 16) & 0xFFFF) as usize * 4
}

// ── Extended capabilities (§7) ───────────────────────────────────────────────

pub const XECP_ID_LEGACY_SUPPORT: u8 = 1;
pub const XECP_ID_SUPPORTED_PROTOCOL: u8 = 2;

/// An extended-capability header: ID in bits 7:0, offset to the next
/// capability (in dwords) in bits 15:8. A `next` of 0 ends the list.
pub fn xecp_id(v: u32) -> u8 {
    (v & 0xFF) as u8
}
pub fn xecp_next_offset(v: u32) -> usize {
    ((v >> 8) & 0xFF) as usize * 4
}

/// USBLEGSUP bits (§7.1.1): the BIOS/OS ownership handshake.
pub const LEGSUP_BIOS_OWNED: u32 = 1 << 16;
pub const LEGSUP_OS_OWNED: u32 = 1 << 24;

// ── Operational registers (§5.4), relative to the operational base ───────────

pub const OP_USBCMD: usize = 0x00;
pub const OP_USBSTS: usize = 0x04;
pub const OP_PAGESIZE: usize = 0x08;
pub const OP_CRCR: usize = 0x18;
pub const OP_DCBAAP: usize = 0x30;
pub const OP_CONFIG: usize = 0x38;
/// First port's PORTSC; port *n* (1-based) is at `OP_PORTSC + (n-1) * 0x10`.
pub const OP_PORTSC: usize = 0x400;
pub const PORT_REGISTER_STRIDE: usize = 0x10;

pub const USBCMD_RS: u32 = 1 << 0;
pub const USBCMD_HCRST: u32 = 1 << 1;
pub const USBCMD_INTE: u32 = 1 << 2;

pub const USBSTS_HCH: u32 = 1 << 0;
pub const USBSTS_HSE: u32 = 1 << 2;
pub const USBSTS_EINT: u32 = 1 << 3;
pub const USBSTS_PCD: u32 = 1 << 4;
pub const USBSTS_CNR: u32 = 1 << 11;
pub const USBSTS_HCE: u32 = 1 << 12;

/// `CRCR` bit 0, the Ring Cycle State the command ring starts on.
pub const CRCR_RCS: u64 = 1;

// ── Runtime registers (§5.5), relative to the runtime base ───────────────────

/// Interrupter 0's register set starts 0x20 into the runtime space.
pub const RUN_IR0: usize = 0x20;
pub const IR_IMAN: usize = 0x00;
pub const IR_IMOD: usize = 0x04;
pub const IR_ERSTSZ: usize = 0x08;
pub const IR_ERSTBA: usize = 0x10;
pub const IR_ERDP: usize = 0x18;

/// `ERDP` bit 3 — Event Handler Busy, write-1-to-clear. Written back with
/// every dequeue-pointer update.
pub const ERDP_EHB: u64 = 1 << 3;

// ── PORTSC (§5.4.8) ──────────────────────────────────────────────────────────

pub const PORTSC_CCS: u32 = 1 << 0;
pub const PORTSC_PED: u32 = 1 << 1;
pub const PORTSC_PR: u32 = 1 << 4;
pub const PORTSC_PP: u32 = 1 << 9;
pub const PORTSC_CSC: u32 = 1 << 17;
pub const PORTSC_PEC: u32 = 1 << 18;
pub const PORTSC_WRC: u32 = 1 << 19;
pub const PORTSC_OCC: u32 = 1 << 20;
pub const PORTSC_PRC: u32 = 1 << 21;
pub const PORTSC_PLC: u32 = 1 << 22;
pub const PORTSC_CEC: u32 = 1 << 23;

/// Every write-1-to-clear bit in PORTSC, plus PED — which is RW1CS, i.e.
/// writing a 1 **disables the port**. Writing a PORTSC value straight back
/// is therefore not a no-op: it would clear every pending change bit and
/// disable the port the driver just enabled. This mask is what makes a
/// read-modify-write safe.
pub const PORTSC_RW1C_MASK: u32 =
    PORTSC_PED | PORTSC_CSC | PORTSC_PEC | PORTSC_WRC | PORTSC_OCC | PORTSC_PRC | PORTSC_PLC | PORTSC_CEC;

/// Prepares a PORTSC write: takes the value just read, drops every
/// write-1-to-clear bit, and sets `bits`. Pass a change bit in `bits` to
/// clear exactly that one.
pub fn portsc_write(current: u32, bits: u32) -> u32 {
    (current & !PORTSC_RW1C_MASK) | bits
}

/// PORTSC bits 13:10 — the link speed the port trained at. Doubles as the
/// Speed field of the slot context (§6.2.2), which is why it is passed
/// straight through rather than translated.
pub fn portsc_speed(v: u32) -> u8 {
    ((v >> 10) & 0x0F) as u8
}

pub const SPEED_FULL: u8 = 1;
pub const SPEED_LOW: u8 = 2;
pub const SPEED_HIGH: u8 = 3;
pub const SPEED_SUPER: u8 = 4;
pub const SPEED_SUPER_PLUS: u8 = 5;

/// Default control-endpoint max packet size for a freshly reset device of
/// the given speed (USB 2.0 §5.5.3 / USB 3.2 §8.12.1). Full speed is the
/// awkward one: 8, 16, 32 and 64 are all legal, so 8 is the only safe
/// starting guess and the real value has to be read out of the first eight
/// bytes of the device descriptor and patched in with an Evaluate Context.
pub fn default_max_packet0(speed: u8) -> u16 {
    match speed {
        SPEED_LOW => 8,
        SPEED_FULL => 8,
        SPEED_HIGH => 64,
        SPEED_SUPER | SPEED_SUPER_PLUS => 512,
        _ => 8,
    }
}

/// Encodes an endpoint descriptor's `bInterval` into the xHCI Interval
/// field, whose unit is always 125 µs × 2^Interval (§6.2.3.6).
///
/// The two speed families disagree about what `bInterval` means, and
/// getting it backwards is not a small error: it is off by a factor of
/// eight in period, which on a keyboard reads as either a controller that
/// polls far too aggressively or one that feels laggy.
///
/// * High/Super speed: `bInterval` is already an exponent in 1..=16 with
///   the same 125 µs base, so Interval = `bInterval - 1`.
/// * Full/Low speed: `bInterval` is a count of 1 ms frames, so Interval is
///   `log2(bInterval) + 3` (the +3 converts 1 ms to 125 µs units),
///   clamped to the 3..=10 range the spec allows for these speeds.
pub fn endpoint_interval(speed: u8, b_interval: u8) -> u8 {
    match speed {
        SPEED_HIGH | SPEED_SUPER | SPEED_SUPER_PLUS => b_interval.saturating_sub(1).min(15),
        _ => {
            let frames = b_interval.max(1) as u32;
            // Largest power of two not exceeding `frames`.
            let log2 = 31 - frames.leading_zeros();
            (log2 + 3).clamp(3, 10) as u8
        }
    }
}

// ── Device context index (§4.5.1) ────────────────────────────────────────────

/// The Device Context Index of an endpoint: EP0 is 1, and endpoint *n*'s
/// IN and OUT halves are separate contexts at `2n+1` and `2n`.
pub fn endpoint_dci(ep_number: u8, is_in: bool) -> u8 {
    if ep_number == 0 {
        1
    } else {
        2 * ep_number + is_in as u8
    }
}

// ── TRBs (§6.4) ──────────────────────────────────────────────────────────────

pub const TRB_NORMAL: u32 = 1;
pub const TRB_SETUP_STAGE: u32 = 2;
pub const TRB_DATA_STAGE: u32 = 3;
pub const TRB_STATUS_STAGE: u32 = 4;
pub const TRB_LINK: u32 = 6;
pub const TRB_ENABLE_SLOT: u32 = 9;
pub const TRB_ADDRESS_DEVICE: u32 = 11;
pub const TRB_CONFIGURE_ENDPOINT: u32 = 12;
pub const TRB_EVALUATE_CONTEXT: u32 = 13;
pub const TRB_RESET_ENDPOINT: u32 = 14;
pub const TRB_STOP_ENDPOINT: u32 = 15;
pub const TRB_SET_TR_DEQUEUE: u32 = 16;
pub const TRB_NO_OP_COMMAND: u32 = 23;
pub const TRB_TRANSFER_EVENT: u32 = 32;
pub const TRB_COMMAND_COMPLETION_EVENT: u32 = 33;
pub const TRB_PORT_STATUS_CHANGE_EVENT: u32 = 34;

/// Completion codes (§6.4.5) worth naming.
pub const COMP_SUCCESS: u8 = 1;
pub const COMP_STALL: u8 = 6;
pub const COMP_SHORT_PACKET: u8 = 13;
pub const COMP_CONTEXT_STATE_ERROR: u8 = 19;
pub const COMP_STOPPED: u8 = 26;

/// Endpoint State, bits 2:0 of an Endpoint Context's dword 0 (§6.2.3,
/// table 6-8) — read back from the controller's *output* device context.
pub const EP_STATE_RUNNING: u32 = 1;
pub const EP_STATE_HALTED: u32 = 2;
pub const EP_STATE_STOPPED: u32 = 3;

const TRB_CYCLE: u32 = 1 << 0;
const TRB_TOGGLE_CYCLE: u32 = 1 << 1;
const TRB_ISP: u32 = 1 << 2;
const TRB_CHAIN: u32 = 1 << 4;
const TRB_IOC: u32 = 1 << 5;
const TRB_IDT: u32 = 1 << 6;

/// One Transfer Request Block: four little-endian dwords, which is exactly
/// how it is laid out in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Trb(pub [u32; 4]);

impl Trb {
    pub const fn zero() -> Self {
        Trb([0; 4])
    }

    fn typed(ty: u32, cycle: bool) -> Self {
        Trb([0, 0, 0, (ty << 10) | (cycle as u32)])
    }

    pub fn trb_type(&self) -> u32 {
        (self.0[3] >> 10) & 0x3F
    }

    pub fn cycle(&self) -> bool {
        self.0[3] & TRB_CYCLE != 0
    }

    /// Replaces just the cycle bit, leaving the rest of the TRB alone —
    /// the operation that hands a prepared Link TRB over to the
    /// controller.
    pub fn with_cycle(mut self, cycle: bool) -> Self {
        self.0[3] = (self.0[3] & !TRB_CYCLE) | cycle as u32;
        self
    }

    /// Link TRB closing a ring segment: points back at the segment's own
    /// start with Toggle Cycle set, so the ring is circular and the cycle
    /// bit inverts on every lap (§4.11.5.1).
    pub fn link(target_phys: u64, cycle: bool) -> Self {
        let mut t = Self::typed(TRB_LINK, cycle);
        t.0[0] = target_phys as u32;
        t.0[1] = (target_phys >> 32) as u32;
        t.0[3] |= TRB_TOGGLE_CYCLE;
        t
    }

    /// Setup Stage of a control transfer. The 8-byte setup packet travels
    /// *inside* the TRB (IDT = Immediate Data), so no separate DMA buffer
    /// is needed for it.
    pub fn setup_stage(setup: [u8; 8], transfer_type: u32, cycle: bool) -> Self {
        let mut t = Self::typed(TRB_SETUP_STAGE, cycle);
        t.0[0] = u32::from_le_bytes([setup[0], setup[1], setup[2], setup[3]]);
        t.0[1] = u32::from_le_bytes([setup[4], setup[5], setup[6], setup[7]]);
        t.0[2] = 8; // the setup packet's own length, always 8
        t.0[3] |= TRB_IDT | (transfer_type << 16);
        t
    }

    /// Data Stage. `is_in` also picks the transfer direction bit.
    pub fn data_stage(buffer_phys: u64, length: u32, is_in: bool, cycle: bool) -> Self {
        let mut t = Self::typed(TRB_DATA_STAGE, cycle);
        t.0[0] = buffer_phys as u32;
        t.0[1] = (buffer_phys >> 32) as u32;
        t.0[2] = length & 0x1_FFFF;
        t.0[3] |= TRB_ISP | ((is_in as u32) << 16);
        t
    }

    /// Status Stage, which always runs opposite to the data stage (and IN
    /// when there was no data at all). Carries the IOC that makes the
    /// whole control transfer report completion.
    pub fn status_stage(is_in: bool, cycle: bool) -> Self {
        let mut t = Self::typed(TRB_STATUS_STAGE, cycle);
        t.0[3] |= TRB_IOC | ((is_in as u32) << 16);
        t
    }

    /// Normal TRB — one buffer on a non-control endpoint. Used for the
    /// keyboard's interrupt IN transfers: ISP so a short report still
    /// completes, IOC so it lands on the event ring.
    pub fn normal(buffer_phys: u64, length: u32, cycle: bool) -> Self {
        let mut t = Self::typed(TRB_NORMAL, cycle);
        t.0[0] = buffer_phys as u32;
        t.0[1] = (buffer_phys >> 32) as u32;
        t.0[2] = length & 0x1_FFFF;
        t.0[3] |= TRB_ISP | TRB_IOC;
        t
    }

    pub fn enable_slot(cycle: bool) -> Self {
        Self::typed(TRB_ENABLE_SLOT, cycle)
    }

    pub fn no_op_command(cycle: bool) -> Self {
        Self::typed(TRB_NO_OP_COMMAND, cycle)
    }

    /// Reset Endpoint — clears a Halted endpoint's state (§4.6.8). An
    /// endpoint that stalls stays halted and silently completes nothing
    /// until this runs, so a single stalled control request otherwise ends
    /// every later transfer to that device.
    pub fn reset_endpoint(slot: u8, dci: u8, cycle: bool) -> Self {
        let mut t = Self::typed(TRB_RESET_ENDPOINT, cycle);
        t.0[3] |= ((dci as u32) << 16) | ((slot as u32) << 24);
        t
    }

    /// Stop Endpoint (§4.6.9) — halts a *running* endpoint so its dequeue
    /// pointer can be moved. The recovery for a transfer that timed out
    /// without the endpoint halting: its TRB is still owned by the
    /// controller, and Set TR Dequeue Pointer is refused (Context State
    /// Error) on an endpoint that is still running.
    pub fn stop_endpoint(slot: u8, dci: u8, cycle: bool) -> Self {
        let mut t = Self::typed(TRB_STOP_ENDPOINT, cycle);
        t.0[3] |= ((dci as u32) << 16) | ((slot as u32) << 24);
        t
    }

    /// Set TR Dequeue Pointer (§4.6.10) — tells the controller where to
    /// resume on an endpoint whose ring position it lost, which is the
    /// required second half of recovering from a halt: Reset Endpoint alone
    /// leaves the dequeue pointer on the TRB that failed.
    pub fn set_tr_dequeue(slot: u8, dci: u8, ring_phys: u64, cycle_state: bool, cycle: bool) -> Self {
        let mut t = Self::typed(TRB_SET_TR_DEQUEUE, cycle);
        let ptr = ring_phys | cycle_state as u64;
        t.0[0] = ptr as u32;
        t.0[1] = (ptr >> 32) as u32;
        t.0[3] |= ((dci as u32) << 16) | ((slot as u32) << 24);
        t
    }

    pub fn address_device(input_ctx_phys: u64, slot: u8, cycle: bool) -> Self {
        Self::slot_command(TRB_ADDRESS_DEVICE, input_ctx_phys, slot, cycle)
    }

    pub fn configure_endpoint(input_ctx_phys: u64, slot: u8, cycle: bool) -> Self {
        Self::slot_command(TRB_CONFIGURE_ENDPOINT, input_ctx_phys, slot, cycle)
    }

    pub fn evaluate_context(input_ctx_phys: u64, slot: u8, cycle: bool) -> Self {
        Self::slot_command(TRB_EVALUATE_CONTEXT, input_ctx_phys, slot, cycle)
    }

    fn slot_command(ty: u32, input_ctx_phys: u64, slot: u8, cycle: bool) -> Self {
        let mut t = Self::typed(ty, cycle);
        t.0[0] = input_ctx_phys as u32;
        t.0[1] = (input_ctx_phys >> 32) as u32;
        t.0[3] |= (slot as u32) << 24;
        t
    }

    /// Chain bit — set on every TRB of a multi-TRB transfer descriptor
    /// except the last, so the controller treats them as one unit.
    pub fn chained(mut self) -> Self {
        self.0[3] |= TRB_CHAIN;
        self
    }

    // ── Event decoding ──────────────────────────────────────────────────

    /// Completion code, bits 31:24 of dword 2 — present on both Transfer
    /// and Command Completion events.
    pub fn completion_code(&self) -> u8 {
        (self.0[2] >> 24) as u8
    }

    /// Slot ID, bits 31:24 of dword 3.
    pub fn slot_id(&self) -> u8 {
        (self.0[3] >> 24) as u8
    }

    /// Endpoint ID (= DCI), bits 20:16 of dword 3 of a Transfer Event.
    pub fn endpoint_id(&self) -> u8 {
        ((self.0[3] >> 16) & 0x1F) as u8
    }

    /// The pointer field (dwords 0-1) — the command TRB's address on a
    /// Command Completion event, the transfer TRB's on a Transfer Event.
    pub fn pointer(&self) -> u64 {
        self.0[0] as u64 | ((self.0[1] as u64) << 32)
    }

    /// Residual byte count, bits 23:0 of dword 2 of a Transfer Event: how
    /// much of the requested length did **not** transfer. A short IN
    /// transfer's actual length is `requested - transfer_length()`.
    pub fn transfer_length(&self) -> u32 {
        self.0[2] & 0x00FF_FFFF
    }

    /// Port ID, bits 31:24 of dword 0 of a Port Status Change event.
    pub fn port_id(&self) -> u8 {
        (self.0[0] >> 24) as u8
    }
}

/// Setup Stage Transfer Type (§6.4.1.2.1, table 6-26).
pub const TRT_NO_DATA: u32 = 0;
pub const TRT_OUT_DATA: u32 = 2;
pub const TRT_IN_DATA: u32 = 3;

// ── Ring state machines (§4.9) ───────────────────────────────────────────────

/// Where the next TRB goes, and what has to happen to the segment's Link
/// TRB afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Enqueue {
    /// TRB slot the caller should write.
    pub index: usize,
    /// Cycle bit that TRB must carry.
    pub cycle: bool,
    /// When `Some`, the Link TRB at this index must have its cycle bit set
    /// to this value **after** the TRB above is written — handing the
    /// controller the new TRB and the lap-closing link in the right order.
    pub stamp_link: Option<(usize, bool)>,
}

/// A producer ring (command ring or transfer ring): TRBs are written by
/// software and consumed by the controller. The final slot of the segment
/// is reserved for the Link TRB, so `size - 1` TRBs are usable per lap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProducerRing {
    size: usize,
    enqueue: usize,
    cycle: bool,
}

impl ProducerRing {
    /// `size` is the segment's total TRB count, Link TRB included.
    pub const fn new(size: usize) -> Self {
        ProducerRing { size, enqueue: 0, cycle: true }
    }

    pub fn cycle(&self) -> bool {
        self.cycle
    }

    pub fn enqueue_index(&self) -> usize {
        self.enqueue
    }

    /// Reserves the next slot, advancing over the Link TRB when the lap
    /// ends (which is also where the cycle bit inverts).
    pub fn enqueue(&mut self) -> Enqueue {
        let index = self.enqueue;
        let cycle = self.cycle;

        self.enqueue += 1;
        let stamp_link = if self.enqueue == self.size - 1 {
            let link = (self.size - 1, self.cycle);
            self.enqueue = 0;
            self.cycle = !self.cycle;
            Some(link)
        } else {
            None
        };

        Enqueue { index, cycle, stamp_link }
    }
}

/// The event ring, which runs the other way round: the controller
/// produces, software consumes, and software knows a TRB is fresh because
/// its cycle bit matches the consumer's own. There is no Link TRB — the
/// segment simply wraps, inverting the expected cycle (§4.9.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventRing {
    size: usize,
    dequeue: usize,
    cycle: bool,
}

impl EventRing {
    pub const fn new(size: usize) -> Self {
        EventRing { size, dequeue: 0, cycle: true }
    }

    pub fn dequeue_index(&self) -> usize {
        self.dequeue
    }

    /// True when the TRB at the dequeue index belongs to this lap, i.e.
    /// the controller has posted it.
    pub fn is_ready(&self, trb_cycle: bool) -> bool {
        trb_cycle == self.cycle
    }

    /// Consumes the current TRB, returning the new dequeue index.
    pub fn advance(&mut self) -> usize {
        self.dequeue += 1;
        if self.dequeue == self.size {
            self.dequeue = 0;
            self.cycle = !self.cycle;
        }
        self.dequeue
    }
}

// ── Contexts (§6.2) ──────────────────────────────────────────────────────────

/// Input Control Context Add flags (§6.2.5.1): bit *i* adds the context at
/// Device Context Index *i*. Bit 0 is the slot context.
pub const ADD_SLOT: u32 = 1 << 0;

pub fn add_flag(dci: u8) -> u32 {
    1 << dci
}

/// Writes the Input Control Context: drop nothing, add the listed
/// contexts. `ctx` is the first 32 bytes (8 dwords) of the input context.
pub fn build_input_control(ctx: &mut [u32], add_flags: u32) {
    ctx[0] = 0; // drop flags — nothing is ever dropped by this driver
    ctx[1] = add_flags;
    for d in ctx.iter_mut().take(8).skip(2) {
        *d = 0;
    }
}

/// Writes a Slot Context (§6.2.2) for a device attached directly to a root
/// hub port: route string 0 (no hub in the path), the port's trained
/// speed, and the highest Device Context Index this device uses.
pub fn build_slot_context(ctx: &mut [u32], speed: u8, root_port: u8, context_entries: u8) {
    ctx[0] = ((context_entries as u32 & 0x1F) << 27) | ((speed as u32 & 0x0F) << 20);
    ctx[1] = (root_port as u32) << 16;
    ctx[2] = 0;
    ctx[3] = 0;
}

/// Endpoint types (§6.2.3, table 6-9).
pub const EP_TYPE_BULK_OUT: u32 = 2;
pub const EP_TYPE_CONTROL: u32 = 4;
pub const EP_TYPE_BULK_IN: u32 = 6;
pub const EP_TYPE_INTERRUPT_IN: u32 = 7;

/// Writes an Endpoint Context.
///
/// `CErr = 3` is the specification's own recommended retry count; leaving
/// it at 0 means "no error recovery", which turns a single bus glitch into
/// an endpoint halted forever.
pub fn build_endpoint_context(
    ctx: &mut [u32],
    ep_type: u32,
    max_packet: u16,
    interval: u8,
    ring_phys: u64,
    average_trb_length: u16,
) {
    ctx[0] = (interval as u32) << 16;
    ctx[1] = ((max_packet as u32) << 16) | (ep_type << 3) | (3 << 1); // CErr = 3
    // TR Dequeue Pointer, with DCS = 1: the ring starts on cycle state 1,
    // matching `ProducerRing::new`.
    ctx[2] = (ring_phys as u32) | 1;
    ctx[3] = (ring_phys >> 32) as u32;
    ctx[4] = average_trb_length as u32;
    if ep_type == EP_TYPE_INTERRUPT_IN {
        // Max ESIT Payload Lo (bits 31:16 of dword 4) — for a periodic
        // endpoint the controller uses it to budget bus time.
        ctx[4] |= (max_packet as u32) << 16;
    }
    ctx[5] = 0;
    ctx[6] = 0;
    ctx[7] = 0;
}

/// Writes a bulk Endpoint Context (§6.2.3): no interval, streams off,
/// and the SuperSpeed companion's `bMaxBurst` in Max Burst Size (dword 1,
/// bits 15:8) — zero for a USB 2 device, which is also correct there.
///
/// Average TRB Length 3 KiB is §4.14.1.1's own suggested starting value
/// for bulk endpoints; the controller only uses it for bandwidth
/// estimation, and zero is not allowed.
pub fn build_bulk_endpoint_context(
    ctx: &mut [u32],
    is_in: bool,
    max_packet: u16,
    max_burst: u8,
    ring_phys: u64,
) {
    let ep_type = if is_in { EP_TYPE_BULK_IN } else { EP_TYPE_BULK_OUT };
    build_endpoint_context(ctx, ep_type, max_packet, 0, ring_phys, 3072);
    ctx[1] |= (max_burst as u32 & 0xFF) << 8;
}

/// The doorbell value for an endpoint: DB Target = DCI, stream 0.
pub fn doorbell_value(dci: u8) -> u32 {
    dci as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Register field decoding ─────────────────────────────────────────

    #[test]
    fn caplength_dword_splits_into_length_and_version() {
        // A real controller: CAPLENGTH 0x20, HCIVERSION 0x0110.
        let dword = 0x0110_0020u32;
        assert_eq!(dword as u8, 0x20);
        assert_eq!(hciversion(dword), 0x0110);
    }

    #[test]
    fn hcsparams_fields() {
        // MaxSlots = 32, MaxIntrs = 8, MaxPorts = 20
        let v = 32 | (8 << 8) | (20 << 24);
        assert_eq!(hcsparams1_slots(v), 32);
        assert_eq!(hcsparams1_ports(v), 20);
    }

    /// The split scratchpad field is the one a register-diagram misread
    /// actually breaks: hi bits must contribute, not be dropped.
    #[test]
    fn scratchpad_count_spans_both_halves() {
        assert_eq!(hcsparams2_max_scratchpad(0), 0);
        assert_eq!(hcsparams2_max_scratchpad(8 << 27), 8);
        assert_eq!(hcsparams2_max_scratchpad(1 << 21), 32);
        assert_eq!(hcsparams2_max_scratchpad((1 << 21) | (3 << 27)), 35);
        assert_eq!(hcsparams2_max_scratchpad(0xFFFF_FFFF), 1023);
    }

    #[test]
    fn hccparams1_fields() {
        assert_eq!(hccparams1_context_size(0), 32);
        assert_eq!(hccparams1_context_size(1 << 2), 64);
        assert!(hccparams1_ac64(1));
        assert!(!hccparams1_ac64(0));
        // xECP is a dword offset: 0x0100 dwords → 0x400 bytes.
        assert_eq!(hccparams1_xecp_offset(0x0100_0000), 0x400);
        assert_eq!(hccparams1_xecp_offset(0), 0);
    }

    #[test]
    fn xecp_header_fields() {
        let hdr = XECP_ID_LEGACY_SUPPORT as u32 | (4 << 8);
        assert_eq!(xecp_id(hdr), XECP_ID_LEGACY_SUPPORT);
        assert_eq!(xecp_next_offset(hdr), 16);
        assert_eq!(xecp_next_offset(XECP_ID_SUPPORTED_PROTOCOL as u32), 0); // end of list
    }

    // ── PORTSC ──────────────────────────────────────────────────────────

    /// Writing a PORTSC value back unchanged would disable the port (PED
    /// is write-1-to-clear) and eat every pending change bit. This is the
    /// guard against that.
    #[test]
    fn portsc_write_never_disables_or_eats_change_bits() {
        let current = PORTSC_CCS | PORTSC_PED | PORTSC_PP | PORTSC_CSC | PORTSC_PRC;
        let out = portsc_write(current, 0);
        assert_eq!(out & PORTSC_PED, 0, "would have disabled the port");
        assert_eq!(out & PORTSC_CSC, 0, "would have cleared a change bit");
        assert_eq!(out & PORTSC_PRC, 0);
        assert_eq!(out & PORTSC_PP, PORTSC_PP, "power must be preserved");
    }

    #[test]
    fn portsc_write_sets_only_requested_bits() {
        let current = PORTSC_CCS | PORTSC_PP | PORTSC_CSC;
        assert_eq!(portsc_write(current, PORTSC_PR) & PORTSC_PR, PORTSC_PR);
        // Clearing one change bit explicitly is the intended use of `bits`.
        assert_eq!(portsc_write(current, PORTSC_CSC) & PORTSC_CSC, PORTSC_CSC);
    }

    #[test]
    fn portsc_speed_field() {
        assert_eq!(portsc_speed(SPEED_HIGH as u32) , 0); // bits 13:10, not 3:0
        assert_eq!(portsc_speed((SPEED_HIGH as u32) << 10), SPEED_HIGH);
        assert_eq!(portsc_speed((SPEED_SUPER as u32) << 10 | PORTSC_CCS), SPEED_SUPER);
    }

    // ── Speed-dependent arithmetic ──────────────────────────────────────

    #[test]
    fn default_control_packet_sizes() {
        assert_eq!(default_max_packet0(SPEED_LOW), 8);
        assert_eq!(default_max_packet0(SPEED_FULL), 8);
        assert_eq!(default_max_packet0(SPEED_HIGH), 64);
        assert_eq!(default_max_packet0(SPEED_SUPER), 512);
        assert_eq!(default_max_packet0(0), 8); // unknown speed → safest
    }

    #[test]
    fn interval_encoding_high_speed_is_exponent_minus_one() {
        assert_eq!(endpoint_interval(SPEED_HIGH, 1), 0); // 125 µs
        assert_eq!(endpoint_interval(SPEED_HIGH, 8), 7); // 16 ms
        assert_eq!(endpoint_interval(SPEED_SUPER, 4), 3);
        assert_eq!(endpoint_interval(SPEED_HIGH, 0), 0); // malformed, clamped
        assert_eq!(endpoint_interval(SPEED_HIGH, 255), 15); // clamped to field width
    }

    #[test]
    fn interval_encoding_full_speed_is_log2_frames_plus_three() {
        assert_eq!(endpoint_interval(SPEED_FULL, 1), 3); // 1 ms
        assert_eq!(endpoint_interval(SPEED_FULL, 8), 6); // 8 ms
        assert_eq!(endpoint_interval(SPEED_LOW, 10), 6); // 8 ms (rounded down)
        assert_eq!(endpoint_interval(SPEED_FULL, 0), 3); // malformed, clamped
        assert_eq!(endpoint_interval(SPEED_FULL, 255), 10); // clamped to spec max
    }

    #[test]
    fn device_context_indices() {
        assert_eq!(endpoint_dci(0, true), 1);
        assert_eq!(endpoint_dci(0, false), 1); // EP0 is one bidirectional context
        assert_eq!(endpoint_dci(1, true), 3);
        assert_eq!(endpoint_dci(1, false), 2);
        assert_eq!(endpoint_dci(15, true), 31);
    }

    // ── TRB encoding ────────────────────────────────────────────────────

    #[test]
    fn trb_type_and_cycle_round_trip() {
        let t = Trb::enable_slot(true);
        assert_eq!(t.trb_type(), TRB_ENABLE_SLOT);
        assert!(t.cycle());
        assert!(!t.with_cycle(false).cycle());
        assert_eq!(t.with_cycle(false).trb_type(), TRB_ENABLE_SLOT);
    }

    #[test]
    fn setup_stage_carries_the_packet_inline() {
        let setup = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x08, 0x00];
        let t = Trb::setup_stage(setup, TRT_IN_DATA, true);
        assert_eq!(t.0[0], 0x0100_0680);
        assert_eq!(t.0[1], 0x0008_0000);
        assert_eq!(t.0[2], 8);
        assert_eq!(t.trb_type(), TRB_SETUP_STAGE);
        assert_ne!(t.0[3] & (1 << 6), 0, "IDT must be set");
        assert_eq!((t.0[3] >> 16) & 0x3, TRT_IN_DATA);
    }

    #[test]
    fn data_and_status_stage_directions() {
        let d = Trb::data_stage(0x1_0000_2000, 18, true, true);
        assert_eq!(d.0[0], 0x0000_2000);
        assert_eq!(d.0[1], 1);
        assert_eq!(d.0[2], 18);
        assert_ne!(d.0[3] & (1 << 16), 0, "IN direction bit");

        let s = Trb::status_stage(false, true);
        assert_eq!(s.0[3] & (1 << 16), 0, "OUT status after an IN data stage");
        assert_ne!(s.0[3] & (1 << 5), 0, "IOC must be set — nothing completes without it");
    }

    #[test]
    fn normal_trb_sets_isp_and_ioc() {
        let t = Trb::normal(0xDEAD_0000, 8, false);
        assert_eq!(t.trb_type(), TRB_NORMAL);
        assert_eq!(t.0[2], 8);
        assert!(!t.cycle());
        assert_ne!(t.0[3] & (1 << 2), 0, "ISP — a short report must still complete");
        assert_ne!(t.0[3] & (1 << 5), 0, "IOC");
    }

    #[test]
    fn slot_commands_carry_context_pointer_and_slot() {
        let t = Trb::address_device(0x4000, 7, true);
        assert_eq!(t.pointer(), 0x4000);
        assert_eq!(t.slot_id(), 7);
        assert_eq!(t.trb_type(), TRB_ADDRESS_DEVICE);

        let t = Trb::configure_endpoint(0x8000, 3, false);
        assert_eq!(t.trb_type(), TRB_CONFIGURE_ENDPOINT);
        assert_eq!(t.slot_id(), 3);
    }

    #[test]
    fn endpoint_recovery_commands_carry_slot_and_dci() {
        let t = Trb::reset_endpoint(4, 1, true);
        assert_eq!(t.trb_type(), TRB_RESET_ENDPOINT);
        assert_eq!(t.slot_id(), 4);
        assert_eq!(t.endpoint_id(), 1);

        let t = Trb::set_tr_dequeue(4, 1, 0x1_0000_5000, true, true);
        assert_eq!(t.trb_type(), TRB_SET_TR_DEQUEUE);
        assert_eq!(t.slot_id(), 4);
        assert_eq!(t.endpoint_id(), 1);
        assert_eq!(t.pointer(), 0x1_0000_5001, "dequeue pointer carries the cycle state");
        assert_eq!(
            Trb::set_tr_dequeue(1, 1, 0x5000, false, true).pointer(),
            0x5000,
            "cycle state 0 leaves bit 0 clear"
        );
    }

    #[test]
    fn link_trb_toggles_the_cycle() {
        let t = Trb::link(0x2000, true);
        assert_eq!(t.trb_type(), TRB_LINK);
        assert_eq!(t.pointer(), 0x2000);
        assert_ne!(t.0[3] & (1 << 1), 0, "Toggle Cycle must be set");
    }

    #[test]
    fn event_field_decoding() {
        // A Transfer Event: success, 2 bytes short of 8, slot 3, DCI 3.
        let mut t = Trb::zero();
        t.0[2] = 2 | ((COMP_SUCCESS as u32) << 24);
        t.0[3] = (TRB_TRANSFER_EVENT << 10) | (3 << 16) | (3 << 24);
        assert_eq!(t.trb_type(), TRB_TRANSFER_EVENT);
        assert_eq!(t.completion_code(), COMP_SUCCESS);
        assert_eq!(t.transfer_length(), 2);
        assert_eq!(t.endpoint_id(), 3);
        assert_eq!(t.slot_id(), 3);

        // A Port Status Change Event for port 5.
        let mut p = Trb::zero();
        p.0[0] = 5 << 24;
        p.0[3] = TRB_PORT_STATUS_CHANGE_EVENT << 10;
        assert_eq!(p.port_id(), 5);
    }

    // ── Rings ───────────────────────────────────────────────────────────

    #[test]
    fn producer_ring_wraps_through_the_link_and_inverts_cycle() {
        let mut ring = ProducerRing::new(4); // 3 usable TRBs + link
        let a = ring.enqueue();
        assert_eq!((a.index, a.cycle, a.stamp_link), (0, true, None));
        let b = ring.enqueue();
        assert_eq!((b.index, b.cycle, b.stamp_link), (1, true, None));

        // Filling the last usable slot also closes the lap.
        let c = ring.enqueue();
        assert_eq!((c.index, c.cycle), (2, true));
        assert_eq!(c.stamp_link, Some((3, true)), "link stamped with the old cycle");

        // Next lap runs on the inverted cycle, back at index 0.
        let d = ring.enqueue();
        assert_eq!((d.index, d.cycle, d.stamp_link), (0, false, None));
        assert!(!ring.cycle());
    }

    #[test]
    fn producer_ring_second_lap_inverts_again() {
        let mut ring = ProducerRing::new(4);
        for _ in 0..3 {
            ring.enqueue();
        }
        assert!(!ring.cycle());
        for _ in 0..3 {
            ring.enqueue();
        }
        assert!(ring.cycle(), "two laps return to the starting cycle");
    }

    #[test]
    fn event_ring_readiness_follows_the_cycle_bit() {
        let mut ring = EventRing::new(3);
        assert!(ring.is_ready(true));
        assert!(!ring.is_ready(false), "a stale TRB must not be consumed");

        ring.advance();
        ring.advance();
        assert_eq!(ring.dequeue_index(), 2);
        assert!(ring.is_ready(true));

        // Wrapping inverts what "fresh" means.
        ring.advance();
        assert_eq!(ring.dequeue_index(), 0);
        assert!(ring.is_ready(false));
        assert!(!ring.is_ready(true));
    }

    // ── Contexts ────────────────────────────────────────────────────────

    #[test]
    fn input_control_adds_only_what_was_asked() {
        let mut ctx = [0xFFFF_FFFFu32; 8];
        build_input_control(&mut ctx, ADD_SLOT | add_flag(1));
        assert_eq!(ctx[0], 0, "drop flags must be cleared");
        assert_eq!(ctx[1], 0b11);
        assert!(ctx[2..8].iter().all(|&d| d == 0), "reserved dwords must be zeroed");
    }

    #[test]
    fn add_flag_maps_dci_to_its_bit() {
        assert_eq!(add_flag(0), ADD_SLOT);
        assert_eq!(add_flag(1), 0b10);
        assert_eq!(add_flag(3), 0b1000);
        assert_eq!(add_flag(31), 1 << 31);
    }

    #[test]
    fn slot_context_field_placement() {
        let mut ctx = [0xFFFF_FFFFu32; 8];
        build_slot_context(&mut ctx, SPEED_HIGH, 7, 3);
        assert_eq!((ctx[0] >> 27) & 0x1F, 3, "context entries");
        assert_eq!((ctx[0] >> 20) & 0x0F, SPEED_HIGH as u32, "speed");
        assert_eq!(ctx[0] & 0x000F_FFFF, 0, "route string must be 0 for a root port");
        assert_eq!((ctx[1] >> 16) & 0xFF, 7, "root hub port number");
        assert_eq!(ctx[3], 0, "device address is assigned by the controller");
    }

    #[test]
    fn control_endpoint_context_field_placement() {
        let mut ctx = [0xFFFF_FFFFu32; 8];
        build_endpoint_context(&mut ctx, EP_TYPE_CONTROL, 64, 0, 0x1_0000_3000, 8);
        assert_eq!((ctx[1] >> 16) & 0xFFFF, 64, "max packet size");
        assert_eq!((ctx[1] >> 3) & 0x7, EP_TYPE_CONTROL, "endpoint type");
        assert_eq!((ctx[1] >> 1) & 0x3, 3, "CErr must be 3, not 0");
        assert_eq!(ctx[2], 0x3000 | 1, "dequeue pointer low + DCS");
        assert_eq!(ctx[3], 1, "dequeue pointer high");
        assert_eq!(ctx[4] & 0xFFFF, 8, "average TRB length");
    }

    #[test]
    fn interrupt_endpoint_context_carries_interval_and_esit() {
        let mut ctx = [0u32; 8];
        build_endpoint_context(&mut ctx, EP_TYPE_INTERRUPT_IN, 8, 6, 0x5000, 8);
        assert_eq!((ctx[0] >> 16) & 0xFF, 6, "interval");
        assert_eq!((ctx[1] >> 3) & 0x7, EP_TYPE_INTERRUPT_IN);
        assert_eq!((ctx[4] >> 16) & 0xFFFF, 8, "max ESIT payload");
    }

    #[test]
    fn bulk_endpoint_context_fields() {
        let mut ctx = [0xFFFF_FFFFu32; 8];
        build_bulk_endpoint_context(&mut ctx, true, 1024, 15, 0x1234_5000);
        assert_eq!(ctx[0], 0, "no interval, no streams, no mult");
        assert_eq!((ctx[1] >> 16) & 0xFFFF, 1024, "max packet");
        assert_eq!((ctx[1] >> 8) & 0xFF, 15, "max burst");
        assert_eq!((ctx[1] >> 3) & 0x7, EP_TYPE_BULK_IN);
        assert_eq!((ctx[1] >> 1) & 0x3, 3, "CErr");
        assert_eq!(ctx[2], 0x1234_5001, "dequeue pointer with DCS=1");
        assert_eq!(ctx[3], 0);
        assert_eq!(ctx[4], 3072, "avg TRB length, and no ESIT payload for bulk");

        build_bulk_endpoint_context(&mut ctx, false, 512, 0, 0x1_0000_2000);
        assert_eq!((ctx[1] >> 3) & 0x7, EP_TYPE_BULK_OUT);
        assert_eq!((ctx[1] >> 8) & 0xFF, 0);
        assert_eq!(ctx[3], 1, "high half of a >4 GiB ring address");
    }

    #[test]
    fn stop_endpoint_trb_fields() {
        let t = Trb::stop_endpoint(3, 5, true);
        assert_eq!(t.trb_type(), TRB_STOP_ENDPOINT);
        assert!(t.cycle());
        assert_eq!((t.0[3] >> 24) & 0xFF, 3, "slot");
        assert_eq!((t.0[3] >> 16) & 0x1F, 5, "endpoint id");
        assert_eq!(t.0[3] & (1 << 23), 0, "suspend bit clear");
        assert_eq!((t.0[0], t.0[1], t.0[2]), (0, 0, 0));
    }
}
