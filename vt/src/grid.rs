//! The screen: cells, cursor, scroll region, alternate screen and damage.
//!
//! Semantics follow xterm (and the VT100 before it) where the kernel
//! console is simpler, because full-screen programs depend on them:
//!
//! - **Deferred wrap.** Printing in the last column leaves the cursor
//!   there with a wrap *pending*; the next printable character wraps
//!   first. Without it, drawing the bottom-right cell scrolls the screen,
//!   which is what `vi` and `less` do on their status line.
//! - **Backspace moves, it does not erase** (the console erases). `ash`
//!   sends `BS` and then redraws the rest of the line itself.
//! - **Line feed does not return the carriage.** The pty's `ONLCR` turns
//!   `\n` into `\r\n` before it gets here.
//! - **Erasing uses the current background** (xterm's `bce`), as the
//!   console does.
//!
//! Every change marks the rows it touched; [`Grid::take_damage`] hands
//! them to the renderer, plus the rows the cursor left and entered.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::palette::{DEFAULT_BG, DEFAULT_FG};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Attrs {
    /// `SGR 1`: the bold weight of the font.
    pub bold: bool,
    /// `SGR 7`: foreground and background swapped when drawn.
    pub reverse: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: u32,
    pub bg: u32,
    pub attrs: Attrs,
}

impl Cell {
    fn blank(bg: u32) -> Self {
        Cell { ch: ' ', fg: DEFAULT_FG, bg, attrs: Attrs::default() }
    }
}

/// What `SGR` sets and printing uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pen {
    pub fg: u32,
    pub bg: u32,
    pub attrs: Attrs,
}

impl Pen {
    pub const DEFAULT: Pen = Pen { fg: DEFAULT_FG, bg: DEFAULT_BG, attrs: Attrs { bold: false, reverse: false } };
}

/// `DECSC`'s snapshot.
#[derive(Clone, Copy, Debug)]
struct Saved {
    row: usize,
    col: usize,
    pen: Pen,
    wrap_pending: bool,
}

/// The rows to redraw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Damage {
    rows: Vec<bool>,
}

impl Damage {
    pub fn is_empty(&self) -> bool {
        !self.rows.iter().any(|&d| d)
    }

    pub fn contains(&self, row: usize) -> bool {
        self.rows.get(row).copied().unwrap_or(false)
    }

    pub fn rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.rows.iter().enumerate().filter(|(_, &d)| d).map(|(i, _)| i)
    }

    /// First damaged row and one past the last, or `None`.
    pub fn span(&self) -> Option<(usize, usize)> {
        let first = self.rows.iter().position(|&d| d)?;
        let last = self.rows.iter().rposition(|&d| d)?;
        Some((first, last + 1))
    }

    /// Every row of a `rows`-row screen: the first frame.
    pub fn all(rows: usize) -> Self {
        Damage { rows: vec![true; rows] }
    }
}

pub struct Grid {
    cols: usize,
    rows: usize,
    cells: Vec<Cell>,
    /// The primary screen while the alternate one is shown.
    primary: Option<Vec<Cell>>,
    row: usize,
    col: usize,
    wrap_pending: bool,
    pub pen: Pen,
    /// Scroll region, rows `[top, bottom)`.
    top: usize,
    bottom: usize,
    saved: Saved,
    /// `?1049`'s own save, separate from `DECSC`'s.
    saved_1049: Saved,
    cursor_visible: bool,
    autowrap: bool,
    app_cursor: bool,
    dirty: Vec<bool>,
    /// Where the cursor was when damage was last taken, i.e. where the
    /// renderer last drew it.
    drawn_cursor: Option<usize>,
}

