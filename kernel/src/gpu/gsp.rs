// kernel/src/gpu/gsp.rs
//
// Phase 4 of docs/gpu/gpu-plan.md: booting the GPU System Processor.
//
//   gpu=fwsec (4c) — FWSEC-FRTS on the GSP falcon: the VBIOS's signed
//          microcode carves the protected memory region (WPR2) out of the top
//          of VRAM. It changes no display state.
//   gpu=gsp   (4d + 4e) — also everything the GSP-RM boot needs and the boot
//          itself: the firmware ELF behind a radix3 table, the bootloader, the
//          `GspFwWprMeta`, the LibOS arguments with three log buffers, the RM
//          arguments and the queues' shared memory (`nvgpu::gspmem`); then, as
//          nouveau does (`tu102_gsp_oneinit`/`tu102_gsp_init`), FRTS, a GSP
//          reset into RISC-V mode, the LibOS address in the GSP mailboxes, the
//          booter on SEC2 (`nvgpu::booter`) and a check that the GSP's RISC-V
//          core runs. No RPC is sent yet (phase 4f): GSP-RM comes up and waits.
//
// The logic is `nvgpu::{falcon, fwsec, gspmem, booter}` (replayed against
// nouveau's trace on the host); this file owns the DMA buffers, the ordering
// and the report. Runs once at boot (IF=0, before the APs), after the display
// levels and before the vblank interrupt is armed.
//
// What these leave behind (WPR2 set, GSP-RM running) stays until a power cycle:
// the reboot into Linux resets it (measured for FRTS, boot #93).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use nvgpu::falcon::{self, FalconError};
use nvgpu::firmware::{Booter, Bootloader, GspImage, SIG_SECTION_GA10X};
use nvgpu::fwsec::{self, Command, Fwsec, FwsecError};
use nvgpu::gspmem;
use nvgpu::rm;
use nvgpu::rpc::{self, Queues, Shm};
use nvgpu::Mmio;

use super::Bar0;
use crate::memory::dma::{DmaBuf, DmaPages};

/// The falcon DMA base register holds `addr >> 8` in 32 bits: 40-bit bus
/// addresses. (`nvgpu::falcon::Falcon::dma_wr`.) Everything the GSP reads is
/// allocated below this too.
const DMA_MASK: u64 = (1 << 40) - 1;

const GSP_FILE: &str = "nvidia/ga106/gsp/gsp-570.144.bin";
const BOOTLOADER_FILE: &str = "nvidia/ga106/gsp/bootloader-570.144.bin";
const BOOTER_FILE: &str = "nvidia/ga106/gsp/booter_load-570.144.bin";

/// What `GSP_SET_SYSTEM_INFO` needs of the PCI device (`r570_gsp_set_system_info`).
#[derive(Clone, Copy)]
pub struct PciInfo {
    pub bar0: u64,
    pub bar1: u64,
    /// The BAR nouveau calls `NVKM_BAR2_INST` (PCI BAR3 on the GA106).
    pub bar3: u64,
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor: u16,
    pub device_id: u16,
    pub sub_vendor: u16,
    pub sub_device: u16,
    pub revision: u8,
}

/// The queues' shared memory as [`Shm`]: volatile accesses to the DMA buffer.
struct ShmBuf<'a>(&'a DmaBuf);

impl Shm for ShmBuf<'_> {
    fn rd32(&self, off: usize) -> u32 {
        let mut b = [0u8; 4];
        self.0.read(off, &mut b);
        u32::from_le_bytes(b)
    }
    fn wr32(&self, off: usize, v: u32) {
        self.0.write(off, &v.to_le_bytes());
    }
    fn read(&self, off: usize, out: &mut [u8]) {
        self.0.read(off, out);
    }
    fn write(&self, off: usize, data: &[u8]) {
        self.0.write(off, data);
    }
}

/// `/proc/kdebug`: FWSEC: 0 = not run, 1 = OK, 2 = STOP/failed.
static STATE: AtomicU32 = AtomicU32::new(0);
static WPR2_LO: AtomicU32 = AtomicU32::new(0);
static WPR2_HI: AtomicU32 = AtomicU32::new(0);
static SIG_INDEX: AtomicU32 = AtomicU32::new(0);
static MS: AtomicU64 = AtomicU64::new(0);
static CODE: AtomicU32 = AtomicU32::new(0);
/// GSP boot: 0 = not run, 1 = memory prepared, 2 = booter ran, 3 = RISC-V
/// active, 4 = INIT_DONE, 5 = GPU name from RM, 6 = RM client objects, 9 = failed.
static GSP_STAGE: AtomicU32 = AtomicU32::new(0);
static BOOTER_MS: AtomicU64 = AtomicU64::new(0);
static BOOTER_MBOX0: AtomicU32 = AtomicU32::new(0);
static BOOTER_MBOX1: AtomicU32 = AtomicU32::new(0);
static LOGINIT_PP: AtomicU64 = AtomicU64::new(0);
/// RPC phase: events received before INIT_DONE, and the time GSP-RM took.
static RPC_EVENTS: AtomicU32 = AtomicU32::new(0);
static INIT_MS: AtomicU64 = AtomicU64::new(0);
static GPU_NAME: spin::Once<String> = spin::Once::new();
/// The name RM gives through our own client's subdevice (phase 4g).
static RM_NAME: spin::Once<String> = spin::Once::new();

