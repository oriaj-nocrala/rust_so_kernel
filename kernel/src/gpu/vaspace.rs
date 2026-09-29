// kernel/src/gpu/vaspace.rs
//
// Phase 6b of docs/gpu/gpu-plan.md: a GPU virtual address space for our RM
// client, `gpu=vaspace`. It runs inside the GSP boot's RPC phase (`gsp.rs`,
// IF=0, before the APs) right after the client's objects exist:
//
//   1. `RM_ALLOC FERMI_VASPACE_A` (0x90f1) under the device, externally owned
//      (`r535_mmu_vaspace_new(.., true)`); RM's reply says which VAs it lets
//      us use (from 64 MiB to 2^49);
//   2. page tables (`nvgpu::mmu::PageTables`) with a small test mapping, written
//      into VRAM through the PRAMIN window and read back;
//   3. `NV0080_CTRL_CMD_DMA_SET_PAGE_DIRECTORY` with the root's address, so RM
//      uses our tables for this space.
//
// Nothing runs on the GPU yet: the tables are first walked by a channel in 6c,
// which is also where the TLB is flushed (`tu102_vmm_flush`). The space and the
// tables stay for good (`gsp.rs` never tears anything down; a reboot resets it).

use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use nvgpu::evo::Pramin;
use nvgpu::mmu::{Flags, PageTables, Target, ROOT_ENTRIES, TABLE_SIZE};
use nvgpu::rm;

use super::gsp::Rm;
use super::Bar0;

/// Where the tables live: VRAM `[64 MiB, 128 MiB)`. The GOP framebuffer is at 0,
/// our scanout buffers at 16/32 MiB and the HDMI one at 48 MiB; nouveau's
/// display instance memory is at `0x1ffc90000` and WPR2 at the top
/// (`docs/gpu/mmu-v3-notes.md`).
pub const TABLE_VRAM: u64 = 64 << 20;
/// The pool: 64 tables (256 KiB, VRAM 64 MiB ..): the 6b test mapping needs 5;
/// `gpu=copy` adds the channel's pages and two 4 MiB buffers (a PT per 2 MiB).
const POOL_TABLES: usize = 64;

/// The test mapping: `TEST_PAGES` pages of VRAM at 96 MiB, seen at VA 4 GiB
/// (the start of the window nouveau reserves for RM: `SPLIT_VAS_SERVER_RM_MANAGED_VA_START`
/// is *not* used here, this is our own space).
const TEST_VA: u64 = 0x1_0000_0000;
const TEST_PA: u64 = 96 << 20;
const TEST_PAGES: u64 = 256;

// 0 = not run, 1 = OK, 2 = failed
static STATE: AtomicU32 = AtomicU32::new(0);
static TABLES: AtomicU32 = AtomicU32::new(0);
static VA_BASE: AtomicU64 = AtomicU64::new(0);
static VA_SIZE: AtomicU64 = AtomicU64::new(0);
static MS: AtomicU64 = AtomicU64::new(0);

fn stop(r: &mut String, why: core::fmt::Arguments) {
    STATE.store(2, Ordering::Relaxed);
    let _ = writeln!(r, "vaspace: STOP: {}", why);
}

