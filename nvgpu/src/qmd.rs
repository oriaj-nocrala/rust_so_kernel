//! The compute launch descriptor (phase 7b of `docs/gpu/gpu-plan.md`): a QMD V03_00 and the one shader the kernel runs.
//!
//! A compute launch on Ampere is a 256-byte *queue meta data* block in memory (`SEND_PCAS_A` names its address `>> 8`,
//! `SEND_SIGNALING_PCAS2_B` schedules it) that says where the program is, how many CTAs of how many threads, how many
//! registers, which constant buffers are bound and, optionally, a semaphore the grid releases when it is done. The layout
//! is `clc6c0qmd.h` "Version 03_00" (`open-gpu-doc/classes/compute`), the version Mesa's NAK fills for `AMPERE_COMPUTE_A`
//! and later (`nak/qmd.rs`, `Qmd3_0`); `AMPERE_COMPUTE_B` (`0xc7c0`, the GA10x class) has the same struct.
//!
//! Pure: numbers in, words out. The oracle is `nvgpu/gen/qmd.c`, which builds the same block from the header's `MW(hi:lo)`
//! ranges with clang (`fixtures/qmd-fill.txt`); the shader is the SASS `ptxas -arch=sm_86` makes of `gen/shader/fill.cu`
//! (`fixtures/shader-fill-sm86.bin`, disassembly in `docs/gpu/gpu-plan.md`).

use alloc::vec::Vec;

pub const QMD_WORDS: usize = 64;
pub const QMD_BYTES: usize = QMD_WORDS * 4;
/// `SEND_PCAS_A` carries the address `>> 8`.
pub const QMD_ALIGN: u64 = 256;
/// A program address: NAK aligns shader uploads to 0x80 (`hw_runner.rs`, `next_multiple_of(0x80)`).
pub const PROGRAM_ALIGN: u64 = 0x80;

// `clc6c0qmd.h`, `NVC6C0_QMDV03_00_*`: (hi, lo) of each `MW(hi:lo)`.
const QMD_VERSION: (usize, usize) = (579, 576);
const QMD_MAJOR_VERSION: (usize, usize) = (583, 580);
const API_VISIBLE_CALL_LIMIT: (usize, usize) = (378, 378); // NO_CHECK = 1
const SAMPLER_INDEX: (usize, usize) = (382, 382); // INDEPENDENTLY = 0
const SM_GLOBAL_CACHING_ENABLE: (usize, usize) = (134, 134);
const BARRIER_COUNT: (usize, usize) = (767, 763);
const CTA_RASTER_WIDTH: (usize, usize) = (415, 384);
const CTA_RASTER_HEIGHT: (usize, usize) = (431, 416);
const CTA_RASTER_DEPTH: (usize, usize) = (463, 448);
const CTA_THREAD_DIMENSION: [(usize, usize); 3] = [(607, 592), (623, 608), (639, 624)];
const PROGRAM_ADDRESS_LOWER: (usize, usize) = (1567, 1536);
const PROGRAM_ADDRESS_UPPER: (usize, usize) = (1584, 1568);
const REGISTER_COUNT_V: (usize, usize) = (656, 648);
const SHADER_LOCAL_MEMORY_LOW_SIZE: (usize, usize) = (759, 736);
const SHADER_LOCAL_MEMORY_HIGH_SIZE: (usize, usize) = (1623, 1600);
const SHARED_MEMORY_SIZE: (usize, usize) = (561, 544);
const MIN_SM_CONFIG_SHARED_MEM_SIZE: (usize, usize) = (567, 562);
const MAX_SM_CONFIG_SHARED_MEM_SIZE: (usize, usize) = (574, 569);
const TARGET_SM_CONFIG_SHARED_MEM_SIZE: (usize, usize) = (662, 657);
const RELEASE0_ADDRESS_LOWER: (usize, usize) = (799, 768);
const RELEASE0_ADDRESS_UPPER: (usize, usize) = (807, 800);
const RELEASE0_MEMBAR_TYPE: (usize, usize) = (819, 819); // FE_SYSMEMBAR = 1
const RELEASE0_ENABLE: (usize, usize) = (823, 823);
const RELEASE0_STRUCTURE_SIZE: (usize, usize) = (831, 830); // SEMAPHORE_ONE_WORD = 1
const RELEASE0_PAYLOAD_LOWER: (usize, usize) = (863, 832);

/// `CONSTANT_BUFFER_VALID(i)`, `_ADDR_LOWER(i)`, `_ADDR_UPPER(i)` and `_SIZE_SHIFTED4(i)` (eight buffers; only 0 is used).
fn cbuf_valid(i: usize) -> (usize, usize) {
    (640 + i, 640 + i)
}
fn cbuf_addr_lower(i: usize) -> (usize, usize) {
    (1055 + i * 64, 1024 + i * 64)
}
fn cbuf_addr_upper(i: usize) -> (usize, usize) {
    (1072 + i * 64, 1056 + i * 64)
}
fn cbuf_size_shifted4(i: usize) -> (usize, usize) {
    (1087 + i * 64, 1075 + i * 64)
}

