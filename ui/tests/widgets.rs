//! The widgets driven as a program drives them: a tree built per frame, input through `handle`, and what `render` gives back — paint
//! operations and the semantic tree — checked. Text is measured by a fake: 8 pixels a character, 16 a line.

use std::cell::Cell;

use gui::theme::{LUNA, NINES};
use ui::*;

struct Mono;

impl Measure for Mono {
    fn width(&mut self, s: &str, _: Font) -> i32 {
        8 * s.chars().count() as i32
    }
    fn line_height(&mut self) -> i32 {
        16
    }
}

const SIZE: (i32, i32) = (400, 300);
const BTN: Id = 10;
const FIELD: Id = 11;
const LIST: Id = 12;
const LABEL: Id = 13;
const SPLIT: Id = 14;
const SIDE: Id = 15;

// Linux key codes
const K_TAB: u32 = 15;
const K_ENTER: u32 = 28;
const K_SHIFT: u32 = 42;
const K_UP: u32 = 103;
const K_DOWN: u32 = 108;
const K_PGDN: u32 = 109;
const K_HOME: u32 = 102;
const K_END: u32 = 107;
const K_LEFT: u32 = 105;
const K_BACKSPACE: u32 = 14;
const K_ESC: u32 = 1;
const K_SPACE: u32 = 57;
const K_A: u32 = 30;
const K_B: u32 = 48;
const K_C: u32 = 46;
const K_H: u32 = 35;
const K_L: u32 = 38;
const K_O: u32 = 24;

const WORDS: [&str; 4] = ["apple", "banana", "cherry", "date"];

fn name(i: usize) -> String {
    format!("{} {:05}", WORDS[i % 4], i)
}

/// A window: a button and a field over a list of `n` rows (two columns).
struct App {
    n: usize,
    calls: Cell<usize>,
    /// Row keys are `i + key_base`: a different base is "rows inserted above".
    key_base: u64,
}

impl App {
    fn new(n: usize) -> App {
        App { n, calls: Cell::new(0), key_base: 0 }
    }

    fn row(&self, i: usize) -> Row {
        self.calls.set(self.calls.get() + 1);
        Row { key: i as u64 + self.key_base, cells: vec![name(i), format!("{} B", i)] }
    }
}

const COLS: [Column; 2] =
    [Column { title: "Name", width: Size::Fill, right: false }, Column { title: "Size", width: Size::Fixed(80), right: true }];

fn tree<'a>(app: &'a App, row: &'a dyn Fn(usize) -> Row) -> Widget<'a> {
    Widget::Column(vec![
        (Size::Auto, Widget::Row(vec![(Size::Auto, Widget::button(BTN, "Add")), (Size::Fill, Widget::field(FIELD, "Search", "type here"))])),
        (Size::Auto, Widget::label(LABEL, "status")),
        (Size::Fill, Widget::List(List { id: LIST, name: "Items", len: app.n, columns: &COLS, style: ListStyle::Table, row })),
    ])
}

/// Runs `f` with the app's tree.
fn with<R>(app: &App, f: impl FnOnce(&Widget) -> R) -> R {
    let row = |i| app.row(i);
    let t = tree(app, &row);
    f(&t)
}

fn key(st: &mut State, app: &App, code: u32, now: u32) -> Vec<Action> {
    let mut a = with(app, |t| st.handle(t, SIZE, Input::Key { code, pressed: true }, now, &mut Mono));
    a.extend(with(app, |t| st.handle(t, SIZE, Input::Key { code, pressed: false }, now, &mut Mono)));
    a
}

fn input(st: &mut State, app: &App, i: Input, now: u32) -> Vec<Action> {
    with(app, |t| st.handle(t, SIZE, i, now, &mut Mono))
}

fn render(st: &mut State, app: &App) -> (Vec<Node>, Vec<Paint<'static>>) {
    let row = |i| app.row(i);
    let t = tree(app, &row);
    let f = st.render("demo", &t, SIZE, &mut Mono);
    let paint: Vec<Paint<'static>> = f
        .paint
        .into_iter()
        .map(|p| match p {
            Paint::Image { .. } => panic!("no images here"),
            Paint::Clip(r) => Paint::Clip(r),
            Paint::Fill { rect, color } => Paint::Fill { rect, color },
            Paint::Button { rect, look, down } => Paint::Button { rect, look, down },
            Paint::Text { x, y, text, font, color } => Paint::Text { x, y, text, font, color },
        })
        .collect();
    (f.nodes, paint)
}