fn stop(r: &mut String, why: core::fmt::Arguments) {
    STATE.store(2, Ordering::Relaxed);
    let _ = writeln!(r, "fwsec: STOP: {}", why);
}

fn gsp_stop(r: &mut String, why: core::fmt::Arguments) {
    GSP_STAGE.store(9, Ordering::Relaxed);
    let _ = writeln!(r, "gsp: STOP: {}", why);
}

fn err_code(e: &FwsecError) -> u32 {
    match e {
        FwsecError::Frts(v) | FwsecError::Sb(v) => *v,
        FwsecError::Falcon(FalconError::BootTimeout { mbox0, .. }) | FwsecError::Falcon(FalconError::Mailbox { mbox0, .. }) => *mbox0,
        _ => 0xffff_ffff,
    }
}

/// Everything the GSP boot reads from `/mnt` and validates, gathered *before*
/// FWSEC runs, so a missing file stops the run before anything changes.
struct Firmware {
    elf: Vec<u8>,
    bootloader: Vec<u8>,
    booter: Vec<u8>,
}

impl Firmware {
    fn load(r: &mut String) -> Option<Firmware> {
        let mut get = |rel: &str| match crate::firmware::load(rel) {
            Ok(d) => Some(d),
            Err(e) => {
                gsp_stop(r, format_args!("{}: {:?} (gsp-570.144.bin is not in disk.img: scripts/sync-usb-data.sh puts it on the stick)", rel, e));
                None
            }
        };
        let elf = get(GSP_FILE)?;
        let bootloader = get(BOOTLOADER_FILE)?;
        let booter = get(BOOTER_FILE)?;
        Some(Firmware { elf, bootloader, booter })
    }
}

