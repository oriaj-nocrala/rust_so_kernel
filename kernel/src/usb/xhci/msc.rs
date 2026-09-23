// kernel/src/usb/xhci/msc.rs
//
// USB Mass Storage (Bulk-Only Transport + SCSI) on top of the xHCI driver
// — the hardware half. The protocol bytes (CBW/CSW, CDBs, INQUIRY / READ
// CAPACITY / sense decoding) live in `hal::msc` and the interface lookup
// in `hal::usb::find_mass_storage`, both host-tested; this file moves
// those bytes over two bulk endpoints and recovers when the device
// objects.
//
// Why it exists: the physical AM4 machine boots from a USB pendrive whose
// second partition is the ext2 filesystem `/mnt` should be. Without this
// the kernel could not read a sector of the disk it booted from — `/mnt`
// stayed unmounted and `$PATH` lost `/mnt/bin` (doom, quake, the C tests).
// See `docs/storage/usb-msc-plan.md`.
//
// A child module of `xhci` on purpose: it needs the controller's private
// rings, doorbells and — above all — `service_events`, the single reader
// of the event ring. A bulk completion is found the same way every other
// completion is, so a keystroke typed during a disk read is decoded and
// kept rather than eaten, and the disk's completion can never be eaten by
// the keyboard poll.
//
// Every transfer here is synchronous and runs with the `CONTROLLERS` lock
// held (see `usb::storage_read`), so there is exactly one party draining
// the ring for its whole duration.
//
// Recovery follows the BOT specification rather than hoping: a STALL on a
// data or status phase clears that endpoint's halt on both sides (the
// controller's Reset Endpoint + Set TR Dequeue, and the device's
// CLEAR_FEATURE) and continues to the CSW; an invalid CSW or a Phase Error
// runs a full Reset Recovery (BOT §5.3.4). QEMU's `usb-storage` never
// stalls and answers atomically, so none of this runs there — it is
// written for the real stick anyway, because that asymmetry is exactly
// what hid the cycle-bit bug in the keyboard driver.

use core::sync::atomic::{Ordering, compiler_fence};

use hal::msc as m;
use hal::usb::{self, SetupPacket};

use super::{Dma, Ring, Xhci, XhciError, describe, x};
use hal::xhci::Trb;

/// Bytes in the data bounce buffer: one 64 KiB, 64 KiB-aligned Buddy
/// block. A single Normal TRB may not cross a 64 KiB boundary (§6.4.1.1),
/// so this is also the largest transfer that needs just one TRB — which is
/// what keeps every bulk transfer here a one-TRB, one-event affair.
const DATA_BYTES: usize = 64 * 1024;
const DATA_ORDER: usize = 16;

/// The block size this driver supports. `hal::block::BlockDevice` is
/// 512-byte-sector-granular; a 4Kn device is refused at bring-up rather
/// than mis-addressed.
pub const BLOCK_SIZE: usize = 512;

/// Sectors per READ(10)/WRITE(10): one bounce buffer's worth.
pub const MAX_SECTORS: usize = DATA_BYTES / BLOCK_SIZE;

/// Per-transfer timeout. Generous: a pendrive's first read after power-up
/// can take a good fraction of a second while its controller wakes, and a
/// false timeout costs a Reset Recovery.
const BULK_TIMEOUT_MS: u64 = 5000;

/// Offsets inside the wrapper page.
const CBW_OFFSET: usize = 0;
const CSW_OFFSET: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MscError {
    Xhci(XhciError),
    Csw(m::CswError),
    /// CSW status "failed", with the sense data REQUEST SENSE returned for
    /// it (if it could be read).
    Failed(Option<m::Sense>),
    PhaseError,
    /// Data phase delivered fewer bytes than the command needs.
    Short { got: u32, wanted: u32 },
    /// Request outside the device.
    OutOfRange,
    NotStorage,
}

impl From<XhciError> for MscError {
    fn from(e: XhciError) -> Self {
        MscError::Xhci(e)
    }
}

