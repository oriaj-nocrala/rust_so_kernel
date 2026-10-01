// kernel/src/gpu/cursor.rs
//
// The display engine's hardware cursor, phase 2 of docs/gpu/hw-cursor-plan.md (the pure part is `nvgpu::cursor`). Driven from `/dev/dispctl`:
//   `cursor probe`            allocate the primary head's cursor channel (no core push), report its state
//   `cursor on [size] [nb]`   write a test arrow (size 32/64/128/256, default 32) into VRAM, push the core methods that enable it (usage bounds
//                             first, unless `nb`), put it at (0, 0)
//   `cursor move <x> <y>`     the position write (two stores, no lock but a counter: it is what the compositor will call per input event)
//   `cursor off`              disable it in the core, release the channel
// An instrument first: nothing here is reached unless a person or a job writes to /dev/dispctl.

use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use nvgpu::cursor as nc;
use nvgpu::evo::{self, Pramin};
use nvgpu::Mmio;

use super::supervisor::{self, RequestError};

/// Where the test image lives: VRAM 80 MiB (between the page-table pool at 64 MiB and the 6b test mapping at 96 MiB, `docs/reference/gpu.md` memory map).
pub const IMAGE_VRAM: u64 = 80 << 20;

/// How long `on` waits for the core to latch its push (a supervisor may have to be served first).
const LATCH_MS: u64 = 1500;

static ALLOCATED: AtomicBool = AtomicBool::new(false);
static ON: AtomicBool = AtomicBool::new(false);
static SIZE: AtomicU32 = AtomicU32::new(0);
static SETS: AtomicU64 = AtomicU64::new(0);
static MOVES: AtomicU64 = AtomicU64::new(0);
static REFUSED: AtomicU64 = AtomicU64::new(0);
static LAST_MOVE_US: AtomicU64 = AtomicU64::new(0);
static MAX_MOVE_US: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, PartialEq, Eq)]
pub enum CursorError {
    /// No display driver (needs gpu=super and up) or it is not running.
    NotReady,
    /// A bad size or state (already on, not on).
    Invalid,
    /// The core push was refused (busy) or the channel did not come up.
    Failed,
}

fn log(line: String) {
    supervisor::log_line(line);
}

/// The test image: a white arrow with a black outline on a transparent background, premultiplied ARGB8888 (transparent = 0).
fn arrow(size: u32) -> alloc::vec::Vec<u32> {
    let scale = size / 32;
    let mut px = alloc::vec![0u32; (size * size) as usize];
    for y in 0..size {
        for x in 0..size {
            let (ux, uy) = (x / scale.max(1), y / scale.max(1));
            // the arrow: the area left of the diagonal x <= y / 2 for y < 22, with a 2-pixel outline
            let inside = |xx: i32, yy: i32| yy >= 0 && yy < 22 && xx >= 0 && xx <= yy / 2 + 1;
            let (xi, yi) = (ux as i32, uy as i32);
            let fill = inside(xi, yi);
            let edge = !fill && [(-1, 0), (1, 0), (0, -1), (0, 1), (-1, -1), (1, 1)].iter().any(|&(dx, dy)| inside(xi + dx, yi + dy));
            px[(y * size + x) as usize] = if fill { 0xffff_ffff } else if edge { 0xff00_0000 } else { 0 };
        }
    }
    px
}

