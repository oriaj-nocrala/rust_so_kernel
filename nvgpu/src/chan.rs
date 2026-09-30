//! A GPFIFO channel with the Ampere copy engine, through GSP-RM (phase 6c).
//! Pure: parameter blocks for the RM calls, the GPFIFO entry and the push
//! buffer that makes the copy engine copy; the kernel adapter
//! (`kernel/src/gpu/copy.rs`) owns memory, the doorbell and the waiting.
//!
//! Sources: nouveau `rm/r570/fifo.c:20-107` (`r570_chan_alloc`) and
//! `rm/r535/fifo.c:150-235` (`r535_chan_ramfc_write`: alloc, BIND, SCHEDULE),
//! `rm/r535/ce.c` (the copy object), `r570/nvrm/fifo.h` (parameter layouts,
//! 570.144), open-gpu-kernel-modules `clc56f.h` (GPFIFO entry, method header),
//! `clc7b5.h` (copy methods), `ctrlc36f.h` (work-submit token). Oracle: the
//! RPCs nouveau sent in `trace-gsp` (`fixtures/rm-ph6-*`).

use alloc::vec;
use alloc::vec::Vec;

// ---- classes, controls, handles ---------------------------------------------------

/// `AMPERE_CHANNEL_GPFIFO_A` and `AMPERE_DMA_COPY_B`.
pub const CLASS_GPFIFO: u32 = 0xc56f;
pub const CLASS_COPY: u32 = 0xc7b5;
/// `NVA06F_CTRL_CMD_BIND` (params: `engineType`), `..._GPFIFO_SCHEDULE`
/// (`bEnable`, `bSkipSubmit`), `NVC36F_CTRL_CMD_GPFIFO_GET_WORK_SUBMIT_TOKEN`
/// (reply: `workSubmitToken`).
pub const CTRL_BIND: u32 = 0xa06f_0104;
pub const CTRL_GPFIFO_SCHEDULE: u32 = 0xa06f_0103;
pub const CTRL_GET_WORK_SUBMIT_TOKEN: u32 = 0xc36f_0108;

/// `NVKM_RM_CHAN(chid)` (`rm/handles.h:16`).
pub fn h_chan(chid: u32) -> u32 {
    0xf1f0_0000 | chid
}
/// The copy object's handle in the trace (`fixtures/rm-ph6-ce-alloc-c7b5-req.bin`).
pub const H_COPY: u32 = 0x0004_c7b5;
/// G4e: a copy object on the GR channel too (NVK pushes image copies to the copy subchannel of its graphics queue). Handle and engine
/// `NV2080_ENGINE_TYPE_COPY0` (`r535/ce.c:38`, `COPY0 + inst`); CE0 shares runlist 0 with GR.
pub const H_COPY_GR: u32 = 0x0005_c7b5;
pub const ENGINE_COPY0: u32 = 9;

/// `NV2080_ENGINE_TYPE_COPY2` = `RM_ENGINE_TYPE_COPY2` (`rm/r535/nvrm/engine.h:136,192`:
/// COPY0 is 9). The trace's CE channel has `engineType = 0xb` and the device
/// table puts CE2 alone on runlist 1, while CE0/CE1 share runlist 0 with GR
/// (`fixtures/rm-ph6-devinfo-rep.bin`): this is the async copy engine.
pub const ENGINE_COPY2: u32 = 0xb;

/// `NV_CHANNELGPFIFO_ALLOCATION_PARAMETERS` in 570.144: 368 bytes.
pub const CHAN_PARAMS_SIZE: usize = 368;
/// A channel's USERD is 0x200 bytes, its instance block 0x1000, the method
/// buffer 0x5000 (`fifo->rm.mthdbuf_size` in the trace).
pub const USERD_SIZE: u64 = 0x200;
pub const INST_SIZE: u64 = 0x1000;
pub const MTHDBUF_SIZE: u64 = 0x5000;

/// `NV_MEMORY_DESC_PARAMS.addressSpace`: RM's `ADDR_SYSMEM = 1`, `ADDR_FBMEM = 2`.
pub const ADDR_SYSMEM: u32 = 1;
pub const ADDR_FBMEM: u32 = 2;

/// Where and what a channel is (all the inputs of `r570_chan_alloc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChanAlloc {
    /// nouveau's channel id: picks the USERD slot (`chid % 8`, page `chid / 8`)
    /// and the object handle (`h_chan(chid)`).
    pub chid: u32,
    pub privileged: bool,
    /// `NV2080_ENGINE_TYPE_*` the channel runs on.
    pub engine_type: u32,
    /// The GPFIFO's address in the channel's VA space and its size in bytes.
    pub gpfifo_va: u64,
    pub gpfifo_bytes: u32,
    /// VRAM addresses of the instance block (RAMFC at its start) and USERD.
    pub inst: u64,
    pub userd: u64,
    /// Bus address of the method buffer in system memory.
    pub mthdbuf: u64,
    /// The VA space object's handle.
    pub vaspace: u32,
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}
fn get32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// `NV_MEMORY_DESC_PARAMS` at `at`: base, size, addressSpace, cacheAttrib.
fn mem_desc(p: &mut [u8], at: usize, base: u64, size: u64, space: u32, cache: u32) {
    put64(p, at, base);
    put64(p, at + 8, size);
    put32(p, at + 16, space);
    put32(p, at + 20, cache);
}

/// `NVOS04_FLAGS_*` as `r570_chan_alloc` sets them (`fifo.h:106-152`): physical
/// channel type (0), runqueue 0, `PRIVILEGED_CHANNEL` bit 5, USERD index in
/// bits 10:8, USERD page in 20:12 with `PAGE_FIXED` bit 21.
pub fn chan_flags(chid: u32, privileged: bool) -> u32 {
    (if privileged { 1 << 5 } else { 0 }) | ((chid % 8) << 8) | (((chid / 8) & 0x1ff) << 12) | (1 << 21)
}

