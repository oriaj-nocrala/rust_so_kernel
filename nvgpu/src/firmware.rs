//! Parsers of the GSP firmware files (phase 4b): the booters, the RISC-V
//! bootloader and the GSP-RM ELF. Pure: they take the bytes of a file and
//! return offsets, sizes and slices into it; nothing is copied, patched or
//! sent to the GPU here.
//!
//! The pinned firmware is 570.144 (`docs/gpu/gpu-plan.md` D1) for the
//! GA106, whose nouveau path is `ga102_gsps` (`nvkm/subdev/gsp/ga102.c:178`).
//! Paths are relative to `drivers/gpu/drm/nouveau/` in Linux v7.2.2 unless
//! they say `open-gpu-kernel-modules`.
//!
//! Formats (measured on the real files, see the tests):
//! - `booter_load-*.bin`, `booter_unload-*.bin` and `bootloader-*.bin` start
//!   with `nvfw_bin_hdr` (`include/nvfw/fw.h:8-15`); NVIDIA's
//!   `nouveau/extract-firmware-nouveau.py` writes them.
//! - a booter's header is `nvfw_hs_header_v2` followed by
//!   `nvfw_hs_load_header_v2` (`include/nvfw/hs.h`); the signature patch
//!   location, the signature offset and the signature count are not in the
//!   header but in words the header points to.
//! - the bootloader's header is `RM_RISCV_UCODE_DESC`
//!   (`open-gpu-kernel-modules/src/nvidia/arch/nvalloc/common/inc/rmRiscvUcode.h:35-87`).
//! - `gsp-*.bin` is an ELF64: `.fwimage` is the image the booter loads and
//!   `.fwsignature_<arch>` its signature (`rm/r535/gsp.c:1849-1867`).

/// `nvfw_bin_hdr.bin_magic` (`nvkm/falcon/fw.c:243-256`, the `0x10de` case;
/// the other magic is for older nouveau-only firmware).
pub const BIN_MAGIC: u32 = 0x0000_10de;

/// ELF section holding the GSP-RM signature on GA10x
/// (`nvkm/subdev/gsp/ga102.c:157`).
pub const SIG_SECTION_GA10X: &str = ".fwsignature_ga10x";

/// Fuse version registers of a signed falcon image, by engine id bit:
/// `base + (ucode_id - 1) * 4` (`nvkm/falcon/ga100.c:38-46`). The booters
/// have engine id 1 (`booter_load` reads `0x824148` in `trace-gsp` at
/// 8,6989 s); FWSEC has 0x400 (`ga102.c:106-107`; it reads `0x8241e0`).
pub const FUSE_BASES: [(u32, u32); 3] = [(0x0000_0001, 0x82_4140), (0x0000_0004, 0x82_4100), (0x0000_0400, 0x82_41c0)];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FwError {
    /// A header, table or section reaches past the end of the file.
    Truncated,
    /// `nvfw_bin_hdr.bin_magic` is not [`BIN_MAGIC`].
    BadMagic(u32),
    /// The file is not a little-endian ELF64 with 64-byte section headers.
    NotElf,
    /// An ELF section name is not there.
    NoSection,
    /// Zero signatures, or a signature that does not divide evenly / fit.
    BadSignatures,
    /// The fuse version is newer than the signatures (`ga100.c:55`, `ga102.c:116`).
    FuseMismatch,
}

fn rd32(d: &[u8], at: usize) -> Result<u32, FwError> {
    let b = d.get(at..at.checked_add(4).ok_or(FwError::Truncated)?).ok_or(FwError::Truncated)?;
    Ok(u32::from_le_bytes(b.try_into().unwrap()))
}
fn rd16(d: &[u8], at: usize) -> Result<u16, FwError> {
    let b = d.get(at..at.checked_add(2).ok_or(FwError::Truncated)?).ok_or(FwError::Truncated)?;
    Ok(u16::from_le_bytes(b.try_into().unwrap()))
}
fn rd64(d: &[u8], at: usize) -> Result<u64, FwError> {
    let b = d.get(at..at.checked_add(8).ok_or(FwError::Truncated)?).ok_or(FwError::Truncated)?;
    Ok(u64::from_le_bytes(b.try_into().unwrap()))
}
fn slice(d: &[u8], at: u64, len: u64) -> Result<&[u8], FwError> {
    let end = at.checked_add(len).ok_or(FwError::Truncated)?;
    let (at, end) = (usize::try_from(at).map_err(|_| FwError::Truncated)?, usize::try_from(end).map_err(|_| FwError::Truncated)?);
    d.get(at..end).ok_or(FwError::Truncated)
}

/// `struct nvfw_bin_hdr` (`include/nvfw/fw.h:8-15`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BinHdr {
    pub bin_ver: u32,
    pub bin_size: u32,
    pub header_offset: u32,
    pub data_offset: u32,
    pub data_size: u32,
}

impl BinHdr {
    pub fn parse(blob: &[u8]) -> Result<Self, FwError> {
        let magic = rd32(blob, 0)?;
        if magic != BIN_MAGIC {
            return Err(FwError::BadMagic(magic));
        }
        let h = BinHdr {
            bin_ver: rd32(blob, 4)?,
            bin_size: rd32(blob, 8)?,
            header_offset: rd32(blob, 12)?,
            data_offset: rd32(blob, 16)?,
            data_size: rd32(blob, 20)?,
        };
        // The image the DMA reads must be all in the file.
        slice(blob, h.data_offset as u64, h.data_size as u64)?;
        Ok(h)
    }