/// `cursor probe`: allocate the primary head's channel and say what it looks like (control, status, exception slot, user region), without enabling
/// anything on the display.
pub fn probe() -> Result<(), CursorError> {
    let (regs, head) = (supervisor::regs().ok_or(CursorError::NotReady)?, supervisor::primary_head().ok_or(CursorError::NotReady)?);
    if ALLOCATED.load(Ordering::Acquire) {
        return Ok(());
    }
    let before = (regs.rd32(nc::chan(head).control()), regs.rd32(nc::chan(head).status().0));
    match nc::init(regs, head) {
        Ok(()) => {
            ALLOCATED.store(true, Ordering::Release);
            let c = nc::chan(head);
            log(alloc::format!(
                "cursor: head {} channel allocated (control {:#x} status {:#x} before): control {:#x} status {:#x} exception {:#x} free {}",
                head, before.0, before.1, regs.rd32(c.control()), regs.rd32(c.status().0), regs.rd32(c.exception()), regs.rd32(c.user_base() + nc::USER_FREE)
            ));
            Ok(())
        }
        Err(e) => {
            REFUSED.fetch_add(1, Ordering::Relaxed);
            log(alloc::format!("cursor: head {} channel init failed: {:?}", head, e));
            Err(CursorError::Failed)
        }
    }
}

/// `cursor image [size]`: the test arrow into VRAM (through PRAMIN: written once, read back).
pub fn write_image(size: u32) -> Result<(), CursorError> {
    let regs = supervisor::regs().ok_or(CursorError::NotReady)?;
    if nc::size_code(size).is_none() {
        return Err(CursorError::Invalid);
    }
    let px = arrow(size);
    let mut p = Pramin::new(regs);
    for (i, v) in px.iter().enumerate() {
        p.wr32(IMAGE_VRAM + 4 * i as u64, *v);
    }
    let readback = p.rd32(IMAGE_VRAM + 4 * (size as u64 + 1)); // pixel (1, 1): inside the arrow
    p.restore();
    if readback != px[(size + 1) as usize] {
        log(alloc::format!("cursor: image readback {:#x} != {:#x}", readback, px[(size + 1) as usize]));
        return Err(CursorError::Failed);
    }
    Ok(())
}

/// `cursor raw <method> <value>`: ONE core method plus UPDATE (a debugging ladder: Ryzen #196/#197 left the core not idle after the whole enable
/// push, its exception slot naming method 0x2088 with type 0). Waits for the core to go idle, with IF=1, and logs where it stands.
pub fn raw(method: u32, value: u32) -> Result<(), CursorError> {
    let (regs, head) = (supervisor::regs().ok_or(CursorError::NotReady)?, supervisor::primary_head().ok_or(CursorError::NotReady)?);
    if !nc::core_method_allowed(head, method) {
        return Err(CursorError::Invalid);
    }
    let was_on = x86_64::instructions::interrupts::are_enabled();
    if !was_on {
        x86_64::instructions::interrupts::enable();
    }
    let faults0 = evo::Faults::read(regs);
    let pushed = supervisor::push_core("cursor raw", &[(method, value)], false);
    let idle = pushed.is_ok() && {
        let t0 = crate::cpu::tsc::read();
        loop {
            if evo::CORE.idle(regs) {
                break true;
            }
            if crate::cpu::tsc::read().wrapping_sub(t0) / (crate::cpu::tsc::freq_hz() / 1000).max(1) >= LATCH_MS {
                break false;
            }
            crate::memory::tlb::service_pending();
            core::hint::spin_loop();
        }
    };
    if !was_on {
        x86_64::instructions::interrupts::disable();
    }
    let f = evo::Faults::read(regs).new_since(&faults0);
    log(alloc::format!(
        "cursor: raw {:#x} = {:#x}: push {:?}, core idle {} status {:#x} put {:#x} get {:#x}, slot {:#x},{:#x},{:#x}, new faults ctrl_disp {:#x} exc_other {:#x}",
        method,
        value,
        pushed,
        idle as u8,
        regs.rd32(0x61_0630),
        regs.rd32(evo::CORE.put()),
        regs.rd32(evo::CORE.get()),
        regs.rd32(evo::CORE.exception()),
        regs.rd32(evo::CORE.exception() + 4),
        regs.rd32(evo::CORE.exception() + 8),
        f.ctrl_disp,
        f.exc_other
    ));
    if pushed.is_err() || !idle {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        return Err(CursorError::Failed);
    }
    Ok(())
}

