// kernel/src/gpu/evo.rs
//
// `gpu=chan` (phase 5.2 of docs/gpu/gpu-plan.md): bring up the display's
// instance memory, the core channel and window 0 over the display the GOP
// left lit, and push one UPDATE on each that repeats what scans out. The
// logic is `nvgpu::evo` (host-tested against the trace); this module owns
// the push buffers and reports each step.
//
// Every step is checked before the next one, and the first surprise stops
// the sequence where it is (no teardown: an untested path on the only video
// output is worse than a channel left idle). The display interrupts that
// would signal a fault (exceptions, supervisors) stay disabled, as the GOP
// left them; this module polls their status registers instead, so the
// vblank handler (phase 3) never sees them.
//
// Runs at boot with IF=0, before the APs are released and before vblank is
// armed (so the vblank report that follows measures the head after the
// UPDATEs).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering};

use nvgpu::evo::{self, Chan, ChanError, Faults, Pramin, Push, PushMem, Ramht};
use nvgpu::Mmio;

use crate::memory::dma::DmaBuf;

use super::Bar0;

/// Push buffers live as long as the channels run: never freed.
static PUSH: spin::Once<[PushBuf; 2]> = spin::Once::new();
/// Where the core channel's next push goes (bytes), once `bring_up` pushed
/// its UPDATE: the supervisor phase (`supervisor.rs`) carries on from there.
static CORE_PUT: AtomicU32 = AtomicU32::new(0);

/// A 4 KiB push buffer in host memory.
pub struct PushBuf(DmaBuf);

impl PushMem for PushBuf {
    fn bus_addr(&self) -> u64 {
        self.0.bus_addr()
    }
    fn len_words(&self) -> usize {
        self.0.len() / 4
    }
    fn write(&self, word: usize, value: u32) {
        self.0.write(word * 4, &value.to_le_bytes());
    }
    /// The GPU reads the buffer by snooped DMA once it sees the PUT write
    /// (an uncached store); the fence orders the buffer's stores first.
    fn flush(&self) {
        // SAFETY: a fence.
        unsafe { core::arch::asm!("mfence", options(nostack, preserves_flags)) };
    }
}

/// Below 4 GiB if the buddy has it (the plan's choice), else below 40 bits
/// (EVO's limit, `dispnv50/disp.c:242-250`).
pub(super) fn alloc_push() -> Result<PushBuf, crate::memory::dma::DmaError> {
    DmaBuf::alloc(4096, 0xffff_ffff).or_else(|_| DmaBuf::alloc(4096, (1 << 40) - 1)).map(PushBuf)
}

fn faults_line(r: &mut String, what: &str, f: &Faults) {
    let _ = writeln!(
        r,
        "chan: {}: disp_intr {:#x} ctrl_disp {:#x} exc other {:#x} win {:#x} winim {:#x} slot core {:#x} wndw0 {:#x}",
        what, f.disp_intr, f.ctrl_disp, f.exc_other, f.exc_win, f.exc_winim, f.core_exc, f.wndw0_exc
    );
}

/// After a step: new faults since the start stop the sequence.
fn check(r: &mut String, regs: &Bar0, step: &str, start: &Faults) -> bool {
    let now = Faults::read(regs);
    let new = now.new_since(start);
    faults_line(r, step, &now);
    if new.bad() {
        let _ = writeln!(r, "chan: STOP after {}: new fault bits {:?}{}", step, new, if new.supervisor() { " (a supervisor: the display saw a mode change)" } else { "" });
        return false;
    }
    true
}

/// `(method, value)` rows of a channel's ARMED state over `methods`.
fn armed(regs: &Bar0, chan: Chan, methods: &[u32]) -> Vec<(u32, u32)> {
    methods.iter().map(|&m| (m, regs.rd32(chan.armed_base() + m))).collect()
}

fn diff_line(r: &mut String, what: &str, before: &[(u32, u32)], after: &[(u32, u32)], expected: &[u32]) -> bool {
    let changed: Vec<(u32, u32, u32)> =
        before.iter().zip(after).filter(|(b, a)| b.1 != a.1).map(|(b, a)| (b.0, b.1, a.1)).collect();
    let unexpected: Vec<&(u32, u32, u32)> = changed.iter().filter(|c| !expected.contains(&c.0)).collect();
    let _ = write!(r, "chan: {} ARMED changed:", what);
    for (m, b, a) in &changed {
        let _ = write!(r, " {:04x} {:08x}->{:08x}", m, b, a);
    }
    let _ = writeln!(r, "{}", if unexpected.is_empty() { " (only pushed methods: as expected)" } else { " (UNEXPECTED)" });
    unexpected.is_empty()
}