pub fn setup(r: &mut String, regs: &Bar0, bdf: (u8, u8, u8), full: bool, vaspace: bool, copy: bool, pci: PciInfo) {
    let Some((bios, _)) = super::VBIOS.get() else {
        stop(r, format_args!("no VBIOS (gpu=disp did not read it)"));
        return;
    };
    let gsp = falcon::GSP;

    // What the GPU looks like before we touch it.
    let (lo, hi) = (regs.rd32(fwsec::WPR2_LO), regs.rd32(fwsec::WPR2_HI));
    let info = gsp.probe(regs);
    let _ = writeln!(
        r,
        "fwsec: before: wpr2 {:#010x} - {:#010x}, 0x1438={:#010x}, GSP falcon v{} secret {} imem {:#x} dmem {:#x}, riscv_active={}",
        lo,
        hi,
        regs.rd32(fwsec::FRTS_ERR),
        info.version,
        info.secret,
        info.code_limit,
        info.data_limit,
        gsp.riscv_active(regs)
    );
    if hi != 0 {
        stop(r, format_args!("WPR2 is already set ({:#x} - {:#x}): FRTS would fail; power-cycle the GPU", lo, hi));
        return;
    }

    // Where FRTS goes (nouveau: `tu102_gsp_oneinit`).
    let fb = nvgpu::evo::vram_size(regs);
    let disp = regs.rd32(fwsec::DISP_VGA_REG);
    let (addr, size) = fwsec::frts_region(fb, disp);
    let _ = writeln!(r, "fwsec: vram {:#x}, 0x625f04 {:#010x}: FRTS region {:#x} + {:#x}", fb, disp, addr, size);

    // gpu=gsp: read and validate every file before FWSEC changes anything.
    let mut prepared = None;
    if full {
        let Some(fw) = Firmware::load(r) else { return };
        let elf = match GspImage::parse(&fw.elf, SIG_SECTION_GA10X) {
            Ok(g) => (g.fwimage.len(), g.signature.len()),
            Err(e) => {
                gsp_stop(r, format_args!("{}: {:?}", GSP_FILE, e));
                return;
            }
        };
        let bl = match Bootloader::parse(&fw.bootloader) {
            Ok(b) => b,
            Err(e) => {
                gsp_stop(r, format_args!("{}: {:?}", BOOTLOADER_FILE, e));
                return;
            }
        };
        if let Err(e) = Booter::parse(&fw.booter) {
            gsp_stop(r, format_args!("{}: {:?}", BOOTER_FILE, e));
            return;
        }
        let layout = gspmem::fb_layout(fb, fwsec::vga_workspace(fb, disp), bl.image.len() as u64, elf.0 as u64, &gspmem::R570_LIBOS3);
        let _ = writeln!(
            r,
            "gsp: firmware: elf {} bytes (signature {}), bootloader {} bytes (app version {:#x}), booter {} bytes",
            elf.0,
            elf.1,
            bl.image.len(),
            bl.desc.app_version,
            fw.booter.len()
        );
        let _ = writeln!(
            r,
            "gsp: layout: wpr2 {:#x}+{:#x} heap {:#x}+{:#x} elf {:#x} boot {:#x} frts {:#x} vga {:#x} nonwpr {:#x}",
            layout.wpr2.addr,
            layout.wpr2.size,
            layout.heap.addr,
            layout.heap.size,
            layout.elf.addr,
            layout.boot.addr,
            layout.frts.addr,
            layout.bios.addr,
            layout.nonwpr_heap.addr
        );
        debug_assert_eq!((layout.frts.addr, layout.frts.size), (addr, size));
        prepared = Some((fw, layout));
    }

    let mut fw = match Fwsec::build(bios, Command::Frts { addr, size }) {
        Ok(f) => f,
        Err(e) => {
            stop(r, format_args!("no FWSEC image in the VBIOS: {:?}", e));
            return;
        }
    };
    let p = fw.params;
    let _ = writeln!(
        r,
        "fwsec: image {} bytes: imem {:#x}@{:#x} dmem {:#x}@{:#x} sign@{:#x} engine {:#x} ucode {} fuse-mask {:#x}",
        fw.image.len(),
        p.imem_size,
        p.imem_base,
        p.dmem_size,
        p.dmem_base,
        p.dmem_sign,
        p.engine_id,
        p.ucode_id,
        fw.fuse_ver
    );

    // The GSP memory is built before FRTS, as nouveau does (its layout is what
    // the FRTS region belongs to); nothing of it touches the GPU.
    let mut mem = None;
    if let Some((fwset, layout)) = prepared.as_ref() {
        match build_memory(r, fwset, layout, &gsp, regs, &pci) {
            Some(m) => mem = Some(m),
            None => return,
        }
    }

    let sig = match fwsec::prepare(regs, &gsp, &mut fw) {
        Ok(i) => i,
        Err(e) => {
            stop(r, format_args!("signature: {:?}", e));
            return;
        }
    };
    SIG_INDEX.store(sig as u32, Ordering::Relaxed);
    let _ = writeln!(r, "fwsec: signature {} patched (fuse register {:#x})", sig, fw.fuse_register().unwrap_or(0));

    // The falcon DMA reads the image from system memory: it needs bus
    // mastering (`evo::bring_up` set it; idempotent).
    crate::pci::enable_mem_and_bus_master(bdf.0, bdf.1, bdf.2);
    let buf = match DmaBuf::alloc(fw.image.len(), DMA_MASK) {
        Ok(b) => b,
        Err(e) => {
            stop(r, format_args!("DMA buffer: {:?}", e));
            return;
        }
    };
    buf.write(0, &fw.image);
    // SAFETY: a fence, so the image is visible to the device before its DMA.
    unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)) };

    let t0 = crate::cpu::tsc::read();
    let res = fwsec::execute(regs, &gsp, &fw, buf.bus_addr(), sig);
    let ms = super::ms_since(t0);
    MS.store(ms, Ordering::Relaxed);
    match res {
        Ok(f) => {
            // The falcon halted: the DMA is over.
            buf.free();
            WPR2_LO.store(f.wpr2_lo, Ordering::Relaxed);
            WPR2_HI.store(f.wpr2_hi, Ordering::Relaxed);
            STATE.store(1, Ordering::Relaxed);
            let _ = writeln!(
                r,
                "fwsec: OK: FRTS in {} ms, WPR2 {:#010x} - {:#010x} (trace-gsp: 0x01ffe000 - 0x01ffee00), mailbox0 {:#010x}",
                ms,
                f.wpr2_lo,
                f.wpr2_hi,
                regs.rd32(gsp.base + 0x040)
            );
        }
        Err(e) => {
            // Do not free `buf`: the falcon may still be reading it (a
            // timeout), and a leaked 64 KiB is cheaper than a DMA into
            // freed memory.
            core::mem::forget(buf);
            CODE.store(err_code(&e), Ordering::Relaxed);
            stop(
                r,
                format_args!(
                    "{:?} after {} ms; mailbox {:#010x}/{:#010x}, wpr2 {:#010x} - {:#010x}, 0x1438={:#010x}, cpuctl {:#x}",
                    e,
                    ms,
                    regs.rd32(gsp.base + 0x040),
                    regs.rd32(gsp.base + 0x044),
                    regs.rd32(fwsec::WPR2_LO),
                    regs.rd32(fwsec::WPR2_HI),
                    regs.rd32(fwsec::FRTS_ERR),
                    regs.rd32(gsp.base + 0x100)
                ),
            );
            return;
        }
    }

    if let (Some(mem), Some((fwset, _))) = (mem, prepared) {
        boot_gsp(r, regs, &fwset, mem, vaspace, copy);
    }
}

/// The host buffers of the GSP boot, kept forever once the GSP runs.
struct Memory {
    wpr_meta: DmaBuf,
    libos: DmaBuf,
    loginit: DmaBuf,
    logintr: DmaBuf,
    logrm: DmaBuf,
    app_version: u32,
    /// The queues' shared memory and the counters of what was sent.
    shm: DmaBuf,
    queues: Queues,
    // Held so nothing frees them: the GSP reads them.
    _keep: Vec<DmaBuf>,
    _pages: Vec<DmaPages>,
}