/// `cursor on`: image, channel, the core push, the first position.
pub fn on(size: u32, bounds: bool) -> Result<(), CursorError> {
    if ON.load(Ordering::Acquire) || nc::size_code(size).is_none() {
        return Err(CursorError::Invalid);
    }
    probe()?;
    let (regs, head) = (supervisor::regs().ok_or(CursorError::NotReady)?, supervisor::primary_head().ok_or(CursorError::NotReady)?);
    write_image(size)?;
    let mut methods = nc::core_methods_set(head, nc::HANDLE_CURSOR_CTX, IMAGE_VRAM, size, 0, 0).ok_or(CursorError::Invalid)?;
    if !bounds {
        methods.remove(0);
    }
    // the supervisors (a usage-bounds change may raise them) come by MSI on CPU 0: wait with IF=1, as `modeset::set` and `hdmi::locked` do
    let was_on = x86_64::instructions::interrupts::are_enabled();
    if !was_on {
        x86_64::instructions::interrupts::enable();
    }
    let faults0 = evo::Faults::read(regs);
    let pushed = supervisor::push_core("cursor on", &methods, false);
    let want = nc::control(size, 0, 0).unwrap_or(0);
    let a = evo::CORE.armed_base();
    let latched = pushed.is_ok() && {
        let t0 = crate::cpu::tsc::read();
        let ms = |t0: u64| crate::cpu::tsc::read().wrapping_sub(t0) / (crate::cpu::tsc::freq_hz() / 1000).max(1);
        loop {
            if regs.rd32(a + nc::core_control(head)) == want && evo::CORE.idle(regs) {
                break true;
            }
            if ms(t0) >= LATCH_MS {
                break false;
            }
            crate::memory::tlb::service_pending();
            core::hint::spin_loop();
        }
    };
    if !was_on {
        x86_64::instructions::interrupts::disable();
    }
    if let Err(e) = pushed {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        log(alloc::format!("cursor: core push refused: {:?}", e));
        return Err(match e {
            RequestError::NotReady => CursorError::NotReady,
            _ => CursorError::Failed,
        });
    }
    if !latched {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        let f = evo::Faults::read(regs).new_since(&faults0);
        log(alloc::format!(
            "cursor: the core did not latch the push in {} ms: ARMED control {:#x} (want {:#x}) core idle {} put {:#x} get {:#x} | new faults: ctrl_disp {:#x} exc_other {:#x} core slot {:#x}; slot now {:#x},{:#x},{:#x}",
            LATCH_MS,
            regs.rd32(a + nc::core_control(head)),
            want,
            evo::CORE.idle(regs) as u8,
            regs.rd32(evo::CORE.put()),
            regs.rd32(evo::CORE.get()),
            f.ctrl_disp,
            f.exc_other,
            f.core_exc,
            regs.rd32(evo::CORE.exception()),
            regs.rd32(evo::CORE.exception() + 4),
            regs.rd32(evo::CORE.exception() + 8),
        ));
        return Err(CursorError::Failed);
    }
    SIZE.store(size, Ordering::Release);
    SETS.fetch_add(1, Ordering::Relaxed);
    ON.store(true, Ordering::Release);
    log(alloc::format!("cursor: head {} on, {}x{} at VRAM {:#x}, usage bounds {}", head, size, size, IMAGE_VRAM, if bounds { "pushed" } else { "left" }));
    Ok(())
}

