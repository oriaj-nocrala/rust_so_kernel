//! Graphics/compute context and the compute class (phase 7a of `docs/gpu/gpu-plan.md`).
//!
//! With GSP-RM the host owns the memory of a GR channel's context: RM says how big each buffer is
//! (`GET_CONTEXT_BUFFERS_INFO`), the host allocates and maps them in the channel's VA space and
//! tells RM where they are (`PROMOTE_CTX`). The first channel is a *golden* one: its promote gives
//! every buffer, and allocating a 3D object on it makes RM build the golden context image
//! (`r535_gr_oneinit`, `nvkm/subdev/gsp/rm/r535/gr.c:250-315`). Every later GR channel promotes only
//! its own (non-global) buffers and refers to the golden run's global ones
//! (`r535_gr_chan_new`, `gr.c:143-170`).
//!
//! Pure: sizes in, parameter blocks and push buffers out. The oracle for the golden sequence is the
//! trace (`fixtures/rm-ph7-gr-*`, RPCs #21-#25 of `trace-gsp`); the non-golden promote and the
//! compute pushes have no trace and rest on nouveau's source and NVIDIA's class headers.

use alloc::vec::Vec;

use crate::chan::incr_header;

/// `AMPERE_B` (3D) and `AMPERE_COMPUTE_B` for GA10x (`nvkm/engine/gr/ga102.c:182-183`,
/// `nvif/class.h:205,275`); the golden context is triggered with the 3D one (`gr.c:312`).
pub const CLASS_THREED: u32 = 0xc797;
pub const CLASS_COMPUTE: u32 = 0xc7c0;

/// `RM_ENGINE_TYPE_GR0` (`rm/r535/nvrm/engine.h:126`).
pub const ENGINE_GR0: u32 = 1;

/// The handle nouveau gives the golden 3D object (`NVKM_RM_THREED`, seen in the trace as
/// `obj=0x97000000`), and ours for the compute object.
pub const H_THREED: u32 = 0x9700_0000;
pub const H_COMPUTE: u32 = 0xc7c0_0000;

/// `NV2080_CTRL_CMD_INTERNAL_STATIC_KGR_GET_CONTEXT_BUFFERS_INFO` (`rm/r570/nvrm/gr.h:11`): 8
/// engines x 26 buffers (r570's `ENGINE_ID_COUNT` is 0x1a; r535's was 0x19) x (size, alignment)
/// = 1664 bytes, as the trace's 0x680.
pub const CTRL_GET_CONTEXT_BUFFERS_INFO: u32 = 0x2080_0a32;
const BUFFERS_PER_ENGINE: usize = 26;
pub const CONTEXT_BUFFERS_INFO_SIZE: usize = 8 * BUFFERS_PER_ENGINE * 8;
/// `NV2080_CTRL_CMD_GPU_PROMOTE_CTX` (`rm/r535/nvrm/fifo.h:327`): a 48-byte header and 16 entries
/// of 32 bytes.
pub const CTRL_PROMOTE_CTX: u32 = 0x2080_012b;
pub const PROMOTE_PARAMS_SIZE: usize = 560;
const PROMOTE_MAX_ENTRIES: usize = 16;
const PROMOTE_HEADER: usize = 48;
const PROMOTE_ENTRY: usize = 32;

/// `NV2080_CTRL_GPU_PROMOTE_CTX_BUFFER_ID_*` (`gr.h:58-70`).
pub mod buffer_id {
    pub const MAIN: u16 = 0;
    pub const PATCH: u16 = 2;
    pub const BUNDLE_CB: u16 = 3;
    pub const PAGEPOOL: u16 = 4;
    pub const ATTRIBUTE_CB: u16 = 5;
    pub const RTV_CB_GLOBAL: u16 = 6;
    pub const FECS_EVENT: u16 = 9;
    pub const PRIV_ACCESS_MAP: u16 = 10;
    pub const UNRESTRICTED_PRIV_ACCESS_MAP: u16 = 11;
}

/// One context buffer as `r535_gr_get_ctxbuf_info` derives it (`gr.c:175-243`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CtxBuf {
    pub id: u16,
    pub size: u32,
    /// log2 of the page size nouveau maps it with (12, 16 or 21).
    pub page: u8,
    /// log2 of the alignment of its virtual address.
    pub align: u8,
    /// Shared by every channel (allocated once, with the golden one).
    pub global: bool,
    /// Zero-filled at allocation and initialised by RM through its physical address.
    pub init: bool,
    /// Mapped read-only.
    pub ro: bool,
}

/// `NV0080_CTRL_FIFO_GET_ENGINE_CONTEXT_PROPERTIES_ENGINE_ID_*` (`gr.h:36-56`) -> buffer, and the
/// `global`/`init`/`ro` columns of nouveau's table (`gr.c:181-194`).
const TABLE: [(usize, u16, bool, bool, bool); 8] = [
    (0, buffer_id::MAIN, false, true, false),
    (13, buffer_id::PAGEPOOL, true, false, false),
    (16, buffer_id::PATCH, false, true, false),
    (17, buffer_id::BUNDLE_CB, true, false, false),
    (19, buffer_id::ATTRIBUTE_CB, true, false, false),
    (20, buffer_id::RTV_CB_GLOBAL, true, false, false),
    (23, buffer_id::FECS_EVENT, true, true, false),
    (24, buffer_id::PRIV_ACCESS_MAP, true, true, true),
];

/// `order_base_2`: the smallest `n` with `2^n >= x`.
fn order_base_2(x: u32) -> u8 {
    if x <= 1 {
        0
    } else {
        (32 - (x - 1).leading_zeros()) as u8
    }
}

/// The reply of `GET_CONTEXT_BUFFERS_INFO` -> the buffers a GR channel needs, in nouveau's order (the
/// engine's buffer index, ascending; `UNRESTRICTED_PRIV_ACCESS_MAP` follows `PRIV_ACCESS_MAP`).
/// Only the first engine's block is read (`engineContextBuffersInfo[0]`).
pub fn ctx_buffers(params: &[u8]) -> Option<Vec<CtxBuf>> {
    if params.len() < CONTEXT_BUFFERS_INFO_SIZE {
        return None;
    }
    let mut out: Vec<CtxBuf> = Vec::new();
    for i in 0..BUFFERS_PER_ENGINE {
        let Some(&(_, id, global, init, ro)) = TABLE.iter().find(|t| t.0 == i) else { continue };
        let mut size = u32::from_le_bytes(params[i * 8..i * 8 + 4].try_into().unwrap());
        if id == buffer_id::MAIN {
            // per-subcontext headers
            size = size.checked_add(0xfff)? & !0xfff;
            size = size.checked_add(64 * 0x1000)?;
        }
        let page = if size >= 1 << 21 {
            21
        } else if size >= 1 << 16 {
            16
        } else {
            12
        };
        let align = if id == buffer_id::ATTRIBUTE_CB { order_base_2(size) } else { page };
        let b = CtxBuf { id, size, page, align, global, init, ro };
        out.push(b);
        if id == buffer_id::PRIV_ACCESS_MAP {
            out.push(CtxBuf { id: buffer_id::UNRESTRICTED_PRIV_ACCESS_MAP, ..b });
        }
    }
    Some(out)
}

/// The parameter block of `GET_CONTEXT_BUFFERS_INFO` (out only).
pub fn ctx_buffers_request() -> Vec<u8> {
    alloc::vec![0u8; CONTEXT_BUFFERS_INFO_SIZE]
}

/// Where the adapter put one context buffer: its VRAM address and its address in the channel's VA
/// space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mem {
    pub pa: u64,
    pub va: u64,
}

/// One `NV2080_CTRL_GPU_PROMOTE_CTX_BUFFER_ENTRY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub id: u16,
    pub pa: u64,
    pub va: u64,
    pub size: u64,
    pub phys_attr: u32,
    pub init: bool,
    pub nonmapped: bool,
}

