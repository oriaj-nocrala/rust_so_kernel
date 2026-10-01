//! RM objects over the RPC channel (phase 4g): the messages `GSP_RM_ALLOC`,
//! `GSP_RM_CONTROL` and `FREE` and the parameter blocks of the first objects a
//! client creates, `NV01_ROOT` -> `NV01_DEVICE_0` -> `NV20_SUBDEVICE_0`. Pure:
//! bytes in, bytes out; [`crate::rpc::Queues`] carries them.
//!
//! Layouts: `nvkm/subdev/gsp/rm/r535/nvrm/{alloc,ctrl,client,device}.h` and
//! `r570/nvrm/client.h` (the 570.144 `NV0000_ALLOC_PARAMETERS` has a trailing
//! pointer, 120 bytes); construction: `rm/r535/{alloc,ctrl,client,device}.c`
//! and `r570/client.c` in Linux v7.2.2. Oracle: the RPCs nouveau sent and
//! the replies it got in `trace-gsp` at 10,109-10,110 s
//! (`fixtures/rm-*-{req,rep}.bin`).

use alloc::vec;
use alloc::vec::Vec;

// ---- function numbers (`r570/nvrm/rpcfn.h`) --------------------------------

pub const FN_FREE: u32 = 10;
pub const FN_GSP_RM_CONTROL: u32 = 76;
pub const FN_GSP_RM_ALLOC: u32 = 103;

// ---- classes and handles ------------------------------------------------------

pub const NV01_ROOT: u32 = 0x0;
pub const NV01_DEVICE_0: u32 = 0x80;
pub const NV20_SUBDEVICE_0: u32 = 0x2080;
/// `FERMI_VASPACE_A` (`nvif/class.h`): a GPU virtual address space.
pub const FERMI_VASPACE_A: u32 = 0x90f1;

/// The handles nouveau uses (`NVKM_RM_*`, `rm/handles.h`; the client's is
/// `0xc1d00000 | id` with id 0). Any values work as long as they are distinct.
pub const H_CLIENT: u32 = 0xc1d0_0000;
pub const H_DEVICE: u32 = 0xde1d_0000;
pub const H_SUBDEVICE: u32 = 0x5d1d_0000;
/// `NVKM_RM_VASPACE` (`rm/handles.h`): the VA space's object handle. The trace
/// (`fixtures/rm-vaspace-req.bin`) uses it under `H_DEVICE`.
pub const H_VASPACE: u32 = 0x90f1_0000;

/// `NV2080_CTRL_CMD_GPU_GET_NAME_STRING` and its parameter block
/// (`ctrl2080gpu.h:291-311`): flags (0 = ASCII) then 64 ASCII bytes or 64
/// UTF-16 units.
pub const CTRL_GPU_GET_NAME_STRING: u32 = 0x2080_0110;
pub const NAME_STRING_PARAMS_SIZE: usize = 4 + 128;

/// `NV0080_CTRL_CMD_DMA_SET_PAGE_DIRECTORY` / `..._UNSET_PAGE_DIRECTORY`
/// (`r535/nvrm/vmm.h:96`), sent to the device object.
pub const CTRL_DMA_SET_PAGE_DIRECTORY: u32 = 0x0080_1813;
pub const CTRL_DMA_SET_PAGE_DIRECTORY_SIZE: usize = 32;

/// `sizeof(rpc_gsp_rm_alloc_v03_00)` and of `rpc_gsp_rm_control_v03_00`.
pub const ALLOC_HDR: usize = 32;
pub const CTRL_HDR: usize = 24;

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn get32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// `r535_gsp_rpc_rm_alloc_get` (`alloc.c:76-100`): hClient, hParent, hObject,
/// hClass, status 0, paramsSize, flags 0, 4 reserved bytes, then the params.
pub fn alloc_request(client: u32, parent: u32, object: u32, class: u32, params: &[u8]) -> Vec<u8> {
    let mut m = vec![0u8; ALLOC_HDR + params.len()];
    put32(&mut m, 0, client);
    put32(&mut m, 4, parent);
    put32(&mut m, 8, object);
    put32(&mut m, 12, class);
    put32(&mut m, 20, params.len() as u32);
    m[ALLOC_HDR..].copy_from_slice(params);
    m
}

