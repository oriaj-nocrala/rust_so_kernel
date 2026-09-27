//! Chip identification from `PMC_BOOT_0`.
//!
//! Nouveau: `nvkm_device_ctor` reads register `0x000000`
//! (`nvkm/engine/device/base.c:3190`) and derives the chipset and revision
//! from it (`base.c:3213-3215`), the architecture from the chipset
//! (`base.c:3216-3250`) and the chip from a table (`base.c:3349`: `0x176`
//! is `nv176_chipset`, named "GA106" at `base.c:2628`). Paths are relative
//! to `drivers/gpu/drm/nouveau/` in the pinned Linux v7.2.2.

use crate::Mmio;

/// `PMC_BOOT_0` (`base.c:3190`).
pub const PMC_BOOT_0: u32 = 0x000000;

/// What `PMC_BOOT_0` says about the chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChipId {
    pub boot0: u32,
    /// `(boot0 & 0x1ff00000) >> 20` (`base.c:3214`): 0x176 on GA106.
    pub chipset: u16,
    /// `boot0 & 0xff` (`base.c:3215`).
    pub chiprev: u8,
}

impl ChipId {
    /// `None` when the register has none of the bits `base.c:3213` requires
    /// (pre-NV10 parts, or a dead BAR reading all zeros). All ones — a
    /// device that fell off the bus — decodes as chipset 0x1ff, which
    /// [`ChipId::arch`] does not know.
    pub fn decode(boot0: u32) -> Option<ChipId> {
        if boot0 & 0x1f00_0000 == 0 {
            return None;
        }
        Some(ChipId { boot0, chipset: ((boot0 & 0x1ff0_0000) >> 20) as u16, chiprev: boot0 as u8 })
    }

    pub fn read(mmio: &impl Mmio) -> Option<ChipId> {
        ChipId::decode(mmio.rd32(PMC_BOOT_0))
    }

    /// Nouveau's architecture name for `chipset & 0x1f0` (`base.c:3216-3250`),
    /// from Turing on (older parts are not this driver's business).
    pub fn arch(&self) -> Option<&'static str> {
        match self.chipset & 0x1f0 {
            0x160 => Some("TU100"),
            0x170 => Some("GA100"),
            0x180 => Some("GH100"),
            0x190 => Some("AD100"),
            _ => None,
        }
    }

    /// Nouveau's chip name, only for the one chip this driver supports.
    pub fn name(&self) -> Option<&'static str> {
        match self.chipset {
            0x176 => Some("GA106"),
            _ => None,
        }
    }

    /// Chip implementation within the architecture (`chipset & 0xf`: 6 on
    /// GA106). Phase 1's metal criterion reads "arch GA10x, impl 6".
    pub fn implementation(&self) -> u8 {
        (self.chipset & 0xf) as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mmio::testing::TableMmio;

    /// The value nouveau read on the target board (phase 0, `trace-nogsp`:
    /// `PMC_BOOT_0 = 0xb76000a1`).
    #[test]
    fn target_board_is_ga106() {
        let mmio = TableMmio::new(&[(PMC_BOOT_0, 0xb760_00a1)]);
        let id = ChipId::read(&mmio).unwrap();
        assert_eq!(id, ChipId { boot0: 0xb760_00a1, chipset: 0x176, chiprev: 0xa1 });
        assert_eq!(id.arch(), Some("GA100"));
        assert_eq!(id.name(), Some("GA106"));
        assert_eq!(id.implementation(), 6);
        assert_eq!(*mmio.reads.borrow(), [PMC_BOOT_0]);
        assert!(mmio.writes.borrow().is_empty());
    }

    #[test]
    fn dead_or_unknown() {
        assert_eq!(ChipId::decode(0), None);
        let ones = ChipId::decode(0xffff_ffff).unwrap();
        assert_eq!((ones.arch(), ones.name()), (None, None));
        // A GA102 (chipset 0x172) is the right family but not this chip.
        let ga102 = ChipId::decode(0xb72000a1).unwrap();
        assert_eq!((ga102.arch(), ga102.name()), (Some("GA100"), None));
    }
}