fn options(nodes: &[Node]) -> Vec<&Node> {
    nodes.iter().filter(|n| n.role == Role::ListBoxOption).collect()
}

fn node(nodes: &[Node], id: Id) -> &Node {
    nodes.iter().find(|n| n.id == id).unwrap_or_else(|| panic!("no node {id}"))
}

fn click(st: &mut State, app: &App, x: i32, y: i32, now: u32) -> Vec<Action> {
    let mut a = input(st, app, Input::Motion { x, y }, now);
    a.extend(input(st, app, Input::Button { code: BTN_LEFT, pressed: true }, now));
    a.extend(input(st, app, Input::Button { code: BTN_LEFT, pressed: false }, now));
    a
}

fn centre(r: Rect) -> (i32, i32) {
    (r.x + r.w / 2, r.y + r.h / 2)
}

#[test]
fn a_10000_row_list_renders_only_what_shows() {
    let app = App::new(10_000);
    let mut st = State::new(&LUNA, 1);
    let (nodes, _) = render(&mut st, &app);
    let opts = options(&nodes);
    let list = node(&nodes, LIST);
    // rows are 20 px, the header 22, the button row 24 and the label 16: (300 - 24 - 16 - 22) / 20 = 11.9 -> 12 rows
    assert_eq!(app.calls.get(), 12, "row() called only for the rows shown");
    assert_eq!(opts.len(), 12);
    assert_eq!(list.set_size, 10_000);
    assert_eq!(list.name, "Items");
    assert_eq!((opts[0].pos, opts[0].set_size, opts[0].name.as_str(), opts[0].value.as_str()), (1, 10_000, "apple 00000", "0 B"));
    assert!(opts.iter().all(|o| o.parent == LIST));
    // handle never asks for rows
    app.calls.set(0);
    input(&mut st, &app, Input::Motion { x: 5, y: 5 }, 0);
    key(&mut st, &app, K_DOWN, 0);
    assert_eq!(app.calls.get(), 0);

    // End: the last rows, and only them
    click(&mut st, &app, 200, 100, 0);
    key(&mut st, &app, K_END, 1000);
    app.calls.set(0);
    let (nodes, _) = render(&mut st, &app);
    let opts = options(&nodes);
    assert!(app.calls.get() <= 13, "{} calls", app.calls.get());
    let last = opts.last().unwrap();
    assert_eq!((last.pos, last.name.as_str()), (10_000, "date 09999"));
    assert!(last.has(flag::SELECTED));
    assert!(last.bounds.y + last.bounds.h <= SIZE.1, "the selected row is in view");
    assert_eq!(opts.iter().filter(|o| o.has(flag::SELECTED)).count(), 1);
    // and Home brings the first row back into view
    key(&mut st, &app, K_HOME, 2000);
    let (nodes, _) = render(&mut st, &app);
    let first = options(&nodes)[0];
    assert_eq!(first.pos, 1);
    assert!(first.has(flag::SELECTED));
}