/// `r535_gsp_rpc_rm_ctrl_get` (`ctrl.c:62-84`): hClient, hObject, cmd, status
/// 0, paramsSize, flags 0, then the params.
pub fn control_request(client: u32, object: u32, cmd: u32, params: &[u8]) -> Vec<u8> {
    let mut m = vec![0u8; CTRL_HDR + params.len()];
    put32(&mut m, 0, client);
    put32(&mut m, 4, object);
    put32(&mut m, 8, cmd);
    put32(&mut m, 16, params.len() as u32);
    m[CTRL_HDR..].copy_from_slice(params);
    m
}

/// `r535_gsp_rpc_rm_free` (`alloc.c:27-44`): `rpc_free_v03_00` = hRoot,
/// hObjectParent (0), hObjectOld, status.
pub fn free_request(client: u32, object: u32) -> Vec<u8> {
    let mut m = vec![0u8; 16];
    put32(&mut m, 0, client);
    put32(&mut m, 8, object);
    m
}

/// The RM status of a reply, mapped as `r535_rpc_status_to_errno` does
/// (`rpc.c:101-112`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RmError {
    /// `NV_ERR_NOT_READY` (0x55) or `NV_ERR_TIMEOUT_RETRY` (0x66): try again.
    Busy(u32),
    /// `NV_ERR_NO_MEMORY` (0x51).
    NoMemory(u32),
    /// Any other non-zero status.
    Invalid(u32),
    /// The reply is shorter than its header or does not answer the request.
    BadReply,
}

pub fn status_error(status: u32) -> Option<RmError> {
    match status {
        0 => None,
        0x55 | 0x66 => Some(RmError::Busy(status)),
        0x51 => Some(RmError::NoMemory(status)),
        s => Some(RmError::Invalid(s)),
    }
}

/// Check an allocation's reply (`r535_gsp_rpc_rm_alloc_push`, `alloc.c:47-72`):
/// it echoes the request with the status filled in.
pub fn check_alloc_reply(payload: &[u8], client: u32, object: u32) -> Result<(), RmError> {
    if payload.len() < ALLOC_HDR || get32(payload, 0) != client || get32(payload, 8) != object {
        return Err(RmError::BadReply);
    }
    status_error(get32(payload, 16)).map_or(Ok(()), Err)
}

/// A control's reply (`ctrl.c:29-58`): its status, and the params it returns.
pub fn check_control_reply(payload: &[u8], client: u32, object: u32, cmd: u32) -> Result<&[u8], RmError> {
    if payload.len() < CTRL_HDR || get32(payload, 0) != client || get32(payload, 4) != object || get32(payload, 8) != cmd {
        return Err(RmError::BadReply);
    }
    match status_error(get32(payload, 12)) {
        Some(e) => Err(e),
        None => {
            let n = (get32(payload, 16) as usize).min(payload.len() - CTRL_HDR);
            Ok(&payload[CTRL_HDR..CTRL_HDR + n])
        }
    }
}

// ---- parameter blocks -------------------------------------------------------------

/// `NV0000_ALLOC_PARAMETERS` (r570, 120 bytes): `hClient`, `processID = ~0`
/// (`r570_gsp_client_ctor`), the process name (empty) and a null `pOsPidInfo`.
pub fn root_params(client: u32) -> Vec<u8> {
    let mut p = vec![0u8; 120];
    put32(&mut p, 0, client);
    put32(&mut p, 4, 0xffff_ffff);
    p
}

/// `NV0080_ALLOC_PARAMETERS` (56 bytes): only `hClientShare` (offset 4) is set
/// (`r535_gsp_device_ctor`, `device.c:117-124`).
pub fn device_params(client: u32) -> Vec<u8> {
    let mut p = vec![0u8; 56];
    put32(&mut p, 4, client);
    p
}