type MResult<T> = core::result::Result<T, MscError>;

/// One Bulk-Only mass-storage device.
pub(super) struct MassStorage {
    interface: u8,
    bulk_in: usb::BulkEndpoint,
    bulk_out: usb::BulkEndpoint,
    in_dci: u8,
    out_dci: u8,
    in_ring: Ring,
    out_ring: Ring,
    /// CBW at `CBW_OFFSET`, CSW at `CSW_OFFSET`.
    wrap: Dma,
    data_phys: u64,
    data_virt: *mut u8,
    /// `dCBWTag` of the last command; each command uses the next one, so a
    /// CSW left over from an earlier, abandoned command can never pass as
    /// this one's.
    tag: u32,
    pub(super) blocks: u64,
    pub(super) vendor: [u8; 8],
    pub(super) product: [u8; 16],
}

impl Xhci {
    fn storage(&mut self, slot: u8) -> MResult<&mut MassStorage> {
        self.device(slot)?.storage.as_mut().ok_or(MscError::NotStorage)
    }

    /// Slots holding a mass-storage device that finished bring-up, with
    /// their size in 512-byte sectors.
    pub fn storage_devices(&self) -> impl Iterator<Item = (u8, u64)> + '_ {
        self.devices.iter().enumerate().filter_map(|(i, d)| {
            let st = d.as_ref()?.storage.as_ref()?;
            (st.blocks > 0).then_some((i as u8 + 1, st.blocks))
        })
    }

    // ── Bring-up ────────────────────────────────────────────────────────

    /// Configures a mass-storage interface's two bulk endpoints and runs
    /// the SCSI bring-up (INQUIRY, TEST UNIT READY, READ CAPACITY). A
    /// device that configures but fails the SCSI half stays in the slot
    /// with `blocks == 0`, so `storage_devices` skips it.
    pub(super) fn configure_storage(&mut self, slot: u8, ms: &usb::MassStorageInterface) -> MResult<()> {
        self.control_out(slot, SetupPacket::set_configuration(ms.config_value))?;

        let in_dci = x::endpoint_dci(ms.bulk_in.number(), true);
        let out_dci = x::endpoint_dci(ms.bulk_out.number(), false);
        let in_ring = Ring::alloc()?;
        let out_ring = Ring::alloc()?;
        let wrap = Dma::alloc()?;

        // SAFETY: a fresh 64 KiB Buddy block, naturally 64 KiB-aligned
        // (Buddy blocks are aligned to their own size), owned by this
        // device for the kernel's lifetime.
        let data = unsafe { crate::allocator::phys_alloc(DATA_ORDER) }.ok_or(XhciError::NoMemory)?;
        let data_phys = data.as_u64();
        let data_virt = (crate::memory::physical_memory_offset() + data_phys).as_mut_ptr::<u8>();

        let (input, speed) = {
            let dev = self.device(slot)?;
            (dev.input, dev.speed)
        };
        let mut ctrl_words = [0u32; 8];
        x::build_input_control(&mut ctrl_words, x::ADD_SLOT | x::add_flag(in_dci) | x::add_flag(out_dci));
        self.write_context(&input, 0, &ctrl_words);

        let root_port = self.root_port_of(slot);
        let mut slot_words = [0u32; 8];
        x::build_slot_context(&mut slot_words, speed, root_port, in_dci.max(out_dci));
        self.write_context(&input, 1, &slot_words);

        let mut ep = [0u32; 8];
        x::build_bulk_endpoint_context(&mut ep, true, ms.bulk_in.max_packet, ms.bulk_in.max_burst, in_ring.dma.phys);
        self.write_context(&input, in_dci as usize + 1, &ep);
        x::build_bulk_endpoint_context(&mut ep, false, ms.bulk_out.max_packet, ms.bulk_out.max_burst, out_ring.dma.phys);
        self.write_context(&input, out_dci as usize + 1, &ep);

        let cycle = self.cmd.state.cycle();
        let trb = self.cmd.push(Trb::configure_endpoint(input.phys, slot, cycle));
        self.doorbell(0, 0);
        self.wait_for_command(trb, 1000)?;

        self.device(slot)?.storage = Some(MassStorage {
            interface: ms.interface,
            bulk_in: ms.bulk_in,
            bulk_out: ms.bulk_out,
            in_dci,
            out_dci,
            in_ring,
            out_ring,
            wrap,
            data_phys,
            data_virt,
            tag: 0,
            blocks: 0,
            vendor: [b' '; 8],
            product: [b' '; 16],
        });

        crate::serial_println!(
            "usb-storage: slot {} interface {} bulk in {:#04x}/{}B burst {} out {:#04x}/{}B burst {}",
            slot, ms.interface, ms.bulk_in.address, ms.bulk_in.max_packet, ms.bulk_in.max_burst,
            ms.bulk_out.address, ms.bulk_out.max_packet, ms.bulk_out.max_burst,
        );

        self.storage_bringup(slot)
    }

    fn storage_bringup(&mut self, slot: u8) -> MResult<()> {
        // Get Max LUN is optional; single-LUN devices may STALL it (BOT
        // §3.2), which `control_transfer`'s own stall recovery handles.
        // Only LUN 0 is used either way — a card reader's other slots are
        // out of scope.
        let iface = self.storage(slot)?.interface;
        let mut lun = [0u8; 1];
        match self.control_in(slot, SetupPacket::get_max_lun(iface), &mut lun) {
            Ok(1) => crate::serial_println!("usb-storage: slot {} max LUN {}", slot, lun[0]),
            _ => crate::serial_println!("usb-storage: slot {} Get Max LUN refused — assuming one LUN", slot),
        }

        let n = self.scsi_retrying(slot, &m::inquiry(), true, m::INQUIRY_LEN as u32)?;
        let mut buf = [0u8; m::INQUIRY_LEN as usize];
        let k = (n as usize).min(buf.len());
        self.copy_data_out(slot, &mut buf[..k])?;
        let q = m::parse_inquiry(&buf[..k]).ok_or(MscError::Short { got: n, wanted: 36 })?;
        crate::serial_println!(
            "usb-storage: slot {} INQUIRY: {} {} {} (type {:#x}{})",
            slot, m::trim(&q.vendor), m::trim(&q.product), m::trim(&q.revision),
            q.device_type, if q.removable { ", removable" } else { "" },
        );
        {
            let st = self.storage(slot)?;
            st.vendor = q.vendor;
            st.product = q.product;
        }
        if q.device_type != m::DEVICE_TYPE_DIRECT_ACCESS {
            return Err(MscError::NotStorage);
        }

        // TEST UNIT READY until the medium answers. `scsi_retrying`
        // already absorbs UNIT ATTENTION and "becoming ready"; this outer
        // loop gives a slow stick a couple of seconds more.
        let mut ready = false;
        for _ in 0..20 {
            match self.scsi_retrying(slot, &m::test_unit_ready(), false, 0) {
                Ok(_) => {
                    ready = true;
                    break;
                }
                Err(MscError::Failed(Some(s))) if m::sense_is_transient(s) => self.delay_ms(100),
                Err(e) => return Err(e),
            }
        }
        if !ready {
            crate::serial_println!("usb-storage: slot {} never became ready", slot);
            return Err(MscError::Xhci(XhciError::Timeout));
        }

        let n = self.scsi_retrying(slot, &m::read_capacity_10(), true, m::READ_CAPACITY_10_LEN)?;
        let mut cap = [0u8; 8];
        let k = (n as usize).min(cap.len());
        self.copy_data_out(slot, &mut cap[..k])?;
        let cap = match m::parse_read_capacity_10(&cap[..k]) {
            Ok(c) => c,
            Err(e) => {
                crate::serial_println!("usb-storage: slot {} READ CAPACITY unusable: {:?}", slot, e);
                return Err(MscError::Short { got: n, wanted: 8 });
            }
        };
        if cap.block_size as usize != BLOCK_SIZE {
            crate::serial_println!(
                "usb-storage: slot {} has {}-byte blocks; only {} is supported",
                slot, cap.block_size, BLOCK_SIZE
            );
            return Err(MscError::NotStorage);
        }
        crate::serial_println!(
            "usb-storage: slot {} {} sectors x {} bytes ({} MiB)",
            slot, cap.blocks, cap.block_size, cap.blocks * cap.block_size as u64 / (1024 * 1024)
        );
        self.storage(slot)?.blocks = cap.blocks;
        Ok(())
    }

    // ── Sector I/O ──────────────────────────────────────────────────────

    /// Reads `count` (1..=`MAX_SECTORS`) sectors at `lba` into `out`.
    pub fn storage_read(&mut self, slot: u8, lba: u32, count: usize, out: &mut [u8]) -> MResult<()> {
        let bytes = self.check_range(slot, lba, count, out.len())?;
        let n = self.scsi_retrying(slot, &m::read_10(lba, count as u16), true, bytes as u32)?;
        if (n as usize) < bytes {
            return Err(MscError::Short { got: n, wanted: bytes as u32 });
        }
        self.copy_data_out(slot, &mut out[..bytes])
    }

    /// Writes `count` (1..=`MAX_SECTORS`) sectors at `lba` from `data`.
    pub fn storage_write(&mut self, slot: u8, lba: u32, count: usize, data: &[u8]) -> MResult<()> {
        let bytes = self.check_range(slot, lba, count, data.len())?;
        {
            let st = self.storage(slot)?;
            // SAFETY: the bounce buffer is DATA_BYTES long and
            // `check_range` bounded `bytes` by it.
            unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), st.data_virt, bytes) };
        }
        compiler_fence(Ordering::SeqCst);
        let n = self.scsi_retrying(slot, &m::write_10(lba, count as u16), false, bytes as u32)?;
        if (n as usize) < bytes {
            return Err(MscError::Short { got: n, wanted: bytes as u32 });
        }
        Ok(())
    }

    fn check_range(&mut self, slot: u8, lba: u32, count: usize, buf_len: usize) -> MResult<usize> {
        let blocks = self.storage(slot)?.blocks;
        let bytes = count * BLOCK_SIZE;
        if count == 0 || count > MAX_SECTORS || buf_len < bytes || lba as u64 + count as u64 > blocks {
            return Err(MscError::OutOfRange);
        }
        Ok(bytes)
    }

    fn copy_data_out(&mut self, slot: u8, out: &mut [u8]) -> MResult<()> {
        let st = self.storage(slot)?;
        compiler_fence(Ordering::SeqCst);
        // SAFETY: `out.len()` is at most one bounce buffer (every caller
        // bounds it by the transfer it just ran, itself ≤ DATA_BYTES).
        unsafe {
            core::ptr::copy_nonoverlapping(st.data_virt, out.as_mut_ptr(), out.len().min(DATA_BYTES))
        };
        Ok(())
    }

    // ── SCSI over Bulk-Only ─────────────────────────────────────────────

    /// One SCSI command, retried while the device reports a transient
    /// condition (UNIT ATTENTION after power-on or reset, NOT READY /
    /// becoming ready). Failures carry the sense data that explains them.
    fn scsi_retrying(&mut self, slot: u8, cdb: &[u8], data_in: bool, len: u32) -> MResult<u32> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.scsi(slot, cdb, data_in, len) {
                Err(MscError::Failed(_)) => {
                    let sense = self.request_sense(slot);
                    match sense {
                        Some(s) if m::sense_is_transient(s) && attempt < 4 => {
                            crate::ktrace!(
                                crate::debug::USB,
                                "usb-storage: slot {} op {:#04x}: {} asc={:#04x}/{:#04x}, retrying",
                                slot, cdb[0], m::describe_sense_key(s.key), s.asc, s.ascq
                            );
                            continue;
                        }
                        Some(s) => crate::serial_println!(
                            "usb-storage: slot {} op {:#04x} failed: {} asc={:#04x}/{:#04x}",
                            slot, cdb[0], m::describe_sense_key(s.key), s.asc, s.ascq
                        ),
                        None => crate::serial_println!(
                            "usb-storage: slot {} op {:#04x} failed, and REQUEST SENSE did too",
                            slot, cdb[0]
                        ),
                    }
                    return Err(MscError::Failed(sense));
                }
                other => return other,
            }
        }
    }

    fn request_sense(&mut self, slot: u8) -> Option<m::Sense> {
        let n = self.scsi(slot, &m::request_sense(), true, m::SENSE_LEN as u32).ok()?;
        let mut buf = [0u8; m::SENSE_LEN as usize];
        let n = (n as usize).min(buf.len());
        self.copy_data_out(slot, &mut buf[..n]).ok()?;
        m::parse_sense(&buf[..n])
    }

    /// One Bulk-Only command: CBW → optional data phase → CSW. Returns
    /// the bytes the data phase actually moved.
    fn scsi(&mut self, slot: u8, cdb: &[u8], data_in: bool, len: u32) -> MResult<u32> {
        let (tag, wrap, data_phys) = {
            let st = self.storage(slot)?;
            st.tag = st.tag.wrapping_add(1);
            (st.tag, st.wrap, st.data_phys)
        };
        let cbw = m::build_cbw(tag, len, data_in, 0, cdb).ok_or(MscError::OutOfRange)?;
        for (i, chunk) in cbw.chunks(4).enumerate() {
            let mut w = [0u8; 4];
            w[..chunk.len()].copy_from_slice(chunk);
            wrap.write_u32(CBW_OFFSET + i * 4, u32::from_le_bytes(w));
        }
        // Stale CSW bytes from the previous command must not validate as
        // this one's if the status phase comes back short.
        for i in 0..4 {
            wrap.write_u32(CSW_OFFSET + i * 4, 0);
        }

        // 1. Command. Any failure here leaves the device's view of the
        //    protocol unknown: Reset Recovery.
        if let Err(e) = self.bulk(slot, false, wrap.phys + CBW_OFFSET as u64, m::CBW_LEN as u32) {
            crate::serial_println!("usb-storage: slot {} CBW failed ({}) — reset recovery", slot, describe_msc(e));
            self.reset_recovery(slot);
            return Err(e);
        }

        // 2. Data. A STALL here is a normal way for a device to end a
        //    data phase early (BOT §6.7.2/§6.7.3): clear the halt and go on
        //    to the CSW, which says what happened.
        let mut moved = 0u32;
        if len > 0 {
            match self.bulk(slot, data_in, data_phys, len) {
                Ok(n) => moved = n,
                Err(MscError::Xhci(XhciError::Failed(x::COMP_STALL))) => {
                    crate::serial_println!("usb-storage: slot {} data phase stalled — clearing halt", slot);
                    self.clear_halt(slot, data_in);
                }
                Err(e) => {
                    crate::serial_println!("usb-storage: slot {} data phase failed ({}) — reset recovery", slot, describe_msc(e));
                    self.reset_recovery(slot);
                    return Err(e);
                }
            }
        }

        // 3. Status. One retry after a stalled status phase (§6.7.2).
        let csw_phys = wrap.phys + CSW_OFFSET as u64;
        let got = match self.bulk(slot, true, csw_phys, m::CSW_LEN as u32) {
            Err(MscError::Xhci(XhciError::Failed(x::COMP_STALL))) => {
                self.clear_halt(slot, true);
                self.bulk(slot, true, csw_phys, m::CSW_LEN as u32)
            }
            other => other,
        };
        let got = match got {
            Ok(n) => n as usize,
            Err(e) => {
                crate::serial_println!("usb-storage: slot {} CSW failed ({}) — reset recovery", slot, describe_msc(e));
                self.reset_recovery(slot);
                return Err(e);
            }
        };
        let mut raw = [0u8; m::CSW_LEN];
        wrap.read_bytes(CSW_OFFSET, &mut raw);
        let csw = match m::parse_csw(&raw[..got.min(m::CSW_LEN)], tag, len) {
            Ok(c) => c,
            Err(e) => {
                crate::serial_println!("usb-storage: slot {} bad CSW {:?} — reset recovery", slot, e);
                self.reset_recovery(slot);
                return Err(MscError::Csw(e));
            }
        };
        match csw.status {
            m::CswStatus::Passed => Ok(moved.min(len - csw.residue)),
            m::CswStatus::Failed => Err(MscError::Failed(None)),
            m::CswStatus::PhaseError => {
                crate::serial_println!("usb-storage: slot {} phase error — reset recovery", slot);
                self.reset_recovery(slot);
                Err(MscError::PhaseError)
            }
        }
    }

    // ── Bulk transfers ──────────────────────────────────────────────────

    /// One single-TRB bulk transfer; returns the bytes moved.
    ///
    /// The completion is matched on slot + endpoint + **TRB pointer**. The
    /// pointer check is safe here, unlike on EP0 (see
    /// `control_transfer_once`), because a one-TRB transfer's event —
    /// success or error — always points at that TRB; what it guards
    /// against is a late event from an earlier transfer that timed out, or
    /// the "Stopped" event a Stop Endpoint produces, being taken for this
    /// one's.
    fn bulk(&mut self, slot: u8, is_in: bool, phys: u64, len: u32) -> MResult<u32> {
        let (dci, trb_phys) = {
            let st = self.storage(slot)?;
            let (dci, ring) = if is_in { (st.in_dci, &mut st.in_ring) } else { (st.out_dci, &mut st.out_ring) };
            let c = ring.state.cycle();
            (dci, ring.push(Trb::normal(phys, len, c)))
        };
        self.doorbell(slot, dci);

        let mine = |t: &Trb| {
            t.trb_type() == x::TRB_TRANSFER_EVENT && t.slot_id() == slot && t.endpoint_id() == dci
        };
        let start = crate::time::ktime_get();
        let mut spins: u64 = 0;
        loop {
            while let Some(trb) = self.service_events(&mine) {
                if trb.pointer() != trb_phys {
                    self.handle_async_event(trb); // stale — logged, bounded
                    continue;
                }
                let code = trb.completion_code();
                if code == x::COMP_SUCCESS || code == x::COMP_SHORT_PACKET {
                    return Ok(len - trb.transfer_length().min(len));
                }
                return Err(MscError::Xhci(XhciError::Failed(code)));
            }
            if self.op_read(x::OP_USBSTS) & (x::USBSTS_HCE | x::USBSTS_HSE) != 0 {
                return Err(MscError::Xhci(XhciError::Unusable));
            }
            if crate::time::ktime_get().wrapping_sub(start) > BULK_TIMEOUT_MS * 1_000_000 {
                break;
            }
            spins += 1;
            if spins > 1_000_000_000 {
                break;
            }
            core::hint::spin_loop();
        }

        // Timed out with the TRB still owned by the controller. Take it
        // back before anything else is queued behind it, or a completion
        // that arrives late lands on a later transfer's data.
        crate::serial_println!(
            "usb-storage: slot {} bulk {} {} bytes timed out — resyncing endpoint",
            slot, if is_in { "IN" } else { "OUT" }, len
        );
        self.resync_endpoint(slot, is_in);
        Err(MscError::Xhci(XhciError::Timeout))
    }

    /// Endpoint State as the controller last wrote it to the output
    /// device context.
    fn endpoint_state(&mut self, slot: u8, dci: u8) -> MResult<u32> {
        let offset = dci as usize * self.context_size;
        Ok(self.device(slot)?._output.read_u32(offset) & 0x7)
    }

    /// Brings the controller's side of a bulk endpoint back to a usable,
    /// empty state: Reset Endpoint if it halted, Stop Endpoint if it is
    /// still running with an abandoned TRB, then Set TR Dequeue Pointer to
    /// where software will enqueue next — so nothing already on the ring
    /// is replayed. Each step is chosen from the endpoint's real state, not
    /// guessed: issuing the wrong one is a Context State Error.
    fn resync_endpoint(&mut self, slot: u8, is_in: bool) {
        let Ok(dci) = self.storage(slot).map(|st| if is_in { st.in_dci } else { st.out_dci }) else {
            return;
        };
        let state = self.endpoint_state(slot, dci).unwrap_or(0);
        let step = if state == x::EP_STATE_HALTED {
            Some(Trb::reset_endpoint(slot, dci, self.cmd.state.cycle()))
        } else if state == x::EP_STATE_RUNNING {
            Some(Trb::stop_endpoint(slot, dci, self.cmd.state.cycle()))
        } else {
            None
        };
        if let Some(t) = step {
            let p = self.cmd.push(t);
            self.doorbell(0, 0);
            if let Err(e) = self.wait_for_command(p, 1000) {
                crate::serial_println!(
                    "usb-storage: slot {} dci {} {} failed: {}",
                    slot, dci, if state == x::EP_STATE_HALTED { "Reset Endpoint" } else { "Stop Endpoint" },
                    describe(e)
                );
            }
        }

        let Ok((ring_phys, ring_cycle)) = self.storage(slot).map(|st| {
            let r = if is_in { &st.in_ring } else { &st.out_ring };
            (r.dma.trb_phys(r.state.enqueue_index()), r.state.cycle())
        }) else {
            return;
        };
        let cycle = self.cmd.state.cycle();
        let p = self.cmd.push(Trb::set_tr_dequeue(slot, dci, ring_phys, ring_cycle, cycle));
        self.doorbell(0, 0);
        if let Err(e) = self.wait_for_command(p, 1000) {
            crate::serial_println!("usb-storage: slot {} dci {} Set TR Dequeue failed: {}", slot, dci, describe(e));
        }
    }

    /// Clears a stalled bulk endpoint on both sides: the controller's
    /// (`resync_endpoint`) and the device's (CLEAR_FEATURE(ENDPOINT_HALT),
    /// which also resets its data toggle to match the controller's reset).
    fn clear_halt(&mut self, slot: u8, is_in: bool) {
        self.resync_endpoint(slot, is_in);
        let Ok(addr) = self.storage(slot).map(|st| if is_in { st.bulk_in.address } else { st.bulk_out.address }) else {
            return;
        };
        if let Err(e) = self.control_out(slot, SetupPacket::clear_endpoint_halt(addr)) {
            crate::serial_println!("usb-storage: slot {} CLEAR_FEATURE(HALT) {:#04x} failed: {}", slot, addr, describe(e));
        }
    }

    /// Reset Recovery (BOT §5.3.4): Bulk-Only Mass Storage Reset, then
    /// clear the halt on both bulk endpoints. Best-effort — every step is
    /// logged, none aborts the others, since a half-recovered device is
    /// still better than one nobody tried to recover.
    fn reset_recovery(&mut self, slot: u8) {
        let Ok(iface) = self.storage(slot).map(|st| st.interface) else {
            return;
        };
        if let Err(e) = self.control_out(slot, SetupPacket::bulk_only_reset(iface)) {
            crate::serial_println!("usb-storage: slot {} Bulk-Only Reset failed: {}", slot, describe(e));
        }
        self.clear_halt(slot, true);
        self.clear_halt(slot, false);
    }
}

fn describe_msc(e: MscError) -> &'static str {
    match e {
        MscError::Xhci(x) => describe(x),
        MscError::Csw(_) => "bad CSW",
        MscError::Failed(_) => "command failed",
        MscError::PhaseError => "phase error",
        MscError::Short { .. } => "short transfer",
        MscError::OutOfRange => "out of range",
        MscError::NotStorage => "not a storage device",
    }
}