/// Returns window 0's push position (PUT, bytes) when every step went as
/// expected, so page flips (`scanout.rs`) carry on in the same buffer.
///
/// `hdmi` (`gpu=hdmi`, phase 5.8): the RAMHT also names the surface for the
/// HP's window (`nvgpu::hdmi::window_chan`), whose channel `hdmi.rs` brings
/// up when the HP is switched on.
pub fn bring_up(r: &mut String, regs: &Bar0, bdf: (u8, u8, u8), hdmi: bool) -> Option<u32> {
    let t0 = crate::cpu::tsc::read();
    let core_methods: Vec<u32> = nvgpu::dispstate::core_methods().collect();
    // nouveau's window list plus the ILUT methods (`clc67e.h:500-514`),
    // which its dump leaves out but an UPDATE would resolve too.
    let wndw_methods: Vec<u32> = nvgpu::dispstate::window_methods().chain([0x440, 0x444, 0x448]).collect();
    let w0 = evo::window(0);

    // --- Evidence, reads only.
    let owner = regs.rd32(evo::DISP_OWNER);
    let vram = evo::vram_size(regs);
    let _ = writeln!(
        r,
        "chan: owner {:#x} vram {} MiB pramin base {:#x} inst (GOP) target {:#x} addr {:#x} ctl core {:#x} wndw0 {:#x} status core {:#x} wndw0 {:#x} heads/sors {:#x} windows {:#x}",
        owner,
        vram >> 20,
        regs.rd32(evo::PRAMIN_BASE_REG),
        regs.rd32(evo::DISP_INST_TARGET),
        regs.rd32(evo::DISP_INST_ADDR),
        regs.rd32(evo::CORE.control()),
        regs.rd32(w0.control()),
        regs.rd32(evo::CORE.status().0),
        regs.rd32(w0.status().0),
        regs.rd32(evo::DISP_HEAD_SOR_MASK),
        regs.rd32(evo::DISP_WNDW_MASK)
    );
    // The GOP's own instance memory (its RAMHT holds the 'DAVE' handle window
    // 0 names), if valid: overwriting it is harmless only because nothing
    // re-reads it before window 0's UPDATE names ours. Logged either way.
    let gop_inst = (regs.rd32(evo::DISP_INST_TARGET) & 0x8 != 0).then(|| (regs.rd32(evo::DISP_INST_ADDR) as u64) << 16);
    if let Some(a) = gop_inst {
        let ours = evo::INST_VRAM..evo::INST_VRAM + evo::INST_SIZE as u64;
        let _ = writeln!(r, "chan: GOP instance memory at VRAM {:#x}{}", a, if ours.contains(&a) { " (inside ours: overwritten)" } else { "" });
    }
    let start = Faults::read(regs);
    faults_line(r, "before", &start);
    if owner & evo::DISP_OWNER_VBIOS != 0 {
        let _ = writeln!(r, "chan: STOP: the VBIOS still owns the display (nouveau would claim it; this phase does not)");
        return None;
    }
    if vram < evo::INST_VRAM + evo::INST_SIZE as u64 {
        let _ = writeln!(r, "chan: STOP: VRAM too small for the instance memory at {:#x}", evo::INST_VRAM);
        return None;
    }
    let core_armed0 = armed(regs, evo::CORE, &core_methods);
    let wndw_armed0 = armed(regs, w0, &wndw_methods);
    let ilut = regs.rd32(w0.armed_base() + 0x444);
    let _ = writeln!(r, "chan: window0 ARMED ilut control {:#x} ctxdma {:#x}", regs.rd32(w0.armed_base() + 0x440), ilut);

    // --- Push buffers, bus mastering (the channels fetch by DMA).
    let bufs = match (alloc_push(), alloc_push()) {
        (Ok(a), Ok(b)) => [a, b],
        (a, b) => {
            let _ = writeln!(r, "chan: STOP: push buffers: {:?} {:?}", a.err(), b.err());
            return None;
        }
    };
    let _ = writeln!(r, "chan: push core {:#x} wndw0 {:#x}", bufs[0].bus_addr(), bufs[1].bus_addr());
    let bufs = PUSH.call_once(|| bufs);
    let (b, d, f) = bdf;
    crate::pci::update_command(b, d, f, hal::pcicfg::COMMAND_MASTER, 0);

    // --- Instance memory through PRAMIN, then the display init nouveau
    // does before its first channel.
    let mut objects = alloc::vec![(w0.user, evo::HANDLE_WNDW_CTX, evo::vram_ctxdma(vram))];
    if hdmi {
        objects.extend(nvgpu::hdmi::ramht_objects(vram));
    }
    let table = Ramht { objects };
    let mut p = Pramin::new(regs);
    let wrote = table.write(&mut p, evo::INST_VRAM);
    let saved = p.saved();
    p.restore();
    match wrote {
        Ok(()) => {
            let _ = writeln!(
                r,
                "chan: instance memory at VRAM {:#x} via PRAMIN (base put back to {:#x}): {} words, read back OK",
                evo::INST_VRAM,
                saved,
                table.words().map_or(0, |w| w.len())
            );
        }
        Err(e) => {
            let _ = writeln!(r, "chan: STOP: instance memory: {:?}", e);
            return None;
        }
    }
    let changed = evo::copy_caps(regs);
    evo::set_instance(regs, evo::INST_VRAM);
    let _ = writeln!(
        r,
        "chan: caps copied ({} words changed), inst target {:#x} addr {:#x}",
        changed,
        regs.rd32(evo::DISP_INST_TARGET),
        regs.rd32(evo::DISP_INST_ADDR)
    );
    if !check(r, regs, "display init", &start) {
        return None;
    }

    // --- Core channel.
    if let Err(e) = evo::init_channel(regs, evo::CORE, &bufs[0]) {
        report_chan_error(r, regs, "core init", e);
        return None;
    }
    let _ = writeln!(r, "chan: core up: ctl {:#x} status {:#x} get {:#x}", regs.rd32(evo::CORE.control()), regs.rd32(evo::CORE.status().0), regs.rd32(evo::CORE.get()));
    if !check(r, regs, "core init", &start) {
        return None;
    }
    let differ = evo::gate(regs, evo::CORE, &core_methods, &evo::CORE_PUSHED);
    if !differ.is_empty() {
        let _ = write!(r, "chan: STOP: core ASSEMBLY differs from ARMED (an UPDATE would change the display):");
        for (m, a, b) in &differ {
            let _ = write!(r, " {:04x} assembly {:08x} armed {:08x}", m, a, b);
        }
        let _ = writeln!(r);
        return None;
    }
    let _ = writeln!(r, "chan: core gate: ASSEMBLY = ARMED on {} methods", core_methods.len() - evo::CORE_PUSHED.len());
    let mut push = Push::new(&bufs[0], 0);
    let res = evo::push_core_update(&mut push).and_then(|_| evo::kick(regs, evo::CORE, &push));
    if let Err(e) = res {
        report_chan_error(r, regs, "core UPDATE", e);
        return None;
    }
    CORE_PUT.store(push.put_bytes(), Ordering::Release);
    // The UPDATE latches at the next vblank: give it two frames.
    regs.udelay(40_000);
    let _ = writeln!(r, "chan: core UPDATE done: put/get {:#x}", regs.rd32(evo::CORE.get()));
    if !check(r, regs, "core UPDATE", &start) {
        return None;
    }
    let core_ok = diff_line(r, "core", &core_armed0, &armed(regs, evo::CORE, &core_methods), &evo::CORE_PUSHED);

    // --- Window 0.
    if ilut != 0 && !evo::is_pri_error(ilut) {
        let _ = writeln!(r, "chan: STOP: window 0's ILUT names context DMA {:#x}, not in this RAMHT", ilut);
        return None;
    }
    if let Err(e) = evo::init_channel(regs, w0, &bufs[1]) {
        report_chan_error(r, regs, "window 0 init", e);
        return None;
    }
    let _ = writeln!(r, "chan: window 0 up: ctl {:#x} status {:#x}", regs.rd32(w0.control()), regs.rd32(w0.status().0));
    if !check(r, regs, "window 0 init", &start) {
        return None;
    }
    // A window channel starts with a reset ASSEMBLY (boot #73): put back
    // the ARMED value of every method that differs, re-check, then UPDATE.
    let wndw_gate = || -> Vec<(u32, u32, u32)> {
        evo::gate(regs, w0, &wndw_methods, &evo::WNDW_PUSHED)
            .into_iter()
            .filter(|(_, a, b)| !evo::is_pri_error(*a) && !evo::is_pri_error(*b))
            .collect()
    };
    let differ = wndw_gate();
    let _ = write!(r, "chan: window 0 ASSEMBLY after bring-up differs on {} methods, restoring ARMED:", differ.len());
    for (m, a, b) in &differ {
        let _ = write!(r, " {:04x} {:08x}->{:08x}", m, a, b);
    }
    let _ = writeln!(r);
    let restore: Vec<(u32, u32)> = differ.iter().map(|d| (d.0, d.2)).collect();
    let mut push = Push::new(&bufs[1], 0);
    if let Err(e) = evo::push_window_state(&mut push, &restore, evo::HANDLE_WNDW_CTX).and_then(|_| evo::kick(regs, w0, &push)) {
        report_chan_error(r, regs, "window 0 state", e);
        return None;
    }
    if !check(r, regs, "window 0 state", &start) {
        return None;
    }
    let still = wndw_gate();
    if !still.is_empty() {
        let _ = write!(r, "chan: STOP: window 0 ASSEMBLY still differs from ARMED after the restore:");
        for (m, a, b) in &still {
            let _ = write!(r, " {:04x} assembly {:08x} armed {:08x}", m, a, b);
        }
        let _ = writeln!(r);
        return None;
    }
    let _ = writeln!(r, "chan: window 0 gate: ASSEMBLY = ARMED (put {:#x})", push.put_bytes());
    if let Err(e) = evo::push_update(&mut push).and_then(|_| evo::kick(regs, w0, &push)) {
        report_chan_error(r, regs, "window 0 UPDATE", e);
        return None;
    }
    let latched = evo::wait(regs, || regs.rd32(w0.armed_base() + evo::WNDW_SET_CONTEXT_DMA_ISO0) == evo::HANDLE_WNDW_CTX);
    let _ = writeln!(r, "chan: window 0 UPDATE {}", if latched { "latched" } else { "NOT latched (ARMED ctxdma unchanged)" });
    if !check(r, regs, "window 0 UPDATE", &start) {
        return None;
    }
    let wndw_ok = diff_line(r, "window 0", &wndw_armed0, &armed(regs, w0, &wndw_methods), &evo::WNDW_PUSHED);
    let _ = writeln!(
        r,
        "chan: {} in {} ms",
        if core_ok && wndw_ok && latched { "OK: channels up, both UPDATEs latched only what was pushed" } else { "DONE with differences (see above)" },
        super::ms_since(t0)
    );
    (core_ok && wndw_ok && latched).then(|| push.put_bytes())
}

/// The core channel's push buffer and where its next push goes, once
/// `bring_up` allocated it.
pub fn core_push() -> Option<(&'static PushBuf, u32)> {
    PUSH.get().map(|b| (&b[0], CORE_PUT.load(Ordering::Acquire)))
}

/// Window 0's push buffer, once `bring_up` allocated it.
pub fn window_push() -> Option<&'static PushBuf> {
    PUSH.get().map(|b| &b[1])
}

fn report_chan_error(r: &mut String, regs: &Bar0, step: &str, e: ChanError) {
    let _ = writeln!(r, "chan: STOP: {}: {:?}", step, e);
    faults_line(r, step, &Faults::read(regs));
    for (name, c) in [("core", evo::CORE), ("wndw0", evo::window(0))] {
        let x = c.exception();
        let _ = writeln!(
            r,
            "chan: {} exception slot {:#x} {:#x} {:#x} ctl {:#x} status {:#x} put {:#x} get {:#x}",
            name,
            regs.rd32(x),
            regs.rd32(x + 4),
            regs.rd32(x + 8),
            regs.rd32(c.control()),
            regs.rd32(c.status().0),
            regs.rd32(c.put()),
            regs.rd32(c.get())
        );
    }
}