/// Set the bits `hi..=lo` of the QMD (the header counts bits from the start of word 0) to `v`.
fn put(q: &mut [u32; QMD_WORDS], (hi, lo): (usize, usize), v: u64) {
    let width = hi - lo + 1;
    assert!(hi < QMD_WORDS * 32 && width <= 64, "MW({}:{}) is outside the QMD", hi, lo);
    assert!(width == 64 || v >> width == 0, "{:#x} does not fit MW({}:{})", v, hi, lo);
    for b in lo..=hi {
        let (w, bit) = (b / 32, b % 32);
        q[w] = (q[w] & !(1 << bit)) | ((((v >> (b - lo)) & 1) as u32) << bit);
    }
}

/// What a launch needs. Everything else in the QMD is zero or the fixed values NAK sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Launch {
    /// GPU VA of the first instruction.
    pub program: u64,
    /// Registers per thread (`SHI_REGISTERS` of the cubin).
    pub registers: u8,
    /// CTAs in x, y, z.
    pub grid: [u32; 3],
    /// Threads in x, y, z.
    pub block: [u16; 3],
    /// Static shared memory, bytes.
    pub smem: u32,
    /// Per-thread local memory, bytes (0: the shader spills nothing).
    pub local: u32,
    /// Constant buffer 0: GPU VA and size in bytes.
    pub cbuf0: (u64, u32),
    /// A semaphore the grid releases when its last CTA is done: GPU VA and the payload (one word).
    pub release: Option<(u64, u32)>,
}

/// GA106 is SM86: `nouveau_device.c:init_shared_mem_sizes` (`sm == 86`): shared memory per SM in KiB.
pub const SM86_SMEM_KB: [u16; 6] = [0, 8, 16, 32, 64, 100];

/// `gv100_smem_size_to_hw` (`nak/qmd.rs`).
fn smem_to_hw(kb: u16) -> u64 {
    assert!(kb % 4 == 0);
    (kb / 4) as u64 + 1
}

/// `gv100_pick_smem_size_kb`: the smallest configuration that holds `workgroups` workgroups of `size` bytes.
fn pick_smem_kb(size: u32, sizes: &[u16], workgroups: u32) -> u16 {
    let highest = *sizes.last().unwrap();
    let workgroups = workgroups.min(highest as u32 * 1024 / size.max(1));
    assert!(workgroups > 0, "the shared memory asked for does not fit the SM");
    *sizes.iter().find(|&&v| v as u32 * 1024 >= size * workgroups).unwrap_or(&highest)
}

/// `gv100_get_hw_smem_sizes`: the (MIN, TARGET, MAX) `SM_CONFIG_SHARED_MEM_SIZE` of a workgroup that uses `smem` bytes
/// when the SM can hold `workgroups_per_sm` of them.
pub fn sm_config(smem: u32, workgroups_per_sm: u32) -> (u64, u64, u64) {
    let min = pick_smem_kb(smem, &SM86_SMEM_KB, 1);
    let target = pick_smem_kb(smem, &SM86_SMEM_KB, workgroups_per_sm);
    let max = *SM86_SMEM_KB.last().unwrap();
    (smem_to_hw(min), smem_to_hw(target), smem_to_hw(max))
}