/// `NV2080_ALLOC_PARAMETERS`: `subDeviceId` 0.
pub fn subdevice_params() -> Vec<u8> {
    vec![0u8; 4]
}

/// The parameters of `GPU_GET_NAME_STRING`: ASCII flavour.
pub fn name_string_params() -> Vec<u8> {
    vec![0u8; NAME_STRING_PARAMS_SIZE]
}

/// The name in a `GPU_GET_NAME_STRING` reply's params (ASCII, NUL terminated).
pub fn name_from_params(params: &[u8]) -> Option<&str> {
    let s = params.get(4..4 + 64)?;
    let end = s.iter().position(|&c| c == 0)?;
    core::str::from_utf8(&s[..end]).ok().filter(|n| !n.is_empty())
}

// ---- the VA space (phase 6a) ------------------------------------------------------

/// `NV_VASPACE_ALLOCATION_PARAMETERS` (`vmm.h:12-22`, 48 bytes) for an
/// *externally owned* space (`r535_mmu_vaspace_new(.., external = true)`,
/// `vmm.c:53-65`): `index` 0 (`GPU_NEW`), `flags` = `IS_EXTERNALLY_OWNED`
/// (bit 3), everything else 0. RM fills the rest in its reply.
pub const VASPACE_PARAMS_SIZE: usize = 48;
pub const VASPACE_FLAG_EXTERNALLY_OWNED: u32 = 1 << 3;

pub fn vaspace_params() -> Vec<u8> {
    let mut p = vec![0u8; VASPACE_PARAMS_SIZE];
    put32(&mut p, 4, VASPACE_FLAG_EXTERNALLY_OWNED);
    p
}

/// What RM says about the space it made (the reply's parameter block): `vaSize`
/// at offset 8 and `vaBase` at 40. Both are RM's; a mapping must lie in
/// `[base, base + size)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaSpace {
    pub base: u64,
    pub size: u64,
}

pub fn vaspace_from_reply(payload: &[u8]) -> Option<VaSpace> {
    let p = payload.get(ALLOC_HDR..ALLOC_HDR + VASPACE_PARAMS_SIZE)?;
    Some(VaSpace {
        size: u64::from_le_bytes(p[8..16].try_into().unwrap()),
        base: u64::from_le_bytes(p[40..48].try_into().unwrap()),
    })
}

/// `NV0080_CTRL_DMA_SET_PAGE_DIRECTORY_PARAMS` (`vmm.h:99-107`, 32 bytes):
/// `physAddress` u64, `numEntries`, `flags` (aperture: 0 = video memory, 1 =
/// coherent system, 2 = non-coherent; `NV0080_CTRL_DMA_SET_PAGE_DIRECTORY_FLAGS_APERTURE`
/// bits 1:0), `hVASpace`, `chId`, `subDeviceId`, `pasid`.
pub fn set_page_directory_params(root: u64, entries: u32, aperture: u32, vaspace: u32) -> Vec<u8> {
    let mut p = vec![0u8; CTRL_DMA_SET_PAGE_DIRECTORY_SIZE];
    p[0..8].copy_from_slice(&root.to_le_bytes());
    put32(&mut p, 8, entries);
    put32(&mut p, 12, aperture);
    put32(&mut p, 16, vaspace);
    p
}

// ---- performance state (G5 note: clocks and pstate) --------------------------------

