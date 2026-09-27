//! Connectors from the DCB, probed by polling: DPCD and EDID over DP AUX,
//! EDID over bit-banged I2C. What `/proc/displays` shows (phase 2 of
//! `docs/gpu/gpu-plan.md`).
//!
//! Only reads and the AUX/I2C transactions nouveau made in `trace-nogsp`.
//! Every register the probe changes is put back as it was found: the pads'
//! power bits and the AUX channel's auto-DPCD bit (nouveau leaves the
//! latter cleared, `i2c/auxch.h:7-11`).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use crate::aux::{self, Aux, AuxError};
use crate::dcb::{self, Dcb};
use crate::edid::{Edid, BLOCK};
use crate::i2c::{Bus, I2cError};
use crate::pad::{self, PadMode};
use crate::Mmio;

/// How the connector's DDC is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Path {
    /// DP AUX channel `ch`, on hybrid pad `pad`.
    Aux { ch: u8, pad: Option<u8> },
    /// Bit-banged port `drive`, on hybrid pad `pad`.
    I2c { drive: u8, pad: Option<u8> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connector {
    /// Index in the DCB connector table.
    pub index: u8,
    pub kind: u8,
    /// DRM type name plus a 1-based count per type, in connector-table
    /// order (`DP-1`, `DP-2`, `DP-3`, `HDMI-A-1` on the target board).
    /// This is nouveau's naming (it creates the DRM connectors in that
    /// order), not the proprietary driver's: that one calls the ASUS
    /// `DP-1` (plan, phase 0 results).
    pub name: String,
    pub path: Option<Path>,
}

/// The connectors some DCB output uses, with their DDC path: a DP output
/// gives the AUX channel of its CCB entry; otherwise the CCB entry's I2C
/// port (`i2c/base.c:278-342` builds the same bus/aux objects from the CCB;
/// `engine/disp/outp.c:390` looks the output's up by `i2c_index`).
pub fn connectors(d: &Dcb) -> Vec<Connector> {
    let mut out: Vec<Connector> = Vec::new();
    for c in &d.connectors {
        let outs: Vec<&dcb::Output> = d.outputs.iter().filter(|o| o.connector == c.index).collect();
        if outs.is_empty() {
            continue;
        }
        let dp = outs.iter().find(|o| o.kind == dcb::OUTPUT_DP);
        let path = match dp {
            Some(o) => d.ccb(o.i2c_index).and_then(|e| e.auxch.map(|ch| Path::Aux { ch, pad: e.share })),
            None => outs
                .iter()
                .find_map(|o| d.ccb(o.i2c_index))
                .and_then(|e| e.drive.map(|drive| Path::I2c { drive, pad: e.share })),
        };
        let ty = dcb::connector_type_name(c.kind);
        let n = out.iter().filter(|x| dcb::connector_type_name(x.kind) == ty).count() + 1;
        out.push(Connector { index: c.index, kind: c.kind, name: format!("{ty}-{n}"), path });
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Connected,
    Disconnected,
    /// The probe failed in a way that says nothing about the monitor.
    Unknown,
}

#[derive(Debug, Clone)]
pub struct Probe {
    pub conn: Connector,
    pub status: Status,
    /// DPCD receiver capabilities (DP only).
    pub dpcd: Option<[u8; aux::RECEIVER_CAP_SIZE]>,
    /// The EDID bytes read: block 0 and, if announced, block 1.
    pub edid: Vec<u8>,
    pub error: Option<String>,
}

/// Probes one connector: DPCD + EDID over AUX, or EDID over I2C.
pub fn probe(m: &impl Mmio, conn: &Connector) -> Probe {
    let mut p = Probe { conn: conn.clone(), status: Status::Unknown, dpcd: None, edid: Vec::new(), error: None };
    match conn.path {
        None => p.error = Some(String::from("no DDC path in the DCB")),
        Some(Path::Aux { ch, pad }) => {
            let saved = pad.map(|n| pad::acquire(m, n, PadMode::Aux));
            let a = Aux::new(m, ch);
            let r = probe_aux(&a, &mut p);
            if let Some(v) = a.autodpcd_found() {
                m.mask(a.autodpcd_reg(), 0x0001_0000, v & 0x0001_0000);
            }
            if let Some(s) = saved {
                pad::release(m, s);
            }
            match r {
                Ok(()) => p.status = Status::Connected,
                Err(AuxError::NoSink) => p.status = Status::Disconnected,
                Err(e) => p.error = Some(format!("aux: {e:?}")),
            }
        }
        Some(Path::I2c { drive, pad }) => {
            let saved = pad.map(|n| pad::acquire(m, n, PadMode::I2c));
            let bus = Bus::new(m, drive);
            bus.init();
            let r = read_edid(|b, out| bus.read_edid_block(b, out), &mut p.edid);
            if let Some(s) = saved {
                pad::release(m, s);
            }
            match r {
                Ok(()) => p.status = Status::Connected,
                // Nothing answers at 0x50: no monitor (or one without DDC).
                Err(I2cError::Nack) if p.edid.is_empty() => p.status = Status::Disconnected,
                Err(e) => p.error = Some(format!("i2c: {e:?}")),
            }
        }
    }
    p
}

fn probe_aux<M: Mmio>(a: &Aux<M>, p: &mut Probe) -> Result<(), AuxError> {
    p.dpcd = Some(a.read_dpcd_caps()?);
    read_edid(|b, out| a.read_edid_block(b, out), &mut p.edid)
}

/// Block 0, then block 1 if the EDID announces an extension. More than one
/// extension would need the E-DDC segment pointer (not in the trace).
fn read_edid<E>(mut rd: impl FnMut(u8, &mut [u8; BLOCK]) -> Result<(), E>, out: &mut Vec<u8>) -> Result<(), E> {
    let mut b = [0u8; BLOCK];
    rd(0, &mut b)?;
    out.extend_from_slice(&b);
    if b[126] > 0 {
        rd(1, &mut b)?;
        out.extend_from_slice(&b);
    }
    Ok(())
}

fn link_rate(code: u8) -> &'static str {
    // DPCD 0x001 (`drm_dp.h:114`): 0.27 Gb/s units.
    match code {
        0x06 => "RBR",
        0x0a => "HBR",
        0x14 => "HBR2",
        0x1e => "HBR3",
        _ => "?",
    }
}

/// FNV-1a 64, the hash the metal jobs compare with the host's copy.
pub fn fnv1a64(d: &[u8]) -> u64 {
    d.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

/// `/proc/displays`: one line per connector, then details for connected
/// ones (identity, preferred mode, range, all detailed modes, the EDID in
/// hex for byte-exact comparison).
pub fn render(probes: &[Probe]) -> String {
    let mut s = String::new();
    for p in probes {
        let path = match p.conn.path {
            Some(Path::Aux { ch, .. }) => format!("aux {ch}"),
            Some(Path::I2c { drive, .. }) => format!("i2c {drive}"),
            None => String::from("-"),
        };
        let status = match p.status {
            Status::Connected => "connected",
            Status::Disconnected => "disconnected",
            Status::Unknown => "unknown",
        };
        let _ = write!(s, "{:<9} {:<12} conn {} {}", p.conn.name, status, p.conn.index, path);
        let edid = Edid::parse(&p.edid);
        if let Ok(e) = &edid {
            let _ = write!(s, "  {} {}", e.manufacturer_str(), e.name.as_deref().unwrap_or("?"));
        }
        let _ = writeln!(s);
        if let Some(err) = &p.error {
            let _ = writeln!(s, "  error: {err}");
        }
        if let Some(d) = p.dpcd {
            let _ = writeln!(s, "  dpcd: {}.{} {} x{}", d[0] >> 4, d[0] & 0xf, link_rate(d[1]), d[2] & 0x1f);
        }
        if p.edid.is_empty() {
            continue;
        }
        match &edid {
            Ok(e) => {
                if let Some(t) = e.preferred() {
                    let (w, h) = t.size();
                    let r = t.refresh_mhz();
                    let _ = writeln!(s, "  preferred: {w}x{h}@{}.{:03} Hz", r / 1000, r % 1000);
                }
                if let Some(r) = e.range {
                    let _ = writeln!(
                        s,
                        "  range: {}-{} Hz, {}-{} kHz, max {} MHz",
                        r.min_vfreq, r.max_vfreq, r.min_hfreq_khz, r.max_hfreq_khz, r.max_pixclk_mhz
                    );
                }
                let _ = write!(s, "  modes:");
                for t in &e.timings {
                    let (w, h) = t.size();
                    let r = t.refresh_mhz();
                    let _ = write!(s, " {w}x{h}{}@{}.{:03}", if t.interlaced { "i" } else { "" }, r / 1000, r % 1000);
                }
                let _ = writeln!(s);
                if e.blocks_read < 1 + e.extensions as usize {
                    let _ = writeln!(s, "  note: {} extension blocks, {} read", e.extensions, e.blocks_read - 1);
                }
            }
            Err(err) => {
                let _ = writeln!(s, "  edid: invalid ({err:?})");
            }
        }
        let _ = writeln!(s, "  edid-fnv1a64: {:016x}", fnv1a64(&p.edid));
        let _ = write!(s, "  edid-hex: ");
        for b in &p.edid {
            let _ = write!(s, "{b:02x}");
        }
        let _ = writeln!(s);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i2c::tests::DdcSim;
    use crate::mmio::testing::ReplayMmio;
    use crate::vbios::{tests::oracle_vbios, Bios};

    const EXTRACT: &str = include_str!("../fixtures/aux-ch3-dpcd-edid.txt");
    const ASUS: &[u8] = include_bytes!("../fixtures/edid-asus-vg279q3a.bin");
    const HP: &[u8] = include_bytes!("../fixtures/edid-hp-2309.bin");

    #[test]
    fn connectors_of_the_target_board() {
        let Some(rom) = oracle_vbios() else { return };
        let d = Dcb::parse(&Bios::new(rom).unwrap()).unwrap();
        let c = connectors(&d);
        let got: Vec<_> = c.iter().map(|c| (c.name.as_str(), c.path)).collect();
        assert_eq!(
            got,
            [
                ("DP-1", Some(Path::Aux { ch: 5, pad: Some(5) })),
                ("DP-2", Some(Path::Aux { ch: 4, pad: Some(4) })),
                // The ASUS: nouveau read its EDID on AUX channel 3.
                ("DP-3", Some(Path::Aux { ch: 3, pad: Some(3) })),
                // The HP: I2C port 5 (register 0xd0b4), pad 2 (0xda10/0xda1c).
                ("HDMI-A-1", Some(Path::I2c { drive: 5, pad: Some(2) })),
            ]
        );
    }

    /// The whole DP probe against the trace: pad 3 found in AUX mode and
    /// off (the values nouveau read), DPCD + EDID replayed, and everything
    /// put back — pad powered off again, auto-DPCD set again.
    #[test]
    fn dp_probe_replays_the_trace_and_restores() {
        // The extract starts at transaction 2, after nouveau's transaction 0
        // had cleared auto-DPCD; put that transaction's read (`0x100fa`,
        // bit 16 set, as the GOP left it) in front.
        let mut m = ReplayMmio::from_extract(&format!("R 0xda58 0x000100fa\n{EXTRACT}"));
        m.fallback = alloc::vec![(0xda60, 0x23a2), (0xda6c, 1)];
        let conn = Connector { index: 2, kind: 0x46, name: "DP-3".into(), path: Some(Path::Aux { ch: 3, pad: Some(3) }) };
        let p = probe(&m, &conn);
        assert_eq!(p.status, Status::Connected, "{:?}", p.error);
        assert_eq!(p.edid, ASUS);
        let w = m.writes.borrow();
        assert_eq!(w[..2], [(0xda60, 0x23a2), (0xda6c, 0)]);
        // Bit 16 was set, so it is set again; then the pad goes off.
        let tail: Vec<_> = w.iter().rev().take(2).rev().copied().collect();
        assert_eq!(tail, [(0xda58, 0x100fa), (0xda6c, 1)]);

        let text = render(&[p]);
        assert!(text.starts_with("DP-3      connected    conn 2 aux 3  AUS VG279Q3A\n"), "{text}");
        assert!(text.contains("  dpcd: 1.4 HBR2 x4\n"), "{text}");
        assert!(text.contains("  preferred: 1920x1080@60.000 Hz\n"), "{text}");
        assert!(text.contains("  range: 48-180 Hz, 250-250 kHz, max 430 MHz\n"), "{text}");
        assert!(text.contains("1920x1080@179.821"), "{text}");
    }

    #[test]
    fn hdmi_probe_over_simulated_ddc() {
        let sim = DdcSim::new(5, Some(HP.try_into().unwrap()));
        // The sim only knows its port; pad registers come from a wrapper.
        struct WithPad<'a>(&'a DdcSim, core::cell::RefCell<Vec<(u32, u32)>>);
        impl Mmio for WithPad<'_> {
            fn rd32(&self, o: u32) -> u32 {
                match o {
                    0xda10 => 0xe3a1,
                    0xda1c => 1,
                    _ => self.0.rd32(o),
                }
            }
            fn wr32(&self, o: u32, v: u32) {
                if o == 0xda10 || o == 0xda1c {
                    self.1.borrow_mut().push((o, v));
                } else {
                    self.0.wr32(o, v)
                }
            }
            fn udelay(&self, _: u32) {}
        }
        let m = WithPad(&sim, Default::default());
        let conn = Connector { index: 3, kind: 0x61, name: "HDMI-A-1".into(), path: Some(Path::I2c { drive: 5, pad: Some(2) }) };
        let p = probe(&m, &conn);
        assert_eq!(p.status, Status::Connected, "{:?}", p.error);
        assert_eq!(p.edid, HP);
        // Pad 2: I2C mode kept, powered on, then off again (as nouveau).
        assert_eq!(*m.1.borrow(), [(0xda10, 0xe3a1), (0xda1c, 0), (0xda1c, 1)]);
        assert!(render(&[p]).starts_with("HDMI-A-1  connected    conn 3 i2c 5  HWP HP 2309\n"));

        // No monitor: NACK → disconnected, no error.
        let empty = DdcSim::new(5, None);
        let p = probe(&WithPad(&empty, Default::default()), &conn);
        assert_eq!((p.status, p.error), (Status::Disconnected, None));
    }
}
