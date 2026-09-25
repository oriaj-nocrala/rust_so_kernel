// kernel/src/usb/xhci.rs
//
// xHCI host controller driver — the hardware half. Everything that is
// arithmetic rather than hardware (register offsets, TRB encoding, ring
// cycle-bit bookkeeping, device-context field placement, descriptor
// parsing, HID report decoding) lives in `hal::xhci` / `hal::usb` /
// `hal::hid`, where `cargo test` reaches it; this file owns the MMIO
// window, the DMA pages, the doorbells, and the waiting.
//
// Scope: enumerate the root-hub ports once at boot, address every device
// found, and drive the interrupt IN endpoint of any HID boot-protocol
// keyboard among them. That is deliberately the smallest thing that makes
// a USB keyboard work on a machine with no PS/2 port, not a general USB
// stack — see the module doc in `usb/mod.rs` for what is left out and why.
//
// Polling, not interrupts, for the same reason `ac97.rs` polls: the IDT is
// a `spin::Once` populated as the first line of `boot()`, long before PCI
// enumeration knows which vector this controller would use. The event ring
// works identically either way — the controller posts events to it whether
// or not anyone is listening on an interrupt line — so `poll()` runs off
// the 100 Hz PIT tick and reads the ring directly. A keyboard's interrupt
// endpoint has an 8 ms service interval, so a 10 ms poll adds at most one
// interval of latency, which is not perceptible on a keypress.

use core::sync::atomic::{Ordering, compiler_fence};

use hal::usb::{self, SetupPacket};
use hal::xhci::{self as x, EventRing, ProducerRing, Trb};
use x86_64::PhysAddr;

use crate::allocator::phys_alloc;

mod msc;
pub use msc::{MAX_SECTORS, MscError};

/// TRBs per ring segment: one 4 KiB page at 16 bytes each. A ring segment
/// may not cross a 64 KiB boundary (xHCI §4.11.5.1); a single page-aligned
/// page never does.
const RING_TRBS: usize = 256;
const TRB_BYTES: usize = 16;

/// How many device slots this driver will ever enable. The controller may
/// offer 32 or more; a keyboard, a mouse and a hub's worth of headroom is
/// all this kernel can use, and every enabled slot costs a DCBAA entry
/// plus its contexts.
const MAX_SLOTS: u8 = 8;

/// Bound on how much of a configuration descriptor is read. Real keyboards
/// are well under 100 bytes; anything larger than this is either a device
/// this driver has no use for or a lie, and the read is capped rather than
/// trusted.
const MAX_CONFIG_BYTES: u16 = 512;

/// EP0's Device Context Index — always 1 (§4.5.1), and the doorbell target
/// for every control transfer.
const EP0_DCI: u8 = 1;

/// Scancodes decoded off the event ring but not yet collected by `poll`.
/// Keyboard reports are now decoded by whoever happens to be draining the
/// ring — including a disk read spinning for its own completion while the
/// timer ISR is locked out — so they need somewhere to wait. 256 bytes is
/// dozens of reports; a keyboard cannot fill it inside one disk transfer.
const PENDING_KEYS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XhciError {
    /// A register never reached the expected state inside its timeout.
    Timeout,
    /// The controller reported a nonzero completion code.
    Failed(u8),
    /// Out of physical memory for a ring/context/buffer.
    NoMemory,
    /// Hardware present but unusable (halted, host controller error).
    Unusable,
}

type Result<T> = core::result::Result<T, XhciError>;

/// Short name for an error, completion codes included (xHCI §6.4.5,
/// table 6-90; the same numbering as Linux's `COMP_*` constants). Codes
/// 18 and up were once shifted by one here — 19 read as "BandwidthOverrun"
/// when it is Context State Error — which would have mislabelled exactly
/// the error endpoint recovery produces. A bare `Failed(4)` read off a photographed screen means
/// nothing; `Failed(4) XactErr` says the device did not answer on the wire,
/// which is a different problem from `Failed(17) ParamErr` (a malformed
/// context this driver built).
fn describe(e: XhciError) -> &'static str {
    match e {
        XhciError::Timeout => "timeout",
        XhciError::NoMemory => "no memory",
        XhciError::Unusable => "unusable",
        XhciError::Failed(code) => match code {
            2 => "DataBufErr",
            3 => "Babble",
            4 => "XactErr",
            5 => "TrbErr",
            6 => "Stall",
            7 => "ResourceErr",
            8 => "BandwidthErr",
            9 => "NoSlots",
            11 => "SlotNotEnabled",
            12 => "EpNotEnabled",
            13 => "ShortPacket",
            14 => "RingUnderrun",
            15 => "RingOverrun",
            17 => "ParamErr",
            18 => "BandwidthOverrun",
            19 => "ContextStateErr",
            20 => "NoPingResponse",
            21 => "EventRingFull",
            22 => "IncompatibleDevice",
            23 => "MissedService",
            24 => "CmdRingStopped",
            25 => "CmdAborted",
            26 => "Stopped",
            27 => "StoppedLenInvalid",
            28 => "StoppedShortPacket",
            29 => "MaxExitLatencyTooLarge",
            _ => "?",
        },
    }
}

/// What one controller's port scan found. Counts rather than a log line,
/// because on the machine this driver exists for the *only* readable
/// output is a few lines on a screen — "8 ports, 1 connected, 0 addressed"
/// says which of enumeration's three very different failures happened
/// (nothing plugged into a port we can see / device seen but never
/// addressed / addressed but not a boot keyboard), and that is the
/// difference between "try another port" and "the driver is wrong".
#[derive(Debug, Clone, Copy, Default)]
pub struct PortScan {
    /// Root-hub ports the controller reports.
    pub ports: usize,
    /// Ports reporting a device connected.
    pub connected: usize,
    /// Devices successfully reset, slotted and addressed.
    pub addressed: usize,
    /// Addressed devices that were neither HID boot devices nor storage.
    pub other_devices: usize,
    /// Ports where setup returned an error.
    pub failed: usize,
    /// HID boot keyboards now being polled.
    pub keyboards: usize,
    /// HID boot mice now being polled. A device with both interfaces
    /// (a keyboard+mouse receiver, a gaming mouse with macro keys) counts
    /// in both.
    pub mice: usize,
    /// Mass-storage devices that finished SCSI bring-up.
    pub storage: usize,
}

/// Which stage of bringing one port's device up was reached, and how it
/// ended. Recorded per port because the summary counters alone proved
/// ambiguous on real hardware: the first bare-metal run reported
/// "4 connected, 0 addressed, 4 errors", which reads as "Address Device
/// failed" — but the old counting incremented `addressed` only if the
/// *whole* sequence (reset, Enable Slot, Address Device, descriptors,
/// configuration) succeeded, so a device that was addressed perfectly well
/// and then failed a `GET_DESCRIPTOR` was indistinguishable from one that
/// never got a slot. Every stage now reports separately.
#[derive(Debug, Clone, Copy)]
pub struct PortOutcome {
    pub port: u8,
    pub portsc: u32,
    pub speed: u8,
    pub slot: u8,
    /// Last stage attempted — `"reset"`, `"slot"`, `"addr"`, `"desc8"`,
    /// `"desc18"`, `"cfg9"`, `"cfgN"`, `"setcfg"`, `"ep"`, `"proto"`,
    /// `"done"`.
    pub stage: &'static str,
    pub error: Option<XhciError>,
    pub addressed: bool,
    pub keyboard: bool,
    pub mouse: bool,
    pub storage: bool,
    /// `idVendor`/`idProduct`, once the device descriptor has been read.
    /// Zero before that. Reported per port because the port *number* alone
    /// does not say which physical device is which — matching a failing
    /// port against `lsusb` output needs the IDs, and on the target machine
    /// that match is the whole difference between "the keyboard failed" and
    /// "some other device failed".
    pub vendor: u16,
    pub product: u16,
}

impl PortOutcome {
    fn new(port: u8, portsc: u32) -> Self {
        PortOutcome {
            port,
            portsc,
            speed: 0,
            slot: 0,
            stage: "reset",
            error: None,
            addressed: false,
            keyboard: false,
            mouse: false,
            storage: false,
            vendor: 0,
            product: 0,
        }
    }
}

impl PortScan {
    pub fn add(&mut self, other: &PortScan) {
        self.ports += other.ports;
        self.connected += other.connected;
        self.addressed += other.addressed;
        self.other_devices += other.other_devices;
        self.failed += other.failed;
        self.keyboards += other.keyboards;
        self.mice += other.mice;
        self.storage += other.storage;
    }
}

// ── DMA pages ────────────────────────────────────────────────────────────────