/// `NV_KERNELCHANNEL_ALLOC_INTERNALFLAGS_*` (`fifo.h:154-170`): privilege in
/// bits 1:0 (USER 0, ADMIN 1), error notifier type NONE (= 1) in 3:2 and the ECC
/// one in 5:4.
pub fn internal_flags(privileged: bool) -> u32 {
    (if privileged { 1 } else { 0 }) | (1 << 2) | (1 << 4)
}

/// The 368-byte parameter block of `RM_ALLOC AMPERE_CHANNEL_GPFIFO_A`.
pub fn alloc_params(c: &ChanAlloc) -> Vec<u8> {
    let mut p = vec![0u8; CHAN_PARAMS_SIZE];
    put64(&mut p, 8, c.gpfifo_va); // gpFifoOffset
    put32(&mut p, 16, c.gpfifo_bytes / 8); // gpFifoEntries
    put32(&mut p, 20, chan_flags(c.chid, c.privileged));
    put32(&mut p, 28, c.vaspace); // hVASpace
    put32(&mut p, 128, c.engine_type);
    mem_desc(&mut p, 144, c.inst, INST_SIZE, ADDR_FBMEM, 1); // instanceMem
    mem_desc(&mut p, 168, c.userd, USERD_SIZE, ADDR_FBMEM, 1); // userdMem
    mem_desc(&mut p, 192, c.inst, 0x200, ADDR_FBMEM, 1); // ramfcMem
    mem_desc(&mut p, 216, c.mthdbuf, MTHDBUF_SIZE, ADDR_SYSMEM, 0); // mthdbufMem
    put32(&mut p, 244, internal_flags(c.privileged));
    p
}

/// `NVA06F_CTRL_BIND_PARAMS`: the engine type.
pub fn bind_params(engine_type: u32) -> Vec<u8> {
    engine_type.to_le_bytes().to_vec()
}

/// `NVA06F_CTRL_GPFIFO_SCHEDULE_PARAMS`: `bEnable = 1`, `bSkipSubmit = 0`.
pub fn schedule_params() -> Vec<u8> {
    vec![1, 0]
}

/// `NVC0B5_ALLOCATION_PARAMETERS` (`r535_ce_alloc`): `version = 1`, `engineType`.
pub fn copy_params(engine_type: u32) -> Vec<u8> {
    let mut p = vec![0u8; 8];
    put32(&mut p, 0, 1);
    put32(&mut p, 4, engine_type);
    p
}

/// The token for the doorbell, from `GET_WORK_SUBMIT_TOKEN`'s reply.
pub fn token_from_params(params: &[u8]) -> Option<u32> {
    (params.len() >= 4).then(|| get32(params, 0))
}

/// The parameter block of `GET_WORK_SUBMIT_TOKEN` (4 bytes, out only).
pub fn token_request_params() -> Vec<u8> {
    vec![0u8; 4]
}

/// A channel's `cid` as RM reports it in the reply of the ALLOC (offset 132 of
/// the parameter block, after the 32-byte ALLOC header).
pub fn cid_from_reply(payload: &[u8]) -> Option<u32> {
    (payload.len() >= crate::rm::ALLOC_HDR + CHAN_PARAMS_SIZE).then(|| get32(payload, crate::rm::ALLOC_HDR + 132))
}

// ---- the doorbell token -----------------------------------------------------------

/// `NV2080_CTRL_CMD_FIFO_GET_DEVICE_INFO_TABLE` (`r535/nvrm/fifo.h:27`), sent to
/// the subdevice: `baseIndex`, `numEntries`, `bMore` (12 bytes with padding),
/// then 32 entries of 100 bytes (`engineData[16]`, `pbdmaIds[2]`,
/// `pbdmaFaultIds[2]`, `numPbdmas`, `engineName[16]`).
pub const CTRL_FIFO_GET_DEVICE_INFO_TABLE: u32 = 0x2080_1112;
pub const DEVICE_INFO_PARAMS_SIZE: usize = 12 + 32 * DEVICE_ENTRY_SIZE;
const DEVICE_ENTRY_SIZE: usize = 100;
/// Indices into `engineData` (`ENGINE_INFO_TYPE_*`, `fifo.h:44-116`).
const ENGINE_INFO_RM_ENGINE_TYPE: usize = 2;
const ENGINE_INFO_RUNLIST: usize = 3;

pub fn device_info_params() -> Vec<u8> {
    vec![0u8; DEVICE_INFO_PARAMS_SIZE]
}

/// The runlist an engine (`RM_ENGINE_TYPE_*`) is on, from the reply's parameter
/// block (what `r535_fifo_runl_ctor` reads, `r535/fifo.c:451-470`).
pub fn runlist_for_engine(params: &[u8], engine_type: u32) -> Option<u32> {
    if params.len() < DEVICE_INFO_PARAMS_SIZE {
        return None;
    }
    let n = (get32(params, 4) as usize).min(32);
    (0..n).find_map(|i| {
        let e = 12 + i * DEVICE_ENTRY_SIZE;
        (get32(params, e + 4 * ENGINE_INFO_RM_ENGINE_TYPE) == engine_type).then(|| get32(params, e + 4 * ENGINE_INFO_RUNLIST))
    })
}

/// The doorbell value on Turing and later with GSP-RM (`tu102_chan_doorbell_handle`,
/// `engine/fifo/tu102.c:35`; `NV_CTRL_VF_DOORBELL` = runlist id above bit 16, the
/// channel id in the low bits). RM's own `GET_WORK_SUBMIT_TOKEN` reply is
/// not it: CPU-RM recomputes the token after the RPC with the channel's real
/// runlist (`kchannelCtrlCmdGpfifoGetWorkSubmitToken`), while GSP builds one
/// for runlist 0 (boot #102-#103: 0x2 for chid 2 on runlist 1).
pub fn doorbell_token(runlist: u32, chid: u32) -> u32 {
    (runlist << 16) | chid
}

// ---- USERD -----------------------------------------------------------------------

