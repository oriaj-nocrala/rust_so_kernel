//! The DCB (Display Configuration Block) of the VBIOS: display outputs, the
//! CCB (which I2C bus / AUX channel each one uses) and the connector table.
//!
//! Only DCB 4.x with a 4.1 CCB — what the target board has (nouveau's
//! `trace-nogsp` dmesg: CCB entries of type 0x80 = `DCB_I2C_PMGR`). Paths
//! are relative to `drivers/gpu/drm/nouveau/` in the pinned Linux v7.2.2.

use alloc::vec::Vec;

use crate::vbios::Bios;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DcbError {
    /// No DCB pointer at 0x36, or a bad signature (`dcb.c:35-60`).
    NoTable,
    /// A DCB, CCB or connector-table version this parser does not handle.
    Version { table: &'static str, version: u8 },
}

/// Output types (`include/nvkm/subdev/bios/dcb.h:5-13`).
pub const OUTPUT_TMDS: u8 = 0x2;
pub const OUTPUT_DP: u8 = 0x6;
const OUTPUT_EOL: u8 = 0xe;
const OUTPUT_UNUSED: u8 = 0xf;

/// One DCB output entry (`dcb.c:121-194`), the fields nouveau prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Output {
    pub index: u8,
    pub kind: u8,
    pub location: u8,
    /// Mask of output resources (SORs) this output can use.
    pub or: u8,
    pub link: u8,
    pub connector: u8,
    /// CCB index of its I2C bus / AUX channel.
    pub i2c_index: u8,
    pub bus: u8,
    pub heads: u8,
    /// DP only: maximum link rate (DPCD units of 0.27 Gb/s) and lanes.
    pub dp_link_bw: u8,
    pub dp_link_nr: u8,
}

/// One CCB entry (`bios/i2c.c:65-135`, the `DCB_I2C_PMGR` case). `None` =
/// `DCB_I2C_UNUSED`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ccb {
    pub index: u8,
    pub used: bool,
    /// Bit-banged I2C port (`0xd014 + drive * 0x20`, `i2c/busgf119.c:93`).
    pub drive: Option<u8>,
    /// DP AUX channel.
    pub auxch: Option<u8>,
    /// The hybrid pad the two share (`i2c/base.c:288-291`).
    pub share: Option<u8>,
}

/// One connector table entry (`conn.c:77-106`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Connector {
    pub index: u8,
    /// `enum dcb_connector_type` (`include/nvkm/subdev/bios/conn.h`).
    pub kind: u8,
    pub location: u8,
    pub hpd: u8,
}

pub struct Dcb {
    pub version: u8,
    pub outputs: Vec<Output>,
    pub ccb: Vec<Ccb>,
    pub connectors: Vec<Connector>,
}

