# Phase 6d: extra interrupt sources in nvgpu::vblank.
#   scripts/gpu-mutate.py nvgpu/src/vblank.rs vblank:: nvgpu/mutations/vblank_extra.py
M = [
    ("(vector / 32, 1 << (vector % 32))", "(vector / 16, 1 << (vector % 32))"),
    ("(vector / 32, 1 << (vector % 32))", "(vector / 32, 1 << (vector % 16))"),
    ("(vector / 32, 1 << (vector % 32))", "(vector / 32, 2 << (vector % 32))"),
    ("        if leaf < VFN_LEAVES {\n            m.wr32(VFN_LEAF_STAT + leaf * 4, bit);\n            m.wr32(VFN_LEAF_ALLOW + leaf * 4, bit);", "        if leaf <= VFN_LEAVES {\n            m.wr32(VFN_LEAF_STAT + leaf * 4, bit);\n            m.wr32(VFN_LEAF_ALLOW + leaf * 4, bit);"),
    ("            m.wr32(VFN_LEAF_STAT + leaf * 4, bit);\n            m.wr32(VFN_LEAF_ALLOW + leaf * 4, bit);\n        }\n    }", "            m.wr32(VFN_LEAF_ALLOW + leaf * 4, bit);\n        }\n    }"),
    ("            m.wr32(VFN_LEAF_STAT + leaf * 4, bit);\n            m.wr32(VFN_LEAF_ALLOW + leaf * 4, bit);\n        }\n    }", "            m.wr32(VFN_LEAF_STAT + leaf * 4, bit);\n        }\n    }"),
    ("            m.wr32(VFN_LEAF_STAT + leaf * 4, bit);\n            m.wr32(VFN_LEAF_ALLOW + leaf * 4, bit);\n        }\n    }", "            m.wr32(VFN_LEAF_STAT + leaf * 4, bit);\n            m.wr32(VFN_LEAF_BLOCK + leaf * 4, bit);\n        }\n    }"),
    ("extra.iter().enumerate().take(32)", "extra.iter().enumerate().take(1)"),
    ("if leaf < VFN_LEAVES && stat[leaf as usize] & bit != 0 {", "if leaf <= VFN_LEAVES && stat[leaf as usize] & bit != 0 {"),
    ("if leaf < VFN_LEAVES && stat[leaf as usize] & bit != 0 {", "if leaf < VFN_LEAVES && stat[leaf as usize] & bit == 0 {"),
    ("out.extra |= 1 << i;", "out.extra = 1 << i;"),
    ("out.extra |= 1 << i;", "out.extra |= 1;"),
    ("            m.wr32(VFN_LEAF_STAT + leaf * 4, bit);\n            stat[leaf as usize] &= !bit;", "            stat[leaf as usize] &= !bit;"),
    ("            m.wr32(VFN_LEAF_STAT + leaf * 4, bit);\n            stat[leaf as usize] &= !bit;", "            m.wr32(VFN_LEAF_STAT + leaf * 4, bit);"),
    ("        let new = after[leaf] & !before[leaf];", "        let new = after[leaf] & before[leaf];"),
    ("        let new = after[leaf] & !before[leaf];", "        let new = after[leaf] | !before[leaf];"),
    ("v.push(leaf as u32 * 32 + bit);", "v.push(leaf as u32 * 31 + bit);"),
    ("        for bit in 0..32 {\n            if new & (1 << bit) != 0 {", "        for bit in 0..31 {\n            if new & (1 << bit) != 0 {"),
    ("        *x = m.rd32(VFN_LEAF_STAT + leaf as u32 * 4);", "        *x = m.rd32(VFN_LEAF_STAT + leaf as u32 * 8);"),
    ("        m.wr32(VFN_LEAF_STAT + leaf * 4, 0xffff_ffff);\n    }\n}\n\n/// The vectors", "        m.wr32(VFN_LEAF_STAT + leaf * 4, 0xffff_fffe);\n    }\n}\n\n/// The vectors"),
    # EQUIVALENT (kept as documentation): every element of `leaf_stats`'s array is overwritten, so the
    # initial value cannot show: ("let mut s = [0u32; ...", "let mut s = [1u32; ...")
    ("        for leaf in 0..VFN_LEAVES {\n            if stat[leaf as usize] != 0 {\n                m.wr32(VFN_LEAF_BLOCK + leaf * 4, stat[leaf as usize]);\n                out.blocked = true;", "        for leaf in 0..VFN_LEAVES {\n            if stat[leaf as usize] != 0 {\n                m.wr32(VFN_LEAF_BLOCK + leaf * 4, stat[leaf as usize]);"),
]