/// `physAttr` nouveau sends for an initialised buffer (`gr.c:126`, seen as 4 in the trace).
const PHYS_ATTR: u32 = 4;

/// Which of `bufs` `PROMOTE_CTX` names and how (`r535_gr_promote_ctx`, `gr.c:60-141`). A golden
/// promote gives every buffer its own memory; a channel's gives its own (non-global) ones and refers
/// to the golden run's for the global ones. `mem[i]` is where buffer `i` is (for a channel, the
/// global ones' golden memory). The entries come in the buffers' order.
pub fn entries(bufs: &[CtxBuf], golden: bool, mem: &[Mem]) -> Vec<Entry> {
    assert_eq!(bufs.len(), mem.len());
    let mut out = Vec::new();
    for (b, m) in bufs.iter().zip(mem) {
        let alloc = golden || !b.global;
        if !alloc && b.id == buffer_id::UNRESTRICTED_PRIV_ACCESS_MAP {
            continue;
        }
        let nonmapped = alloc && b.id == buffer_id::PRIV_ACCESS_MAP;
        let init = b.init && alloc;
        let mut e = Entry { id: b.id, pa: 0, va: 0, size: 0, phys_attr: 0, init, nonmapped };
        if !nonmapped {
            e.va = m.va;
        }
        if init {
            e.pa = m.pa;
            e.size = b.size as u64;
            e.phys_attr = PHYS_ATTR;
        }
        out.push(e);
    }
    out
}

/// Whether the buffer needs its own memory in this promote (`alloc` in nouveau).
pub fn allocates(b: &CtxBuf, golden: bool) -> bool {
    golden || !b.global
}

/// Whether the adapter must map the buffer in the VA space for this promote (`!bNonmapped`, and not
/// skipped).
pub fn maps(b: &CtxBuf, golden: bool) -> bool {
    !(allocates(b, golden) && b.id == buffer_id::PRIV_ACCESS_MAP) && !(!allocates(b, golden) && b.id == buffer_id::UNRESTRICTED_PRIV_ACCESS_MAP)
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// `NV2080_CTRL_GPU_PROMOTE_CTX_PARAMS` (560 bytes) for `entries` on the channel `chan` of `client`.
pub fn promote_params(client: u32, chan: u32, entries: &[Entry]) -> Vec<u8> {
    assert!(entries.len() <= PROMOTE_MAX_ENTRIES);
    let mut p = alloc::vec![0u8; PROMOTE_PARAMS_SIZE];
    put32(&mut p, 0, ENGINE_GR0); // engineType
    put32(&mut p, 12, client); // hChanClient (hClient stays 0)
    put32(&mut p, 16, chan); // hObject
    put32(&mut p, 40, entries.len() as u32);
    for (i, e) in entries.iter().enumerate() {
        let at = PROMOTE_HEADER + i * PROMOTE_ENTRY;
        put64(&mut p, at, e.pa);
        put64(&mut p, at + 8, e.va);
        put64(&mut p, at + 16, e.size);
        put32(&mut p, at + 24, e.phys_attr);
        p[at + 28..at + 30].copy_from_slice(&e.id.to_le_bytes());
        p[at + 30] = e.init as u8;
        p[at + 31] = e.nonmapped as u8;
    }
    p
}


// ---- the layout the adapter uses ------------------------------------------------------------

/// VRAM `[0x1f1000000, +128 KiB)` (7.77 GiB, in the region just below the GSP's heap where nouveau's instance memory was
/// in the trace; boots #126-#132 had it at 256 MiB): the golden channel's block (instance `+0`, USERD `+0x1000`) and the GR
/// channel's block (`+0x10000`: instance, USERD, GPFIFO, push buffer, a destination page).
/// Everything else `gpu=copy` keeps is below 176 MiB (`chan::FRAME_VRAM + FRAME_BYTES`), the page
/// tables are at 64 MiB, and RM's own memory is at the top of VRAM.
pub const VRAM_GOLDEN: u64 = 0x1_f100_0000;
pub const VRAM_CHAN: u64 = VRAM_GOLDEN + 0x1_0000;
pub const CHAN_INST: u64 = VRAM_CHAN;
pub const CHAN_USERD: u64 = VRAM_CHAN + 0x1000;
pub const CHAN_GPFIFO: u64 = VRAM_CHAN + 0x2000; // 8 KiB
pub const CHAN_PUSH: u64 = VRAM_CHAN + 0x4000; // 4 KiB
pub const CHAN_DST: u64 = VRAM_CHAN + 0x5000; // 4 KiB, the VRAM destination of the last rung
pub const GOLDEN_INST: u64 = VRAM_GOLDEN;
pub const GOLDEN_USERD: u64 = VRAM_GOLDEN + 0x1000;
pub const GPFIFO_ENTRIES: u32 = 1024;
/// Where the context buffers start (2 MiB aligned) and their VAs: VRAM 7.75 GiB, the region just below the GSP's heap
/// (`0x1f4000000`) where nouveau's own were in the trace (`0x1efb00000..0x1f07b8000`); boots #126-#131 had them at 260 MiB.
pub const VRAM_CTX: u64 = 0x1_f000_0000;
pub const VA_CTX: u64 = 0x3_0000_0000;
/// The channel's own VAs: GPFIFO (8 KiB), push buffer, the VRAM destination page, and the host page
/// (`HOST_PAGES` pages: the data target, then the semaphores).
pub const VA_CHAN: u64 = 0x3_8000_0000;
pub const GPFIFO_VA: u64 = VA_CHAN;
pub const PUSH_VA: u64 = VA_CHAN + 0x1_0000;
pub const VDST_VA: u64 = VA_CHAN + 0x2_0000;
pub const HOST_VA: u64 = VA_CHAN + 0x3_0000;
pub const HOST_PAGES: u64 = 2;
/// Inside the host buffer: the data the inline writes target (page 0) and the semaphores (page 1).
pub const HOST_DATA_OFF: u64 = 0;
pub const HOST_SEM_OFF: u64 = 0x1000;

/// One mapping of a context buffer: `len` bytes at `va` onto VRAM `pa`, with pages of `2^page` bytes (12, 16 or 21:
/// nouveau maps each buffer with the page size its size and alignment allow, `gr.c:112-119`). Always privileged
/// (`gf100_vmm_map_v0.priv = 1`, `gr.c:106`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mapping {
    pub id: u16,
    pub va: u64,
    pub pa: u64,
    pub len: u64,
    pub page: u8,
    pub ro: bool,
}

/// Where every context buffer goes: the golden set (all of them) and the GR channel's (MAIN and PATCH
/// again, the rest the golden's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub golden: Vec<Mem>,
    pub chan: Vec<Mem>,
    pub vram_end: u64,
    pub va_end: u64,
}

const HUGE: u64 = 2 << 20;

fn round_up(x: u64, to: u64) -> u64 {
    (x + to - 1) & !(to - 1)
}

/// The length a buffer occupies: whole pages of its own size (4 KiB, 64 KiB or 2 MiB).
pub fn mapped_len(b: &CtxBuf) -> u64 {
    round_up(b.size as u64, 1 << b.page)
}

/// Lay the buffers out one after another from [`VRAM_CTX`] and [`VA_CTX`]. Physical addresses are
/// aligned to 64 KiB (2 MiB for a huge page), virtual ones to the buffer's own alignment (16 MiB for
/// ATTRIBUTE_CB).
pub fn plan(bufs: &[CtxBuf]) -> Plan {
    let (mut pa, mut va) = (VRAM_CTX, VA_CTX);
    let mut place = |b: &CtxBuf| {
        let pa_align = if b.page >= 21 { HUGE } else { 0x1_0000 };
        let len = mapped_len(b);
        pa = round_up(pa, pa_align);
        va = round_up(va, (1u64 << b.align).max(pa_align));
        let m = Mem { pa, va };
        pa += len;
        va += len;
        m
    };
    let golden: Vec<Mem> = bufs.iter().map(&mut place).collect();
    // the channel's own: the non-global ones again, at fresh addresses; the rest are the golden's
    let chan: Vec<Mem> = bufs.iter().zip(&golden).map(|(b, g)| if b.global { *g } else { place(b) }).collect();
    Plan { golden, chan, vram_end: pa, va_end: va }
}