pub(super) fn setup(r: &mut String, regs: &Bar0, rm: &mut Rm, copy: bool) {
    let t0 = crate::cpu::tsc::read();

    // 1. The space itself.
    let reply = match rm.alloc(rm::H_DEVICE, rm::H_VASPACE, rm::FERMI_VASPACE_A, &rm::vaspace_params()) {
        Ok(p) => p,
        Err(e) => return stop(r, format_args!("ALLOC FERMI_VASPACE_A: {}", e)),
    };
    let Some(va) = rm::vaspace_from_reply(&reply) else {
        return stop(r, format_args!("ALLOC FERMI_VASPACE_A: reply of {} bytes has no space description", reply.len()));
    };
    VA_BASE.store(va.base, Ordering::Relaxed);
    VA_SIZE.store(va.size, Ordering::Relaxed);
    let _ = writeln!(r, "vaspace: FERMI_VASPACE_A {:#x} allocated: RM gives VAs {:#x} .. {:#x}", rm::H_VASPACE, va.base, va.base + va.size);
    if TEST_VA < va.base || TEST_VA + TEST_PAGES * 0x1000 > va.base + va.size {
        return stop(r, format_args!("the test mapping at {:#x} is outside RM's range", TEST_VA));
    }

    // 2. Tables with one test mapping (and, for gpu=copy, the channel's), into VRAM.
    let mut pt = PageTables::new(TABLE_VRAM, POOL_TABLES, Target::Vram);
    if let Err(e) = pt.map_range(TEST_VA, TEST_PA, TEST_PAGES * 0x1000, Target::Vram, Flags::default()) {
        return stop(r, format_args!("building the tables: {:?}", e));
    }
    let bufs = if copy {
        let Some(b) = super::copy::prepare(r) else { return };
        if let Err(e) = b.map(&mut pt) {
            return stop(r, format_args!("mapping the copy buffers: {:?}", e));
        }
        Some(b)
    } else {
        None
    };
    let mut p = Pramin::new(regs);
    let mut bad = None;
    'write: for (pa, img) in pt.images() {
        for (i, w) in img.chunks_exact(4).enumerate() {
            p.wr32(pa + (i * 4) as u64, u32::from_le_bytes(w.try_into().unwrap()));
        }
        for (i, w) in img.chunks_exact(4).enumerate() {
            let want = u32::from_le_bytes(w.try_into().unwrap());
            let got = p.rd32(pa + (i * 4) as u64);
            if got != want {
                bad = Some((pa + (i * 4) as u64, want, got));
                break 'write;
            }
        }
    }
    p.restore();
    if let Some((at, want, got)) = bad {
        return stop(r, format_args!("table word at VRAM {:#x}: wrote {:#x}, read back {:#x}", at, want, got));
    }
    TABLES.store(pt.len() as u32, Ordering::Relaxed);
    let _ = writeln!(
        r,
        "vaspace: {} tables ({} KiB) at VRAM {:#x} via PRAMIN, read back OK; {} pages mapped at VA {:#x} -> VRAM {:#x}",
        pt.len(),
        pt.len() * TABLE_SIZE / 1024,
        TABLE_VRAM,
        TEST_PAGES,
        TEST_VA,
        TEST_PA
    );

    // 3. Hand RM the root (aperture 0 = video memory).
    let params = rm::set_page_directory_params(pt.root(), ROOT_ENTRIES, 0, rm::H_VASPACE);
    if let Err(e) = rm.control(rm::H_DEVICE, rm::CTRL_DMA_SET_PAGE_DIRECTORY, &params) {
        return stop(r, format_args!("SET_PAGE_DIRECTORY (root {:#x}): {}", pt.root(), e));
    }
    let ms = super::ms_since(t0);
    MS.store(ms, Ordering::Relaxed);
    STATE.store(1, Ordering::Relaxed);
    let _ = writeln!(r, "vaspace: OK: RM accepted the page directory at VRAM {:#x} ({} entries) in {} ms", pt.root(), ROOT_ENTRIES, ms);

    // 4. Phase 6c: a channel over these tables.
    if let Some(b) = bufs {
        super::copy::run(r, regs, rm, b);
    }
}

/// `/proc/kdebug` line (empty when the level was not asked for).
pub fn render_kdebug() -> String {
    match STATE.load(Ordering::Relaxed) {
        0 => String::new(),
        s => alloc::format!(
            "gpu_vaspace: state={} tables={} va_base={:#x} va_size={:#x} ms={}",
            if s == 1 { "ok" } else { "failed" },
            TABLES.load(Ordering::Relaxed),
            VA_BASE.load(Ordering::Relaxed),
            VA_SIZE.load(Ordering::Relaxed),
            MS.load(Ordering::Relaxed)
        ),
    }
}