impl Dcb {
    pub fn parse(bios: &Bios) -> Result<Dcb, DcbError> {
        // `dcb_table` (`dcb.c:28-97`), 3.x/4.x branch.
        let dcb = bios.rd16(0x36) as u32;
        if dcb == 0 {
            return Err(DcbError::NoTable);
        }
        let version = bios.rd08(dcb);
        if !(0x30..0x42).contains(&version) {
            return Err(DcbError::Version { table: "DCB", version });
        }
        if bios.rd32(dcb + 6) != 0x4edc_bdcb {
            return Err(DcbError::NoTable);
        }
        let hdr = bios.rd08(dcb + 1) as u32;
        let cnt = bios.rd08(dcb + 2) as u32;
        let len = bios.rd08(dcb + 3) as u32;

        let mut outputs = Vec::new();
        // The display engine's walk (`nvkm/engine/disp/nv50.c:1647-1653`):
        // every entry up to `cnt`, skipping type UNUSED, stopping at EOL.
        for idx in 0..cnt {
            let e = dcb + hdr + idx * len;
            let conn = bios.rd32(e);
            match (conn & 0xf) as u8 {
                OUTPUT_UNUSED => continue,
                OUTPUT_EOL => break,
                _ => {}
            }
            // `dcb.c:128-137`.
            let mut o = Output {
                index: idx as u8,
                or: ((conn & 0x0f00_0000) >> 24) as u8,
                location: ((conn & 0x0030_0000) >> 20) as u8,
                bus: ((conn & 0x000f_0000) >> 16) as u8,
                connector: ((conn & 0x0000_f000) >> 12) as u8,
                heads: ((conn & 0x0000_0f00) >> 8) as u8,
                i2c_index: ((conn & 0x0000_00f0) >> 4) as u8,
                kind: (conn & 0x0000_000f) as u8,
                link: 0,
                dp_link_bw: 0,
                dp_link_nr: 0,
            };
            if version >= 0x40 {
                let conf = bios.rd32(e + 4);
                // `dcb.c:142-180`.
                if o.kind == OUTPUT_DP {
                    o.dp_link_bw = match conf & 0x00e0_0000 {
                        0x0000_0000 => 0x06,
                        0x0020_0000 => 0x0a,
                        0x0040_0000 => 0x14,
                        _ => 0x1e,
                    };
                    o.dp_link_nr = match (conf & 0x0f00_0000) >> 24 {
                        0xf | 0x4 => 4,
                        0x3 | 0x2 => 2,
                        _ => 1,
                    };
                }
                if matches!(o.kind, OUTPUT_DP | OUTPUT_TMDS | 0x3) {
                    o.link = ((conf & 0x30) >> 4) as u8;
                }
            }
            outputs.push(o);
        }

        Ok(Dcb { version, outputs, ccb: parse_ccb(bios, dcb)?, connectors: parse_connectors(bios, dcb, hdr)? })
    }

    pub fn ccb(&self, index: u8) -> Option<&Ccb> {
        self.ccb.iter().find(|c| c.index == index && c.used)
    }
    pub fn connector(&self, index: u8) -> Option<&Connector> {
        self.connectors.iter().find(|c| c.index == index)
    }
}

/// `dcb_i2c_table` + `dcb_i2c_parse` (`bios/i2c.c:28-135`), CCB 4.1 only.
fn parse_ccb(bios: &Bios, dcb: u32) -> Result<Vec<Ccb>, DcbError> {
    let i2c = bios.rd16(dcb + 4) as u32; // `bios/i2c.c:37-38`
    if i2c == 0 {
        return Ok(Vec::new());
    }
    let version = bios.rd08(i2c);
    if version != 0x41 {
        return Err(DcbError::Version { table: "CCB", version });
    }
    let hdr = bios.rd08(i2c + 1) as u32;
    let cnt = bios.rd08(i2c + 2) as u32;
    let len = bios.rd08(i2c + 3) as u32;
    let mut out = Vec::new();
    for idx in 0..cnt {
        let ent = bios.rd32(i2c + hdr + idx * len);
        // `bios/i2c.c:77-85`: both ports 0x1f = unused.
        let port = ent & 0x1f;
        let aux = (ent >> 5) & 0x1f;
        let used = !(port == 0x1f && aux == 0x1f);
        // `bios/i2c.c:118-126`.
        let drive = (used && port != 0x1f).then_some(port as u8);
        let auxch = (used && aux != 0x1f).then_some(aux as u8);
        out.push(Ccb { index: idx as u8, used, drive, auxch, share: auxch });
    }
    Ok(out)
}

