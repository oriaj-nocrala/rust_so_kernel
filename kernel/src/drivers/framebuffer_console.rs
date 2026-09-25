// kernel/src/drivers/framebuffer_console.rs
//
// Framebuffer text console with ANSI escape code support.
//
// All instances share a single global cursor position (FB_STATE) so
// that parent/child processes after fork() see a consistent cursor.

use alloc::boxed::Box;
use crate::sync::Mutex;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::{
    framebuffer::{FRAMEBUFFER, Color, Framebuffer},
    fs::types::Stat,
    process::file::{FileHandle, FileError, FileResult},
};

// ── Layout constants ──────────────────────────────────────────────────────────

const MARGIN_X: usize = 4;
const MARGIN_Y: usize = 4;

// ── Font ──────────────────────────────────────────────────────────────────────
//
// Noto Sans Mono, pre-rasterised with antialiasing (`noto-sans-mono-bitmap`,
// no_std, no alloc — the crate `bootloader` itself logs with). It replaced
// the 8x8 bitmap font at scale 1, which at 1920x1080 made a 240x120 grid of
// 8-pixel glyphs: hard to read on the physical machine. The size is picked
// once from the screen height (`init_font`) so a line stays around the same
// physical size on any screen; the 8x8 font stays for the boot banner and
// the panic screen, which draw with `Framebuffer::draw_text` directly.

use noto_sans_mono_bitmap::{get_raster, get_raster_width, FontWeight, RasterHeight};

#[derive(Clone, Copy)]
struct FontMetrics {
    size: RasterHeight,
    /// Cell width and height in pixels. The raster height already includes
    /// the font's line spacing, so no extra gap is added.
    w: usize,
    h: usize,
}

const fn metrics(size: RasterHeight) -> FontMetrics {
    FontMetrics {
        size,
        w: get_raster_width(FontWeight::Regular, size),
        h: size.val(),
    }
}

/// Before `init_font` (nothing draws text that early) and on a headless
/// boot.
const DEFAULT_FONT: FontMetrics = metrics(RasterHeight::Size16);

static FONT: spin::Once<FontMetrics> = spin::Once::new();

fn font() -> FontMetrics {
    FONT.get().copied().unwrap_or(DEFAULT_FONT)
}

fn char_w() -> usize {
    font().w
}

fn char_h() -> usize {
    font().h
}

/// Rows of text for the screen height, about 44-54 lines: 16 px up to 720
/// lines, 20 px to 1000, 24 px to 1400 (1920x1080 gets 174x45 cells of
/// 11x24), 32 px above. Needs `noto-sans-mono-bitmap` >= 0.3: 0.2 placed
/// every glyph low in its raster, so descenders (g, j, p, q, y) ran off
/// the bottom of the cell and were cut.
fn pick_font(height: usize) -> RasterHeight {
    match height {
        0..=719 => RasterHeight::Size16,
        720..=999 => RasterHeight::Size20,
        1000..=1399 => RasterHeight::Size24,
        _ => RasterHeight::Size32,
    }
}

/// Choose the console font for the global framebuffer's size. Called once
/// from `init::boot`, right after the framebuffer is registered and before
/// anything writes text.
pub fn init_font() {
    let height = FRAMEBUFFER.lock().as_ref().map(|fb| fb.dimensions().1);
    if let Some(h) = height {
        FONT.call_once(|| metrics(pick_font(h)));
    }
}

/// Draw `text` in the bold 32 px face straight onto `fb`, outside the
/// console grid — the boot banner. Returns the height drawn, so the caller
/// can reserve the rows it covers.
pub fn draw_banner(fb: &mut Framebuffer, x: usize, y: usize, text: &str, fg: Color) -> usize {
    let size = RasterHeight::Size32;
    let w = get_raster_width(FontWeight::Bold, size);
    for (i, c) in text.chars().enumerate() {
        if let Some(glyph) = get_raster(c, FontWeight::Bold, size) {
            fb.draw_glyph(x + i * w, y, w, size.val(), glyph.raster(), fg, DEFAULT_BG);
        }
    }
    size.val()
}

/// Cell size in pixels, for `/proc/fbinfo`.
pub fn cell_size() -> (usize, usize) {
    (char_w(), char_h())
}

// Soft white on black rather than VGA's grey: easier on the eyes at this
// size without looking washed out.
const DEFAULT_FG: Color = Color::rgb(0xD8, 0xDB, 0xE0);
const DEFAULT_BG: Color = Color::rgb(0, 0, 0);

// ── ANSI color palette ────────────────────────────────────────────────────────
//
// Tuned for a black background (close to Atom's One Dark). The old one was
// the VGA text-mode palette, whose blue is (0, 0, 170): nearly invisible on
// black, and it is the colour `ls --color` gives every directory.

