//! EDID parser: identity, detailed timings, range limits, CTA-861 extension.
//!
//! Layouts follow DRM's (`include/drm/drm_edid.h`, `drivers/gpu/drm/drm_edid.c`
//! in the pinned Linux v7.2.2, MIT) and VESA E-EDID 1.4. The tests compare
//! the result with `edid-decode`'s output for the two monitors of the
//! target machine (`fixtures/edid-*.txt`).

use alloc::string::String;
use alloc::vec::Vec;

pub const BLOCK: usize = 128;
const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];

/// Display descriptor tags (`drm_edid.h:185-188`).
const TAG_NAME: u8 = 0xfc;
const TAG_RANGE: u8 = 0xfd;
const TAG_SERIAL: u8 = 0xff;
/// CTA-861 extension tag and its video data block (`drm_edid.c:4178`).
const CTA_EXT: u8 = 0x02;
const CTA_DB_VIDEO: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdidError {
    /// Shorter than one block, or not a multiple of 128 bytes.
    Length,
    /// Block 0 does not start with the fixed header.
    Header,
    /// Block `n` does not sum to 0 mod 256.
    Checksum(usize),
}

/// A detailed timing descriptor (`struct detailed_pixel_timing`,
/// `drm_edid.h:75-92`, after the 2-byte pixel clock).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub pixclk_khz: u32,
    pub hactive: u16,
    pub hblank: u16,
    pub hsync_offset: u16,
    pub hsync_width: u16,
    pub vactive: u16,
    pub vblank: u16,
    pub vsync_offset: u16,
    pub vsync_width: u16,
    pub width_mm: u16,
    pub height_mm: u16,
    /// `DRM_EDID_PT_INTERLACED` (`drm_edid.h:72`).
    pub interlaced: bool,
    /// Sync polarities, for digital separate sync (`misc` bits 4:3 = 11).
    pub hsync_positive: bool,
    pub vsync_positive: bool,
}

impl Timing {
    fn parse(d: &[u8]) -> Option<Timing> {
        let clk = u16::from_le_bytes([d[0], d[1]]) as u32;
        if clk == 0 {
            return None; // a display descriptor, not a timing
        }
        let t = &d[2..];
        let separate = t[15] & 0x18 == 0x18;
        Some(Timing {
            pixclk_khz: clk * 10,
            hactive: t[0] as u16 | ((t[2] as u16 & 0xf0) << 4),
            hblank: t[1] as u16 | ((t[2] as u16 & 0x0f) << 8),
            vactive: t[3] as u16 | ((t[5] as u16 & 0xf0) << 4),
            vblank: t[4] as u16 | ((t[5] as u16 & 0x0f) << 8),
            hsync_offset: t[6] as u16 | ((t[9] as u16 & 0xc0) << 2),
            hsync_width: t[7] as u16 | ((t[9] as u16 & 0x30) << 4),
            vsync_offset: (t[8] as u16 >> 4) | ((t[9] as u16 & 0x0c) << 2),
            vsync_width: (t[8] as u16 & 0x0f) | ((t[9] as u16 & 0x03) << 4),
            width_mm: t[10] as u16 | ((t[12] as u16 & 0xf0) << 4),
            height_mm: t[11] as u16 | ((t[12] as u16 & 0x0f) << 8),
            interlaced: t[15] & 0x80 != 0,
            hsync_positive: separate && t[15] & 0x02 != 0,
            vsync_positive: separate && t[15] & 0x04 != 0,
        })
    }

    pub fn htotal(&self) -> u32 {
        self.hactive as u32 + self.hblank as u32
    }

    /// Refresh in µHz (field rate if interlaced: two fields of
    /// `vactive + vblank` lines plus the half line between them).
    pub fn refresh_uhz(&self) -> u64 {
        let v = self.vactive as u64 + self.vblank as u64;
        let (num, den) = if self.interlaced { (2, 2 * v + 1) } else { (1, v) };
        let den = self.htotal() as u64 * den;
        if den == 0 {
            return 0;
        }
        (self.pixclk_khz as u64 * 1_000_000_000 * num + den / 2) / den
    }