    /// The payload (`blob->data + data_offset`, `data_size` bytes).
    pub fn data<'a>(&self, blob: &'a [u8]) -> &'a [u8] {
        &blob[self.data_offset as usize..(self.data_offset as usize + self.data_size as usize)]
    }
}

/// `struct nvfw_hs_header_v2` (`include/nvfw/hs.h:24-34`), fields as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HsHeaderV2 {
    pub sig_prod_offset: u32,
    pub sig_prod_size: u32,
    /// Offset of the word holding the signature patch location.
    pub patch_loc: u32,
    /// Offset of the word holding the signature offset.
    pub patch_sig: u32,
    pub meta_data_offset: u32,
    pub meta_data_size: u32,
    /// Offset of the word holding the signature count.
    pub num_sig: u32,
    pub header_offset: u32,
    pub header_size: u32,
}

impl HsHeaderV2 {
    pub fn parse(blob: &[u8], at: u32) -> Result<Self, FwError> {
        let a = at as usize;
        Ok(HsHeaderV2 {
            sig_prod_offset: rd32(blob, a)?,
            sig_prod_size: rd32(blob, a + 4)?,
            patch_loc: rd32(blob, a + 8)?,
            patch_sig: rd32(blob, a + 12)?,
            meta_data_offset: rd32(blob, a + 16)?,
            meta_data_size: rd32(blob, a + 20)?,
            num_sig: rd32(blob, a + 24)?,
            header_offset: rd32(blob, a + 28)?,
            header_size: rd32(blob, a + 32)?,
        })
    }
}

/// `struct nvfw_hs_load_header_v2` (`include/nvfw/hs.h:51-62`); only the
/// first application is used (`ga102.c:72-81`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HsLoadHeaderV2 {
    pub os_code_offset: u32,
    pub os_code_size: u32,
    pub os_data_offset: u32,
    pub os_data_size: u32,
    pub num_apps: u32,
    pub app0_offset: u32,
    pub app0_size: u32,
    pub app0_data_offset: u32,
    pub app0_data_size: u32,
}

impl HsLoadHeaderV2 {
    pub fn parse(blob: &[u8], at: u32) -> Result<Self, FwError> {
        let a = at as usize;
        let num_apps = rd32(blob, a + 16)?;
        if num_apps == 0 {
            return Err(FwError::Truncated);
        }
        Ok(HsLoadHeaderV2 {
            os_code_offset: rd32(blob, a)?,
            os_code_size: rd32(blob, a + 4)?,
            os_data_offset: rd32(blob, a + 8)?,
            os_data_size: rd32(blob, a + 12)?,
            num_apps,
            app0_offset: rd32(blob, a + 20)?,
            app0_size: rd32(blob, a + 24)?,
            app0_data_offset: rd32(blob, a + 28)?,
            app0_data_size: rd32(blob, a + 32)?,
        })
    }
}

/// A booter (`booter_load` / `booter_unload`) read the way
/// `ga102_gsp_booter_ctor` does it (`ga102.c:41-92`): the image to DMA, the
/// signatures to patch into it, and where each part goes in the falcon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Booter<'a> {
    /// `blob + data_offset`, `data_size` bytes: the image the falcon DMA reads.
    pub image: &'a [u8],
    /// `num_sigs` signatures of `sig_size` bytes each, back to back.
    sigs: &'a [u8],
    pub num_sigs: u32,
    pub sig_size: u32,
    /// Offset in `image` where the chosen signature is written (`fw->sig_base_img`).
    pub patch_loc: u32,
    /// `meta[0]`: the newest fuse version the signatures are for.
    pub fuse_ver: u32,
    /// `meta[1]`, `meta[2]` (`ga102.c:84-85`).
    pub engine_id: u32,
    pub ucode_id: u32,
    /// IMEM: offset in `image`, size (`ga102.c:72-74`); loaded at IMEM 0.
    pub imem_base_img: u32,
    pub imem_size: u32,
    /// DMEM: offset in `image`, size (`ga102.c:76-78`); loaded at DMEM 0.
    pub dmem_base_img: u32,
    pub dmem_size: u32,
    /// Where in the DMEM data the signature lands (`ga102.c:79`).
    pub dmem_sign: u32,
    /// Boot address (`ga102.c:81`).
    pub boot_addr: u32,
}