const ANSI_COLORS: [Color; 8] = [
    Color::rgb(0x3F, 0x44, 0x4E), // 0: black (a dark grey, so it still shows)
    Color::rgb(0xE0, 0x6C, 0x75), // 1: red
    Color::rgb(0x98, 0xC3, 0x79), // 2: green
    Color::rgb(0xE5, 0xC0, 0x7B), // 3: yellow
    Color::rgb(0x61, 0xAF, 0xEF), // 4: blue
    Color::rgb(0xC6, 0x78, 0xDD), // 5: magenta
    Color::rgb(0x56, 0xB6, 0xC2), // 6: cyan
    Color::rgb(0xD8, 0xDB, 0xE0), // 7: white
];

const ANSI_BRIGHT: [Color; 8] = [
    Color::rgb(0x7F, 0x84, 0x8E), // 0: bright black (grey)
    Color::rgb(0xFF, 0x7A, 0x85), // 1: bright red
    Color::rgb(0xB5, 0xE8, 0x90), // 2: bright green
    Color::rgb(0xFF, 0xD6, 0x8A), // 3: bright yellow
    Color::rgb(0x8C, 0xC8, 0xFF), // 4: bright blue
    Color::rgb(0xDD, 0x9C, 0xF5), // 5: bright magenta
    Color::rgb(0x7F, 0xD8, 0xE3), // 6: bright cyan
    Color::rgb(0xFF, 0xFF, 0xFF), // 7: bright white
];

fn ansi_color(idx: u8, bright: bool) -> Color {
    let i = (idx as usize) & 7;
    if bright { ANSI_BRIGHT[i] } else { ANSI_COLORS[i] }
}

/// Palette entry 0 is a dark grey only so that black *text* stays visible on
/// the black screen. As a background it has to be real black: programs that
/// ask for `ESC[40m` (cmatrix, ncurses apps with a black pair) mean "the
/// screen colour", and got a grey slab instead.
fn ansi_bg(idx: u8) -> Color {
    if idx & 7 == 0 { DEFAULT_BG } else { ansi_color(idx, false) }
}

fn color256_bg(n: u8) -> Color {
    if n == 0 { DEFAULT_BG } else { color256(n) }
}

fn color256(n: u8) -> Color {
    match n {
        0..=7   => ANSI_COLORS[n as usize],
        8..=15  => ANSI_BRIGHT[(n - 8) as usize],
        16..=231 => {
            // 6×6×6 cube: index = 16 + 36*r + 6*g + b, each component 0-5
            let idx = n - 16;
            let b_comp = idx % 6;
            let g_comp = (idx / 6) % 6;
            let r_comp = idx / 36;
            let scale = |v: u8| if v == 0 { 0u8 } else { 55u8.saturating_add(v.saturating_mul(40)) };
            Color::rgb(scale(r_comp), scale(g_comp), scale(b_comp))
        }
        232..=255 => {
            // 24 grayscale steps from 8 to 238
            let v = 8u8.saturating_add((n - 232).saturating_mul(10));
            Color::rgb(v, v, v)
        }
    }
}

// ── ANSI state machine ────────────────────────────────────────────────────────

enum AnsiState {
    Normal,
    Escape,
    Csi { buf: [u8; 32], len: usize },
}

// ── Global cursor + color state ───────────────────────────────────────────────

struct FbState {
    col:  usize,
    row:  usize,
    fg:   Color,
    bg:   Color,
    /// SGR 1: drawn with the bold weight of the font.
    bold: bool,
    /// SGR 7: foreground and background swapped (`less`'s and `vi`'s
    /// status lines, selections).
    reverse: bool,
    ansi: AnsiState,
}

static FB_STATE: Mutex<FbState> = Mutex::new(FbState {
    col: 0,
    row: 0,
    fg: DEFAULT_FG,
    bg: DEFAULT_BG,
    bold: false,
    reverse: false,
    ansi: AnsiState::Normal,
});

/// Draw one character cell at pixel `(px, py)` in the current attributes.
/// A byte the font has no glyph for (anything outside printable ASCII
/// reaches here only as such) is drawn as `?`.
fn draw_cell(fb: &mut Framebuffer, px: usize, py: usize, byte: u8, state: &FbState) {
    let f = font();
    let (fg, bg) = if state.reverse { (state.bg, state.fg) } else { (state.fg, state.bg) };
    let weight = if state.bold { FontWeight::Bold } else { FontWeight::Regular };
    match get_raster(byte as char, weight, f.size).or_else(|| get_raster('?', weight, f.size)) {
        Some(glyph) => fb.draw_glyph(px, py, f.w, f.h, glyph.raster(), fg, bg),
        None => fb.fill_rect(px, py, f.w, f.h, bg),
    }
}
static FB_CLEARED: AtomicBool = AtomicBool::new(false);

/// Set by [`kernel_alert`] / [`kernel_print`] — i.e. whenever the kernel
/// itself has put text on the console. Read by `FramebufferConsole::new`,
/// which must not clear the screen out from under it; see that function.
static KERNEL_WROTE: AtomicBool = AtomicBool::new(false);