fn alloc_buf(r: &mut String, what: &str, len: usize) -> Option<DmaBuf> {
    match DmaBuf::alloc(len, DMA_MASK) {
        Ok(b) => Some(b),
        Err(e) => {
            gsp_stop(r, format_args!("{} ({} bytes): {:?}", what, len, e));
            None
        }
    }
}

/// Phase 4d: allocate and fill every buffer the GSP-RM boot reads.
fn build_memory(r: &mut String, fw: &Firmware, layout: &gspmem::FbLayout, gsp: &falcon::Falcon, regs: &Bar0, pci: &PciInfo) -> Option<Memory> {
    let g = GspImage::parse(&fw.elf, SIG_SECTION_GA10X).ok()?;
    let bl = Bootloader::parse(&fw.bootloader).ok()?;
    let t0 = crate::cpu::tsc::read();

    // The firmware image, page by page, behind the radix3 table.
    let n = g.fwimage.len().div_ceil(gspmem::PAGE);
    let img = match DmaPages::alloc(n, DMA_MASK) {
        Ok(p) => p,
        Err(e) => {
            gsp_stop(r, format_args!("firmware image pages ({}): {:?}", n, e));
            return None;
        }
    };
    for i in 0..n {
        let start = i * gspmem::PAGE;
        let end = (start + gspmem::PAGE).min(g.fwimage.len());
        img.page(i).copy_in(0, &g.fwimage[start..end]);
    }
    let l2n = gspmem::radix3_lvl2_pages(g.fwimage.len() as u64);
    let lvl2 = match DmaPages::alloc(l2n, DMA_MASK) {
        Ok(p) => p,
        Err(e) => {
            gsp_stop(r, format_args!("radix3 level 2 ({} pages): {:?}", l2n, e));
            return None;
        }
    };
    let lvl0 = alloc_buf(r, "radix3 level 0", gspmem::PAGE)?;
    let lvl1 = alloc_buf(r, "radix3 level 1", gspmem::PAGE)?;
    let img_addrs: Vec<u64> = (0..n).map(|i| img.page(i).bus_addr()).collect();
    let l2_addrs: Vec<u64> = (0..l2n).map(|i| lvl2.page(i).bus_addr()).collect();
    let rx = match gspmem::radix3(&img_addrs, lvl1.bus_addr(), &l2_addrs) {
        Ok(x) => x,
        Err(e) => {
            gsp_stop(r, format_args!("radix3: {:?}", e));
            return None;
        }
    };
    lvl0.write(0, &rx.lvl0);
    lvl1.write(0, &rx.lvl1);
    for i in 0..l2n {
        lvl2.page(i).write(0, &rx.lvl2[i * gspmem::PAGE..(i + 1) * gspmem::PAGE]);
    }

    let boot = alloc_buf(r, "bootloader", bl.image.len())?;
    boot.copy_in(0, bl.image);
    let sig_size = (g.signature.len() as u64 + 255) & !255;
    let sig = alloc_buf(r, "signature", sig_size as usize)?;
    sig.write(0, g.signature);

    // The queues' shared memory and the RM arguments.
    let shm = alloc_buf(r, "shared memory", gspmem::SHARED_SIZE)?;
    shm.write(0, &gspmem::shared_memory(shm.bus_addr()));
    let rmargs = alloc_buf(r, "RM arguments", gspmem::PAGE)?;
    rmargs.write(0, &gspmem::rm_args(shm.bus_addr()));

    // The LibOS arguments and the three log buffers.
    let loginit = alloc_buf(r, "LOGINIT", 0x10000)?;
    let logintr = alloc_buf(r, "LOGINTR", 0x10000)?;
    let logrm = alloc_buf(r, "LOGRM", 0x10000)?;
    for b in [&loginit, &logintr, &logrm] {
        let mut page_table = alloc::vec![0u8; 0x10000];
        gspmem::log_buffer_init(&mut page_table, b.bus_addr());
        b.write(0, &page_table);
    }
    let libos = alloc_buf(r, "LibOS arguments", gspmem::PAGE)?;
    libos.write(
        0,
        &gspmem::libos_args(&[
            (loginit.bus_addr(), 0x10000),
            (logintr.bus_addr(), 0x10000),
            (logrm.bus_addr(), 0x10000),
            (rmargs.bus_addr(), gspmem::PAGE as u64),
        ]),
    );

    // The WPR meta, last: it points at all of the above.
    let meta = gspmem::wpr_meta(
        layout,
        &gspmem::HostBufs {
            radix3_lvl0: lvl0.bus_addr(),
            bootloader: boot.bus_addr(),
            signature: sig.bus_addr(),
            signature_size: sig_size,
            code_offset: bl.desc.monitor_code_offset as u64,
            data_offset: bl.desc.monitor_data_offset as u64,
            manifest_offset: bl.desc.manifest_offset as u64,
        },
    );
    let wpr_meta = alloc_buf(r, "WPR meta", gspmem::PAGE)?;
    wpr_meta.write(0, &meta.to_bytes());
    // SAFETY: a fence, so everything is visible to the device before it reads it.
    unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)) };

    GSP_STAGE.store(1, Ordering::Relaxed);
    let _ = writeln!(
        r,
        "gsp: memory: {} image pages + {} radix3 pages in {} ms; wpr meta @{:#x}, libos @{:#x}, shm @{:#x}, rmargs @{:#x}, logs @{:#x}/{:#x}/{:#x}",
        n,
        l2n,
        super::ms_since(t0),
        wpr_meta.bus_addr(),
        libos.bus_addr(),
        shm.bus_addr(),
        rmargs.bus_addr(),
        loginit.bus_addr(),
        logintr.bus_addr(),
        logrm.bus_addr()
    );

    // The two RPCs GSP-RM reads first: system info and registry (`r535_gsp_oneinit`
    // sends them before FWSEC; the doorbell rings on a GSP that is not running yet).
    let mut queues = Queues::default();
    let info = rpc::SystemInfo {
        gpu_phys_addr: pci.bar0,
        gpu_phys_fb_addr: pci.bar1,
        gpu_phys_inst_addr: pci.bar3,
        bus_device_func: ((pci.bus as u64) << 8) | ((pci.device as u64) << 3) | pci.function as u64,
        max_user_va: 0x7fff_ffff_f000,
        pci_config_mirror_base: 0x88000,
        pci_config_mirror_size: 0x1000,
        pci_device_id: ((pci.device_id as u32) << 16) | pci.vendor as u32,
        pci_sub_device_id: ((pci.sub_device as u32) << 16) | pci.sub_vendor as u32,
        pci_revision_id: pci.revision as u32,
        is_primary: false,
    };
    let shm_view = ShmBuf(&shm);
    let sent = queues
        .send(&shm_view, regs, gsp, rpc::FN_GSP_SET_SYSTEM_INFO, &info.to_bytes(), true)
        .and_then(|_| queues.send(&shm_view, regs, gsp, rpc::FN_SET_REGISTRY, &rpc::registry(&rpc::REGISTRY), true));
    if let Err(e) = sent {
        gsp_stop(r, format_args!("queueing the boot RPCs: {:?}", e));
        return None;
    }
    let _ = writeln!(r, "gsp: rpc: SET_SYSTEM_INFO and SET_REGISTRY queued (cmdq write pointer {})", shm_view.rd32(gspmem::CMDQ_OFFSET + 16));

    Some(Memory {
        shm,
        queues,
        wpr_meta,
        libos,
        loginit,
        logintr,
        logrm,
        app_version: bl.desc.app_version,
        _keep: alloc::vec![lvl0, lvl1, boot, sig, rmargs],
        _pages: alloc::vec![img, lvl2],
    })
}

