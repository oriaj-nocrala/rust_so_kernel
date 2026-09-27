//! Boot options: `key=value` words read from `/mnt/etc/kernel.conf` and,
//! during an unattended metal run, `/mnt/autorun/kernel.conf` (see
//! `docs/reference/gpu.md`). UEFI gives this kernel no command line, so
//! these files are the command line.
//!
//! Format: words separated by whitespace or newlines; `#` starts a comment
//! that runs to the end of the line. A later file's value for a key
//! replaces an earlier one's. Unknown keys are kept (and reported), so a
//! typo is visible in the log instead of silently ignored.

use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BootOpts {
    pairs: Vec<(String, String)>,
}

impl BootOpts {
    /// Merges one file's text in; returns the words that are not
    /// `key=value` (reported by the caller, otherwise ignored).
    pub fn merge(&mut self, text: &str) -> Vec<String> {
        let mut bad = Vec::new();
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("");
            for word in line.split_whitespace() {
                match word.split_once('=') {
                    Some((k, v)) if !k.is_empty() => {
                        if let Some(p) = self.pairs.iter_mut().find(|(pk, _)| pk == k) {
                            p.1 = v.into();
                        } else {
                            self.pairs.push((k.into(), v.into()));
                        }
                    }
                    _ => bad.push(word.into()),
                }
            }
        }
        bad
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    pub fn pairs(&self) -> impl Iterator<Item = (&str, &str)> {
        self.pairs.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// How far the GPU driver may go (`gpu=`, `docs/gpu/gpu-plan.md`,
/// principle 6). Ordered: each level includes the ones before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GpuLevel {
    /// Default: the GPU is not touched at all.
    Off,
    /// Phase 1: read configuration space, size and map the BARs, read
    /// `PMC_BOOT_0`. Writes nothing to the GPU's registers.
    Probe,
    /// Phase 2: also read the VBIOS (PROM), its DCB, and each connector's
    /// DPCD/EDID. Writes only the AUX and I2C transactions nouveau makes
    /// for that, and puts back every register it changes.
    Disp,
    /// Phase 3: also arm the display's vblank interrupt (MSI) on the heads
    /// the firmware lit, and serve `/dev/vblank`. Leaves interrupts
    /// enabled in the GPU and bus mastering on (MSI is a memory write).
    Vblank,
}

impl GpuLevel {
    /// `None` for a value this kernel does not know yet (the caller logs it
    /// and stays `Off`: a later phase's level on an older kernel must not
    /// be read as a smaller one).
    pub fn parse(v: &str) -> Option<GpuLevel> {
        match v {
            "off" => Some(GpuLevel::Off),
            "probe" => Some(GpuLevel::Probe),
            "disp" => Some(GpuLevel::Disp),
            "vblank" => Some(GpuLevel::Vblank),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn later_file_wins_and_comments_are_ignored() {
        let mut o = BootOpts::default();
        assert!(o.merge("gpu=off  # default\nfoo=1 bar=\n").is_empty());
        assert_eq!(o.merge("# metal run\ngpu=probe\n"), Vec::<String>::new());
        assert_eq!(o.get("gpu"), Some("probe"));
        assert_eq!(o.get("foo"), Some("1"));
        assert_eq!(o.get("bar"), Some(""));
        assert_eq!(o.get("baz"), None);
        assert_eq!(o.pairs().count(), 3);
    }

    #[test]
    fn malformed_words_are_reported() {
        let mut o = BootOpts::default();
        assert_eq!(o.merge("gpu =probe =x ok=1"), ["gpu", "=probe", "=x"]);
        assert_eq!(o.get("ok"), Some("1"));
        assert_eq!(o.get("gpu"), None);
    }

    #[test]
    fn gpu_levels() {
        assert_eq!(GpuLevel::parse("off"), Some(GpuLevel::Off));
        assert_eq!(GpuLevel::parse("probe"), Some(GpuLevel::Probe));
        assert_eq!(GpuLevel::parse("disp"), Some(GpuLevel::Disp));
        assert_eq!(GpuLevel::parse("vblank"), Some(GpuLevel::Vblank));
        assert_eq!(GpuLevel::parse("gsp"), None);
        assert!(GpuLevel::Vblank > GpuLevel::Disp && GpuLevel::Disp > GpuLevel::Probe && GpuLevel::Probe > GpuLevel::Off);
    }
}
