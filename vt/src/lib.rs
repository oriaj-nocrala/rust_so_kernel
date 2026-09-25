//! `vt` — host-testable terminal emulator for the windowed terminal.
//!
//! Phase 3.4 of `docs/gui/gui-plan.md`, in the line of `gui` and `tty`:
//! logic that speaks in plain types lives where a plain `cargo test`
//! reaches it. `no_std` + `alloc`, no syscalls — the terminal client
//! (`term`, phase 3.5) owns the pty, the surface and the event loop.
//!
//! - [`grid`]: the cells, the cursor, the scroll region, the alternate
//!   screen and the per-row damage.
//! - [`parser`]: bytes from the pty master → operations on a [`Grid`].
//!   Answers to queries (`DSR`, `DA`) come back as data
//!   ([`Parser::take_replies`]) for the caller to write to the master.
//! - [`render`]: a grid's damaged rows into a `&mut [u32]` of `0x00RRGGBB`
//!   pixels, with the same Noto Sans Mono rasters as the kernel console.
//! - [`keymap`]: a Linux `KEY_*` code and its press/release → the bytes a
//!   terminal sends.
//! - [`palette`]: the kernel console's colours.
//!
//! **The kernel console is not built on this** (decision recorded in the
//! plan): its speed is measured on the Ryzen and a grid would mean
//! measuring it all again. The palette and the SGR rules are copied from
//! it, so both look the same.

#![no_std]

extern crate alloc;

pub mod grid;
pub mod keymap;
pub mod palette;
pub mod parser;
pub mod render;

pub use grid::{Attrs, Cell, Damage, Grid};
pub use keymap::{KeyBytes, Keyboard};
pub use parser::Parser;
pub use render::{render, Font, PixelRect};

/// A grid and the parser that feeds it: what a terminal window holds.
pub struct Terminal {
    pub grid: Grid,
    pub parser: Parser,
}

impl Terminal {
    pub fn new(cols: usize, rows: usize) -> Self {
        Terminal { grid: Grid::new(cols, rows), parser: Parser::new() }
    }

    /// Bytes read from the pty master.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.feed(&mut self.grid, bytes);
    }
}