    pub fn refresh_mhz(&self) -> u64 {
        (self.refresh_uhz() + 500) / 1000
    }

    /// `1920x1080`, `1920x1080i`: the frame size (interlaced timings
    /// describe one field, so the frame has twice the lines).
    pub fn size(&self) -> (u16, u16) {
        (self.hactive, if self.interlaced { self.vactive * 2 } else { self.vactive })
    }
}

/// Monitor range limits (`struct detailed_data_monitor_range`,
/// `drm_edid.h:119-145`), offsets of EDID 1.4 applied (`drm_edid.h:99-102`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub min_vfreq: u16,
    pub max_vfreq: u16,
    pub min_hfreq_khz: u16,
    pub max_hfreq_khz: u16,
    /// 0 = not given.
    pub max_pixclk_mhz: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edid {
    /// Three-letter PNP id (`mfg_id`, `drm_edid.h:297`).
    pub manufacturer: [u8; 3],
    pub product: u16,
    pub serial: u32,
    pub week: u8,
    pub year: u16,
    pub version: (u8, u8),
    pub name: Option<String>,
    pub serial_text: Option<String>,
    pub range: Option<Range>,
    /// Block 0's DTDs, then the CTA extension's. The first one is the
    /// preferred mode (EDID 1.4 always; 1.3 when the feature bit says so).
    pub timings: Vec<Timing>,
    /// VICs of the CTA video data block, in order, and which are native.
    pub vics: Vec<(u8, bool)>,
    pub extensions: u8,
    /// Extension blocks present (the count can exceed what was read).
    pub blocks_read: usize,
}

fn text(d: &[u8]) -> String {
    // 13 bytes, ended by 0x0a, padded with spaces.
    let s = &d[5..18];
    let end = s.iter().position(|&b| b == 0x0a).unwrap_or(s.len());
    let t: String = s[..end].iter().map(|&b| if (0x20..0x7f).contains(&b) { b as char } else { '?' }).collect();
    String::from(t.trim_end())
}

impl Edid {
    /// Parses block 0 and any extension blocks present in `data`.
    pub fn parse(data: &[u8]) -> Result<Edid, EdidError> {
        if data.len() < BLOCK || data.len() % BLOCK != 0 {
            return Err(EdidError::Length);
        }
        if data[..8] != HEADER {
            return Err(EdidError::Header);
        }
        for (i, b) in data.chunks(BLOCK).enumerate() {
            if b.iter().fold(0u8, |s, &x| s.wrapping_add(x)) != 0 {
                return Err(EdidError::Checksum(i));
            }
        }
        let id = u16::from_be_bytes([data[8], data[9]]);
        let letter = |s: u16| b'A' - 1 + ((id >> s) & 0x1f) as u8;
        let mut e = Edid {
            manufacturer: [letter(10), letter(5), letter(0)],
            product: u16::from_le_bytes([data[10], data[11]]),
            serial: u32::from_le_bytes(data[12..16].try_into().unwrap()),
            week: data[16],
            year: 1990 + data[17] as u16,
            version: (data[18], data[19]),
            name: None,
            serial_text: None,
            range: None,
            timings: Vec::new(),
            vics: Vec::new(),
            extensions: data[126],
            blocks_read: data.len() / BLOCK,
        };
        for d in data[54..126].chunks(18) {
            if let Some(t) = Timing::parse(d) {
                e.timings.push(t);
                continue;
            }
            match d[3] {
                TAG_NAME => e.name = Some(text(d)),
                TAG_SERIAL => e.serial_text = Some(text(d)),
                TAG_RANGE => {
                    let f = d[4];
                    let add = |bit: u8, v: u8| v as u16 + if f & bit != 0 { 255 } else { 0 };
                    // A max offset implies the min one only for its own
                    // bit; E-EDID 1.4 §3.10.3.3.
                    e.range = Some(Range {
                        min_vfreq: add(1 << 0, d[5]),
                        max_vfreq: add(1 << 1, d[6]),
                        min_hfreq_khz: add(1 << 2, d[7]),
                        max_hfreq_khz: add(1 << 3, d[8]),
                        max_pixclk_mhz: d[9] as u16 * 10,
                    });
                }
                _ => {}
            }
        }
        for b in data[BLOCK..].chunks(BLOCK) {
            if b[0] == CTA_EXT {
                e.parse_cta(b);
            }
        }
        Ok(e)
    }

