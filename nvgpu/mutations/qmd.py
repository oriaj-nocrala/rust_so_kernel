# scripts/gpu-mutate.py nvgpu/src/qmd.rs qmd:: nvgpu/mutations/qmd.py   (phase 7b: QMD V03_00 fields, SM86 shared-memory configs, the fill shader's constants)
# Equivalent mutants, documented (8 of 97 survive): `scribble_word` masking with 0x7ff (the callers use i < 1024 = 0x400); every
# range check that a wider field or the thread product enforces again (`put` refuses a value wider than its field: constant buffer
# size `1 << 14`, grid y/z `1 << 17`, local memory `1 << 25`, program address `>> 50`; block x/y limits of 1025, since the product
# of threads is capped at 1024 anyway).
# Every `(hi, lo)` range moves one bit each way; the values `build` sets, the alignment/limit checks of the SM config and the
# constants of the fill shader are changed one at a time. Equivalent mutants are documented in the plan, not chased.
import re, os
_src = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "src", "qmd.rs")).read()
_src = _src[:_src.index("#[cfg(test)]")]
M = []
for m in re.finditer(r"^(const \w+: (?:\(usize, usize\)) = )\((\d+), (\d+)\);", _src, re.M):
    head, hi, lo = m.group(1), int(m.group(2)), int(m.group(3))
    old = m.group(0)
    M.append((old, "%s(%d, %d);" % (head, hi + 1, lo)))
    M.append((old, "%s(%d, %d);" % (head, hi, lo - 1)))