/// `nvbios_connTe` + `nvbios_connEp` (`conn.c:28-106`). Entries of type
/// `DCB_CONNECTOR_NONE` (0xff, "Skip Entry", `conn.h:78`) are left out.
fn parse_connectors(bios: &Bios, dcb: u32, dcb_hdr: u32) -> Result<Vec<Connector>, DcbError> {
    if dcb_hdr < 0x16 {
        return Ok(Vec::new());
    }
    let t = bios.rd16(dcb + 0x14) as u32;
    if t == 0 {
        return Ok(Vec::new());
    }
    let version = bios.rd08(t);
    if version != 0x30 && version != 0x40 {
        return Err(DcbError::Version { table: "connector", version });
    }
    let hdr = bios.rd08(t + 1) as u32;
    let cnt = bios.rd08(t + 2) as u32;
    let len = bios.rd08(t + 3) as u32;
    let mut out = Vec::new();
    for idx in 0..cnt {
        let e = t + hdr + idx * len;
        let kind = bios.rd08(e);
        if kind == 0xff {
            continue;
        }
        let b1 = bios.rd08(e + 1);
        let mut hpd = (b1 & 0x30) >> 4;
        if len >= 4 {
            hpd |= (bios.rd08(e + 2) & 0x03) << 2;
            hpd |= (bios.rd08(e + 3) & 0x07) << 4;
        }
        out.push(Connector { index: idx as u8, kind, location: b1 & 0x0f, hpd });
    }
    Ok(out)
}

