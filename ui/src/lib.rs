//! Widgets for a program's window: the app describes its window each frame as a [`Widget`] tree, [`State`] keeps what must last between
//! frames (focus, selections, scroll positions, text being typed, split positions), turns input into [`Action`]s ([`State::handle`]) and
//! renders the tree ([`State::render`]) as [`Paint`] operations plus the window's semantic tree (`gui::semantic`: what a test, an agent or
//! a screen reader reads, principle P7).
//!
//! - Looks come from the compositor's theme (`gui::theme::Widgets`), so a window matches its frame. Lengths are logical pixels × the
//!   look's scale.
//! - Text is measured through [`Measure`]; the crate has no fonts (the `render` feature's painter has).
//! - A [`List`] is virtualized: it asks for the rows it shows and no others (`row` is called only for them, and only by `render`), and
//!   the semantic tree carries those rows with their position in the whole list.
//! - Ids: the app gives each widget a non-zero id below [`DERIVED`], unique in the window and the same every frame. Rows and column
//!   headers get ids derived from their list and their key, so a row keeps its id when others are added above it. [`ROOT`] is the
//!   window's node.
//!
//! Keyboard: Tab / Shift+Tab move the focus (buttons, fields, lists, in the order the tree has them); Space or Enter press a button; a
//! field edits (arrows, Home/End, Backspace/Delete; Enter submits); a list moves its selection (arrows, Page Up/Down, Home/End), Enter
//! activates the selected row, and typing selects the first row whose first column starts with what was typed (type-to-find, reset
//! after a second). A key nobody used comes back as [`Action::Key`]. Pointer: click, double-click a row, drag a split's handle, the
//! wheel scrolls a list or a scroll area.

#![no_std]

extern crate alloc;

#[cfg(feature = "render")]
pub mod render;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

pub use gui::region::Rect;
pub use gui::semantic::{self, action, flag, Node, Role};
use gui::theme::{self, Theme, Widgets};
use vt::keymap::{code, Keyboard};

pub type Id = u32;
/// The window's own node.
pub const ROOT: Id = 1;
/// App ids are below this; derived ids (rows, headers) at or above.
pub const DERIVED: Id = 0x8000_0000;

pub const BTN_LEFT: u32 = 0x110;
/// Two clicks on the same row closer than this are a double click.
pub const DOUBLE_CLICK_MS: u32 = 400;
/// Type-to-find forgets what was typed after this long without a key.
pub const FIND_RESET_MS: u32 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Font {
    Sans,
    SansBold,
    Mono,
}

/// Text metrics, from the program's fonts.
pub trait Measure {
    /// Width of `s` on one line, in pixels.
    fn width(&mut self, s: &str, font: Font) -> i32;
    /// Height of a line, in pixels (the same for every font here).
    fn line_height(&mut self) -> i32;
}

/// How much of its container's length a child takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    /// Logical pixels (× scale).
    Fixed(i32),
    /// What the child asks for.
    Auto,
    /// A share of what is left.
    Fill,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Column<'a> {
    pub title: &'a str,
    pub width: Size,
    /// Right-aligned (sizes).
    pub right: bool,
}

/// One row of a list: `key` identifies it across frames (a file's name hashed, say), `cells` one string per column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub key: u64,
    pub cells: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListStyle {
    /// Columns with headers, the field's background.
    Table,
    /// One column, the sidebar's background, taller rows (places).
    Sidebar,
}

pub struct List<'a> {
    pub id: Id,
    /// Its accessible name ("Files", "Places").
    pub name: &'a str,
    pub len: usize,
    pub columns: &'a [Column<'a>],
    pub style: ListStyle,
    /// Row `i` (`< len`); called only for the rows shown.
    pub row: &'a dyn Fn(usize) -> Row,
}

