//! The RPC channel to GSP-RM (phase 4f): the message queues, the messages the
//! host sends to boot it, the sequencer commands GSP-RM asks the host to run
//! while it boots, and the static configuration reply that carries the GPU's
//! name. Pure: the queues' shared memory is behind [`Shm`], registers behind
//! [`Mmio`], and nothing blocks (the caller loops with [`Queues::recv`]).
//!
//! Layouts are those of `open-gpu-kernel-modules` 570.144 as nouveau carries
//! them (`nvkm/subdev/gsp/rm/r570/nvrm/gsp.h`, `r535/nvrm/gsp.h`); the offsets
//! asserted in the tests come from a C compiler (`nvgpu/gen/sysinfo.c`,
//! `staticinfo.c`) and from the payloads nouveau dumped in `trace-gsp`
//! (`fixtures/rpc72.bin`, `rpc73.bin`, `rpc-seq.bin`, `rpc65reply.bin`).
//! Paths are relative to `drivers/gpu/drm/nouveau/nvkm/subdev/gsp/rm/` in
//! Linux v7.2.2.

use alloc::vec;
use alloc::vec::Vec;

use crate::falcon::{Falcon, FalconError};
use crate::gspmem::{CMDQ_OFFSET, MSGQ_OFFSET, PAGE};
use crate::Mmio;

// ---- function numbers (`r570/nvrm/rpcfn.h`, `msgfn.h`) ---------------------

pub const FN_GET_GSP_STATIC_INFO: u32 = 65;
pub const FN_CONTINUATION_RECORD: u32 = 71;
pub const FN_GSP_SET_SYSTEM_INFO: u32 = 72;
pub const FN_SET_REGISTRY: u32 = 73;
pub const EVENT_GSP_INIT_DONE: u32 = 0x1001;
pub const EVENT_GSP_RUN_CPU_SEQUENCER: u32 = 0x1002;
pub const EVENT_POST_EVENT: u32 = 0x1003;
pub const EVENT_RC_TRIGGERED: u32 = 0x1004;
pub const EVENT_MMU_FAULT_QUEUED: u32 = 0x1005;
pub const EVENT_OS_ERROR_LOG: u32 = 0x1006;
pub const EVENT_UCODE_LIBOS_PRINT: u32 = 0x100c;
pub const EVENT_GSP_POST_NOCAT_RECORD: u32 = 0x1020;

/// The name of an event RM sends (`nvrm/msgfn.h`, r570's `rpc_global_enums.h`), for reports.
/// The channel id in an `RC_TRIGGERED` payload (`rpc_rc_triggered_v17_02`: `nv2080EngineType` u32, then `chid` u32).
pub fn rc_chid(payload: &[u8]) -> Option<u32> {
    (payload.len() >= 8).then(|| u32::from_le_bytes(payload[4..8].try_into().unwrap()))
}

pub fn event_name(function: u32) -> &'static str {
    match function {
        EVENT_GSP_INIT_DONE => "INIT_DONE",
        EVENT_GSP_RUN_CPU_SEQUENCER => "RUN_CPU_SEQUENCER",
        EVENT_POST_EVENT => "POST_EVENT",
        EVENT_RC_TRIGGERED => "RC_TRIGGERED",
        EVENT_MMU_FAULT_QUEUED => "MMU_FAULT_QUEUED",
        EVENT_OS_ERROR_LOG => "OS_ERROR_LOG",
        EVENT_UCODE_LIBOS_PRINT => "UCODE_LIBOS_PRINT",
        EVENT_GSP_POST_NOCAT_RECORD => "POST_NOCAT_RECORD",
        _ => "?",
    }
}

// ---- the queues -----------------------------------------------------------

/// `GSP_MSG_HDR_SIZE` = `offsetof(struct r535_gsp_msg, data)` (`r535/rpc.c:98`):
/// auth tag (16), AAD (16), checksum, sequence, element count, pad.
pub const MSG_HDR: usize = 48;
/// `sizeof(struct nvfw_gsp_rpc)` (`rpc.c:88-99`).
pub const RPC_HDR: usize = 32;
/// `GSP_MSG_MAX_SIZE` = 16 pages (`rpc.c:27`).
pub const MSG_MAX: usize = 16 * PAGE;
/// Entries per queue: `(0x40000 - 0x1000) / 0x1000`.
pub const QUEUE_ENTRIES: u32 = 63;

// Pointers in the shared memory (`r535_gsp_shared_init`, `r535/gsp.c:1135-1183`).
const CMDQ_WPTR: usize = CMDQ_OFFSET + 16; // our write pointer (cmdq tx header)
const CMDQ_RPTR: usize = MSGQ_OFFSET + 32; // their read pointer (msgq rx header)
const MSGQ_WPTR: usize = MSGQ_OFFSET + 16; // their write pointer (msgq tx header)
const MSGQ_RPTR: usize = CMDQ_OFFSET + 32; // our read pointer (cmdq rx header)

/// The doorbell: any write to `0xc00` of the GSP falcon (`r535_gsp_cmdq_push`,
/// `rpc.c:413`; the trace: `W 0x110c00 0`).
pub const DOORBELL: u32 = 0xc00;