/// Phase 4e, after FRTS: reset the GSP into RISC-V mode, give it the LibOS
/// address, run the booter on SEC2, check the RISC-V core.
fn boot_gsp(r: &mut String, regs: &Bar0, fw: &Firmware, mut mem: Memory, vaspace: bool, copy: bool) {
    use nvgpu::booter;
    let gsp = falcon::GSP;
    let b = match Booter::parse(&fw.booter) {
        Ok(b) => b,
        Err(e) => {
            gsp_stop(r, format_args!("booter: {:?}", e));
            return;
        }
    };
    // `tu102_gsp_oneinit`: reset into RISC-V mode, LibOS address in the mailboxes.
    if let Err(e) = gsp.gsp_reset(regs) {
        gsp_stop(r, format_args!("GSP reset: {:?}", e));
        return;
    }
    gsp.set_mailboxes(regs, mem.libos.bus_addr() as u32, (mem.libos.bus_addr() >> 32) as u32);

    let prep = match booter::prepare(regs, &booter::SEC2, &b) {
        Ok(p) => p,
        Err(e) => {
            gsp_stop(r, format_args!("booter signature: {:?}", e));
            return;
        }
    };
    let buf = match DmaBuf::alloc(prep.image.len(), DMA_MASK) {
        Ok(x) => x,
        Err(e) => {
            gsp_stop(r, format_args!("booter DMA buffer: {:?}", e));
            return;
        }
    };
    buf.copy_in(0, &prep.image);
    let t0 = crate::cpu::tsc::read();
    let res = booter::execute(regs, &booter::SEC2, &prep, buf.bus_addr(), mem.wpr_meta.bus_addr());
    let ms = super::ms_since(t0);
    BOOTER_MS.store(ms, Ordering::Relaxed);
    let (m0, m1) = (regs.rd32(booter::SEC2.base + 0x040), regs.rd32(booter::SEC2.base + 0x044));
    BOOTER_MBOX0.store(m0, Ordering::Relaxed);
    BOOTER_MBOX1.store(m1, Ordering::Relaxed);
    match res {
        Ok(_) => buf.free(),
        Err(e) => {
            core::mem::forget(buf);
            gsp_stop(
                r,
                format_args!(
                    "booter {:?} after {} ms; SEC2 mailbox {:#010x}/{:#010x}, wpr2 {:#010x} - {:#010x}, cpuctl {:#x}",
                    e,
                    ms,
                    m0,
                    m1,
                    regs.rd32(fwsec::WPR2_LO),
                    regs.rd32(fwsec::WPR2_HI),
                    regs.rd32(booter::SEC2.base + 0x100)
                ),
            );
            return;
        }
    }
    GSP_STAGE.store(2, Ordering::Relaxed);
    let _ = writeln!(r, "gsp: booter ran in {} ms (trace-gsp: 250 ms), SEC2 mailbox {:#010x}/{:#010x}, wpr2 now {:#010x} - {:#010x}", ms, m0, m1, regs.rd32(fwsec::WPR2_LO), regs.rd32(fwsec::WPR2_HI));

    if !booter::gsp_running(regs, &gsp, mem.app_version) {
        gsp_stop(r, format_args!("the GSP's RISC-V core is not active after the booter (0x111388 = {:#x})", regs.rd32(gsp.base + gsp.addr2 + 0x388)));
        return;
    }
    GSP_STAGE.store(3, Ordering::Relaxed);
    let _ = writeln!(r, "gsp: RISC-V active (0x111388 = {:#x})", regs.rd32(gsp.base + gsp.addr2 + 0x388));

    // Phase 4f: GSP-RM boots on its own, asking the host for register work
    // (RUN_CPU_SEQUENCER) until it says INIT_DONE; then the static configuration.
    rpc_phase(r, regs, &mut mem, vaspace, copy);

    // Then see whether it wrote its logs.
    for _ in 0..50 {
        regs.udelay(1000);
    }
    let mut pp_total = 0u64;
    for (name, b) in [("LOGINIT", &mem.loginit), ("LOGINTR", &mem.logintr), ("LOGRM", &mem.logrm)] {
        let mut hdr = [0u8; 8];
        b.read(0, &mut hdr);
        let pp = u64::from_le_bytes(hdr);
        pp_total += pp;
        // Beyond the page table (8 + 16 * 8 bytes) look for written words.
        let mut data = alloc::vec![0u8; 0x10000];
        b.read(0, &mut data);
        let used = data[8 + 16 * 8..].iter().rposition(|&x| x != 0).map_or(0, |i| i + 1);
        let _ = writeln!(r, "gsp: {} put pointer {} , {} bytes of data past the page table", name, pp, used);
        if used > 0 {
            let start = 8 + 16 * 8;
            let n = used.min(96);
            let mut line = String::new();
            for byte in &data[start..start + n] {
                let _ = write!(line, "{:02x}", byte);
            }
            let _ = writeln!(r, "gsp: {} first bytes {}", name, line);
        }
    }
    LOGINIT_PP.store(pp_total, Ordering::Relaxed);
    let _ = writeln!(
        r,
        "gsp: OK: booted{}; GSP mailbox {:#010x}/{:#010x}, 0x110080 = {:#x}",
        match GPU_NAME.get() {
            Some(n) => alloc::format!(" and RM says the GPU is \"{}\"", n),
            None => String::from(", RM not reached"),
        },
        regs.rd32(gsp.base + 0x040),
        regs.rd32(gsp.base + 0x044),
        regs.rd32(gsp.base + 0x080)
    );
    // Everything stays allocated: the GSP reads and writes it from now on.
    core::mem::forget(mem);
}