/// `AmpereAControlGPFifo` (`clc56f.h`): the USERD fields the host uses.
pub const USERD_GP_GET: u64 = 0x88;
pub const USERD_GP_PUT: u64 = 0x8c;

// ---- GPFIFO entries and push buffers -----------------------------------------------

/// One GPFIFO entry (`NVC56F_GP_ENTRY0/1`, `clc56f.h:266-277`): word 0 holds
/// `address[31:2]` (FETCH 0 = unconditional), word 1 `address[39:32]` in bits
/// 7:0, LEVEL 0 (main), LENGTH in 32-bit words in bits 30:10, SYNC 0.
pub fn gp_entry(va: u64, len_bytes: u32) -> u64 {
    assert!(va % 4 == 0 && len_bytes % 4 == 0 && (len_bytes / 4) < (1 << 21) && va >> 40 == 0);
    let w0 = (va & 0xffff_fffc) as u32;
    let w1 = ((va >> 32) as u32 & 0xff) | ((len_bytes / 4) << 10);
    (w1 as u64) << 32 | w0 as u64
}

/// The subchannel the copy object is bound to (nouveau's `NvSubCopy`).
pub const SUBCH_COPY: u32 = 4;

/// `NVC56F_DMA_INCR_*` (`clc56f.h:310-314`): opcode 1 in bits 31:29, count in
/// 28:16, subchannel 15:13, method address `>> 2` in 11:0.
pub fn incr_header(subch: u32, method: u32, count: u32) -> u32 {
    assert!(subch < 8 && method % 4 == 0 && method < 0x4000 && count > 0 && count < 0x2000);
    (1 << 29) | (count << 16) | (subch << 13) | (method >> 2)
}

// `clc7b5.h` copy methods.
const CE_SET_SEMAPHORE_A: u32 = 0x240;
const CE_LAUNCH_DMA: u32 = 0x300;
const CE_OFFSET_IN_UPPER: u32 = 0x400;
const CE_PITCH_IN: u32 = 0x410;

/// `NVC7B5_LAUNCH_DMA`: DATA_TRANSFER_TYPE NON_PIPELINED (2) in 1:0, FLUSH_ENABLE
/// bit 2, SEMAPHORE_TYPE RELEASE_ONE_WORD (1) in 4:3, SRC/DST_MEMORY_LAYOUT
/// PITCH bits 7 and 8; SRC/DST_TYPE VIRTUAL (0), one line.
pub const LAUNCH_DMA_COPY_WITH_SEMAPHORE: u32 = 2 | (1 << 2) | (1 << 3) | (1 << 7) | (1 << 8);

/// `NVC7B5_LAUNCH_DMA_MULTI_LINE_ENABLE` (bit 9): copy `LINE_COUNT` lines of
/// `LINE_LENGTH_IN` bytes, `PITCH_IN`/`PITCH_OUT` apart (`clc7b5.h:112`).
pub const LAUNCH_DMA_MULTI_LINE: u32 = 1 << 9;

/// The push buffer of one linear copy of `len` bytes between two virtual
/// addresses of the channel's space, ending with a one-word semaphore release
/// (`payload` at `sem_va`) after the data is flushed.
pub fn copy_push(src_va: u64, dst_va: u64, len: u32, sem_va: u64, payload: u32) -> Vec<u32> {
    copy_rect_push(src_va, dst_va, len, len, len, 1, sem_va, payload)
}

/// The push buffer of a pitch-linear 2D copy: `lines` lines of `line_bytes`
/// bytes, the source's lines `src_pitch` apart and the destination's `dst_pitch`
/// apart (a rectangle of a framebuffer), then the semaphore release. With one
/// line it is exactly [`copy_push`]; with more, MULTI_LINE_ENABLE is set.
#[allow(clippy::too_many_arguments)]
pub fn copy_rect_push(src_va: u64, dst_va: u64, src_pitch: u32, dst_pitch: u32, line_bytes: u32, lines: u32, sem_va: u64, payload: u32) -> Vec<u32> {
    assert!(lines > 0 && line_bytes > 0 && line_bytes <= src_pitch.min(dst_pitch));
    let mut w = Vec::new();
    // SET_OBJECT (method 0): the class binds the copy object to the subchannel
    w.push(incr_header(SUBCH_COPY, 0, 1));
    w.push(CLASS_COPY);
    // OFFSET_IN_UPPER, OFFSET_IN_LOWER, OFFSET_OUT_UPPER, OFFSET_OUT_LOWER
    w.push(incr_header(SUBCH_COPY, CE_OFFSET_IN_UPPER, 4));
    w.extend([(src_va >> 32) as u32, src_va as u32, (dst_va >> 32) as u32, dst_va as u32]);
    // PITCH_IN, PITCH_OUT, LINE_LENGTH_IN, LINE_COUNT
    w.push(incr_header(SUBCH_COPY, CE_PITCH_IN, 4));
    w.extend([src_pitch, dst_pitch, line_bytes, lines]);
    // SET_SEMAPHORE_A (upper), _B (lower), _PAYLOAD
    w.push(incr_header(SUBCH_COPY, CE_SET_SEMAPHORE_A, 3));
    w.extend([(sem_va >> 32) as u32, sem_va as u32, payload]);
    // LAUNCH_DMA
    w.push(incr_header(SUBCH_COPY, CE_LAUNCH_DMA, 1));
    w.push(LAUNCH_DMA_COPY_WITH_SEMAPHORE | if lines > 1 { LAUNCH_DMA_MULTI_LINE } else { 0 });
    w
}

/// `NVC7B5_LAUNCH_DMA_INTERRUPT_TYPE` (bits 6:5) NON_BLOCKING = 2 (`clc7b5.h:102-105`): when the
/// copy finishes the engine raises its non-stall interrupt and goes on.
pub const LAUNCH_DMA_INTERRUPT_NON_BLOCKING: u32 = 2 << 5;