/// Set by `FBIO_BLIT` (`sys_ioctl`) every time a raw-pixel client (e.g. the
/// DOOM port) blits a frame directly onto the framebuffer, bypassing this
/// driver's char/cursor tracking entirely. `FbState.row`/`col` are left
/// stale from whatever text was on screen before the raw client started —
/// without this flag, the next text write (e.g. the shell prompt after
/// DOOM exits) resumes at that stale position on top of the client's last
/// rendered frame instead of a clean screen. Checked and cleared on the
/// next `FramebufferConsole::write`, which does one full clear + cursor
/// reset before drawing anything.
static FB_RAW_DIRTY: AtomicBool = AtomicBool::new(false);

/// Called by `FBIO_BLIT`'s ioctl handler after every raw blit.
pub fn mark_raw_dirty() {
    FB_RAW_DIRTY.store(true, Ordering::SeqCst);
}

/// Graphics mode (Linux's `KD_GRAPHICS`): someone holds `/dev/fb0` and
/// the screen is theirs. The console keeps parsing what processes write
/// to `/dev/fb` (and mirroring it to serial and `klog`) but draws none of
/// it, and the cursor stops blinking. `kalert!` still draws — a process
/// death must stay visible — and whoever owns the screen paints over it.
static GRAPHICS: AtomicBool = AtomicBool::new(false);

pub fn in_graphics_mode() -> bool {
    GRAPHICS.load(Ordering::SeqCst)
}

/// `/dev/fb0` was opened. Whatever cursor was on screen is about to be
/// painted over, so the console forgets it.
pub fn enter_graphics_mode() {
    GRAPHICS.store(true, Ordering::SeqCst);
    CURSOR_DRAWN.store(false, Ordering::Relaxed);
}

/// The last `/dev/fb0` handle is gone. The console has no copy of the text
/// the graphics client covered, so it starts over on a clear screen, as it
/// does after a raw blit — now rather than at the next write, so a
/// compositor that dies in the background does not leave its last frame
/// up.
pub fn leave_graphics_mode() {
    GRAPHICS.store(false, Ordering::SeqCst);
    FB_RAW_DIRTY.store(true, Ordering::SeqCst);
    let mut state = FB_STATE.lock();
    let mut fb_guard = FRAMEBUFFER.lock();
    if let Some(fb) = fb_guard.as_mut() {
        render_bytes(&mut state, fb, b"");
    }
}

// ── Blinking text cursor ──────────────────────────────────────────────────────
//
// Renders as an inverse-video block over the current cell, toggled by
// `tick_cursor_blink()` (called from the 100 Hz PIT ISR). It never tracks
// glyph content — `xor_rect` is self-inverting, so "drawn"/"not drawn" is
// the only state that needs to survive between toggles. `CURSOR_DRAWN`
// records which of those two states is currently on screen so a text write
// landing on the cursor cell can undo it before drawing over it, and so the
// ISR toggle and a concurrent write never fight over the same XOR parity.

static CURSOR_DRAWN: AtomicBool = AtomicBool::new(false);
static CURSOR_TICKS: AtomicU64 = AtomicU64::new(0);
/// The timer tick is 100 Hz (the LAPIC timer, or the PIT on the 8259
/// fallback — see `interrupts::apic`); 50 ticks is
/// a 500ms on/off period, the conventional terminal blink rate.
const CURSOR_BLINK_TICKS: u64 = 50;

fn cursor_cell_rect(state: &FbState) -> (usize, usize, usize, usize) {
    let px = MARGIN_X + state.col * char_w();
    let py = MARGIN_Y + state.row * char_h();
    (px, py, char_w(), char_h())
}

/// If the cursor is currently rendered (inverted) at `state`'s position,
/// un-invert it and clear the flag. Must be called with both `FB_STATE` and
/// `FRAMEBUFFER` already locked by the caller.
fn undraw_cursor_locked(state: &FbState, fb: &mut Framebuffer) {
    if CURSOR_DRAWN.swap(false, Ordering::Relaxed) {
        let (x, y, w, h) = cursor_cell_rect(state);
        fb.xor_rect(x, y, w, h);
    }
}

/// Called once per PIT tick from `timer_preempt_handler`. Runs with
/// interrupts already disabled (ISR context), so it must never block: a
/// `lock()` here could spin forever against a process that was preempted
/// mid-write while holding the same lock. `try_lock` + "skip this beat" is
/// the whole strategy — missing an occasional 10ms tick just delays the
/// next blink phase slightly, which is invisible to a human.
pub fn tick_cursor_blink() {
    // A raw-blit client (DOOM/Quake) or a graphics-mode one (`/dev/fb0`)
    // owns the screen; any cursor state from before it started pointed at
    // pixels that are long gone, and inverting "the same" rectangle now
    // would just corrupt its frame.
    if FB_RAW_DIRTY.load(Ordering::SeqCst) || GRAPHICS.load(Ordering::SeqCst) {
        CURSOR_DRAWN.store(false, Ordering::Relaxed);
        return;
    }

    let ticks = CURSOR_TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    if ticks % CURSOR_BLINK_TICKS != 0 {
        return;
    }

    let Some(state) = FB_STATE.try_lock() else { return; };
    let Some(mut fb_guard) = FRAMEBUFFER.try_lock() else { return; };
    let Some(fb) = fb_guard.as_mut() else { return; };

    let (x, y, w, h) = cursor_cell_rect(&state);
    fb.xor_rect(x, y, w, h);
    let now_drawn = !CURSOR_DRAWN.load(Ordering::Relaxed);
    CURSOR_DRAWN.store(now_drawn, Ordering::Relaxed);
}