/// The queues' state, for a report when the RPC phase stops.
fn queue_diag(regs: &Bar0, shm: &ShmBuf) -> String {
    let gsp = falcon::GSP;
    alloc::format!(
        "cmdq wptr {} rptr {}, msgq wptr {} rptr {}, GSP mailbox {:#010x}/{:#010x}, 0x111388 {:#x}",
        shm.rd32(gspmem::CMDQ_OFFSET + 16),
        shm.rd32(gspmem::MSGQ_OFFSET + 32),
        shm.rd32(gspmem::MSGQ_OFFSET + 16),
        shm.rd32(gspmem::CMDQ_OFFSET + 32),
        regs.rd32(gsp.base + 0x040),
        regs.rd32(gsp.base + 0x044),
        regs.rd32(gsp.base + gsp.addr2 + 0x388)
    )
}

/// Read the status queue until `want` arrives, running the sequencer commands
/// GSP-RM sends meanwhile (`r535_gsp_msg_recv`, `rm/r535/rpc.c:262-330`). Other
/// events are counted and dropped, as nouveau does for the ones it has no
/// handler for.
fn wait_for(regs: &Bar0, shm: &ShmBuf, q: &Queues, env: &rpc::SeqEnv, want: u32, budget_ms: u64, seen: &mut alloc::vec::Vec<u32>) -> Result<rpc::Message, String> {
    let t0 = crate::cpu::tsc::read();
    loop {
        match q.recv(shm) {
            Ok(Some(m)) => {
                if m.result != 0 {
                    return Err(alloc::format!("fn {:#x} came back with result {:#x} (private {:#x})", m.function, m.result, m.result_private));
                }
                if m.function == want {
                    return Ok(m);
                }
                if m.function == rpc::EVENT_GSP_RUN_CPU_SEQUENCER {
                    let (ops, mut save) = rpc::decode_sequencer(&m.payload).map_err(|e| alloc::format!("sequencer message: {:?}", e))?;
                    let n = ops.len();
                    let timeouts = rpc::run_sequencer(regs, env, &ops, &mut save).map_err(|e| alloc::format!("sequencer ({} commands): {:?}", n, e))?;
                    seen.push(m.function);
                    if timeouts != 0 {
                        return Err(alloc::format!("sequencer ran {} commands, {} polls timed out", n, timeouts));
                    }
                } else {
                    seen.push(m.function);
                }
            }
            Ok(None) => {
                if super::ms_since(t0) > budget_ms {
                    return Err(alloc::format!("nothing from GSP-RM in {} ms", budget_ms));
                }
                regs.udelay(20);
            }
            Err(e) => return Err(alloc::format!("status queue: {:?}", e)),
        }
    }
}