impl<'a> Booter<'a> {
    pub fn parse(blob: &'a [u8]) -> Result<Self, FwError> {
        let hdr = BinHdr::parse(blob)?;
        let hs = HsHeaderV2::parse(blob, hdr.header_offset)?;
        let loc = rd32(blob, hs.patch_loc as usize)?;
        let sig = rd32(blob, hs.patch_sig as usize)?;
        let cnt = rd32(blob, hs.num_sig as usize)?;
        let lh = HsLoadHeaderV2::parse(blob, hs.header_offset)?;
        let image = hdr.data(blob);

        if cnt == 0 || hs.sig_prod_size % cnt != 0 {
            return Err(FwError::BadSignatures);
        }
        let sig_size = hs.sig_prod_size / cnt;
        let sigs = slice(blob, hs.sig_prod_offset as u64 + sig as u64, hs.sig_prod_size as u64)?;
        // The signature has to fit where it is written, and be whole words.
        if sig_size % 4 != 0 || loc as u64 + sig_size as u64 > image.len() as u64 {
            return Err(FwError::BadSignatures);
        }
        // The three meta words (`ga102.c:83-85`).
        if hs.meta_data_size < 12 {
            return Err(FwError::Truncated);
        }
        let m = hs.meta_data_offset as usize;
        let (fuse_ver, engine_id, ucode_id) = (rd32(blob, m)?, rd32(blob, m + 4)?, rd32(blob, m + 8)?);

        // The IMEM and DMEM ranges the loader will copy must be in the image.
        slice(image, lh.app0_offset as u64, lh.app0_size as u64)?;
        slice(image, lh.os_data_offset as u64, lh.os_data_size as u64)?;
        if loc < lh.os_data_offset {
            return Err(FwError::BadSignatures);
        }

        Ok(Booter {
            image,
            sigs,
            num_sigs: cnt,
            sig_size,
            patch_loc: loc,
            fuse_ver,
            engine_id,
            ucode_id,
            imem_base_img: lh.app0_offset,
            imem_size: lh.app0_size,
            dmem_base_img: lh.os_data_offset,
            dmem_size: lh.os_data_size,
            dmem_sign: loc - lh.os_data_offset,
            boot_addr: lh.app0_offset,
        })
    }

    /// Signature number `idx`, as stored.
    pub fn signature(&self, idx: usize) -> Option<&'a [u8]> {
        let s = self.sig_size as usize;
        let start = idx.checked_mul(s)?;
        self.sigs.get(start..start.checked_add(s)?)
    }

    /// The register holding the fuse version this image is checked
    /// against (`ga100_flcn_fw_signature`, `ga100.c:38-46`); `None` when the
    /// engine id has none of the known bits (nouveau `WARN`s, `-ENOSYS`).
    pub fn fuse_register(&self) -> Option<u32> {
        let (_, base) = FUSE_BASES.iter().find(|(bit, _)| self.engine_id & bit != 0)?;
        Some(base.wrapping_add(self.ucode_id.wrapping_sub(1).wrapping_mul(4)))
    }

    /// Which signature to patch in, given the fuse register's value
    /// (`ga100.c:48-62`): a burnt fuse version `v` (its highest set bit)
    /// selects `fuse_ver - v`; a register of 0 the last signature.
    pub fn signature_index(&self, fuse_reg: u32) -> Result<usize, FwError> {
        let idx = if fuse_reg != 0 {
            let fls = 32 - fuse_reg.leading_zeros();
            // A fuse newer than the signatures wraps to a huge index.
            self.fuse_ver.wrapping_sub(fls) as usize
        } else {
            self.num_sigs as usize - 1
        };
        // nouveau reads past the signatures here (`ga100.c:55` only checks
        // the version); the port refuses.
        if idx >= self.num_sigs as usize {
            return Err(FwError::FuseMismatch);
        }
        Ok(idx)
    }

    /// Write signature `idx` over the image copy `dst` (`nvkm_falcon_fw_patch`,
    /// `nvkm/falcon/fw.c:30-64`). `dst` is a copy of [`Booter::image`].
    pub fn patch(&self, dst: &mut [u8], idx: usize) -> Result<(), FwError> {
        let sig = self.signature(idx).ok_or(FwError::BadSignatures)?;
        let at = self.patch_loc as usize;
        dst.get_mut(at..at + sig.len()).ok_or(FwError::Truncated)?.copy_from_slice(sig);
        Ok(())
    }
}

/// FWSEC's variant (phase 4c), not the booters': `ga102_gsp_fwsec_signature`
/// after the register read (`ga102.c:114-125`). `sig_fuse_version` is the
/// signature set's bit mask (`fw->fuse_ver` of the VBIOS image), `fuse_reg`
/// the raw register (`trace-gsp`: `0x8241e0 = 3`). The register is turned
/// into one bit (`BIT(fls(reg))`, the highest fuse version burnt, plus one)
/// and the index is how many signature versions below it the set has.
pub fn fwsec_signature_index(sig_fuse_version: u32, fuse_reg: u32) -> Result<usize, FwError> {
    // fls(): 1-based position of the highest set bit, 0 for 0. BIT(32) does
    // not exist in 32 bits: no signature can match then.
    let fls = 32 - fuse_reg.leading_zeros();
    let mut reg = 1u32.checked_shl(fls).ok_or(FwError::FuseMismatch)?;
    if reg & sig_fuse_version == 0 {
        return Err(FwError::FuseMismatch);
    }
    let mut sig = sig_fuse_version;
    let mut idx = 0;
    // Terminates: `reg` has one bit and it is also in `sig`.
    while reg & sig & 1 == 0 {
        idx += (sig & 1) as usize;
        reg >>= 1;
        sig >>= 1;
    }
    Ok(idx)
}