// ── Serial mirror ──────────────────────────────────────────────────────────
//
// User-process stdout *and* stderr (fds 1 and 2) are both bound to this
// driver, so they're only ever visible on the framebuffer — invisible in
// headless runs (`-display none`) short of a `screendump`. Mirror every byte
// written here out over COM1 too (raw port I/O, same as
// SerialConsole::write — no shared lock, so no deadlock risk against the
// FB_STATE/FRAMEBUFFER locks already held by the caller), tagged with a
// `[fb] ` prefix at the start of each line so it's greppable/distinguishable
// from the kernel's own `serial_println!` diagnostics in the same log. The
// mirror also feeds `klog`, so `/proc/dmesg` and the USB log partition carry
// user output too.
static STDOUT_AT_LINE_START: AtomicBool = AtomicBool::new(true);

fn mirror_to_serial(buf: &[u8]) {
    use x86_64::instructions::port::Port;
    let t0 = crate::cpu::tsc::read();
    let mut port = Port::<u8>::new(0x3F8);
    // The same bytes go into the kernel log ring, one `push` per line
    // rather than per byte. Without this, user output reached COM1 but not
    // `klog` — so not `/proc/dmesg`, and not the USB log partition
    // (`block::logpart`), which on the serial-less target is the only place
    // a program's output can be read back from.
    for line in buf.split_inclusive(|&b| b == b'\n') {
        if STDOUT_AT_LINE_START.load(Ordering::Relaxed) {
            for &b in b"[fb] " {
                unsafe { port.write(b); }
            }
            crate::klog::push(b"[fb] ");
            STDOUT_AT_LINE_START.store(false, Ordering::Relaxed);
        }
        for &byte in line {
            unsafe { port.write(byte); }
        }
        crate::klog::push(line);
        if line.last() == Some(&b'\n') {
            STDOUT_AT_LINE_START.store(true, Ordering::Relaxed);
        }
    }
    // Counted because on the target machine nothing is listening at the
    // other end: this is one I/O port write per byte of user output, on
    // the hot path, for a log nobody can read there. Whether that is worth
    // a condition is a question for the measurement, not for a guess —
    // see `/proc/fbinfo`'s `fb_serial_mirror`.
    crate::debug::FB_SERIAL_MIRROR.record(buf.len() as u64, crate::cpu::tsc::read().wrapping_sub(t0));
}

// ── Parse CSI parameter string ────────────────────────────────────────────────

fn parse_params(buf: &[u8]) -> ([u32; 16], usize) {
    if buf.is_empty() {
        return ([0u32; 16], 1);
    }

    let mut params = [0u32; 16];
    let mut count = 0usize;
    let mut cur = 0u32;

    for &b in buf {
        if b == b';' {
            if count < 16 {
                params[count] = cur;
                count += 1;
            }
            cur = 0;
        } else if b >= b'0' && b <= b'9' {
            cur = cur.saturating_mul(10).saturating_add((b - b'0') as u32);
        }
    }
    if count < 16 {
        params[count] = cur;
        count += 1;
    }

    (params, count)
}

// ── SGR handler ───────────────────────────────────────────────────────────────

fn apply_sgr(params: &[u32], state: &mut FbState) {
    let mut i = 0;
    while i < params.len() {
        match params[i] {
            0 => {
                state.fg = DEFAULT_FG;
                state.bg = DEFAULT_BG;
                state.bold = false;
                state.reverse = false;
            }
            1 => state.bold = true,
            7 => state.reverse = true,
            22 => state.bold = false,
            27 => state.reverse = false,
            2..=29 => {}  // dim, italic, underline, blink... — ignored
            30..=37 => state.fg = ansi_color((params[i] - 30) as u8, false),
            38 => {
                if i + 2 < params.len() && params[i + 1] == 5 {
                    state.fg = color256(params[i + 2] as u8);
                    i += 2;
                } else if i + 4 < params.len() && params[i + 1] == 2 {
                    state.fg = Color::rgb(
                        params[i + 2] as u8,
                        params[i + 3] as u8,
                        params[i + 4] as u8,
                    );
                    i += 4;
                }
            }
            39 => state.fg = DEFAULT_FG,
            40..=47 => state.bg = ansi_bg((params[i] - 40) as u8),
            48 => {
                if i + 2 < params.len() && params[i + 1] == 5 {
                    state.bg = color256_bg(params[i + 2] as u8);
                    i += 2;
                } else if i + 4 < params.len() && params[i + 1] == 2 {
                    state.bg = Color::rgb(
                        params[i + 2] as u8,
                        params[i + 3] as u8,
                        params[i + 4] as u8,
                    );
                    i += 4;
                }
            }
            49 => state.bg = DEFAULT_BG,
            90..=97  => state.fg = ansi_color((params[i] - 90) as u8, true),
            100..=107 => state.bg = ansi_color((params[i] - 100) as u8, true),
            _ => {}
        }
        i += 1;
    }
}

