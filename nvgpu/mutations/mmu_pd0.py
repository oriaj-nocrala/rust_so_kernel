# Example for scripts/gpu-mutate.py: the PD0 dual-PDE halves (the bug of boots #101-#104).
#   scripts/gpu-mutate.py nvgpu/src/mmu.rs mmu:: nvgpu/mutations/mmu_pd0.py
M = [
    ("pub const PD0_SMALL: usize = 8;", "pub const PD0_SMALL: usize = 0;"),
    ("pub const PD0_SMALL: usize = 8;", "pub const PD0_SMALL: usize = 4;"),
    ("if level == 3 { i * 16 + PD0_SMALL } else { i * 8 }", "if level == 3 { i * 16 } else { i * 8 }"),
    ("if level == 3 { i * 16 + PD0_SMALL } else { i * 8 }", "if level == 2 { i * 16 + PD0_SMALL } else { i * 8 }"),
    ("t = self.child(t, ix[3] * 16 + PD0_SMALL)?;", "t = self.child(t, ix[3] * 16)?;"),
    ("t = self.child(t, ix[3] * 16 + PD0_SMALL)?;", "t = self.child(t, ix[3] * 8 + PD0_SMALL)?;"),
]