/// Build the QMD (`Qmd3_0::new` + `fill_qmd` of `nak/qmd.rs`, plus the grid's release semaphore).
pub fn build(l: &Launch) -> [u32; QMD_WORDS] {
    assert!(l.program % PROGRAM_ALIGN == 0 && l.program >> 49 == 0, "program address {:#x}", l.program);
    assert!(l.grid.iter().all(|&g| g > 0) && l.grid[1] < 1 << 16 && l.grid[2] < 1 << 16, "grid {:?}", l.grid);
    // CUDA's limits for a CTA on SM86: 1024 x 1024 x 64 threads and 1024 in all
    let threads: u64 = l.block.iter().map(|&b| b as u64).product();
    assert!(l.block.iter().all(|&b| b > 0) && l.block[0] <= 1024 && l.block[1] <= 1024 && l.block[2] <= 64 && threads <= 1024, "block {:?}", l.block);
    assert!(l.local % 16 == 0 && l.local < 1 << 24, "local memory {}", l.local);
    let (cb_addr, cb_size) = l.cbuf0;
    assert!(cb_size > 0 && cb_size % 16 == 0 && cb_size >> 4 < 1 << 13, "constant buffer size {:#x}", cb_size);
    assert!(l.smem % 0x100 == 0, "shared memory {:#x} is not a multiple of 256", l.smem);
    let mut q = [0u32; QMD_WORDS];
    put(&mut q, QMD_MAJOR_VERSION, 3);
    put(&mut q, QMD_VERSION, 0);
    put(&mut q, API_VISIBLE_CALL_LIMIT, 1);
    put(&mut q, SAMPLER_INDEX, 0);
    put(&mut q, SM_GLOBAL_CACHING_ENABLE, 1);
    put(&mut q, BARRIER_COUNT, 0);
    put(&mut q, CTA_RASTER_WIDTH, l.grid[0] as u64);
    put(&mut q, CTA_RASTER_HEIGHT, l.grid[1] as u64);
    put(&mut q, CTA_RASTER_DEPTH, l.grid[2] as u64);
    for (d, f) in CTA_THREAD_DIMENSION.iter().enumerate() {
        put(&mut q, *f, l.block[d] as u64);
    }
    put(&mut q, PROGRAM_ADDRESS_LOWER, l.program & 0xffff_ffff);
    put(&mut q, PROGRAM_ADDRESS_UPPER, l.program >> 32);
    put(&mut q, REGISTER_COUNT_V, l.registers as u64);
    put(&mut q, SHADER_LOCAL_MEMORY_HIGH_SIZE, 0);
    put(&mut q, SHADER_LOCAL_MEMORY_LOW_SIZE, l.local as u64);
    put(&mut q, SHARED_MEMORY_SIZE, l.smem as u64);
    let warps = (threads as u32).div_ceil(32);
    // `max_warps_per_sm` for SM86 is 48 (1536 threads per SM)
    let (min, target, max) = sm_config(l.smem, 48 / warps.max(1));
    put(&mut q, MIN_SM_CONFIG_SHARED_MEM_SIZE, min);
    put(&mut q, MAX_SM_CONFIG_SHARED_MEM_SIZE, max);
    put(&mut q, TARGET_SM_CONFIG_SHARED_MEM_SIZE, target);
    put(&mut q, cbuf_addr_lower(0), cb_addr & 0xffff_ffff);
    put(&mut q, cbuf_addr_upper(0), cb_addr >> 32);
    put(&mut q, cbuf_size_shifted4(0), (cb_size >> 4) as u64);
    put(&mut q, cbuf_valid(0), 1);
    if let Some((addr, payload)) = l.release {
        put(&mut q, RELEASE0_ADDRESS_LOWER, addr & 0xffff_ffff);
        put(&mut q, RELEASE0_ADDRESS_UPPER, addr >> 32);
        put(&mut q, RELEASE0_MEMBAR_TYPE, 1);
        put(&mut q, RELEASE0_ENABLE, 1);
        put(&mut q, RELEASE0_STRUCTURE_SIZE, 1);
        put(&mut q, RELEASE0_PAYLOAD_LOWER, payload as u64);
    }
    q
}

/// The QMD as the bytes the GPU reads.
pub fn bytes(q: &[u32; QMD_WORDS]) -> Vec<u8> {
    q.iter().flat_map(|w| w.to_le_bytes()).collect()
}

// ---- the shader ----------------------------------------------------------------------------------

/// `gen/shader/fill.cu` for SM86: thread `i = ctaid.x * 32 + tid.x` stores `fill_word(i)` at `out[i]`, `out` being the
/// pointer parameter in constant buffer 0 (`FILL_PARAM`). 12 instructions (0xc0 bytes: the last is the `BRA` after `EXIT`)
/// and NOP padding to 0x180.
pub const FILL: &[u8] = include_bytes!("../fixtures/shader-fill-sm86.bin");
/// `SHI_REGISTERS` of the cubin.
pub const FILL_REGISTERS: u8 = 8;
/// CUDA's ABI: kernel parameters start at `c[0x0][0x160]` (sm_80+); `.nv.constant0.fill` is 0x168 bytes.
pub const FILL_PARAM: usize = 0x160;
pub const FILL_CBUF0_BYTES: u32 = 0x200;
/// The threads per CTA the source hard-codes (`blockIdx.x * 32`).
pub const FILL_BLOCK: u16 = 32;

/// What the shader stores for global thread index `i` (`out[i] = (i * 0x9e3779b1) ^ 0xc0de0000`).
pub fn fill_word(i: u32) -> u32 {
    i.wrapping_mul(0x9e37_79b1) ^ 0xc0de_0000
}

/// `gen/shader/copy.cu` for SM86: `dst[i] = ld.global.cg(src[i])` for the same global index; parameters `dst` at
/// `c[0x0][0x160]` and `src` at `c[0x0][0x168]`. The GPU's own view of a page (the L2's), to tell stores that did not land from
/// stores the CPU cannot see yet.
pub const COPY: &[u8] = include_bytes!("../fixtures/shader-copy-sm86.bin");
pub const COPY_REGISTERS: u8 = 8;
pub const COPY_PARAM_SRC: usize = 0x168;

/// Constant buffer 0 for the copy shader.
pub fn copy_cbuf0(dst_va: u64, src_va: u64) -> Vec<u8> {
    let mut c = fill_cbuf0(dst_va);
    c[COPY_PARAM_SRC..COPY_PARAM_SRC + 8].copy_from_slice(&src_va.to_le_bytes());
    c
}

/// `gen/shader/fillwt.cu`: `fill` with a system-scope store (`STG.E.STRONG.SYS`, from `__stwt`) and the words XOR
/// `FILLWT_XOR`, so that a line of the `fill` shader that is still dirty somewhere cannot pass for this one's result.
pub const FILLWT: &[u8] = include_bytes!("../fixtures/shader-fillwt-sm86.bin");
pub const FILLWT_REGISTERS: u8 = 8;
pub const FILLWT_XOR: u32 = 0x1234_5678;

