//! The booter on SEC2 and the start of the GSP (phase 4e). `booter_load` is a
//! signed image that runs on SEC2 (the Security Engine 2 falcon), reads the
//! `GspFwWprMeta` the host prepared, sets up the final WPR2 and starts the GSP's
//! RISC-V core with the firmware loaded; it reports through mailbox 0.
//! Pure over [`Mmio`], like [`crate::falcon`]. Paths are relative to
//! `drivers/gpu/drm/nouveau/nvkm/` in Linux v7.2.2.
//!
//! Sequence (`subdev/gsp/tu102.c:189-210` `tu102_gsp_init`, `falcon/fw.c:74-124`,
//! `subdev/gsp/rm/r535/gsp.c:1782-1790`):
//! 1. [`prepare`]: the falcon's configuration is read, the fuse register picks the
//!    signature, and the signature is patched into the image copy;
//! 2. the caller copies the patched image to DMA memory;
//! 3. [`execute`]: reset, DMA load, boot with the WPR meta's bus address in the
//!    mailboxes, wait for the halt, mailbox 0 must be 0;
//! 4. [`gsp_running`]: write the GSP boot app version to the GSP falcon and
//!    check its RISC-V core is active.
//!
//! Oracle: `fixtures/booter-load.txt` (`trace-gsp`, 8,576 to 8,838 s).

use alloc::vec::Vec;

use crate::falcon::{Falcon, FalconError, LoadParams};
use crate::firmware::{Booter, FwError};
use crate::Mmio;

/// SEC2 on the GA106: base `0x840000`, second bank at `+0x1000` (the trace
/// reads `0x841668` for the select; `engine/sec2/ga102.c` uses the same
/// `ga102_flcn_*` helpers as the GSP).
pub const SEC2: Falcon = Falcon { base: 0x84_0000, addr2: 0x1000 };

/// The GSP falcon register `nvkm_falcon_wr32(&gsp->falcon, 0x080, app_version)`
/// writes (`rm/r535/gsp.c:1785`; the trace: `W 0x110080 0`).
pub const GSP_APP_VERSION: u32 = 0x080;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BooterError {
    /// The booter's engine id has no fuse register this port knows.
    NoFuseRegister,
    Fuse(FwError),
    Falcon(FalconError),
}

impl From<FalconError> for BooterError {
    fn from(e: FalconError) -> Self {
        BooterError::Falcon(e)
    }
}

/// `ga102_gsp_booter_ctor` (`subdev/gsp/ga102.c:41-92`): what the falcon
/// needs of a parsed booter.
pub fn load_params(b: &Booter) -> LoadParams {
    LoadParams {
        imem_base_img: b.imem_base_img,
        imem_base: 0,
        imem_size: b.imem_size,
        dmem_base_img: b.dmem_base_img,
        dmem_base: 0,
        dmem_size: b.dmem_size,
        dmem_sign: b.dmem_sign,
        boot_addr: b.boot_addr,
        engine_id: b.engine_id,
        ucode_id: b.ucode_id,
    }
}

/// A booter ready to be copied to DMA memory: the image with its signature
/// patched in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub image: Vec<u8>,
    pub params: LoadParams,
    pub sig_index: usize,
}

/// `nvkm_falcon_get` (falcon configuration reads) and the signature patch
/// (`nvkm_falcon_fw_patch`, `falcon/fw.c:30-64`, index by
/// `ga100_flcn_fw_signature`, `falcon/ga100.c:31-65`).
pub fn prepare(m: &impl Mmio, falcon: &Falcon, b: &Booter) -> Result<Prepared, BooterError> {
    falcon.probe(m);
    let reg = b.fuse_register().ok_or(BooterError::NoFuseRegister)?;
    let idx = b.signature_index(m.rd32(reg)).map_err(BooterError::Fuse)?;
    let mut image = b.image.to_vec();
    b.patch(&mut image, idx).map_err(BooterError::Fuse)?;
    Ok(Prepared { image, params: load_params(b), sig_index: idx })
}

