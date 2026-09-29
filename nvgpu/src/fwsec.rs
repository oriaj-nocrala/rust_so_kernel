//! FWSEC (phase 4c): the signed microcode in the VBIOS that sets up the
//! protected memory region (WPR2) before the GSP's own boot, run on the GSP
//! falcon. This module finds it in the VBIOS, builds the image with the
//! signature and the command patched in, and runs it through
//! [`crate::falcon`].
//!
//! Only the `FRTS` command is ported (`nvkm_gsp_fwsec_frts`,
//! `nvkm/subdev/gsp/fwsec.c:346-377`); `SB` (`fwsec-sb`, the one of the
//! suspend path) shares the image building and is here so a later phase
//! needs no rewrite. Descriptor version 3 only (the GA10x one; v2 needs the
//! `acr/bl` firmware file). Paths are relative to
//! `drivers/gpu/drm/nouveau/nvkm/` in Linux v7.2.2.
//!
//! Oracle: `fixtures/fwsec-frts.txt` (`trace-gsp`, 8,340 to 8,576 s) and
//! the VBIOS nouveau read in that boot (`trace-gsp/vbios-nouveau.bin`, all
//! images: FWSEC lives in the `0xe0` one, past what sysfs exposes).

use alloc::vec;
use alloc::vec::Vec;

use crate::falcon::{Falcon, FalconError, LoadParams};
use crate::firmware::{fwsec_signature_index, FwError};
use crate::vbios::Bios;
use crate::Mmio;

/// PMU table entry type of the FWSEC ucode (`fwsec.c:276`).
pub const PMU_TYPE_FWSEC: u8 = 0x85;
/// `NVFW_FALCON_APPIF_ID_DMEMMAPPER` (`fwsec.c:40`).
pub const APPIF_ID_DMEMMAPPER: u32 = 4;
/// `NVFW_FALCON_APPIF_DMEMMAPPER_CMD_FRTS` / `_SB` (`fwsec.c:60-61`).
pub const CMD_FRTS: u32 = 0x15;
pub const CMD_SB: u32 = 0x19;
/// `NVFW_FRTS_CMD_REGION_TYPE_FB` (`fwsec.c:83`).
pub const FRTS_REGION_FB: u32 = 2;
/// Size of one signature in the descriptor (`fwsec.c:251`, `96 * 4`).
pub const SIG_SIZE: usize = 96 * 4;
/// Where the engine id tells the fuse register is (`ga102.c:106`).
const FUSE_ENGINE_BIT: u32 = 0x400;
/// `0x8241c0 + (ucode_id - 1) * 4` (`ga102.c:107`).
const FUSE_REG_BASE: u32 = 0x82_41c0;

/// FRTS error register: `0x1400 + 0xe*4`, upper half (`fwsec.c:364`), and
/// the SB one, `0x1400 + 0x15*4`, lower half (`fwsec.c:330`).
pub const FRTS_ERR: u32 = 0x00_1438;
pub const SB_ERR: u32 = 0x00_1454;
/// The WPR2 bounds the FRTS leaves programmed (`fwsec.c:369-370`).
pub const WPR2_LO: u32 = 0x1f_a824;
pub const WPR2_HI: u32 = 0x1f_a828;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FwsecError {
    /// The VBIOS has no BIT `'p'` version 2 entry, or no PMU table.
    NoPmuTable,
    /// No PMU entry of type `0x85`.
    NoFwsec,
    /// The descriptor is not a supported one (bit 0 of the header clear, or a
    /// version other than 3): the version is given.
    Unsupported(u8),
    /// A table, the image or the signatures reach past the VBIOS or the image.
    Truncated,
    /// The DMEM interface has no supported application, or its version is not 1.
    BadInterface,
    /// No signatures, or the engine id has no known fuse register.
    NoSignature,
    Fuse(FwError),
    Falcon(FalconError),
    /// FWSEC-FRTS reported an error in `0x1438` (upper half).
    Frts(u32),
    /// FWSEC-SB reported an error in `0x1454` (lower half).
    Sb(u32),
}

impl From<FalconError> for FwsecError {
    fn from(e: FalconError) -> Self {
        FwsecError::Falcon(e)
    }
}

/// What FWSEC is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Create the FRTS region: `addr`/`size` in bytes of VRAM (`fwsec.c:117-123`).
    Frts { addr: u64, size: u64 },
    /// Secure boot (suspend path).
    Sb,
}

impl Command {
    fn init_cmd(&self) -> u32 {
        match self {
            Command::Frts { .. } => CMD_FRTS,
            Command::Sb => CMD_SB,
        }
    }
}

/// The FWSEC image ready to copy to DMA memory, and how to load it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fwsec {
    /// IMEM code then DMEM data (`dmem_size` bytes, zero padded to 256).
    pub image: Vec<u8>,
    pub params: LoadParams,
    /// `SignatureVersions`: bit mask of the fuse versions the signatures are for.
    pub fuse_ver: u32,
    sigs: Vec<u8>,
    /// Offset of the signature in `image` (`dmem_base_img + PKCDataOffset`).
    sig_at: usize,
    pub command: Command,
}