/// `NV2080_CTRL_CMD_PERF_GET_CURRENT_PSTATE` (`ctrl2080perf.h`): one `NvU32`, a `NV2080_CTRL_PERF_PSTATES_Pn` bit (P8 = `0x100`).
pub const CTRL_PERF_GET_CURRENT_PSTATE: u32 = 0x2080_2068;
pub const PSTATE_PARAMS_SIZE: usize = 4;
/// `NV2080_CTRL_CMD_PERF_GET_LEVEL_INFO_V2`: `level`, `flags`, 32 x `GET_CLK_INFO` (24 bytes: flags, domain, current, default, min, max kHz), the list's length
/// (sizes checked by `gen/perf.c`).
pub const CTRL_PERF_GET_LEVEL_INFO_V2: u32 = 0x2080_200b;
pub const LEVEL_INFO_PARAMS_SIZE: usize = 780;
const CLK_INFO_SIZE: usize = 24;
const CLK_INFO_MAX: usize = 32;
const CLK_INFO_LIST: usize = 8;

/// One clock domain of a performance level, frequencies in kHz as RM reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClkInfo {
    pub domain: u32,
    pub current: u32,
    pub default: u32,
    pub min: u32,
    pub max: u32,
}

/// The parameters of `GET_LEVEL_INFO_V2` for `level` (flags 0: default type, no mode).
pub fn level_info_params(level: u32) -> Vec<u8> {
    let mut p = vec![0u8; LEVEL_INFO_PARAMS_SIZE];
    put32(&mut p, 0, level);
    p
}

/// The P-state in a `GET_CURRENT_PSTATE` reply: the number n of the `Pn` bit (`None` when zero or not a single bit).
pub fn pstate_from_params(params: &[u8]) -> Option<u32> {
    let bits = get32(params.get(..PSTATE_PARAMS_SIZE)?, 0);
    (bits.count_ones() == 1 && bits <= 0x8000).then(|| bits.trailing_zeros())
}