pub enum Widget<'a> {
    Label { id: Id, text: &'a str, font: Font },
    Button { id: Id, label: &'a str },
    /// A single-line text field; its text lives in the state ([`State::text`]). `label` is its accessible name.
    Field { id: Id, label: &'a str, placeholder: &'a str },
    List(List<'a>),
    /// `0x00RRGGBB` pixels, `w x h`, drawn at the top left of its box.
    Image { id: Id, name: &'a str, pixels: &'a [u32], w: usize, h: usize },
    /// Children top to bottom.
    Column(Vec<(Size, Widget<'a>)>),
    /// Children left to right.
    Row(Vec<(Size, Widget<'a>)>),
    /// Two children side by side with a handle between them the user drags; `at` is the first one's starting width and `min` the
    /// least either may get (logical pixels).
    Split { id: Id, first: Box<Widget<'a>>, second: Box<Widget<'a>>, at: i32, min: i32 },
    /// Its child at the height it asks for, scrolled by the wheel.
    Scroll { id: Id, child: Box<Widget<'a>> },
    /// A named group (an inspector, a toolbar): a node in the semantic tree, nothing on the screen.
    Pane { id: Id, name: &'a str, role: Role, child: Box<Widget<'a>> },
    /// Nothing; takes room.
    Space,
}

impl<'a> Widget<'a> {
    pub fn label(id: Id, text: &'a str) -> Self {
        Widget::Label { id, text, font: Font::Sans }
    }
    pub fn button(id: Id, label: &'a str) -> Self {
        Widget::Button { id, label }
    }
    pub fn field(id: Id, label: &'a str, placeholder: &'a str) -> Self {
        Widget::Field { id, label, placeholder }
    }
    pub fn split(id: Id, first: Widget<'a>, second: Widget<'a>, at: i32, min: i32) -> Self {
        Widget::Split { id, first: Box::new(first), second: Box::new(second), at, min }
    }
    pub fn scroll(id: Id, child: Widget<'a>) -> Self {
        Widget::Scroll { id, child: Box::new(child) }
    }
    pub fn pane(id: Id, name: &'a str, child: Widget<'a>) -> Self {
        Widget::Pane { id, name, role: Role::Pane, child: Box::new(child) }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    /// A Linux `KEY_*`.
    Key { code: u32, pressed: bool },
    /// The pointer, in the window's pixels.
    Motion { x: i32, y: i32 },
    /// A Linux `BTN_*`.
    Button { code: u32, pressed: bool },
    /// Wheel notches, positive towards the user (scrolls down).
    Wheel { dy: i32 },
    /// The window got or lost the keyboard.
    Focus(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Clicked(Id),
    /// The selection moved to `row`.
    Selected { list: Id, row: usize },
    /// Enter or a double click on `row`.
    Activated { list: Id, row: usize },
    /// A field's text changed.
    Changed(Id),
    /// Enter in a field.
    Submitted(Id),
    /// A key press no widget used (Escape, Backspace in a list, Space…), with the widget that had the focus.
    Key { code: u32, focus: Option<Id> },
}

/// What to draw, in order. Every operation is clipped to the last `Clip` (the whole window before the first).
#[derive(Clone, Debug, PartialEq)]
pub enum Paint<'a> {
    Clip(Rect),
    Fill { rect: Rect, color: u32 },
    /// A theme's button or column header (`theme::Button::paint`).
    Button { rect: Rect, look: &'static theme::Button, down: bool },
    /// One line with its box's top left at (`x`, `y`).
    Text { x: i32, y: i32, text: String, font: Font, color: u32 },
    Image { rect: Rect, pixels: &'a [u32], w: usize, h: usize },
}

/// One rendered frame.
pub struct Frame<'a> {
    pub paint: Vec<Paint<'a>>,
    /// Parents before children; `nodes[0]` is the window ([`ROOT`]).
    pub nodes: Vec<Node>,
}

#[derive(Clone, Debug, Default)]
struct ListState {
    selected: Option<usize>,
    /// Pixels scrolled from the first row.
    scroll: i32,
    find: String,
    find_ms: u32,
    /// `find` changed and is not searched yet.
    finding: bool,
}

#[derive(Clone, Debug, Default)]
struct FieldState {
    text: String,
    /// A byte index at a char boundary.
    cursor: usize,
}

/// Where a widget ended up, for input.
#[derive(Clone, Debug)]
struct Placed {
    id: Id,
    rect: Rect,
    /// The visible part.
    clip: Rect,
    kind: Kind,
}

#[derive(Clone, Debug)]
enum Kind {
    Button,
    Field,
    List { len: usize, row_h: i32, head_h: i32, first_cell: bool },
    /// The handle; the split's whole box is `rect`.
    Split { handle: Rect, min: i32 },
    Scroll { content_h: i32 },
    Other,
}

impl Kind {
    fn focusable(&self) -> bool {
        matches!(self, Kind::Button | Kind::Field | Kind::List { .. })
    }
}

pub struct State {
    w: &'static Widgets,
    scale: i32,
    focus: Option<Id>,
    window_focused: bool,
    kb: Keyboard,
    pointer: (i32, i32),
    /// A button held down by the pointer.
    pressed: Option<Id>,
    /// A split being dragged: its id and the pointer's offset in the handle.
    drag: Option<(Id, i32)>,
    last_click: Option<(Id, usize, u32)>,
    lists: BTreeMap<Id, ListState>,
    fields: BTreeMap<Id, FieldState>,
    splits: BTreeMap<Id, i32>,
    scrolls: BTreeMap<Id, i32>,
}

impl State {
    /// Widgets in `theme`'s look at `scale` (the compositor's HIDPI factor: 1, 2…).
    pub fn new(theme: &'static Theme, scale: i32) -> State {
        State {
            w: &theme.widgets,
            scale: scale.max(1),
            focus: None,
            window_focused: true,
            kb: Keyboard::new(),
            pointer: (-1, -1),
            pressed: None,
            drag: None,
            last_click: None,
            lists: BTreeMap::new(),
            fields: BTreeMap::new(),
            splits: BTreeMap::new(),
            scrolls: BTreeMap::new(),
        }
    }

    pub fn set_theme(&mut self, theme: &'static Theme) {
        self.w = &theme.widgets;
    }

    pub fn focus(&self) -> Option<Id> {
        self.focus
    }

    pub fn set_focus(&mut self, id: Option<Id>) {
        self.focus = id;
    }

    pub fn selected(&self, list: Id) -> Option<usize> {
        self.lists.get(&list).and_then(|l| l.selected)
    }

    /// Selects `row` (or nothing) without an [`Action`]; the next render scrolls it into view.
    pub fn select(&mut self, list: Id, row: Option<usize>) {
        let l = self.lists.entry(list).or_default();
        l.selected = row;
        l.find.clear();
        if row.is_none() {
            l.scroll = 0;
        }
    }

    pub fn text(&self, field: Id) -> &str {
        self.fields.get(&field).map_or("", |f| f.text.as_str())
    }

    /// Replaces a field's text, the cursor at its end.
    pub fn set_text(&mut self, field: Id, s: &str) {
        let f = self.fields.entry(field).or_default();
        f.text = s.into();
        f.cursor = f.text.len();
    }

    fn px(&self, logical: i32) -> i32 {
        logical * self.scale
    }

    /// One input event against the window as `root` describes it at `size`.
    pub fn handle(&mut self, root: &Widget, size: (i32, i32), input: Input, now_ms: u32, m: &mut dyn Measure) -> Vec<Action> {
        let mut out = Vec::new();
        let placed = {
            let mut cx = Cx::new(self, m, false, "");
            cx.walk(root, Rect::new(0, 0, size.0, size.1), Rect::new(0, 0, size.0, size.1), ROOT);
            cx.placed
        };
        match input {
            Input::Focus(f) => {
                self.window_focused = f;
                if !f {
                    self.kb.release_all();
                    self.pressed = None;
                    self.drag = None;
                }
            }
            Input::Key { code: c, pressed } => {
                let bytes = self.kb.key(c, pressed, false);
                if pressed && !is_modifier(c) {
                    let ch = bytes.as_bytes().first().copied().filter(|&b| bytes.as_bytes().len() == 1 && (0x20..0x7f).contains(&b));
                    let ch = ch.filter(|_| !self.kb.ctrl() && !self.kb.alt());
                    self.key(c, ch.map(|b| b as char), &placed, now_ms, &mut out);
                    self.find(root, &mut out);
                }
            }
            Input::Motion { x, y } => {
                self.pointer = (x, y);
                if let Some((id, off)) = self.drag {
                    if let Some(p) = placed.iter().find(|p| p.id == id) {
                        if let Kind::Split { handle, min } = p.kind {
                            let max = (p.rect.w - handle.w - min).max(min);
                            self.splits.insert(id, (x - off - p.rect.x).clamp(min, max));
                        }
                    }
                }
            }
            Input::Button { code: BTN_LEFT, pressed: true } => self.press(&placed, now_ms, m, &mut out),
            Input::Button { code: BTN_LEFT, pressed: false } => {
                if let Some(id) = self.pressed.take() {
                    if placed.iter().any(|p| p.id == id && hit(p, self.pointer)) {
                        out.push(Action::Clicked(id));
                    }
                }
                self.drag = None;
            }
            Input::Button { .. } => {}
            Input::Wheel { dy } => {
                let (x, y) = self.pointer;
                let lh = m.line_height();
                for p in placed.iter().rev() {
                    if !hit(p, (x, y)) {
                        continue;
                    }
                    match p.kind {
                        Kind::List { len, row_h, head_h, .. } => {
                            let l = self.lists.entry(p.id).or_default();
                            let max = (len as i32 * row_h - (p.rect.h - head_h)).max(0);
                            l.scroll = (l.scroll + dy * 3 * row_h).clamp(0, max);
                            break;
                        }
                        Kind::Scroll { content_h } => {
                            let s = self.scrolls.entry(p.id).or_default();
                            *s = (*s + dy * 3 * lh).clamp(0, (content_h - p.rect.h).max(0));
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }
        out
    }

    fn press(&mut self, placed: &[Placed], now_ms: u32, m: &mut dyn Measure, out: &mut Vec<Action>) {
        let pt = self.pointer;
        // the topmost (last placed) widget under the pointer that takes clicks
        let Some(p) = placed.iter().rev().find(|p| hit(p, pt) && !matches!(p.kind, Kind::Other | Kind::Scroll { .. })) else { return };
        match p.kind {
            Kind::Split { handle, .. } => {
                if handle.contains(pt.0, pt.1) {
                    self.drag = Some((p.id, pt.0 - handle.x));
                }
            }
            Kind::Button => {
                self.pressed = Some(p.id);
                self.focus = Some(p.id);
            }
            Kind::Field => {
                self.focus = Some(p.id);
                let pad = self.px(4);
                let f = self.fields.entry(p.id).or_default();
                // the char boundary nearest the click
                let want = pt.0 - (p.rect.x + pad);
                let mut best = (i32::MAX, 0);
                for (i, _) in f.text.char_indices().chain(core::iter::once((f.text.len(), ' '))) {
                    let d = (m.width(&f.text[..i], Font::Sans) - want).abs();
                    if d < best.0 {
                        best = (d, i);
                    }
                }
                f.cursor = best.1;
            }
            Kind::List { len, row_h, head_h, .. } => {
                self.focus = Some(p.id);
                let l = self.lists.entry(p.id).or_default();
                let y = pt.1 - p.rect.y - head_h;
                if y < 0 {
                    return;
                }
                let row = ((y + l.scroll) / row_h) as usize;
                if row >= len {
                    return;
                }
                l.find.clear();
                if l.selected != Some(row) {
                    l.selected = Some(row);
                    out.push(Action::Selected { list: p.id, row });
                }
                match self.last_click {
                    Some((id, r, t)) if id == p.id && r == row && now_ms.wrapping_sub(t) <= DOUBLE_CLICK_MS => {
                        out.push(Action::Activated { list: p.id, row });
                        self.last_click = None;
                    }
                    _ => self.last_click = Some((p.id, row, now_ms)),
                }
            }
            Kind::Scroll { .. } | Kind::Other => {}
        }
    }

    fn key(&mut self, c: u32, ch: Option<char>, placed: &[Placed], now_ms: u32, out: &mut Vec<Action>) {
        if c == code::TAB {
            let order: Vec<Id> = placed.iter().filter(|p| p.kind.focusable()).map(|p| p.id).collect();
            if order.is_empty() {
                return;
            }
            let at = self.focus.and_then(|f| order.iter().position(|&i| i == f));
            let n = order.len();
            let next = match (at, self.kb.shift()) {
                (None, false) => 0,
                (None, true) => n - 1,
                (Some(i), false) => (i + 1) % n,
                (Some(i), true) => (i + n - 1) % n,
            };
            self.focus = Some(order[next]);
            return;
        }
        let focused = self.focus.and_then(|f| placed.iter().find(|p| p.id == f));
        let used = match focused.map(|p| (p.id, &p.kind, p.rect)) {
            Some((id, Kind::Button, _)) if c == code::SPACE || c == code::ENTER || c == code::KPENTER => {
                out.push(Action::Clicked(id));
                true
            }
            Some((id, Kind::Field, _)) => self.field_key(id, c, ch, out),
            Some((id, &Kind::List { len, row_h, head_h, first_cell }, rect)) => {
                self.list_key(id, len, row_h, rect.h - head_h, first_cell, c, ch, now_ms, out)
            }
            _ => false,
        };
        if !used {
            out.push(Action::Key { code: c, focus: self.focus });
        }
    }

    fn field_key(&mut self, id: Id, c: u32, ch: Option<char>, out: &mut Vec<Action>) -> bool {
        let f = self.fields.entry(id).or_default();
        let prev = |s: &str, i: usize| s[..i].char_indices().next_back().map_or(0, |(j, _)| j);
        let next = |s: &str, i: usize| s[i..].chars().next().map_or(i, |ch| i + ch.len_utf8());
        match c {
            code::LEFT => f.cursor = prev(&f.text, f.cursor),
            code::RIGHT => f.cursor = next(&f.text, f.cursor),
            code::HOME => f.cursor = 0,
            code::END => f.cursor = f.text.len(),
            code::BACKSPACE => {
                if f.cursor == 0 {
                    return true;
                }
                let p = prev(&f.text, f.cursor);
                f.text.replace_range(p..f.cursor, "");
                f.cursor = p;
                out.push(Action::Changed(id));
            }
            code::DELETE => {
                if f.cursor == f.text.len() {
                    return true;
                }
                let n = next(&f.text, f.cursor);
                f.text.replace_range(f.cursor..n, "");
                out.push(Action::Changed(id));
            }
            code::ENTER | code::KPENTER => out.push(Action::Submitted(id)),
            _ => {
                let Some(ch) = ch else { return false };
                f.text.insert(f.cursor, ch);
                f.cursor += ch.len_utf8();
                out.push(Action::Changed(id));
            }
        }
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn list_key(&mut self, id: Id, len: usize, row_h: i32, view_h: i32, first_cell: bool, c: u32, ch: Option<char>, now_ms: u32, out: &mut Vec<Action>) -> bool {
        let page = ((view_h / row_h.max(1)) as usize).max(1);
        let l = self.lists.entry(id).or_default();
        if len == 0 {
            return false;
        }
        let cur = l.selected;
        let to = match c {
            code::UP => Some(cur.map_or(0, |r| r.saturating_sub(1))),
            code::DOWN => Some(cur.map_or(0, |r| (r + 1).min(len - 1))),
            code::PAGEUP => Some(cur.map_or(0, |r| r.saturating_sub(page))),
            code::PAGEDOWN => Some(cur.map_or(0, |r| (r + page).min(len - 1))),
            code::HOME => Some(0),
            code::END => Some(len - 1),
            code::ENTER | code::KPENTER => {
                if let Some(r) = cur {
                    out.push(Action::Activated { list: id, row: r });
                }
                return true;
            }
            _ => {
                // type-to-find: a space only continues a search
                let Some(ch) = ch.filter(|&ch| first_cell && (ch != ' ' || !l.find.is_empty())) else { return false };
                if now_ms.wrapping_sub(l.find_ms) > FIND_RESET_MS {
                    l.find.clear();
                }
                l.find.push(ch);
                l.find_ms = now_ms;
                // the search needs the rows: `handle` runs it (`find`) once the key is done
                l.finding = true;
                return true;
            }
        };
        l.find.clear();
        if to != cur {
            l.selected = to;
            out.push(Action::Selected { list: id, row: to.unwrap() });
        }
        // into view
        let r = to.unwrap() as i32;
        if r * row_h < l.scroll {
            l.scroll = r * row_h;
        } else if (r + 1) * row_h > l.scroll + view_h {
            l.scroll = (r + 1) * row_h - view_h;
        }
        true
    }

    /// The frame for `root` at `size`: what to paint and the semantic tree, with `title` the window's name.
    pub fn render<'a>(&mut self, title: &str, root: &Widget<'a>, size: (i32, i32), m: &mut dyn Measure) -> Frame<'a> {
        let face = self.w.face;
        let all = Rect::new(0, 0, size.0, size.1);
        let mut cx = Cx::new(self, m, true, title);
        cx.paint.push(Paint::Clip(all));
        cx.paint.push(Paint::Fill { rect: all, color: face });
        cx.nodes[0].bounds = all;
        cx.walk(root, all, all, ROOT);
        Frame { paint: cx.paint, nodes: cx.nodes }
    }

    /// Type-to-find in the list where something was just typed: the first row whose first cell starts with it (case-insensitively);
    /// nothing matching leaves the selection where it was.
    fn find(&mut self, w: &Widget, out: &mut Vec<Action>) {
        match w {
            Widget::List(list) => {
                let Some(l) = self.lists.get_mut(&list.id).filter(|l| l.finding) else { return };
                l.finding = false;
                let want = l.find.to_lowercase();
                let hit = (0..list.len).find(|&i| (list.row)(i).cells.first().is_some_and(|c| c.to_lowercase().starts_with(&want)));
                if let Some(row) = hit.filter(|&r| l.selected != Some(r)) {
                    l.selected = Some(row);
                    l.scroll = -1; // into view at the next walk
                    out.push(Action::Selected { list: list.id, row });
                }
            }
            Widget::Column(v) | Widget::Row(v) => v.iter().for_each(|(_, c)| self.find(c, out)),
            Widget::Split { first, second, .. } => {
                self.find(first, out);
                self.find(second, out);
            }
            Widget::Scroll { child, .. } | Widget::Pane { child, .. } => self.find(child, out),
            _ => {}
        }
    }
}

fn is_modifier(c: u32) -> bool {
    matches!(c, code::LEFTSHIFT | code::RIGHTSHIFT | code::LEFTCTRL | code::RIGHTCTRL | code::LEFTALT | code::RIGHTALT | code::CAPSLOCK)
}

fn hit(p: &Placed, (x, y): (i32, i32)) -> bool {
    p.rect.contains(x, y) && p.clip.contains(x, y)
}

fn intersect(a: Rect, b: Rect) -> Rect {
    a.intersect(&b).unwrap_or(Rect::new(a.x, a.y, 0, 0))
}

/// FNV-1a of a list's id and a row's key, in the derived range.
fn derive(list: Id, key: u64, salt: u8) -> Id {
    let mut h: u32 = 0x811c_9dc5;
    for b in list.to_le_bytes().iter().chain(key.to_le_bytes().iter()).chain(core::iter::once(&salt)) {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h | DERIVED
}

/// One walk of the tree: layout always; paint and nodes when `paint`.
struct Cx<'s, 'm, 'a> {
    st: &'s mut State,
    m: &'m mut dyn Measure,
    paint_on: bool,
    placed: Vec<Placed>,
    paint: Vec<Paint<'a>>,
    nodes: Vec<Node>,
    used: BTreeSet<Id>,
    clip: Rect,
}

impl<'s, 'm, 'a> Cx<'s, 'm, 'a> {
    fn new(st: &'s mut State, m: &'m mut dyn Measure, paint_on: bool, title: &str) -> Self {
        let mut cx = Cx { st, m, paint_on, placed: Vec::new(), paint: Vec::new(), nodes: Vec::new(), used: BTreeSet::new(), clip: Rect::default() };
        if paint_on {
            let mut n = Node::new(ROOT, 0, Role::Window, Rect::default());
            n.name = title.into();
            if cx.st.window_focused {
                n.flags |= flag::FOCUSED;
            }
            cx.nodes.push(n);
            cx.used.insert(ROOT);
        }
        cx
    }

    fn px(&self, l: i32) -> i32 {
        self.st.px(l)
    }

    fn lh(&mut self) -> i32 {
        self.m.line_height()
    }

    fn row_h(&mut self, style: ListStyle) -> i32 {
        self.lh() + self.px(if style == ListStyle::Sidebar { 10 } else { 4 })
    }

    fn set_clip(&mut self, r: Rect) {
        if self.paint_on && r != self.clip {
            self.paint.push(Paint::Clip(r));
        }
        self.clip = r;
    }

    fn node(&mut self, id: Id, parent: Id, role: Role, bounds: Rect) -> Option<&mut Node> {
        if !self.paint_on {
            return None;
        }
        let mut id = id;
        while !self.used.insert(id) {
            id = id.wrapping_add(1) | DERIVED; // a derived id that collided this frame
        }
        self.nodes.push(Node::new(id, parent, role, bounds));
        if self.st.window_focused && self.st.focus == Some(id) {
            self.nodes.last_mut().unwrap().flags |= flag::FOCUSED;
        }
        self.nodes.last_mut()
    }

    fn text(&mut self, x: i32, y: i32, s: &str, font: Font, color: u32) {
        if self.paint_on && !s.is_empty() {
            self.paint.push(Paint::Text { x, y, text: s.into(), font, color });
        }
    }

    fn fill(&mut self, rect: Rect, color: u32) {
        if self.paint_on {
            self.paint.push(Paint::Fill { rect, color });
        }
    }

    /// A 1-pixel (× scale) ring inside `r`.
    fn ring(&mut self, r: Rect, color: u32) {
        let t = self.px(1);
        self.fill(Rect::new(r.x, r.y, r.w, t), color);
        self.fill(Rect::new(r.x, r.y + r.h - t, r.w, t), color);
        self.fill(Rect::new(r.x, r.y, t, r.h), color);
        self.fill(Rect::new(r.x + r.w - t, r.y, t, r.h), color);
    }

    /// The size `w` asks for.
    fn pref(&mut self, w: &Widget) -> (i32, i32) {
        let lh = self.lh();
        let pad = self.px(4);
        match w {
            Widget::Label { text, font, .. } => (self.m.width(text, *font), lh),
            Widget::Button { label, .. } => (self.m.width(label, Font::Sans) + 4 * pad, lh + 2 * pad),
            Widget::Field { .. } => (self.px(160), lh + 2 * pad),
            Widget::List(l) => (self.px(100), self.row_h(l.style) * 4),
            Widget::Image { w, h, .. } => (*w as i32, *h as i32),
            Widget::Column(v) => v.iter().fold((0, 0), |(aw, ah), (sz, c)| {
                let (cw, ch) = self.pref(c);
                (aw.max(cw), ah + if let Size::Fixed(n) = sz { self.px(*n) } else { ch })
            }),
            Widget::Row(v) => v.iter().fold((0, 0), |(aw, ah), (sz, c)| {
                let (cw, ch) = self.pref(c);
                (aw + if let Size::Fixed(n) = sz { self.px(*n) } else { cw }, ah.max(ch))
            }),
            Widget::Split { first, second, .. } => {
                let (a, b) = (self.pref(first), self.pref(second));
                (a.0 + b.0 + self.px(5), a.1.max(b.1))
            }
            Widget::Scroll { child, .. } | Widget::Pane { child, .. } => self.pref(child),
            Widget::Space => (0, 0),
        }
    }

    /// Splits `total` along a container's axis.
    fn shares(&mut self, v: &[(Size, Widget)], total: i32, vertical: bool) -> Vec<i32> {
        let mut out: Vec<i32> = Vec::with_capacity(v.len());
        let mut fills = 0;
        for (sz, c) in v {
            out.push(match sz {
                Size::Fixed(n) => self.px(*n),
                Size::Auto => {
                    let p = self.pref(c);
                    if vertical { p.1 } else { p.0 }
                }
                Size::Fill => {
                    fills += 1;
                    0
                }
            });
        }
        let left = (total - out.iter().sum::<i32>()).max(0);
        let mut given = 0;
        let mut k = 0;
        for (i, (sz, _)) in v.iter().enumerate() {
            if *sz == Size::Fill {
                k += 1;
                let share = if k == fills { left - given } else { left / fills };
                out[i] = share;
                given += share;
            }
        }
        out
    }

    fn walk(&mut self, w: &Widget<'a>, r: Rect, clip: Rect, parent: Id) {
        let vis = intersect(r, clip);
        let lh = self.lh();
        let pad = self.px(4);
        let st_w = self.st.w;
        match w {
            Widget::Label { id, text, font } => {
                self.placed.push(Placed { id: *id, rect: r, clip, kind: Kind::Other });
                self.set_clip(vis);
                self.text(r.x, r.y + (r.h - lh) / 2, text, *font, st_w.fg);
                if let Some(n) = self.node(*id, parent, Role::Label, r) {
                    n.name = (*text).into();
                }
            }
            Widget::Button { id, label } => {
                self.placed.push(Placed { id: *id, rect: r, clip, kind: Kind::Button });
                let down = self.st.pressed == Some(*id) && r.contains(self.st.pointer.0, self.st.pointer.1);
                self.set_clip(vis);
                if self.paint_on {
                    self.paint.push(Paint::Button { rect: r, look: &st_w.button, down });
                }
                let tw = self.m.width(label, Font::Sans);
                let off = if down { self.px(1) } else { 0 };
                self.text(r.x + (r.w - tw) / 2 + off, r.y + (r.h - lh) / 2 + off, label, Font::Sans, st_w.button_fg);
                let focused = self.st.focus == Some(*id) && self.st.window_focused;
                if focused {
                    let i = self.px(3);
                    self.ring(Rect::new(r.x + i, r.y + i, r.w - 2 * i, r.h - 2 * i), st_w.focus);
                }
                if let Some(n) = self.node(*id, parent, Role::Button, r) {
                    n.name = (*label).into();
                    n.actions = action::CLICK | action::FOCUS;
                }
            }
            Widget::Field { id, label, placeholder } => {
                self.placed.push(Placed { id: *id, rect: r, clip, kind: Kind::Field });
                let focused = self.st.focus == Some(*id) && self.st.window_focused;
                let f = self.st.fields.get(id).cloned().unwrap_or_default();
                self.set_clip(vis);
                self.fill(r, if focused { st_w.focus } else { st_w.field_border });
                let t = self.px(1);
                let inner = Rect::new(r.x + t, r.y + t, r.w - 2 * t, r.h - 2 * t);
                self.fill(inner, st_w.field);
                self.set_clip(intersect(inner, clip));
                let ty = r.y + (r.h - lh) / 2;
                if f.text.is_empty() {
                    if !focused {
                        self.text(r.x + pad, ty, placeholder, Font::Sans, st_w.dim_fg);
                    }
                } else {
                    // keep the cursor in view
                    let cx = self.m.width(&f.text[..f.cursor], Font::Sans);
                    let room = inner.w - 2 * pad;
                    let shift = (cx - room).max(0);
                    self.text(r.x + pad - shift, ty, &f.text, Font::Sans, st_w.fg);
                }
                if focused {
                    let cx = self.m.width(&f.text[..f.cursor], Font::Sans);
                    let shift = (cx - (inner.w - 2 * pad)).max(0);
                    self.fill(Rect::new(r.x + pad + cx - shift, ty, self.px(1), lh), st_w.fg);
                }
                if let Some(n) = self.node(*id, parent, Role::TextInput, r) {
                    n.name = (*label).into();
                    n.value = f.text.clone();
                    n.actions = action::FOCUS | action::SET_VALUE;
                }
            }
            Widget::List(list) => self.list(list, r, clip, parent),
            Widget::Image { id, name, pixels, w, h } => {
                self.placed.push(Placed { id: *id, rect: r, clip, kind: Kind::Other });
                self.set_clip(vis);
                let rect = Rect::new(r.x, r.y, (*w as i32).min(r.w), (*h as i32).min(r.h));
                if self.paint_on && pixels.len() >= w * h {
                    self.paint.push(Paint::Image { rect, pixels, w: *w, h: *h });
                }
                if let Some(n) = self.node(*id, parent, Role::Image, rect) {
                    n.name = (*name).into();
                }
            }
            Widget::Column(v) => {
                let hs = self.shares(v, r.h, true);
                let mut y = r.y;
                for ((_, c), h) in v.iter().zip(hs) {
                    self.walk(c, Rect::new(r.x, y, r.w, h), clip, parent);
                    y += h;
                }
            }
            Widget::Row(v) => {
                let ws = self.shares(v, r.w, false);
                let mut x = r.x;
                for ((_, c), w) in v.iter().zip(ws) {
                    self.walk(c, Rect::new(x, r.y, w, r.h), clip, parent);
                    x += w;
                }
            }
            Widget::Split { id, first, second, at, min } => {
                let hw = self.px(5);
                let min = self.px(*min);
                let max = (r.w - hw - min).max(min);
                let a = self.st.splits.get(id).copied().unwrap_or(self.px(*at)).clamp(min, max);
                let handle = Rect::new(r.x + a, r.y, hw, r.h);
                self.placed.push(Placed { id: *id, rect: r, clip, kind: Kind::Split { handle, min } });
                self.walk(first, Rect::new(r.x, r.y, a, r.h), clip, parent);
                self.set_clip(intersect(handle, clip));
                self.fill(handle, st_w.splitter);
                if let Some(n) = self.node(*id, parent, Role::Splitter, handle) {
                    n.value = alloc::format!("{}", a);
                }
                self.walk(second, Rect::new(r.x + a + hw, r.y, r.w - a - hw, r.h), clip, parent);
            }
            Widget::Scroll { id, child } => {
                let ch = self.pref(child).1.max(r.h);
                let s = self.st.scrolls.get(id).copied().unwrap_or(0).clamp(0, ch - r.h);
                self.st.scrolls.insert(*id, s);
                self.placed.push(Placed { id: *id, rect: r, clip, kind: Kind::Scroll { content_h: ch } });
                let me = if let Some(n) = self.node(*id, parent, Role::ScrollView, r) {
                    if s > 0 {
                        n.actions |= action::SCROLL_UP;
                    }
                    if s < ch - r.h {
                        n.actions |= action::SCROLL_DOWN;
                    }
                    n.id
                } else {
                    *id
                };
                self.walk(child, Rect::new(r.x, r.y - s, r.w, ch), vis, me);
            }
            Widget::Pane { id, name, role, child } => {
                let me = if let Some(n) = self.node(*id, parent, *role, r) {
                    n.name = (*name).into();
                    n.id
                } else {
                    *id
                };
                self.walk(child, r, clip, me);
            }
            Widget::Space => {}
        }
    }

    fn list(&mut self, list: &List<'a>, r: Rect, clip: Rect, parent: Id) {
        let st_w = self.st.w;
        let lh = self.lh();
        let pad = self.px(4);
        let row_h = self.row_h(list.style);
        let table = list.style == ListStyle::Table;
        let head_h = if table && list.columns.iter().any(|c| !c.title.is_empty()) { lh + self.px(6) } else { 0 };
        let first_cell = !list.columns.is_empty() || !table;
        self.placed.push(Placed { id: list.id, rect: r, clip, kind: Kind::List { len: list.len, row_h, head_h, first_cell } });
        let focused = self.st.focus == Some(list.id) && self.st.window_focused;
        let view_h = (r.h - head_h).max(0);
        let max_scroll = (list.len as i32 * row_h - view_h).max(0);
        let l = self.st.lists.entry(list.id).or_default();
        if l.selected.is_some_and(|s| s >= list.len) {
            l.selected = if list.len > 0 { Some(list.len - 1) } else { None };
        }
        if l.scroll < 0 {
            // a find just selected a row: into view
            l.scroll = l.selected.map_or(0, |s| (s as i32 * row_h - view_h / 2).max(0));
        }
        l.scroll = l.scroll.clamp(0, max_scroll);
        let (scroll, selected) = (l.scroll, l.selected);

        let me = if let Some(n) = self.node(list.id, parent, Role::ListBox, r) {
            n.name = list.name.into();
            n.actions = action::FOCUS | action::SCROLL_INTO_VIEW;
            if scroll > 0 {
                n.actions |= action::SCROLL_UP;
            }
            if scroll < max_scroll {
                n.actions |= action::SCROLL_DOWN;
            }
            n.set_size = list.len as u32;
            n.id
        } else {
            list.id
        };
        if !self.paint_on {
            return;
        }
        let vis = intersect(r, clip);
        self.set_clip(vis);
        self.fill(r, if table { st_w.field } else { st_w.sidebar });

        // columns: x and width inside the list (the scroll thumb's room off the last one)
        let thumb_w = if max_scroll > 0 { self.px(6) } else { 0 };
        let cols: Vec<(Size, Widget)> = list.columns.iter().map(|c| (c.width, Widget::Space)).collect();
        let ws = if cols.is_empty() { alloc::vec![r.w - thumb_w] } else { self.shares(&cols, r.w - thumb_w, false) };
        let mut xs = Vec::with_capacity(ws.len());
        let mut x = r.x;
        for w in &ws {
            xs.push((x, *w));
            x += w;
        }

        if head_h > 0 {
            for (i, c) in list.columns.iter().enumerate() {
                let (cx, cw) = xs[i];
                let hr = Rect::new(cx, r.y, cw, head_h);
                self.set_clip(intersect(hr, vis));
                self.paint.push(Paint::Button { rect: hr, look: &st_w.header, down: false });
                let tw = self.m.width(c.title, Font::Sans);
                let tx = if c.right { cx + cw - pad - tw } else { cx + pad };
                self.text(tx, r.y + (head_h - lh) / 2, c.title, Font::Sans, st_w.header_fg);
                if let Some(n) = self.node(derive(list.id, i as u64, 1), me, Role::ColumnHeader, hr) {
                    n.name = c.title.into();
                }
            }
        }

        let body = Rect::new(r.x, r.y + head_h, r.w, view_h);
        let body_clip = intersect(body, vis);
        let first = (scroll / row_h) as usize;
        let last = (((scroll + view_h + row_h - 1) / row_h) as usize).min(list.len);
        for i in first..last {
            let row = (list.row)(i);
            let y = body.y + i as i32 * row_h - scroll;
            let rr = Rect::new(r.x, y, r.w - thumb_w, row_h);
            self.set_clip(body_clip);
            let sel = selected == Some(i);
            let fg = if sel {
                let (bg, fg) = if focused { (st_w.selection, st_w.selection_fg) } else { (st_w.selection_idle, st_w.selection_idle_fg) };
                self.fill(rr, bg);
                fg
            } else if table {
                st_w.fg
            } else {
                st_w.sidebar_fg
            };
            for (j, cell) in row.cells.iter().enumerate() {
                let Some(&(cx, cw)) = xs.get(j) else { break };
                let right = list.columns.get(j).is_some_and(|c| c.right);
                let cr = Rect::new(cx + pad, y, (cw - 2 * pad).max(0), row_h);
                self.set_clip(intersect(cr, body_clip));
                let tx = if right { cr.x + cr.w - self.m.width(cell, Font::Sans) } else { cr.x };
                let color = if j > 0 && !sel { st_w.dim_fg } else { fg };
                self.text(tx, y + (row_h - lh) / 2, cell, Font::Sans, color);
            }
            if sel && focused {
                self.set_clip(body_clip);
                self.ring(rr, st_w.selection_fg);
            }
            if let Some(n) = self.node(derive(list.id, row.key, 0), me, Role::ListBoxOption, rr) {
                n.name = row.cells.first().cloned().unwrap_or_default();
                n.value = row.cells.iter().skip(1).filter(|c| !c.is_empty()).cloned().collect::<Vec<_>>().join("; ");
                n.pos = i as u32 + 1;
                n.set_size = list.len as u32;
                n.actions = action::CLICK | action::FOCUS | action::SCROLL_INTO_VIEW;
                if sel {
                    n.flags |= flag::SELECTED;
                }
            }
        }
        if max_scroll > 0 {
            self.set_clip(vis);
            let th = (view_h * view_h / (list.len as i32 * row_h)).max(self.px(12)).min(view_h);
            let ty = body.y + (view_h - th) * scroll / max_scroll;
            self.fill(Rect::new(r.x + r.w - thumb_w, ty, thumb_w, th), st_w.splitter);
        }
    }
}