fn rd32(d: &[u8], at: usize) -> Result<u32, FwsecError> {
    d.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).ok_or(FwsecError::Truncated)
}
fn put32(d: &mut [u8], at: usize, v: u32) -> Result<(), FwsecError> {
    d.get_mut(at..at + 4).ok_or(FwsecError::Truncated)?.copy_from_slice(&v.to_le_bytes());
    Ok(())
}

/// `nvbios_pmuTe`/`nvbios_pmuEp` + the search of `nvkm_gsp_fwsec_init`
/// (`subdev/bios/pmu.c:28-77`, `fwsec.c:274-281`): the VBIOS address of the
/// FWSEC descriptor.
pub fn find_descriptor(bios: &Bios) -> Result<u32, FwsecError> {
    let p = bios.bit_entry(b'p').filter(|e| e.version == 2 && e.length >= 4).ok_or(FwsecError::NoPmuTable)?;
    let table = bios.rd32(p.offset as u32);
    if table == 0 {
        return Err(FwsecError::NoPmuTable);
    }
    let (hdr, len, cnt) = (bios.rd08(table + 1) as u32, bios.rd08(table + 2) as u32, bios.rd08(table + 3) as u32);
    for idx in 0..cnt {
        let e = table + hdr + idx * len;
        if bios.rd08(e) == PMU_TYPE_FWSEC {
            return Ok(bios.rd32(e + 2));
        }
    }
    Err(FwsecError::NoFwsec)
}

impl Fwsec {
    /// `nvkm_gsp_fwsec_init` + `nvkm_gsp_fwsec_v3` + `nvkm_gsp_fwsec_patch`
    /// (`fwsec.c:171-305`): read the descriptor, copy the image, register the
    /// signatures (not yet patched in) and patch the command into the DMEM
    /// interface.
    pub fn build(bios: &Bios, command: Command) -> Result<Fwsec, FwsecError> {
        let desc_at = find_descriptor(bios)?;
        // The v3 descriptor's fixed part (`fwsec.c:153-168`).
        let d = bios.slice(desc_at, 0x2c).ok_or(FwsecError::Truncated)?;
        let hdr = rd32(d, 0)?;
        if hdr & 1 == 0 {
            return Err(FwsecError::Unsupported(0));
        }
        let (size, vers) = ((hdr >> 16) & 0xffff, ((hdr >> 8) & 0xff) as u8);
        if vers != 3 {
            return Err(FwsecError::Unsupported(vers));
        }
        let pkc = rd32(d, 0x08)?;
        let iface = rd32(d, 0x0c)? as usize;
        let imem_phys = rd32(d, 0x10)?;
        let imem_size = rd32(d, 0x14)?;
        let dmem_phys = rd32(d, 0x1c)?;
        let dmem_load = rd32(d, 0x20)?;
        let engine_id = u16::from_le_bytes([d[0x24], d[0x25]]) as u32;
        let ucode_id = d[0x26] as u32;
        let nsig = d[0x27] as usize;
        let fuse_ver = u16::from_le_bytes([d[0x28], d[0x29]]) as u32;

        // `fw->dmem_size = ALIGN(DMEMLoadSize, 256)` (`fwsec.c:243`); the
        // image is the IMEM bytes then the DMEM bytes, as stored.
        let dmem_size = dmem_load.checked_add(255).ok_or(FwsecError::Truncated)? & !255;
        let stored = imem_size.checked_add(dmem_load).ok_or(FwsecError::Truncated)?;
        let src = bios.slice(desc_at + size, stored).ok_or(FwsecError::Truncated)?;
        let mut image = vec![0u8; imem_size as usize + dmem_size as usize];
        image[..src.len()].copy_from_slice(src);

        if nsig == 0 {
            return Err(FwsecError::NoSignature);
        }
        let sigs = bios.slice(desc_at + 0x2c, (nsig * SIG_SIZE) as u32).ok_or(FwsecError::Truncated)?.to_vec();
        let sig_at = imem_size as usize + pkc as usize;
        if sig_at + SIG_SIZE > image.len() {
            return Err(FwsecError::Truncated);
        }

        let mut fw = Fwsec {
            image,
            params: LoadParams {
                imem_base_img: 0,
                imem_base: imem_phys,
                imem_size,
                dmem_base_img: imem_size,
                dmem_base: dmem_phys,
                dmem_size,
                dmem_sign: pkc,
                boot_addr: 0,
                engine_id,
                ucode_id,
            },
            fuse_ver,
            sigs,
            sig_at,
            command,
        };
        fw.patch_interface(iface)?;
        Ok(fw)
    }