    /// CTA-861 extension: data blocks from byte 4 to the DTD offset, then
    /// DTDs up to the checksum.
    fn parse_cta(&mut self, b: &[u8]) {
        let dtd = (b[2] as usize).min(BLOCK - 1);
        if dtd >= 4 {
            let mut i = 4;
            while i < dtd {
                // `cea_db_tag`, `cea_db_payload_len` (`drm_edid.c:5022-5031`).
                let tag = b[i] >> 5;
                let len = (b[i] & 0x1f) as usize;
                let end = (i + 1 + len).min(dtd);
                if tag == CTA_DB_VIDEO {
                    for &svd in &b[i + 1..end] {
                        // `svd_to_vic` (`drm_edid.c:4596-4603`).
                        let native = (129..=192).contains(&svd);
                        let vic = if (1..=64).contains(&svd) || native { svd & 127 } else { svd };
                        self.vics.push((vic, native));
                    }
                }
                i += 1 + len;
            }
        }
        if dtd != 0 {
            let mut i = dtd;
            while i + 18 <= BLOCK - 1 {
                match Timing::parse(&b[i..i + 18]) {
                    Some(t) => self.timings.push(t),
                    None => break,
                }
                i += 18;
            }
        }
    }

    pub fn manufacturer_str(&self) -> &str {
        core::str::from_utf8(&self.manufacturer).unwrap_or("???")
    }