#[test]
fn keyboard_moves_focus_and_selection() {
    let app = App::new(100);
    let mut st = State::new(&LUNA, 1);
    assert_eq!(key(&mut st, &app, K_TAB, 0), vec![]);
    assert_eq!(st.focus(), Some(BTN));
    key(&mut st, &app, K_TAB, 0);
    assert_eq!(st.focus(), Some(FIELD));
    key(&mut st, &app, K_TAB, 0);
    assert_eq!(st.focus(), Some(LIST));
    key(&mut st, &app, K_TAB, 0);
    assert_eq!(st.focus(), Some(BTN), "Tab wraps");
    // Shift+Tab goes back
    input(&mut st, &app, Input::Key { code: K_SHIFT, pressed: true }, 0);
    key(&mut st, &app, K_TAB, 0);
    input(&mut st, &app, Input::Key { code: K_SHIFT, pressed: false }, 0);
    assert_eq!(st.focus(), Some(LIST));

    assert_eq!(key(&mut st, &app, K_DOWN, 0), vec![Action::Selected { list: LIST, row: 0 }]);
    assert_eq!(key(&mut st, &app, K_DOWN, 0), vec![Action::Selected { list: LIST, row: 1 }]);
    assert_eq!(key(&mut st, &app, K_UP, 0), vec![Action::Selected { list: LIST, row: 0 }]);
    assert_eq!(key(&mut st, &app, K_UP, 0), vec![], "nothing above row 0");
    // a page is the rows that fit (238 / 20 = 11)
    assert_eq!(key(&mut st, &app, K_PGDN, 0), vec![Action::Selected { list: LIST, row: 11 }]);
    assert_eq!(key(&mut st, &app, K_END, 0), vec![Action::Selected { list: LIST, row: 99 }]);
    assert_eq!(key(&mut st, &app, K_HOME, 0), vec![Action::Selected { list: LIST, row: 0 }]);
    assert_eq!(key(&mut st, &app, K_ENTER, 0), vec![Action::Activated { list: LIST, row: 0 }]);
    // keys nobody uses come back, with the focus
    assert_eq!(key(&mut st, &app, K_ESC, 0), vec![Action::Key { code: K_ESC, focus: Some(LIST) }]);
    assert_eq!(key(&mut st, &app, K_BACKSPACE, 0), vec![Action::Key { code: K_BACKSPACE, focus: Some(LIST) }]);
    assert_eq!(key(&mut st, &app, K_SPACE, 0), vec![Action::Key { code: K_SPACE, focus: Some(LIST) }], "Space is the app's (preview)");

    // the button: Space and Enter press it
    st.set_focus(Some(BTN));
    assert_eq!(key(&mut st, &app, K_SPACE, 0), vec![Action::Clicked(BTN)]);
    assert_eq!(key(&mut st, &app, K_ENTER, 0), vec![Action::Clicked(BTN)]);

    // the tree says who has the focus
    let (nodes, _) = render(&mut st, &app);
    assert!(node(&nodes, BTN).has(flag::FOCUSED));
    assert!(!node(&nodes, LIST).has(flag::FOCUSED));
    assert!(node(&nodes, ROOT).has(flag::FOCUSED));
    input(&mut st, &app, Input::Focus(false), 0);
    let (nodes, _) = render(&mut st, &app);
    assert!(!node(&nodes, BTN).has(flag::FOCUSED) && !node(&nodes, ROOT).has(flag::FOCUSED));
}

#[test]
fn typing_in_a_list_finds_a_row() {
    let app = App::new(100);
    let mut st = State::new(&LUNA, 1);
    st.set_focus(Some(LIST));
    assert_eq!(key(&mut st, &app, K_C, 0), vec![Action::Selected { list: LIST, row: 2 }]); // "cherry 00002"
    key(&mut st, &app, K_H, 100);
    assert_eq!(st.selected(LIST), Some(2), "\"ch\" still cherry 2");
    // "chb": no row; the selection stays
    assert_eq!(key(&mut st, &app, K_B, 200), vec![]);
    assert_eq!(st.selected(LIST), Some(2));
    // after a pause it starts over
    assert_eq!(key(&mut st, &app, K_B, 2000), vec![Action::Selected { list: LIST, row: 1 }]);
    assert_eq!(key(&mut st, &app, K_A, 2100), vec![], "\"ba\" is banana 1 already");
    // a typed row deep in the list is scrolled to
    let app = App::new(10_000);
    let mut st = State::new(&LUNA, 1);
    st.set_focus(Some(LIST));
    for (i, k) in [K_D, K_A, K_T, K_E, K_SPACE, K_0, K_9, K_9, K_9, K_9].iter().enumerate() {
        key(&mut st, &app, *k, i as u32 * 10);
    }
    assert_eq!(st.selected(LIST), Some(9999));
    let (nodes, _) = render(&mut st, &app);
    assert!(options(&nodes).iter().any(|o| o.pos == 10_000 && o.has(flag::SELECTED)));
}

const K_D: u32 = 32;
const K_T: u32 = 20;
const K_E: u32 = 18;
const K_0: u32 = 11;
const K_9: u32 = 10;