/// Phase 4f: wait for INIT_DONE, then GET_GSP_STATIC_INFO for the GPU's name.
fn rpc_phase(r: &mut String, regs: &Bar0, mem: &mut Memory, vaspace: bool, copy: bool) {
    let gsp = falcon::GSP;
    let shm = ShmBuf(&mem.shm);
    let env = rpc::SeqEnv {
        gsp,
        sec2: nvgpu::booter::SEC2,
        libos_addr: mem.libos.bus_addr(),
        app_version: mem.app_version,
        bar0_len: regs.len as u32,
    };
    let mut seen = alloc::vec::Vec::new();
    let t0 = crate::cpu::tsc::read();
    if let Err(e) = wait_for(regs, &shm, &mem.queues, &env, rpc::EVENT_GSP_INIT_DONE, 20_000, &mut seen) {
        gsp_stop(r, format_args!("waiting for INIT_DONE: {} ({}); events so far {:x?}", e, queue_diag(regs, &shm), seen));
        return;
    }
    let init_ms = super::ms_since(t0);
    INIT_MS.store(init_ms, Ordering::Relaxed);
    RPC_EVENTS.store(seen.len() as u32, Ordering::Relaxed);
    GSP_STAGE.store(4, Ordering::Relaxed);
    let _ = writeln!(r, "gsp: INIT_DONE after {} ms (trace-gsp: 1.24 s after the booter); {} events before it: {:x?}", init_ms, seen.len(), seen);

    // From here on the channel is a plain request/reply one: one RPC at a time.
    let mut rm = Rm { regs, shm: ShmBuf(&mem.shm), queues: &mut mem.queues, env };
    match rm.call(rpc::FN_GET_GSP_STATIC_INFO, &alloc::vec![0u8; rpc::STATIC_INFO_SIZE]) {
        Ok(reply) => match rpc::gpu_name(&reply.payload) {
            Some(name) => {
                GPU_NAME.call_once(|| String::from(name));
                GSP_STAGE.store(5, Ordering::Relaxed);
                let _ = writeln!(r, "gsp: GET_GSP_STATIC_INFO: {} bytes, the GPU is \"{}\" (RM's own answer)", reply.payload.len(), name);
            }
            None => {
                gsp_stop(r, format_args!("GET_GSP_STATIC_INFO answered {} bytes with no GPU name", reply.payload.len()));
                return;
            }
        },
        Err(e) => {
            gsp_stop(r, format_args!("GET_GSP_STATIC_INFO: {} ({})", e, queue_diag(regs, &rm.shm)));
            return;
        }
    }

    // Phase 4g: our own RM client: root -> device -> subdevice, then one control
    // on the subdevice (`r570_gsp_client_ctor`, `r535_gsp_device_ctor`).
    match rm.create_client() {
        Ok(name) => {
            GSP_STAGE.store(6, Ordering::Relaxed);
            let _ = writeln!(r, "gsp: RM objects: client {:#x}, device {:#x}, subdevice {:#x} allocated (status 0); GPU_GET_NAME_STRING on the subdevice says \"{}\"", rm::H_CLIENT, rm::H_DEVICE, rm::H_SUBDEVICE, name);
            RM_NAME.call_once(|| name);
        }
        Err(e) => {
            gsp_stop(r, format_args!("RM objects: {} ({})", e, queue_diag(regs, &rm.shm)));
            return;
        }
    }

    // Phase 6b: a GPU virtual address space for that client.
    if vaspace {
        super::vaspace::setup(r, regs, &mut rm, copy);
    }
}

/// One RPC at a time over the queues, with the boot's event handling.
pub(super) struct Rm<'a> {
    regs: &'a Bar0,
    shm: ShmBuf<'a>,
    queues: &'a mut Queues,
    env: rpc::SeqEnv,
}