pub fn fillwt_word(i: u32) -> u32 {
    fill_word(i) ^ FILLWT_XOR
}

/// The words a launch's output page starts with, so a word the grid did not write cannot pass for one it did.
pub fn scribble_word(i: u32) -> u32 {
    0x5c21_b000 | (i & 0xfff)
}

/// CTAs the launch of the fill shader has (8 x 32 threads = 256 words of the 1024 of the output page).
pub const FILL_CTAS: u32 = 8;
pub const FILL_WORDS: u32 = FILL_CTAS * FILL_BLOCK as u32;

/// Constant buffer 0 for the fill shader: zeros with the output pointer at `FILL_PARAM`.
pub fn fill_cbuf0(out_va: u64) -> Vec<u8> {
    let mut c = alloc::vec![0u8; FILL_CBUF0_BYTES as usize];
    c[FILL_PARAM..FILL_PARAM + 8].copy_from_slice(&out_va.to_le_bytes());
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORACLE: &str = include_str!("../fixtures/qmd-fill.txt");

    /// The dispatch `gen/qmd.c` builds (`kern` = `gr::KERN_VA`).
    fn fill_launch() -> Launch {
        let kern = 0x3_8004_0000u64;
        Launch {
            program: kern,
            registers: FILL_REGISTERS,
            grid: [8, 1, 1],
            block: [32, 1, 1],
            smem: 0,
            local: 0,
            cbuf0: (kern + 0x800, FILL_CBUF0_BYTES),
            release: Some((kern + 0x2010, 0x4242)),
        }
    }

    fn oracle() -> Vec<u32> {
        ORACLE
            .lines()
            .map(|l| {
                let (i, v) = l.split_once(' ').unwrap();
                assert_eq!(i.parse::<usize>().unwrap(), ORACLE.lines().position(|x| x == l).unwrap());
                u32::from_str_radix(v, 16).unwrap()
            })
            .collect()
    }

    #[test]
    fn the_qmd_is_the_one_clang_builds_from_the_header() {
        let want = oracle();
        assert_eq!(want.len(), QMD_WORDS);
        let got = build(&fill_launch());
        for i in 0..QMD_WORDS {
            assert_eq!(got[i], want[i], "word {}", i);
        }
        assert_eq!(bytes(&got).len(), QMD_BYTES);
    }

    const FIELDS: &str = include_str!("../fixtures/qmd-fields.txt");

    /// `NAME hi lo` of the header, or `NAME value` for an enum.
    fn header(name: &str) -> Vec<usize> {
        FIELDS
            .lines()
            .find_map(|l| {
                let mut it = l.split(' ');
                (it.next() == Some(name)).then(|| it.map(|x| x.parse().unwrap()).collect())
            })
            .unwrap_or_else(|| panic!("{} is not in the header's field list", name))
    }

    fn get(q: &[u32; QMD_WORDS], (hi, lo): (usize, usize)) -> u64 {
        (lo..=hi).fold(0u64, |v, b| v | ((((q[b / 32] >> (b % 32)) & 1) as u64) << (b - lo)))
    }

    #[test]
    fn every_field_range_is_the_headers() {
        let all = [
            ("QMD_VERSION", QMD_VERSION),
            ("QMD_MAJOR_VERSION", QMD_MAJOR_VERSION),
            ("API_VISIBLE_CALL_LIMIT", API_VISIBLE_CALL_LIMIT),
            ("SAMPLER_INDEX", SAMPLER_INDEX),
            ("SM_GLOBAL_CACHING_ENABLE", SM_GLOBAL_CACHING_ENABLE),
            ("BARRIER_COUNT", BARRIER_COUNT),
            ("CTA_RASTER_WIDTH", CTA_RASTER_WIDTH),
            ("CTA_RASTER_HEIGHT", CTA_RASTER_HEIGHT),
            ("CTA_RASTER_DEPTH", CTA_RASTER_DEPTH),
            ("CTA_THREAD_DIMENSION0", CTA_THREAD_DIMENSION[0]),
            ("CTA_THREAD_DIMENSION1", CTA_THREAD_DIMENSION[1]),
            ("CTA_THREAD_DIMENSION2", CTA_THREAD_DIMENSION[2]),
            ("PROGRAM_ADDRESS_LOWER", PROGRAM_ADDRESS_LOWER),
            ("PROGRAM_ADDRESS_UPPER", PROGRAM_ADDRESS_UPPER),
            ("REGISTER_COUNT_V", REGISTER_COUNT_V),
            ("SHADER_LOCAL_MEMORY_LOW_SIZE", SHADER_LOCAL_MEMORY_LOW_SIZE),
            ("SHADER_LOCAL_MEMORY_HIGH_SIZE", SHADER_LOCAL_MEMORY_HIGH_SIZE),
            ("SHARED_MEMORY_SIZE", SHARED_MEMORY_SIZE),
            ("MIN_SM_CONFIG_SHARED_MEM_SIZE", MIN_SM_CONFIG_SHARED_MEM_SIZE),
            ("MAX_SM_CONFIG_SHARED_MEM_SIZE", MAX_SM_CONFIG_SHARED_MEM_SIZE),
            ("TARGET_SM_CONFIG_SHARED_MEM_SIZE", TARGET_SM_CONFIG_SHARED_MEM_SIZE),
            ("RELEASE0_ADDRESS_LOWER", RELEASE0_ADDRESS_LOWER),
            ("RELEASE0_ADDRESS_UPPER", RELEASE0_ADDRESS_UPPER),
            ("RELEASE0_MEMBAR_TYPE", RELEASE0_MEMBAR_TYPE),
            ("RELEASE0_ENABLE", RELEASE0_ENABLE),
            ("RELEASE0_STRUCTURE_SIZE", RELEASE0_STRUCTURE_SIZE),
            ("RELEASE0_PAYLOAD_LOWER", RELEASE0_PAYLOAD_LOWER),
        ];
        for (name, (hi, lo)) in all {
            assert_eq!(header(name), vec![hi, lo], "{}", name);
        }
        for i in 0..8 {
            for (name, (hi, lo)) in [
                ("CONSTANT_BUFFER_VALID", cbuf_valid(i)),
                ("CONSTANT_BUFFER_ADDR_LOWER", cbuf_addr_lower(i)),
                ("CONSTANT_BUFFER_ADDR_UPPER", cbuf_addr_upper(i)),
                ("CONSTANT_BUFFER_SIZE_SHIFTED4", cbuf_size_shifted4(i)),
            ] {
                assert_eq!(header(&format!("{}({})", name, i)), vec![hi, lo], "{}({})", name, i);
            }
        }
    }

    #[test]
    fn the_fixed_values_are_the_headers_enums() {
        let q = build(&fill_launch());
        assert_eq!(get(&q, QMD_MAJOR_VERSION), 3);
        assert_eq!(get(&q, QMD_VERSION), 0);
        assert_eq!(get(&q, API_VISIBLE_CALL_LIMIT), header("API_VISIBLE_CALL_LIMIT_NO_CHECK")[0] as u64);
        assert_eq!(get(&q, SAMPLER_INDEX), header("SAMPLER_INDEX_INDEPENDENTLY")[0] as u64);
        assert_eq!(get(&q, RELEASE0_MEMBAR_TYPE), header("RELEASE0_MEMBAR_TYPE_FE_SYSMEMBAR")[0] as u64);
        assert_eq!(get(&q, RELEASE0_STRUCTURE_SIZE), header("RELEASE0_STRUCTURE_SIZE_SEMAPHORE_ONE_WORD")[0] as u64);
        assert_eq!(get(&q, RELEASE0_ENABLE), 1);
        assert_eq!(get(&q, SM_GLOBAL_CACHING_ENABLE), 1);
    }

    #[test]
    fn the_extremes_of_every_input_land_in_their_fields() {
        let l = Launch {
            program: 0x1_ffff_ffff_ff80,
            registers: 255,
            grid: [0xffff_ffff, 0xffff, 0xffff],
            block: [32, 1, 1],
            smem: 0,
            local: 0xff_fff0,
            cbuf0: (0xff_ffff_ffff, 0x1_fff0),
            release: Some((0xff_ffff_fffc, 0xffff_ffff)),
        };
        let q = build(&l);
        assert_eq!(get(&q, PROGRAM_ADDRESS_LOWER), 0xffff_ff80);
        assert_eq!(get(&q, PROGRAM_ADDRESS_UPPER), 0x1_ffff);
        assert_eq!(get(&q, REGISTER_COUNT_V), 255);
        assert_eq!(get(&q, CTA_RASTER_WIDTH), 0xffff_ffff);
        assert_eq!(get(&q, CTA_RASTER_HEIGHT), 0xffff);
        assert_eq!(get(&q, CTA_RASTER_DEPTH), 0xffff);
        assert_eq!(get(&q, SHADER_LOCAL_MEMORY_LOW_SIZE), 0xff_fff0);
        assert_eq!(get(&q, SHADER_LOCAL_MEMORY_HIGH_SIZE), 0);
        assert_eq!(get(&q, cbuf_addr_lower(0)), 0xffff_ffff);
        assert_eq!(get(&q, cbuf_addr_upper(0)), 0xff);
        assert_eq!(get(&q, cbuf_size_shifted4(0)), 0x1fff);
        assert_eq!(get(&q, cbuf_valid(0)), 1);
        assert_eq!(get(&q, RELEASE0_ADDRESS_LOWER), 0xffff_fffc);
        assert_eq!(get(&q, RELEASE0_ADDRESS_UPPER), 0xff);
        assert_eq!(get(&q, RELEASE0_PAYLOAD_LOWER), 0xffff_ffff);
        // no bit outside the fields `build` sets, whatever the values
        let mut mask = [0u32; QMD_WORDS];
        for (hi, lo) in [
            QMD_MAJOR_VERSION, QMD_VERSION, API_VISIBLE_CALL_LIMIT, SAMPLER_INDEX, SM_GLOBAL_CACHING_ENABLE, BARRIER_COUNT,
            CTA_RASTER_WIDTH, CTA_RASTER_HEIGHT, CTA_RASTER_DEPTH, CTA_THREAD_DIMENSION[0], CTA_THREAD_DIMENSION[1], CTA_THREAD_DIMENSION[2],
            PROGRAM_ADDRESS_LOWER, PROGRAM_ADDRESS_UPPER, REGISTER_COUNT_V, SHADER_LOCAL_MEMORY_LOW_SIZE, SHADER_LOCAL_MEMORY_HIGH_SIZE,
            SHARED_MEMORY_SIZE, MIN_SM_CONFIG_SHARED_MEM_SIZE, MAX_SM_CONFIG_SHARED_MEM_SIZE, TARGET_SM_CONFIG_SHARED_MEM_SIZE,
            cbuf_valid(0), cbuf_addr_lower(0), cbuf_addr_upper(0), cbuf_size_shifted4(0),
            RELEASE0_ADDRESS_LOWER, RELEASE0_ADDRESS_UPPER, RELEASE0_MEMBAR_TYPE, RELEASE0_ENABLE, RELEASE0_STRUCTURE_SIZE, RELEASE0_PAYLOAD_LOWER,
        ] {
            for b in lo..=hi {
                mask[b / 32] |= 1 << (b % 32);
            }
        }
        for i in 0..QMD_WORDS {
            assert_eq!(q[i] & !mask[i], 0, "word {} has bits outside the fields", i);
        }
    }

    #[test]
    fn every_thread_dimension_holds_its_largest_value() {
        let b = fill_launch();
        for block in [[1024u16, 1, 1], [1, 1024, 1], [1, 1, 64]] {
            let q = build(&Launch { block, ..b });
            for d in 0..3 {
                assert_eq!(get(&q, CTA_THREAD_DIMENSION[d]), block[d] as u64, "block {:?}, dimension {}", block, d);
            }
        }
        let refused = |block| std::panic::catch_unwind(|| build(&Launch { block, ..b })).is_err();
        assert!(refused([1025, 1, 1]) && refused([1, 1025, 1]) && refused([1, 1, 65]) && refused([64, 32, 1]) && refused([5, 5, 41]), "1025 threads in all");
        assert!(!refused([16, 8, 8]) && !refused([8, 8, 16]));
        assert!(!refused([32, 32, 1]) && !refused([16, 8, 8]));
    }

    #[test]
    fn shared_memory_goes_into_the_sm_config() {
        let cfg = |l: Launch| {
            let q = build(&l);
            (get(&q, SHARED_MEMORY_SIZE), get(&q, MIN_SM_CONFIG_SHARED_MEM_SIZE), get(&q, TARGET_SM_CONFIG_SHARED_MEM_SIZE), get(&q, MAX_SM_CONFIG_SHARED_MEM_SIZE))
        };
        let b = fill_launch();
        // 32 threads = 1 warp = 48 workgroups per SM; 1 KiB each: 48 KiB -> the 64 KiB config (17); alone the 8 KiB one (3)
        assert_eq!(cfg(Launch { smem: 1024, ..b }), (1024, 3, 17, 26));
        // 48 threads = 2 warps (rounded up) = 24 workgroups: 24 KiB -> 32 KiB (9). Round down would give 1 warp and 48 workgroups.
        assert_eq!(cfg(Launch { smem: 1024, block: [48, 1, 1], ..b }), (1024, 3, 9, 26));
        // 1024 threads = 32 warps = 1 workgroup per SM: 48 KiB -> 64 KiB
        assert_eq!(cfg(Launch { smem: 48 * 1024, block: [1024, 1, 1], ..b }), (48 * 1024, 17, 17, 26));
        // and 100 KiB, the whole SM
        assert_eq!(cfg(Launch { smem: 100 * 1024, ..b }), (100 * 1024, 26, 26, 26));
    }

    #[test]
    fn the_limits_of_the_inputs() {
        let b = fill_launch();
        let refused = |l: Launch| std::panic::catch_unwind(|| build(&l)).is_err();
        assert!(refused(Launch { program: b.program + 0x40, ..b }), "a program must be 128-byte aligned");
        assert!(!refused(Launch { program: b.program + 0x80, ..b }));
        assert!(refused(Launch { smem: 0x80, ..b }), "shared memory comes in 256-byte steps");
        assert!(refused(Launch { cbuf0: (b.cbuf0.0, 0x2_0000), ..b }), "the size field is 13 bits of 16 bytes");
        assert!(!refused(Launch { cbuf0: (b.cbuf0.0, 0x1_fff0), ..b }));
        assert!(refused(Launch { cbuf0: (b.cbuf0.0, 0), ..b }));
        assert!(refused(Launch { cbuf0: (b.cbuf0.0, 0x108), ..b }));
        assert!(refused(Launch { grid: [0, 1, 1], ..b }));
        assert!(refused(Launch { grid: [1, 1 << 16, 1], ..b }));
        assert!(refused(Launch { block: [32, 0, 1], ..b }));
        assert!(refused(Launch { local: 8, ..b }));
        assert!(refused(Launch { local: 1 << 24, ..b }));
        assert_eq!(QMD_ALIGN, 1 << 8);
        assert_eq!(PROGRAM_ALIGN, 0x80);
    }

    #[test]
    fn a_launch_without_release_leaves_its_words_zero() {
        let mut l = fill_launch();
        l.release = None;
        let q = build(&l);
        // RELEASE0: words 24..=26 (bits 768..=863)
        assert_eq!(&q[24..27], &[0, 0, 0]);
        let with = build(&fill_launch());
        for i in (0..QMD_WORDS).filter(|i| !(24..27).contains(i)) {
            assert_eq!(q[i], with[i], "word {}", i);
        }
    }

    #[test]
    fn each_field_lands_in_its_own_bits() {
        // change one input at a time: exactly the expected words differ from the base
        let base = build(&fill_launch());
        let diff = |l: Launch| -> Vec<usize> {
            let q = build(&l);
            (0..QMD_WORDS).filter(|&i| q[i] != base[i]).collect()
        };
        let b = fill_launch();
        assert_eq!(diff(Launch { grid: [9, 1, 1], ..b }), vec![12]);
        assert_eq!(diff(Launch { grid: [8, 2, 1], ..b }), vec![13]);
        assert_eq!(diff(Launch { grid: [8, 1, 3], ..b }), vec![14]);
        assert_eq!(diff(Launch { block: [64, 1, 1], ..b }), vec![18]);
        assert_eq!(diff(Launch { block: [32, 2, 1], ..b }), vec![19]);
        assert_eq!(diff(Launch { block: [32, 1, 4], ..b }), vec![19]);
        assert_eq!(diff(Launch { registers: 9, ..b }), vec![20]);
        assert_eq!(diff(Launch { program: b.program + 0x80, ..b }), vec![48]);
        assert_eq!(diff(Launch { program: b.program + (1 << 32), ..b }), vec![49]);
        assert_eq!(diff(Launch { local: 0x10, ..b }), vec![23]);
        assert_eq!(diff(Launch { cbuf0: (b.cbuf0.0 + 0x100, b.cbuf0.1), ..b }), vec![32]);
        assert_eq!(diff(Launch { cbuf0: (b.cbuf0.0, 0x300), ..b }), vec![33]);
        assert_eq!(diff(Launch { release: Some((b.release.unwrap().0, 0x4243)), ..b }), vec![26]);
    }

    #[test]
    fn shared_memory_configurations_of_sm86() {
        // no shared memory: the smallest configuration, and the largest of the SM as the maximum (100 KiB -> 26)
        assert_eq!(sm_config(0, 48), (1, 1, 26));
        // 1 KiB, alone: 8 KiB is the smallest that holds it; 48 of them need 48 KiB -> 64 KiB
        assert_eq!(sm_config(1024, 1), (3, 3, 26));
        assert_eq!(sm_config(1024, 48), (3, 17, 26));
        // 40 KiB: alone it needs the 64 KiB configuration (17), but two fit in 100 KiB (26) and that is the most that fit
        assert_eq!(sm_config(40 * 1024, 48), (17, 26, 26));
        assert_eq!(smem_to_hw(100), 26);
        assert_eq!(smem_to_hw(0), 1);
    }

    #[test]
    #[should_panic(expected = "does not fit the SM")]
    fn more_shared_memory_than_the_sm_has_is_refused() {
        sm_config(101 * 1024, 1);
    }

    #[test]
    #[should_panic(expected = "does not fit")]
    fn a_value_wider_than_its_field_is_refused() {
        let mut q = [0u32; QMD_WORDS];
        put(&mut q, (607, 592), 1 << 16);
    }

    #[test]
    fn the_fixture_is_the_sass_ptxas_makes() {
        // 12 instructions of 16 bytes and NOP padding: 0x180 bytes; the words that carry the constants of the source
        assert_eq!(FILL.len(), 0x180);
        let ins = |n: usize| u128::from_le_bytes(FILL[n * 16..n * 16 + 16].try_into().unwrap());
        // MOV R1, c[0x0][0x28]
        assert_eq!(ins(0) & 0xffff_ffff_ffff_ffff, 0x0000_0a00_0001_7a02);
        // IMAD R0, R2.reuse, -0x61c8864f, RZ: the multiplier 0x9e3779b1 is the instruction's upper immediate
        assert_eq!((ins(6) >> 32) as u32, 0x9e37_79b1);
        assert_eq!(ins(6) as u32, 0x0200_7824);
        // LOP3.LUT R5, R0, 0xc0de0000, RZ, 0x3c: xor with the constant
        assert_eq!((ins(8) >> 32) as u32, 0xc0de_0000);
        // STG.E [R2.64], R5
        assert_eq!(ins(9) as u64, 0x0000_0005_0200_7986);
        // EXIT, then the BRA to itself
        assert_eq!(ins(10) as u64, 0x0000_0000_0000_794d);
        assert_eq!(ins(11) as u64, 0xffff_fff0_0000_7947);
        // the parameter offset the source's ABI implies: IMAD.WIDE.U32 R2, R2, R3, c[0x0][0x160]
        // (a constant-bank offset is encoded as `offset << 6`: MOV R1, c[0x0][0x28] is 0x0a00 in its word)
        assert_eq!(ins(7) as u64, 0x0000_5800_0202_7625);
        assert_eq!(FILL_PARAM << 6, 0x5800);
        assert_eq!(0x28 << 6, 0x0a00);
    }

    #[test]
    fn the_copy_shader_is_the_sass_ptxas_makes() {
        assert_eq!(COPY.len(), 0x180);
        let ins = |n: usize| u128::from_le_bytes(COPY[n * 16..n * 16 + 16].try_into().unwrap()) as u64;
        // IMAD.WIDE.U32 R2, R4, R5, c[0x0][0x168]: the source pointer
        assert_eq!(ins(6), 0x0000_5a00_0402_7625);
        assert_eq!(COPY_PARAM_SRC << 6, 0x5a00);
        // LDG.E.STRONG.GPU R3, [R2.64]
        assert_eq!(ins(7), 0x0000_0004_0203_7981);
        // IMAD.WIDE.U32 R4, R4, R5, c[0x0][0x160]: the destination pointer
        assert_eq!(ins(8), 0x0000_5800_0404_7625);
        // STG.E [R4.64], R3, EXIT, BRA
        assert_eq!(ins(9), 0x0000_0003_0400_7986);
        assert_eq!(ins(10), 0x0000_0000_0000_794d);
        assert_eq!(ins(11), 0xffff_fff0_0000_7947);
        assert_eq!(COPY_REGISTERS, 8, "SHI_REGISTERS of copy.cubin");
        let c = copy_cbuf0(0x1111_2222_3333, 0x4444_5555_6666);
        assert_eq!(&c[0x160..0x168], &0x1111_2222_3333u64.to_le_bytes());
        assert_eq!(&c[0x168..0x170], &0x4444_5555_6666u64.to_le_bytes());
        assert_eq!(c.len(), 0x200);
        assert!(c[..0x160].iter().all(|&b| b == 0) && c[0x170..].iter().all(|&b| b == 0));
    }

    #[test]
    fn the_write_through_shader_is_fill_with_a_system_store() {
        assert_eq!(FILLWT.len(), 0x180);
        let ins = |n: usize| u128::from_le_bytes(FILLWT[n * 16..n * 16 + 16].try_into().unwrap());
        // LOP3.LUT R5, R0, 0xd2ea5678: 0xc0de0000 ^ 0x12345678 folded into the immediate
        assert_eq!((ins(8) >> 32) as u32, 0xc0de_0000 ^ FILLWT_XOR);
        assert_eq!((ins(8) >> 32) as u32, 0xd2ea_5678);
        // STG.E.STRONG.SYS [R2.64], R5: the same word as fill's STG.E with the scope/strength bits of the upper half set
        let (wt, plain) = (ins(9), u128::from_le_bytes(FILL[9 * 16..9 * 16 + 16].try_into().unwrap()));
        assert_eq!(wt as u64, plain as u64);
        assert_ne!(wt >> 64, plain >> 64);
        // the rest of the program is fill's, but for the constant and the store's flags
        assert_eq!(&FILLWT[..0x10], &FILL[..0x10]);
        assert_eq!(fillwt_word(3), fill_word(3) ^ 0x1234_5678);
        assert!((0..1024).all(|i| fillwt_word(i) != fill_word(i) && fillwt_word(i) != scribble_word(i)));
        assert_eq!(FILLWT_REGISTERS, FILL_REGISTERS);
    }

    #[test]
    fn the_reference_words() {
        assert_eq!(fill_word(0), 0xc0de_0000);
        assert_eq!(fill_word(1), 0x9e37_79b1 ^ 0xc0de_0000);
        assert_eq!(fill_word(255), 255u32.wrapping_mul(0x9e37_79b1) ^ 0xc0de_0000);
        let words: Vec<u32> = (0..256).map(fill_word).collect();
        let mut sorted = words.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 256, "distinct, so a misplaced word shows");
    }

    #[test]
    fn the_scribble_never_looks_like_a_result() {
        assert_eq!(FILL_WORDS, 256);
        for i in 0..1024 {
            assert_ne!(scribble_word(i), fill_word(i), "word {}", i);
        }
        // and no result is zero (a zeroed page is what a lost write leaves)
        assert!((0..FILL_WORDS).all(|i| fill_word(i) != 0));
        // the grid of the launch is what the QMD oracle says
        let l = super::tests::fill_launch();
        assert_eq!((l.grid, l.block), ([FILL_CTAS, 1, 1], [FILL_BLOCK, 1, 1]));
    }

    #[test]
    fn cbuf0_holds_the_pointer_where_cuda_puts_parameters() {
        let c = fill_cbuf0(0x3_8004_1000);
        assert_eq!(c.len(), 0x200);
        assert_eq!(&c[0x160..0x168], &0x3_8004_1000u64.to_le_bytes());
        assert!(c[..0x160].iter().all(|&b| b == 0) && c[0x168..].iter().all(|&b| b == 0));
    }
}