    pub fn preferred(&self) -> Option<&Timing> {
        self.timings.first()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    const ASUS: &[u8] = include_bytes!("../fixtures/edid-asus-vg279q3a.bin");
    const ASUS_TXT: &str = include_str!("../fixtures/edid-asus-vg279q3a.txt");
    const HP: &[u8] = include_bytes!("../fixtures/edid-hp-2309.bin");
    const HP_TXT: &str = include_str!("../fixtures/edid-hp-2309.txt");

    /// `DTD n:  WxH[i]   R Hz` lines of edid-decode, as (size, refresh).
    fn decode_dtds(txt: &str) -> Vec<(String, String)> {
        txt.lines()
            .filter(|l| l.trim_start().starts_with("DTD "))
            .map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                (String::from(f[2]), String::from(f[3]))
            })
            .collect()
    }

    fn decode_vics(txt: &str) -> Vec<(u8, bool)> {
        txt.lines()
            .filter(|l| l.trim_start().starts_with("VIC "))
            .map(|l| {
                let n = l.trim_start()[3..].trim_start().split(':').next().unwrap().trim();
                (n.parse().unwrap(), l.contains("(native)"))
            })
            .collect()
    }

    fn ours(e: &Edid) -> Vec<(String, String)> {
        e.timings
            .iter()
            .map(|t| {
                let (w, h) = t.size();
                let r = t.refresh_uhz();
                (format!("{w}x{h}{}", if t.interlaced { "i" } else { "" }), format!("{}.{:06}", r / 1_000_000, r % 1_000_000))
            })
            .collect()
    }

    #[test]
    fn asus_vg279q3a_like_edid_decode() {
        let e = Edid::parse(ASUS).unwrap();
        assert!(ASUS_TXT.contains("Manufacturer: AUS") && e.manufacturer_str() == "AUS");
        assert!(ASUS_TXT.contains(&format!("Model: {}", e.product)));
        assert!(ASUS_TXT.contains(&format!("Made in: week {} of {}", e.week, e.year)));
        assert!(ASUS_TXT.contains(&format!("Display Product Name: '{}'", e.name.as_deref().unwrap())));
        assert_eq!(e.name.as_deref(), Some("VG279Q3A"));
        assert!(ASUS_TXT.contains(&format!("Serial Number: '{}'", e.serial_text.as_deref().unwrap())));
        let r = e.range.unwrap();
        assert!(ASUS_TXT.contains(&format!(
            "{}-{} Hz V, {}-{} kHz H, max dotclock {} MHz",
            r.min_vfreq, r.max_vfreq, r.min_hfreq_khz, r.max_hfreq_khz, r.max_pixclk_mhz
        )));
        assert_eq!((r.min_vfreq, r.max_vfreq, r.max_pixclk_mhz), (48, 180, 430));
        assert_eq!(ours(&e), decode_dtds(ASUS_TXT));
        assert_eq!(e.vics, decode_vics(ASUS_TXT));
        let p = e.preferred().unwrap();
        assert_eq!((p.size(), p.refresh_mhz(), p.pixclk_khz), ((1920, 1080), 60_000, 148_500));
        assert_eq!((p.hsync_offset, p.hsync_width, p.vsync_offset, p.vsync_width), (88, 44, 4, 5));
        assert!(p.hsync_positive && p.vsync_positive);
        assert_eq!((e.extensions, e.blocks_read), (1, 2));
    }

    #[test]
    fn hp_2309_like_edid_decode() {
        let e = Edid::parse(HP).unwrap();
        assert_eq!(e.manufacturer_str(), "HWP");
        assert!(HP_TXT.contains(&format!("Model: {}", e.product)));
        assert_eq!(e.name.as_deref(), Some("HP 2309"));
        let r = e.range.unwrap();
        assert!(HP_TXT.contains(&format!(
            "{}-{} Hz V, {}-{} kHz H, max dotclock {} MHz",
            r.min_vfreq, r.max_vfreq, r.min_hfreq_khz, r.max_hfreq_khz, r.max_pixclk_mhz
        )));
        // Five DTDs, two of them interlaced (field rate).
        assert_eq!(ours(&e), decode_dtds(HP_TXT));
        assert_eq!(e.timings.len(), 5);
        assert_eq!(e.vics, decode_vics(HP_TXT));
    }

    #[test]
    fn malformed() {
        assert_eq!(Edid::parse(&ASUS[..100]), Err(EdidError::Length));
        assert_eq!(Edid::parse(&ASUS[..200]), Err(EdidError::Length));
        let mut bad = ASUS.to_vec();
        bad[0] = 1;
        assert_eq!(Edid::parse(&bad), Err(EdidError::Header));
        let mut bad = ASUS.to_vec();
        bad[200] ^= 1;
        assert_eq!(Edid::parse(&bad), Err(EdidError::Checksum(1)));
        // Block 0 alone parses (extension count says 1, one block read).
        let e = Edid::parse(&ASUS[..128]).unwrap();
        assert_eq!((e.extensions, e.blocks_read, e.vics.len()), (1, 1, 0));
        // A CTA block with lengths pointing past its end: no panic.
        let mut ext = alloc::vec![0u8; 128];
        ext[0] = CTA_EXT;
        ext[2] = 200;
        ext[4] = 0x5f; // video block claiming 31 bytes
        let sum = ext.iter().fold(0u8, |s, &x| s.wrapping_add(x));
        ext[127] = 0u8.wrapping_sub(sum);
        let mut two = ASUS[..128].to_vec();
        two.extend_from_slice(&ext);
        assert!(Edid::parse(&two).is_ok());
        // SVDs: 0x90 is native VIC 16; 193 and above are VICs as they are
        // (`svd_to_vic`), never native.
        let mut ext = alloc::vec![0u8; 128];
        ext[0] = CTA_EXT;
        ext[2] = 8;
        ext[4] = (CTA_DB_VIDEO << 5) | 3;
        ext[5..8].copy_from_slice(&[0x90, 0xc1, 0x04]);
        let sum = ext.iter().fold(0u8, |s, &x| s.wrapping_add(x));
        ext[127] = 0u8.wrapping_sub(sum);
        let mut two = ASUS[..128].to_vec();
        two.extend_from_slice(&ext);
        assert_eq!(Edid::parse(&two).unwrap().vics, [(16, true), (193, false), (4, false)]);
    }
}