impl Grid {
    pub fn new(cols: usize, rows: usize) -> Self {
        assert!(cols > 0 && rows > 0, "a grid needs at least one cell");
        let home = Saved { row: 0, col: 0, pen: Pen::DEFAULT, wrap_pending: false };
        Grid {
            cols,
            rows,
            cells: vec![Cell::blank(DEFAULT_BG); cols * rows],
            primary: None,
            row: 0,
            col: 0,
            wrap_pending: false,
            pen: Pen::DEFAULT,
            top: 0,
            bottom: rows,
            saved: home,
            saved_1049: home,
            cursor_visible: true,
            autowrap: true,
            app_cursor: false,
            dirty: vec![true; rows],
            drawn_cursor: None,
        }
    }

    // ── Queries ──────────────────────────────────────────────────────────

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn cell(&self, row: usize, col: usize) -> &Cell {
        &self.cells[row * self.cols + col]
    }

    /// `(row, col)`, zero-based.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
    }

    /// `DECCKM`: arrows send `ESC O x` instead of `ESC [ x`.
    pub fn app_cursor(&self) -> bool {
        self.app_cursor
    }

    pub fn alt_screen(&self) -> bool {
        self.primary.is_some()
    }

    pub fn scroll_region(&self) -> (usize, usize) {
        (self.top, self.bottom)
    }

    /// A row's characters, trailing blanks removed.
    pub fn row_text(&self, row: usize) -> String {
        let cells = &self.cells[row * self.cols..(row + 1) * self.cols];
        let s: String = cells.iter().map(|c| c.ch).collect();
        String::from(s.trim_end())
    }

    /// The rows changed since the last call, plus the rows the cursor
    /// left and is on now.
    pub fn take_damage(&mut self) -> Damage {
        if let Some(r) = self.drawn_cursor {
            self.dirty[r] = true;
        }
        self.dirty[self.row] = true;
        self.drawn_cursor = Some(self.row);
        let rows = core::mem::replace(&mut self.dirty, vec![false; self.rows]);
        Damage { rows }
    }

    // ── Printing and cursor motion ───────────────────────────────────────

    pub fn print(&mut self, ch: char) {
        if self.wrap_pending {
            self.wrap_pending = false;
            self.col = 0;
            self.index();
        }
        let pen = self.pen;
        let i = self.row * self.cols + self.col;
        self.cells[i] = Cell { ch, fg: pen.fg, bg: pen.bg, attrs: pen.attrs };
        self.dirty[self.row] = true;
        if self.col + 1 < self.cols {
            self.col += 1;
        } else if self.autowrap {
            self.wrap_pending = true;
        }
    }

    /// `LF`/`VT`/`FF`/`IND`: down one row, scrolling at the region's
    /// bottom. Below the region it stops at the screen's last row.
    pub fn index(&mut self) {
        self.wrap_pending = false;
        if self.row + 1 == self.bottom {
            self.scroll_up(1);
        } else if self.row + 1 < self.rows {
            self.row += 1;
        }
    }

    /// `RI`: up one row, scrolling down at the region's top.
    pub fn reverse_index(&mut self) {
        self.wrap_pending = false;
        if self.row == self.top {
            self.scroll_down(1);
        } else if self.row > 0 {
            self.row -= 1;
        }
    }

    pub fn carriage_return(&mut self) {
        self.wrap_pending = false;
        self.col = 0;
    }

    pub fn backspace(&mut self) {
        self.wrap_pending = false;
        self.col = self.col.saturating_sub(1);
    }

    /// `HT`: next multiple of 8, never past the last column.
    pub fn tab(&mut self) {
        self.wrap_pending = false;
        self.col = ((self.col / 8 + 1) * 8).min(self.cols - 1);
    }

    /// Absolute position, zero-based, clamped to the screen.
    pub fn move_to(&mut self, row: usize, col: usize) {
        self.wrap_pending = false;
        self.row = row.min(self.rows - 1);
        self.col = col.min(self.cols - 1);
    }

    /// `CUU`: stops at the region's top if the cursor started inside it.
    pub fn up(&mut self, n: usize) {
        let floor = if self.row >= self.top { self.top } else { 0 };
        let row = self.row.saturating_sub(n).max(floor);
        self.move_to(row, self.col);
    }

    /// `CUD`: stops at the region's bottom if the cursor started inside it.
    pub fn down(&mut self, n: usize) {
        let ceil = if self.row < self.bottom { self.bottom - 1 } else { self.rows - 1 };
        let row = self.row.saturating_add(n).min(ceil);
        self.move_to(row, self.col);
    }

    pub fn right(&mut self, n: usize) {
        self.move_to(self.row, self.col.saturating_add(n));
    }

    pub fn left(&mut self, n: usize) {
        self.move_to(self.row, self.col.saturating_sub(n));
    }

    // ── Scrolling ────────────────────────────────────────────────────────

    /// Rows `[top, bottom)` move up `n`; blanks enter at the bottom.
    fn scroll_region_up(&mut self, top: usize, bottom: usize, n: usize) {
        let n = n.min(bottom - top);
        let c = self.cols;
        self.cells.copy_within((top + n) * c..bottom * c, top * c);
        self.blank_range((bottom - n) * c, bottom * c);
        self.mark(top, bottom);
    }

    /// Rows `[top, bottom)` move down `n`; blanks enter at the top.
    fn scroll_region_down(&mut self, top: usize, bottom: usize, n: usize) {
        let n = n.min(bottom - top);
        let c = self.cols;
        self.cells.copy_within(top * c..(bottom - n) * c, (top + n) * c);
        self.blank_range(top * c, (top + n) * c);
        self.mark(top, bottom);
    }

    /// `SU`, and a line feed at the region's bottom.
    pub fn scroll_up(&mut self, n: usize) {
        self.scroll_region_up(self.top, self.bottom, n);
    }

    /// `SD`, and a reverse index at the region's top.
    pub fn scroll_down(&mut self, n: usize) {
        self.scroll_region_down(self.top, self.bottom, n);
    }

    /// `IL`: only inside the scroll region; the cursor goes to column 0.
    pub fn insert_lines(&mut self, n: usize) {
        if self.row < self.top || self.row >= self.bottom {
            return;
        }
        self.scroll_region_down(self.row, self.bottom, n);
        self.carriage_return();
    }

    /// `DL`: only inside the scroll region; the cursor goes to column 0.
    pub fn delete_lines(&mut self, n: usize) {
        if self.row < self.top || self.row >= self.bottom {
            return;
        }
        self.scroll_region_up(self.row, self.bottom, n);
        self.carriage_return();
    }

    /// `DECSTBM`, zero-based `[top, bottom)`. An empty or one-row region is
    /// refused, as xterm does; a valid one homes the cursor.
    pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let bottom = bottom.min(self.rows);
        if top + 1 >= bottom {
            return;
        }
        self.top = top;
        self.bottom = bottom;
        self.move_to(0, 0);
    }

    // ── Editing within a row ─────────────────────────────────────────────

    /// `ICH`: blanks at the cursor, the rest of the row shifts right.
    pub fn insert_chars(&mut self, n: usize) {
        self.wrap_pending = false;
        let start = self.row * self.cols + self.col;
        let end = (self.row + 1) * self.cols;
        let n = n.min(end - start);
        self.cells.copy_within(start..end - n, start + n);
        self.blank_range(start, start + n);
        self.dirty[self.row] = true;
    }

    /// `DCH`: the rest of the row shifts left; blanks enter at the end.
    pub fn delete_chars(&mut self, n: usize) {
        self.wrap_pending = false;
        let start = self.row * self.cols + self.col;
        let end = (self.row + 1) * self.cols;
        let n = n.min(end - start);
        self.cells.copy_within(start + n..end, start);
        self.blank_range(end - n, end);
        self.dirty[self.row] = true;
    }

    /// `ECH`: blanks from the cursor, nothing moves.
    pub fn erase_chars(&mut self, n: usize) {
        self.wrap_pending = false;
        let start = self.row * self.cols + self.col;
        let end = start + n.min(self.cols - self.col);
        self.blank_range(start, end);
        self.dirty[self.row] = true;
    }

    /// `EL`: 0 cursor to end, 1 start to cursor, 2 whole row.
    pub fn erase_line(&mut self, mode: u32) {
        self.wrap_pending = false;
        let base = self.row * self.cols;
        let (a, b) = match mode {
            0 => (self.col, self.cols),
            1 => (0, self.col + 1),
            2 => (0, self.cols),
            _ => return,
        };
        self.blank_range(base + a, base + b);
        self.dirty[self.row] = true;
    }

    /// `ED`: 0 cursor to end, 1 start to cursor, 2 (and 3, which on xterm
    /// also clears the scrollback this terminal does not have) everything.
    /// The cursor does not move.
    pub fn erase_display(&mut self, mode: u32) {
        self.wrap_pending = false;
        let here = self.row * self.cols + self.col;
        match mode {
            0 => {
                self.blank_range(here, self.cells.len());
                self.mark(self.row, self.rows);
            }
            1 => {
                self.blank_range(0, here + 1);
                self.mark(0, self.row + 1);
            }
            2 | 3 => {
                self.blank_range(0, self.cells.len());
                self.mark(0, self.rows);
            }
            _ => {}
        }
    }

    // ── Modes, saving, screens ───────────────────────────────────────────

    pub fn set_cursor_visible(&mut self, on: bool) {
        self.cursor_visible = on;
        self.dirty[self.row] = true;
    }

    pub fn set_autowrap(&mut self, on: bool) {
        self.autowrap = on;
        if !on {
            self.wrap_pending = false;
        }
    }

    pub fn set_app_cursor(&mut self, on: bool) {
        self.app_cursor = on;
    }

    fn snapshot(&self) -> Saved {
        Saved { row: self.row, col: self.col, pen: self.pen, wrap_pending: self.wrap_pending }
    }

    fn restore(&mut self, s: Saved) {
        self.move_to(s.row, s.col);
        self.pen = s.pen;
        self.wrap_pending = s.wrap_pending && self.col == self.cols - 1;
    }

    /// `DECSC` (`ESC 7`, `CSI s`, `?1048h`).
    pub fn save_cursor(&mut self) {
        self.saved = self.snapshot();
    }

    /// `DECRC`.
    pub fn restore_cursor(&mut self) {
        self.restore(self.saved);
    }

    /// `?47h`/`?1047h`/`?1049h`. With `save` (1049) the cursor is saved
    /// first. The alternate screen is not kept between uses, so it always
    /// starts clear; 1049 also clears it when already on it.
    pub fn enter_alt_screen(&mut self, save: bool) {
        if save {
            self.saved_1049 = self.snapshot();
        }
        if self.primary.is_none() {
            let alt = vec![Cell::blank(DEFAULT_BG); self.cells.len()];
            self.primary = Some(core::mem::replace(&mut self.cells, alt));
        }
        if save {
            self.blank_range(0, self.cells.len());
        }
        self.mark(0, self.rows);
    }

    /// `?47l`/`?1047l`/`?1049l`: back to the primary screen as it was.
    pub fn leave_alt_screen(&mut self, restore: bool) {
        if let Some(primary) = self.primary.take() {
            self.cells = primary;
            self.mark(0, self.rows);
        }
        if restore {
            self.restore(self.saved_1049);
        }
    }

    /// `RIS` (`ESC c`): as new, same size.
    pub fn reset(&mut self) {
        *self = Grid::new(self.cols, self.rows);
    }

    // ── Helpers ──────────────────────────────────────────────────────────

    /// Blank cells `[a, b)` in the current background.
    fn blank_range(&mut self, a: usize, b: usize) {
        let blank = Cell::blank(self.pen.bg);
        self.cells[a..b].fill(blank);
    }

    fn mark(&mut self, a: usize, b: usize) {
        self.dirty[a..b].fill(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_str(g: &mut Grid, s: &str) {
        for c in s.chars() {
            g.print(c);
        }
    }

    #[test]
    fn last_column_defers_the_wrap() {
        let mut g = Grid::new(4, 2);
        type_str(&mut g, "abcd");
        assert_eq!(g.cursor(), (0, 3), "stays on the last column");
        type_str(&mut g, "e");
        assert_eq!(g.row_text(0), "abcd");
        assert_eq!(g.row_text(1), "e");
        assert_eq!(g.cursor(), (1, 1));
    }

    #[test]
    fn bottom_right_cell_does_not_scroll_until_the_next_character() {
        let mut g = Grid::new(3, 2);
        g.move_to(1, 0);
        type_str(&mut g, "xyz");
        assert_eq!(g.row_text(1), "xyz", "nothing scrolled");
        g.print('!');
        assert_eq!(g.row_text(0), "xyz");
        assert_eq!(g.row_text(1), "!");
    }

    #[test]
    fn a_carriage_return_cancels_the_pending_wrap() {
        let mut g = Grid::new(3, 2);
        type_str(&mut g, "abc");
        g.carriage_return();
        g.print('X');
        assert_eq!(g.row_text(0), "Xbc");
        assert_eq!(g.row_text(1), "");
    }

    #[test]
    fn without_autowrap_the_last_column_is_overwritten() {
        let mut g = Grid::new(3, 2);
        g.set_autowrap(false);
        type_str(&mut g, "abcdef");
        assert_eq!(g.row_text(0), "abf");
        assert_eq!(g.cursor(), (0, 2));
    }

    #[test]
    fn index_scrolls_only_the_region() {
        let mut g = Grid::new(2, 4);
        for (r, s) in ["a", "b", "c", "d"].iter().enumerate() {
            g.move_to(r, 0);
            type_str(&mut g, s);
        }
        g.set_scroll_region(1, 3);
        g.move_to(2, 0);
        g.index();
        let rows: Vec<String> = (0..4).map(|r| g.row_text(r)).collect();
        assert_eq!(rows, ["a", "c", "", "d"]);
    }

    #[test]
    fn reverse_index_at_the_top_scrolls_down() {
        let mut g = Grid::new(2, 3);
        type_str(&mut g, "a");
        g.move_to(0, 0);
        g.reverse_index();
        assert_eq!(g.row_text(0), "");
        assert_eq!(g.row_text(1), "a");
    }

    #[test]
    fn insert_and_delete_lines_stay_inside_the_region() {
        let mut g = Grid::new(2, 5);
        for r in 0..5 {
            g.move_to(r, 0);
            g.print((b'0' + r as u8) as char);
        }
        g.set_scroll_region(1, 4);
        g.move_to(2, 1);
        g.insert_lines(1);
        let rows: Vec<String> = (0..5).map(|r| g.row_text(r)).collect();
        assert_eq!(rows, ["0", "1", "", "2", "4"]);
        assert_eq!(g.cursor(), (2, 0));
        g.delete_lines(2);
        let rows: Vec<String> = (0..5).map(|r| g.row_text(r)).collect();
        assert_eq!(rows, ["0", "1", "", "", "4"]);
        // Outside the region: nothing.
        g.move_to(4, 0);
        g.delete_lines(1);
        assert_eq!(g.row_text(4), "4");
    }

    #[test]
    fn insert_delete_erase_chars() {
        let mut g = Grid::new(6, 1);
        type_str(&mut g, "abcdef");
        g.move_to(0, 1);
        g.insert_chars(2);
        assert_eq!(g.row_text(0), "a  bcd");
        g.delete_chars(3);
        assert_eq!(g.row_text(0), "acd");
        g.erase_chars(1);
        assert_eq!(g.row_text(0), "a d");
        g.insert_chars(100);
        assert_eq!(g.row_text(0), "a");
    }

    #[test]
    fn erase_uses_the_current_background() {
        let mut g = Grid::new(3, 2);
        g.pen.bg = 0x123456;
        g.erase_display(2);
        assert!((0..2).all(|r| (0..3).all(|c| g.cell(r, c).bg == 0x123456)));
    }

    #[test]
    fn erase_display_modes() {
        let mut g = Grid::new(3, 3);
        for r in 0..3 {
            g.move_to(r, 0);
            type_str(&mut g, "xyz");
        }
        g.move_to(1, 1);
        g.erase_display(0);
        assert_eq!([g.row_text(0), g.row_text(1), g.row_text(2)], ["xyz", "x", ""]);
        g.move_to(1, 0);
        g.erase_display(1);
        assert_eq!([g.row_text(0), g.row_text(1)], ["", ""]);
        assert_eq!(g.cursor(), (1, 0), "ED does not move the cursor");
    }

    #[test]
    fn alt_screen_keeps_the_primary_and_the_cursor() {
        let mut g = Grid::new(4, 2);
        type_str(&mut g, "sh$");
        g.enter_alt_screen(true);
        assert!(g.alt_screen());
        assert_eq!(g.row_text(0), "", "the alternate screen starts clear");
        g.move_to(1, 0);
        type_str(&mut g, "vi");
        g.leave_alt_screen(true);
        assert!(!g.alt_screen());
        assert_eq!(g.row_text(0), "sh$");
        assert_eq!(g.row_text(1), "");
        assert_eq!(g.cursor(), (0, 3));
    }

    #[test]
    fn save_and_restore_carry_the_pen() {
        let mut g = Grid::new(5, 5);
        g.move_to(2, 3);
        g.pen.attrs.bold = true;
        g.save_cursor();
        g.move_to(0, 0);
        g.pen = Pen::DEFAULT;
        g.restore_cursor();
        assert_eq!(g.cursor(), (2, 3));
        assert!(g.pen.attrs.bold);
    }

    #[test]
    fn cursor_motion_respects_the_region_it_started_in() {
        let mut g = Grid::new(5, 10);
        g.set_scroll_region(2, 6);
        g.move_to(4, 0);
        g.up(10);
        assert_eq!(g.cursor().0, 2);
        g.down(10);
        assert_eq!(g.cursor().0, 5);
        g.move_to(0, 0);
        g.up(1);
        assert_eq!(g.cursor().0, 0);
        g.move_to(8, 0);
        g.down(10);
        assert_eq!(g.cursor().0, 9);
    }

    #[test]
    fn a_degenerate_region_is_refused() {
        let mut g = Grid::new(5, 10);
        g.move_to(3, 3);
        g.set_scroll_region(4, 5);
        assert_eq!(g.scroll_region(), (0, 10));
        assert_eq!(g.cursor(), (3, 3));
    }

    #[test]
    fn damage_names_the_touched_rows_and_the_cursor_rows() {
        let mut g = Grid::new(4, 6);
        assert_eq!(g.take_damage().span(), Some((0, 6)), "a new grid is all damage");
        assert_eq!(g.take_damage().rows().collect::<Vec<_>>(), [0], "only the cursor's row");
        g.move_to(3, 0);
        g.print('x');
        let d = g.take_damage();
        assert_eq!(d.rows().collect::<Vec<_>>(), [0, 3], "the row it left and the one it wrote");
        g.move_to(5, 0);
        assert_eq!(g.take_damage().rows().collect::<Vec<_>>(), [3, 5]);
    }

    #[test]
    fn tab_stops_every_eight_and_at_the_edge() {
        let mut g = Grid::new(20, 1);
        g.tab();
        assert_eq!(g.cursor(), (0, 8));
        g.tab();
        g.tab();
        assert_eq!(g.cursor(), (0, 19));
    }
}