/// `cursor move`: the position write. Counts and times itself (the time is the two stores' cost, an uncached BAR0 write each).
pub fn move_to(x: i32, y: i32) -> Result<(), CursorError> {
    if !ALLOCATED.load(Ordering::Acquire) {
        return Err(CursorError::Invalid);
    }
    let (regs, head) = (supervisor::regs().ok_or(CursorError::NotReady)?, supervisor::primary_head().ok_or(CursorError::NotReady)?);
    let t0 = crate::time::ktime_get();
    let r = nc::move_to(regs, head, x, y);
    let dt = crate::time::ktime_get().saturating_sub(t0) / 1000;
    LAST_MOVE_US.store(dt, Ordering::Relaxed);
    MAX_MOVE_US.fetch_max(dt, Ordering::Relaxed);
    match r {
        Ok(()) => {
            MOVES.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        Err(_) => {
            REFUSED.fetch_add(1, Ordering::Relaxed);
            Err(CursorError::Failed)
        }
    }
}

/// `cursor off`: disable in the core, then release the channel.
pub fn off() -> Result<(), CursorError> {
    if !ON.swap(false, Ordering::AcqRel) {
        return Err(CursorError::Invalid);
    }
    let (regs, head) = (supervisor::regs().ok_or(CursorError::NotReady)?, supervisor::primary_head().ok_or(CursorError::NotReady)?);
    let res = supervisor::push_core("cursor off", &nc::core_methods_clear(head), false);
    if let Err(e) = res {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        log(alloc::format!("cursor: off push refused: {:?}", e));
        ON.store(true, Ordering::Release);
        return Err(CursorError::Failed);
    }
    // the channel is released once the core has let go of the image
    let c = evo::CORE;
    let _ = evo::wait(regs, || c.idle(regs));
    if nc::fini(regs, head).is_err() {
        log(alloc::format!("cursor: head {} channel did not go idle on fini", head));
    }
    ALLOCATED.store(false, Ordering::Release);
    log(alloc::format!("cursor: head {} off", head));
    Ok(())
}

/// What the core has armed for the cursor and what the channel says, for the job's evidence.
pub fn status() -> String {
    let (Some(regs), Some(head)) = (supervisor::regs(), supervisor::primary_head()) else { return String::new() };
    let c = nc::chan(head);
    let a = evo::CORE.armed_base();
    let mut s = String::new();
    let _ = writeln!(
        s,
        "cursor: head {} on={} size={} chan ctl {:#x} st {:#x} exc {:#x},{:#x},{:#x} free {} | core ARMED control {:#x} comp {:#x} ctxdma {:#x} offset {:#x} bounds {:#x} idle {}",
        head,
        ON.load(Ordering::Relaxed) as u8,
        SIZE.load(Ordering::Relaxed),
        regs.rd32(c.control()),
        regs.rd32(c.status().0),
        regs.rd32(c.exception()),
        regs.rd32(c.exception() + 4),
        regs.rd32(c.exception() + 8),
        regs.rd32(c.user_base() + nc::USER_FREE),
        regs.rd32(a + nc::core_control(head)),
        regs.rd32(a + nc::core_composition(head)),
        regs.rd32(a + nc::core_context_dma(head)),
        regs.rd32(a + nc::core_offset(head)),
        regs.rd32(a + nc::core_usage_bounds(head)),
        evo::CORE.idle(regs) as u8
    );
    let _ = writeln!(
        s,
        "cursor: core status {:#x} put {:#x} get {:#x} exception slot {:#x},{:#x},{:#x} ctrl_disp {:#x} exc_other {:#x} supers_done {}",
        regs.rd32(0x61_0630),
        regs.rd32(evo::CORE.put()),
        regs.rd32(evo::CORE.get()),
        regs.rd32(evo::CORE.exception()),
        regs.rd32(evo::CORE.exception() + 4),
        regs.rd32(evo::CORE.exception() + 8),
        regs.rd32(evo::CTRL_DISP_STAT),
        regs.rd32(0x61_1854),
        supervisor::supers_done()
    );
    s
}

pub fn render_kdebug() -> String {
    alloc::format!(
        "gpu_cursor: on={} size={} sets={} moves={} refused={} move_us last={} max={}",
        ON.load(Ordering::Relaxed) as u8,
        SIZE.load(Ordering::Relaxed),
        SETS.load(Ordering::Relaxed),
        MOVES.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
        LAST_MOVE_US.load(Ordering::Relaxed),
        MAX_MOVE_US.load(Ordering::Relaxed)
    )
}