    /// `nvkm_gsp_fwsec_patch` (`fwsec.c:88-132`): find the DMEM mapper
    /// application in the interface table and fill its command buffer.
    fn patch_interface(&mut self, iface: usize) -> Result<(), FwsecError> {
        let dmem = self.params.dmem_base_img as usize;
        let hdr = dmem + iface;
        let h = self.image.get(hdr..hdr + 4).ok_or(FwsecError::Truncated)?;
        let (ver, hsize, len, cnt) = (h[0] as usize, h[1] as usize, h[2] as usize, h[3] as usize);
        if ver != 1 {
            return Err(FwsecError::BadInterface);
        }
        for i in 0..cnt {
            let app = hdr + hsize + i * len;
            if rd32(&self.image, app)? != APPIF_ID_DMEMMAPPER {
                continue;
            }
            let map = dmem + rd32(&self.image, app + 4)? as usize;
            // nvfw_falcon_appif_dmemmapper v3: init_cmd at +0x2c,
            // cmd_in_buffer_offset at +0x08 (`fwsec.c:46-67`).
            put32(&mut self.image, map + 0x2c, self.command.init_cmd())?;
            let cmd = dmem + rd32(&self.image, map + 0x08)? as usize;
            // read_vbios: ver 1, hdr = sizeof (24), addr 0 (u64), size 0, flags 2.
            put32(&mut self.image, cmd, 1)?;
            put32(&mut self.image, cmd + 4, 24)?;
            put32(&mut self.image, cmd + 8, 0)?;
            put32(&mut self.image, cmd + 12, 0)?;
            put32(&mut self.image, cmd + 16, 0)?;
            put32(&mut self.image, cmd + 20, 2)?;
            if let Command::Frts { addr, size } = self.command {
                // frts_region: ver 1, hdr = sizeof (20), addr, size (in 4 KiB pages), type FB.
                put32(&mut self.image, cmd + 24, 1)?;
                put32(&mut self.image, cmd + 28, 20)?;
                put32(&mut self.image, cmd + 32, (addr >> 12) as u32)?;
                put32(&mut self.image, cmd + 36, (size >> 12) as u32)?;
                put32(&mut self.image, cmd + 40, FRTS_REGION_FB)?;
            }
            return Ok(());
        }
        Err(FwsecError::BadInterface)
    }

    /// The register whose value picks the signature (`ga102.c:106-111`).
    pub fn fuse_register(&self) -> Result<u32, FwsecError> {
        if self.params.engine_id & FUSE_ENGINE_BIT == 0 {
            return Err(FwsecError::NoSignature);
        }
        Ok(FUSE_REG_BASE + self.params.ucode_id.wrapping_sub(1).wrapping_mul(4))
    }

    /// Patch the signature that matches the fuse register's value into the
    /// image (`ga102_gsp_fwsec_signature` + `nvkm_falcon_fw_patch`,
    /// `subdev/gsp/ga102.c:94-126`, `falcon/fw.c:30-64`). Returns its index.
    pub fn patch_signature(&mut self, fuse_reg: u32) -> Result<usize, FwsecError> {
        let idx = fwsec_signature_index(self.fuse_ver, fuse_reg).map_err(FwsecError::Fuse)?;
        let sig = self.sigs.get(idx * SIG_SIZE..(idx + 1) * SIG_SIZE).ok_or(FwsecError::Fuse(FwError::BadSignatures))?;
        self.image[self.sig_at..self.sig_at + SIG_SIZE].copy_from_slice(sig);
        Ok(idx)
    }
}

/// What FWSEC-FRTS left behind (`fwsec.c:369-371`): the raw WPR2 registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frts {
    pub wpr2_lo: u32,
    pub wpr2_hi: u32,
    /// The signature index that was patched.
    pub sig_index: usize,
}

/// The first half of `nvkm_falcon_fw_boot` that touches no falcon memory:
/// `nvkm_falcon_get` (reads the falcon's configuration) and the signature
/// patch (reads the fuse register). Afterwards `fw.image` is final: copy it
/// to DMA memory, then [`execute`].
pub fn prepare(m: &impl Mmio, falcon: &Falcon, fw: &mut Fwsec) -> Result<usize, FwsecError> {
    falcon.probe(m);
    let reg = m.rd32(fw.fuse_register()?);
    fw.patch_signature(reg)
}

