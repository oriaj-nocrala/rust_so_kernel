//! Prints every window's semantic tree (`gui::semantic`) as the compositor has it: one line per node, indented under its parent.
//!
//!     gui-tree            (inside the compositor: from term, or anything with $GUI_DISPLAY)
//!
//! A window's line: `window <toplevel> "<title>" at X,Y WxH [focused]` (its content on the screen). A node's:
//! `<Role>#<id> "<name>" = "<value>" @x,y wxh (pos/set) [flags] {actions}`, the bounds relative to the window's content; empty parts
//! are left out. `scripts/gui-e2e.sh ui` reads it.

use std::fmt::Write;
use std::process::ExitCode;

use gui_client::semantic::{action, flag};
use gui_client::{Node, WindowTree};

fn line(n: &Node, depth: usize) -> String {
    let mut s = format!("{:indent$}{}#{}", "", n.role.name(), n.id, indent = 2 * depth);
    if !n.name.is_empty() {
        write!(s, " {:?}", n.name).unwrap();
    }
    if !n.value.is_empty() {
        write!(s, " = {:?}", n.value).unwrap();
    }
    let b = n.bounds;
    write!(s, " @{},{} {}x{}", b.x, b.y, b.w, b.h).unwrap();
    if n.set_size > 0 {
        if n.pos > 0 {
            write!(s, " ({}/{})", n.pos, n.set_size).unwrap();
        } else {
            write!(s, " (of {})", n.set_size).unwrap();
        }
    }
    let flags: Vec<&str> = flag::NAMES.iter().filter(|(b, _)| n.flags & b != 0).map(|(_, s)| *s).collect();
    if !flags.is_empty() {
        write!(s, " [{}]", flags.join(",")).unwrap();
    }
    let acts: Vec<&str> = action::NAMES.iter().filter(|(b, _)| n.actions & b != 0).map(|(_, s)| *s).collect();
    if !acts.is_empty() {
        write!(s, " {{{}}}", acts.join(",")).unwrap();
    }
    s
}

fn print(w: &WindowTree) {
    let c = w.content;
    println!("window {} {:?} at {},{} {}x{}{}", w.toplevel, w.title, c.x, c.y, c.w, c.h, if w.focused { " [focused]" } else { "" });
    // depth from the parent chain (parents come first)
    let mut depth: std::collections::HashMap<u32, usize> = std::collections::HashMap::new();
    for n in &w.nodes {
        let d = if n.parent == 0 { 1 } else { depth.get(&n.parent).map_or(1, |d| d + 1) };
        depth.insert(n.id, d);
        println!("{}", line(n, d));
    }
}

fn main() -> ExitCode {
    match gui_client::semantics() {
        Ok(ws) => {
            for w in &ws {
                print(w);
            }
            println!("gui-tree: {} windows", ws.len());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("gui-tree: {}", e);
            ExitCode::FAILURE
        }
    }
}