/// One 4 KiB physically-contiguous, zeroed page, reachable both by the CPU
/// (through the bootloader's physical-memory window) and by the controller
/// (by physical address).
///
/// Cacheable on purpose — unlike the register window, which
/// `memory::mmio::map` maps uncached. x86 DMA is cache-coherent, so ring
/// and context memory is ordinary RAM as far as correctness goes, and
/// making it uncached would only slow every TRB read down.
///
/// Never freed: these live for the lifetime of the controller, which is
/// the lifetime of the kernel.
#[derive(Clone, Copy)]
struct Dma {
    phys: u64,
    virt: *mut u8,
}

impl Dma {
    fn alloc() -> Result<Dma> {
        // SAFETY: order 12 = one 4 KiB frame, the granularity every other
        // DMA user here (ac97's BDL/ring buffers) asks the Buddy for.
        let phys = unsafe { phys_alloc(12) }.ok_or(XhciError::NoMemory)?;
        let virt = (crate::memory::physical_memory_offset() + phys.as_u64()).as_mut_ptr::<u8>();
        // SAFETY: a freshly allocated frame, mapped by the bootloader's
        // physical window, owned exclusively by this driver from here on.
        unsafe { core::ptr::write_bytes(virt, 0, 4096) };
        Ok(Dma { phys: phys.as_u64(), virt })
    }

    fn write_u32(&self, byte_offset: usize, value: u32) {
        unsafe { core::ptr::write_volatile(self.virt.add(byte_offset) as *mut u32, value) };
    }

    fn read_u32(&self, byte_offset: usize) -> u32 {
        unsafe { core::ptr::read_volatile(self.virt.add(byte_offset) as *const u32) }
    }

    fn write_u64(&self, byte_offset: usize, value: u64) {
        unsafe { core::ptr::write_volatile(self.virt.add(byte_offset) as *mut u64, value) };
    }

    fn read_bytes(&self, byte_offset: usize, out: &mut [u8]) {
        unsafe { core::ptr::copy_nonoverlapping(self.virt.add(byte_offset), out.as_mut_ptr(), out.len()) };
    }

    /// Reads one TRB out of this page, payload first.
    ///
    /// Only valid for **software-owned** memory — a transfer or command
    /// ring this driver wrote itself (`Ring::push` re-reading its own Link
    /// TRB). It must not be used on the event ring, where the controller is
    /// the writer and the cycle bit has to be read before the payload; see
    /// `Xhci::next_event`.
    fn trb(&self, index: usize) -> Trb {
        let base = index * TRB_BYTES;
        Trb([
            self.read_u32(base),
            self.read_u32(base + 4),
            self.read_u32(base + 8),
            self.read_u32(base + 12),
        ])
    }

    /// Writes one TRB, **cycle bit last**. The controller owns a TRB the
    /// instant its cycle bit matches the ring's current state, so writing
    /// dword 3 first would hand over a TRB whose pointer and length fields
    /// are still whatever the previous lap left there.
    fn write_trb(&self, index: usize, trb: Trb, cycle: bool) {
        let base = index * TRB_BYTES;
        self.write_u32(base, trb.0[0]);
        self.write_u32(base + 4, trb.0[1]);
        self.write_u32(base + 8, trb.0[2]);
        compiler_fence(Ordering::SeqCst);
        self.write_u32(base + 12, trb.with_cycle(cycle).0[3]);
    }

    fn trb_phys(&self, index: usize) -> u64 {
        self.phys + (index * TRB_BYTES) as u64
    }
}

/// A ring segment: its page plus the software-side enqueue/cycle state.
struct Ring {
    dma: Dma,
    state: ProducerRing,
}

impl Ring {
    /// Allocates a segment and writes its closing Link TRB. The link is
    /// stamped with cycle 0 — the opposite of the ring's initial producer
    /// cycle — so the controller stops there until the first lap actually
    /// closes and `push` flips it.
    fn alloc() -> Result<Ring> {
        let dma = Dma::alloc()?;
        let state = ProducerRing::new(RING_TRBS);
        dma.write_trb(RING_TRBS - 1, Trb::link(dma.phys, false), false);
        Ok(Ring { dma, state })
    }

    /// Enqueues one TRB, returning the physical address it was written to
    /// (which is what the matching Transfer/Command Completion event will
    /// point back at).
    fn push(&mut self, trb: Trb) -> u64 {
        let e = self.state.enqueue();
        self.dma.write_trb(e.index, trb, e.cycle);
        if let Some((link_index, link_cycle)) = e.stamp_link {
            // After the TRB, never before: the link must not become valid
            // while the slot it leads back to is still being written.
            compiler_fence(Ordering::SeqCst);
            let link = self.dma.trb(link_index);
            self.dma.write_trb(link_index, link, link_cycle);
        }
        self.dma.trb_phys(e.index)
    }
}

// ── Per-device state ─────────────────────────────────────────────────────────

/// What `configure_device` made of a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceKind {
    /// At least one of the two is true.
    Hid { keyboard: bool, mouse: bool },
    Storage,
    Other,
}

/// One HID boot interface's interrupt IN endpoint, with exactly one
/// transfer outstanding on it at all times (see `queue_hid_report`).
struct HidEndpoint {
    /// Device Context Index of the interrupt IN endpoint.
    dci: u8,
    ring: Ring,
    /// DMA page the boot reports land in.
    report: Dma,
    report_len: u16,
}

/// A HID boot keyboard this driver is actively polling.
struct Keyboard {
    ep: HidEndpoint,
    decoder: hal::hid::BootKeyboard,
}

/// Which of a device's HID endpoints a transfer event or a re-arm is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HidRole {
    Keyboard,
    Mouse,
}

/// One addressed USB device.
struct Device {
    speed: u8,
    input: Dma,
    /// The output device context — the controller's own copy, pointed at
    /// by the DCBAA. Kept because Configure Endpoint has to be issued
    /// against a slot whose context already exists.
    _output: Dma,
    ep0: Ring,
    /// Scratch page for control-transfer data stages.
    buf: Dma,
    keyboard: Option<Keyboard>,
    /// A boot mouse needs no decoder state: each report is already
    /// relative motion plus button levels (`hal::hid::decode_boot_mouse`).
    mouse: Option<HidEndpoint>,
    storage: Option<msc::MassStorage>,
}

impl Device {
    fn hid(&mut self, role: HidRole) -> Option<&mut HidEndpoint> {
        match role {
            HidRole::Keyboard => self.keyboard.as_mut().map(|k| &mut k.ep),
            HidRole::Mouse => self.mouse.as_mut(),
        }
    }
}

// ── The controller ───────────────────────────────────────────────────────────

pub struct Xhci {
    /// Operational, runtime and doorbell register bases (virtual,
    /// uncached), all derived from the capability base at init time —
    /// which is itself not kept, since nothing after bring-up reads the
    /// capability registers again.
    op: *mut u8,
    run: *mut u8,
    db: *mut u32,
    context_size: usize,
    max_ports: u8,
    enabled_slots: u8,

    dcbaa: Dma,
    cmd: Ring,
    event: Dma,
    event_state: EventRing,
    _erst: Dma,

    devices: [Option<Device>; MAX_SLOTS as usize],
    /// How many unexpected events have been logged — see
    /// `handle_async_event`.
    async_events_logged: u32,

    /// Decoded keyboard scancodes awaiting `poll` — see [`PENDING_KEYS`].
    pending_keys: [u8; PENDING_KEYS],
    pending_len: usize,
}

// SAFETY: the raw pointers are a fixed MMIO window and permanently-owned
// DMA pages; the struct is only ever reached through the `Mutex` in
// `usb::mod`.
unsafe impl Send for Xhci {}

impl Xhci {
    // ── Raw register access ─────────────────────────────────────────────

    fn read32(base: *mut u8, offset: usize) -> u32 {
        unsafe { core::ptr::read_volatile(base.add(offset) as *const u32) }
    }

    fn write32(base: *mut u8, offset: usize, value: u32) {
        unsafe { core::ptr::write_volatile(base.add(offset) as *mut u32, value) };
    }

    /// 64-bit registers are written as two 32-bit stores, low half first —
    /// what Linux's `xhci_write_64` does. Some controllers reject a single
    /// 64-bit store to these registers, and none rejects the split form.
    fn write64(base: *mut u8, offset: usize, value: u64) {
        Self::write32(base, offset, value as u32);
        Self::write32(base, offset + 4, (value >> 32) as u32);
    }

    fn op_read(&self, offset: usize) -> u32 {
        Self::read32(self.op, offset)
    }
    fn op_write(&self, offset: usize, value: u32) {
        Self::write32(self.op, offset, value)
    }
    fn portsc_offset(port: u8) -> usize {
        x::OP_PORTSC + (port as usize - 1) * x::PORT_REGISTER_STRIDE
    }