/// Every mapping the two promotes need, each once.
pub fn mappings(bufs: &[CtxBuf], plan: &Plan) -> Vec<Mapping> {
    let mut out = Vec::new();
    for (i, b) in bufs.iter().enumerate() {
        let m = |mem: Mem| Mapping { id: b.id, va: mem.va, pa: mem.pa, len: mapped_len(b), page: b.page, ro: b.ro };
        // the golden set maps everything but the nonmapped PRIV_ACCESS_MAP, which a channel maps
        // read-only at the golden's address, so it is mapped once for both
        out.push(m(plan.golden[i]));
        if !b.global {
            out.push(m(plan.chan[i]));
        }
    }
    out
}

/// `promote_params` with the two fields nouveau leaves 0 and OpenRM's own callers set
/// (`kernel_falcon.c:213-218`): `hClient` (offset 4) and the channel's hardware id (`ChID`, offset 8,
/// "deprecated").
pub fn with_client_and_chid(mut p: Vec<u8>, client: u32, chid: u32) -> Vec<u8> {
    put32(&mut p, 4, client);
    put32(&mut p, 8, chid);
    p
}


// ---- nouveau's golden VA space (RM-managed) ------------------------------------------------

/// The handle of the golden channel's own VA space object (nouveau gives it `NVKM_RM_VASPACE`, in its own
/// client; here it shares ours, so a second handle).
pub const H_VASPACE_GOLDEN: u32 = 0x90f1_0001;

/// `NV_VASPACE_ALLOCATION_PARAMETERS` of a VA space RM manages (`r535_mmu_vaspace_new(.., external =
/// false)`, `vmm.c:57-62`): `index = GPU_NEW`, no flags (the externally owned one sets bit 3).
pub fn golden_vaspace_params() -> Vec<u8> {
    alloc::vec![0u8; crate::rm::VASPACE_PARAMS_SIZE]
}

/// `NV90F1_CTRL_CMD_VASPACE_COPY_SERVER_RESERVED_PDES` (`vmm.h:33`).
pub const CTRL_COPY_SERVER_RESERVED_PDES: u32 = 0x90f1_0106;
pub const COPY_PDES_PARAMS_SIZE: usize = 184;
/// The server-reserved range (`SPLIT_VAS_SERVER_RM_MANAGED_VA_START/SIZE`, `vmm.h:28-29`): 512 MiB at 4 GiB.
const RSVD_VA: u64 = 0x1_0000_0000;
const RSVD_SIZE: u64 = 0x2000_0000;

/// The three page-directory levels nouveau's golden VMM hands RM (`vmm.c:107-138`: the levels above a 512 MiB
/// page, root first): (size in bytes, page shift) — a 32-byte root, then two 4 KiB tables. `tables` are the
/// VRAM addresses of the three (zeroed) instances.
const PDES_LEVELS: [(u64, u8); 3] = [(0x20, 0x2f), (0x1000, 0x26), (0x1000, 0x1d)];

pub fn copy_server_reserved_pdes_params(tables: [u64; 3]) -> Vec<u8> {
    let mut p = alloc::vec![0u8; COPY_PDES_PARAMS_SIZE];
    put64(&mut p, 8, RSVD_SIZE); // pageSize
    put64(&mut p, 16, RSVD_VA); // virtAddrLo
    put64(&mut p, 24, RSVD_VA + RSVD_SIZE - 1); // virtAddrHi
    put32(&mut p, 32, PDES_LEVELS.len() as u32); // numLevelsToCopy
    for (i, (t, (size, shift))) in tables.iter().zip(PDES_LEVELS).enumerate() {
        let at = 40 + i * 24;
        put64(&mut p, at, *t);
        put64(&mut p, at + 8, size);
        put32(&mut p, at + 16, 1); // aperture
        p[at + 20] = shift;
    }
    p
}

/// The VAs nouveau's golden VMM gave each buffer (trace RPC #24), by buffer id; the nonmapped
/// PRIV_ACCESS_MAP has none.
pub const TRACE_VAS: [(u16, u64); 9] = [
    (buffer_id::MAIN, 0x1_0000),
    (buffer_id::PAGEPOOL, 0xe_0000),
    (buffer_id::PATCH, 0x1000),
    (buffer_id::BUNDLE_CB, 0x5000),
    (buffer_id::ATTRIBUTE_CB, 0x100_0000),
    (buffer_id::RTV_CB_GLOBAL, 0x10_0000),
    (buffer_id::FECS_EVENT, 0x18_0000),
    (buffer_id::PRIV_ACCESS_MAP, 0),
    (buffer_id::UNRESTRICTED_PRIV_ACCESS_MAP, 0x19_0000),
];

/// The golden promote as nouveau lays it out in its own golden VMM: every buffer at the plan's VRAM `mem[i].pa`
/// and at the trace's VA. `None` when a buffer has no VA in the trace.
pub fn trace_placement(bufs: &[CtxBuf], mem: &[Mem]) -> Option<Vec<Mem>> {
    bufs.iter()
        .zip(mem)
        .map(|(b, m)| TRACE_VAS.iter().find(|t| t.0 == b.id).map(|t| Mem { pa: m.pa, va: t.1 }))
        .collect()
}

/// The mappings of a golden promote at `mem`: every buffer the golden promote maps (all but the nonmapped
/// PRIV_ACCESS_MAP), with its own page size (`gr.c:100-119`).
pub fn golden_mappings(bufs: &[CtxBuf], mem: &[Mem]) -> Vec<Mapping> {
    bufs.iter()
        .zip(mem)
        .filter(|(b, _)| maps(b, true))
        .map(|(b, m)| Mapping { id: b.id, va: m.va, pa: m.pa, len: mapped_len(b), page: b.page, ro: b.ro })
        .collect()
}

/// The page tables of the golden VA space (RM-managed, over tables of ours: nouveau's `grGoldenVmm`): 8 tables in
/// the golden block after nouveau's `0x12000`-byte layout would put the method buffer (ours is in system memory):
/// root, PD2, PD1, PD0, the 4 KiB table and two 64 KiB tables of the first 4 MiB (ATTRIBUTE_CB takes 2 MiB PTEs).
pub const GOLDEN_TABLES: u64 = VRAM_GOLDEN + 0x8000;
pub const GOLDEN_TABLES_MAX: usize = 8;

// ---- the compute class -----------------------------------------------------------------

/// The subchannel the compute object is bound to.
pub const SUBCH_COMPUTE: u32 = 1;

// `clc7c0.h` (same offsets as `clc6c0.h`): methods of `AMPERE_COMPUTE_B`.
const C_LINE_LENGTH_IN: u32 = 0x180;
const C_LAUNCH_DMA: u32 = 0x1b0;
const C_SET_I2M_SEMAPHORE_A: u32 = 0x1dc;
const C_SET_REPORT_SEMAPHORE_A: u32 = 0x1b00;

/// `NVC56F_DMA_ONE_INC` (`clc56f.h:36`): the first data word goes to `method`, the rest to the
/// next method, non-incrementing. Mesa's inline-data uploads use it (`nvk_cmd_dispatch.c:446`).
pub fn one_inc_header(subch: u32, method: u32, count: u32) -> u32 {
    assert!(subch < 8 && method % 4 == 0 && method < 0x4000 && count > 0 && count < 0x2000);
    (5 << 29) | (count << 16) | (subch << 13) | (method >> 2)
}