/// The rest: reset, load from `dma_addr` (where the caller copied
/// `fw.image`), boot with mailbox 0 = 0, and verify. For FRTS that is the
/// error field of `0x1438` and the WPR2 registers (`fwsec.c:359-372`); for
/// SB, `0x1454` (`fwsec.c:325-336`).
pub fn execute(m: &impl Mmio, falcon: &Falcon, fw: &Fwsec, dma_addr: u64, sig_index: usize) -> Result<Frts, FwsecError> {
    falcon.reset(m)?;
    falcon.load(m, dma_addr, &fw.params)?;
    falcon.boot(m, &fw.params, Some(0), None, 0, 0)?;
    match fw.command {
        Command::Frts { .. } => {
            let err = m.rd32(FRTS_ERR) >> 16;
            if err != 0 {
                return Err(FwsecError::Frts(err));
            }
            Ok(Frts { wpr2_lo: m.rd32(WPR2_LO), wpr2_hi: m.rd32(WPR2_HI), sig_index })
        }
        Command::Sb => {
            let err = m.rd32(SB_ERR) & 0xffff;
            if err != 0 {
                return Err(FwsecError::Sb(err));
            }
            Ok(Frts { wpr2_lo: 0, wpr2_hi: 0, sig_index })
        }
    }
}

/// `tu102_gsp_vga_workspace_addr` (`subdev/gsp/tu102.c:275-292`): where the
/// VGA workspace starts, at the top of VRAM. `disp_reg` is `0x625f04`; with
/// bit 3 clear it is the last MiB, else the register's address (`<< 8`) unless
/// that is below the last MiB, when it is the last 128 KiB.
pub fn vga_workspace(fb_size: u64, disp_reg: u32) -> u64 {
    let base = fb_size - 0x10_0000;
    if disp_reg & 0x8 == 0 {
        return base;
    }
    let addr = ((disp_reg & 0xffff_ff00) as u64) << 8;
    if addr < base {
        fb_size - 0x2_0000
    } else {
        addr
    }
}

/// Where FRTS goes in VRAM (`tu102_gsp_oneinit`, `subdev/gsp/tu102.c:336-377`):
/// 1 MiB just below the VGA workspace, 128 KiB aligned.
pub fn frts_region(fb_size: u64, disp_reg: u32) -> (u64, u64) {
    let size = 0x10_0000u64;
    ((vga_workspace(fb_size, disp_reg) & !0x1_ffff) - size, size)
}