/// Overwrite `row`'s cells in `[start_col, end_col)` with blanks in the
/// current background color. Shared by `ESC[J`'s partial-screen-clear
/// cases (0 and 1), which need to blank a range of whole rows plus one
/// partial row, the same way `ESC[K` already blanks a range within a
/// single row.
///
/// One `fill_rect` for the whole span, not one `draw_char(b' ')` per cell.
/// That rewrite is the fix for the symptom this whole line of work started
/// from: on the physical machine, `ash` redrawing its line after a
/// backspace emits `ESC[J`, which from a prompt a quarter of the way down
/// a 1920x1080 screen used to mean ~22,000 cells x 64 pixels ~= 1.4
/// million individual writes to an uncacheable PCIe aperture — about a
/// second of frozen console, and imperceptible in QEMU because there the
/// framebuffer is host RAM. See `Framebuffer::fill_rect`.
///
/// Blanks the full cell height, including the line-spacing rows that
/// `draw_char` never touches — the old per-cell version left that row
/// holding whatever was there, which after a scroll or a raw blit was not
/// necessarily background.
fn clear_row_from(fb: &mut Framebuffer, state: &FbState, row: usize, start_col: usize, end_col: usize) {
    if end_col <= start_col {
        return;
    }
    fb.fill_rect(
        MARGIN_X + start_col * char_w(),
        MARGIN_Y + row * char_h(),
        (end_col - start_col) * char_w(),
        char_h(),
        state.bg,
    );
}

/// Blank whole rows `[row0, row1)` across all `cols` columns — one
/// `fill_rect` for the entire block rather than one per row, which is what
/// turns `ESC[J`'s "and everything below" into a single operation.
fn clear_rows(fb: &mut Framebuffer, state: &FbState, row0: usize, row1: usize, cols: usize) {
    if row1 <= row0 || cols == 0 {
        return;
    }
    fb.fill_rect(
        MARGIN_X,
        MARGIN_Y + row0 * char_h(),
        cols * char_w(),
        (row1 - row0) * char_h(),
        state.bg,
    );
}

// ── CSI dispatcher ────────────────────────────────────────────────────────────

fn dispatch_csi(
    final_byte: u8,
    param_buf: &[u8],
    state: &mut FbState,
    fb: &mut Framebuffer,
    cols: usize,
    rows: usize,
) {
    let (params, nparams) = parse_params(param_buf);

    match final_byte {
        b'm' => {
            apply_sgr(&params[..nparams], state);
        }
        b'H' | b'f' => {
            // ESC[r;cH — cursor position (1-based, default 1;1)
            let r = if params[0] == 0 { 1 } else { params[0] as usize };
            let c = if nparams < 2 || params[1] == 0 { 1 } else { params[1] as usize };
            state.row = (r - 1).min(rows - 1);
            state.col = (c - 1).min(cols - 1);
        }
        b'A' => {
            let n = if params[0] == 0 { 1 } else { params[0] as usize };
            state.row = state.row.saturating_sub(n);
        }
        b'B' => {
            let n = if params[0] == 0 { 1 } else { params[0] as usize };
            state.row = (state.row + n).min(rows - 1);
        }
        b'C' => {
            let n = if params[0] == 0 { 1 } else { params[0] as usize };
            state.col = (state.col + n).min(cols - 1);
        }
        b'D' => {
            let n = if params[0] == 0 { 1 } else { params[0] as usize };
            state.col = state.col.saturating_sub(n);
        }
        b'J' => {
            // ESC[J with no explicit parameter means ESC[0J ("clear from
            // cursor to end of screen"), not "do nothing" — `parse_params`
            // already reports that as params[0] == 0, same as a real
            // ESC[0J. Real full-screen apps (BusyBox `vi`'s `redraw()`,
            // see ESC_SET_CURSOR_TOPLEFT ESC_CLEAR2EOS) send exactly
            // ESC[H ESC[J to clear the whole screen — cursor-home followed
            // by the no-param form — never ESC[2J. Treating the no-param
            // case as a no-op (the previous behavior here) meant the
            // screen was never actually cleared: only the cells a program
            // explicitly overwrote changed, leaving old text bleeding
            // through everywhere else (visible as e.g. the boot banner's
            // "BusyBox..." still on screen with just its leading "B"
            // overwritten by vi's "~" column).
            match params[0] {
                0 => {
                    clear_row_from(fb, state, state.row, state.col, cols);
                    clear_rows(fb, state, state.row + 1, rows, cols);
                }
                1 => {
                    clear_rows(fb, state, 0, state.row, cols);
                    clear_row_from(fb, state, state.row, 0, state.col + 1);
                }
                2 | 3 => {
                    fb.clear(state.bg);
                    state.col = 0;
                    state.row = 0;
                }
                _ => {}
            }
        }
        b'K' => {
            // Same one-`fill_rect`-per-span treatment as `ESC[J` above;
            // `vi` emits an `ESC[K` per line it repaints, so this is the
            // hot one in a full-screen editor rather than the dramatic one.
            match params[0] {
                0 => clear_row_from(fb, state, state.row, state.col, cols),
                1 => clear_row_from(fb, state, state.row, 0, state.col + 1),
                2 => clear_row_from(fb, state, state.row, 0, cols),
                _ => {}
            }
        }
        _ => {}
    }
}