/// `push` (ending in a LAUNCH_DMA, as [`copy_push`] and [`release_push`] do) with the non-stall
/// interrupt added to that launch.
pub fn with_interrupt(mut push: Vec<u32>) -> Vec<u32> {
    let n = push.len();
    assert!(n >= 2 && push[n - 2] == incr_header(SUBCH_COPY, CE_LAUNCH_DMA, 1), "the push does not end in a LAUNCH_DMA");
    push[n - 1] |= LAUNCH_DMA_INTERRUPT_NON_BLOCKING;
    push
}

/// `LAUNCH_DMA` for a semaphore release with no data transfer: DATA_TRANSFER_TYPE
/// NONE (0), FLUSH_ENABLE, SEMAPHORE_TYPE RELEASE_ONE_WORD (UVM's
/// `ce_semaphore_release`).
pub const LAUNCH_DMA_SEMAPHORE_ONLY: u32 = (1 << 2) | (1 << 3);

/// The push buffer of a bare semaphore release: binds the copy object and writes
/// `payload` at `sem_va` (no data moves). The first rung of the bring-up ladder:
/// it shows the channel runs and the semaphore's page is writable.
pub fn release_push(sem_va: u64, payload: u32) -> Vec<u32> {
    let mut w = Vec::new();
    w.push(incr_header(SUBCH_COPY, 0, 1));
    w.push(CLASS_COPY);
    w.push(incr_header(SUBCH_COPY, CE_SET_SEMAPHORE_A, 3));
    w.extend([(sem_va >> 32) as u32, sem_va as u32, payload]);
    w.push(incr_header(SUBCH_COPY, CE_LAUNCH_DMA, 1));
    w.push(LAUNCH_DMA_SEMAPHORE_ONLY);
    w
}

/// The size of [`release_push`] in bytes: the kernel's fence on a copy-engine channel (`/dev/nvgpu`, `hwq::Queue`).
pub const RELEASE_PUSH_BYTES: u32 = 32;

/// Only the object bind of [`release_push`]: `SET_OBJECT` of the copy class on its subchannel. NVK's copy contexts push copy
/// methods at that subchannel without ever binding it, so the kernel runs this before each of their submissions (idempotent).
pub fn bind_push() -> Vec<u32> {
    alloc::vec![incr_header(SUBCH_COPY, 0, 1), CLASS_COPY]
}

/// The size of [`bind_push`] in bytes.
pub const BIND_PUSH_BYTES: u32 = 8;

// ---- the layout the adapter uses -------------------------------------------------------

/// One buffer: where the GPU sees it (VA) and where it is (VRAM address or bus
/// address, per aperture).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub va: u64,
    pub pa: u64,
    pub len: u64,
}

/// VRAM used for the channel's own memory: `[128 MiB, 128 MiB + 64 KiB)`
/// (instance block, USERD, GPFIFO, push buffer, fence page), and the copy
/// destination buffer after it.
pub const VRAM_CHAN: u64 = 128 << 20;
pub const VRAM_DST: u64 = 144 << 20;
pub const GPFIFO_ENTRIES: u32 = 1024;

pub const INST_VRAM: u64 = VRAM_CHAN;
pub const USERD_VRAM: u64 = VRAM_CHAN + 0x1000;
pub const GPFIFO_VRAM: u64 = VRAM_CHAN + 0x2000; // 8 KiB
pub const PUSH_VRAM: u64 = VRAM_CHAN + 0x4000; // 4 KiB
pub const FENCE_VRAM: u64 = VRAM_CHAN + 0x5000; // 4 KiB