/// The shared memory the two queues live in (a DMA buffer in the kernel).
pub trait Shm {
    fn rd32(&self, off: usize) -> u32;
    fn wr32(&self, off: usize, v: u32);
    fn read(&self, off: usize, out: &mut [u8]);
    fn write(&self, off: usize, data: &[u8]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcError {
    /// The command queue has no room for the message.
    Full,
    /// The RPC needs more than one 16-page element (continuation records are not ported).
    TooBig,
    /// A received element is not a valid RPC.
    BadMessage,
    /// GSP-RM answered with a non-zero result (`rpc_result`).
    Result(u32),
}

/// A received message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub function: u32,
    pub result: u32,
    pub result_private: u32,
    pub sequence: u32,
    /// The RPC payload (after the 32-byte RPC header).
    pub payload: Vec<u8>,
}

/// The two counters `struct nvkm_gsp` keeps: `cmdq.seq` and `rpc_seq`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Queues {
    pub cmdq_seq: u32,
    pub rpc_seq: u32,
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn get32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// The bytes of one command-queue message: element header, RPC header,
/// payload, zero padded to whole 4 KiB pages, with the checksum
/// (`r535_gsp_rpc_get` + `r535_gsp_cmdq_push`, `rpc.c:380-445,533-548`).
/// `sequence` is the element's sequence number (`cmdq.seq`), `rpc_sequence`
/// the RPC's (0 for the `NOSEQ` policy).
pub fn build_message(function: u32, payload: &[u8], sequence: u32, rpc_sequence: u32) -> Result<Vec<u8>, RpcError> {
    let rpc_len = RPC_HDR + payload.len();
    // `r535_gsp_rpc_get`: the buffer is ALIGN(rpc_len, 8); the RPC's own length is exact.
    let buf_len = (rpc_len + 7) & !7;
    let len = (MSG_HDR + buf_len).div_ceil(PAGE) * PAGE;
    if len > MSG_MAX {
        return Err(RpcError::TooBig);
    }
    let mut m = vec![0u8; len];
    put32(&mut m, MSG_HDR, 0x0300_0000); // header_version
    put32(&mut m, MSG_HDR + 4, u32::from_be_bytes(*b"CPRV")); // signature ('C'<<24 | 'P'<<16 | 'R'<<8 | 'V')
    put32(&mut m, MSG_HDR + 8, rpc_len as u32); // length
    put32(&mut m, MSG_HDR + 12, function);
    put32(&mut m, MSG_HDR + 16, 0xffff_ffff); // rpc_result
    put32(&mut m, MSG_HDR + 20, 0xffff_ffff); // rpc_result_private
    put32(&mut m, MSG_HDR + 24, rpc_sequence);
    m[MSG_HDR + RPC_HDR..MSG_HDR + RPC_HDR + payload.len()].copy_from_slice(payload);
    // element header: checksum (0 while summing), sequence, element count, pad
    put32(&mut m, 32, 0);
    put32(&mut m, 36, sequence);
    put32(&mut m, 40, (len / PAGE) as u32);
    put32(&mut m, 44, 0);
    let mut csum = 0u64;
    for w in m.chunks_exact(8) {
        csum ^= u64::from_le_bytes(w.try_into().unwrap());
    }
    put32(&mut m, 32, (csum >> 32) as u32 ^ csum as u32);
    Ok(m)
}

impl Queues {
    /// `r535_gsp_cmdq_push` (`rpc.c:380-445`): write the message into the
    /// command queue (wrapping), publish the write pointer, ring the doorbell.
    /// `noseq` = `NVKM_GSP_RPC_REPLY_NOSEQ` (the RPC's sequence is 0).
    pub fn send(&mut self, shm: &impl Shm, m: &impl Mmio, gsp: &Falcon, function: u32, payload: &[u8], noseq: bool) -> Result<(), RpcError> {
        let rpc_seq = if noseq {
            0
        } else {
            let s = self.rpc_seq;
            self.rpc_seq = s.wrapping_add(1);
            s
        };
        let seq = self.cmdq_seq;
        let msg = build_message(function, payload, seq, rpc_seq)?;
        let cnt = QUEUE_ENTRIES;
        let mut wptr = shm.rd32(CMDQ_WPTR);
        let rptr = shm.rd32(CMDQ_RPTR);
        // free entries, one always kept empty (`rpc.c:405-407`)
        let mut free = (rptr + cnt).wrapping_sub(wptr).wrapping_sub(1);
        if free >= cnt {
            free = free.wrapping_sub(cnt);
        }
        let need = (msg.len() / PAGE) as u32;
        if free < need {
            return Err(RpcError::Full);
        }
        self.cmdq_seq = seq.wrapping_add(1);
        let (mut off, mut left) = (0usize, msg.len());
        while left > 0 {
            // contiguous entries up to the end of the queue
            let step = (cnt - wptr) as usize;
            let size = left.min(step * PAGE);
            shm.write(CMDQ_OFFSET + PAGE + wptr as usize * PAGE, &msg[off..off + size]);
            wptr += size.div_ceil(PAGE) as u32;
            if wptr == cnt {
                wptr = 0;
            }
            off += size;
            left -= size;
        }
        shm.wr32(CMDQ_WPTR, wptr);
        gsp.wr(m, DOORBELL, 0);
        Ok(())
    }

    /// One message from the status queue, if one is complete
    /// (`r535_gsp_msgq_peek`/`recv_one_elem`, `rpc.c:110-240`), and the read
    /// pointer moved past it. `Ok(None)` when the queue is empty.
    pub fn recv(&self, shm: &impl Shm) -> Result<Option<Message>, RpcError> {
        let cnt = QUEUE_ENTRIES;
        let wptr = shm.rd32(MSGQ_WPTR);
        let mut rptr = shm.rd32(MSGQ_RPTR);
        let mut used = (wptr + cnt).wrapping_sub(rptr);
        if used >= cnt {
            used = used.wrapping_sub(cnt);
        }
        if used == 0 {
            return Ok(None);
        }
        if wptr >= cnt || rptr >= cnt {
            return Err(RpcError::BadMessage);
        }
        let entry = MSGQ_OFFSET + PAGE + rptr as usize * PAGE;
        let mut hdr = [0u8; MSG_HDR + RPC_HDR];
        shm.read(entry, &mut hdr);
        let length = get32(&hdr, MSG_HDR + 8) as usize;
        if length < RPC_HDR || length > MSG_MAX - MSG_HDR || get32(&hdr, MSG_HDR + 4) != u32::from_be_bytes(*b"CPRV") {
            return Err(RpcError::BadMessage);
        }
        let pages = (MSG_HDR + length).div_ceil(PAGE) as u32;
        if used < pages {
            return Ok(None); // the rest of the element is not there yet
        }
        // Copy `length` bytes of RPC (header + payload), wrapping at the queue's end.
        let mut rpc = vec![0u8; length];
        let first = (((cnt - rptr) as usize) * PAGE - MSG_HDR).min(length);
        shm.read(entry + MSG_HDR, &mut rpc[..first]);
        if first < length {
            shm.read(MSGQ_OFFSET + PAGE, &mut rpc[first..]);
        }
        rptr = (rptr + pages) % cnt;
        shm.wr32(MSGQ_RPTR, rptr);
        Ok(Some(Message {
            function: get32(&rpc, 12),
            result: get32(&rpc, 16),
            result_private: get32(&rpc, 20),
            sequence: get32(&rpc, 24),
            payload: rpc[RPC_HDR..].to_vec(),
        }))
    }
}

// ---- GSP_SET_SYSTEM_INFO ----------------------------------------------------

/// `sizeof(GspSystemInfo)` (clang on the r570 header: `nvgpu/gen/sysinfo.c`).
pub const SYSTEM_INFO_SIZE: usize = 928;

/// What `r570_gsp_set_system_info` fills (`r570/gsp.c:130-160`); everything
/// else is zero. The ACPI method data is a stub: nouveau finds no ACPI
/// methods on this board and leaves the three status words at `0xffff`
/// (`trace-gsp`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemInfo {
    pub gpu_phys_addr: u64,
    pub gpu_phys_fb_addr: u64,
    pub gpu_phys_inst_addr: u64,
    /// `pci_dev_id`: `bus << 8 | devfn`.
    pub bus_device_func: u64,
    pub max_user_va: u64,
    pub pci_config_mirror_base: u32,
    pub pci_config_mirror_size: u32,
    /// `(device << 16) | vendor`.
    pub pci_device_id: u32,
    /// `(subsystem device << 16) | subsystem vendor`.
    pub pci_sub_device_id: u32,
    pub pci_revision_id: u32,
    pub is_primary: bool,
}

impl SystemInfo {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = vec![0u8; SYSTEM_INFO_SIZE];
        let put64 = |b: &mut [u8], at: usize, v: u64| b[at..at + 8].copy_from_slice(&v.to_le_bytes());
        put64(&mut b, 0, self.gpu_phys_addr);
        put64(&mut b, 8, self.gpu_phys_fb_addr);
        put64(&mut b, 16, self.gpu_phys_inst_addr);
        put64(&mut b, 32, self.bus_device_func);
        put64(&mut b, 72, self.max_user_va);
        put32(&mut b, 80, self.pci_config_mirror_base);
        put32(&mut b, 84, self.pci_config_mirror_size);
        put32(&mut b, 88, self.pci_device_id);
        put32(&mut b, 92, self.pci_sub_device_id);
        put32(&mut b, 96, self.pci_revision_id);
        // acpiMethodData at 160: bValid, then the DOD, JT and CAPS status words.
        put32(&mut b, 160, 1);
        put32(&mut b, 164, 0xffff);
        put32(&mut b, 236, 0xffff);
        put32(&mut b, 828, 0xffff);
        b[896] = self.is_primary as u8; // bIsPrimary
        b
    }
}

// ---- SET_REGISTRY -------------------------------------------------------------

