# scripts/gpu-mutate.py nvgpu/src/mmu.rs mmu:: nvgpu/mutations/mmu_big.py   (phase 7a: 64 KiB pages, PTE address mask)
# Equivalent mutant, documented: `if pde != 0 && pde & PTE_VALID == 0` -> `if pde != 0` (a valid 2 MiB PTE has already been
# returned by the block above, so no PTE reaches this test).
M = [
    ("pub const BIG_PAGE: u64 = 64 << 10;", "pub const BIG_PAGE: u64 = 32 << 10;"),
    ("const BIG_ENTRIES: usize = 32;", "const BIG_ENTRIES: usize = 16;"),
    ("(e & 0x00ff_ffff_ffff_fff0) << 4\n}\n\n/// The physical address a PTE maps", "(e & 0x00ff_ffff_ffff_ff00) << 4\n}\n\n/// The physical address a PTE maps"),
    ("(e & 0x00ff_ffff_ffff_ff00) << 4\n}\n\nfn rd64", "(e & 0x00ff_ffff_ffff_fff0) << 4\n}\n\nfn rd64"),
    ("if rd64(&self.tables[t], ix[3] * 16) & PTE_VALID != 0 {\n            return Err(MapError::Overlap);\n        }\n        t = self.child(t, ix[3] * 16 + PD0_SMALL)?;", "if rd64(&self.tables[t], ix[3] * 16) != 0 {\n            return Err(MapError::Overlap);\n        }\n        t = self.child(t, ix[3] * 16 + PD0_SMALL)?;"),
    ("            return Err(MapError::Overlap); // a 2 MiB page covers this VA\n        }\n        t = self.child(t, ix[3] * 16)?;", "        }\n        t = self.child(t, ix[3] * 16)?;"),
    ("t = self.child(t, ix[3] * 16)?;\n        let at = ((va >> 16) as usize & (BIG_ENTRIES - 1)) * 8;", "t = self.child(t, ix[3] * 16 + PD0_SMALL)?;\n        let at = ((va >> 16) as usize & (BIG_ENTRIES - 1)) * 8;"),
    ("let at = ((va >> 16) as usize & (BIG_ENTRIES - 1)) * 8;\n        if rd64(&self.tables[t], at) != 0 {\n            return Err(MapError::AlreadyMapped);", "let at = ((va >> 16) as usize & (BIG_ENTRIES - 1)) * 8;\n        if false {\n            return Err(MapError::AlreadyMapped);"),
    ("if va & (BIG_PAGE - 1) != 0 || pa & (BIG_PAGE - 1) != 0 {", "if va & (BIG_PAGE - 1) != 0 {"),
    ("if va & (BIG_PAGE - 1) != 0 || pa & (BIG_PAGE - 1) != 0 {", "if pa & (BIG_PAGE - 1) != 0 {"),
    ("if len & (BIG_PAGE - 1) != 0 {\n            return Err(MapError::Unaligned);\n        }\n        let mut off = 0;\n        while off < len {\n            self.map_big(va + off, pa + off, target, f)?;\n            off += BIG_PAGE;", "if len & (BIG_PAGE - 1) != 0 {\n            return Err(MapError::Unaligned);\n        }\n        let mut off = 0;\n        while off < len {\n            self.map_big(va + off, pa + off, target, f)?;\n            off += 0x1000;"),
    ("if pde != 0 && pde & PTE_VALID == 0 {", "if pde != 0 {"),
    ("return Some((pte_addr(e) | (va & (BIG_PAGE - 1)), e));", "return Some((pte_addr(e) | (va & 0xfff), e));"),
    ("return Some((pte_addr(e) | (va & (HUGE_PAGE - 1)), e));", "return Some((entry_addr(e) | (va & (HUGE_PAGE - 1)), e));"),
    ("(e & PTE_VALID != 0).then(|| (pte_addr(e) | (va & 0xfff), e))", "(e & PTE_VALID != 0).then(|| (entry_addr(e) | (va & 0xfff), e))"),
    ("let e = rd64(&self.tables[bt], ((va >> 16) as usize & (BIG_ENTRIES - 1)) * 8);\n                    if e & PTE_VALID != 0 {", "let e = rd64(&self.tables[bt], ((va >> 16) as usize & (BIG_ENTRIES - 1)) * 8);\n                    if e != 0 {"),
]