M += [
    ("const CTA_THREAD_DIMENSION: [(usize, usize); 3] = [(607, 592), (623, 608), (639, 624)];", "const CTA_THREAD_DIMENSION: [(usize, usize); 3] = [(607, 592), (639, 624), (623, 608)];"),
    ("(640 + i, 640 + i)", "(641 + i, 641 + i)"),
    ("(1055 + i * 64, 1024 + i * 64)", "(1055 + i * 64, 1023 + i * 64)"),
    ("(1072 + i * 64, 1056 + i * 64)", "(1072 + i * 64, 1055 + i * 64)"),
    ("(1087 + i * 64, 1075 + i * 64)", "(1087 + i * 64, 1074 + i * 64)"),
    ("(1055 + i * 64, 1024 + i * 64)", "(1055 + i * 32, 1024 + i * 32)"),
    ("put(&mut q, QMD_MAJOR_VERSION, 3);", "put(&mut q, QMD_MAJOR_VERSION, 2);"),
    ("put(&mut q, API_VISIBLE_CALL_LIMIT, 1);", "put(&mut q, API_VISIBLE_CALL_LIMIT, 0);"),
    ("put(&mut q, SM_GLOBAL_CACHING_ENABLE, 1);", "put(&mut q, SM_GLOBAL_CACHING_ENABLE, 0);"),
    ("put(&mut q, cbuf_valid(0), 1);", "put(&mut q, cbuf_valid(0), 0);"),
    ("put(&mut q, cbuf_size_shifted4(0), (cb_size >> 4) as u64);", "put(&mut q, cbuf_size_shifted4(0), (cb_size >> 5) as u64);"),
    ("put(&mut q, RELEASE0_MEMBAR_TYPE, 1);", "put(&mut q, RELEASE0_MEMBAR_TYPE, 0);"),
    ("put(&mut q, RELEASE0_ENABLE, 1);", "put(&mut q, RELEASE0_ENABLE, 0);"),
    ("put(&mut q, RELEASE0_STRUCTURE_SIZE, 1);", "put(&mut q, RELEASE0_STRUCTURE_SIZE, 0);"),
    ("put(&mut q, PROGRAM_ADDRESS_UPPER, l.program >> 32);", "put(&mut q, PROGRAM_ADDRESS_UPPER, l.program >> 31);"),
    ("put(&mut q, cbuf_addr_upper(0), cb_addr >> 32);", "put(&mut q, cbuf_addr_upper(0), cb_addr >> 33);"),
    ("put(&mut q, RELEASE0_ADDRESS_UPPER, addr >> 32);", "put(&mut q, RELEASE0_ADDRESS_UPPER, addr >> 33);"),
    ("let (min, target, max) = sm_config(l.smem, 48 / warps.max(1));", "let (min, target, max) = sm_config(l.smem, 24 / warps.max(1));"),
    ("let warps = (threads as u32).div_ceil(32);", "let warps = (threads as u32) / 32;"),
    ("pub const SM86_SMEM_KB: [u16; 6] = [0, 8, 16, 32, 64, 100];", "pub const SM86_SMEM_KB: [u16; 6] = [0, 8, 16, 32, 64, 96];"),
    ("pub const SM86_SMEM_KB: [u16; 6] = [0, 8, 16, 32, 64, 100];", "pub const SM86_SMEM_KB: [u16; 6] = [0, 8, 16, 32, 48, 100];"),
    ("(kb / 4) as u64 + 1", "(kb / 4) as u64"),
    ("let workgroups = workgroups.min(highest as u32 * 1024 / size.max(1));", "let workgroups = workgroups;"),
    (".find(|&&v| v as u32 * 1024 >= size * workgroups)", ".find(|&&v| v as u32 * 1024 > size * workgroups)"),
    ("let min = pick_smem_kb(smem, &SM86_SMEM_KB, 1);", "let min = pick_smem_kb(smem, &SM86_SMEM_KB, 2);"),
    ("let target = pick_smem_kb(smem, &SM86_SMEM_KB, workgroups_per_sm);", "let target = pick_smem_kb(smem, &SM86_SMEM_KB, 1);"),
    ("let max = *SM86_SMEM_KB.last().unwrap();", "let max = SM86_SMEM_KB[SM86_SMEM_KB.len() - 2];"),
    ("pub const QMD_ALIGN: u64 = 256;", "pub const QMD_ALIGN: u64 = 128;"),
    ("pub const PROGRAM_ALIGN: u64 = 0x80;", "pub const PROGRAM_ALIGN: u64 = 0x40;"),
    ("pub const FILL_REGISTERS: u8 = 8;", "pub const FILL_REGISTERS: u8 = 9;"),
    ("pub const FILL_PARAM: usize = 0x160;", "pub const FILL_PARAM: usize = 0x168;"),
    ("pub const FILL_CBUF0_BYTES: u32 = 0x200;", "pub const FILL_CBUF0_BYTES: u32 = 0x100;"),
    ("pub const FILL_BLOCK: u16 = 32;", "pub const FILL_BLOCK: u16 = 64;"),
    ("i.wrapping_mul(0x9e37_79b1) ^ 0xc0de_0000", "i.wrapping_mul(0x9e37_79b1) ^ 0xc0de_0001"),
    ("i.wrapping_mul(0x9e37_79b1) ^ 0xc0de_0000", "i.wrapping_mul(0x9e37_79b3) ^ 0xc0de_0000"),
    ("0x5c21_b000 | (i & 0xfff)", "0x5c21_b000 | (i & 0x7ff)"),
    ("pub const FILL_CTAS: u32 = 8;", "pub const FILL_CTAS: u32 = 4;"),
    ("c[FILL_PARAM..FILL_PARAM + 8].copy_from_slice(&out_va.to_le_bytes());", "c[FILL_PARAM..FILL_PARAM + 8].copy_from_slice(&(out_va >> 8).to_le_bytes());"),
    ("assert!(l.smem % 0x100 == 0,", "assert!(l.smem % 0x80 == 0,"),
    ("assert!(cb_size > 0 && cb_size % 16 == 0 && cb_size >> 4 < 1 << 13,", "assert!(cb_size > 0 && cb_size % 16 == 0 && cb_size >> 4 < 1 << 14,"),
    ("l.block[0] <= 1024 && l.block[1] <= 1024 && l.block[2] <= 64 && threads <= 1024", "l.block[0] <= 1025 && l.block[1] <= 1024 && l.block[2] <= 64 && threads <= 1024"),
    ("l.block[0] <= 1024 && l.block[1] <= 1024 && l.block[2] <= 64 && threads <= 1024", "l.block[0] <= 1024 && l.block[1] <= 1025 && l.block[2] <= 64 && threads <= 1024"),
    ("l.block[0] <= 1024 && l.block[1] <= 1024 && l.block[2] <= 64 && threads <= 1024", "l.block[0] <= 1024 && l.block[1] <= 1024 && l.block[2] <= 65 && threads <= 1024"),
    ("l.block[0] <= 1024 && l.block[1] <= 1024 && l.block[2] <= 64 && threads <= 1024", "l.block[0] <= 1024 && l.block[1] <= 1024 && l.block[2] <= 64 && threads <= 1025"),
    ("l.grid[1] < 1 << 16 && l.grid[2] < 1 << 16,", "l.grid[1] < 1 << 17 && l.grid[2] < 1 << 16,"),
    ("l.grid[1] < 1 << 16 && l.grid[2] < 1 << 16,", "l.grid[1] < 1 << 16 && l.grid[2] < 1 << 17,"),
    ("assert!(l.local % 16 == 0 && l.local < 1 << 24,", "assert!(l.local % 16 == 0 && l.local < 1 << 25,"),
    ("assert!(l.local % 16 == 0 && l.local < 1 << 24,", "assert!(l.local % 8 == 0 && l.local < 1 << 24,"),
    ("l.program % PROGRAM_ALIGN == 0 && l.program >> 49 == 0", "l.program % PROGRAM_ALIGN == 0 && l.program >> 50 == 0"),
    ("pub const COPY_PARAM_SRC: usize = 0x168;", "pub const COPY_PARAM_SRC: usize = 0x170;"),
    ("pub const COPY_REGISTERS: u8 = 8;", "pub const COPY_REGISTERS: u8 = 7;"),
    ("pub const FILLWT_XOR: u32 = 0x1234_5678;", "pub const FILLWT_XOR: u32 = 0x1234_5679;"),
    ("pub const FILLWT_REGISTERS: u8 = 8;", "pub const FILLWT_REGISTERS: u8 = 9;"),
    ("fill_word(i) ^ FILLWT_XOR", "fill_word(i)"),
    ("c[COPY_PARAM_SRC..COPY_PARAM_SRC + 8].copy_from_slice(&src_va.to_le_bytes());", "c[COPY_PARAM_SRC..COPY_PARAM_SRC + 8].copy_from_slice(&dst_va.to_le_bytes());"),
]