#[test]
fn a_field_edits_and_submits() {
    let app = App::new(3);
    let mut st = State::new(&LUNA, 1);
    st.set_focus(Some(FIELD));
    for k in [K_H, K_O, K_L, K_A] {
        assert_eq!(key(&mut st, &app, k, 0), vec![Action::Changed(FIELD)]);
    }
    assert_eq!(st.text(FIELD), "hola");
    key(&mut st, &app, K_LEFT, 0);
    key(&mut st, &app, K_BACKSPACE, 0);
    assert_eq!(st.text(FIELD), "hoa");
    input(&mut st, &app, Input::Key { code: K_SHIFT, pressed: true }, 0);
    key(&mut st, &app, K_L, 0);
    input(&mut st, &app, Input::Key { code: K_SHIFT, pressed: false }, 0);
    assert_eq!(st.text(FIELD), "hoLa");
    assert_eq!(key(&mut st, &app, K_ENTER, 0), vec![Action::Submitted(FIELD)]);
    // Shift held when the window lost the keyboard: its release went elsewhere, so it is forgotten
    input(&mut st, &app, Input::Key { code: K_SHIFT, pressed: true }, 0);
    input(&mut st, &app, Input::Focus(false), 0);
    input(&mut st, &app, Input::Focus(true), 0);
    key(&mut st, &app, K_A, 0);
    assert_eq!(st.text(FIELD), "hoLaa");
    key(&mut st, &app, K_BACKSPACE, 0);
    assert_eq!(key(&mut st, &app, K_HOME, 0), vec![]);
    key(&mut st, &app, K_BACKSPACE, 0);
    assert_eq!(st.text(FIELD), "hoLa", "Backspace at the start does nothing");
    let (nodes, paint) = render(&mut st, &app);
    let f = node(&nodes, FIELD);
    assert_eq!((f.role, f.name.as_str(), f.value.as_str()), (Role::TextInput, "Search", "hoLa"));
    assert!(paint.iter().any(|p| matches!(p, Paint::Text { text, .. } if text == "hoLa")));
    // a click puts the cursor at the nearest character: after "ho"
    let r = f.bounds;
    click(&mut st, &app, r.x + 4 + 16 + 3, r.y + 5, 0);
    key(&mut st, &app, K_BACKSPACE, 0);
    assert_eq!(st.text(FIELD), "hLa");
    // the placeholder shows only when empty and unfocused
    st.set_text(FIELD, "");
    st.set_focus(None);
    let (_, paint) = render(&mut st, &app);
    assert!(paint.iter().any(|p| matches!(p, Paint::Text { text, color, .. } if text == "type here" && *color == LUNA.widgets.dim_fg)));
}

#[test]
fn the_pointer_clicks_selects_and_scrolls() {
    let app = App::new(1000);
    let mut st = State::new(&LUNA, 1);
    let (nodes, _) = render(&mut st, &app);
    let b = node(&nodes, BTN).bounds;
    let (bx, by) = centre(b);
    assert_eq!(click(&mut st, &app, bx, by, 0), vec![Action::Clicked(BTN)]);
    // released outside: no click
    input(&mut st, &app, Input::Motion { x: bx, y: by }, 0);
    input(&mut st, &app, Input::Button { code: BTN_LEFT, pressed: true }, 0);
    input(&mut st, &app, Input::Motion { x: 390, y: 290 }, 0);
    assert_eq!(input(&mut st, &app, Input::Button { code: BTN_LEFT, pressed: false }, 0), vec![]);

    let third = options(&nodes)[2].bounds;
    let (rx, ry) = centre(third);
    assert_eq!(click(&mut st, &app, rx, ry, 1000), vec![Action::Selected { list: LIST, row: 2 }]);
    assert_eq!(st.focus(), Some(LIST));
    assert_eq!(click(&mut st, &app, rx, ry, 1200), vec![Action::Activated { list: LIST, row: 2 }], "double click");
    assert_eq!(click(&mut st, &app, rx, ry, 2000), vec![], "a third click starts over");
    assert_eq!(click(&mut st, &app, rx, ry, 3000), vec![], "too slow for a double click");
    let (sx, sy) = centre(options(&nodes)[3].bounds);
    assert_eq!(click(&mut st, &app, sx, sy, 3100), vec![Action::Selected { list: LIST, row: 3 }], "another row is not a double click");
    // the header is not a row
    let head = nodes.iter().find(|n| n.role == Role::ColumnHeader && n.name == "Name").unwrap().bounds;
    assert_eq!(click(&mut st, &app, head.x + 5, head.y + 5, 5000), vec![]);

    // the wheel: three rows a notch
    input(&mut st, &app, Input::Motion { x: rx, y: ry }, 0);
    input(&mut st, &app, Input::Wheel { dy: 2 }, 0);
    let (nodes, _) = render(&mut st, &app);
    assert_eq!(options(&nodes)[0].pos, 7);
    input(&mut st, &app, Input::Wheel { dy: -100 }, 0);
    let (nodes, _) = render(&mut st, &app);
    assert_eq!(options(&nodes)[0].pos, 1, "clamped at the top");
    input(&mut st, &app, Input::Wheel { dy: 100_000 }, 0);
    let (nodes, _) = render(&mut st, &app);
    assert_eq!(options(&nodes).last().unwrap().pos, 1000, "clamped at the bottom");
    input(&mut st, &app, Input::Wheel { dy: -1 }, 0);
    let (nodes, _) = render(&mut st, &app);
    assert_eq!(options(&nodes).last().unwrap().pos, 997, "one notch up from the bottom, not from past it");
}