/// The domains of a `GET_LEVEL_INFO_V2` reply (the first `perfGetClkInfoListSize`, at most 32).
pub fn level_info_from_params(params: &[u8]) -> Option<Vec<ClkInfo>> {
    let params = params.get(..LEVEL_INFO_PARAMS_SIZE)?;
    let n = get32(params, CLK_INFO_LIST + CLK_INFO_MAX * CLK_INFO_SIZE) as usize;
    if n > CLK_INFO_MAX {
        return None;
    }
    Some(
        (0..n)
            .map(|i| {
                let at = CLK_INFO_LIST + i * CLK_INFO_SIZE;
                ClkInfo { domain: get32(params, at + 4), current: get32(params, at + 8), default: get32(params, at + 12), min: get32(params, at + 16), max: get32(params, at + 20) }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT_REQ: &[u8] = include_bytes!("../fixtures/rm-root-req.bin");
    const ROOT_REP: &[u8] = include_bytes!("../fixtures/rm-root-rep.bin");
    const DEV_REQ: &[u8] = include_bytes!("../fixtures/rm-device-req.bin");
    const DEV_REP: &[u8] = include_bytes!("../fixtures/rm-device-rep.bin");
    const SUB_REQ: &[u8] = include_bytes!("../fixtures/rm-subdev-req.bin");
    const SUB_REP: &[u8] = include_bytes!("../fixtures/rm-subdev-rep.bin");
    const CTRL_REQ: &[u8] = include_bytes!("../fixtures/rm-ctrl801813-req.bin");
    const CTRL_REP: &[u8] = include_bytes!("../fixtures/rm-ctrl801813-rep.bin");

    #[test]
    fn the_three_allocations_reproduce_nouveaus_rpcs() {
        assert_eq!(alloc_request(H_CLIENT, H_CLIENT, H_CLIENT, NV01_ROOT, &root_params(H_CLIENT)), ROOT_REQ);
        assert_eq!(alloc_request(H_CLIENT, H_CLIENT, H_DEVICE, NV01_DEVICE_0, &device_params(H_CLIENT)), DEV_REQ);
        assert_eq!(alloc_request(H_CLIENT, H_DEVICE, H_SUBDEVICE, NV20_SUBDEVICE_0, &subdevice_params()), SUB_REQ);
        assert_eq!((ROOT_REQ.len(), DEV_REQ.len(), SUB_REQ.len()), (152, 88, 36));
    }

    #[test]
    fn the_replies_are_the_request_with_status_zero() {
        assert_eq!(ROOT_REP, ROOT_REQ);
        assert_eq!(check_alloc_reply(ROOT_REP, H_CLIENT, H_CLIENT), Ok(()));
        assert_eq!(check_alloc_reply(DEV_REP, H_CLIENT, H_DEVICE), Ok(()));
        assert_eq!(check_alloc_reply(SUB_REP, H_CLIENT, H_SUBDEVICE), Ok(()));
    }

    #[test]
    fn a_control_request_matches_nouveaus() {
        // the trace's control on the device: cmd 0x801813, 32 parameter bytes
        let params = &CTRL_REQ[CTRL_HDR..];
        assert_eq!(params.len(), 32);
        assert_eq!(control_request(H_CLIENT, H_DEVICE, 0x0080_1813, params), CTRL_REQ);
        assert_eq!(check_control_reply(CTRL_REP, H_CLIENT, H_DEVICE, 0x0080_1813), Ok(params));
    }

    #[test]
    fn replies_are_checked_against_the_request() {
        assert_eq!(check_alloc_reply(&ROOT_REP[..31], H_CLIENT, H_CLIENT), Err(RmError::BadReply));
        assert_eq!(check_alloc_reply(DEV_REP, H_CLIENT + 1, H_DEVICE), Err(RmError::BadReply));
        assert_eq!(check_alloc_reply(DEV_REP, H_CLIENT, H_DEVICE + 1), Err(RmError::BadReply));
        assert_eq!(check_control_reply(&CTRL_REP[..23], H_CLIENT, H_DEVICE, 0x0080_1813), Err(RmError::BadReply));
        assert_eq!(check_control_reply(CTRL_REP, H_CLIENT + 1, H_DEVICE, 0x0080_1813), Err(RmError::BadReply));
        assert_eq!(check_control_reply(CTRL_REP, H_CLIENT, H_DEVICE + 1, 0x0080_1813), Err(RmError::BadReply));
        assert_eq!(check_control_reply(CTRL_REP, H_CLIENT, H_DEVICE, 0x0080_1814), Err(RmError::BadReply));
        // a status
        let mut bad = DEV_REP.to_vec();
        bad[16] = 0x51;
        assert_eq!(check_alloc_reply(&bad, H_CLIENT, H_DEVICE), Err(RmError::NoMemory(0x51)));
        let mut bad = CTRL_REP.to_vec();
        bad[12] = 0x66;
        assert_eq!(check_control_reply(&bad, H_CLIENT, H_DEVICE, 0x0080_1813), Err(RmError::Busy(0x66)));
    }

    #[test]
    fn status_mapping_is_the_errno_table() {
        assert_eq!(status_error(0), None);
        assert_eq!(status_error(0x55), Some(RmError::Busy(0x55)));
        assert_eq!(status_error(0x66), Some(RmError::Busy(0x66)));
        assert_eq!(status_error(0x51), Some(RmError::NoMemory(0x51)));
        assert_eq!(status_error(0x1f), Some(RmError::Invalid(0x1f)));
        assert_eq!(status_error(1), Some(RmError::Invalid(1)));
    }

    #[test]
    fn control_reply_params_are_bounded_by_the_message() {
        // paramsSize larger than the bytes present: only what is there
        let mut m = CTRL_REP.to_vec();
        put32(&mut m, 16, 4000);
        assert_eq!(check_control_reply(&m, H_CLIENT, H_DEVICE, 0x0080_1813).unwrap().len(), 32);
        put32(&mut m, 16, 8);
        assert_eq!(check_control_reply(&m, H_CLIENT, H_DEVICE, 0x0080_1813).unwrap().len(), 8);
    }

    const VAS_REQ: &[u8] = include_bytes!("../fixtures/rm-vaspace-req.bin");
    const VAS_REP: &[u8] = include_bytes!("../fixtures/rm-vaspace-rep.bin");

    #[test]
    fn the_external_vaspace_matches_nouveaus_rpc() {
        assert_eq!(alloc_request(H_CLIENT, H_DEVICE, H_VASPACE, FERMI_VASPACE_A, &vaspace_params()), VAS_REQ);
        assert_eq!(VAS_REQ.len(), ALLOC_HDR + 48);
        assert_eq!(check_alloc_reply(VAS_REP, H_CLIENT, H_VASPACE), Ok(()));
        // RM's answer: the space starts at 64 MiB and runs to the 49-bit top
        let v = vaspace_from_reply(VAS_REP).unwrap();
        assert_eq!((v.base, v.size), (0x400_0000, (1 << 49) - 0x400_0000));
        assert_eq!(vaspace_from_reply(&VAS_REP[..ALLOC_HDR + 47]), None);
        // the request itself carries no size yet
        assert_eq!(vaspace_from_reply(VAS_REQ), Some(VaSpace { base: 0, size: 0 }));
    }

    #[test]
    fn set_page_directory_matches_nouveaus_control() {
        // the trace: root 0x1_f07d_1000 in VRAM, 4 root entries
        let p = set_page_directory_params(0x1_f07d_1000, 4, 0, H_VASPACE);
        assert_eq!(p, &CTRL_REQ[CTRL_HDR..]);
        assert_eq!(control_request(H_CLIENT, H_DEVICE, CTRL_DMA_SET_PAGE_DIRECTORY, &p), CTRL_REQ);
        assert_eq!(p.len(), CTRL_DMA_SET_PAGE_DIRECTORY_SIZE);
        // every field where the C struct has it
        let q = set_page_directory_params(0x1122_3344_5566_7788, 5, 6, 7);
        assert_eq!(q[0..8], 0x1122_3344_5566_7788u64.to_le_bytes());
        assert_eq!((get32(&q, 8), get32(&q, 12), get32(&q, 16)), (5, 6, 7));
        assert!(q[20..].iter().all(|&b| b == 0));
        assert_eq!((CTRL_DMA_SET_PAGE_DIRECTORY, FERMI_VASPACE_A, H_VASPACE), (0x80_1813, 0x90f1, 0x90f1_0000));
    }

    #[test]
    fn free_and_headers_have_the_documented_layout() {
        let f = free_request(H_CLIENT, H_DEVICE);
        assert_eq!(f.len(), 16);
        assert_eq!((get32(&f, 0), get32(&f, 4), get32(&f, 8), get32(&f, 12)), (H_CLIENT, 0, H_DEVICE, 0));
        // header field offsets, with all fields distinct
        let a = alloc_request(1, 2, 3, 4, &[9, 9]);
        assert_eq!([get32(&a, 0), get32(&a, 4), get32(&a, 8), get32(&a, 12), get32(&a, 16), get32(&a, 20), get32(&a, 24)], [1, 2, 3, 4, 0, 2, 0]);
        assert_eq!((a.len(), &a[ALLOC_HDR..]), (34, &[9u8, 9][..]));
        let c = control_request(1, 2, 3, &[7]);
        assert_eq!([get32(&c, 0), get32(&c, 4), get32(&c, 8), get32(&c, 12), get32(&c, 16), get32(&c, 20)], [1, 2, 3, 0, 1, 0]);
        assert_eq!((c.len(), c[CTRL_HDR]), (25, 7));
        assert_eq!((FN_FREE, FN_GSP_RM_CONTROL, FN_GSP_RM_ALLOC), (10, 76, 103));
        assert_eq!((NV01_ROOT, NV01_DEVICE_0, NV20_SUBDEVICE_0), (0, 0x80, 0x2080));
    }

    #[test]
    fn name_string_params_and_reply() {
        assert_eq!(name_string_params().len(), 132);
        assert_eq!(CTRL_GPU_GET_NAME_STRING, 0x2080_0110);
        let mut p = name_string_params();
        assert_eq!(name_from_params(&p), None, "empty");
        p[4..4 + 7].copy_from_slice(b"RTX 305");
        assert_eq!(name_from_params(&p), Some("RTX 305"));
        p[4..4 + 64].fill(b'x');
        assert_eq!(name_from_params(&p), None, "no terminator in the 64 bytes");
        p[4 + 63] = 0;
        assert_eq!(name_from_params(&p).map(|s| s.len()), Some(63));
        assert_eq!(name_from_params(&p[..67]), None, "field cut short");
    }

    #[test]
    fn parameter_blocks_put_their_fields_where_the_c_structs_do() {
        let r = root_params(0x1234);
        assert_eq!((get32(&r, 0), get32(&r, 4)), (0x1234, 0xffff_ffff));
        assert!(r[8..].iter().all(|&b| b == 0) && r.len() == 120);
        let d = device_params(0x5678);
        assert_eq!(d.len(), 56);
        assert_eq!(get32(&d, 4), 0x5678);
        assert!(d[..4].iter().all(|&b| b == 0) && d[8..].iter().all(|&b| b == 0));
        assert_eq!(subdevice_params(), [0, 0, 0, 0]);
    }

    #[test]
    fn perf_controls_follow_the_c_layouts() {
        // sizes and offsets from nvgpu/gen/perf.c (clang on ctrl2080perf.h)
        assert_eq!((PSTATE_PARAMS_SIZE, LEVEL_INFO_PARAMS_SIZE), (4, 780));
        assert_eq!(4 + 4 + CLK_INFO_MAX * CLK_INFO_SIZE + 4, LEVEL_INFO_PARAMS_SIZE);
        assert_eq!((CTRL_PERF_GET_CURRENT_PSTATE, CTRL_PERF_GET_LEVEL_INFO_V2), (0x20802068, 0x2080200b));
        let q = level_info_params(3);
        assert_eq!((q.len(), get32(&q, 0), get32(&q, 4)), (780, 3, 0));
        assert!(q[8..].iter().all(|&b| b == 0));
    }

    #[test]
    fn pstate_is_the_bit_number() {
        assert_eq!(pstate_from_params(&0x100u32.to_le_bytes()), Some(8));
        assert_eq!(pstate_from_params(&1u32.to_le_bytes()), Some(0));
        assert_eq!(pstate_from_params(&0x8000u32.to_le_bytes()), Some(15));
        assert_eq!(pstate_from_params(&0u32.to_le_bytes()), None);
        assert_eq!(pstate_from_params(&0x180u32.to_le_bytes()), None, "two bits are not a state");
        assert_eq!(pstate_from_params(&0x1_0000u32.to_le_bytes()), None, "SKIP_ENTRY is not a state");
        assert_eq!(pstate_from_params(&[1, 0, 0]), None, "reply cut short");
    }

    #[test]
    fn level_info_reads_the_list_with_its_fields_in_place() {
        let mut p = level_info_params(1);
        for (i, (dom, cur)) in [(0x10u32, 210_000u32), (0x4, 405_000)].iter().enumerate() {
            let at = CLK_INFO_LIST + i * CLK_INFO_SIZE;
            put32(&mut p, at, 0xf0f0);
            for (k, v) in [*dom, *cur, cur + 1, cur + 2, cur + 3].iter().enumerate() {
                put32(&mut p, at + 4 + 4 * k, *v);
            }
        }
        put32(&mut p, 776, 2);
        let l = level_info_from_params(&p).unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(l[0], ClkInfo { domain: 0x10, current: 210_000, default: 210_001, min: 210_002, max: 210_003 });
        assert_eq!(l[1], ClkInfo { domain: 0x4, current: 405_000, default: 405_001, min: 405_002, max: 405_003 });
        put32(&mut p, 776, 32);
        assert_eq!(level_info_from_params(&p).unwrap().len(), 32);
        put32(&mut p, 776, 33);
        assert!(level_info_from_params(&p).is_none(), "a count past the array");
        assert!(level_info_from_params(&p[..779]).is_none(), "reply cut short");
    }
}