/// `NVC6C0_LAUNCH_DMA`: DST_MEMORY_LAYOUT PITCH (bit 0), COMPLETION_TYPE (5:4).
const LAUNCH_DMA_PITCH: u32 = 1;
const LAUNCH_DMA_FLUSH_ONLY: u32 = 1 << 4;
const LAUNCH_DMA_RELEASE_SEMAPHORE: u32 = 2 << 4;
/// `SEMAPHORE_STRUCT_SIZE` ONE_WORD (bit 12).
const LAUNCH_DMA_ONE_WORD: u32 = 1 << 12;

/// The start of every push buffer: bind the compute object to the subchannel.
fn set_object() -> [u32; 2] {
    [incr_header(SUBCH_COMPUTE, 0, 1), CLASS_COMPUTE]
}

/// `SET_REPORT_SEMAPHORE_D`: OPERATION RELEASE (0), STRUCTURE_SIZE ONE_WORD (bit 28).
const REPORT_D_RELEASE_ONE_WORD: u32 = 1 << 28;

/// Rung 1: a bare semaphore release by the GR pipeline (`SET_REPORT_SEMAPHORE_A..D`): `payload`
/// at `sem_va` once the class's work is done. No shader, no memory but the semaphore's.
pub fn report_semaphore_push(sem_va: u64, payload: u32) -> Vec<u32> {
    let mut w = Vec::new();
    w.extend(set_object());
    report_semaphore(&mut w, sem_va, payload);
    w
}

fn report_semaphore(w: &mut Vec<u32>, sem_va: u64, payload: u32) {
    w.push(incr_header(SUBCH_COMPUTE, C_SET_REPORT_SEMAPHORE_A, 4));
    w.extend([(sem_va >> 32) as u32, sem_va as u32, payload, REPORT_D_RELEASE_ONE_WORD]);
}

/// Rungs 2 and 3: `data` written to `dst_va` by the inline-to-memory path (`LINE_LENGTH_IN`,
/// `LINE_COUNT`, `OFFSET_OUT`, `LAUNCH_DMA` + the data, as `nvk_cmd_compute_indirect_copy` does), then
/// `payload` released at `sem_va` when the data has landed (`SET_I2M_SEMAPHORE_A..C` and
/// COMPLETION_TYPE RELEASE_SEMAPHORE). `data.len()` is in words.
/// The most words one inline write carries so that its push (with the object bind and the semaphore) fits the
/// channel's 4 KiB push buffer; bigger writes go in several pushes (boot #135 panicked on a 4 KiB one).
pub const INLINE_CHUNK_WORDS: usize = 256;
pub const PUSH_BYTES: usize = 0x1000;

pub fn inline_write_push(dst_va: u64, data: &[u32], sem_va: u64, payload: u32) -> Vec<u32> {
    assert!(!data.is_empty() && data.len() < 0x1000);
    let mut w = Vec::new();
    w.extend(set_object());
    // the semaphore first: LAUNCH_DMA's completion uses it
    w.push(incr_header(SUBCH_COMPUTE, C_SET_I2M_SEMAPHORE_A, 3));
    w.extend([(sem_va >> 32) as u32, sem_va as u32, payload]);
    // LINE_LENGTH_IN, LINE_COUNT, OFFSET_OUT_UPPER, OFFSET_OUT
    w.push(incr_header(SUBCH_COMPUTE, C_LINE_LENGTH_IN, 4));
    w.extend([(data.len() * 4) as u32, 1, (dst_va >> 32) as u32, dst_va as u32]);
    // LAUNCH_DMA, then LOAD_INLINE_DATA x n
    w.push(one_inc_header(SUBCH_COMPUTE, C_LAUNCH_DMA, 1 + data.len() as u32));
    w.push(LAUNCH_DMA_PITCH | LAUNCH_DMA_RELEASE_SEMAPHORE | LAUNCH_DMA_ONE_WORD);
    w.extend_from_slice(data);
    w
}

// ---- a compute launch (phase 7b) ---------------------------------------------------------------

/// The host pages a launch uses, at `KERN_VA` (mapped read/write, system memory): the shader, the QMD, constant buffer 0, the
/// output page and the semaphores. All 256-byte aligned (`SEND_PCAS_A` takes the QMD address `>> 8`).
pub const KERN_VA: u64 = VA_CHAN + 0x4_0000;
pub const KERN_PAGES: u64 = 3;
pub const KERN_SHADER_OFF: u64 = 0;
pub const KERN_QMD_OFF: u64 = 0x400;
pub const KERN_CB0_OFF: u64 = 0x800;
/// 4 KiB the shader writes to (in host memory).
pub const KERN_OUT_OFF: u64 = 0x1000;
/// The class's own semaphore (`SET_REPORT_SEMAPHORE`, after `WAIT_FOR_IDLE`) at `+0`, the grid's (`RELEASE0` of the QMD) at `+0x10`.
pub const KERN_SEM_OFF: u64 = 0x2000;
pub const KERN_SEM_GRID: u64 = 0x10;

// `clc7c0.h`: methods of the launch.
const C_WAIT_FOR_IDLE: u32 = 0x110;
const C_INVALIDATE_SKED_CACHES: u32 = 0x298;
const C_SET_SHADER_SHARED_MEMORY_WINDOW_A: u32 = 0x2a0;
const C_SEND_PCAS_A: u32 = 0x2b4;
const C_SEND_SIGNALING_PCAS2_B: u32 = 0x2c0;
const C_SET_SHADER_LOCAL_MEMORY_WINDOW_A: u32 = 0x7b0;
/// `NVC7C0_SEND_SIGNALING_PCAS2_B_PCAS_ACTION_INVALIDATE_COPY_SCHEDULE`.
const PCAS_INVALIDATE_COPY_SCHEDULE: u32 = 3;
/// Where the shader's shared and local memory windows sit in the address space (`hw_runner.rs`, `PushBuilder::new`).
pub const SHARED_WINDOW: u64 = 0xfe00_0000;
pub const LOCAL_WINDOW: u64 = 0xff00_0000;

/// Launch the QMD at `qmd_va` and wait for the engine to go idle: what `nak/hw_runner.rs` pushes (`SET_OBJECT`, the two
/// memory windows, `INVALIDATE_SKED_CACHES`, `SEND_PCAS_A` with the QMD address, `SEND_SIGNALING_PCAS2_B` with
/// INVALIDATE_COPY_SCHEDULE), then `WAIT_FOR_IDLE` and a semaphore release of `payload` at `sem_va`. Nouveau's own fences do the
/// same wait before releasing (the release must not overtake the grid); the QMD's `RELEASE0` is the grid's own signal.
pub fn dispatch_push(qmd_va: u64, sem_va: u64, payload: u32) -> Vec<u32> {
    assert!(qmd_va % crate::qmd::QMD_ALIGN == 0 && qmd_va >> 40 == 0, "QMD address {:#x}", qmd_va);
    let mut w = Vec::new();
    w.extend(set_object());
    w.push(incr_header(SUBCH_COMPUTE, C_SET_SHADER_SHARED_MEMORY_WINDOW_A, 2));
    w.extend([(SHARED_WINDOW >> 32) as u32, SHARED_WINDOW as u32]);
    w.push(incr_header(SUBCH_COMPUTE, C_SET_SHADER_LOCAL_MEMORY_WINDOW_A, 2));
    w.extend([(LOCAL_WINDOW >> 32) as u32, LOCAL_WINDOW as u32]);
    w.push(incr_header(SUBCH_COMPUTE, C_INVALIDATE_SKED_CACHES, 1));
    w.push(0);
    w.push(incr_header(SUBCH_COMPUTE, C_SEND_PCAS_A, 1));
    w.push((qmd_va >> 8) as u32);
    w.push(incr_header(SUBCH_COMPUTE, C_SEND_SIGNALING_PCAS2_B, 1));
    w.push(PCAS_INVALIDATE_COPY_SCHEDULE);
    w.push(incr_header(SUBCH_COMPUTE, C_WAIT_FOR_IDLE, 1));
    w.push(0);
    report_semaphore(&mut w, sem_va, payload);
    w
}