#[test]
fn ids_are_stable_and_follow_the_row() {
    let mut app = App::new(50);
    let mut st = State::new(&LUNA, 1);
    let (a, _) = render(&mut st, &app);
    let (b, _) = render(&mut st, &app);
    assert_eq!(a.iter().map(|n| n.id).collect::<Vec<_>>(), b.iter().map(|n| n.id).collect::<Vec<_>>());
    let ids: std::collections::BTreeSet<Id> = a.iter().map(|n| n.id).collect();
    assert_eq!(ids.len(), a.len(), "unique");
    assert!(a.iter().all(|n| n.id != 0));
    assert!(options(&a).iter().all(|o| o.id >= DERIVED));
    // the row with key 5 is row 5 now; give it another index (two rows "inserted above") and it keeps its id
    let id5 = options(&a)[5].id;
    app.key_base = 2; // row i now has key i + 2: key 5 is row 3
    let (c, _) = render(&mut st, &app);
    assert_eq!(options(&c)[3].id, id5);
    // parents come before children
    for (i, n) in c.iter().enumerate() {
        if n.parent != 0 {
            assert!(c[..i].iter().any(|p| p.id == n.parent), "node {} before its parent {}", n.id, n.parent);
        }
    }
    assert_eq!(c[0].id, ROOT);
    assert_eq!(c[0].name, "demo");
    assert_eq!(c[0].bounds, Rect::new(0, 0, SIZE.0, SIZE.1));
}

#[test]
fn looks_come_from_the_theme() {
    let app = App::new(5);
    for theme in [&LUNA, &NINES] {
        let mut st = State::new(theme, 1);
        st.set_focus(Some(LIST));
        key(&mut st, &app, K_DOWN, 0);
        let (nodes, paint) = render(&mut st, &app);
        let row = options(&nodes)[0].bounds;
        let w = &theme.widgets;
        assert_eq!(paint[1], Paint::Fill { rect: Rect::new(0, 0, SIZE.0, SIZE.1), color: w.face });
        assert!(paint.iter().any(|p| *p == Paint::Fill { rect: row, color: w.selection }), "{}: focused selection", theme.name);
        assert!(paint.iter().any(|p| matches!(p, Paint::Text { text, color, .. } if text == "apple 00000" && *color == w.selection_fg)));
        assert!(paint.iter().any(|p| matches!(p, Paint::Button { look, .. } if std::ptr::eq(*look, &w.button))));
        assert!(paint.iter().any(|p| matches!(p, Paint::Button { look, .. } if std::ptr::eq(*look, &w.header))));
        // without the keyboard the selection goes idle
        input(&mut st, &app, Input::Focus(false), 0);
        let (_, paint) = render(&mut st, &app);
        assert!(paint.iter().any(|p| *p == Paint::Fill { rect: row, color: w.selection_idle }));
        assert!(!paint.iter().any(|p| *p == Paint::Fill { rect: row, color: w.selection }));
    }
}

