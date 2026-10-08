//! The `ui` widgets in a window: a button, a search field, a sidebar of places and a 10 000-row list side by side, a status line. Every
//! action is printed (`ui-demo: action …`) and shown in the status line; the semantic tree goes to the compositor with every frame.
//! Esc or the close button ends it.
//!
//!     compositor /mnt/bin/ui-demo term      (then `gui-tree` in term)

use std::process::ExitCode;
use std::time::Instant;

use gui_client::{Event, Window};
use ui::render::{Painter, FONT_FILES};
use ui::{Action, Column, Input, List, ListStyle, Row, Size, State, Widget};

const FONT_DIR: &str = "/mnt/usr/share/fonts";
const ADD: u32 = 10;
const SEARCH: u32 = 11;
const ITEMS: u32 = 12;
const STATUS: u32 = 13;
const SPLIT: u32 = 14;
const PLACES: u32 = 15;
const N: usize = 10_000;
const KEY_ESC: u32 = 1;

const WORDS: [&str; 4] = ["apple", "banana", "cherry", "date"];
const PLACE_NAMES: [&str; 5] = ["/", "/mnt", "/tmp", "/proc", "/dev"];
const COLS: [Column; 2] =
    [Column { title: "Name", width: Size::Fill, right: false }, Column { title: "Size", width: Size::Fixed(90), right: true }];

fn item(i: usize) -> Row {
    Row { key: i as u64, cells: vec![format!("{} {:05}", WORDS[i % 4], i), format!("{} B", i * 37 % 100_000)] }
}

fn place(i: usize) -> Row {
    Row { key: i as u64, cells: vec![PLACE_NAMES[i].into()] }
}

fn tree(status: &str) -> Widget<'_> {
    Widget::Column(vec![
        (
            Size::Auto,
            Widget::Row(vec![
                (Size::Auto, Widget::button(ADD, "Add")),
                (Size::Fixed(6), Widget::Space),
                (Size::Fill, Widget::field(SEARCH, "Search", "type and press Enter")),
            ]),
        ),
        (Size::Fixed(4), Widget::Space),
        (
            Size::Fill,
            Widget::split(
                SPLIT,
                Widget::pane(
                    16,
                    "Places",
                    Widget::List(List { id: PLACES, name: "Places", len: PLACE_NAMES.len(), columns: &[], style: ListStyle::Sidebar, row: &place }),
                ),
                Widget::List(List { id: ITEMS, name: "Items", len: N, columns: &COLS, style: ListStyle::Table, row: &item }),
                120,
                60,
            ),
        ),
        (Size::Auto, Widget::label(STATUS, status)),
    ])
}

fn input(e: Event) -> Option<Input> {
    Some(match e {
        Event::Key { code, pressed } => Input::Key { code, pressed },
        Event::Motion { x, y } => Input::Motion { x, y },
        Event::Button { code, pressed } => Input::Button { code, pressed },
        Event::Focus(f) => Input::Focus(f),
        _ => return None,
    })
}

fn run() -> std::io::Result<()> {
    let mut files = Vec::new();
    for f in FONT_FILES {
        let path = format!("{}/{}", FONT_DIR, f);
        files.push(std::fs::read(&path).map_err(|e| std::io::Error::new(e.kind(), format!("cannot read the font {}: {}", path, e)))?);
    }
    let mut p = Painter::new(files, 15.0).ok_or_else(|| std::io::Error::other("the font files are not fonts"))?;
    let mut win = Window::open("ui-demo", Some((560, 360)))?;
    win.set_resizable(240, 160)?;
    let mut st = State::new(&gui::theme::LUNA, 1);
    let mut status = String::from("ready");
    let t0 = Instant::now();
    let mut clicks = 0;
    let mut first = true;
    loop {
        let (w, h) = win.size();
        {
            let t = tree(&status);
            let f = st.render("ui-demo", &t, (w as i32, h as i32), &mut p);
            p.paint(&f.paint, win.frame()?, w, h);
            win.set_semantics(&f.nodes)?;
        }
        win.present()?;
        if first {
            println!("ui-demo: ready {}x{}", w, h);
            first = false;
        }
        let Some(ev) = win.next_event(None)? else { continue };
        let ev = match ev {
            Event::Close => break,
            Event::Resize { width, height } => {
                win.resize(width, height)?;
                continue;
            }
            e => e,
        };
        let Some(i) = input(ev) else { continue };
        let now = t0.elapsed().as_millis() as u32;
        let actions = {
            let t = tree(&status);
            st.handle(&t, (w as i32, h as i32), i, now, &mut p)
        };
        for a in actions {
            println!("ui-demo: action {:?}", a);
            status = match a {
                Action::Clicked(ADD) => {
                    clicks += 1;
                    format!("Add pressed {} times", clicks)
                }
                Action::Selected { list, row } => format!("selected {}", if list == ITEMS { item(row).cells[0].clone() } else { place(row).cells[0].clone() }),
                Action::Activated { list, row } => format!("opened {}", if list == ITEMS { item(row).cells[0].clone() } else { place(row).cells[0].clone() }),
                Action::Submitted(SEARCH) => format!("searched for \"{}\"", st.text(SEARCH)),
                Action::Key { code: KEY_ESC, .. } => {
                    println!("ui-demo: bye");
                    return Ok(());
                }
                _ => continue,
            };
        }
    }
    println!("ui-demo: bye");
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ui-demo: {}", e);
            ExitCode::FAILURE
        }
    }
}