/// `PACKED_REGISTRY_TABLE` with dword entries (`r535/gsp.c:440-545`): size,
/// count, then per entry `nameOffset, type (1 = dword), data, length (4)`,
/// then the names, NUL terminated.
pub fn registry(entries: &[(&str, u32)]) -> Vec<u8> {
    let head = 8 + entries.len() * 16;
    let total = head + entries.iter().map(|(n, _)| n.len() + 1).sum::<usize>();
    let mut b = vec![0u8; total];
    put32(&mut b, 0, total as u32);
    put32(&mut b, 4, entries.len() as u32);
    let mut name_at = head;
    for (i, (name, value)) in entries.iter().enumerate() {
        let e = 8 + i * 16;
        put32(&mut b, e, name_at as u32);
        b[e + 4] = 1; // NV_REG_TYPE_DWORD
        put32(&mut b, e + 8, *value);
        put32(&mut b, e + 12, 4);
        b[name_at..name_at + name.len()].copy_from_slice(name.as_bytes());
        name_at += name.len() + 1;
    }
    b
}

/// The three entries nouveau always sets (`r535_registry_entries`, `gsp.c:588-592`).
pub const REGISTRY: [(&str, u32); 3] = [("RMSecBusResetEnable", 1), ("RMForcePcieConfigSave", 1), ("RMDevidCheckIgnore", 1)];

// ---- GET_GSP_STATIC_INFO ------------------------------------------------------

/// `sizeof(GspStaticConfigInfo)` and where `gpuNameString` is (clang on the r570 header).
pub const STATIC_INFO_SIZE: usize = 1656;
const STATIC_NAME_AT: usize = 1260;
const STATIC_NAME_LEN: usize = 0x40;

/// The GPU's name from the reply's `gpuNameString`.
pub fn gpu_name(reply: &[u8]) -> Option<&str> {
    let s = reply.get(STATIC_NAME_AT..STATIC_NAME_AT + STATIC_NAME_LEN)?;
    let end = s.iter().position(|&c| c == 0)?;
    core::str::from_utf8(&s[..end]).ok().filter(|n| !n.is_empty())
}

// ---- the CPU sequencer -----------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqOp {
    RegWrite { addr: u32, val: u32 },
    RegModify { addr: u32, mask: u32, val: u32 },
    RegPoll { addr: u32, mask: u32, val: u32, timeout: u32 },
    DelayUs(u32),
    RegStore { addr: u32, index: u32 },
    CoreReset,
    CoreStart,
    CoreWaitForHalt,
    CoreResume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqError {
    /// The payload is shorter than its own header says.
    Truncated,
    /// An opcode this port does not know (`r535_gsp_msg_run_cpu_sequencer` returns `-EINVAL`).
    UnknownOpcode(u32),
    /// A register address outside BAR0 or not word aligned.
    BadAddress(u32),
    /// `REG_STORE` into a slot past the 8 of the save area.
    BadSlot(u32),
    Falcon(FalconError),
    /// `CORE_RESUME`: SEC2's mailbox 0 was not 0.
    Sec2Mailbox(u32),
    /// `CORE_RESUME`: SEC2 did not signal (`0x1180f8` bit 26) in 2 s.
    Sec2Timeout,
    /// `CORE_RESUME`: the RISC-V core is not active.
    RiscvInactive,
}

impl From<FalconError> for SeqError {
    fn from(e: FalconError) -> Self {
        SeqError::Falcon(e)
    }
}

/// `GSP_SEQUENCER_PAYLOAD_SIZE_DWORDS` (`r535/nvrm/gsp.h:684-693`).
fn payload_dwords(opcode: u32) -> Option<usize> {
    Some(match opcode {
        0 => 2, // REG_WRITE
        1 => 3, // REG_MODIFY
        2 => 5, // REG_POLL
        3 => 1, // DELAY_US
        4 => 2, // REG_STORE
        5..=8 => 0,
        _ => return None,
    })
}

/// Decode the payload of a `GSP_RUN_CPU_SEQUENCER` event (`rpc_run_cpu_sequencer_v17_00`:
/// `bufferSizeDWord`, `cmdIndex`, `regSaveArea[8]`, then the command buffer).
pub fn decode_sequencer(payload: &[u8]) -> Result<(Vec<SeqOp>, [u32; 8]), SeqError> {
    if payload.len() < 40 {
        return Err(SeqError::Truncated);
    }
    let buffer_dwords = get32(payload, 0) as usize;
    let used = get32(payload, 4) as usize;
    let mut save = [0u32; 8];
    for (i, s) in save.iter_mut().enumerate() {
        *s = get32(payload, 8 + i * 4);
    }
    let buf = &payload[40..];
    if used > buffer_dwords || used * 4 > buf.len() {
        return Err(SeqError::Truncated);
    }
    let dw = |i: usize| get32(buf, i * 4);
    let mut ops = Vec::new();
    let mut p = 0;
    while p < used {
        let opcode = dw(p);
        let n = payload_dwords(opcode).ok_or(SeqError::UnknownOpcode(opcode))?;
        if p + 1 + n > used {
            return Err(SeqError::Truncated);
        }
        let a = |k: usize| dw(p + 1 + k);
        ops.push(match opcode {
            0 => SeqOp::RegWrite { addr: a(0), val: a(1) },
            1 => SeqOp::RegModify { addr: a(0), mask: a(1), val: a(2) },
            2 => SeqOp::RegPoll { addr: a(0), mask: a(1), val: a(2), timeout: a(3) },
            3 => SeqOp::DelayUs(a(0)),
            4 => SeqOp::RegStore { addr: a(0), index: a(1) },
            5 => SeqOp::CoreReset,
            6 => SeqOp::CoreStart,
            7 => SeqOp::CoreWaitForHalt,
            _ => SeqOp::CoreResume,
        });
        p += 1 + n;
    }
    Ok((ops, save))
}

/// What the core operations need to know.
#[derive(Debug, Clone, Copy)]
pub struct SeqEnv {
    pub gsp: Falcon,
    pub sec2: Falcon,
    /// The LibOS arguments' bus address (`gsp->libos.addr`).
    pub libos_addr: u64,
    /// The bootloader's `appVersion` (`gsp->boot.app_version`).
    pub app_version: u32,
    /// Size of BAR0, to refuse addresses outside it before touching anything.
    pub bar0_len: u32,
}