/// The fence the kernel appends to every `EXEC` (G4c, `hwq::Queue`): bind the compute object, wait for the GR engine to go idle
/// (what a user push's stores must have finished before the release) and release `payload` at `sem_va` (host memory).
pub fn fence_push(sem_va: u64, payload: u32) -> Vec<u32> {
    let mut w = Vec::new();
    w.extend(set_object());
    w.push(incr_header(SUBCH_COMPUTE, C_WAIT_FOR_IDLE, 1));
    w.push(0);
    report_semaphore(&mut w, sem_va, payload);
    w
}

/// The size of [`fence_push`] in bytes.
pub const FENCE_PUSH_BYTES: u32 = 36;

// `clc56f.h`: the channel's memory operations, host methods (below 0x100, executed by the PBDMA, whatever the subchannel).
const H_MEM_OP_A: u32 = 0x28;
const MEM_OP_D_OPERATION_SHIFT: u32 = 27;
/// `NVC56F_MEM_OP_D_OPERATION_L2_FLUSH_DIRTY`.
const MEM_OP_L2_FLUSH_DIRTY: u32 = 0x10;
/// `NVC56F_MEM_OP_C_MEMBAR_TYPE_SYS_MEMBAR`.
const MEM_OP_C_SYS_MEMBAR: u32 = 0;

/// Write back the L2's dirty lines from inside the channel (`MEM_OP_A..D`, D last: "MEM_OP_D MUST be preceded by MEM_OPs
/// A-C"), then release `payload` at `sem_va`. The host method waits for the engine's earlier work before it runs.
pub fn l2_flush_push(sem_va: u64, payload: u32) -> Vec<u32> {
    let mut w = Vec::new();
    w.extend(set_object());
    w.push(incr_header(0, H_MEM_OP_A, 4));
    w.extend([0, 0, MEM_OP_C_SYS_MEMBAR, MEM_OP_L2_FLUSH_DIRTY << MEM_OP_D_OPERATION_SHIFT]);
    report_semaphore(&mut w, sem_va, payload);
    w
}