#[test]
fn splits_drag_and_scroll_areas_scroll() {
    let rows = |i: usize| Row { key: i as u64, cells: vec![format!("place {i}")] };
    let lines: Vec<String> = (0..40).map(|i| format!("line {i}")).collect();
    let t = Widget::split(
            SPLIT,
            Widget::List(List { id: SIDE, name: "Places", len: 5, columns: &[], style: ListStyle::Sidebar, row: &rows }),
            Widget::scroll(30, Widget::pane(31, "Inspector", Widget::Column(lines.iter().enumerate().map(|(i, l)| (Size::Auto, Widget::label(100 + i as u32, l))).collect()))),
            120,
            60,
        );
    let mut st = State::new(&LUNA, 1);
    let f = st.render("s", &t, SIZE, &mut Mono);
    let split = node(&f.nodes, SPLIT).bounds;
    assert_eq!((split.x, split.w), (120, 5));
    assert_eq!(node(&f.nodes, SIDE).bounds.w, 120);
    assert_eq!(node(&f.nodes, 31).parent, 30, "the pane is inside the scroll view");
    assert_eq!(node(&f.nodes, 100).parent, 31);
    let side_opts: Vec<_> = f.nodes.iter().filter(|n| n.parent == SIDE).collect();
    assert_eq!(side_opts.len(), 5);
    assert_eq!(side_opts[0].bounds.h, 26, "sidebar rows are taller");
    drop(f);
    // drag the handle 50 px right
    let (hx, hy) = centre(split);
    for i in [Input::Motion { x: hx, y: hy }, Input::Button { code: BTN_LEFT, pressed: true }, Input::Motion { x: hx + 50, y: hy }, Input::Button { code: BTN_LEFT, pressed: false }] {
        st.handle(&t, SIZE, i, 0, &mut Mono);
    }
    let f = st.render("s", &t, SIZE, &mut Mono);
    assert_eq!(node(&f.nodes, SPLIT).bounds.x, 170, "grabbed 2 px into the handle");
    assert_eq!(node(&f.nodes, SIDE).bounds.w, 170);
    drop(f);
    // and far left: stops at the minimum
    for i in [Input::Motion { x: 174, y: hy }, Input::Button { code: BTN_LEFT, pressed: true }, Input::Motion { x: 0, y: hy }, Input::Button { code: BTN_LEFT, pressed: false }] {
        st.handle(&t, SIZE, i, 0, &mut Mono);
    }
    let f = st.render("s", &t, SIZE, &mut Mono);
    assert_eq!(node(&f.nodes, SIDE).bounds.w, 60);
    let y0 = node(&f.nodes, 100).bounds.y;
    assert!(node(&f.nodes, 30).actions & action::SCROLL_DOWN != 0);
    drop(f);
    // the wheel over the scroll area moves its content: 40 lines of 16 in 300
    st.handle(&t, SIZE, Input::Motion { x: 300, y: 100 }, 0, &mut Mono);
    st.handle(&t, SIZE, Input::Wheel { dy: 1 }, 0, &mut Mono);
    let f = st.render("s", &t, SIZE, &mut Mono);
    assert_eq!(node(&f.nodes, 100).bounds.y, y0 - 48);
    drop(f);
    st.handle(&t, SIZE, Input::Wheel { dy: 100 }, 0, &mut Mono);
    let f = st.render("s", &t, SIZE, &mut Mono);
    assert_eq!(node(&f.nodes, 139).bounds.y + 16, 300, "the last line at the bottom");
    assert!(node(&f.nodes, 30).actions & action::SCROLL_DOWN == 0);
}

#[test]
fn a_vertical_split_stacks_and_drags_up_and_down() {
    let rows = |i: usize| Row { key: i as u64, cells: vec![format!("r{i}")] };
    let t = Widget::vsplit(
        SPLIT,
        Widget::List(List { id: SIDE, name: "Top", len: 50, columns: &[], style: ListStyle::Table, row: &rows }),
        Widget::label(LABEL, "bottom"),
        100,
        40,
    );
    let mut st = State::new(&LUNA, 1);
    let f = st.render("v", &t, SIZE, &mut Mono);
    assert_eq!(node(&f.nodes, SPLIT).bounds, Rect::new(0, 100, 400, 5));
    assert_eq!(node(&f.nodes, SIDE).bounds, Rect::new(0, 0, 400, 100));
    assert_eq!(node(&f.nodes, LABEL).bounds.y, 105);
    drop(f);
    for i in [Input::Motion { x: 200, y: 102 }, Input::Button { code: BTN_LEFT, pressed: true }, Input::Motion { x: 10, y: 152 }, Input::Button { code: BTN_LEFT, pressed: false }] {
        st.handle(&t, SIZE, i, 0, &mut Mono);
    }
    let f = st.render("v", &t, SIZE, &mut Mono);
    assert_eq!(node(&f.nodes, SPLIT).bounds.y, 150, "followed the pointer down, not across");
    assert_eq!(node(&f.nodes, LABEL).bounds.y, 155);
    drop(f);
    for i in [Input::Motion { x: 200, y: 152 }, Input::Button { code: BTN_LEFT, pressed: true }, Input::Motion { x: 200, y: 299 }, Input::Button { code: BTN_LEFT, pressed: false }] {
        st.handle(&t, SIZE, i, 0, &mut Mono);
    }
    let f = st.render("v", &t, SIZE, &mut Mono);
    assert_eq!(node(&f.nodes, SPLIT).bounds.y, 300 - 5 - 40, "the bottom keeps its minimum");
}