/// `RM_RISCV_UCODE_DESC` (`rmRiscvUcode.h:35-87`), all 21 words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiscvUcodeDesc {
    pub version: u32,
    pub bootloader_offset: u32,
    pub bootloader_size: u32,
    pub bootloader_param_offset: u32,
    pub bootloader_param_size: u32,
    pub riscv_elf_offset: u32,
    pub riscv_elf_size: u32,
    pub app_version: u32,
    pub manifest_offset: u32,
    pub manifest_size: u32,
    pub monitor_data_offset: u32,
    pub monitor_data_size: u32,
    pub monitor_code_offset: u32,
    pub monitor_code_size: u32,
    pub is_monitor_enabled: u32,
    pub swbrom_code_offset: u32,
    pub swbrom_code_size: u32,
    pub swbrom_data_offset: u32,
    pub swbrom_data_size: u32,
    pub fb_reserved_size: u32,
    pub signed_as_code: u32,
}

/// Size of [`RiscvUcodeDesc`] in the file.
pub const RISCV_DESC_SIZE: usize = 21 * 4;

/// The GSP-RM bootloader (`bootloader-*.bin`): the descriptor and the image
/// copied to the WPR2 boot area (`r535_gsp_rm_boot_ctor`, `rm/r535/gsp.c:1813-1842`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bootloader<'a> {
    pub desc: RiscvUcodeDesc,
    /// `blob + data_offset`, `data_size` bytes (`gsp->boot.fw`).
    pub image: &'a [u8],
}

impl<'a> Bootloader<'a> {
    pub fn parse(blob: &'a [u8]) -> Result<Self, FwError> {
        let hdr = BinHdr::parse(blob)?;
        let a = hdr.header_offset as usize;
        let w = |i: usize| rd32(blob, a + i * 4);
        let desc = RiscvUcodeDesc {
            version: w(0)?,
            bootloader_offset: w(1)?,
            bootloader_size: w(2)?,
            bootloader_param_offset: w(3)?,
            bootloader_param_size: w(4)?,
            riscv_elf_offset: w(5)?,
            riscv_elf_size: w(6)?,
            app_version: w(7)?,
            manifest_offset: w(8)?,
            manifest_size: w(9)?,
            monitor_data_offset: w(10)?,
            monitor_data_size: w(11)?,
            monitor_code_offset: w(12)?,
            monitor_code_size: w(13)?,
            is_monitor_enabled: w(14)?,
            swbrom_code_offset: w(15)?,
            swbrom_code_size: w(16)?,
            swbrom_data_offset: w(17)?,
            swbrom_data_size: w(18)?,
            fb_reserved_size: w(19)?,
            signed_as_code: w(20)?,
        };
        Ok(Bootloader { desc, image: hdr.data(blob) })
    }
}

/// The named section of a little-endian ELF64 (`r535_gsp_elf_section`,
/// `rm/r535/gsp.c:1849-1867`), bounds-checked where nouveau trusts the file.
pub fn elf_section<'a>(img: &'a [u8], name: &str) -> Result<&'a [u8], FwError> {
    if img.get(..4) != Some(b"\x7fELF") || img.get(4) != Some(&2) || img.get(5) != Some(&1) {
        return Err(FwError::NotElf);
    }
    let shoff = rd64(img, 0x28)?;
    let shentsize = rd16(img, 0x3a)? as u64;
    let shnum = rd16(img, 0x3c)? as u64;
    let shstrndx = rd16(img, 0x3e)? as u64;
    if shentsize != 64 || shstrndx >= shnum {
        return Err(FwError::NotElf);
    }
    let sh = |i: u64| -> Result<&[u8], FwError> {
        slice(img, shoff.checked_add(i * shentsize).ok_or(FwError::Truncated)?, shentsize)
    };
    let strs = sh(shstrndx)?;
    let names = slice(img, rd64(strs, 0x18)?, rd64(strs, 0x20)?)?;
    for i in 0..shnum {
        let s = sh(i)?;
        let at = rd32(s, 0)? as usize;
        let Some(tail) = names.get(at..) else { continue };
        let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
        if &tail[..end] == name.as_bytes() {
            return slice(img, rd64(s, 0x18)?, rd64(s, 0x20)?);
        }
    }
    Err(FwError::NoSection)
}

/// The GSP-RM ELF: what `r535_gsp_oneinit` takes from it (`rm/r535/gsp.c:2143-2161`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GspImage<'a> {
    /// `.fwimage`: the firmware the radix3 table maps (`gsp->fw`).
    pub fwimage: &'a [u8],
    /// The architecture's signature section (`gsp->sig`).
    pub signature: &'a [u8],
}