/// Render `buf` onto the console: the whole text path — control codes, ANSI
/// escapes, scrolling and cursor bookkeeping — with both locks already held
/// by the caller.
///
/// Split out of `FramebufferConsole::write` so the kernel's own
/// [`kernel_alert`] can reach the same renderer without either duplicating
/// the ANSI parser or taking the blocking `lock()`s that a `FileHandle`
/// write can afford and a fault handler cannot.
///
/// One batch per call (`Framebuffer::begin_batch`): with a RAM shadow, the
/// primitives below only draw into RAM and VRAM gets one copy of the
/// union of what they touched, at the end. A write of 400 lines that
/// scrolls 400 times is then 400 `memmove`s in RAM and one flush, instead
/// of 400 full-screen copies. Batches nest, so callers that render several
/// pieces under one lock (`kernel_write_bytes`, `kalert!`) wrap themselves
/// too and flush once.
fn render_bytes(state: &mut FbState, fb: &mut Framebuffer, buf: &[u8]) {
    fb.begin_batch();
    render_bytes_inner(state, fb, buf);
    fb.end_batch();
}

fn render_bytes_inner(state: &mut FbState, fb: &mut Framebuffer, buf: &[u8]) {
    let t0 = crate::cpu::tsc::read();
    // Only `kalert!` renders in graphics mode: no cursor to undraw or to
    // leave behind, and no clearing the graphics client's screen.
    let graphics = GRAPHICS.load(Ordering::SeqCst);
    if FB_RAW_DIRTY.load(Ordering::SeqCst) || graphics {
        // Screen already belongs to (or was just handed back from) a
        // raw-blit client — whatever the flag was tracking is stale.
        CURSOR_DRAWN.store(false, Ordering::Relaxed);
    } else {
        undraw_cursor_locked(state, fb);
    }

    if !graphics && FB_RAW_DIRTY.swap(false, Ordering::SeqCst) {
        fb.clear(DEFAULT_BG);
        state.col = 0;
        state.row = 0;
        state.fg = DEFAULT_FG;
        state.bg = DEFAULT_BG;
        state.bold = false;
        state.reverse = false;
        state.ansi = AnsiState::Normal;
    }

    let (w, h) = fb.dimensions();
    let cols = (w.saturating_sub(MARGIN_X)) / char_w();
    let rows = (h.saturating_sub(MARGIN_Y)) / char_h();

    for &byte in buf {
        // Replace state.ansi with Normal, taking ownership of the old value.
        // This avoids a borrow conflict when we need &mut state later.
        let ansi = core::mem::replace(&mut state.ansi, AnsiState::Normal);
        match ansi {
            AnsiState::Normal => {
                match byte {
                    0x1B => {
                        state.ansi = AnsiState::Escape;
                    }
                    b'\n' => {
                        state.col = 0;
                        state.row += 1;
                        if state.row >= rows {
                            fb.scroll_up(char_h());
                            state.row = rows - 1;
                        }
                    }
                    b'\r' => {
                        state.col = 0;
                    }
                    0x08 | 0x7f => {
                        if state.col > 0 {
                            state.col -= 1;
                            let px = MARGIN_X + state.col * char_w();
                            let py = MARGIN_Y + state.row * char_h();
                            draw_cell(fb, px, py, b' ', state);
                        }
                    }
                    b if b >= 0x20 && b < 0x7f => {
                        let px = MARGIN_X + state.col * char_w();
                        let py = MARGIN_Y + state.row * char_h();
                        draw_cell(fb, px, py, b, state);
                        state.col += 1;
                        if state.col >= cols {
                            state.col = 0;
                            state.row += 1;
                            if state.row >= rows {
                                fb.scroll_up(char_h());
                                state.row = rows - 1;
                            }
                        }
                    }
                    _ => {}
                }
            }
            AnsiState::Escape => {
                if byte == b'[' {
                    state.ansi = AnsiState::Csi { buf: [0u8; 32], len: 0 };
                }
                // else: unrecognised escape — state.ansi stays Normal
            }
            AnsiState::Csi { mut buf, mut len } => {
                if byte >= 0x40 && byte <= 0x7E {
                    // Final byte — dispatch and return to Normal
                    dispatch_csi(byte, &buf[..len], &mut *state, fb, cols, rows);
                    // state.ansi already Normal from the replace above
                } else if byte >= 0x20 && byte <= 0x3F {
                    // Parameter or intermediate byte — accumulate
                    if len < 32 {
                        buf[len] = byte;
                        len += 1;
                    }
                    state.ansi = AnsiState::Csi { buf, len };
                }
                // else: C0 control inside CSI — abort, stay Normal
            }
        }
    }

    // Show the cursor solid at its new position right away instead of
    // waiting up to one full blink period — same feel as a real
    // terminal, which keeps the cursor lit right after each keystroke
    // and only starts blinking once input pauses.
    if !graphics {
        let (x, y, w, h) = cursor_cell_rect(state);
        fb.xor_rect(x, y, w, h);
        CURSOR_DRAWN.store(true, Ordering::Relaxed);
        CURSOR_TICKS.store(0, Ordering::Relaxed);
    }

    // `bytes` here is the *input* byte count, not framebuffer bytes — the
    // rate `/proc/fbinfo` derives from it is "console throughput in bytes
    // of text per second", which is the figure a human comparing a
    // before/after actually wants, not a memory bandwidth.
    crate::debug::FB_RENDER.record(buf.len() as u64, crate::cpu::tsc::read().wrapping_sub(t0));
}

