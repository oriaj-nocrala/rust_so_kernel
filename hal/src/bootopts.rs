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

/// How far the Realtek NIC driver may go (`nic=`, `docs/net/rtl8168.md`).
/// Ordered: each level includes the ones before it. Does not affect virtio-net
/// (QEMU), which is always brought up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NicLevel {
    /// Default: the NIC is not touched at all.
    Off,
    /// Read-only: size and map BAR2, read the registers (XID, MAC, PHY
    /// status) and log them with a hex dump of the window. Writes nothing
    /// to the device (it only turns on the PCI memory decode if firmware
    /// left it off).
    Probe,
    /// Also reset the chip, restart auto-negotiation and wait for link. No
    /// DMA: bus mastering stays off, no rings, no packets.
    Reset,
    /// Also the rings, TX/RX and the network stack on it (DHCP, sockets),
    /// interrupt-driven through MSI-X or MSI (polled when neither works).
    Net,
    /// `Net` without interrupts: the 100 Hz tick drives the NIC. The way back
    /// if an interrupt misbehaves on a machine.
    NetPoll,
}

impl NicLevel {
    /// `None` for a value this kernel does not know.
    pub fn parse(v: &str) -> Option<NicLevel> {
        match v {
            "off" => Some(NicLevel::Off),
            "probe" => Some(NicLevel::Probe),
            "reset" => Some(NicLevel::Reset),
            "net" => Some(NicLevel::Net),
            "netpoll" => Some(NicLevel::NetPoll),
            _ => None,
        }
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
    /// Phase 5.1: also read the display's ARMED method state (core and
    /// window 0) before arming vblank, and publish it in `/proc/dispstate`.
    /// Reads only.
    Dispstate,
    /// Phase 5.2: also, before arming vblank, bring up the display's
    /// instance memory (VRAM, through PRAMIN), the core channel and window
    /// 0 (push buffers in host memory), and push one UPDATE on each that
    /// repeats the state the GOP left: the image must not change. Leaves
    /// both channels running and bus mastering on.
    Chan,
    /// Phase 5.3: also scan out from two buffers of this kernel's own in
    /// VRAM (BAR1, write-combining) instead of the GOP framebuffer, same
    /// mode; the framebuffer copies its RAM shadow there and `/dev/fb0`'s
    /// `FBIO_FLUSH` becomes a page flip at the next vblank.
    Scanout,
    /// Phase 5.4: also service the display's supervisor interrupts (the
    /// three steps of a core UPDATE that changes what drives a head), and
    /// open `/dev/dispctl`, whose `detach`/`attach` take the primary head's
    /// SOR off and put it back at the same mode.
    Super,
    /// Phase 5.5: also program the heads' pixel clocks (VPLL) in
    /// supervisor 2.1, and let `/dev/dispctl`'s `clock <kHz>` change the
    /// primary head's pixel clock on the same raster.
    Vpll,
    /// Phase 5.6: also let `/dev/dispctl`'s `train <lanes> <rate>` retrain
    /// the DP link of the primary head's output while its SOR is detached
    /// (VBIOS DP scripts, SOR lane setup, training over AUX).
    Dplink,
    /// Phase 5.7: also let `/dev/dispctl`'s `mode WxH@Hz` set the primary
    /// head to one of the monitor's EDID modes or a CVT-RB2 one (same size
    /// as the framebuffer), retraining the DP link when the mode needs it.
    Modes,
    /// Phase 5.8: also bring up the HP on HDMI (head 1, SOR-0, window 2)
    /// with a picture of the kernel's own: `/dev/dispctl`'s `hdmi on` /
    /// `hdmi off`.
    Hdmi,
    /// Phase 4c: also run FWSEC-FRTS (the VBIOS's signed microcode) on the
    /// GSP falcon, which carves the protected memory region (WPR2) the GSP's
    /// own boot needs. Runs once at boot; changes no display state.
    Fwsec,
    /// Phases 4d + 4e: also build everything GSP-RM's boot reads (firmware
    /// behind radix3, WPR meta, LibOS arguments and logs, queues), reset the
    /// GSP into RISC-V mode, run the booter on SEC2 and check the RISC-V core.
    /// Needs `gsp-570.144.bin` on the stick (`scripts/sync-usb-data.sh`). No
    /// RPC yet.
    Gsp,
    /// Phase 6b: also give our RM client a GPU virtual address space: page
    /// tables built in VRAM (`nvgpu::mmu`) and an externally owned
    /// `FERMI_VASPACE_A` whose page directory is ours. Implies `gsp`.
    Vaspace,
    /// Phase 6c: also a GPFIFO channel on the Ampere copy engine and a
    /// measured system -> VRAM -> system copy through it. Implies `vaspace`.
    Copy,
    /// Phase 7a: also a GR channel with a golden context and the Ampere compute
    /// class, and a semaphore release and inline writes through it. Implies `copy`.
    Compute,
    /// G4c: also keep that GR channel and the GPU page tables for `/dev/nvgpu`: user space binds its own buffers at run time and
    /// submits pushes to the channel (`kernel/src/gpu/uapi.rs`). Implies `compute`.
    Uapi,
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
            "dispstate" => Some(GpuLevel::Dispstate),
            "chan" => Some(GpuLevel::Chan),
            "scanout" => Some(GpuLevel::Scanout),
            "super" => Some(GpuLevel::Super),
            "vpll" => Some(GpuLevel::Vpll),
            "dplink" => Some(GpuLevel::Dplink),
            "modes" => Some(GpuLevel::Modes),
            "hdmi" => Some(GpuLevel::Hdmi),
            "fwsec" => Some(GpuLevel::Fwsec),
            "gsp" => Some(GpuLevel::Gsp),
            "vaspace" => Some(GpuLevel::Vaspace),
            "copy" => Some(GpuLevel::Copy),
            "compute" => Some(GpuLevel::Compute),
            "uapi" => Some(GpuLevel::Uapi),
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
        assert_eq!(GpuLevel::parse("dispstate"), Some(GpuLevel::Dispstate));
        assert_eq!(GpuLevel::parse("frobnicate"), None);
        assert_eq!(GpuLevel::parse("chan"), Some(GpuLevel::Chan));
        assert_eq!(GpuLevel::parse("scanout"), Some(GpuLevel::Scanout));
        assert_eq!(GpuLevel::parse("super"), Some(GpuLevel::Super));
        assert_eq!(GpuLevel::parse("vpll"), Some(GpuLevel::Vpll));
        assert_eq!(GpuLevel::parse("dplink"), Some(GpuLevel::Dplink));
        assert!(GpuLevel::Dplink > GpuLevel::Vpll);
        assert_eq!(GpuLevel::parse("modes"), Some(GpuLevel::Modes));
        assert!(GpuLevel::Modes > GpuLevel::Dplink);
        assert_eq!(GpuLevel::parse("hdmi"), Some(GpuLevel::Hdmi));
        assert!(GpuLevel::Hdmi > GpuLevel::Modes);
        assert_eq!(GpuLevel::parse("fwsec"), Some(GpuLevel::Fwsec));
        assert!(GpuLevel::Fwsec > GpuLevel::Hdmi);
        assert_eq!(GpuLevel::parse("gsp"), Some(GpuLevel::Gsp));
        assert!(GpuLevel::Gsp > GpuLevel::Fwsec);
        assert_eq!(GpuLevel::parse("vaspace"), Some(GpuLevel::Vaspace));
        assert!(GpuLevel::Vaspace > GpuLevel::Gsp);
        assert_eq!(GpuLevel::parse("copy"), Some(GpuLevel::Copy));
        assert!(GpuLevel::Copy > GpuLevel::Vaspace);
        assert_eq!(GpuLevel::parse("compute"), Some(GpuLevel::Compute));
        assert!(GpuLevel::Compute > GpuLevel::Copy);
        assert_eq!(GpuLevel::parse("uapi"), Some(GpuLevel::Uapi));
        assert!(GpuLevel::Uapi > GpuLevel::Compute);
        assert!(GpuLevel::Vpll > GpuLevel::Super);
        assert!(GpuLevel::Super > GpuLevel::Scanout && GpuLevel::Scanout > GpuLevel::Chan);
        assert!(GpuLevel::Chan > GpuLevel::Dispstate && GpuLevel::Dispstate > GpuLevel::Vblank);
        assert!(GpuLevel::Vblank > GpuLevel::Disp && GpuLevel::Disp > GpuLevel::Probe && GpuLevel::Probe > GpuLevel::Off);
    }

    #[test]
    fn nic_levels() {
        assert_eq!(NicLevel::parse("off"), Some(NicLevel::Off));
        assert_eq!(NicLevel::parse("probe"), Some(NicLevel::Probe));
        assert_eq!(NicLevel::parse("reset"), Some(NicLevel::Reset));
        assert_eq!(NicLevel::parse("net"), Some(NicLevel::Net));
        assert_eq!(NicLevel::parse("netpoll"), Some(NicLevel::NetPoll));
        assert!(NicLevel::Net < NicLevel::NetPoll);
        assert_eq!(NicLevel::parse("on"), None);
        assert!(NicLevel::Off < NicLevel::Probe && NicLevel::Probe < NicLevel::Reset && NicLevel::Reset < NicLevel::Net);
    }
}