/// The DRM connector type name for a DCB connector type: nouveau's mapping
/// (`nouveau_connector.c:1253-1280`) to DRM's names (`drm_connector.c:93-115`).
pub fn connector_type_name(kind: u8) -> &'static str {
    match kind {
        0x00 => "VGA",
        0x10 | 0x11 | 0x13 => "TV",
        0x38 | 0x39 | 0x30 => "DVI-I",
        0x31 => "DVI-D",
        0x40 | 0x41 => "LVDS",
        0x64 | 0x65 | 0x46 | 0x48 | 0x71 => "DP",
        0x47 => "eDP",
        0x60 | 0x61 | 0x63 => "HDMI-A",
        0x70 => "Virtual",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vbios::tests::oracle_vbios;

    /// Against nouveau's own reading of the same VBIOS (`trace-nogsp`
    /// dmesg lines 77-91 and 939-952).
    #[test]
    fn oracle_dcb_matches_nouveau() {
        let Some(rom) = oracle_vbios() else { return };
        let bios = Bios::new(rom).unwrap();
        let dcb = Dcb::parse(&bios).unwrap();

        // "disp: outp II:...: type TT loc L or O link K con C edid E bus B head H"
        let got: Vec<_> = dcb
            .outputs
            .iter()
            .map(|o| (o.index, o.kind, o.location, o.or, o.link, o.connector, o.i2c_index, o.bus, o.heads))
            .collect();
        assert_eq!(
            got,
            [
                (0, 6, 0, 4, 2, 0, 8, 0, 0xf),
                (1, 2, 0, 4, 2, 0, 8, 0, 0xf),
                (2, 6, 0, 4, 1, 1, 7, 1, 0xf),
                (3, 2, 0, 4, 1, 1, 7, 1, 0xf),
                (4, 6, 0, 2, 2, 2, 6, 2, 0xf),
                (5, 2, 0, 2, 2, 2, 6, 2, 0xf),
                (7, 2, 0, 2, 1, 3, 5, 3, 0xf),
            ]
        );
        // "bios dp 42 13 00 00": not decoded; the DCB's own DP limits.
        assert!(dcb.outputs.iter().filter(|o| o.kind == OUTPUT_DP).all(|o| o.dp_link_nr == 4));

        // "i2c: ccb NN: type 80 drive DD sense ff share SS auxch AA"
        assert_eq!(dcb.ccb.len(), 15);
        for c in &dcb.ccb {
            let i = c.index;
            match i {
                0..=2 => assert_eq!((c.used, c.drive, c.auxch), (true, Some(i), None), "ccb {i}"),
                3..=9 => assert_eq!((c.used, c.drive, c.auxch), (true, Some(i), Some(i - 3)), "ccb {i}"),
                _ => assert!(!c.used, "ccb {i}"),
            }
            assert_eq!(c.share, c.auxch);
        }

        // "disp: conn II:LLTT: type TT loc L hpd HH ..."
        let got: Vec<_> = dcb.connectors.iter().map(|c| (c.index, c.kind, c.location, c.hpd)).collect();
        assert_eq!(&got[..4], [(0, 0x46, 0, 0x20), (1, 0x46, 1, 0x10), (2, 0x46, 2, 0x08), (3, 0x61, 3, 0x04)]);
        assert_eq!(connector_type_name(0x46), "DP");
        assert_eq!(connector_type_name(0x61), "HDMI-A");
    }

    /// A one-image ROM with a DCB 4.1 at 0x100 holding `entries` (8 bytes
    /// each: conn, conf) and no CCB or connector table.
    fn rom_with_dcb(entries: &[(u32, u32)]) -> Bios {
        let mut r = alloc::vec![0u8; 0x400];
        r[0..2].copy_from_slice(&0xaa55u16.to_le_bytes());
        r[0x18..0x1a].copy_from_slice(&0x40u16.to_le_bytes());
        r[0x40..0x44].copy_from_slice(b"PCIR");
        r[0x50..0x52].copy_from_slice(&2u16.to_le_bytes());
        r[0x55] = 0x80;
        r[0x36..0x38].copy_from_slice(&0x100u16.to_le_bytes());
        let d = 0x100;
        r[d] = 0x41;
        r[d + 1] = 0x10; // header size (< 0x16: no connector table)
        r[d + 2] = entries.len() as u8;
        r[d + 3] = 8;
        r[d + 6..d + 10].copy_from_slice(&0x4edc_bdcbu32.to_le_bytes());
        for (i, (conn, conf)) in entries.iter().enumerate() {
            let e = d + 0x10 + i * 8;
            r[e..e + 4].copy_from_slice(&conn.to_le_bytes());
            r[e + 4..e + 8].copy_from_slice(&conf.to_le_bytes());
        }
        Bios::new(r).unwrap()
    }

    #[test]
    fn eol_ends_the_walk_unused_is_skipped() {
        // DP, UNUSED, TMDS, EOL, TMDS.
        let b = rom_with_dcb(&[(0x0200_0f86, 0), (0xf, 0), (0x0200_1f52, 0x10), (0xe, 0), (0x0200_2f52, 0)]);
        let d = Dcb::parse(&b).unwrap();
        let got: Vec<_> = d.outputs.iter().map(|o| (o.index, o.kind, o.connector, o.i2c_index, o.link)).collect();
        assert_eq!(got, [(0, 6, 0, 8, 0), (2, 2, 1, 5, 1)]);
        assert!(d.ccb.is_empty() && d.connectors.is_empty());
    }

    #[test]
    fn no_table_is_an_error_not_a_panic() {
        // A valid one-image ROM with nothing at 0x36.
        let mut r = alloc::vec![0u8; 0x400];
        r[0..2].copy_from_slice(&0xaa55u16.to_le_bytes());
        r[0x18..0x1a].copy_from_slice(&0x40u16.to_le_bytes());
        r[0x40..0x44].copy_from_slice(b"PCIR");
        r[0x50..0x52].copy_from_slice(&2u16.to_le_bytes());
        r[0x55] = 0x80;
        let bios = Bios::new(r.clone()).unwrap();
        assert!(matches!(Dcb::parse(&bios), Err(DcbError::NoTable)));
        // A pointer to a table with a wrong version.
        r[0x36..0x38].copy_from_slice(&0x100u16.to_le_bytes());
        r[0x100] = 0x50;
        let bios = Bios::new(r.clone()).unwrap();
        assert!(matches!(Dcb::parse(&bios), Err(DcbError::Version { table: "DCB", version: 0x50 })));
        // Right version, bad signature.
        r[0x100] = 0x41;
        let bios = Bios::new(r.clone()).unwrap();
        assert!(matches!(Dcb::parse(&bios), Err(DcbError::NoTable)));
        // Pointer past the end of the ROM: reads as zeros.
        r[0x36..0x38].copy_from_slice(&0xfff0u16.to_le_bytes());
        let bios = Bios::new(r).unwrap();
        assert!(Dcb::parse(&bios).is_err());
    }
}