// ── Kernel-originated notices ────────────────────────────────────────────────

/// Adapter letting `core::fmt` write straight through [`render_bytes`] with
/// the locks already held — no intermediate buffer, and in particular no
/// allocation, since the one caller runs inside a CPU exception handler.
struct ConsoleWriter<'a> {
    state: &'a mut FbState,
    fb:    &'a mut Framebuffer,
}

impl core::fmt::Write for ConsoleWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        render_bytes(self.state, self.fb, s.as_bytes());
        Ok(())
    }
}

/// Announce a kernel-level event on the framebuffer console, in bright red,
/// on a line of its own. Use [`kalert!`] rather than calling this directly.
///
/// Why this exists at all, when `serial_println!` already says everything:
/// this kernel is also brought up on a physical machine with no serial
/// capture, and there a killed process and a hard hang look *identical* —
/// the screen simply stops changing while the PIT-driven cursor keeps
/// blinking. Telling those two apart cost real time during the 2026-09-21
/// bring-up, and nothing on screen distinguished them.
///
/// `try_lock`, never `lock`, on both locks. The caller is a fault handler,
/// which can run while the interrupted process sits mid-`write` holding
/// either one; blocking there would turn a process death into exactly the
/// system-wide freeze this is meant to rule out. Same "skip this beat"
/// strategy `tick_cursor_blink` uses, for the same reason — and the cost of
/// skipping is only a line the serial log still has.
///
/// Deliberately does *not* mirror to serial: every caller already logs its
/// own `serial_println!`, and duplicating it would just double the noise in
/// the one place that was never the problem.
pub fn kernel_alert(args: core::fmt::Arguments) {
    use core::fmt::Write;

    let Some(mut state) = FB_STATE.try_lock() else { return };
    let Some(mut fb_guard) = FRAMEBUFFER.try_lock() else { return };
    let Some(fb) = fb_guard.as_mut() else { return };

    fb.begin_batch();
    let mut w = ConsoleWriter { state: &mut state, fb };
    // Reset the colour afterwards so the next writer — a shell prompt, some
    // other process's output — is not left painted red.
    let _ = w.write_str("\r\n\x1b[1;31m");
    let _ = w.write_fmt(args);
    let _ = w.write_str("\x1b[0m\r\n");
    w.fb.end_batch();
    KERNEL_WROTE.store(true, Ordering::SeqCst);
}

/// Plain (uncoloured) kernel output on the console — [`kernel_alert`]'s
/// quiet sibling, for text that is informative rather than alarming: the
/// boot-log dump `init::boot` renders when the machine turns out to have no
/// keyboard at all, which would be unreadable in all-red.
///
/// Same `try_lock`-never-`lock` discipline and the same reason. Marks the
/// console as kernel-written so the first user process does not clear it.
pub fn kernel_print(args: core::fmt::Arguments) {
    use core::fmt::Write;

    if in_graphics_mode() {
        return;
    }
    let Some(mut state) = FB_STATE.try_lock() else { return };
    let Some(mut fb_guard) = FRAMEBUFFER.try_lock() else { return };
    let Some(fb) = fb_guard.as_mut() else { return };

    fb.begin_batch();
    let mut w = ConsoleWriter { state: &mut state, fb };
    let _ = w.write_fmt(args);
    w.fb.end_batch();
    KERNEL_WROTE.store(true, Ordering::SeqCst);
}