/// The same write without a semaphore (completion FLUSH_ONLY): what NVK's own uploads use, kept for
/// the test that pins the field values.
pub fn inline_write_flush_only(dst_va: u64, data: &[u32]) -> Vec<u32> {
    let mut w = Vec::new();
    w.extend(set_object());
    w.push(incr_header(SUBCH_COMPUTE, C_LINE_LENGTH_IN, 4));
    w.extend([(data.len() * 4) as u32, 1, (dst_va >> 32) as u32, dst_va as u32]);
    w.push(one_inc_header(SUBCH_COMPUTE, C_LAUNCH_DMA, 1 + data.len() as u32));
    w.push(LAUNCH_DMA_PITCH | LAUNCH_DMA_FLUSH_ONLY);
    w.extend_from_slice(data);
    w
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chan::{self, ChanAlloc};
    use crate::rm;

    const INFO_REP: &[u8] = include_bytes!("../fixtures/rm-ph7-gr-ctxbufs-rep.bin");
    const INFO_REQ: &[u8] = include_bytes!("../fixtures/rm-ph7-gr-ctxbufs-req.bin");
    const GOLDEN_CHAN_REQ: &[u8] = include_bytes!("../fixtures/rm-ph7-gr-golden-chan-req.bin");
    const PROMOTE_REQ: &[u8] = include_bytes!("../fixtures/rm-ph7-gr-promote-golden-req.bin");
    const THREED_REQ: &[u8] = include_bytes!("../fixtures/rm-ph7-gr-alloc-c797-req.bin");

    fn info() -> Vec<CtxBuf> {
        // the reply's params after the 24-byte control header
        ctx_buffers(&INFO_REP[rm::CTRL_HDR..]).unwrap()
    }

    fn cb(id: u16, size: u32, page: u8, align: u8, global: bool, init: bool, ro: bool) -> CtxBuf {
        CtxBuf { id, size, page, align, global, init, ro }
    }

    #[test]
    fn the_context_buffers_of_this_gpu() {
        // sizes from RM's reply in the trace; MAIN grows by the per-subcontext headers (0x86300 ->
        // 0x87000 + 0x40000), ATTRIBUTE_CB is aligned to 2^24 (0x851200), 2 MiB pages from 2 MiB up
        let want = [
            cb(0, 0xc7000, 16, 16, false, true, false),
            cb(4, 0x20000, 16, 16, true, false, false),
            cb(2, 0x4000, 12, 12, false, true, false),
            cb(3, 0x3000, 12, 12, true, false, false),
            cb(5, 0x851200, 21, 24, true, false, false),
            cb(6, 0x80000, 16, 16, true, false, false),
            cb(9, 0x10000, 16, 16, true, true, false),
            cb(10, 0x80000, 16, 16, true, true, true),
            cb(11, 0x80000, 16, 16, true, true, true),
        ];
        assert_eq!(info(), want);
        assert_eq!(ctx_buffers(&INFO_REP[rm::CTRL_HDR..rm::CTRL_HDR + 1663]), None);
        assert_eq!(CONTEXT_BUFFERS_INFO_SIZE, 1664);
        assert_eq!(rm::control_request(0xc2000006, 0xabcd2080, CTRL_GET_CONTEXT_BUFFERS_INFO, &ctx_buffers_request()), INFO_REQ);
    }

    #[test]
    fn page_sizes_change_exactly_at_64_kib_and_2_mib() {
        // a reply whose PAGEPOOL (index 13) is `size` bytes; the others zero
        let with = |size: u32| {
            let mut p = vec![0u8; CONTEXT_BUFFERS_INFO_SIZE];
            p[13 * 8..13 * 8 + 4].copy_from_slice(&size.to_le_bytes());
            let all = ctx_buffers(&p).unwrap();
            let b = all.iter().find(|b| b.id == buffer_id::PAGEPOOL).unwrap();
            (b.page, b.align)
        };
        assert_eq!(with(0xffff), (12, 12));
        assert_eq!(with(0x1_0000), (16, 16));
        assert_eq!(with((2 << 20) - 1), (16, 16));
        assert_eq!(with(2 << 20), (21, 21));
        assert_eq!(with((2 << 20) + 1), (21, 21));
    }

    #[test]
    fn the_channel_block_keeps_its_pieces_apart() {
        assert!(CHAN_INST + chan::INST_SIZE <= CHAN_USERD, "instance block, then USERD");
        assert!(CHAN_USERD + chan::USERD_SIZE <= CHAN_GPFIFO, "USERD, then the GPFIFO");
        assert!(CHAN_GPFIFO + GPFIFO_ENTRIES as u64 * 8 <= CHAN_PUSH);
        assert!(CHAN_PUSH + 0x1000 <= CHAN_DST && CHAN_DST + 0x1000 <= VRAM_CHAN + 0x1_0000);
        assert!(GOLDEN_INST + chan::INST_SIZE <= GOLDEN_USERD && GOLDEN_USERD + chan::USERD_SIZE <= VRAM_CHAN);
        // the USERD page the channel names is the one whose GP_PUT the adapter writes
        assert_eq!((CHAN_USERD - CHAN_INST, GOLDEN_USERD - GOLDEN_INST), (0x1000, 0x1000));
    }

    #[test]
    fn the_context_math_helpers() {
        assert_eq!([order_base_2(0), order_base_2(1), order_base_2(2), order_base_2(3), order_base_2(0x1000), order_base_2(0x1001)], [0, 0, 1, 2, 12, 13]);
        assert_eq!(order_base_2(0x851200), 24);
        assert_eq!(order_base_2(1 << 31), 31);
    }

    /// nouveau's placement in the trace: every entry's `pa`/`va` (only the initialised ones have a `pa`).
    fn trace_mem() -> Vec<Mem> {
        let p = &PROMOTE_REQ[rm::CTRL_HDR..];
        (0..9)
            .map(|i| {
                let at = PROMOTE_HEADER + i * PROMOTE_ENTRY;
                Mem { pa: u64::from_le_bytes(p[at..at + 8].try_into().unwrap()), va: u64::from_le_bytes(p[at + 8..at + 16].try_into().unwrap()) }
            })
            .collect()
    }

    #[test]
    fn the_golden_promote_reproduces_nouveaus_rpc() {
        // The trace's nine entries come in the buffers' order. The two the trace does not place
        // (PRIV_ACCESS_MAP nonmapped: va 0; and the globals that are not initialised: pa 0) come out
        // of the rules, not of the numbers given here.
        let bufs = info();
        let e = entries(&bufs, true, &trace_mem());
        assert_eq!(e.len(), 9);
        let p = promote_params(rm::H_CLIENT, 0, &e);
        let mut want = PROMOTE_REQ[rm::CTRL_HDR..].to_vec();
        // the trace's client and channel handle
        assert_eq!(&want[12..20], &[0x01, 0x00, 0xd0, 0xc1, 0x00, 0x00, 0xf0, 0xf1]);
        put32(&mut want, 12, rm::H_CLIENT);
        put32(&mut want, 16, 0);
        assert_eq!(p.len(), PROMOTE_PARAMS_SIZE);
        // a line by line comparison: never assert_eq! on a 560-byte block
        for i in 0..p.len() {
            assert_eq!(p[i], want[i], "byte {i} of the promote block");
        }
        let full = rm::control_request(0xc1d00001, rm::H_SUBDEVICE, CTRL_PROMOTE_CTX, &promote_params(0xc1d00001, 0xf1f00000, &e));
        assert_eq!(full.len(), PROMOTE_REQ.len());
        for i in 0..full.len() {
            assert_eq!(full[i], PROMOTE_REQ[i], "byte {i} of the whole RPC");
        }
    }

    #[test]
    fn a_channels_promote_names_its_own_buffers_and_refers_to_the_golden_globals() {
        let bufs = info();
        let mem: Vec<Mem> = (0..bufs.len()).map(|i| Mem { pa: 0x1000_0000 + 0x100_0000 * i as u64, va: 0x3_0000_0000 + 0x1000_0000 * i as u64 }).collect();
        let e = entries(&bufs, false, &mem);
        // UNRESTRICTED_PRIV_ACCESS_MAP is skipped, PRIV_ACCESS_MAP is mapped now
        assert_eq!(e.iter().map(|e| e.id).collect::<Vec<_>>(), [0, 4, 2, 3, 5, 6, 9, 10]);
        for (e, b) in e.iter().zip(bufs.iter().filter(|b| b.id != 11)) {
            let i = bufs.iter().position(|x| x.id == b.id).unwrap();
            assert_eq!(e.va, mem[i].va, "buffer {}: always mapped", b.id);
            assert!(!e.nonmapped);
            if !b.global {
                // MAIN and PATCH: this channel's own memory, initialised
                assert_eq!((e.init, e.pa, e.size as u32, e.phys_attr), (true, mem[i].pa, b.size, 4), "buffer {}", b.id);
            } else {
                assert_eq!((e.init, e.pa, e.size, e.phys_attr), (false, 0, 0, 0), "buffer {}: global, not initialised again", b.id);
            }
        }
    }

    #[test]
    fn what_each_promote_allocates_and_maps() {
        let bufs = info();
        let g: Vec<(bool, bool)> = bufs.iter().map(|b| (allocates(b, true), maps(b, true))).collect();
        // golden: everything allocated; only the nonmapped PRIV_ACCESS_MAP is not mapped
        assert_eq!(g, [(true, true), (true, true), (true, true), (true, true), (true, true), (true, true), (true, true), (true, false), (true, true)]);
        let c: Vec<(bool, bool)> = bufs.iter().map(|b| (allocates(b, false), maps(b, false))).collect();
        // channel: MAIN and PATCH new; the globals are the golden's; UNRESTRICTED is skipped
        assert_eq!(c, [(true, true), (false, true), (true, true), (false, true), (false, true), (false, true), (false, true), (false, true), (false, false)]);
    }


    #[test]
    fn the_plan_has_aligned_disjoint_buffers() {
        let bufs = info();
        let pl = plan(&bufs);
        let maps = mappings(&bufs, &pl);
        // MAIN and PATCH twice, the rest once
        assert_eq!(maps.len(), 9 + 2);
        for m in &maps {
            let b = bufs.iter().find(|b| b.id == m.id).unwrap();
            let al = if m.page >= 21 { HUGE } else { 0x1_0000 };
            assert_eq!(m.pa % al, 0, "buffer {} pa", m.id);
            assert_eq!(m.va % (1u64 << b.align).max(al), 0, "buffer {} va", m.id);
            assert_eq!(m.len, mapped_len(b));
            assert!(m.len >= b.size as u64);
            assert_eq!(m.page, b.page);
            assert_eq!(m.ro, b.id == buffer_id::PRIV_ACCESS_MAP || b.id == buffer_id::UNRESTRICTED_PRIV_ACCESS_MAP);
        }
        for (i, a) in maps.iter().enumerate() {
            for b in &maps[i + 1..] {
                assert!(a.pa + a.len <= b.pa || b.pa + b.len <= a.pa, "VRAM of {} and {}", a.id, b.id);
                assert!(a.va + a.len <= b.va || b.va + b.len <= a.va, "VA of {} and {}", a.id, b.id);
            }
            assert!(a.pa >= VRAM_CTX && a.pa + a.len <= pl.vram_end);
            assert!(a.va >= VA_CTX && a.va + a.len <= pl.va_end);
        }
        // the channel's promote refers to the golden's globals and its own MAIN/PATCH
        for (i, b) in bufs.iter().enumerate() {
            assert_eq!(pl.chan[i] == pl.golden[i], b.global, "buffer {}", b.id);
        }
        // the huge ATTRIBUTE_CB is 2 MiB-aligned in VRAM and 16 MiB in VA
        let a = bufs.iter().position(|b| b.id == buffer_id::ATTRIBUTE_CB).unwrap();
        assert_eq!((pl.golden[a].pa % HUGE, pl.golden[a].va % (16 << 20)), (0, 0));
        assert_eq!(mapped_len(&bufs[a]), 5 * HUGE);
        // and below the GSP's heap (0x1f4000000), where the WPR2 and RM's own memory are
        assert!(pl.vram_end <= 0x1_f400_0000, "{:#x}", pl.vram_end);
    }

    #[test]
    fn the_blocks_do_not_overlap_the_other_users_of_vram_and_va() {
        // pool of page tables 64..96 MiB, test mapping at 96 MiB (1 MiB), CE block 128 MiB (+64 KiB),
        // CE destination 144 MiB (+4 MiB), the 6d frame buffer 160..176 MiB
        assert!(VRAM_GOLDEN >= 176 << 20);
        assert!(VRAM_GOLDEN + 0x2_0000 <= 0x1_f400_0000, "below the GSP's heap");
        assert_eq!(VRAM_CHAN, VRAM_GOLDEN + 0x1_0000);
        assert!(CHAN_DST + 0x1000 <= VRAM_GOLDEN + 0x2_0000);
        assert!(CHAN_GPFIFO + GPFIFO_ENTRIES as u64 * 8 <= CHAN_PUSH);
        assert!(plan(&info()).vram_end <= VRAM_GOLDEN, "the context buffers end below the channel blocks");
        // VAs: `gpu=copy` uses 0x2_0000_0000..0x2_a000_0000; the context buffers and the channel sit above
        assert!(VA_CTX >= 0x2_a000_0000);
        let pl = plan(&info());
        assert!(pl.va_end <= VA_CHAN, "{:#x}", pl.va_end);
        assert!(GPFIFO_VA + GPFIFO_ENTRIES as u64 * 8 <= PUSH_VA && PUSH_VA + 0x1000 <= VDST_VA && VDST_VA + 0x1000 <= HOST_VA);
        assert_eq!(HOST_SEM_OFF, 0x1000);
    }

    #[test]
    fn hclient_and_chid_sit_at_offsets_4_and_8() {
        let p = with_client_and_chid(promote_params(7, 8, &[]), 0xaaaa, 0xbbbb);
        assert_eq!((get(&p, 0), get(&p, 4), get(&p, 8), get(&p, 12), get(&p, 16)), (1, 0xaaaa, 0xbbbb, 7, 8));
    }

    #[test]
    fn nouveaus_golden_vaspace_calls_match_the_trace() {
        const V2: &[u8] = include_bytes!("../fixtures/rm-ph6-vaspace2-req.bin");
        const PD: &[u8] = include_bytes!("../fixtures/rm-ph6-vaspace-ctrl-90f10106-req.bin");
        // the golden client's own VA space: RM-managed (flags 0)
        assert_eq!(rm::alloc_request(0xc1d00001, rm::H_DEVICE, 0x90f1_0000, rm::FERMI_VASPACE_A, &golden_vaspace_params()), V2);
        // the three tables the trace named
        let p = copy_server_reserved_pdes_params([0x1_f07b_5000, 0x1_f07b_4000, 0x1_f07b_3000]);
        assert_eq!(p.len(), COPY_PDES_PARAMS_SIZE);
        let want = rm::control_request(0xc1d00001, 0x90f1_0000, CTRL_COPY_SERVER_RESERVED_PDES, &p);
        assert_eq!(want.len(), PD.len());
        for i in 0..PD.len() {
            assert_eq!(want[i], PD[i], "byte {i}");
        }
        assert_eq!((H_VASPACE_GOLDEN, CTRL_COPY_SERVER_RESERVED_PDES), (0x90f10001, 0x90f10106));
    }

    #[test]
    fn the_trace_vas_are_what_the_trace_promoted() {
        let t = trace_mem();
        for (i, (id, va)) in TRACE_VAS.iter().enumerate() {
            assert_eq!(t[i].va, *va, "buffer {id}");
        }
        assert_eq!(TRACE_VAS.map(|x| x.0), [0, 4, 2, 3, 5, 6, 9, 10, 11]);
    }

    #[test]
    fn the_golden_va_space_maps_every_buffer_at_the_trace_va_and_leaves_rms_window_free() {
        use crate::mmu::{Flags, PageTables, Target};
        let bufs = info();
        let pl = plan(&bufs);
        let at = trace_placement(&bufs, &pl.golden).unwrap();
        assert_eq!(at.iter().map(|m| m.va).collect::<Vec<_>>(), TRACE_VAS.map(|t| t.1));
        assert!(at.iter().zip(&pl.golden).all(|(a, g)| a.pa == g.pa), "VRAM stays the plan's");
        assert_eq!(trace_placement(&[cb(1, 0x1000, 12, 12, false, true, false)], &[Mem::default()]), None, "PM has no trace VA");
        // the promote names exactly those VAs (and PRIV_ACCESS_MAP nonmapped)
        let e = entries(&bufs, true, &at);
        for (x, t) in e.iter().zip(TRACE_VAS) {
            assert_eq!((x.id, x.va), t);
        }
        let maps = golden_mappings(&bufs, &at);
        assert_eq!(maps.len(), 8, "all but the nonmapped PRIV_ACCESS_MAP");
        assert!(maps.iter().all(|m| m.id != buffer_id::PRIV_ACCESS_MAP));
        for (i, a) in maps.iter().enumerate() {
            for b in &maps[i + 1..] {
                assert!(a.va + a.len <= b.va || b.va + b.len <= a.va, "VA of {} and {}", a.id, b.id);
            }
        }
        // the tables, built as the adapter builds them, fit the pool and translate every page
        let mut pt = PageTables::new(GOLDEN_TABLES, GOLDEN_TABLES_MAX, Target::Vram);
        for m in &maps {
            let f = Flags { privileged: true, read_only: m.ro, kind: 0 };
            match m.page {
                21 => pt.map_huge_range(m.va, m.pa, m.len, Target::Vram, f).unwrap(),
                16 => pt.map_big_range(m.va, m.pa, m.len, Target::Vram, f).unwrap(),
                _ => pt.map_range(m.va, m.pa, m.len, Target::Vram, f).unwrap(),
            }
        }
        assert!(pt.len() <= GOLDEN_TABLES_MAX, "{} tables", pt.len());
        for m in &maps {
            for off in (0..m.len).step_by(0x1000) {
                let (pa, pte) = pt.translate(m.va + off).unwrap();
                assert_eq!(pa, m.pa + off, "buffer {} at +{off:#x}", m.id);
                assert_eq!(pte & (1 << 5), 1 << 5, "privileged");
                assert_eq!(pte & (1 << 6) != 0, m.ro);
            }
        }
        // the levels RM is told about are this tree's (root, then the tables under slot 0), and the server-reserved
        // window at 4 GiB (PD1 slot 8) is left for RM to fill
        let d = pt.directories(0).unwrap();
        assert_eq!(d, [GOLDEN_TABLES, GOLDEN_TABLES + 0x1000, GOLDEN_TABLES + 0x2000]);
        assert_eq!(pt.directories(RSVD_VA), Some(d));
        let pd1 = pt.images().find(|(a, _)| *a == d[2]).unwrap().1;
        assert_eq!(u64::from_le_bytes(pd1[8 * 8..9 * 8].try_into().unwrap()), 0);
        assert!(pt.translate(RSVD_VA).is_none() && pt.translate(RSVD_VA + RSVD_SIZE - 0x1000).is_none());
        // and the pool sits in the golden block, after the instance block and USERD, before the GR channel's block
        assert!(GOLDEN_TABLES >= GOLDEN_USERD + 0x1000 && GOLDEN_TABLES + (GOLDEN_TABLES_MAX * 0x1000) as u64 <= VRAM_CHAN);
    }

    #[test]
    fn entries_with_no_buffers_and_the_limit() {
        assert!(entries(&[], true, &[]).is_empty());
        let p = promote_params(1, 2, &[]);
        assert_eq!((get(&p, 0), get(&p, 12), get(&p, 16), get(&p, 40)), (1, 1, 2, 0));
        assert_eq!(p.len(), 560);
    }

    fn get(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn the_golden_channel_and_the_3d_object_match_the_trace() {
        // the golden channel: chid 1 (nouveau's `rsvd_chids`), privileged, GR0, a shell GPFIFO (offset 0,
        // 512 entries) that is never scheduled; the addresses are the trace's
        let p = &GOLDEN_CHAN_REQ[rm::ALLOC_HDR..];
        let q = |o: usize| u64::from_le_bytes(p[o..o + 8].try_into().unwrap());
        let c = ChanAlloc {
            chid: 1,
            privileged: true,
            engine_type: ENGINE_GR0,
            gpfifo_va: 0,
            gpfifo_bytes: 0x1000,
            inst: q(144),
            userd: q(168),
            mthdbuf: q(216),
            vaspace: 0x90f1_0000,
        };
        assert_eq!((c.inst, c.userd), (0x1_f07b_6000, 0x1_f07b_7000));
        let a = rm::alloc_request(0xc1d00001, rm::H_DEVICE, chan::h_chan(0), chan::CLASS_GPFIFO, &chan::alloc_params(&c));
        assert_eq!(a.len(), GOLDEN_CHAN_REQ.len());
        for i in 0..a.len() {
            assert_eq!(a[i], GOLDEN_CHAN_REQ[i], "byte {i} of the golden channel's ALLOC");
        }
        // the 3D object: no parameters, under the channel
        assert_eq!(rm::alloc_request(0xc1d00001, 0xf1f00000, H_THREED, CLASS_THREED, &[]), THREED_REQ);
    }

    #[test]
    fn class_ids_and_engine() {
        assert_eq!((CLASS_THREED, CLASS_COMPUTE, ENGINE_GR0), (0xc797, 0xc7c0, 1));
        assert_eq!((CTRL_GET_CONTEXT_BUFFERS_INFO, CTRL_PROMOTE_CTX), (0x20800a32, 0x2080012b));
    }

    // ---- the compute class ----

    #[test]
    fn one_inc_is_opcode_5() {
        // `NVC56F_DMA_SEC_OP_ONE_INC` = 5 in 31:29; count 28:16, subchannel 15:13, method >> 2
        assert_eq!(one_inc_header(1, 0x1b0, 3), 0xa003_206c);
        assert_eq!(one_inc_header(7, 0x3ffc, 0x1fff), (5 << 29) | (0x1fff << 16) | (7 << 13) | 0xfff);
    }

    #[test]
    fn a_report_semaphore_release() {
        let w = report_semaphore_push(0x2_0003_0000, 7);
        assert_eq!(w, [incr_header(1, 0, 1), 0xc7c0, incr_header(1, 0x1b00, 4), 2, 0x0003_0000, 7, 1 << 28]);
        assert_eq!(incr_header(1, 0x1b00, 4), 0x2004_26c0);
    }

    #[test]
    fn an_inline_write() {
        let w = inline_write_push(0x2_5000_1000, &[0x11, 0x22, 0x33], 0x2_0003_0000, 9);
        assert_eq!(w[..2], [incr_header(1, 0, 1), 0xc7c0]);
        // SET_I2M_SEMAPHORE_A/B/C
        assert_eq!(w[2..6], [incr_header(1, 0x1dc, 3), 2, 0x0003_0000, 9]);
        // LINE_LENGTH_IN (bytes), LINE_COUNT, OFFSET_OUT_UPPER, OFFSET_OUT
        assert_eq!(w[6..11], [incr_header(1, 0x180, 4), 12, 1, 2, 0x5000_1000]);
        // LAUNCH_DMA: pitch layout, release a one-word semaphore; then the data
        assert_eq!(w[11], one_inc_header(1, 0x1b0, 4));
        assert_eq!(w[12], 1 | (2 << 4) | (1 << 12));
        assert_eq!(w[13..], [0x11, 0x22, 0x33]);
        // a chunk fits the push buffer; one word more than a whole 4 KiB page of data does not
        assert!(inline_write_push(0, &[0; INLINE_CHUNK_WORDS], 0, 1).len() * 4 <= PUSH_BYTES);
        assert!(inline_write_push(0, &[0; 1024], 0, 1).len() * 4 > PUSH_BYTES);
        // NVK's own upload only flushes
        let f = inline_write_flush_only(0x1000, &[5]);
        assert_eq!(f[f.len() - 2], 1 | (1 << 4));
    }

    #[test]
    fn a_compute_launch() {
        let w = dispatch_push(0x3_8004_0400, 0x3_8004_2000, 0x5555);
        // the method numbers of clc7c0.h, spelled out, and the values NAK's PushBuilder gives them
        let want = [
            incr_header(1, 0x0000, 1), 0xc7c0, // SET_OBJECT
            incr_header(1, 0x02a0, 2), 0, 0xfe00_0000, // SET_SHADER_SHARED_MEMORY_WINDOW_A/B
            incr_header(1, 0x07b0, 2), 0, 0xff00_0000, // SET_SHADER_LOCAL_MEMORY_WINDOW_A/B
            incr_header(1, 0x0298, 1), 0, // INVALIDATE_SKED_CACHES
            incr_header(1, 0x02b4, 1), 0x0380_0404, // SEND_PCAS_A: 0x3_8004_0400 >> 8
            incr_header(1, 0x02c0, 1), 3, // SEND_SIGNALING_PCAS2_B: INVALIDATE_COPY_SCHEDULE
            incr_header(1, 0x0110, 1), 0, // WAIT_FOR_IDLE
            incr_header(1, 0x1b00, 4), 3, 0x8004_2000, 0x5555, 1 << 28, // the release, as rung 1
        ];
        assert_eq!(w, want);
        assert_eq!(incr_header(1, 0x02b4, 1), 0x2001_20ad); // opcode 1, count 1, subchannel 1, method >> 2 = 0xad
        assert!(w.len() * 4 <= PUSH_BYTES);
        // the addresses of the launch's pages: 256-byte aligned, inside their pages, disjoint
        let (sh, q, cb, out, sem) = (KERN_SHADER_OFF, KERN_QMD_OFF, KERN_CB0_OFF, KERN_OUT_OFF, KERN_SEM_OFF);
        for a in [sh, q, cb, out, sem] {
            assert_eq!(a % 256, 0);
        }
        assert!(sh + crate::qmd::FILL.len() as u64 <= q);
        assert!(q + crate::qmd::QMD_BYTES as u64 <= cb);
        assert!(cb + crate::qmd::FILL_CBUF0_BYTES as u64 <= out);
        assert!(out + 0x1000 <= sem);
        assert_eq!(out % 0x1000, 0, "the output is a page: the shader's 1024 words never cross into another mapping");
        assert!(sem + KERN_SEM_GRID + 4 <= KERN_PAGES * 0x1000);
        assert_eq!(KERN_VA, 0x3_8004_0000);
        // and clear of the other host pages of the channel
        assert!(KERN_VA >= HOST_VA + HOST_PAGES * 0x1000);
    }

    #[test]
    fn an_l2_flush_from_the_channel() {
        let w = l2_flush_push(0x3_8004_2000, 0x77);
        assert_eq!(w[..2], [incr_header(1, 0, 1), 0xc7c0]);
        // MEM_OP_A (0x28) .. MEM_OP_D (0x34) in one increasing run, subchannel 0; D = operation 0x10 in bits 31:27
        assert_eq!(w[2..7], [incr_header(0, 0x28, 4), 0, 0, 0, 0x8000_0000]);
        assert_eq!(incr_header(0, 0x28, 4), 0x2004_000a);
        assert_eq!(w[7..], [incr_header(1, 0x1b00, 4), 3, 0x8004_2000, 0x77, 1 << 28]);
        assert!(w.len() * 4 <= PUSH_BYTES);
    }

    #[test]
    fn the_kernels_fence_waits_for_idle_then_releases() {
        let w = fence_push(0x3_8000_1000, 0xabcd);
        assert_eq!(w.len() as u32 * 4, FENCE_PUSH_BYTES);
        assert_eq!(w[..2], [incr_header(1, 0, 1), 0xc7c0]);
        // WAIT_FOR_IDLE (0x110), then the release: the order is the whole point, a release first would overtake the shader's stores
        assert_eq!(w[2..4], [incr_header(1, 0x110, 1), 0]);
        assert_eq!(w[4..], [incr_header(1, 0x1b00, 4), 3, 0x8000_1000, 0xabcd, 1 << 28]);
        assert!(FENCE_PUSH_BYTES as usize <= 64, "a fence slot is 64 bytes");
    }

    #[test]
    #[should_panic(expected = "QMD address")]
    fn a_qmd_must_be_256_byte_aligned() {
        dispatch_push(0x3_8004_0080, 0, 0);
    }

    #[test]
    #[should_panic(expected = "QMD address")]
    fn a_qmd_must_be_within_40_bits() {
        dispatch_push(1 << 40, 0, 0);
    }
}