/// The register nouveau reads to place the workspace (`tu102.c:283`).
pub const DISP_VGA_REG: u32 = 0x62_5f04;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::falcon::tests::{frts_replay, replay_of, trace_params, FRTS_FIXTURE, TRACE_DMA_ADDR, TRACE_LIBOS};
    use crate::falcon::GSP;
    use crate::falcon::LoadParams;
    use std::string::String;

    /// The VBIOS nouveau read in the `trace-gsp` boot: all images, 565 760
    /// bytes. Not in git (D3): `None`, with a note, when it is not there.
    fn full_vbios_bytes() -> Option<Vec<u8>> {
        let dir = std::env::var("GPU_ORACLE")
            .unwrap_or_else(|_| std::format!("{}/constanos-gpu-oracle", std::env::var("HOME").unwrap_or_default()));
        match std::fs::read(std::format!("{dir}/trace-gsp/vbios-nouveau.bin")) {
            Ok(d) => Some(d),
            Err(e) => {
                std::eprintln!("SKIP: no full VBIOS in {dir} ({e}); see docs/gpu/gpu-plan.md D3");
                None
            }
        }
    }
    fn full_vbios() -> Option<Bios> {
        full_vbios_bytes().map(|d| Bios::new(d).unwrap())
    }

    const FB: u64 = 0x2_0000_0000; // 0x1183a4 = 0x2000 MiB (trace-gsp)

    #[test]
    fn frts_region_of_the_trace() {
        // 8 GiB, 0x625f04 = 1 (bit 3 clear): the workspace is the last MiB, so FRTS is
        // [0x1ffe00000, +1 MiB); the WPR2 low bound nouveau read, 0x01ffe000, is that >> 8.
        let (addr, size) = frts_region(FB, 1);
        assert_eq!((addr, size), (0x1_ffe0_0000, 0x10_0000));
        assert_eq!(addr >> 8, 0x01ff_e000);
    }

    #[test]
    fn frts_region_with_a_display_workspace() {
        // bit 3 set: address from the register (`(reg & ~0xff) << 8`); below the
        // last MiB it falls back to the last 128 KiB.
        let base = FB - 0x10_0000;
        let reg_hi = (((base + 0x2_0000) >> 8) as u32 & 0xffff_ff00) | 8;
        assert_eq!(frts_region(FB, reg_hi).0, ((base + 0x2_0000) & !0x1_ffff) - 0x10_0000);
        let reg_lo = (((base - 0x10_0000) >> 8) as u32 & 0xffff_ff00) | 8;
        assert_eq!(frts_region(FB, reg_lo).0, ((FB - 0x2_0000) & !0x1_ffff) - 0x10_0000);
        // Exactly at the base counts as "not below".
        let reg_eq = ((base >> 8) as u32 & 0xffff_ff00) | 8;
        assert_eq!(frts_region(FB, reg_eq).0, (base & !0x1_ffff) - 0x10_0000);
    }

    #[test]
    fn the_descriptor_of_this_board() {
        let Some(bios) = full_vbios() else { return };
        let fw = Fwsec::build(&bios, Command::Frts { addr: 0x1_ffe0_0000, size: 0x10_0000 }).unwrap();
        // What the trace shows nouveau loading: IMEM 0xe100 at 0, DMEM 0x800 at 0
        // from image offset 0xe100, signature hint 0x5a4, engine 0x400, ucode 9.
        assert_eq!(fw.params, trace_params());
        assert_eq!(fw.image.len(), 0xe100 + 0x800);
        assert_eq!(fw.fuse_register(), Ok(0x8241e0));
        // GA10x descriptors carry several signatures of 384 bytes.
        assert!(fw.sigs.len() >= SIG_SIZE && fw.sigs.len() % SIG_SIZE == 0);
    }

    #[test]
    fn the_interface_is_patched_with_the_command() {
        let Some(bios) = full_vbios() else { return };
        let dmem = 0xe100usize;
        let frts = Fwsec::build(&bios, Command::Frts { addr: 0x1_ffe0_0000, size: 0x10_0000 }).unwrap();
        let sb = Fwsec::build(&bios, Command::Sb).unwrap();
        let unpatched = {
            // the raw bytes, straight from the VBIOS
            let d = bios.slice(find_descriptor(&bios).unwrap(), 0x2c).unwrap();
            let size = u32::from_le_bytes(d[0..4].try_into().unwrap()) >> 16;
            bios.slice(find_descriptor(&bios).unwrap() + size, 0xe100 + 0x800).unwrap().to_vec()
        };
        // Only the interface's command buffer and init_cmd differ from the stored image.
        let diff = |a: &[u8], b: &[u8]| -> Vec<usize> { (0..b.len()).filter(|&i| a[i] != b[i]).collect() };
        let df = diff(&frts.image, &unpatched);
        let ds = diff(&sb.image, &unpatched);
        assert!(!df.is_empty() && !ds.is_empty());
        assert!(df.iter().all(|&i| i >= dmem), "the code is untouched");
        // init_cmd of the mapper: find it by the difference of the two commands
        let init = df.iter().copied().find(|i| frts.image[*i..*i + 4] == [0x15, 0, 0, 0]).unwrap();
        assert_eq!(&sb.image[init..init + 4], &[0x19, 0, 0, 0]);
        // the FRTS command buffer carries the region in pages; SB leaves it zero.
        let region = frts.image.windows(20).position(|w| {
            w == [1, 0, 0, 0, 20, 0, 0, 0, 0x00, 0xfe, 0x1f, 0x00, 0x00, 0x01, 0, 0, 2, 0, 0, 0]
        });
        assert!(region.is_some(), "frts_region (ver 1, hdr 20, addr 0x1ffe00, size 0x100, type FB) missing");
        assert!(sb.image.windows(20).position(|w| w[4..8] == [20, 0, 0, 0] && w[16..20] == [2, 0, 0, 0]).is_none());
    }

    #[test]
    fn signature_is_patched_where_the_trace_says() {
        let Some(bios) = full_vbios() else { return };
        let mut fw = Fwsec::build(&bios, Command::Frts { addr: 0x1_ffe0_0000, size: 0x10_0000 }).unwrap();
        let before = fw.image.clone();
        // trace-gsp: 0x8241e0 = 3
        let idx = fw.patch_signature(3).unwrap();
        let at = 0xe100 + 0x5a4;
        assert_eq!(&fw.image[at..at + SIG_SIZE], &fw.sigs[idx * SIG_SIZE..(idx + 1) * SIG_SIZE]);
        assert_eq!(&fw.image[..at], &before[..at]);
        assert_eq!(&fw.image[at + SIG_SIZE..], &before[at + SIG_SIZE..]);
        // a fuse newer than the signatures is refused and the image is left alone
        let mut fw2 = Fwsec::build(&bios, Command::Sb).unwrap();
        let clean = fw2.image.clone();
        assert!(matches!(fw2.patch_signature(0xffff_ffff), Err(FwsecError::Fuse(_))));
        assert_eq!(fw2.image, clean);
    }

    #[test]
    fn full_sequence_replays_the_trace() {
        let Some(bios) = full_vbios() else { return };
        let (addr, size) = frts_region(FB, 1);
        let mut fw = Fwsec::build(&bios, Command::Frts { addr, size }).unwrap();
        let m = frts_replay();
        let idx = prepare(&m, &GSP, &mut fw).unwrap();
        let r = execute(&m, &GSP, &fw, TRACE_DMA_ADDR, idx).unwrap();
        // The verify of the trace: no error, WPR2 0x01ffe000 - 0x01ffee00.
        assert_eq!((r.wpr2_lo, r.wpr2_hi), (0x01ff_e000, 0x01ff_ee00));
        GSP.gsp_reset(&m).unwrap();
        GSP.set_mailboxes(&m, TRACE_LIBOS as u32, (TRACE_LIBOS >> 32) as u32);

        let f = |w: &[(u32, u32)]| -> String { w.iter().map(|(o, v)| std::format!("W {o:#08x} {v:#010x}\n")).collect() };
        assert_eq!(f(&m.writes.borrow()), f(&m.expected_writes));
        // Every read of the fixture was consumed exactly.
        for reg in [0x11_012c, 0x11_0108, 0x82_41e0, 0x11_1668, 0x11_0048, 0x11_03c0, 0x11_0624, 0x11_0600, 0x00_0000, 0x11_0044, 0x1f_a824, 0x1f_a828, 0x00_1438]
        {
            assert_eq!(m.unread(reg), 0, "{reg:#x}");
        }
        assert_eq!(m.unread(0x11_0040), 0);
    }

    #[test]
    fn frts_error_is_the_upper_half_of_0x1438() {
        let Some(bios) = full_vbios() else { return };
        let (addr, size) = frts_region(FB, 1);
        let mut fw = Fwsec::build(&bios, Command::Frts { addr, size }).unwrap();
        // The trace's read of 0x1438 is 0; make it 0x00070000 (error 7) and
        // check the low half is not an error.
        let with = |v: &str| FRTS_FIXTURE.replace("R 0x001438 0x00000000", &std::format!("R 0x001438 {v}"));
        let m = replay_of(&with("0x00070000"));
        let idx = prepare(&m, &GSP, &mut fw).unwrap();
        assert_eq!(execute(&m, &GSP, &fw, TRACE_DMA_ADDR, idx), Err(FwsecError::Frts(7)));
        let m = replay_of(&with("0x0000ffff"));
        let idx = prepare(&m, &GSP, &mut fw).unwrap();
        assert!(execute(&m, &GSP, &fw, TRACE_DMA_ADDR, idx).is_ok());
    }

    #[test]
    fn a_falcon_that_never_halts_is_a_boot_timeout_not_an_frts_error() {
        let Some(bios) = full_vbios() else { return };
        let (addr, size) = frts_region(FB, 1);
        let mut fw = Fwsec::build(&bios, Command::Frts { addr, size }).unwrap();
        let mut m = frts_replay();
        m.fallback.retain(|(o, _)| *o != 0x11_0100);
        m.fallback.push((0x11_0100, 0));
        let idx = prepare(&m, &GSP, &mut fw).unwrap();
        assert!(matches!(
            execute(&m, &GSP, &fw, TRACE_DMA_ADDR, idx),
            Err(FwsecError::Falcon(FalconError::BootTimeout { .. }))
        ));
    }

    #[test]
    fn unsupported_descriptors_are_refused() {
        let Some(mut raw) = full_vbios_bytes() else { return };
        let bios = Bios::new(raw.clone()).unwrap();
        let at = find_descriptor(&bios).unwrap();
        let o = bios.raw_offset(at, 4).unwrap();
        // Hdr byte 1 = descriptor version.
        raw[o + 1] = 2;
        let v2 = Bios::new(raw.clone()).unwrap();
        assert_eq!(Fwsec::build(&v2, Command::Sb).map(|_| ()), Err(FwsecError::Unsupported(2)));
        raw[o + 1] = 3;
        // Hdr bit 0 clear.
        raw[o] &= !1;
        let nb = Bios::new(raw.clone()).unwrap();
        assert_eq!(Fwsec::build(&nb, Command::Sb).map(|_| ()), Err(FwsecError::Unsupported(0)));
        raw[o] |= 1;
        // No signatures.
        raw[o + 0x27] = 0;
        let ns = Bios::new(raw.clone()).unwrap();
        assert_eq!(Fwsec::build(&ns, Command::Sb).map(|_| ()), Err(FwsecError::NoSignature));
        raw[o + 0x27] = Bios::new(full_vbios_bytes().unwrap()).unwrap().rd08(at + 0x27);
        // Interface version other than 1.
        let good = Bios::new(raw.clone()).unwrap();
        assert!(Fwsec::build(&good, Command::Sb).is_ok());
    }

    /// The raw VBIOS offset of the FWSEC descriptor and of the BIT `'p'`
    /// entry, for tests that corrupt one field of a copy.
    fn offsets(raw: &[u8]) -> (usize, usize) {
        let bios = Bios::new(raw.to_vec()).unwrap();
        let desc = bios.raw_offset(find_descriptor(&bios).unwrap(), 0x2c).unwrap();
        let bit = raw.windows(5).position(|w| w == b"\xff\xb8BIT").unwrap();
        let (entries, stride) = (raw[bit + 10] as usize, raw[bit + 9] as usize);
        let p = (0..entries).map(|i| bit + 12 + i * stride).find(|&e| raw[e] == b'p').unwrap();
        (desc, p)
    }
    fn le32(raw: &mut [u8], at: usize, v: u32) {
        raw[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    #[test]
    fn the_patched_image_is_the_stored_one_plus_exactly_the_command() {
        // Measured on this board's VBIOS (`the_interface_...` in the notes of the plan):
        // the interface at DMEM+0x1c is version 1, header 4, entry 8, two apps
        // (id 4 -> DMEM+0x560, id 5 -> DMEM+0x7ac); the id-4 mapper is "DMAP" v3
        // whose cmd_in_buffer is at DMEM+0x7c0.
        let Some(raw) = full_vbios_bytes() else { return };
        let bios = Bios::new(raw.clone()).unwrap();
        let (desc, _) = offsets(&raw);
        let dmem = 0xe100usize;
        let stored = raw[desc + 0x4ac..desc + 0x4ac + 0xe100 + 0x800].to_vec();
        assert_eq!(&stored[dmem + 0x560..dmem + 0x564], b"DMAP");

        let expect = |init: u32, frts: Option<(u32, u32)>| -> Vec<u8> {
            let mut e = stored.clone();
            e.resize(0xe100 + 0x800, 0);
            let put = |e: &mut Vec<u8>, at: usize, v: u32| e[dmem + at..dmem + at + 4].copy_from_slice(&v.to_le_bytes());
            put(&mut e, 0x560 + 0x2c, init);
            // read_vbios { ver 1, hdr 24, addr 0, size 0, flags 2 }
            for (i, v) in [1u32, 24, 0, 0, 0, 2].into_iter().enumerate() {
                put(&mut e, 0x7c0 + i * 4, v);
            }
            if let Some((a, sz)) = frts {
                // frts_region { ver 1, hdr 20, addr, size, type FB }
                for (i, v) in [1u32, 20, a, sz, 2].into_iter().enumerate() {
                    put(&mut e, 0x7c0 + 24 + i * 4, v);
                }
            }
            e
        };
        let f = Fwsec::build(&bios, Command::Frts { addr: 0x1_ffe0_0000, size: 0x10_0000 }).unwrap();
        assert_eq!(f.image, expect(0x15, Some((0x1ffe00, 0x100))));
        let sb = Fwsec::build(&bios, Command::Sb).unwrap();
        assert_eq!(sb.image, expect(0x19, None));
        // the second application (id 5) is untouched
        assert_eq!(&f.image[dmem + 0x7ac..dmem + 0x7c0], &stored[dmem + 0x7ac..dmem + 0x7c0]);
    }

    #[test]
    fn descriptor_fields_are_read_from_the_right_places() {
        let Some(mut raw) = full_vbios_bytes() else { return };
        let (desc, _) = offsets(&raw);
        // 3 signatures of 384 bytes, right after the fixed 0x2c bytes.
        let bios = Bios::new(raw.clone()).unwrap();
        let fw = Fwsec::build(&bios, Command::Sb).unwrap();
        assert_eq!(fw.sigs, &raw[desc + 0x2c..desc + 0x2c + 3 * 384]);
        assert_eq!(fw.sigs.len(), 3 * SIG_SIZE);
        assert_eq!(SIG_SIZE, 384);
        assert_eq!(fw.fuse_ver, 7);
        // Fields that are 0 or equal on this board, moved to distinct values.
        le32(&mut raw, desc + 0x10, 0x100); // IMEMPhysBase
        le32(&mut raw, desc + 0x1c, 0x40); // DMEMPhysBase
        le32(&mut raw, desc + 0x20, 0x880); // DMEMLoadSize: a multiple of 128, not of 256
        raw[desc + 0x28..desc + 0x2a].copy_from_slice(&0x0107u16.to_le_bytes()); // SignatureVersions > 8 bits
        raw[desc + 0x24..desc + 0x26].copy_from_slice(&0x0400u16.to_le_bytes());
        let fw = Fwsec::build(&Bios::new(raw.clone()).unwrap(), Command::Sb).unwrap();
        assert_eq!((fw.params.imem_base, fw.params.dmem_base), (0x100, 0x40));
        assert_eq!(fw.params.dmem_size, 0x900, "rounded up to 256");
        assert_eq!(fw.image.len(), 0xe100 + 0x900);
        assert_eq!(fw.fuse_ver, 0x107);
        assert_eq!(fw.params.engine_id, 0x400);
        // The fuse register needs the 0x400 engine bit.
        raw[desc + 0x24..desc + 0x26].copy_from_slice(&0x0100u16.to_le_bytes());
        let fw = Fwsec::build(&Bios::new(raw.clone()).unwrap(), Command::Sb).unwrap();
        assert_eq!(fw.fuse_register(), Err(FwsecError::NoSignature));
        assert_eq!(DISP_VGA_REG, 0x625f04);
    }

    #[test]
    fn corrupt_tables_are_refused() {
        let Some(raw) = full_vbios_bytes() else { return };
        let (desc, p) = offsets(&raw);
        let build = |r: &[u8]| Fwsec::build(&Bios::new(r.to_vec()).unwrap(), Command::Sb).map(|_| ());
        // BIT 'p' entry: version 2 and length >= 4, and a non-null table pointer.
        let mut r = raw.clone();
        r[p + 1] = 1;
        assert_eq!(build(&r), Err(FwsecError::NoPmuTable));
        let mut r = raw.clone();
        r[p + 2..p + 4].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(build(&r), Err(FwsecError::NoPmuTable));
        let mut r = raw.clone();
        let off = u16::from_le_bytes([raw[p + 4], raw[p + 5]]) as usize;
        le32(&mut r, off, 0);
        assert_eq!(build(&r), Err(FwsecError::NoPmuTable));
        // Signature location outside the DMEM image.
        let mut r = raw.clone();
        le32(&mut r, desc + 8, 0x10000);
        assert_eq!(build(&r), Err(FwsecError::Truncated));
        // Interface version other than 1, and no DMEM mapper application.
        let iface = desc + 0x4ac + 0xe100 + 0x1c;
        let mut r = raw.clone();
        r[iface] = 2;
        assert_eq!(build(&r), Err(FwsecError::BadInterface));
        let mut r = raw.clone();
        le32(&mut r, iface + 4, 6); // app 0's id 4 -> 6
        assert_eq!(build(&r), Err(FwsecError::BadInterface));
        assert!(build(&raw).is_ok());
    }

    #[test]
    fn pmu_table_header_and_entry_sizes_are_not_confused() {
        // On this board the header and the entries are both 6 bytes; rebuild the
        // table with 12-byte entries so that they differ.
        let Some(mut raw) = full_vbios_bytes() else { return };
        let (_, p) = offsets(&raw);
        let bios = Bios::new(raw.clone()).unwrap();
        let bit_p = bios.bit_entry(b'p').unwrap();
        let table = bios.rd32(bit_p.offset as u32);
        let t = bios.raw_offset(table, 6 + 16 * 6).unwrap();
        assert_eq!((raw[t + 1], raw[t + 2], raw[t + 3]), (6, 6, 16));
        let entries: Vec<[u8; 6]> = (0..16).map(|i| raw[t + 6 + i * 6..t + 12 + i * 6].try_into().unwrap()).collect();
        raw[t + 2] = 12;
        for (i, e) in entries.iter().enumerate() {
            let at = t + 6 + i * 12;
            raw[at..at + 12].fill(0);
            raw[at..at + 6].copy_from_slice(e);
        }
        let want = find_descriptor(&bios).unwrap();
        assert_eq!(find_descriptor(&Bios::new(raw).unwrap()), Ok(want));
        let _ = p;
    }

    #[test]
    fn sb_verifies_its_own_register_and_half() {
        let Some(bios) = full_vbios() else { return };
        let mut fw = Fwsec::build(&bios, Command::Sb).unwrap();
        // The fixture is FRTS's: give it the SB error register too (0x1454, low half).
        let with = |v: &str| std::format!("{}\nR 0x001454 {v}\n", FRTS_FIXTURE);
        let run = |v: &str, fw: &mut Fwsec| {
            let m = replay_of(&with(v));
            let idx = prepare(&m, &GSP, fw).unwrap();
            execute(&m, &GSP, fw, TRACE_DMA_ADDR, idx)
        };
        assert!(run("0x00000000", &mut fw).is_ok());
        assert_eq!(run("0x00000005", &mut fw), Err(FwsecError::Sb(5)));
        assert!(run("0x00050000", &mut fw).is_ok(), "the upper half is not SB's error");
        assert_eq!(SB_ERR, 0x1454);
    }

    #[test]
    fn frts_region_rounds_the_workspace_down_to_128k() {
        // A workspace address that is not 128 KiB aligned (the register has 64 KiB granularity).
        let base = FB - 0x10_0000;
        let reg = (((base + 0x1_0000) >> 8) as u32 & 0xffff_ff00) | 8;
        assert_eq!(frts_region(FB, reg).0, base - 0x10_0000);
        // The fallback is the last 128 KiB, and FRTS is 1 MiB below it.
        let reg = (((base - 0x1_0000) >> 8) as u32 & 0xffff_ff00) | 8;
        assert_eq!(frts_region(FB, reg), (FB - 0x2_0000 - 0x10_0000, 0x10_0000));
    }

    #[test]
    fn a_vbios_without_the_pmu_table_has_no_fwsec() {
        let bios = Bios::new(crate::vbios::tests::tiny_image()).unwrap();
        assert_eq!(find_descriptor(&bios), Err(FwsecError::NoPmuTable));
        assert!(Fwsec::build(&bios, Command::Sb).is_err());
    }

    // silence the unused import when the oracle VBIOS is absent
    #[allow(dead_code)]
    fn _uses(_: LoadParams) {}
}