    fn doorbell(&self, slot: u8, dci: u8) {
        // Every TRB write must be visible before the doorbell that tells
        // the controller to go look at it.
        compiler_fence(Ordering::SeqCst);
        unsafe {
            core::ptr::write_volatile(self.db.add(slot as usize), x::doorbell_value(dci));
        }
    }

    // ── Bring-up ────────────────────────────────────────────────────────

    /// Maps a controller's registers and brings it up to the point where
    /// commands can be issued. Does not enumerate ports — see
    /// [`Xhci::enumerate_ports`].
    pub fn init(bar: u64) -> Result<Xhci> {
        // The capability header is enough to learn the real window size,
        // but mapping a fixed 64 KiB covers every controller's cap +
        // operational + runtime + doorbell space with room to spare; the
        // largest of those, the port register array, tops out at 0x400 +
        // 255 * 0x10.
        let cap = unsafe { crate::memory::mmio::map(PhysAddr::new(bar), 0x10000) }
            .ok_or(XhciError::NoMemory)?
            .as_mut_ptr::<u8>();

        // CAPLENGTH and HCIVERSION share one dword (low byte, high half)
        // — HCIVERSION cannot be read as an aligned u32 of its own.
        let caplength_dword = Self::read32(cap, x::CAP_CAPLENGTH);
        let caplength = (caplength_dword & 0xFF) as usize;
        let hcsparams1 = Self::read32(cap, x::CAP_HCSPARAMS1);
        let hcsparams2 = Self::read32(cap, x::CAP_HCSPARAMS2);
        let hccparams1 = Self::read32(cap, x::CAP_HCCPARAMS1);
        let dboff = Self::read32(cap, x::CAP_DBOFF) as usize & !0x3;
        let rtsoff = Self::read32(cap, x::CAP_RTSOFF) as usize & !0x1F;

        let max_ports = x::hcsparams1_ports(hcsparams1);
        let max_slots = x::hcsparams1_slots(hcsparams1);
        let context_size = x::hccparams1_context_size(hccparams1);

        crate::ktrace!(
            crate::debug::USB,
            "xhci: caplength={:#x} slots={} ports={} ctxsize={} ac64={} version={:#06x}",
            caplength,
            max_slots,
            max_ports,
            context_size,
            x::hccparams1_ac64(hccparams1),
            x::hciversion(caplength_dword),
        );

        if max_ports == 0 || max_slots == 0 {
            return Err(XhciError::Unusable);
        }

        let op = unsafe { cap.add(caplength) };
        let run = unsafe { cap.add(rtsoff) };
        let db = unsafe { cap.add(dboff) as *mut u32 };

        // 1. Take ownership away from the firmware before touching
        //    anything else. Skipping this leaves the BIOS's SMM handler
        //    fielding USB interrupts for a controller the OS is
        //    reprogramming underneath it — on a machine whose firmware
        //    implements legacy keyboard emulation, that is a live SMI
        //    source racing every register write below.
        Self::take_ownership(cap, hccparams1);

        let mut ctrl = Xhci {
            op,
            run,
            db,
            context_size,
            max_ports,
            enabled_slots: max_slots.min(MAX_SLOTS),
            dcbaa: Dma::alloc()?,
            cmd: Ring::alloc()?,
            event: Dma::alloc()?,
            event_state: EventRing::new(RING_TRBS),
            _erst: Dma::alloc()?,
            devices: [const { None }; MAX_SLOTS as usize],
            async_events_logged: 0,
            pending_keys: [0; PENDING_KEYS],
            pending_len: 0,
        };

        ctrl.reset()?;
        ctrl.setup_rings(hcsparams2)?;
        ctrl.start()?;
        Ok(ctrl)
    }

    /// Walks the extended capability list for USB Legacy Support and
    /// performs the BIOS→OS ownership handshake (xHCI §4.22.1).
    fn take_ownership(cap: *mut u8, hccparams1: u32) {
        let mut offset = x::hccparams1_xecp_offset(hccparams1);
        let mut guard = 0;
        while offset != 0 && guard < 64 {
            guard += 1;
            let header = Self::read32(cap, offset);
            if header == 0xFFFF_FFFF {
                return;
            }
            if x::xecp_id(header) == x::XECP_ID_LEGACY_SUPPORT {
                let legsup = Self::read32(cap, offset);
                if legsup & x::LEGSUP_BIOS_OWNED == 0 && legsup & x::LEGSUP_OS_OWNED != 0 {
                    return; // already ours
                }
                Self::write32(cap, offset, legsup | x::LEGSUP_OS_OWNED);

                let mut spins = 0u32;
                while Self::read32(cap, offset) & x::LEGSUP_BIOS_OWNED != 0 && spins < 1_000_000 {
                    spins += 1;
                    core::hint::spin_loop();
                }
                if Self::read32(cap, offset) & x::LEGSUP_BIOS_OWNED != 0 {
                    crate::serial_println!("xhci: firmware never released the controller — forcing");
                    // Forcing is the lesser evil: the alternative is no
                    // keyboard at all. Clear the BIOS bit ourselves.
                    let v = Self::read32(cap, offset);
                    Self::write32(cap, offset, (v & !x::LEGSUP_BIOS_OWNED) | x::LEGSUP_OS_OWNED);
                }

                // USBLEGCTLSTS, 4 bytes on: disable every SMI the firmware
                // asked for and acknowledge the write-1-to-clear status
                // bits (31:29), so no SMI fires for events this driver is
                // about to cause.
                let ctlsts = Self::read32(cap, offset + 4);
                Self::write32(cap, offset + 4, (ctlsts & 0x1FFF_0000) | 0xE000_0000);
                return;
            }
            let next = x::xecp_next_offset(header);
            if next == 0 {
                return;
            }
            offset += next;
        }
    }

    /// Halts and resets the controller (§4.2, step 1-3).
    fn reset(&mut self) -> Result<()> {
        // Stop it first — resetting a running controller is undefined.
        let cmd = self.op_read(x::OP_USBCMD);
        if cmd & x::USBCMD_RS != 0 {
            self.op_write(x::OP_USBCMD, cmd & !x::USBCMD_RS);
        }
        self.wait_for(|s| s.op_read(x::OP_USBSTS) & x::USBSTS_HCH != 0, 1000)
            .map_err(|_| {
                crate::serial_println!("xhci: controller never halted");
                XhciError::Timeout
            })?;

        self.op_write(x::OP_USBCMD, x::USBCMD_HCRST);
        // HCRST self-clears when the reset completes, and CNR ("Controller
        // Not Ready") stays set until the register file is usable. Both
        // must be waited on — a driver that only waits for HCRST writes
        // its rings into a controller that is still initialising.
        self.wait_for(|s| s.op_read(x::OP_USBCMD) & x::USBCMD_HCRST == 0, 1000)
            .map_err(|_| {
                crate::serial_println!("xhci: reset never completed");
                XhciError::Timeout
            })?;
        self.wait_for(|s| s.op_read(x::OP_USBSTS) & x::USBSTS_CNR == 0, 1000)
            .map_err(|_| {
                crate::serial_println!("xhci: controller not ready after reset");
                XhciError::Timeout
            })?;
        Ok(())
    }