/// The channel's VA space (all above 8 GiB, away from the 4 GiB window nouveau
/// leaves to RM in its non-external mode).
pub const GPFIFO_VA: u64 = 0x2_0000_0000;
pub const PUSH_VA: u64 = GPFIFO_VA + 0x10000;
pub const FENCE_VA: u64 = GPFIFO_VA + 0x20000;
pub const DST_VA: u64 = 0x2_1000_0000;
pub const SRC_VA: u64 = 0x2_2000_0000;
pub const BACK_VA: u64 = 0x2_3000_0000;
/// Phase 6d: a fence page in host memory (the CPU polls it from cache, no BAR0
/// read), and the same three buffers seen through 2 MiB pages (`mmu::map_huge`).
pub const HFENCE_VA: u64 = GPFIFO_VA + 0x30000;
pub const HUGE_DST_VA: u64 = 0x2_5000_0000;
pub const HUGE_SRC_VA: u64 = 0x2_6000_0000;
pub const HUGE_BACK_VA: u64 = 0x2_7000_0000;
/// Phase 6d, frame-shaped copies: a 16 MiB host buffer (a framebuffer's shadow)
/// to 16 MiB of VRAM at 160 MiB (where a scanout buffer would be), both through
/// 2 MiB pages.
pub const FRAME_BYTES: u64 = 16 << 20;
pub const FRAME_VRAM: u64 = 160 << 20;
pub const FRAME_SRC_VA: u64 = 0x2_8000_0000;
pub const FRAME_DST_VA: u64 = 0x2_9000_0000;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rm;

    const A_REQ: &[u8] = include_bytes!("../fixtures/rm-ph6-chan-c56f-a-req.bin");
    const A_REP: &[u8] = include_bytes!("../fixtures/rm-ph6-chan-c56f-a-rep.bin");
    const CE_REQ: &[u8] = include_bytes!("../fixtures/rm-ph6-chan-c56f-ce-req.bin");
    const BIND_REQ: &[u8] = include_bytes!("../fixtures/rm-ph6-ce-chan-ctrl-a06f0104-req.bin");
    const SCHED_REQ: &[u8] = include_bytes!("../fixtures/rm-ph6-ce-chan-ctrl-a06f0103-req.bin");
    const CE_ALLOC_REQ: &[u8] = include_bytes!("../fixtures/rm-ph6-ce-alloc-c7b5-req.bin");
    const CE_ALLOC_REP: &[u8] = include_bytes!("../fixtures/rm-ph6-ce-alloc-c7b5-rep.bin");

    // The trace's two channels of client 0xc1d00000 (one on the GR engine, one on the CE).
    fn gr_chan() -> ChanAlloc {
        ChanAlloc {
            chid: 1,
            privileged: false,
            engine_type: 1,
            gpfifo_va: 0x11000,
            gpfifo_bytes: 0x2000,
            inst: 0x1_f07c_8000,
            userd: 0x209_8000,
            mthdbuf: 0xfffe_8000,
            vaspace: rm::H_VASPACE,
        }
    }
    fn ce_chan() -> ChanAlloc {
        ChanAlloc {
            chid: 2,
            privileged: true,
            engine_type: ENGINE_COPY2,
            gpfifo_va: 0x2c000,
            gpfifo_bytes: 0x2000,
            inst: 0x1_f07c_7000,
            userd: 0x209_9000,
            mthdbuf: 0xfff9_8000,
            vaspace: rm::H_VASPACE,
        }
    }

    #[test]
    fn the_channel_allocations_reproduce_nouveaus_rpcs() {
        let a = rm::alloc_request(rm::H_CLIENT, rm::H_DEVICE, h_chan(1), CLASS_GPFIFO, &alloc_params(&gr_chan()));
        assert_eq!(a.len(), 400);
        assert_eq!(a, A_REQ, "the GR channel (non-privileged, chid 1)");
        let c = rm::alloc_request(rm::H_CLIENT, rm::H_DEVICE, h_chan(2), CLASS_GPFIFO, &alloc_params(&ce_chan()));
        assert_eq!(c, CE_REQ, "the CE channel (privileged, chid 2)");
    }

    #[test]
    fn the_replies_add_only_what_rm_decides() {
        // RM fills cid (offset 132) and hPhysChannelGroup (240); the rest echoes
        assert_eq!(A_REP.len(), A_REQ.len());
        let diff: Vec<usize> = (0..A_REQ.len()).filter(|&i| A_REQ[i] != A_REP[i]).collect();
        assert!(diff.iter().all(|&i| {
            let o = i - rm::ALLOC_HDR.min(i);
            i >= rm::ALLOC_HDR && ((132..136).contains(&o) || (240..244).contains(&o))
        }), "{diff:?}");
        assert_eq!(cid_from_reply(A_REP), Some(5));
        assert_eq!(cid_from_reply(A_REQ), Some(0));
        assert_eq!(cid_from_reply(&A_REP[..399]), None);
        assert_eq!(rm::check_alloc_reply(A_REP, rm::H_CLIENT, h_chan(1)), Ok(()));
    }

    #[test]
    fn flags_put_the_userd_slot_where_the_header_says() {
        assert_eq!(chan_flags(1, false), 0x0020_0100);
        assert_eq!(chan_flags(2, true), 0x0020_0220);
        // chid 13 = page 1, index 5
        assert_eq!(chan_flags(13, false), 0x0020_0000 | (1 << 12) | (5 << 8));
        assert_eq!(chan_flags(0, false), 1 << 21);
        assert_eq!(internal_flags(false), 0x14);
        assert_eq!(internal_flags(true), 0x15);
    }

    #[test]
    fn parameter_blocks_have_each_field_at_its_offset() {
        // all-distinct values expose a swapped offset
        let c = ChanAlloc {
            chid: 9,
            privileged: true,
            engine_type: 0x1234,
            gpfifo_va: 0x1111_2222_3333,
            gpfifo_bytes: 0x800,
            inst: 0xa1a1_a1a1_1000,
            userd: 0xb2b2_b2b2_2000,
            mthdbuf: 0xc3c3_c3c3_3000,
            vaspace: 0x5555,
        };
        let p = alloc_params(&c);
        let q = |o: usize| u64::from_le_bytes(p[o..o + 8].try_into().unwrap());
        assert_eq!(p.len(), 368);
        assert_eq!((q(8), get32(&p, 16), get32(&p, 28), get32(&p, 128)), (0x1111_2222_3333, 0x100, 0x5555, 0x1234));
        assert_eq!(get32(&p, 20), chan_flags(9, true));
        assert_eq!((q(144), q(152), get32(&p, 160), get32(&p, 164)), (0xa1a1_a1a1_1000, 0x1000, 2, 1));
        assert_eq!((q(168), q(176), get32(&p, 184), get32(&p, 188)), (0xb2b2_b2b2_2000, 0x200, 2, 1));
        assert_eq!((q(192), q(200), get32(&p, 208), get32(&p, 212)), (0xa1a1_a1a1_1000, 0x200, 2, 1));
        assert_eq!((q(216), q(224), get32(&p, 232), get32(&p, 236)), (0xc3c3_c3c3_3000, 0x5000, 1, 0));
        assert_eq!(get32(&p, 244), 0x15);
        // nothing else is set
        let set: Vec<usize> = (0..p.len() / 4).filter(|&i| get32(&p, i * 4) != 0).map(|i| i * 4).collect();
        assert_eq!(set, [8, 12, 16, 20, 28, 128, 144, 148, 152, 160, 164, 168, 172, 176, 184, 188, 192, 196, 200, 208, 212, 216, 220, 224, 232, 244]);
    }

    #[test]
    fn the_controls_and_the_copy_object_match_the_trace() {
        let ch = h_chan(2);
        assert_eq!(rm::control_request(rm::H_CLIENT, ch, CTRL_BIND, &bind_params(ENGINE_COPY2)), BIND_REQ);
        assert_eq!(rm::control_request(rm::H_CLIENT, ch, CTRL_GPFIFO_SCHEDULE, &schedule_params()), SCHED_REQ);
        assert_eq!(rm::alloc_request(rm::H_CLIENT, ch, H_COPY, CLASS_COPY, &copy_params(ENGINE_COPY2)), CE_ALLOC_REQ);
        assert_eq!(CE_ALLOC_REP, CE_ALLOC_REQ);
        assert_eq!(rm::check_alloc_reply(CE_ALLOC_REP, rm::H_CLIENT, H_COPY), Ok(()));
        assert_eq!((CLASS_GPFIFO, CLASS_COPY, CTRL_BIND, CTRL_GPFIFO_SCHEDULE, CTRL_GET_WORK_SUBMIT_TOKEN), (0xc56f, 0xc7b5, 0xa06f0104, 0xa06f0103, 0xc36f0108));
        assert_eq!((h_chan(2), H_COPY, ENGINE_COPY2), (0xf1f0_0002, 0x4c7b5, 0xb));
        assert_eq!(token_request_params(), [0; 4]);
        assert_eq!(token_from_params(&[0x78, 0x56, 0x34, 0x12]), Some(0x1234_5678));
        assert_eq!(token_from_params(&[1, 2, 3]), None);
    }

    const DEVINFO_REQ: &[u8] = include_bytes!("../fixtures/rm-ph6-devinfo-req.bin");
    const DEVINFO_REP: &[u8] = include_bytes!("../fixtures/rm-ph6-devinfo-rep.bin");

    #[test]
    fn the_device_table_puts_ce2_alone_on_runlist_1() {
        let p = &DEVINFO_REP[rm::CTRL_HDR..];
        assert_eq!((p.len(), DEVICE_INFO_PARAMS_SIZE), (3212, 3212));
        assert_eq!(&DEVINFO_REQ[rm::CTRL_HDR..], &device_info_params()[..], "the request is all zeros");
        assert_eq!(rm::check_control_reply(DEVINFO_REP, 0xc200_0006, 0xabcd_2080, CTRL_FIFO_GET_DEVICE_INFO_TABLE), Ok(p));
        // the table of this board: GR0 1 -> runlist 0, CE0 9 -> 0, CE1 0xa -> 0, CE2 0xb -> 1, CE3 0xc -> 2,
        // CE4 0xd -> 8, NVDEC0 0x1d -> 3, SEC2 0x31 -> 5, NVENC0 0x25 -> 6, OFA 0x3e -> 7
        for (eng, rl) in [(1, 0), (9, 0), (0xa, 0), (0xb, 1), (0xc, 2), (0xd, 8), (0x1d, 3), (0x31, 5), (0x25, 6), (0x3e, 7)] {
            assert_eq!(runlist_for_engine(p, eng), Some(rl), "engine {eng:#x}");
        }
        assert_eq!(runlist_for_engine(p, ENGINE_COPY2), Some(1));
        assert_eq!(runlist_for_engine(p, 0x77), None);
        assert_eq!(runlist_for_engine(&p[..3000], 1), None, "a short reply");
        assert_eq!(runlist_for_engine(&p[..3211], 1), None, "one byte short");
    }

    #[test]
    fn a_table_entry_is_only_read_when_it_is_counted() {
        let mut p = DEVINFO_REP[rm::CTRL_HDR..].to_vec();
        p[4..8].copy_from_slice(&6u32.to_le_bytes()); // six entries: CE2 is the seventh
        assert_eq!(runlist_for_engine(&p, 0xb), None);
        p[4..8].copy_from_slice(&7u32.to_le_bytes());
        assert_eq!(runlist_for_engine(&p, 0xb), Some(1));
        p[4..8].copy_from_slice(&1000u32.to_le_bytes()); // a count past the table is clamped
        assert_eq!(runlist_for_engine(&p, 0xb), Some(1));
        assert_eq!(runlist_for_engine(&p, 0x77), None, "and never read past the 32 entries");
    }

    #[test]
    fn the_doorbell_token_is_runlist_over_channel() {
        assert_eq!(doorbell_token(1, 2), 0x1_0002);
        assert_eq!(doorbell_token(0, 2), 2, "what GSP-RM's own token said");
        assert_eq!(doorbell_token(8, 0x7ff), 0x8_07ff);
        assert_eq!(CTRL_FIFO_GET_DEVICE_INFO_TABLE, 0x2080_1112);
    }

    #[test]
    fn a_gpfifo_entry_is_laid_out_as_the_header_says() {
        // address 0x2_0001_0000, 40 bytes = 10 words
        let e = gp_entry(0x2_0001_0000, 40);
        assert_eq!(e as u32, 0x0001_0000, "word 0: address[31:2] in place, fetch 0");
        assert_eq!((e >> 32) as u32, 2 | (10 << 10), "word 1: address[39:32], level 0, length 10 words, sync 0");
        // low bits and the top of the address and the maximum length
        assert_eq!(gp_entry(0xff_ffff_fffc, 4), 0xff_ffff_fffc & 0xffff_fffc | (0xffu64 | 1 << 10) << 32);
        assert_eq!((gp_entry(0x1000, (1 << 23) - 4) >> 32) as u32 >> 10, (1 << 21) - 1);
        assert_eq!(gp_entry(0x4, 4) as u32, 4);
    }

    #[test]
    #[should_panic]
    fn an_unaligned_gpfifo_address_is_refused() {
        gp_entry(0x1002, 4);
    }

    #[test]
    #[should_panic]
    fn a_gpfifo_address_beyond_40_bits_is_refused() {
        gp_entry(1 << 40, 4);
    }

    #[test]
    #[should_panic]
    fn a_gpfifo_length_that_is_not_whole_words_is_refused() {
        gp_entry(0x1000, 6);
    }

    #[test]
    fn userd_offsets_and_the_channels_constants_are_the_headers() {
        // clc56f.h: GPGet 0x88, GPPut 0x8c (Nvc56fControl_struct)
        assert_eq!((USERD_GP_GET, USERD_GP_PUT), (0x88, 0x8c));
        assert_eq!(SUBCH_COPY, 4);
        // the trace's GPFIFO is 0x2000 bytes = 1024 entries
        assert_eq!((GPFIFO_ENTRIES, GPFIFO_ENTRIES as u64 * 8), (1024, 0x2000));
        assert_eq!(alloc_params(&ce_chan())[16..20], GPFIFO_ENTRIES.to_le_bytes());
    }

    #[test]
    #[should_panic]
    fn a_method_header_with_no_data_is_refused() {
        incr_header(4, 0x400, 0);
    }

    #[test]
    #[should_panic]
    fn a_method_header_with_too_many_words_is_refused() {
        incr_header(4, 0x400, 0x2000);
    }

    #[test]
    #[should_panic]
    fn a_ninth_subchannel_is_refused() {
        incr_header(8, 0x400, 1);
    }

    #[test]
    #[should_panic]
    fn a_method_beyond_the_12_bit_field_is_refused() {
        incr_header(4, 0x4000, 1);
    }

    #[test]
    #[should_panic]
    fn an_unaligned_method_is_refused() {
        incr_header(4, 0x402, 1);
    }

    #[test]
    #[should_panic]
    fn a_gpfifo_entry_too_long_is_refused() {
        gp_entry(0x1000, 1 << 23);
    }

    #[test]
    fn method_headers_are_incrementing_type_one() {
        assert_eq!(incr_header(4, 0x400, 4), (1 << 29) | (4 << 16) | (4 << 13) | 0x100);
        assert_eq!(incr_header(0, 0, 1), 0x2001_0000);
        assert_eq!(incr_header(7, 0x300, 1) >> 13 & 7, 7);
        assert_eq!(incr_header(3, 0x3ffc, 8) & 0xfff, 0xfff);
        // the widest count the field holds (13 bits)
        assert_eq!(incr_header(0, 0, 0x1fff) >> 16 & 0x1fff, 0x1fff);
    }

    /// Decode a push buffer with an independent reader: (method, data) pairs.
    fn decode(w: &[u32]) -> Vec<(u32, u32, u32)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < w.len() {
            let h = w[i];
            assert_eq!(h >> 29, 1, "only incrementing headers");
            let (count, subch, m) = ((h >> 16) & 0x1fff, (h >> 13) & 7, (h & 0xfff) << 2);
            for k in 0..count {
                out.push((subch, m + 4 * k, w[i + 1 + k as usize]));
            }
            i += 1 + count as usize;
        }
        out
    }

    #[test]
    fn the_copy_push_sets_every_method_the_copy_engine_needs() {
        let w = copy_push(0x2_2000_1000, 0x2_1000_2000, 0x40_0000, 0x2_0002_0010, 0xcafe);
        assert_eq!(w.len(), 2 + 5 + 5 + 4 + 2);
        let d = decode(&w);
        let s = SUBCH_COPY;
        assert_eq!(
            d,
            [
                (s, 0x000, 0xc7b5),
                (s, 0x400, 2),
                (s, 0x404, 0x2000_1000),
                (s, 0x408, 2),
                (s, 0x40c, 0x1000_2000),
                (s, 0x410, 0x40_0000),
                (s, 0x414, 0x40_0000),
                (s, 0x418, 0x40_0000),
                (s, 0x41c, 1),
                (s, 0x240, 2),
                (s, 0x244, 0x0002_0010),
                (s, 0x248, 0xcafe),
                (s, 0x300, 0x18e),
            ]
        );
        // LAUNCH_DMA by field: NON_PIPELINED, flush, one-word semaphore, pitch both sides, virtual both
        let l = LAUNCH_DMA_COPY_WITH_SEMAPHORE;
        assert_eq!((l & 3, l >> 2 & 1, l >> 3 & 3, l >> 5 & 3, l >> 7 & 1, l >> 8 & 1, l >> 9 & 1, l >> 12 & 3), (2, 1, 1, 0, 1, 1, 0, 0));
        assert_eq!(l, 0x18e);
    }

    #[test]
    fn the_bare_release_is_the_copy_push_without_the_transfer() {
        let w = release_push(0x2_0002_0010, 0x77);
        let s = SUBCH_COPY;
        assert_eq!(decode(&w), [(s, 0x000, 0xc7b5), (s, 0x240, 2), (s, 0x244, 0x0002_0010), (s, 0x248, 0x77), (s, 0x300, 0xc)]);
        assert_eq!(LAUNCH_DMA_SEMAPHORE_ONLY, 0xc);
        // the same release words as the copy's
        let c = copy_push(0x1000, 0x2000, 16, 0x2_0002_0010, 0x77);
        assert_eq!(&c[c.len() - 6..c.len() - 2], &w[w.len() - 6..w.len() - 2][..4]);
    }

    #[test]
    fn a_rectangle_copy_is_a_pitch_linear_multi_line_copy() {
        let w = copy_rect_push(0x2_2000_0400, 0x2_1000_0800, 8192, 8192, 1600, 300, 0x2_0002_0010, 0x77);
        let s = SUBCH_COPY;
        assert_eq!(
            decode(&w),
            [
                (s, 0x000, 0xc7b5),
                (s, 0x400, 0x2),
                (s, 0x404, 0x2000_0400),
                (s, 0x408, 0x2),
                (s, 0x40c, 0x1000_0800),
                (s, 0x410, 8192),
                (s, 0x414, 8192),
                (s, 0x418, 1600),
                (s, 0x41c, 300),
                (s, 0x240, 2),
                (s, 0x244, 0x0002_0010),
                (s, 0x248, 0x77),
                (s, 0x300, LAUNCH_DMA_COPY_WITH_SEMAPHORE | 0x200),
            ]
        );
        assert_eq!(LAUNCH_DMA_MULTI_LINE, 0x200);
        // different pitches land in the right words
        let w = copy_rect_push(0x1000, 0x2000, 4096, 8192, 100, 2, 0x3000, 1);
        let d = decode(&w);
        assert_eq!((d[5], d[6], d[7], d[8]), ((s, 0x410, 4096), (s, 0x414, 8192), (s, 0x418, 100), (s, 0x41c, 2)));
        // one line: identical to the linear copy, no MULTI_LINE
        assert_eq!(copy_rect_push(0x1000, 0x2000, 64, 64, 64, 1, 0x3000, 5), copy_push(0x1000, 0x2000, 64, 0x3000, 5));
        assert_eq!(decode(&copy_push(0x1000, 0x2000, 64, 0x3000, 5)).last().unwrap().2, LAUNCH_DMA_COPY_WITH_SEMAPHORE);
    }

    #[test]
    #[should_panic]
    fn a_line_longer_than_the_pitch_is_refused() {
        copy_rect_push(0x1000, 0x2000, 100, 8192, 101, 2, 0x3000, 1);
    }

    #[test]
    fn an_interrupt_is_one_more_launch_bit() {
        assert_eq!(LAUNCH_DMA_INTERRUPT_NON_BLOCKING, 0x40);
        let plain = copy_push(0x1000, 0x2000, 64, 0x3000, 5);
        let irq = with_interrupt(plain.clone());
        assert_eq!(irq.len(), plain.len());
        assert_eq!(&irq[..irq.len() - 1], &plain[..plain.len() - 1], "only the launch word changes");
        assert_eq!(irq[irq.len() - 1], LAUNCH_DMA_COPY_WITH_SEMAPHORE | 0x40);
        assert_eq!(with_interrupt(release_push(0x3000, 1)).last(), Some(&(LAUNCH_DMA_SEMAPHORE_ONLY | 0x40)));
        // together with the multi-line bit of a rectangle
        let r = with_interrupt(copy_rect_push(0x1000, 0x2000, 256, 256, 64, 4, 0x3000, 5));
        assert_eq!(r[r.len() - 1], LAUNCH_DMA_COPY_WITH_SEMAPHORE | LAUNCH_DMA_MULTI_LINE | 0x40);
    }

    #[test]
    #[should_panic]
    fn an_interrupt_needs_a_launch_to_ride_on() {
        with_interrupt(vec![incr_header(SUBCH_COPY, 0, 1), CLASS_COPY]);
    }

    #[test]
    #[should_panic]
    fn a_rectangle_needs_at_least_one_line() {
        copy_rect_push(0x1000, 0x2000, 256, 256, 64, 0, 0x3000, 1);
    }

    #[test]
    fn a_1080p_frame_at_pitch_8192_fits_the_frame_buffer() {
        assert!(FRAME_BYTES >= 8192 * 1080);
        assert!(FRAME_BYTES.is_power_of_two(), "a DmaBuf is a power-of-two block");
    }

    #[test]
    fn the_layout_does_not_overlap() {
        // VRAM: the channel's pages, then the destination; the tables (64 MiB) and
        // the 6b test mapping (96 MiB) are below
        assert!(INST_VRAM + INST_SIZE <= USERD_VRAM);
        assert!(USERD_VRAM + 0x1000 <= GPFIFO_VRAM);
        assert!(GPFIFO_VRAM + (GPFIFO_ENTRIES as u64) * 8 <= PUSH_VRAM);
        assert!(PUSH_VRAM + 0x1000 <= FENCE_VRAM);
        assert!(FENCE_VRAM + 0x1000 <= VRAM_CHAN + 0x10000);
        assert!(VRAM_CHAN + 0x10000 <= VRAM_DST && VRAM_CHAN > (96 << 20) + (1 << 20));
        // VA: the small buffers, then the three big ones 256 MiB apart
        assert!(GPFIFO_VA + 0x2000 <= PUSH_VA && PUSH_VA + 0x1000 <= FENCE_VA);
        assert!(FENCE_VA + 0x1000 <= DST_VA && DST_VA + (256 << 20) <= SRC_VA && SRC_VA + (256 << 20) <= BACK_VA);
        for va in [GPFIFO_VA, PUSH_VA, FENCE_VA, DST_VA, SRC_VA, BACK_VA] {
            assert!(va % 0x1000 == 0 && va >> 49 == 0);
        }
        // 6d: the host fence page after the VRAM one, and the 2 MiB-page buffers
        // (2 MiB aligned, 256 MiB apart, above the 4 KiB-page ones)
        assert!(FENCE_VA + 0x1000 <= HFENCE_VA && HFENCE_VA + 0x1000 <= DST_VA);
        assert!(BACK_VA + (256 << 20) <= HUGE_DST_VA);
        assert!(HUGE_DST_VA + (256 << 20) <= HUGE_SRC_VA && HUGE_SRC_VA + (256 << 20) <= HUGE_BACK_VA);
        for va in [HUGE_DST_VA, HUGE_SRC_VA, HUGE_BACK_VA, FRAME_SRC_VA, FRAME_DST_VA] {
            assert!(va % (2 << 20) == 0 && va >> 49 == 0);
        }
        assert!(HUGE_BACK_VA + (256 << 20) <= FRAME_SRC_VA && FRAME_SRC_VA + (256 << 20) <= FRAME_DST_VA);
        // the frame's VRAM: after the 4 MiB destination, 2 MiB aligned, below the GSP heap
        assert!(VRAM_DST + (4 << 20) <= FRAME_VRAM && FRAME_VRAM % (2 << 20) == 0 && FRAME_VRAM + FRAME_BYTES < (1 << 32));
    }

    #[test]
    fn the_copy_channels_kernel_pushes() {
        let r = release_push(0x2_0003_0000, 0x55);
        assert_eq!(r.len() as u32 * 4, RELEASE_PUSH_BYTES);
        assert_eq!(bind_push().len() as u32 * 4, BIND_PUSH_BYTES);
        // the bind is the release's own first two words: SET_OBJECT (method 0) on the copy subchannel with the copy class
        assert_eq!(bind_push(), r[..2]);
        assert_eq!(bind_push(), [incr_header(4, 0, 1), 0xc7b5]);
        // the release: SET_SEMAPHORE_A/B/C, then LAUNCH_DMA with NONE + FLUSH + RELEASE_ONE_WORD
        assert_eq!(r[2..], [incr_header(4, 0x240, 3), 2, 0x0003_0000, 0x55, incr_header(4, 0x300, 1), 0xc]);
    }
}