impl Rm<'_> {
    /// Send `function` and wait for its reply (events meanwhile are handled or dropped).
    fn call(&mut self, function: u32, payload: &[u8]) -> Result<rpc::Message, String> {
        self.queues
            .send(&self.shm, self.regs, &falcon::GSP, function, payload, false)
            .map_err(|e| alloc::format!("sending fn {}: {:?}", function, e))?;
        let mut events = alloc::vec::Vec::new();
        wait_for(self.regs, &self.shm, self.queues, &self.env, function, 10_000, &mut events)
    }

    /// Read whatever RM has queued for up to `budget_ms`, running sequencer
    /// commands and dropping the rest as `wait_for` does; the function numbers
    /// of what arrived (for a report when something did not complete).
    pub(super) fn drain(&mut self, budget_ms: u64) -> alloc::vec::Vec<u32> {
        let mut seen = alloc::vec::Vec::new();
        let _ = wait_for(self.regs, &self.shm, self.queues, &self.env, u32::MAX, budget_ms, &mut seen);
        seen
    }

    /// The reply's payload (the request echoed with the status and any output fields filled).
    pub(super) fn alloc(&mut self, parent: u32, object: u32, class: u32, params: &[u8]) -> Result<Vec<u8>, String> {
        let reply = self.call(rm::FN_GSP_RM_ALLOC, &rm::alloc_request(rm::H_CLIENT, parent, object, class, params))?;
        rm::check_alloc_reply(&reply.payload, rm::H_CLIENT, object).map_err(|e| alloc::format!("alloc class {:#x}: {:?}", class, e))?;
        Ok(reply.payload)
    }

    pub(super) fn control(&mut self, object: u32, cmd: u32, params: &[u8]) -> Result<alloc::vec::Vec<u8>, String> {
        let reply = self.call(rm::FN_GSP_RM_CONTROL, &rm::control_request(rm::H_CLIENT, object, cmd, params))?;
        rm::check_control_reply(&reply.payload, rm::H_CLIENT, object, cmd).map(|p| p.to_vec()).map_err(|e| alloc::format!("control {:#x}: {:?}", cmd, e))
    }

    /// The client, its device and subdevice, then the GPU's name from RM once more.
    fn create_client(&mut self) -> Result<String, String> {
        self.alloc(rm::H_CLIENT, rm::H_CLIENT, rm::NV01_ROOT, &rm::root_params(rm::H_CLIENT))?;
        self.alloc(rm::H_CLIENT, rm::H_DEVICE, rm::NV01_DEVICE_0, &rm::device_params(rm::H_CLIENT))?;
        self.alloc(rm::H_DEVICE, rm::H_SUBDEVICE, rm::NV20_SUBDEVICE_0, &rm::subdevice_params())?;
        let p = self.control(rm::H_SUBDEVICE, rm::CTRL_GPU_GET_NAME_STRING, &rm::name_string_params())?;
        rm::name_from_params(&p).map(String::from).ok_or_else(|| String::from("GPU_GET_NAME_STRING returned no name"))
    }
}

/// `/proc/kdebug` lines.
pub fn render_kdebug() -> String {
    let base = render_gsp_kdebug();
    let mut out = base;
    for v in [super::vaspace::render_kdebug(), super::copy::render_kdebug()] {
        if !v.is_empty() {
            out += "\n";
            out += &v;
        }
    }
    out
}

fn render_gsp_kdebug() -> String {
    let fw = match STATE.load(Ordering::Relaxed) {
        0 => String::from("gpu_fwsec: off"),
        s => alloc::format!(
            "gpu_fwsec: state={} wpr2_lo={:#x} wpr2_hi={:#x} sig={} ms={} code={:#x}",
            if s == 1 { "ok" } else { "failed" },
            WPR2_LO.load(Ordering::Relaxed),
            WPR2_HI.load(Ordering::Relaxed),
            SIG_INDEX.load(Ordering::Relaxed),
            MS.load(Ordering::Relaxed),
            CODE.load(Ordering::Relaxed)
        ),
    };
    match GSP_STAGE.load(Ordering::Relaxed) {
        0 => fw,
        s => alloc::format!(
            "{}\ngpu_gsp: stage={} booter_ms={} booter_mbox0={:#x} booter_mbox1={:#x} log_pp={}",
            fw,
            match s {
                1 => "memory",
                2 => "booter",
                3 => "riscv",
                4 => "init_done",
                5 => "name",
                6 => "objects",
                _ => "failed",
            },
            BOOTER_MS.load(Ordering::Relaxed),
            BOOTER_MBOX0.load(Ordering::Relaxed),
            BOOTER_MBOX1.load(Ordering::Relaxed),
            LOGINIT_PP.load(Ordering::Relaxed)
        ) + &alloc::format!(
            " init_ms={} events={} name=\"{}\" rm_name=\"{}\"",
            INIT_MS.load(Ordering::Relaxed),
            RPC_EVENTS.load(Ordering::Relaxed),
            GPU_NAME.get().map(|s| s.as_str()).unwrap_or(""),
            RM_NAME.get().map(|s| s.as_str()).unwrap_or("")
        ),
    }
}