    /// Publishes the DCBAA, scratchpad, command ring and event ring
    /// (§4.2, steps 4-8).
    fn setup_rings(&mut self, hcsparams2: u32) -> Result<()> {
        // MaxSlotsEn — the controller ignores Enable Slot commands beyond
        // this count.
        self.op_write(x::OP_CONFIG, self.enabled_slots as u32);

        // Scratchpad: memory the controller wants for its own internal
        // use. DCBAA entry 0 points at an array of buffer pointers.
        let scratchpads = x::hcsparams2_max_scratchpad(hcsparams2);
        if scratchpads > 0 {
            let array = Dma::alloc()?;
            let capacity = 4096 / 8;
            if scratchpads as usize > capacity {
                // One page holds 512 pointers; no real controller asks for
                // more, and silently under-providing would hand the
                // controller memory it doesn't own.
                crate::serial_println!("xhci: {} scratchpad buffers is more than supported", scratchpads);
                return Err(XhciError::Unusable);
            }
            for i in 0..scratchpads as usize {
                let page = Dma::alloc()?;
                array.write_u64(i * 8, page.phys);
            }
            self.dcbaa.write_u64(0, array.phys);
            crate::ktrace!(
            crate::debug::USB,
                "xhci: {} scratchpad buffers, array={:#x}", scratchpads, array.phys
            );
        }
        crate::ktrace!(
            crate::debug::USB,
            "xhci: dcbaa={:#x} cmd={:#x} evt={:#x} erst={:#x}",
            self.dcbaa.phys, self.cmd.dma.phys, self.event.phys, self._erst.phys,
        );
        Self::write64(self.op, x::OP_DCBAAP, self.dcbaa.phys);

        // Command ring, starting on cycle state 1 to match `ProducerRing`.
        Self::write64(self.op, x::OP_CRCR, self.cmd.dma.phys | x::CRCR_RCS);

        // Event ring: one segment, described by a one-entry ERST.
        // ERST entry layout (§6.5): ring segment base (64 bits), size in
        // TRBs (16 bits), reserved.
        self._erst.write_u64(0, self.event.phys);
        self._erst.write_u32(8, RING_TRBS as u32);
        self._erst.write_u32(12, 0);

        Self::write32(self.run, x::RUN_IR0 + x::IR_ERSTSZ, 1);
        // Dequeue pointer before the table base: the controller may start
        // consuming the moment ERSTBA lands.
        Self::write64(self.run, x::RUN_IR0 + x::IR_ERDP, self.event.phys);
        Self::write64(self.run, x::RUN_IR0 + x::IR_ERSTBA, self._erst.phys);
        // Interrupt moderation off; nothing is listening on an interrupt
        // line anyway (see the module comment).
        Self::write32(self.run, x::RUN_IR0 + x::IR_IMOD, 0);

        // Read back what the controller actually kept. A register that
        // does not hold what was written is the one failure mode that
        // makes every later step inexplicable, and it costs four reads to
        // rule out.
        crate::ktrace!(
            crate::debug::USB,
            "xhci: readback config={:#x} (wanted {}) dcbaap={:#x} crcr={:#x} erstsz={} pagesize={:#x}",
            self.op_read(x::OP_CONFIG) & 0xFF,
            self.enabled_slots,
            Self::read32(self.op, x::OP_DCBAAP),
            Self::read32(self.op, x::OP_CRCR),
            Self::read32(self.run, x::RUN_IR0 + x::IR_ERSTSZ),
            self.op_read(x::OP_PAGESIZE),
        );
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        let cmd = self.op_read(x::OP_USBCMD);
        self.op_write(x::OP_USBCMD, cmd | x::USBCMD_RS);
        self.wait_for(|s| s.op_read(x::OP_USBSTS) & x::USBSTS_HCH == 0, 1000)
            .map_err(|_| {
                crate::serial_println!("xhci: controller did not start");
                XhciError::Timeout
            })?;

        crate::ktrace!(
            crate::debug::USB,
            "xhci: running, usbsts={:#010x} usbcmd={:#010x}",
            self.op_read(x::OP_USBSTS), self.op_read(x::OP_USBCMD),
        );

        // A No Op command round-trips the command ring and the event ring
        // in one step: if this completes, the DCBAA/CRCR/ERST addresses
        // are all good and events are reaching us. Failing here, rather
        // than three enumeration steps later, is worth one command.
        let trb_phys = self.cmd.push(Trb::no_op_command(self.cmd.state.cycle()));
        self.doorbell(0, 0);
        match self.wait_for_command(trb_phys, 1000) {
            Ok(_) => Ok(()),
            Err(e) => {
                crate::serial_println!("xhci: No Op command failed ({:?}) — controller unusable", e);
                Err(e)
            }
        }
    }

    // ── Waiting ─────────────────────────────────────────────────────────

    /// Spins until `cond` holds or `timeout_ms` elapses.
    ///
    /// Bounded twice over: by the monotonic clock *and* by a spin count.
    /// The clock is the real bound, but this runs during boot with
    /// interrupts still masked, and if the selected clocksource ever falls
    /// back to jiffies (which only advance from the timer ISR) it would
    /// never move — turning a "best-effort, never hangs boot" probe into a
    /// dead machine. The spin cap makes that impossible.
    fn wait_for(&self, cond: impl Fn(&Self) -> bool, timeout_ms: u64) -> Result<()> {
        let start = crate::time::ktime_get();
        let limit_ns = timeout_ms * 1_000_000;
        let mut spins: u64 = 0;
        loop {
            if cond(self) {
                return Ok(());
            }
            if crate::time::ktime_get().wrapping_sub(start) > limit_ns {
                return Err(XhciError::Timeout);
            }
            spins += 1;
            if spins > 200_000_000 {
                return Err(XhciError::Timeout);
            }
            core::hint::spin_loop();
        }
    }

    /// Bounded busy-wait, same dual bound as `wait_for`. USB is full of
    /// mandatory settling delays (a port needs ~20 ms after reset before
    /// it will answer) and there is no sleep available at this point in
    /// boot.
    fn delay_ms(&self, ms: u64) {
        let _ = self.wait_for(|_| false, ms);
    }

    /// Publishes the event ring dequeue pointer and clears Event Handler
    /// Busy. Without this the controller eventually stops posting events:
    /// it believes software is still working through the ones it already
    /// wrote.
    fn update_erdp(&self) {
        let phys = self.event.trb_phys(self.event_state.dequeue_index());
        Self::write64(self.run, x::RUN_IR0 + x::IR_ERDP, phys | x::ERDP_EHB);
    }

    /// Takes the next event TRB, if the controller has posted one.
    ///
    /// **Dword 3 is read first, on purpose.** It carries the cycle bit, and
    /// the controller writes it *last* — that write is what hands the TRB
    /// over. Reading the payload first and the ownership flag second races
    /// the DMA write: dwords 0-2 can be sampled before the controller
    /// writes them and dword 3 after, yielding an event whose type, slot
    /// and endpoint are fresh while its completion code and TRB pointer are
    /// still zero.
    ///
    /// That is not hypothetical — it is the bug that made this driver fail
    /// on real hardware while passing in QEMU, where the device model
    /// writes the whole TRB atomically with respect to the guest and the
    /// window simply does not exist. It showed on screen as
    /// `slot 3 ep0 unknown-trb stage failed: Failed(0) ? (trb=0x0)`: a
    /// completion code of 0, which the specification never assigns, next to
    /// a null pointer. Each torn read also *consumed* the slot, so the real
    /// event that followed was lost and its transfer timed out.
    ///
    /// Note the asymmetry this fixes: `Dma::write_trb` already wrote the
    /// cycle bit last for exactly the mirror-image reason. The producer side
    /// was right and the consumer side was backwards.
    fn next_event(&mut self) -> Option<Trb> {
        let index = self.event_state.dequeue_index();
        let base = index * TRB_BYTES;

        let dword3 = self.event.read_u32(base + 12);
        if !self.event_state.is_ready(dword3 & 1 != 0) {
            return None;
        }
        // Ownership is confirmed, so dwords 0-2 are already written.
        compiler_fence(Ordering::Acquire);

        let trb = Trb([
            self.event.read_u32(base),
            self.event.read_u32(base + 4),
            self.event.read_u32(base + 8),
            dword3,
        ]);

        self.event_state.advance();
        self.update_erdp();
        Some(trb)
    }

    /// Waits for the Command Completion event belonging to a specific
    /// command TRB, dispatching anything else that arrives meanwhile.
    fn wait_for_command(&mut self, cmd_trb_phys: u64, timeout_ms: u64) -> Result<Trb> {
        let start = crate::time::ktime_get();
        let limit_ns = timeout_ms * 1_000_000;
        let mut spins: u64 = 0;
        loop {
            let mine = |t: &Trb| {
                t.trb_type() == x::TRB_COMMAND_COMPLETION_EVENT && t.pointer() == cmd_trb_phys
            };
            if let Some(trb) = self.service_events(&mine) {
                return if trb.completion_code() == x::COMP_SUCCESS {
                    Ok(trb)
                } else {
                    Err(XhciError::Failed(trb.completion_code()))
                };
            }
            if self.op_read(x::OP_USBSTS) & (x::USBSTS_HCE | x::USBSTS_HSE) != 0 {
                crate::serial_println!("xhci: host controller error while waiting for a command");
                return Err(XhciError::Unusable);
            }
            if crate::time::ktime_get().wrapping_sub(start) > limit_ns {
                return Err(XhciError::Timeout);
            }
            spins += 1;
            if spins > 200_000_000 {
                return Err(XhciError::Timeout);
            }
            core::hint::spin_loop();
        }
    }

