// kernel/src/drivers/framebuffer_console.rs
//
// Framebuffer text console with ANSI escape code support.
//
// All instances share a single global cursor position (FB_STATE) so
// that parent/child processes after fork() see a consistent cursor.

use alloc::boxed::Box;
use spin::Mutex;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::{
    framebuffer::{FRAMEBUFFER, Color, Framebuffer},
    fs::types::Stat,
    process::file::{FileHandle, FileError, FileResult},
};

// ── Layout constants ──────────────────────────────────────────────────────────

const MARGIN_X: usize = 4;
const MARGIN_Y: usize = 4;
const CHAR_W:   usize = 8;   // font8x8 glyphs are 8 px wide
const CHAR_H:   usize = 9;   // 8px glyph + 1px gap

const DEFAULT_FG: Color = Color::rgb(220, 220, 220);
const DEFAULT_BG: Color = Color::rgb(0, 0, 0);

// ── ANSI color palette ────────────────────────────────────────────────────────

const ANSI_COLORS: [Color; 8] = [
    Color::rgb(0,   0,   0  ), // 0: black
    Color::rgb(170, 0,   0  ), // 1: red
    Color::rgb(0,   170, 0  ), // 2: green
    Color::rgb(170, 85,  0  ), // 3: yellow (dark)
    Color::rgb(0,   0,   170), // 4: blue
    Color::rgb(170, 0,   170), // 5: magenta
    Color::rgb(0,   170, 170), // 6: cyan
    Color::rgb(170, 170, 170), // 7: white (light gray)
];

const ANSI_BRIGHT: [Color; 8] = [
    Color::rgb(85,  85,  85 ), // 0: bright black (dark gray)
    Color::rgb(255, 85,  85 ), // 1: bright red
    Color::rgb(85,  255, 85 ), // 2: bright green
    Color::rgb(255, 255, 85 ), // 3: bright yellow
    Color::rgb(85,  85,  255), // 4: bright blue
    Color::rgb(255, 85,  255), // 5: bright magenta
    Color::rgb(85,  255, 255), // 6: bright cyan
    Color::rgb(255, 255, 255), // 7: bright white
];

fn ansi_color(idx: u8, bright: bool) -> Color {
    let i = (idx as usize) & 7;
    if bright { ANSI_BRIGHT[i] } else { ANSI_COLORS[i] }
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
    ansi: AnsiState,
}

static FB_STATE: Mutex<FbState> = Mutex::new(FbState {
    col: 0,
    row: 0,
    fg: DEFAULT_FG,
    bg: DEFAULT_BG,
    ansi: AnsiState::Normal,
});
static FB_CLEARED: AtomicBool = AtomicBool::new(false);

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
            }
            1..=29 => {}  // bold, italic, underline etc — ignore
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
            40..=47 => state.bg = ansi_color((params[i] - 40) as u8, false),
            48 => {
                if i + 2 < params.len() && params[i + 1] == 5 {
                    state.bg = color256(params[i + 2] as u8);
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
            match params[0] {
                2 | 3 => {
                    fb.clear(state.bg);
                    state.col = 0;
                    state.row = 0;
                }
                _ => {}
            }
        }
        b'K' => {
            match params[0] {
                0 => {
                    for c in state.col..cols {
                        let px = MARGIN_X + c * CHAR_W;
                        let py = MARGIN_Y + state.row * CHAR_H;
                        fb.draw_char(px, py, b' ', DEFAULT_FG, state.bg, 1);
                    }
                }
                1 => {
                    for c in 0..=state.col {
                        let px = MARGIN_X + c * CHAR_W;
                        let py = MARGIN_Y + state.row * CHAR_H;
                        fb.draw_char(px, py, b' ', DEFAULT_FG, state.bg, 1);
                    }
                }
                2 => {
                    for c in 0..cols {
                        let px = MARGIN_X + c * CHAR_W;
                        let py = MARGIN_Y + state.row * CHAR_H;
                        fb.draw_char(px, py, b' ', DEFAULT_FG, state.bg, 1);
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

// ── Driver struct (ZST — all state is global) ─────────────────────────────────

pub struct FramebufferConsole;

impl FramebufferConsole {
    pub fn new() -> Self {
        if !FB_CLEARED.swap(true, Ordering::SeqCst) {
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
        let mut state = FB_STATE.lock();
        let mut fb_guard = FRAMEBUFFER.lock();
        let Some(fb) = fb_guard.as_mut() else { return Ok(buf.len()); };

        let (w, h) = fb.dimensions();
        let cols = (w.saturating_sub(MARGIN_X)) / CHAR_W;
        let rows = (h.saturating_sub(MARGIN_Y)) / CHAR_H;

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
                                fb.scroll_up(CHAR_H);
                                state.row = rows - 1;
                            }
                        }
                        b'\r' => {
                            state.col = 0;
                        }
                        0x08 | 0x7f => {
                            if state.col > 0 {
                                state.col -= 1;
                                let px = MARGIN_X + state.col * CHAR_W;
                                let py = MARGIN_Y + state.row * CHAR_H;
                                fb.draw_char(px, py, b' ', state.fg, state.bg, 1);
                            }
                        }
                        b if b >= 0x20 && b < 0x7f => {
                            let px = MARGIN_X + state.col * CHAR_W;
                            let py = MARGIN_Y + state.row * CHAR_H;
                            fb.draw_char(px, py, b, state.fg, state.bg, 1);
                            state.col += 1;
                            if state.col >= cols {
                                state.col = 0;
                                state.row += 1;
                                if state.row >= rows {
                                    fb.scroll_up(CHAR_H);
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

pub fn open() -> Box<dyn FileHandle> {
    Box::new(FramebufferConsole::new())
}