/// Writes raw bytes to the console, translating bare `\n` into `\r\n` — the
/// boot-log dump's path, since `klog`'s contents are newline-terminated
/// lines as they were handed to the serial port, which has no notion of a
/// carriage return being required.
pub fn kernel_write_bytes(buf: &[u8]) {
    if in_graphics_mode() {
        return;
    }
    let Some(mut state) = FB_STATE.try_lock() else { return };
    let Some(mut fb_guard) = FRAMEBUFFER.try_lock() else { return };
    let Some(fb) = fb_guard.as_mut() else { return };

    // One batch around the whole dump: byte-at-a-time `render_bytes`
    // calls would otherwise each flush on their own.
    fb.begin_batch();
    for &b in buf {
        if b == b'\n' {
            render_bytes(&mut state, fb, b"\r\n");
        } else {
            render_bytes(&mut state, fb, &[b]);
        }
    }
    fb.end_batch();
    KERNEL_WROTE.store(true, Ordering::SeqCst);
}

/// Moves the console's cursor below the top `px` pixel rows, so text drawn directly onto
/// the framebuffer above it (the boot banner) is not overprinted. Only ever
/// moves the cursor forward.
pub fn reserve_pixels_at_top(px: usize) {
    let rows = px.saturating_sub(MARGIN_Y).div_ceil(char_h());
    let Some(mut state) = FB_STATE.try_lock() else { return };
    if state.row < rows {
        state.row = rows;
        state.col = 0;
    }
}

/// `kernel_print!`: [`kernel_alert`]'s uncoloured counterpart.
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => {
        $crate::drivers::framebuffer_console::kernel_print(format_args!($($arg)*))
    };
}

/// `serial_println!`'s visible counterpart: renders one bright-red notice on
/// the framebuffer console. For events a user watching a screen with no
/// serial attached must not miss — a process dying, above all. See
/// [`kernel_alert`].
#[macro_export]
macro_rules! kalert {
    ($($arg:tt)*) => {
        $crate::drivers::framebuffer_console::kernel_alert(format_args!($($arg)*))
    };
}

// ── Driver struct (ZST — all state is global) ─────────────────────────────────

pub struct FramebufferConsole;

impl FramebufferConsole {
    /// Clears the screen once, the first time a process opens `/dev/fb` —
    /// **unless the kernel has already written to the console itself**.
    ///
    /// That exception is not cosmetic. The clear runs when PID 1's stdout
    /// is opened, which is after every driver has initialised, so it used
    /// to erase every `kalert!` the boot had produced microseconds before
    /// anyone could read it. On the machine those notices exist for — no
    /// serial capture, screen only — the result was a driver that reported
    /// its own failure into a buffer that was then wiped, which is
    /// indistinguishable from a driver that said nothing at all. It cost a
    /// full bare-metal debugging cycle to notice.
    ///
    /// When the kernel has drawn, the console instead continues below what
    /// is already there: the shell's output scrolls up from the boot log
    /// rather than replacing it.
    pub fn new() -> Self {
        if !FB_CLEARED.swap(true, Ordering::SeqCst) && !KERNEL_WROTE.load(Ordering::SeqCst) {
            if let Some(fb) = FRAMEBUFFER.lock().as_mut() {
                fb.clear(DEFAULT_BG);
            }
        }
        Self
    }
}

impl FileHandle for FramebufferConsole {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Err(FileError::NotSupported)
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        mirror_to_serial(buf);
        if in_graphics_mode() {
            // Not drawn: the screen belongs to `/dev/fb0`'s holder.
            return Ok(buf.len());
        }

        let mut state = FB_STATE.lock();
        let mut fb_guard = FRAMEBUFFER.lock();
        let Some(fb) = fb_guard.as_mut() else { return Ok(buf.len()); };

        render_bytes(&mut state, fb, buf);

        Ok(buf.len())
    }

    fn stat(&self) -> Option<Stat> {
        Some(Stat::chardev(0))
    }

    // A bare unit struct — all real state (cursor, color, ANSI parser) is
    // the global FB_STATE, so a second instance is already a correct dup,
    // no need to route through ::new()'s one-time-clear check again.
    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        Some(Box::new(FramebufferConsole))
    }

    fn name(&self) -> &str {
        "fb"
    }
}

/// Text grid size (cols, rows) of the framebuffer console, in the same
/// units `TIOCGWINSZ` reports. Falls back to 80x25 if no framebuffer was
/// set up (headless/serial-only boot) — same default the ioctl used to
/// hardcode unconditionally.
pub fn text_dimensions() -> (usize, usize) {
    let fb_guard = FRAMEBUFFER.lock();
    let Some(fb) = fb_guard.as_ref() else { return (80, 25); };
    let (w, h) = fb.dimensions();
    let cols = (w.saturating_sub(MARGIN_X)) / char_w();
    let rows = (h.saturating_sub(MARGIN_Y)) / char_h();
    (cols.max(1), rows.max(1))
}

pub fn open() -> Box<dyn FileHandle> {
    Box::new(FramebufferConsole::new())
}