    /// **The only reader of the event ring.** Drains it, routing every
    /// event to where it belongs, and returns the first one `want` accepts
    /// — leaving everything after it on the ring for the next call.
    ///
    /// Why one reader: the ring is shared by every command and every
    /// endpoint on the controller. Before mass storage there were two
    /// readers — `poll` for the keyboard, and each waiter for its own
    /// completion — and each treated whatever it found as its own or as
    /// noise. A keyboard report that arrived while enumeration was waiting
    /// on another device's control transfer was logged and dropped, and
    /// with it the keyboard's only outstanding transfer: nothing re-armed
    /// it and the keyboard went silent for good. A disk read spinning for
    /// its completion would have done the same to every keystroke typed
    /// during it — and, in the other direction, `poll` would have swallowed
    /// the disk's completion, which then times out a second later disguised
    /// as a plain `Timeout`, the failure shape that cost three bare-metal
    /// cycles on the keyboard driver.
    ///
    /// So routing lives here, once: keyboard transfer events are decoded
    /// into `pending_keys` and re-armed no matter who is draining; the
    /// caller's own event comes back to it; anything else is logged. The
    /// caller always holds the `CONTROLLERS` lock, which is what makes
    /// "whoever is draining" a single party at any instant.
    ///
    /// Bounded to one ring's worth of events per call, so a controller
    /// flooding events cannot pin the timer ISR.
    fn service_events(&mut self, want: &dyn Fn(&Trb) -> bool) -> Option<Trb> {
        for _ in 0..RING_TRBS {
            let trb = self.next_event()?;
            if self.handle_hid_event(&trb) {
                continue;
            }
            if want(&trb) {
                return Some(trb);
            }
            self.handle_async_event(trb);
        }
        None
    }

    /// Consumes a Transfer Event if it belongs to a keyboard's or a
    /// mouse's interrupt endpoint: a keyboard report is decoded into
    /// `pending_keys`, a mouse report goes straight onto the mouse event
    /// queue (`mouse::push_usb_event` takes no lock but its own, so it is
    /// safe under `CONTROLLERS`); either way the transfer is re-armed.
    /// Returns whether it was one of those.
    fn handle_hid_event(&mut self, trb: &Trb) -> bool {
        if trb.trb_type() != x::TRB_TRANSFER_EVENT {
            return false;
        }
        let slot = trb.slot_id();
        if slot == 0 || slot as usize > self.devices.len() {
            return false;
        }
        let mut report = [0u8; 8];
        let (role, report_len) = {
            let Some(dev) = self.devices[slot as usize - 1].as_mut() else {
                return false;
            };
            let role = if dev.keyboard.as_ref().is_some_and(|k| k.ep.dci == trb.endpoint_id()) {
                HidRole::Keyboard
            } else if dev.mouse.as_ref().is_some_and(|m| m.dci == trb.endpoint_id()) {
                HidRole::Mouse
            } else {
                return false;
            };
            let ep = dev.hid(role).expect("role was just matched");
            ep.report.read_bytes(0, &mut report);
            (role, ep.report_len)
        };

        let code = trb.completion_code();
        let ok = code == x::COMP_SUCCESS || code == x::COMP_SHORT_PACKET;
        let valid = (report_len as u32).saturating_sub(trb.transfer_length()) as usize;
        if ok && role == HidRole::Mouse {
            if let Some(ev) = hal::hid::decode_boot_mouse(&report[..valid.min(report.len())]) {
                crate::debug::inc_usb_mouse_reports();
                crate::mouse::push_usb_event(ev);
            }
        } else if ok {
            let mut decoded = [0u8; 2 * hal::hid::MAX_EVENTS];
            let n = self.decode_report(slot, &report[..valid.min(report.len())], &mut decoded);
            let room = PENDING_KEYS - self.pending_len;
            let take = n.min(room);
            self.pending_keys[self.pending_len..self.pending_len + take].copy_from_slice(&decoded[..take]);
            self.pending_len += take;
            if take < n {
                crate::debug::add_usb_keys_dropped((n - take) as u64);
            }
        }

        // Re-arm regardless of completion code: a stalled endpoint would
        // need a Reset Endpoint command this driver doesn't issue, but a
        // transient error must not silently end input.
        let _ = self.queue_hid_report(slot, role);
        true
    }

    /// Events that arrive while waiting for something else — port-status
    /// changes, and transfer events belonging to another endpoint.
    ///
    /// Logged rather than dropped, up to a bound. Silently discarding these
    /// is what hid the real cause of every control-transfer failure in the
    /// first bare-metal run (see `control_transfer`); the bound keeps a
    /// stuck controller from filling the log with the same line.
    fn handle_async_event(&mut self, trb: Trb) {
        const MAX_LOGGED: u32 = 16;
        if self.async_events_logged >= MAX_LOGGED {
            return;
        }
        self.async_events_logged += 1;
        crate::ktrace!(
            crate::debug::USB,
            "xhci: unhandled event type={} slot={} ep={} cc={} ptr={:#x}",
            trb.trb_type(), trb.slot_id(), trb.endpoint_id(), trb.completion_code(), trb.pointer(),
        );
    }

    // ── Port enumeration ────────────────────────────────────────────────

    /// Scans the root hub ports and sets up every device found. Returns
    /// how many HID boot keyboards are now being polled.
    pub fn enumerate_ports(&mut self) -> PortScan {
        let mut scan = PortScan { ports: self.max_ports as usize, ..PortScan::default() };
        for port in 1..=self.max_ports {
            let offset = Self::portsc_offset(port);
            let sc = self.op_read(offset);
            if sc == 0xFFFF_FFFF {
                continue; // register window ends here
            }

            // Port Power is software-controlled on some controllers; a
            // port with no power reports no connection, so a driver that
            // skips this simply never sees the device.
            if sc & x::PORTSC_PP == 0 {
                self.op_write(offset, x::portsc_write(sc, x::PORTSC_PP));
                self.delay_ms(20);
            }

            let sc = self.op_read(offset);
            if sc & x::PORTSC_CCS == 0 {
                continue; // nothing plugged in
            }
            scan.connected += 1;

            let out = self.setup_port(port, sc);

            // One line per port, carrying every stage's result — this is
            // what gets read off a screen on a machine with no serial.
            match out.error {
                None => crate::ktrace!(
            crate::debug::USB,
                    "xhci: p{} spd={} slot={} [{:04x}:{:04x}] {} OK{}",
                    out.port, out.speed, out.slot, out.vendor, out.product, out.stage,
                    match (out.keyboard, out.mouse) {
                        (true, true) => " KBD+MOUSE",
                        (true, false) => " KBD",
                        (false, true) => " MOUSE",
                        _ if out.storage => " STORAGE",
                        _ => "",
                    },
                ),
                Some(e) => crate::serial_println!(
                    "xhci: p{} spd={} slot={} [{:04x}:{:04x}] sc={:#x} FAIL at {} ({:?} {})",
                    out.port, out.speed, out.slot, out.vendor, out.product,
                    out.portsc, out.stage, e, describe(e),
                ),
            }

            if out.addressed {
                scan.addressed += 1;
            }
            if out.keyboard {
                scan.keyboards += 1;
            }
            if out.mouse {
                scan.mice += 1;
            }
            if out.storage {
                scan.storage += 1;
            } else if !out.keyboard && !out.mouse && out.error.is_none() {
                scan.other_devices += 1;
            }
            if out.error.is_some() {
                scan.failed += 1;
            }
        }
        scan
    }