/// Refuse a sequence with an address outside BAR0 or a store slot past 8,
/// before any register is touched.
pub fn validate_sequencer(ops: &[SeqOp], env: &SeqEnv) -> Result<(), SeqError> {
    let ok = |a: u32| a % 4 == 0 && (a as u64) + 4 <= env.bar0_len as u64;
    for op in ops {
        match *op {
            SeqOp::RegWrite { addr, .. } | SeqOp::RegModify { addr, .. } | SeqOp::RegPoll { addr, .. } if !ok(addr) => return Err(SeqError::BadAddress(addr)),
            SeqOp::RegStore { addr, index } => {
                if !ok(addr) {
                    return Err(SeqError::BadAddress(addr));
                }
                if index >= 8 {
                    return Err(SeqError::BadSlot(index));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// `r535_gsp_msg_run_cpu_sequencer` (`r535/gsp.c:1007-1130`). A `REG_POLL` that
/// times out is not an error there (its `error` field is commented out); the
/// number of them is returned so the caller can report it.
pub fn run_sequencer(m: &impl Mmio, env: &SeqEnv, ops: &[SeqOp], save: &mut [u32; 8]) -> Result<u32, SeqError> {
    validate_sequencer(ops, env)?;
    let mut timeouts = 0;
    for op in ops {
        match *op {
            SeqOp::RegWrite { addr, val } => m.wr32(addr, val),
            SeqOp::RegModify { addr, mask, val } => {
                m.mask(addr, mask, val);
            }
            SeqOp::RegPoll { addr, mask, val, timeout } => {
                let usec = if timeout != 0 { timeout } else { 4_000_000 };
                m.rd32(addr);
                if !poll(m, usec, || m.rd32(addr) & mask == val) {
                    timeouts += 1;
                }
            }
            SeqOp::DelayUs(us) => m.udelay(us),
            SeqOp::RegStore { addr, index } => save[index as usize] = m.rd32(addr),
            SeqOp::CoreReset => {
                env.gsp.reset(m)?;
                env.gsp.wr(m, 0x624, env.gsp.rd(m, 0x624) & !0x80 | 0x80);
                env.gsp.wr(m, 0x10c, 0);
            }
            SeqOp::CoreStart => {
                if env.gsp.rd(m, 0x100) & 0x40 != 0 {
                    env.gsp.wr(m, 0x130, 2);
                } else {
                    env.gsp.wr(m, 0x100, 2);
                }
            }
            SeqOp::CoreWaitForHalt => {
                if !poll(m, 2_000_000, || env.gsp.rd(m, 0x100) & 0x10 != 0) {
                    timeouts += 1;
                }
            }
            SeqOp::CoreResume => {
                // ga102_gsp_reset, the LibOS address, SEC2 started (its booter
                // image is still in its memory), then the same checks as the boot.
                env.gsp.gsp_reset(m)?;
                env.gsp.set_mailboxes(m, env.libos_addr as u32, (env.libos_addr >> 32) as u32);
                env.sec2.wr(m, 0x130, 2);
                if !poll(m, 2_000_000, || m.rd32(0x11_80f8) & 0x0400_0000 != 0) {
                    return Err(SeqError::Sec2Timeout);
                }
                let mbox0 = env.sec2.rd(m, 0x040);
                if mbox0 != 0 {
                    return Err(SeqError::Sec2Mailbox(mbox0));
                }
                env.gsp.wr(m, 0x080, env.app_version);
                if !env.gsp.riscv_active(m) {
                    return Err(SeqError::RiscvInactive);
                }
            }
        }
    }
    Ok(timeouts)
}

/// `nvkm_usec`/`nvkm_msec`: read until `done`, at most `us` iterations of one
/// microsecond each; true if it was met (also on the last try).
fn poll(m: &impl Mmio, us: u32, done: impl Fn() -> bool) -> bool {
    let mut left = us;
    loop {
        if done() {
            return true;
        }
        if left == 0 {
            return false;
        }
        m.udelay(1);
        left -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::falcon::tests::Sim;
    use crate::falcon::GSP;
    use crate::mmio::testing::ReplayMmio;
    use alloc::string::String;
    use core::cell::RefCell;

    /// A `Shm` over a byte vector.
    struct Mem(RefCell<Vec<u8>>);
    impl Mem {
        fn new() -> Mem {
            Mem(RefCell::new(vec![0u8; crate::gspmem::SHARED_SIZE]))
        }
    }
    impl Shm for Mem {
        fn rd32(&self, off: usize) -> u32 {
            get32(&self.0.borrow(), off)
        }
        fn wr32(&self, off: usize, v: u32) {
            put32(&mut self.0.borrow_mut(), off, v)
        }
        fn read(&self, off: usize, out: &mut [u8]) {
            out.copy_from_slice(&self.0.borrow()[off..off + out.len()])
        }
        fn write(&self, off: usize, data: &[u8]) {
            self.0.borrow_mut()[off..off + data.len()].copy_from_slice(data)
        }
    }

    /// Put a status-queue element the way GSP-RM does, for `recv` to read.
    fn gsp_writes(shm: &Mem, function: u32, payload: &[u8]) {
        let msg = build_message(function, payload, 0, 0).unwrap();
        let cnt = QUEUE_ENTRIES;
        let mut w = shm.rd32(MSGQ_WPTR);
        let (mut off, mut left) = (0, msg.len());
        while left > 0 {
            let size = left.min((cnt - w) as usize * PAGE);
            shm.write(MSGQ_OFFSET + PAGE + w as usize * PAGE, &msg[off..off + size]);
            w = (w + size.div_ceil(PAGE) as u32) % cnt;
            off += size;
            left -= size;
        }
        shm.wr32(MSGQ_WPTR, w);
    }

    // ---- payloads against what nouveau dumped in trace-gsp ----

    #[test]
    fn system_info_reproduces_nouveaus_rpc() {
        // trace-gsp: BAR0 0xf5000000, BAR1 0x7c00000000, BAR3 0x7e00000000, 09:00.0,
        // 0x2507 / 0x10de, subsystem 0x19da / 0xc630 (as read), rev 0xa1.
        let info = SystemInfo {
            gpu_phys_addr: 0xf500_0000,
            gpu_phys_fb_addr: 0x7c_0000_0000,
            gpu_phys_inst_addr: 0x7e_0000_0000,
            bus_device_func: 0x900,
            max_user_va: 0x7fff_ffff_f000,
            pci_config_mirror_base: 0x88000,
            pci_config_mirror_size: 0x1000,
            pci_device_id: 0x2507_10de,
            pci_sub_device_id: 0xc630_19da,
            pci_revision_id: 0xa1,
            is_primary: false,
        };
        let want = include_bytes!("../fixtures/rpc72.bin");
        assert_eq!(want.len(), SYSTEM_INFO_SIZE);
        assert_eq!(info.to_bytes(), want.to_vec());
    }

    #[test]
    fn system_info_offsets_match_the_c_struct() {
        // offsets from clang (nvgpu/gen/sysinfo.c), one field at a time
        let mut i = SystemInfo {
            gpu_phys_addr: 1,
            gpu_phys_fb_addr: 2,
            gpu_phys_inst_addr: 3,
            bus_device_func: 4,
            max_user_va: 5,
            pci_config_mirror_base: 6,
            pci_config_mirror_size: 7,
            pci_device_id: 8,
            pci_sub_device_id: 9,
            pci_revision_id: 10,
            is_primary: true,
        };
        let b = i.to_bytes();
        for (off, v) in [(0, 1u64), (8, 2), (16, 3), (32, 4), (72, 5)] {
            assert_eq!(u64::from_le_bytes(b[off..off + 8].try_into().unwrap()), v, "offset {off}");
        }
        for (off, v) in [(80, 6u32), (84, 7), (88, 8), (92, 9), (96, 10)] {
            assert_eq!(get32(&b, off), v, "offset {off}");
        }
        assert_eq!(b[896], 1, "bIsPrimary");
        i.is_primary = false;
        assert_eq!(i.to_bytes()[896], 0);
        assert_eq!(b.len(), 928);
    }

    #[test]
    fn registry_reproduces_nouveaus_rpc() {
        let want = include_bytes!("../fixtures/rpc73.bin");
        assert_eq!(registry(&REGISTRY), want.to_vec());
        assert_eq!(want.len(), 0x75);
        // other tables: sizes and offsets follow from the entries
        let one = registry(&[("A", 7)]);
        assert_eq!(one.len(), 8 + 16 + 2);
        assert_eq!(get32(&one, 0), 26);
        assert_eq!(get32(&one, 4), 1);
        assert_eq!(get32(&one, 8), 24, "nameOffset");
        assert_eq!(one[12], 1);
        assert_eq!(get32(&one, 16), 7);
        assert_eq!(get32(&one, 20), 4);
        assert_eq!(&one[24..], b"A\0");
        assert_eq!(registry(&[]).len(), 8);
    }

    #[test]
    fn the_gpu_name_of_the_real_reply() {
        let reply = include_bytes!("../fixtures/rpc65reply.bin");
        assert_eq!(reply.len(), STATIC_INFO_SIZE);
        assert_eq!(gpu_name(reply), Some("NVIDIA GeForce RTX 3050"));
        assert_eq!(gpu_name(&reply[..100]), None);
        let mut blank = reply.to_vec();
        blank[STATIC_NAME_AT..STATIC_NAME_AT + 64].fill(0);
        assert_eq!(gpu_name(&blank), None);
        blank[STATIC_NAME_AT..STATIC_NAME_AT + 64].fill(b'x');
        assert_eq!(gpu_name(&blank), None, "no terminator inside the field");
    }

    // ---- the message format ----

    #[test]
    fn message_layout_and_checksum() {
        let m = build_message(FN_SET_REGISTRY, &[1, 2, 3], 5, 0).unwrap();
        assert_eq!(m.len(), 4096);
        assert_eq!(get32(&m, 36), 5, "sequence");
        assert_eq!(get32(&m, 40), 1, "elem_count");
        assert_eq!(get32(&m, 44), 0, "pad");
        assert_eq!(get32(&m, 48), 0x0300_0000, "header_version");
        assert_eq!(&m[52..56], b"VRPC", "signature 'CPRV' as a little-endian u32");
        assert_eq!(get32(&m, 56), 32 + 3, "length");
        assert_eq!(get32(&m, 60), 73);
        assert_eq!(get32(&m, 64), 0xffff_ffff);
        assert_eq!(get32(&m, 68), 0xffff_ffff);
        assert_eq!(get32(&m, 72), 0, "rpc sequence");
        assert_eq!(&m[80..83], &[1, 2, 3]);
        // XOR of all u64 words (checksum included) folds to 0
        let x = m.chunks_exact(8).fold(0u64, |a, w| a ^ u64::from_le_bytes(w.try_into().unwrap()));
        assert_eq!((x >> 32) as u32 ^ x as u32, 0);
        // a payload that spills into a second page
        let big = build_message(FN_SET_REGISTRY, &vec![7u8; 4100], 0, 0).unwrap();
        assert_eq!(big.len(), 8192);
        assert_eq!(get32(&big, 40), 2);
        // the maximum is 16 pages including the headers
        assert!(build_message(1, &vec![0; MSG_MAX - MSG_HDR - RPC_HDR], 0, 0).is_ok());
        assert_eq!(build_message(1, &vec![0; MSG_MAX - MSG_HDR - RPC_HDR + 1], 0, 0), Err(RpcError::TooBig));
    }

    #[test]
    fn rpc_length_is_exact_but_the_buffer_is_8_aligned() {
        // rpc_len 35 -> buffer 40: the element still fits one page and length says 35.
        let m = build_message(1, &[0; 3], 0, 0).unwrap();
        assert_eq!(get32(&m, 56), 35);
        // payload filling the page exactly: 4096 - 48 - 32 = 4016 bytes
        assert_eq!(build_message(1, &vec![0; 4016], 0, 0).unwrap().len(), 4096);
        assert_eq!(build_message(1, &vec![0; 4017], 0, 0).unwrap().len(), 8192);
    }

    #[test]
    fn send_writes_the_element_moves_the_pointer_and_rings_the_bell() {
        let shm = Mem::new();
        let m = Sim::new(GSP.base);
        let mut q = Queues::default();
        q.send(&shm, &m, &GSP, FN_GSP_SET_SYSTEM_INFO, &vec![9u8; 928], true).unwrap();
        // 48 + 32 + 928 = 1008 -> one page at entry 0
        assert_eq!(shm.rd32(CMDQ_WPTR), 1);
        let mut e = vec![0u8; 4096];
        shm.read(CMDQ_OFFSET + PAGE, &mut e);
        assert_eq!(e, build_message(FN_GSP_SET_SYSTEM_INFO, &vec![9u8; 928], 0, 0).unwrap());
        assert_eq!(m.writes.borrow().as_slice(), [(GSP.base + 0xc00, 0)]);
        assert_eq!((q.cmdq_seq, q.rpc_seq), (1, 0), "NOSEQ leaves the RPC counter alone");
        // a second one: sequences advance, entry 1
        q.send(&shm, &m, &GSP, FN_SET_REGISTRY, &registry(&REGISTRY), false).unwrap();
        assert_eq!(shm.rd32(CMDQ_WPTR), 2);
        assert_eq!((q.cmdq_seq, q.rpc_seq), (2, 1));
        shm.read(CMDQ_OFFSET + 2 * PAGE, &mut e);
        assert_eq!(e, build_message(FN_SET_REGISTRY, &registry(&REGISTRY), 1, 0).unwrap());
    }

    #[test]
    fn send_wraps_around_the_end_and_refuses_when_full() {
        let shm = Mem::new();
        let m = Sim::new(GSP.base);
        let mut q = Queues::default();
        // 62 free entries (one kept empty): a message of 3 pages at wptr 61 wraps
        shm.wr32(CMDQ_WPTR, 61);
        shm.wr32(CMDQ_RPTR, 61); // the GSP has read everything
        let payload = vec![0xabu8; 8200]; // 8232 + 48 -> 3 pages
        q.send(&shm, &m, &GSP, 1, &payload, true).unwrap();
        assert_eq!(shm.rd32(CMDQ_WPTR), 1, "61 + 3 wraps to 1");
        let msg = build_message(1, &payload, 0, 0).unwrap();
        let mut got = vec![0u8; 4096 * 2];
        shm.read(CMDQ_OFFSET + PAGE + 61 * PAGE, &mut got);
        assert_eq!(got, msg[..8192]);
        let mut tail = vec![0u8; 4096];
        shm.read(CMDQ_OFFSET + PAGE, &mut tail);
        assert_eq!(tail, msg[8192..]);
        // full: only 62 entries are usable
        let shm = Mem::new();
        let mut q = Queues::default();
        shm.wr32(CMDQ_WPTR, 0);
        shm.wr32(CMDQ_RPTR, 0);
        for _ in 0..62 {
            q.send(&shm, &m, &GSP, 1, &[0], true).unwrap();
        }
        assert_eq!(q.send(&shm, &m, &GSP, 1, &[0], true), Err(RpcError::Full));
        assert_eq!(q.cmdq_seq, 62, "a refused message does not use a sequence number");
        // the GSP reads one: room again
        shm.wr32(CMDQ_RPTR, 1);
        q.send(&shm, &m, &GSP, 1, &[0], true).unwrap();
        // the ring doorbell rang once per accepted message
        assert_eq!(m.writes.borrow().iter().filter(|w| **w == (GSP.base + 0xc00, 0)).count(), 1 + 62 + 1);
    }

    #[test]
    fn recv_reads_what_the_gsp_wrote_and_moves_the_read_pointer() {
        let shm = Mem::new();
        let q = Queues::default();
        assert_eq!(q.recv(&shm), Ok(None));
        gsp_writes(&shm, EVENT_GSP_INIT_DONE, &[]);
        gsp_writes(&shm, 65, &[1, 2, 3, 4, 5]);
        let a = q.recv(&shm).unwrap().unwrap();
        assert_eq!((a.function, a.result, a.payload.len()), (EVENT_GSP_INIT_DONE, 0xffff_ffff, 0));
        assert_eq!(shm.rd32(MSGQ_RPTR), 1);
        let b = q.recv(&shm).unwrap().unwrap();
        assert_eq!((b.function, b.payload.as_slice()), (65, &[1u8, 2, 3, 4, 5][..]));
        assert_eq!(shm.rd32(MSGQ_RPTR), 2);
        assert_eq!(q.recv(&shm), Ok(None));
    }

    #[test]
    fn recv_handles_a_multi_page_element_that_wraps() {
        let shm = Mem::new();
        let q = Queues::default();
        // the GSP writes 3 pages starting at entry 61
        shm.wr32(MSGQ_WPTR, 61);
        shm.wr32(MSGQ_RPTR, 61);
        let payload: Vec<u8> = (0..8200u32).map(|i| i as u8).collect();
        gsp_writes(&shm, 99, &payload);
        assert_eq!(shm.rd32(MSGQ_WPTR), 1);
        let m = q.recv(&shm).unwrap().unwrap();
        assert_eq!(m.function, 99);
        assert_eq!(m.payload, payload);
        assert_eq!(shm.rd32(MSGQ_RPTR), 1);
    }

    #[test]
    fn recv_waits_for_an_incomplete_element_and_refuses_garbage() {
        let shm = Mem::new();
        let q = Queues::default();
        // a 2-page element of which only the first page has been published
        let payload = vec![5u8; 5000];
        let msg = build_message(7, &payload, 0, 0).unwrap();
        shm.write(MSGQ_OFFSET + PAGE, &msg);
        shm.wr32(MSGQ_WPTR, 1);
        assert_eq!(q.recv(&shm), Ok(None));
        assert_eq!(shm.rd32(MSGQ_RPTR), 0, "nothing consumed");
        shm.wr32(MSGQ_WPTR, 2);
        assert_eq!(q.recv(&shm).unwrap().unwrap().payload, payload);
        // garbage: no signature
        let shm = Mem::new();
        shm.wr32(MSGQ_WPTR, 1);
        assert_eq!(q.recv(&shm), Err(RpcError::BadMessage));
        // an absurd length
        let shm = Mem::new();
        let mut bad = build_message(1, &[], 0, 0).unwrap();
        put32(&mut bad, 56, 0x10_0000);
        shm.write(MSGQ_OFFSET + PAGE, &bad);
        shm.wr32(MSGQ_WPTR, 1);
        assert_eq!(q.recv(&shm), Err(RpcError::BadMessage));
        // pointers out of range
        let shm = Mem::new();
        shm.wr32(MSGQ_WPTR, 200);
        assert_eq!(q.recv(&shm), Err(RpcError::BadMessage));
    }

    #[test]
    fn queue_pointers_sit_where_the_c_headers_put_them() {
        // msgqTxHeader is 32 bytes (clang): writePtr at +16, and the RX header follows at +32.
        assert_eq!(CMDQ_WPTR, 0x1000 + 16);
        assert_eq!(MSGQ_WPTR, 0x41000 + 16);
        assert_eq!(MSGQ_RPTR, 0x1000 + 32);
        assert_eq!(CMDQ_RPTR, 0x41000 + 32);
        assert_eq!(QUEUE_ENTRIES, ((crate::gspmem::CMDQ_SIZE - PAGE) / PAGE) as u32);
    }

    #[test]
    fn function_numbers_and_sizes_are_the_headers() {
        // r570/nvrm/rpcfn.h and msgfn.h; clang for the sizes
        assert_eq!((FN_GET_GSP_STATIC_INFO, FN_CONTINUATION_RECORD, FN_GSP_SET_SYSTEM_INFO, FN_SET_REGISTRY), (65, 71, 72, 73));
        assert_eq!((EVENT_GSP_INIT_DONE, EVENT_GSP_RUN_CPU_SEQUENCER), (0x1001, 0x1002));
        assert_eq!((MSG_HDR, RPC_HDR, MSG_MAX, DOORBELL), (48, 32, 65536, 0xc00));
        // 16 pages minus the 80 header bytes fit, one byte more does not
        assert_eq!(build_message(1, &vec![0; 16 * PAGE - 80], 0, 0).unwrap().len(), 16 * PAGE);
        assert!(build_message(1, &vec![0; 16 * PAGE - 79], 0, 0).is_err());
    }

    #[test]
    fn send_counts_free_entries_around_the_ring() {
        // reader ahead of the writer: 5 -> 10 leaves 4 free entries (one kept empty)
        let m = Sim::new(GSP.base);
        let shm = Mem::new();
        let mut q = Queues::default();
        shm.wr32(CMDQ_WPTR, 5);
        shm.wr32(CMDQ_RPTR, 10);
        assert_eq!(q.send(&shm, &m, &GSP, 1, &vec![0; 4 * PAGE], true), Err(RpcError::Full), "5 pages into 4 free");
        q.send(&shm, &m, &GSP, 1, &vec![0; 3 * PAGE], true).unwrap();
        assert_eq!(shm.rd32(CMDQ_WPTR), 9);
        assert_eq!(q.send(&shm, &m, &GSP, 1, &[0; 4], true), Err(RpcError::Full), "wptr 9, rptr 10: none left");
    }

    #[test]
    fn recv_needs_the_whole_element_even_across_the_wrap() {
        let q = Queues::default();
        let payload = vec![3u8; 8200]; // 3 pages
        let shm = Mem::new();
        shm.wr32(MSGQ_RPTR, 61);
        // published up to entry 0 (= 2 pages from 61): not complete
        gsp_writes_to(&shm, 61, 3, &payload);
        shm.wr32(MSGQ_WPTR, 0);
        assert_eq!(q.recv(&shm), Ok(None));
        shm.wr32(MSGQ_WPTR, 1);
        assert!(q.recv(&shm).unwrap().is_some());
        // an element whose RPC length just fits a page but whose header pushes it to two
        let shm = Mem::new();
        let msg = build_message(9, &vec![1u8; 4060 - RPC_HDR], 0, 0).unwrap();
        assert_eq!(get32(&msg, 56), 4060);
        assert_eq!(msg.len(), 8192);
        shm.write(MSGQ_OFFSET + PAGE, &msg);
        shm.wr32(MSGQ_WPTR, 1);
        assert_eq!(q.recv(&shm), Ok(None), "48 + 4060 needs two pages");
        shm.wr32(MSGQ_WPTR, 2);
        assert_eq!(q.recv(&shm).unwrap().unwrap().payload.len(), 4060 - RPC_HDR);
    }

    fn gsp_writes_to(shm: &Mem, at: u32, _pages: u32, payload: &[u8]) {
        let msg = build_message(1, payload, 0, 0).unwrap();
        let cnt = QUEUE_ENTRIES;
        let (mut w, mut off, mut left) = (at, 0, msg.len());
        while left > 0 {
            let size = left.min((cnt - w) as usize * PAGE);
            shm.write(MSGQ_OFFSET + PAGE + w as usize * PAGE, &msg[off..off + size]);
            w = (w + size.div_ceil(PAGE) as u32) % cnt;
            off += size;
            left -= size;
        }
    }

    #[test]
    fn recv_validates_pointers_length_and_signature_separately() {
        let q = Queues::default();
        let good = build_message(1, &[7; 8], 0, 0).unwrap();
        // pointers past the queue, with a perfectly good element at entry 0
        let shm = Mem::new();
        shm.write(MSGQ_OFFSET + PAGE, &good);
        shm.wr32(MSGQ_WPTR, 200);
        assert_eq!(q.recv(&shm), Err(RpcError::BadMessage));
        let shm = Mem::new();
        shm.write(MSGQ_OFFSET + PAGE, &good);
        shm.wr32(MSGQ_WPTR, 1);
        shm.wr32(MSGQ_RPTR, 63);
        assert_eq!(q.recv(&shm), Err(RpcError::BadMessage), "rptr == cnt is out of range");
        // an RPC shorter than its own header
        let shm = Mem::new();
        let mut short = good.clone();
        put32(&mut short, 56, 8);
        shm.write(MSGQ_OFFSET + PAGE, &short);
        shm.wr32(MSGQ_WPTR, 1);
        assert_eq!(q.recv(&shm), Err(RpcError::BadMessage));
        // the maximum length is fine, one more is not
        let shm = Mem::new();
        let mut max = good.clone();
        put32(&mut max, 56, (MSG_MAX - MSG_HDR) as u32);
        shm.write(MSGQ_OFFSET + PAGE, &max);
        shm.wr32(MSGQ_WPTR, 16);
        assert!(q.recv(&shm).unwrap().is_some(), "16 pages, all published");
        let shm = Mem::new();
        put32(&mut max, 56, (MSG_MAX - MSG_HDR + 1) as u32);
        shm.write(MSGQ_OFFSET + PAGE, &max);
        shm.wr32(MSGQ_WPTR, 17);
        assert_eq!(q.recv(&shm), Err(RpcError::BadMessage));
        // a good length with the wrong signature
        let shm = Mem::new();
        let mut sig = good.clone();
        sig[52] = b'X';
        shm.write(MSGQ_OFFSET + PAGE, &sig);
        shm.wr32(MSGQ_WPTR, 1);
        assert_eq!(q.recv(&shm), Err(RpcError::BadMessage));
    }

    #[test]
    fn recv_keeps_result_result_private_and_sequence_apart() {
        let q = Queues::default();
        let shm = Mem::new();
        let mut m = build_message(0x1002, &[1, 2, 3, 4], 7, 9).unwrap();
        put32(&mut m, 64, 0x11); // rpc_result
        put32(&mut m, 68, 0x22); // rpc_result_private
        shm.write(MSGQ_OFFSET + PAGE, &m);
        shm.wr32(MSGQ_WPTR, 1);
        let got = q.recv(&shm).unwrap().unwrap();
        assert_eq!((got.function, got.result, got.result_private, got.sequence), (0x1002, 0x11, 0x22, 9));
        assert_eq!(got.payload, [1, 2, 3, 4]);
    }

    #[test]
    fn gpu_name_uses_the_whole_64_byte_field() {
        let mut reply = vec![0u8; STATIC_INFO_SIZE];
        reply[1260..1260 + 40].fill(b'y');
        assert_eq!(gpu_name(&reply).map(|n| n.len()), Some(40));
        reply[1260 + 63] = 0;
        reply[1260..1260 + 63].fill(b'z');
        assert_eq!(gpu_name(&reply).map(|n| n.len()), Some(63));
    }

    #[test]
    fn sequencer_header_fields_are_bounds_checked_separately() {
        // declared size plenty, but the bytes are not there
        let mut p = vec![0u8; 44];
        put32(&mut p, 0, 100);
        put32(&mut p, 4, 2);
        put32(&mut p, 40, 5);
        assert_eq!(decode_sequencer(&p), Err(SeqError::Truncated));
        // bytes there, declared size too small
        let mut p = vec![0u8; 40 + 16];
        put32(&mut p, 0, 1);
        put32(&mut p, 4, 2);
        assert_eq!(decode_sequencer(&p), Err(SeqError::Truncated));
    }

    // ---- the sequencer ----

    const SEQ: &[u8] = include_bytes!("../fixtures/rpc-seq.bin");
    const SEQ_OPS: &str = include_str!("../fixtures/seq-ops.txt");
    const SEQ_WINDOW: &str = include_str!("../fixtures/seq-window.txt");

    fn text_of(ops: &[SeqOp]) -> String {
        let mut s = String::new();
        for op in ops {
            use core::fmt::Write;
            let _ = match *op {
                SeqOp::RegWrite { addr, val } => writeln!(s, "seq wr32 {addr:06x} {val:08x}"),
                SeqOp::RegPoll { addr, mask, val, timeout } => {
                    // nouveau prints the timeout after the 4 s default for 0
                    writeln!(s, "seq poll {addr:06x} {mask:08x} {val:08x} {}", if timeout == 0 { 4_000_000 } else { timeout })
                }
                SeqOp::CoreReset => writeln!(s, "seq core reset"),
                SeqOp::CoreStart => writeln!(s, "seq core start"),
                SeqOp::CoreWaitForHalt => writeln!(s, "seq core wait halt"),
                SeqOp::CoreResume => writeln!(s, "seq core resume"),
                other => writeln!(s, "{other:?}"),
            };
        }
        s
    }

    #[test]
    fn the_sequencer_message_decodes_to_what_nouveau_printed() {
        assert_eq!(SEQ.len(), 0x1898);
        let (ops, save) = decode_sequencer(SEQ).unwrap();
        assert_eq!((get32(SEQ, 0), get32(SEQ, 4)), (0x3fe2, 0x61c), "bufferSizeDWord and cmdIndex, as nouveau printed");
        assert_eq!(ops.len(), 420);
        let want: String = SEQ_OPS.lines().filter(|l| !l.starts_with('#')).map(|l| std::format!("{l}\n")).collect();
        let (got, want): (Vec<&str>, Vec<&str>) = (text_of(&ops).leak().lines().collect(), want.leak().lines().collect());
        assert_eq!(got.len(), want.len());
        // compare line by line so a mismatch names the line, not 30 KiB of text
        for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
            assert_eq!(g, w, "command {i}");
        }
        assert_eq!(save, [0; 8]);
    }

    #[test]
    fn sequencer_decoding_rejects_bad_input() {
        assert_eq!(decode_sequencer(&[0; 39]), Err(SeqError::Truncated));
        // cmdIndex past the buffer's declared size
        let mut p = vec![0u8; 40 + 16];
        put32(&mut p, 0, 2);
        put32(&mut p, 4, 3);
        assert_eq!(decode_sequencer(&p), Err(SeqError::Truncated));
        // cmdIndex past the bytes there
        put32(&mut p, 0, 100);
        put32(&mut p, 4, 5);
        assert_eq!(decode_sequencer(&p), Err(SeqError::Truncated));
        // a write cut short: opcode 0 wants 2 more dwords
        put32(&mut p, 4, 2);
        assert_eq!(decode_sequencer(&p), Err(SeqError::Truncated));
        // an opcode nobody knows
        put32(&mut p, 40, 9);
        put32(&mut p, 4, 1);
        assert_eq!(decode_sequencer(&p), Err(SeqError::UnknownOpcode(9)));
        // every opcode and its payload size
        let mut p = vec![0u8; 40 + 4 * 30];
        let cmds: [&[u32]; 9] = [&[0, 1, 2], &[1, 3, 4, 5], &[2, 6, 7, 8, 9, 10], &[3, 11], &[4, 12, 3], &[5], &[6], &[7], &[8]];
        let flat: Vec<u32> = cmds.iter().flat_map(|c| c.iter().copied()).collect();
        put32(&mut p, 0, 30);
        put32(&mut p, 4, flat.len() as u32);
        for (i, v) in flat.iter().enumerate() {
            put32(&mut p, 40 + i * 4, *v);
        }
        let (ops, _) = decode_sequencer(&p).unwrap();
        assert_eq!(
            ops,
            [
                SeqOp::RegWrite { addr: 1, val: 2 },
                SeqOp::RegModify { addr: 3, mask: 4, val: 5 },
                SeqOp::RegPoll { addr: 6, mask: 7, val: 8, timeout: 9 },
                SeqOp::DelayUs(11),
                SeqOp::RegStore { addr: 12, index: 3 },
                SeqOp::CoreReset,
                SeqOp::CoreStart,
                SeqOp::CoreWaitForHalt,
                SeqOp::CoreResume,
            ]
        );
    }

    fn env() -> SeqEnv {
        SeqEnv { gsp: GSP, sec2: crate::booter::SEC2, libos_addr: 0xf7fd_f000, app_version: 0, bar0_len: 16 << 20 }
    }

    fn replay() -> ReplayMmio {
        let mut m = ReplayMmio::from_extract(SEQ_WINDOW);
        // the falcon's timed status registers: idle (no scrub, halted)
        m.fallback = vec![(0x11_00f4, 0x47f7), (0x11_0100, 0x10)];
        m
    }

    #[test]
    fn running_the_sequencer_replays_nouveaus_register_accesses() {
        let (ops, mut save) = decode_sequencer(SEQ).unwrap();
        let m = replay();
        let timeouts = run_sequencer(&m, &env(), &ops, &mut save).unwrap();
        assert_eq!(timeouts, 0);
        let ours = m.writes.borrow().clone();
        assert_eq!(ours.len(), m.expected_writes.len());
        for (i, (a, b)) in ours.iter().zip(m.expected_writes.iter()).enumerate() {
            assert_eq!(a, b, "write {i}");
        }
        // the sequencer's own polls consumed exactly what the trace read
        for reg in [0x11_0040, 0x11_0118, 0x11_80f8, 0x11_1668, 0x11_03c0, 0x00_0000, 0x84_0040, 0x11_1388] {
            assert_eq!(m.unread(reg), 0, "{reg:#x}");
        }
    }

    #[test]
    fn a_hostile_sequence_is_refused_before_it_touches_anything() {
        let m = replay();
        let mut save = [0; 8];
        let ops = [SeqOp::RegWrite { addr: 0x110040, val: 1 }, SeqOp::RegWrite { addr: 0x0100_0000, val: 1 }];
        assert_eq!(run_sequencer(&m, &env(), &ops, &mut save), Err(SeqError::BadAddress(0x0100_0000)));
        assert!(m.writes.borrow().is_empty(), "the first write did not happen");
        assert_eq!(run_sequencer(&m, &env(), &[SeqOp::RegPoll { addr: 0x1001, mask: 0, val: 0, timeout: 1 }], &mut save), Err(SeqError::BadAddress(0x1001)));
        assert_eq!(run_sequencer(&m, &env(), &[SeqOp::RegModify { addr: 0x2, mask: 0, val: 0 }], &mut save), Err(SeqError::BadAddress(2)));
        assert_eq!(run_sequencer(&m, &env(), &[SeqOp::RegStore { addr: 0x10, index: 8 }], &mut save), Err(SeqError::BadSlot(8)));
        // the last word of BAR0 is fine
        assert_eq!(validate_sequencer(&[SeqOp::RegWrite { addr: (16 << 20) - 4, val: 0 }], &env()), Ok(()));
        assert_eq!(validate_sequencer(&[SeqOp::RegWrite { addr: 16 << 20, val: 0 }], &env()), Err(SeqError::BadAddress(16 << 20)));
    }

    #[test]
    fn simple_ops_do_what_they_say() {
        let s = Sim::new(0);
        s.set(0x3000, 0xf0);
        s.set(0x3004, 0x55);
        let mut save = [0u32; 8];
        let ops = [
            SeqOp::RegWrite { addr: 0x10, val: 7 },
            SeqOp::RegModify { addr: 0x3000, mask: 0x0f, val: 0x03 },
            SeqOp::RegStore { addr: 0x3004, index: 5 },
            SeqOp::DelayUs(25),
        ];
        let mut env = env();
        env.gsp = Falcon { base: 0x2_0000, addr2: 0x1000 };
        run_sequencer(&s, &env, &ops, &mut save).unwrap();
        assert_eq!(s.writes.borrow().as_slice(), [(0x10, 7), (0x3000, 0xf3)]);
        assert_eq!(save[5], 0x55);
        assert!(s.delays.borrow().iter().any(|&(_, us)| us == 25));
    }

    #[test]
    fn polls_that_time_out_are_counted_not_fatal() {
        let s = Sim::new(0);
        let mut save = [0u32; 8];
        let mut env = env();
        env.gsp = Falcon { base: 0x2_0000, addr2: 0x1000 };
        let ops = [SeqOp::RegPoll { addr: 0x40, mask: 1, val: 1, timeout: 50 }, SeqOp::RegPoll { addr: 0x40, mask: 1, val: 0, timeout: 50 }];
        assert_eq!(run_sequencer(&s, &env, &ops, &mut save), Ok(1));
        // 1 discarded read + 51 polls for the first; 1 + 1 for the second
        assert_eq!(s.count_reads(0x40), 1 + 51 + 1 + 1);
        // the default timeout is 4 s
        let ops = [SeqOp::RegPoll { addr: 0x44, mask: 1, val: 1, timeout: 0 }];
        assert_eq!(run_sequencer(&s, &env, &ops, &mut save), Ok(1));
        assert_eq!(s.count_reads(0x44), 1 + 4_000_001);
    }

    #[test]
    fn core_start_uses_the_alias_when_the_falcon_is_in_riscv_mode() {
        let mut env = env();
        env.gsp = Falcon { base: 0x2_0000, addr2: 0x1000 };
        let mut save = [0u32; 8];
        let s = Sim::new(0);
        run_sequencer(&s, &env, &[SeqOp::CoreStart], &mut save).unwrap();
        assert_eq!(s.writes.borrow().as_slice(), [(0x2_0100, 2)]);
        let s = Sim::new(0);
        s.set(0x2_0100, 0x40);
        run_sequencer(&s, &env, &[SeqOp::CoreStart], &mut save).unwrap();
        assert_eq!(s.writes.borrow().as_slice(), [(0x2_0130, 2)]);
    }

    #[test]
    fn core_resume_reports_each_way_it_can_fail() {
        let text = |edit: &dyn Fn(&str) -> String| edit(SEQ_WINDOW);
        let (ops, mut save) = decode_sequencer(SEQ).unwrap();
        let resume = [*ops.last().unwrap()];
        assert_eq!(resume, [SeqOp::CoreResume]);
        let mut run = |t: String| {
            let mut m = ReplayMmio::from_extract(&t);
            m.fallback = vec![(0x11_00f4, 0x47f7), (0x11_0100, 0x10)];
            run_sequencer(&m, &env(), &resume, &mut save)
        };
        assert_eq!(run(text(&|t| t.to_string())), Ok(0));
        // SEC2 answers 0x40 in its mailbox
        assert_eq!(run(text(&|t| t.replace("R 0x840040 0x00000000\nW 0x110080", "R 0x840040 0x00000040\nW 0x110080"))), Err(SeqError::Sec2Mailbox(0x40)));
        // the RISC-V bit does not come up
        assert_eq!(run(text(&|t| t.replace("R 0x111388 0x00000080", "R 0x111388 0x00000000"))), Err(SeqError::RiscvInactive));
        // SEC2 never signals
        assert_eq!(run(text(&|t| t.replace("0x17100000", "0x13100000"))), Err(SeqError::Sec2Timeout));
    }

    #[test]
    fn event_numbers_and_names() {
        // nvrm/msgfn.h counts from FIRST_EVENT = 0x1000; r570 adds NOCAT at 0x1020
        assert_eq!((EVENT_POST_EVENT, EVENT_RC_TRIGGERED, EVENT_MMU_FAULT_QUEUED, EVENT_OS_ERROR_LOG), (0x1003, 0x1004, 0x1005, 0x1006));
        assert_eq!((EVENT_UCODE_LIBOS_PRINT, EVENT_GSP_POST_NOCAT_RECORD), (0x100c, 0x1020));
        assert_eq!(event_name(0x1001), "INIT_DONE");
        assert_eq!(event_name(0x1002), "RUN_CPU_SEQUENCER");
        assert_eq!(event_name(0x1003), "POST_EVENT");
        assert_eq!(event_name(0x1004), "RC_TRIGGERED");
        assert_eq!(event_name(0x1005), "MMU_FAULT_QUEUED");
        assert_eq!(event_name(0x1006), "OS_ERROR_LOG");
        assert_eq!(event_name(0x100c), "UCODE_LIBOS_PRINT");
        assert_eq!(event_name(0x1020), "POST_NOCAT_RECORD");
        assert_eq!(event_name(0x1000), "?");
        assert_eq!(event_name(0x9999), "?");
    }

    #[test]
    fn an_rc_names_its_channel() {
        let mut p = vec![0u8; 64];
        p[0..4].copy_from_slice(&1u32.to_le_bytes()); // engine GR0
        p[4..8].copy_from_slice(&5u32.to_le_bytes());
        assert_eq!(rc_chid(&p), Some(5));
        assert_eq!(rc_chid(&p[..7]), None);
    }
}