/// Reset SEC2, load the image from `dma_addr`, start it with `wpr_meta`'s bus
/// address in mailboxes 0 and 1 (`tu102_gsp_booter_load`, `tu102.c:70-74`), and
/// wait for it. Returns the mailboxes (`(0, 0)` on success).
pub fn execute(m: &impl Mmio, sec2: &Falcon, p: &Prepared, dma_addr: u64, wpr_meta: u64) -> Result<(u32, u32), BooterError> {
    sec2.reset(m)?;
    sec2.load(m, dma_addr, &p.params)?;
    Ok(sec2.boot(m, &p.params, Some(wpr_meta as u32), Some((wpr_meta >> 32) as u32), 0, 0)?)
}

/// `r535_gsp_init` up to the wait (`rm/r535/gsp.c:1782-1790`): the boot app
/// version goes to the GSP falcon and its RISC-V core must be running.
pub fn gsp_running(m: &impl Mmio, gsp: &Falcon, app_version: u32) -> bool {
    gsp.wr(m, GSP_APP_VERSION, app_version);
    gsp.riscv_active(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::falcon::tests::{replay_of, Sim};
    use crate::falcon::GSP;
    use crate::firmware::tests::firmware;
    use std::string::String;

    const FIXTURE: &str = include_str!("../fixtures/booter-load.txt");
    /// `W 0x840110 0x00fffd00`: the booter image's bus address in the trace.
    const TRACE_DMA_ADDR: u64 = 0xfffd_0000;
    /// `W 0x840040 0xf7efe000`, `W 0x840044 0`: the WPR meta's bus address.
    const TRACE_WPR_META: u64 = 0xf7ef_e000;

    /// The trace's polled registers of SEC2 and the GSP falcon, idle.
    fn replay() -> crate::mmio::testing::ReplayMmio {
        let mut m = replay_of(FIXTURE);
        m.fallback.extend([(0x84_00f4, 0x47f7), (0x84_0100, 0x10), (0x84_0118, 0x602)]);
        m
    }

    fn fmt(w: &[(u32, u32)]) -> String {
        w.iter().map(|(o, v)| std::format!("W {o:#08x} {v:#010x}\n")).collect()
    }

    fn booter_blob() -> Option<Vec<u8>> {
        firmware("booter_load-570.144.bin")
    }

    #[test]
    fn sequence_replays_the_trace() {
        let Some(blob) = booter_blob() else { return };
        let b = Booter::parse(&blob).unwrap();
        let m = replay();
        let p = prepare(&m, &SEC2, &b).unwrap();
        // 0x824148 = 1 -> signature 0
        assert_eq!(p.sig_index, 0);
        let mb = execute(&m, &SEC2, &p, TRACE_DMA_ADDR, TRACE_WPR_META).unwrap();
        assert_eq!(mb, (0, 0));
        // r535_gsp_init: app version 0, then the RISC-V bit (0x111388 = 0x80).
        assert!(gsp_running(&m, &GSP, 0));
        assert_eq!(fmt(&m.writes.borrow()), fmt(&m.expected_writes));
        for reg in [0x84_012c, 0x84_0108, 0x82_4148, 0x84_1668, 0x84_0048, 0x84_03c0, 0x00_0000, 0x84_0624, 0x84_0600, 0x84_0040, 0x84_0044, 0x11_1388] {
            assert_eq!(m.unread(reg), 0, "{reg:#x}");
        }
    }

    #[test]
    fn the_image_carries_signature_0_where_the_header_says() {
        let Some(blob) = booter_blob() else { return };
        let b = Booter::parse(&blob).unwrap();
        let m = replay();
        let p = prepare(&m, &SEC2, &b).unwrap();
        assert_ne!(p.image, b.image);
        let at = b.patch_loc as usize;
        assert_eq!(&p.image[at..at + b.sig_size as usize], b.signature(0).unwrap());
        assert_eq!(&p.image[..at], &b.image[..at]);
        assert_eq!(p.params.boot_addr, 0x100);
        assert_eq!(p.params.imem_size, 0x8900);
        assert_eq!((p.params.dmem_base_img, p.params.dmem_size), (0x8a00, 0x6200));
    }

    #[test]
    fn load_params_mirror_the_ga102_ctor() {
        let Some(blob) = booter_blob() else { return };
        let b = Booter::parse(&blob).unwrap();
        let p = load_params(&b);
        assert_eq!((p.imem_base_img, p.imem_base, p.imem_size), (0x100, 0, 0x8900));
        assert_eq!((p.dmem_base_img, p.dmem_base, p.dmem_size), (0x8a00, 0, 0x6200));
        assert_eq!((p.dmem_sign, p.boot_addr), (35344 - 0x8a00, 0x100));
        assert_eq!((p.engine_id, p.ucode_id), (1, 3));
    }

    #[test]
    fn booter_without_a_known_fuse_register_is_refused() {
        let Some(blob) = booter_blob() else { return };
        let b = Booter::parse(&blob).unwrap();
        let mut b2 = b.clone();
        b2.engine_id = 0x2;
        let m = replay();
        assert_eq!(prepare(&m, &SEC2, &b2), Err(BooterError::NoFuseRegister));
    }

    #[test]
    fn fuse_newer_than_the_signatures_is_refused_before_anything_is_reset() {
        let Some(blob) = booter_blob() else { return };
        let b = Booter::parse(&blob).unwrap();
        let text = FIXTURE.replace("R 0x824148 0x00000001", "R 0x824148 0x00000004");
        let m = replay_of(&text);
        assert_eq!(prepare(&m, &SEC2, &b), Err(BooterError::Fuse(FwError::FuseMismatch)));
        assert!(m.writes.borrow().is_empty());
    }

    #[test]
    fn a_booter_that_leaves_a_bad_mailbox_or_never_halts_is_reported() {
        let Some(blob) = booter_blob() else { return };
        let b = Booter::parse(&blob).unwrap();
        // mailbox 0 = 0x40 after the run (the fixture reads 0 at the end).
        let text = FIXTURE.replace("R 0x840040 0x00000000\nR 0x840044 0x00000000\nW 0x110080", "R 0x840040 0x00000040\nR 0x840044 0x00000001\nW 0x110080");
        let m = replay_of(&text);
        let mut m = m;
        m.fallback.extend([(0x84_00f4, 0x47f7), (0x84_0100, 0x10), (0x84_0118, 0x602)]);
        let p = prepare(&m, &SEC2, &b).unwrap();
        assert_eq!(
            execute(&m, &SEC2, &p, TRACE_DMA_ADDR, TRACE_WPR_META),
            Err(BooterError::Falcon(FalconError::Mailbox { mbox0: 0x40, mbox1: 1 }))
        );
        let mut m = replay();
        m.fallback.retain(|(o, _)| *o != 0x84_0100);
        m.fallback.push((0x84_0100, 0));
        let p = prepare(&m, &SEC2, &b).unwrap();
        assert!(matches!(
            execute(&m, &SEC2, &p, TRACE_DMA_ADDR, TRACE_WPR_META),
            Err(BooterError::Falcon(FalconError::BootTimeout { .. }))
        ));
    }

    #[test]
    fn gsp_not_running_is_reported() {
        let s = Sim::new(GSP.base);
        assert!(!gsp_running(&s, &GSP, 0x1234));
        assert!(s.writes.borrow().contains(&(GSP.base + 0x080, 0x1234)));
        s.set(GSP.base + 0x1388, 0x80);
        assert!(gsp_running(&s, &GSP, 0));
        assert_eq!(SEC2.base, 0x840000);
    }
}