    /// Resets (if needed), addresses and configures the device on `port`.
    /// Returns whether it turned out to be a keyboard.
    /// Resets (if needed), addresses and configures the device on `port`,
    /// recording how far it got. Never returns `Err`: the outcome *is* the
    /// report, and a stage that succeeded stays visible even when a later
    /// one fails — see [`PortOutcome`].
    fn setup_port(&mut self, port: u8, portsc: u32) -> PortOutcome {
        let mut out = PortOutcome::new(port, portsc);
        let offset = Self::portsc_offset(port);
        let mut sc = portsc;

        // USB 3 ports train and enable themselves on connect; USB 2 ports
        // need an explicit reset to get there.
        // Reset, retrying the way Linux's `hub_port_reset` does (it allows
        // several attempts before giving up on a port). A single attempt is
        // enough in QEMU and demonstrably not always enough on real
        // silicon, where a device can miss the first reset entirely.
        let mut attempt = 0;
        while sc & x::PORTSC_PED == 0 && attempt < 3 {
            attempt += 1;
            self.op_write(offset, x::portsc_write(sc, x::PORTSC_PR));
            let waited = self.wait_for(
                |s| {
                    let v = s.op_read(offset);
                    v & x::PORTSC_PRC != 0 || v & x::PORTSC_PED != 0
                },
                500,
            );
            // 20 ms recovery — twice USB 2.0 §7.1.7.5's TRSTRCY minimum,
            // since a device that answers late costs a whole retry.
            self.delay_ms(20);
            sc = self.op_read(offset);
            if waited.is_err() {
                crate::serial_println!(
                    "xhci: p{} reset attempt {} timed out (sc={:#010x})", port, attempt, sc
                );
            } else if sc & x::PORTSC_PED == 0 {
                crate::serial_println!(
                    "xhci: p{} reset attempt {} left port disabled (sc={:#010x})", port, attempt, sc
                );
            }
            // Acknowledge this attempt's change bits before retrying, or
            // the next wait sees a stale PRC and returns immediately.
            self.op_write(
                offset,
                x::portsc_write(sc, x::PORTSC_CSC | x::PORTSC_PRC | x::PORTSC_PEC | x::PORTSC_PLC),
            );
            sc = self.op_read(offset);
        }

        // A USB 3 port that was already enabled never went through the
        // loop above, so its connect-status change still needs clearing —
        // otherwise a later poll reads it as a fresh hot-plug.
        self.op_write(
            offset,
            x::portsc_write(sc, x::PORTSC_CSC | x::PORTSC_PRC | x::PORTSC_PEC | x::PORTSC_PLC),
        );

        let sc = self.op_read(offset);
        out.portsc = sc;
        if sc & x::PORTSC_PED == 0 {
            out.error = Some(XhciError::Timeout);
            return out;
        }
        out.speed = x::portsc_speed(sc);

        out.stage = "slot";
        let slot = match self.enable_slot() {
            Ok(s) => s,
            Err(e) => {
                out.error = Some(e);
                return out;
            }
        };
        out.slot = slot;

        out.stage = "addr";
        if let Err(first) = self.address_device(slot, port, out.speed) {
            // One retry, logged as such so a boot that only works because
            // of it says so rather than looking like it always worked. A
            // device that is still settling after its reset answers the
            // SET_ADDRESS late, which the controller reports as a plain
            // transaction error.
            crate::serial_println!(
                "xhci: p{} address_device failed ({:?} {}), retrying once",
                port, first, describe(first)
            );
            self.delay_ms(50);
            if let Err(second) = self.address_device(slot, port, out.speed) {
                out.error = Some(second);
                return out;
            }
            crate::serial_println!("xhci: p{} address_device succeeded on retry", port);
        }
        out.addressed = true;

        match self.configure_device(slot, &mut out) {
            Ok(kind) => {
                if let DeviceKind::Hid { keyboard, mouse } = kind {
                    out.keyboard = keyboard;
                    out.mouse = mouse;
                }
                out.storage = kind == DeviceKind::Storage;
                out.stage = "done";
            }
            Err(e) => out.error = Some(e),
        }
        out
    }

    fn enable_slot(&mut self) -> Result<u8> {
        let cycle = self.cmd.state.cycle();
        let trb_phys = self.cmd.push(Trb::enable_slot(cycle));
        self.doorbell(0, 0);
        let event = self.wait_for_command(trb_phys, 1000)?;
        let slot = event.slot_id();
        if slot == 0 || slot as usize > self.devices.len() {
            crate::serial_println!("xhci: Enable Slot returned unusable slot {}", slot);
            return Err(XhciError::Unusable);
        }
        Ok(slot)
    }

    /// Byte offset of a context inside an input context page. Index 0 is
    /// the Input Control Context, 1 the slot context, and *n+1* the
    /// context at Device Context Index *n*.
    fn input_ctx_offset(&self, index: usize) -> usize {
        index * self.context_size
    }

    /// Builds the input context for Address Device and issues the command.
    fn address_device(&mut self, slot: u8, port: u8, speed: u8) -> Result<()> {
        let input = Dma::alloc()?;
        let output = Dma::alloc()?;
        let ep0 = Ring::alloc()?;
        let buf = Dma::alloc()?;

        let mut ctrl_words = [0u32; 8];
        x::build_input_control(&mut ctrl_words, x::ADD_SLOT | x::add_flag(1));
        self.write_context(&input, 0, &ctrl_words);

        let mut slot_words = [0u32; 8];
        x::build_slot_context(&mut slot_words, speed, port, 1);
        self.write_context(&input, 1, &slot_words);

        let mut ep_words = [0u32; 8];
        x::build_endpoint_context(
            &mut ep_words,
            x::EP_TYPE_CONTROL,
            x::default_max_packet0(speed),
            0,
            ep0.dma.phys,
            8,
        );
        self.write_context(&input, 2, &ep_words);

        // The DCBAA entry must be live before the command runs — it is
        // where the controller writes the device context it builds.
        self.dcbaa.write_u64(slot as usize * 8, output.phys);

        let cycle = self.cmd.state.cycle();
        let trb_phys = self.cmd.push(Trb::address_device(input.phys, slot, cycle));
        self.doorbell(0, 0);
        self.wait_for_command(trb_phys, 1000)?;

        self.devices[slot as usize - 1] = Some(Device {
            speed,
            input,
            _output: output,
            ep0,
            buf,
            keyboard: None,
            mouse: None,
            storage: None,
        });
        Ok(())
    }

    /// Writes one 32- or 64-byte context out of eight dwords. The trailing
    /// dwords of a 64-byte context are reserved and must be zero, which
    /// they already are (the page was allocated zeroed and contexts are
    /// only written once).
    fn write_context(&self, page: &Dma, index: usize, words: &[u32; 8]) {
        let base = self.input_ctx_offset(index);
        for (i, &w) in words.iter().enumerate() {
            page.write_u32(base + i * 4, w);
        }
    }

    // ── Device configuration ────────────────────────────────────────────

    /// Reads the descriptors of an addressed device and sets up the ones
    /// this driver has a use for: a HID boot keyboard (interrupt endpoint,
    /// polled) or a Bulk-Only mass-storage device (see `msc.rs`).
    fn configure_device(&mut self, slot: u8, out: &mut PortOutcome) -> Result<DeviceKind> {
        out.stage = "desc8";
        // 1. First eight bytes of the device descriptor, for the real
        //    control max packet size.
        let mut header = [0u8; 8];
        let n = self.control_in(slot, SetupPacket::get_descriptor(usb::DESC_DEVICE, 0, 8), &mut header)?;
        let Some(desc) = usb::parse_device_descriptor(&header[..n]) else {
            return Ok(DeviceKind::Other);
        };

        let speed = self.device(slot)?.speed;
        if speed == x::SPEED_FULL && desc.max_packet0 as u16 != x::default_max_packet0(speed) {
            // A full-speed device's EP0 may be 8/16/32/64 bytes and only
            // the descriptor says which. The guess used for Address Device
            // has to be corrected before any longer transfer.
            out.stage = "evalctx";
            self.evaluate_max_packet(slot, desc.max_packet0 as u16)?;
        }

        // 2. The full 18-byte device descriptor, purely so the log can name
        //    the device by its USB IDs. Failing here is not fatal to
        //    enumeration — the IDs are diagnostics, not something this
        //    driver acts on — so the error is logged and the walk
        //    continues.
        out.stage = "desc18";
        let mut full = [0u8; 18];
        match self.control_in(slot, SetupPacket::get_descriptor(usb::DESC_DEVICE, 0, 18), &mut full) {
            Ok(n) => {
                if let Some(d) = usb::parse_device_descriptor(&full[..n]) {
                    out.vendor = d.vendor;
                    out.product = d.product;
                }
            }
            Err(e) => crate::serial_println!(
                "xhci: p{} device descriptor read failed ({:?} {}) — continuing",
                out.port, e, describe(e)
            ),
        }

        // 3. Configuration descriptor header, then the whole blob.
        out.stage = "cfg9";
        let mut cfg_header = [0u8; 9];
        let n = self.control_in(
            slot,
            SetupPacket::get_descriptor(usb::DESC_CONFIGURATION, 0, 9),
            &mut cfg_header,
        )?;
        let Some(total) = usb::config_total_length(&cfg_header[..n]) else {
            return Ok(DeviceKind::Other);
        };
        let total = total.min(MAX_CONFIG_BYTES);

        out.stage = "cfgN";
        let mut config = [0u8; MAX_CONFIG_BYTES as usize];
        let n = self.control_in(
            slot,
            SetupPacket::get_descriptor(usb::DESC_CONFIGURATION, 0, total),
            &mut config[..total as usize],
        )?;

        let kb = usb::find_boot_keyboard(&config[..n]);
        let mouse = usb::find_boot_mouse(&config[..n]);
        if kb.is_none() && mouse.is_none() {
            if let Some(ms) = usb::find_mass_storage(&config[..n]) {
                out.stage = "msc";
                return match self.configure_storage(slot, &ms) {
                    Ok(()) => Ok(DeviceKind::Storage),
                    Err(e) => {
                        crate::serial_println!("usb-storage: slot {} bring-up failed: {:?}", slot, e);
                        Err(match e {
                            MscError::Xhci(x) => x,
                            _ => XhciError::Unusable,
                        })
                    }
                };
            }
            crate::ktrace!(crate::debug::USB, "xhci: slot {} is neither a boot HID device nor storage", slot);
            return Ok(DeviceKind::Other);
        }
        for (what, hid) in [("keyboard", kb), ("mouse", mouse)] {
            if let Some(i) = hid {
                crate::serial_println!(
                    "xhci: slot {} boot {} on interface {} ep {:#04x} ({} bytes, bInterval={})",
                    slot, what, i.interface, i.ep_address, i.ep_max_packet, i.ep_interval
                );
            }
        }

        // 4. SET_CONFIGURATION before Configure Endpoint: the device must
        //    be in the configured state for its endpoints to exist. Both
        //    interfaces come from the same configuration blob, so they
        //    share its value.
        out.stage = "setcfg";
        let config_value = kb.or(mouse).map(|i| i.config_value).unwrap_or(1);
        self.control_out(slot, SetupPacket::set_configuration(config_value))?;
        out.stage = "ep";
        self.configure_hid_endpoints(slot, kb.as_ref(), mouse.as_ref())?;

        // 5. Boot protocol per interface — the whole reason no HID report
        //    descriptor parser is needed — then SET_IDLE so the device only
        //    reports on change. An interface that refuses boot protocol is
        //    dropped on its own (its endpoint stays configured but is never
        //    armed), so a mouse's refusal cannot cost the keyboard on the
        //    same receiver, or the other way round.
        out.stage = "proto";
        let mut live = (false, false);
        for (role, hid) in [(HidRole::Keyboard, kb), (HidRole::Mouse, mouse)] {
            let Some(i) = hid else { continue };
            if let Err(e) = self.control_out(slot, SetupPacket::set_boot_protocol(i.interface)) {
                crate::serial_println!(
                    "xhci: slot {} interface {} refused boot protocol ({:?} {}) — {:?} not used",
                    slot, i.interface, e, describe(e), role
                );
                let dev = self.device(slot)?;
                match role {
                    HidRole::Keyboard => dev.keyboard = None,
                    HidRole::Mouse => dev.mouse = None,
                }
                continue;
            }
            // SET_IDLE is optional and some devices STALL it; a failure
            // here costs nothing but redundant reports.
            let _ = self.control_out(slot, SetupPacket::set_idle(i.interface));
            self.queue_hid_report(slot, role)?;
            match role {
                HidRole::Keyboard => live.0 = true,
                HidRole::Mouse => live.1 = true,
            }
        }
        if live == (false, false) {
            return Err(XhciError::Unusable);
        }
        Ok(DeviceKind::Hid { keyboard: live.0, mouse: live.1 })
    }