impl<'a> GspImage<'a> {
    pub fn parse(elf: &'a [u8], sig_section: &str) -> Result<Self, FwError> {
        Ok(GspImage { fwimage: elf_section(elf, ".fwimage")?, signature: elf_section(elf, sig_section)? })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;
    use std::vec::Vec;

    /// A pinned firmware file (D3: not in git). The small ones are read
    /// from `disk-image-root/lib/firmware/` (where the root build puts
    /// them); else, and for the 63 MB ELF, `zstd -dc` of the host's
    /// `/usr/lib/firmware`. `None` (and a note) when neither is there.
    pub(crate) fn firmware(name: &str) -> Option<Vec<u8>> {
        let rel = "nvidia/ga106/gsp";
        let local = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../disk-image-root/lib/firmware").join(rel).join(name);
        if let Ok(d) = std::fs::read(&local) {
            return Some(d);
        }
        let z = PathBuf::from("/usr/lib/firmware").join(rel).join(std::format!("{name}.zst"));
        match Command::new("zstd").arg("-dc").arg(&z).output() {
            Ok(o) if o.status.success() => Some(o.stdout),
            _ => {
                std::eprintln!("SKIP: no firmware {name} ({}); see docs/gpu/gpu-plan.md D3", local.display());
                None
            }
        }
    }

    // ---- synthetic files -------------------------------------------------

    fn put(d: &mut [u8], at: usize, w: u32) {
        d[at..at + 4].copy_from_slice(&w.to_le_bytes());
    }

    /// A booter shaped like the real ones, small: header 0x18, hs header
    /// 0x18.., signatures 2 x 16 B, meta, load header, data at 0x100.
    fn booter_blob() -> Vec<u8> {
        let mut d = std::vec![0u8; 0x300];
        // nvfw_bin_hdr: magic, ver, size, header_offset, data_offset, data_size
        for (i, w) in [BIN_MAGIC, 1, 0x300, 0x18, 0x100, 0x200].into_iter().enumerate() {
            put(&mut d, i * 4, w);
        }
        // hs_header_v2 at 0x18: sig_prod (0x40, 0x20), patch_loc word @0x60,
        // patch_sig word @0x64, meta (0x68, 0xc), num_sig word @0x74,
        // load header (0x78, 0x24)
        for (i, w) in [0x40, 0x20, 0x60, 0x64, 0x68, 0xc, 0x74, 0x78, 0x24].into_iter().enumerate() {
            put(&mut d, 0x18 + i * 4, w);
        }
        for i in 0..0x20 {
            d[0x40 + i] = i as u8; // signatures: 0..15, 16..31
        }
        put(&mut d, 0x60, 0x80); // patch_loc = 0x80 in the image
        put(&mut d, 0x64, 0); // sig offset 0
        for (i, w) in [2, 0x400, 3].into_iter().enumerate() {
            put(&mut d, 0x68 + i * 4, w); // fuse_ver, engine_id, ucode_id
        }
        put(&mut d, 0x74, 2); // num_sig
        // load header at 0x78: os_code 0/0x20, os_data 0x40/0x100, 1 app (0x20, 0x20, 0, 0)
        for (i, w) in [0, 0x20, 0x40, 0x100, 1, 0x20, 0x20, 0, 0].into_iter().enumerate() {
            put(&mut d, 0x78 + i * 4, w);
        }
        d
    }

    #[test]
    fn booter_synthetic() {
        let blob = booter_blob();
        let b = Booter::parse(&blob).unwrap();
        assert_eq!(b.image.len(), 0x200);
        assert_eq!((b.num_sigs, b.sig_size, b.patch_loc), (2, 16, 0x80));
        assert_eq!((b.fuse_ver, b.engine_id, b.ucode_id), (2, 0x400, 3));
        assert_eq!((b.imem_base_img, b.imem_size), (0x20, 0x20));
        assert_eq!((b.dmem_base_img, b.dmem_size, b.dmem_sign), (0x40, 0x100, 0x40));
        assert_eq!(b.boot_addr, 0x20);
        assert_eq!(b.signature(0).unwrap(), &(0..16).collect::<Vec<u8>>()[..]);
        assert_eq!(b.signature(1).unwrap(), &(16..32).collect::<Vec<u8>>()[..]);
        assert_eq!(b.signature(2), None);
        assert_eq!(b.signature(usize::MAX), None);
        assert_eq!(b.signature(usize::MAX / 16), None); // start + 16 would wrap
        // engine 0x400, ucode 3 -> the third register of that bank
        assert_eq!(b.fuse_register(), Some(0x8241c8));
        // patch writes signature 1 at 0x80 and nothing else
        let mut img = b.image.to_vec();
        b.patch(&mut img, 1).unwrap();
        assert_eq!(&img[0x80..0x90], b.signature(1).unwrap());
        assert_eq!(&img[..0x80], &b.image[..0x80]);
        assert_eq!(&img[0x90..], &b.image[0x90..]);
        assert_eq!(b.patch(&mut img, 2), Err(FwError::BadSignatures));
    }

    #[test]
    fn booter_rejects_bad_files() {
        let good = booter_blob();
        // wrong magic
        let mut d = good.clone();
        put(&mut d, 0, 0x3b1d14f0);
        assert_eq!(Booter::parse(&d), Err(FwError::BadMagic(0x3b1d14f0)));
        // payload past the end of the file
        let mut d = good.clone();
        put(&mut d, 20, 0x201);
        assert_eq!(Booter::parse(&d), Err(FwError::Truncated));
        // file cut in the middle of the hs header
        assert_eq!(Booter::parse(&good[..0x20]), Err(FwError::Truncated));
        assert_eq!(Booter::parse(&[]), Err(FwError::Truncated));
        // zero signatures, and a size that does not divide
        let mut d = good.clone();
        put(&mut d, 0x74, 0);
        assert_eq!(Booter::parse(&d), Err(FwError::BadSignatures));
        let mut d = good.clone();
        put(&mut d, 0x74, 3);
        assert_eq!(Booter::parse(&d), Err(FwError::BadSignatures));
        // signatures not in the file
        let mut d = good.clone();
        put(&mut d, 0x64, 0x300);
        assert_eq!(Booter::parse(&d), Err(FwError::Truncated));
        // patch location so late the signature would not fit the image
        let mut d = good.clone();
        put(&mut d, 0x60, 0x1f8);
        assert_eq!(Booter::parse(&d), Err(FwError::BadSignatures));
        // patch location before the DMEM data
        let mut d = good.clone();
        put(&mut d, 0x60, 0x20);
        assert_eq!(Booter::parse(&d), Err(FwError::BadSignatures));
        // IMEM range outside the image
        let mut d = good.clone();
        put(&mut d, 0x78 + 5 * 4, 0x1f0);
        assert_eq!(Booter::parse(&d), Err(FwError::Truncated));
        // signature block that is not a whole number per signature
        let mut d = good.clone();
        put(&mut d, 0x18 + 4, 0x21); // 2 signatures in 0x21 bytes
        assert_eq!(Booter::parse(&d), Err(FwError::BadSignatures));
        // signatures that are not whole words (0x1e / 2 = 15)
        let mut d = good.clone();
        put(&mut d, 0x18 + 4, 0x1e);
        assert_eq!(Booter::parse(&d), Err(FwError::BadSignatures));
        // DMEM data range outside the image (loc 0x80 is inside it)
        let mut d = good.clone();
        put(&mut d, 0x78 + 3 * 4, 0x1f1); // os_data_offset 0x40, size 0x1f1
        assert_eq!(Booter::parse(&d), Err(FwError::Truncated));
        // meta too short
        let mut d = good.clone();
        put(&mut d, 0x18 + 5 * 4, 8);
        assert_eq!(Booter::parse(&d), Err(FwError::Truncated));
        // no application in the load header
        let mut d = good;
        put(&mut d, 0x78 + 4 * 4, 0);
        assert_eq!(Booter::parse(&d), Err(FwError::Truncated));
    }

    #[test]
    fn fuse_register_by_engine_bit() {
        let blob = booter_blob();
        let mut b = Booter::parse(&blob).unwrap();
        b.ucode_id = 3;
        for (engine, reg) in [(0x1, 0x824148), (0x4, 0x824108), (0x400, 0x8241c8), (0x401, 0x824148)] {
            b.engine_id = engine;
            assert_eq!(b.fuse_register(), Some(reg), "engine {engine:#x}");
        }
        b.engine_id = 0x2;
        assert_eq!(b.fuse_register(), None);
        b.engine_id = 0;
        assert_eq!(b.fuse_register(), None);
        b.engine_id = 0x400;
        b.ucode_id = 1;
        assert_eq!(b.fuse_register(), Some(0x8241c0));
    }

    #[test]
    fn signature_index_is_fuse_ver_minus_burnt_version() {
        let blob = booter_blob(); // fuse_ver 2, two signatures
        let b = Booter::parse(&blob).unwrap();
        // Nothing burnt: the last signature.
        assert_eq!(b.signature_index(0), Ok(1));
        // Version 1 burnt -> 2 - 1; version 2 -> 0 (only the highest bit counts).
        assert_eq!(b.signature_index(0b1), Ok(1));
        assert_eq!(b.signature_index(0b11), Ok(0));
        assert_eq!(b.signature_index(0b10), Ok(0));
        // Fuse newer than the signatures.
        assert_eq!(b.signature_index(0b100), Err(FwError::FuseMismatch));
        assert_eq!(b.signature_index(0x8000_0000), Err(FwError::FuseMismatch));
    }

    #[test]
    fn signature_index_never_leaves_the_signatures() {
        let blob = booter_blob();
        let mut b = Booter::parse(&blob).unwrap();
        // A file whose fuse_ver is past its signature count: nouveau would
        // read beyond them, the port refuses.
        b.fuse_ver = 5;
        assert_eq!(b.signature_index(0b1), Err(FwError::FuseMismatch)); // 5 - 1 = 4 >= 2
        assert_eq!(b.signature_index(0b10000), Ok(0)); // 5 - 5
    }

    #[test]
    fn fwsec_signature_follows_the_fuse_walk() {
        // `ga102.c:114-125`. Signatures for fuse versions 1 and 2 (mask 0b11).
        // Register 0 (nothing burnt): BIT(fls 0) = 1 -> first signature.
        assert_eq!(fwsec_signature_index(0b11, 0), Ok(0));
        // Register 1: BIT(fls 1) = 2 -> the second version -> index 1.
        assert_eq!(fwsec_signature_index(0b11, 1), Ok(1));
        // A set with a gap: versions 1 and 3 (0b101): register 3 -> BIT(2)=4 -> index 1.
        assert_eq!(fwsec_signature_index(0b101, 0b11), Ok(1));
        assert_eq!(fwsec_signature_index(0b101, 0), Ok(0));
        // Skipped version: register 1 -> BIT(1) = 2, not in 0b101.
        assert_eq!(fwsec_signature_index(0b101, 1), Err(FwError::FuseMismatch));
        // Fuse newer than every signature.
        assert_eq!(fwsec_signature_index(0b11, 0b111), Err(FwError::FuseMismatch));
        // fls = 32 has no BIT.
        assert_eq!(fwsec_signature_index(0xffff_ffff, 0x8000_0000), Err(FwError::FuseMismatch));
        // Only the highest burnt bit counts.
        assert_eq!(fwsec_signature_index(0b1000, 0b100), Ok(0));
        assert_eq!(fwsec_signature_index(0b1100, 0b100), Ok(1));
        // The Ryzen (`trace-gsp`: 0x8241e0 = 3): BIT(fls 3) = 4.
        assert_eq!(fwsec_signature_index(0b100, 3), Ok(0));
        assert_eq!(fwsec_signature_index(0b110, 3), Ok(1));
    }

    #[test]
    fn bootloader_synthetic() {
        let mut d = std::vec![0u8; 0x100];
        for (i, w) in [BIN_MAGIC, 1, 0x100, 0x18, 0x80, 0x40].into_iter().enumerate() {
            put(&mut d, i * 4, w);
        }
        for i in 0..21 {
            put(&mut d, 0x18 + i * 4, 100 + i as u32);
        }
        let b = Bootloader::parse(&d).unwrap();
        assert_eq!(b.image.len(), 0x40);
        assert_eq!(b.desc.version, 100);
        assert_eq!(b.desc.app_version, 107);
        assert_eq!((b.desc.manifest_offset, b.desc.monitor_data_offset, b.desc.monitor_code_offset), (108, 110, 112));
        assert_eq!(b.desc.fb_reserved_size, 119);
        assert_eq!(b.desc.signed_as_code, 120);
        // the descriptor must be in the file
        d.truncate(0x18 + 20 * 4);
        assert_eq!(Bootloader::parse(&d), Err(FwError::Truncated));
    }

    /// A minimal ELF64 with the given named sections (contents appended after the headers).
    fn elf(sections: &[(&str, &[u8])]) -> Vec<u8> {
        let mut strtab = std::vec![0u8];
        let mut name_at = Vec::new();
        for (n, _) in sections {
            name_at.push(strtab.len() as u32);
            strtab.extend_from_slice(n.as_bytes());
            strtab.push(0);
        }
        let shstr_name = strtab.len() as u32;
        strtab.extend_from_slice(b".shstrtab\0");
        let nsec = sections.len() + 2; // null + sections + shstrtab
        let mut d = std::vec![0u8; 64 + nsec * 64];
        d[..4].copy_from_slice(b"\x7fELF");
        d[4] = 2;
        d[5] = 1;
        d[0x28..0x30].copy_from_slice(&64u64.to_le_bytes());
        d[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
        d[0x3c..0x3e].copy_from_slice(&(nsec as u16).to_le_bytes());
        d[0x3e..0x40].copy_from_slice(&((nsec - 1) as u16).to_le_bytes());
        let add = |d: &mut Vec<u8>, i: usize, name: u32, body: &[u8]| {
            let off = d.len() as u64;
            d.extend_from_slice(body);
            let h = 64 + i * 64;
            d[h..h + 4].copy_from_slice(&name.to_le_bytes());
            d[h + 0x18..h + 0x20].copy_from_slice(&off.to_le_bytes());
            d[h + 0x20..h + 0x28].copy_from_slice(&(body.len() as u64).to_le_bytes());
        };
        for (i, (_, body)) in sections.iter().enumerate() {
            add(&mut d, i + 1, name_at[i], body);
        }
        add(&mut d, nsec - 1, shstr_name, &strtab);
        d
    }

    #[test]
    fn elf_sections_by_name() {
        let img = elf(&[(".fwimage", b"IMAGE"), (".fwsignature_ga10x", b"SIG"), (".fwsignature_gh100", b"OTHER")]);
        assert_eq!(elf_section(&img, ".fwimage").unwrap(), b"IMAGE");
        assert_eq!(elf_section(&img, ".fwsignature_ga10x").unwrap(), b"SIG");
        assert_eq!(elf_section(&img, ".fwsignature_gh100").unwrap(), b"OTHER");
        assert_eq!(elf_section(&img, ".fwsignature_ad10x"), Err(FwError::NoSection));
        // a prefix is not the name
        assert_eq!(elf_section(&img, ".fwsignature"), Err(FwError::NoSection));
        let g = GspImage::parse(&img, SIG_SECTION_GA10X).unwrap();
        assert_eq!((g.fwimage, g.signature), (&b"IMAGE"[..], &b"SIG"[..]));
        assert_eq!(GspImage::parse(&img, ".fwsignature_zz"), Err(FwError::NoSection));
    }

    #[test]
    fn elf_rejects_bad_files() {
        let good = elf(&[(".fwimage", b"IMAGE")]);
        assert_eq!(elf_section(&[], ".fwimage"), Err(FwError::NotElf));
        assert_eq!(elf_section(b"MZ\x90\0", ".fwimage"), Err(FwError::NotElf));
        let mut d = good.clone();
        d[4] = 1; // ELFCLASS32
        assert_eq!(elf_section(&d, ".fwimage"), Err(FwError::NotElf));
        let mut d = good.clone();
        d[5] = 2; // big endian
        assert_eq!(elf_section(&d, ".fwimage"), Err(FwError::NotElf));
        let mut d = good.clone();
        d[0x3a] = 32; // section header size
        assert_eq!(elf_section(&d, ".fwimage"), Err(FwError::NotElf));
        let mut d = good.clone();
        d[0x3e] = 9; // shstrndx past shnum
        assert_eq!(elf_section(&d, ".fwimage"), Err(FwError::NotElf));
        // section headers past the end
        assert_eq!(elf_section(&good[..100], ".fwimage"), Err(FwError::Truncated));
        // section body past the end
        let mut d = good.clone();
        let h = 64 + 64;
        d[h + 0x20..h + 0x28].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(elf_section(&d, ".fwimage"), Err(FwError::Truncated));
        // section header table offset overflowing
        let mut d = good;
        d[0x28..0x30].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(elf_section(&d, ".fwimage"), Err(FwError::Truncated));
    }

    // ---- the real files (measured with a script, 2026-09-29) ------------

    #[test]
    fn real_booter_load() {
        let Some(blob) = firmware("booter_load-570.144.bin") else { return };
        let b = Booter::parse(&blob).unwrap();
        // nvfw_bin_hdr: data at 0x378, 0xec00 bytes; hs header: 2 signatures
        // of 0x300 / 2 = 0x180 bytes; patch at 35344 (0x8a10); meta 1, 1, 3.
        assert_eq!(b.image.len(), 0xec00);
        assert_eq!((b.num_sigs, b.sig_size, b.patch_loc), (2, 0x180, 35344));
        assert_eq!((b.fuse_ver, b.engine_id, b.ucode_id), (1, 1, 3));
        // load header: os_data 0x8a00/0x6200, one app at 0x100 of 0x8900.
        assert_eq!((b.imem_base_img, b.imem_size, b.boot_addr), (0x100, 0x8900, 0x100));
        assert_eq!((b.dmem_base_img, b.dmem_size, b.dmem_sign), (0x8a00, 0x6200, 35344 - 0x8a00));
        // `trace-gsp` (8,6989 s): nouveau reads 0x824148 = 1 booting
        // booter_load, and patches signature 0 (1 - fls(1)).
        assert_eq!(b.fuse_register(), Some(0x824148));
        assert_eq!(b.signature_index(1), Ok(0));
        // Nothing burnt: the last of the two; a newer fuse: refused.
        assert_eq!(b.signature_index(0), Ok(1));
        assert_eq!(b.signature_index(2), Err(FwError::FuseMismatch));
        let mut img = b.image.to_vec();
        b.patch(&mut img, 0).unwrap();
        assert_eq!(&img[35344..35344 + 0x180], b.signature(0).unwrap());
    }

    #[test]
    fn real_booter_unload() {
        let Some(blob) = firmware("booter_unload-570.144.bin") else { return };
        let b = Booter::parse(&blob).unwrap();
        assert_eq!(b.image.len(), 0x9d00);
        assert_eq!((b.num_sigs, b.sig_size, b.patch_loc), (2, 0x180, 20496));
        assert_eq!((b.fuse_ver, b.engine_id, b.ucode_id), (1, 1, 3));
        assert_eq!(b.fuse_register(), Some(0x824148));
        assert_eq!((b.imem_base_img, b.imem_size), (0x100, 0x4f00));
        assert_eq!((b.dmem_base_img, b.dmem_size, b.dmem_sign), (0x5000, 0x4d00, 20496 - 0x5000));
    }

    #[test]
    fn real_bootloader() {
        let Some(blob) = firmware("bootloader-570.144.bin") else { return };
        let b = Bootloader::parse(&blob).unwrap();
        assert_eq!(b.image.len(), 0x6000);
        let d = b.desc;
        assert_eq!(d.version, 5);
        assert_eq!((d.bootloader_offset, d.bootloader_size), (0x5000, 0x880));
        assert_eq!((d.bootloader_param_offset, d.bootloader_param_size), (0x5880, 0x10));
        assert_eq!((d.manifest_offset, d.manifest_size), (0, 0x800));
        assert_eq!((d.monitor_data_offset, d.monitor_data_size), (0x800, 0x1000));
        assert_eq!((d.monitor_code_offset, d.monitor_code_size), (0x1800, 0x2900));
        assert_eq!(d.is_monitor_enabled, 1);
        assert_eq!((d.riscv_elf_offset, d.riscv_elf_size, d.app_version), (0, 0, 0));
        assert_eq!((d.fb_reserved_size, d.signed_as_code), (0x6000, 0));
        // Every part the descriptor names is inside the image.
        for (o, s) in [
            (d.bootloader_offset, d.bootloader_size),
            (d.bootloader_param_offset, d.bootloader_param_size),
            (d.manifest_offset, d.manifest_size),
            (d.monitor_data_offset, d.monitor_data_size),
            (d.monitor_code_offset, d.monitor_code_size),
        ] {
            assert!(o as usize + s as usize <= b.image.len(), "{o:#x}+{s:#x}");
        }
    }

    #[test]
    fn real_gsp_elf() {
        let Some(elf) = firmware("gsp-570.144.bin") else { return };
        let g = GspImage::parse(&elf, SIG_SECTION_GA10X).unwrap();
        // Section table of the file: .fwimage at 0x40, 0x3c99000 bytes;
        // every architecture's signature is 0x1000 bytes.
        assert_eq!(g.fwimage.len(), 0x3c9_9000);
        assert_eq!(g.signature.len(), 0x1000);
        assert_eq!(g.fwimage.as_ptr() as usize - elf.as_ptr() as usize, 0x40);
        assert_eq!(g.signature.as_ptr() as usize - elf.as_ptr() as usize, 0x3c9_f06c);
        // The image is followed by nothing this port reads; the other
        // architectures' signatures are different bytes.
        assert_ne!(elf_section(&elf, ".fwsignature_ad10x").unwrap(), g.signature);
        assert_eq!(elf_section(&elf, ".fwsignature_tu10x"), Err(FwError::NoSection));
    }
}