    fn device(&mut self, slot: u8) -> Result<&mut Device> {
        self.devices
            .get_mut(slot as usize - 1)
            .and_then(|d| d.as_mut())
            .ok_or(XhciError::Unusable)
    }

    /// Patches EP0's Max Packet Size with an Evaluate Context command.
    fn evaluate_max_packet(&mut self, slot: u8, max_packet: u16) -> Result<()> {
        let (input_phys, input, ep0_phys) = {
            let dev = self.device(slot)?;
            (dev.input.phys, dev.input, dev.ep0.dma.phys)
        };

        let mut ctrl_words = [0u32; 8];
        // Only EP0 is being re-evaluated; the slot context is untouched.
        x::build_input_control(&mut ctrl_words, x::add_flag(1));
        self.write_context(&input, 0, &ctrl_words);

        let mut ep_words = [0u32; 8];
        x::build_endpoint_context(&mut ep_words, x::EP_TYPE_CONTROL, max_packet, 0, ep0_phys, 8);
        self.write_context(&input, 2, &ep_words);

        let cycle = self.cmd.state.cycle();
        let trb_phys = self.cmd.push(Trb::evaluate_context(input_phys, slot, cycle));
        self.doorbell(0, 0);
        self.wait_for_command(trb_phys, 1000)?;
        crate::serial_println!("xhci: slot {} EP0 max packet corrected to {}", slot, max_packet);
        Ok(())
    }

    /// Adds the HID interrupt IN endpoints (a keyboard's, a mouse's, or
    /// both) to the device with one Configure Endpoint command, each with
    /// its own transfer ring — the same single-command shape as
    /// `configure_storage`'s bulk pair.
    fn configure_hid_endpoints(
        &mut self,
        slot: u8,
        kb: Option<&usb::BootHidInterface>,
        mouse: Option<&usb::BootHidInterface>,
    ) -> Result<()> {
        let (input, speed) = {
            let dev = self.device(slot)?;
            (dev.input, dev.speed)
        };

        let mut endpoints: [Option<(HidRole, &usb::BootHidInterface, u8)>; 2] = [None, None];
        let mut add = x::ADD_SLOT;
        let mut max_dci = 1u8;
        for (n, (role, hid)) in [(HidRole::Keyboard, kb), (HidRole::Mouse, mouse)].into_iter().enumerate() {
            if let Some(i) = hid {
                let dci = x::endpoint_dci(i.ep_number(), true);
                add |= x::add_flag(dci);
                max_dci = max_dci.max(dci);
                endpoints[n] = Some((role, i, dci));
            }
        }

        let mut ctrl_words = [0u32; 8];
        x::build_input_control(&mut ctrl_words, add);
        self.write_context(&input, 0, &ctrl_words);

        // The slot context has to be re-supplied with Context Entries
        // raised to cover the highest new endpoint — the controller sizes
        // the device context from this field.
        let root_port = self.root_port_of(slot);
        let mut slot_words = [0u32; 8];
        x::build_slot_context(&mut slot_words, speed, root_port, max_dci);
        self.write_context(&input, 1, &slot_words);

        let mut built: [Option<(HidRole, HidEndpoint)>; 2] = [None, None];
        for (n, e) in endpoints.iter().enumerate() {
            let Some((role, i, dci)) = *e else { continue };
            let ring = Ring::alloc()?;
            let report = Dma::alloc()?;
            let mut ep_words = [0u32; 8];
            x::build_endpoint_context(
                &mut ep_words,
                x::EP_TYPE_INTERRUPT_IN,
                i.ep_max_packet,
                x::endpoint_interval(speed, i.ep_interval),
                ring.dma.phys,
                i.ep_max_packet,
            );
            self.write_context(&input, dci as usize + 1, &ep_words);
            // A boot keyboard report is exactly 8 bytes. A mouse's is 3 or
            // 4, but its endpoint may declare a larger packet (the report
            // protocol's size); a TRB shorter than what the device sends
            // is a babble error, so the mouse gets its full max packet
            // (capped — it is one DMA page) and `handle_hid_event` reads
            // only the first 8 bytes back.
            let report_len = match role {
                HidRole::Keyboard => i.ep_max_packet.min(8),
                HidRole::Mouse => i.ep_max_packet.clamp(3, 64),
            };
            built[n] = Some((role, HidEndpoint { dci, ring, report, report_len }));
        }

        let cycle = self.cmd.state.cycle();
        let trb_phys = self.cmd.push(Trb::configure_endpoint(input.phys, slot, cycle));
        self.doorbell(0, 0);
        self.wait_for_command(trb_phys, 1000)?;

        let dev = self.device(slot)?;
        for (role, ep) in built.into_iter().flatten() {
            match role {
                HidRole::Keyboard => {
                    dev.keyboard = Some(Keyboard { ep, decoder: hal::hid::BootKeyboard::new() })
                }
                HidRole::Mouse => dev.mouse = Some(ep),
            }
        }
        Ok(())
    }

    /// The root hub port a slot's device sits on. Recorded at Address
    /// Device time in the input slot context, which is still intact.
    fn root_port_of(&self, slot: u8) -> u8 {
        let Some(dev) = self.devices.get(slot as usize - 1).and_then(|d| d.as_ref()) else {
            return 0;
        };
        ((dev.input.read_u32(self.input_ctx_offset(1) + 4) >> 16) & 0xFF) as u8
    }

    /// Posts one Normal TRB on a HID endpoint's interrupt ring. The
    /// controller fills it at the endpoint's service interval and posts a
    /// Transfer Event; `handle_hid_event` re-arms it. Exactly one transfer
    /// is outstanding at a time, which is all a keyboard or a mouse needs
    /// and keeps the ring's state trivially correct.
    fn queue_hid_report(&mut self, slot: u8, role: HidRole) -> Result<()> {
        let dev = self.device(slot)?;
        let Some(ep) = dev.hid(role) else {
            return Err(XhciError::Unusable);
        };
        let cycle = ep.ring.state.cycle();
        ep.ring.push(Trb::normal(ep.report.phys, ep.report_len as u32, cycle));
        let dci = ep.dci;
        self.doorbell(slot, dci);
        Ok(())
    }

    // ── Control transfers ───────────────────────────────────────────────

    /// A control transfer with a device-to-host data stage. Returns how
    /// many bytes actually arrived (which may be fewer than requested —
    /// a short packet is normal, not an error).
    fn control_in(&mut self, slot: u8, setup: SetupPacket, out: &mut [u8]) -> Result<usize> {
        let len = out.len().min(setup.length as usize);
        let transferred = self.control_transfer(slot, setup, len as u16)?;
        let n = transferred.min(len);
        let dev = self.device(slot)?;
        dev.buf.read_bytes(0, &mut out[..n]);
        Ok(n)
    }

    /// A control transfer with no data stage.
    fn control_out(&mut self, slot: u8, setup: SetupPacket) -> Result<()> {
        self.control_transfer(slot, setup, 0).map(|_| ())
    }

    /// A control transfer, with one recovery attempt if the endpoint
    /// halts.
    ///
    /// A `Stall` is not a transient error: the endpoint stays Halted and
    /// silently completes nothing until Reset Endpoint and Set TR Dequeue
    /// Pointer have run (§4.6.8, §4.6.10). Without this, one stalled
    /// request ends every later transfer to that device — which is exactly
    /// the shape of the first bare-metal run, where a device would satisfy
    /// two or three requests and then fail every remaining one.
    fn control_transfer(&mut self, slot: u8, setup: SetupPacket, data_len: u16) -> Result<usize> {
        match self.control_transfer_once(slot, setup, data_len) {
            Err(XhciError::Failed(code)) if code == 6 => {
                crate::serial_println!("xhci: slot {} ep0 stalled — resetting endpoint", slot);
                self.recover_endpoint(slot, EP0_DCI)?;
                let out = self.control_transfer_once(slot, setup, data_len);
                crate::serial_println!(
                    "xhci: slot {} ep0 retry after stall: {}",
                    slot,
                    if out.is_ok() { "ok" } else { "failed again" },
                );
                out
            }
            other => other,
        }
    }

    /// Clears a halted endpoint and repositions its transfer ring. Both
    /// commands are required: Reset Endpoint alone leaves the controller's
    /// dequeue pointer parked on the TRB that failed.
    fn recover_endpoint(&mut self, slot: u8, dci: u8) -> Result<()> {
        let cycle = self.cmd.state.cycle();
        let trb = self.cmd.push(Trb::reset_endpoint(slot, dci, cycle));
        self.doorbell(0, 0);
        self.wait_for_command(trb, 1000)?;

        // Resume where software will write next, not where the controller
        // stopped — the failed TRB must not be replayed.
        let (ring_phys, cycle_state) = {
            let dev = self.device(slot)?;
            (
                dev.ep0.dma.trb_phys(dev.ep0.state.enqueue_index()),
                dev.ep0.state.cycle(),
            )
        };
        let cycle = self.cmd.state.cycle();
        let trb = self.cmd.push(Trb::set_tr_dequeue(slot, dci, ring_phys, cycle_state, cycle));
        self.doorbell(0, 0);
        self.wait_for_command(trb, 1000)?;
        Ok(())
    }

    /// Runs the three stages of a control transfer on EP0 and waits for
    /// the Status Stage's completion event.
    fn control_transfer_once(&mut self, slot: u8, setup: SetupPacket, data_len: u16) -> Result<usize> {
        let is_in = setup.is_in();
        let buf_phys = self.device(slot)?.buf.phys;

        let trt = if data_len == 0 {
            x::TRT_NO_DATA
        } else if is_in {
            x::TRT_IN_DATA
        } else {
            x::TRT_OUT_DATA
        };

        let (setup_trb_phys, data_trb_phys, status_trb_phys) = {
            let dev = self.device(slot)?;
            let ring = &mut dev.ep0;
            let c = ring.state.cycle();
            let setup_phys = ring.push(Trb::setup_stage(setup.to_bytes(), trt, c));

            let data_phys = if data_len > 0 {
                let c = ring.state.cycle();
                Some(ring.push(Trb::data_stage(buf_phys, data_len as u32, is_in, c)))
            } else {
                None
            };

            // The status stage always runs opposite to the data stage, and
            // IN when there was no data at all (USB 2.0 §8.5.3).
            let status_in = !(data_len > 0 && is_in);
            let c = ring.state.cycle();
            let status_phys = ring.push(Trb::status_stage(status_in, c));
            (setup_phys, data_phys, status_phys)
        };

        self.doorbell(slot, EP0_DCI);

        // Wait for the status stage's event, noting any short-packet event
        // the data stage produced along the way — that is where the real
        // transferred length comes from.
        let mut residual = 0u32;
        let start = crate::time::ktime_get();
        let mut spins: u64 = 0;
        let mine = |t: &Trb| {
            t.trb_type() == x::TRB_TRANSFER_EVENT && t.slot_id() == slot && t.endpoint_id() == EP0_DCI
        };
        loop {
            while let Some(trb) = self.service_events(&mine) {
                // **Any** event for this slot's EP0 belongs to this
                // transfer — matching on the TRB pointer alone was a real
                // bug, and an expensive one: an error reported against the
                // *Setup Stage* TRB (whose address was never recorded) fell
                // through to the discard branch below, so the genuine
                // completion code was thrown away and the transfer was
                // reported as a plain timeout a second later. Every failure
                // in the first instrumented bare-metal run read as
                // `Timeout` for exactly that reason, which said nothing
                // about what the hardware had actually objected to.
                let code = trb.completion_code();
                let ptr = trb.pointer();
                if code != x::COMP_SUCCESS && code != x::COMP_SHORT_PACKET {
                    let stage = if ptr == setup_trb_phys {
                        "setup"
                    } else if Some(ptr) == data_trb_phys {
                        "data"
                    } else if ptr == status_trb_phys {
                        "status"
                    } else {
                        "unknown-trb"
                    };
                    crate::serial_println!(
                        "xhci: slot {} ep0 {} stage failed: {:?} {} (trb={:#x})",
                        slot, stage, XhciError::Failed(code), describe(XhciError::Failed(code)), ptr,
                    );
                    return Err(XhciError::Failed(code));
                }

                if Some(ptr) == data_trb_phys {
                    residual = trb.transfer_length();
                    continue;
                }
                if ptr == status_trb_phys {
                    return Ok(data_len.saturating_sub(residual.min(u16::MAX as u32) as u16) as usize);
                }
                // A successful event against the setup stage (or a TRB this
                // transfer doesn't know): keep waiting for the status stage.
            }
            if crate::time::ktime_get().wrapping_sub(start) > 1_000_000_000 {
                return Err(XhciError::Timeout);
            }
            spins += 1;
            if spins > 200_000_000 {
                return Err(XhciError::Timeout);
            }
            core::hint::spin_loop();
        }
    }

    // ── Runtime polling ─────────────────────────────────────────────────

    /// Drains the event ring and hands out the keyboard scancodes decoded
    /// since the last call (the PS/2 Set-1 bytes to feed
    /// `keyboard::process_scancode`), returning how many were written.
    ///
    /// Called from the timer ISR, so it does no allocation, takes no other
    /// lock, and never waits. The scancodes are *returned* rather than
    /// dispatched here so the caller can drop this driver's lock before
    /// feeding them into the keyboard pipeline — which itself takes the
    /// scheduler lock (`tty::feed_input` can deliver SIGINT). Whatever does
    /// not fit in `scancodes` stays pending for the next tick.
    pub fn poll(&mut self, scancodes: &mut [u8]) -> usize {
        let _ = self.service_events(&|_| false);
        let n = self.pending_len.min(scancodes.len());
        scancodes[..n].copy_from_slice(&self.pending_keys[..n]);
        self.pending_keys.copy_within(n..self.pending_len, 0);
        self.pending_len -= n;
        n
    }

    /// Turns one boot report into Set-1 scancodes.
    fn decode_report(&mut self, slot: u8, report: &[u8], out: &mut [u8]) -> usize {
        let Some(dev) = self.devices[slot as usize - 1].as_mut() else {
            return 0;
        };
        let Some(kb) = dev.keyboard.as_mut() else {
            return 0;
        };

        let mut events = [hal::hid::HidKeyEvent { usage: 0, pressed: false }; hal::hid::MAX_EVENTS];
        let n = kb.decoder.process(report, &mut events);

        let mut written = 0usize;
        for ev in &events[..n] {
            let Some(code) = hal::hid::usage_to_set1(ev.usage) else {
                continue;
            };
            // Two bytes worst case (the 0xE0 prefix plus the code), so
            // stop while both still fit.
            if written + 2 > out.len() {
                break;
            }
            if code.extended {
                out[written] = 0xE0;
                written += 1;
            }
            out[written] = if ev.pressed { code.code } else { code.code | 0x80 };
            written += 1;
        }
        written
    }

    /// Whether any keyboard is being polled — what the boot summary
    /// reports.
    pub fn keyboard_count(&self) -> usize {
        self.devices
            .iter()
            .filter(|d| d.as_ref().is_some_and(|d| d.keyboard.is_some()))
            .count()
    }
}
