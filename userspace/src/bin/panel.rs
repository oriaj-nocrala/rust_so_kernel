#![no_std]
#![no_main]

//! The panel (phase 4.4 of `docs/gui/gui-plan.md`): a strip along the
//! bottom of the screen, a client of the compositor with the panel role
//! (`set_panel`) — undecorated, above every window, outside the work area.
//!
//! From left to right:
//!
//! - **Apps**, the start menu: a popup surface (`set_popup`) above the
//!   strip with the apps read from `/mnt/etc/gui/apps` (one
//!   `name<TAB>command` per line; a click starts one with
//!   `userspace::launch::spawn`, as the compositor starts its arguments)
//!   and the theme selector (`set_theme`). A click anywhere else, or
//!   Escape, closes it (the compositor's `popup_done`). Its look follows the
//!   theme: Luna's header and two columns, 9x's side banner, or flat.
//! - **The window list**: a button per window (the `toplevel*` events the
//!   compositor sends the panel alone), the focused one highlighted; a
//!   click raises and focuses it (`activate`).
//! - **A clock**, `HH:MM` UTC (as `/etc/localtime`), redrawn each minute.
//!
//! **Looks**: the compositor says its theme (`theme` event, `gui::theme`). The flat one is drawn as
//! always, opaque; in a theme with a `Taskbar` the strip is the compositor's (a shape under this
//! surface) and the panel draws only its buttons, with the theme's own shapes and bevels
//! (`Shape::paint`, the GPU's maths in software), into a premultiplied `ARGB8888` buffer that is
//! transparent everywhere else.
//!
//! Started by `compositor` without arguments. If it dies the compositor
//! goes on without it; it exits when the compositor goes.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use draw::Canvas;
use gui::compositor::scale_for;
use gui::protocol::{Event, Interface, Request, FORMAT_ARGB8888};
use gui::region::Rect;
use gui::theme::{self, Button as ThemeButton, Taskbar, Theme};
use gui::wire::{Decoder, Encoder};
use userspace::args::Args;
use userspace::syscall::{self, AF_UNIX, MAP_SHARED, PROT_READ, PROT_WRITE, SOCK_STREAM};
use userspace::text::{Style, Text, SANS};
use userspace::{entry, launch, println};

entry!(main);

const POOL: u32 = 2;
const BUFFER: u32 = 3;
const SURFACE: u32 = 4;

const APPS_FILE: &str = "/mnt/etc/gui/apps";
/// Height at scale 1, in pixels.
const HEIGHT: i32 = 32;
const BTN_LEFT: u32 = 0x110;


struct App {
    name: String,
    cmd: String,
}

/// `name<TAB>command` lines; blank lines and `#` comments skipped. Just a
/// terminal when the file is missing.
fn read_apps() -> Vec<App> {
    let mut apps = Vec::new();
    if let Ok(bytes) = userspace::fs::read_file(APPS_FILE) {
        for line in core::str::from_utf8(&bytes).unwrap_or("").lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((name, cmd)) = line.split_once('\t') {
                apps.push(App { name: name.trim().into(), cmd: cmd.trim().into() });
            }
        }
    }
    if apps.is_empty() {
        println!("panel: nothing in {}, offering a terminal", APPS_FILE);
        apps.push(App { name: "Terminal".into(), cmd: "term".into() });
    }
    apps
}

/// What a button does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Menu,
    Activate(u32),
}

/// What a menu item does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Item {
    App(usize),
    Theme(&'static str),
}

/// The themes the menu offers: `gui::theme` name, label.
const THEME_ITEMS: [(&str, &str); 2] = [("luna", "Luna 2026"), ("9x", "9x moderno")];

/// The menu laid out for the current theme: its size and where everything goes, in its own pixels.
struct MenuLayout {
    w: i32,
    h: i32,
    items: Vec<(Rect, String, Item)>,
    /// The areas a theme fills: the apps' column, the second column, header, banner, footer.
    left: Rect,
    side: Option<Rect>,
    header: Option<Rect>,
    banner: Option<Rect>,
    footer: Option<Rect>,
    /// The line between the apps and the themes (one column), and the second column's title.
    separator: Option<Rect>,
    side_title: Option<(i32, i32)>,
}

struct Button {
    x: i32,
    w: i32,
    label: String,
    action: Action,
}

struct Panel {
    k: i32,
    w: i32,
    h: i32,
    text: Text,
    apps: Vec<App>,
    /// `(toplevel id, title)`, in the order they appeared.
    windows: Vec<(u32, String)>,
    focused: u32,
    theme: &'static Theme,
    menu_open: bool,
    /// Screen height (the menu's log line gives screen coordinates).
    screen_h: i32,
    /// The item under the pointer in the open menu.
    menu_hover: Option<usize>,
    hover_x: Option<i32>,
    clock: String,
    /// The buttons as last logged (`label@x+w`), for scripts: logged
    /// again only when they change.
    logged: String,
}

impl Panel {
    fn style(&self, color: u32) -> Style<'static> {
        Style::new(SANS, 13.0 * self.k as f32).color(color)
    }

    fn text_w(&mut self, s: &str) -> i32 {
        let st = self.style(0);
        self.text.measure(s, &st, None).0
    }

    /// `s` shortened with "..." to fit `max` pixels.
    fn fit(&mut self, s: &str, max: i32) -> String {
        if self.text_w(s) <= max {
            return s.into();
        }
        let mut t: String = s.into();
        while !t.is_empty() {
            t.pop();
            let mut c = t.clone();
            c.push_str("...");
            if self.text_w(&c) <= max {
                return c;
            }
        }
        String::new()
    }

    /// The buttons, left to right: Apps, then the windows. Nothing reaches
    /// into the clock's space.
    fn layout(&mut self) -> Vec<Button> {
        let k = self.k;
        let gap = 4 * k;
        let pad = 12 * k;
        let clock_w = self.text_w("00:00") + 2 * pad;
        let limit = self.w - clock_w;
        let mut out = Vec::new();
        let mut x = gap;
        let w = self.text_w("Apps") + 2 * pad;
        out.push(Button { x, w, label: "Apps".into(), action: Action::Menu });
        x += w + 3 * gap;
        let items: Vec<(String, Action)> = self
            .windows
            .iter()
            .map(|(id, t)| (if t.is_empty() { String::from("(untitled)") } else { t.clone() }, Action::Activate(*id)))
            .collect();
        for (label, action) in items {
            let max = 200 * k;
            let label = self.fit(&label, max - 2 * pad);
            let w = self.text_w(&label) + 2 * pad;
            if x + w > limit {
                break;
            }
            out.push(Button { x, w, label, action });
            x += w + gap;
        }
        out
    }

    fn draw(&mut self, px: &mut [u32]) {
        let tb = &self.theme.taskbar;
        self.draw_themed(px, tb);
    }

    /// The buttons log line, when they change (scripts read it).
    fn log_buttons(&mut self, buttons: &[Button]) {
        let mut desc = String::new();
        for b in buttons {
            desc.push_str(&alloc::format!(" {}@{}+{}", b.label, b.x, b.w));
        }
        if desc != self.logged {
            println!("panel: buttons{}", desc);
            self.logged = desc;
        }
    }

    /// A theme's buttons over a transparent buffer (the strip under it is the compositor's).
    fn draw_themed(&mut self, px: &mut [u32], tb: &Taskbar) {
        let (w, h, k) = (self.w, self.h, self.k);
        let buttons = self.layout();
        self.log_buttons(&buttons);
        px.fill(0);
        let all = Rect::new(0, 0, w, h);
        let (by, bh) = (4 * k, h - 7 * k);
        let st = self.style(tb.task_fg);
        let (_, lh) = self.text.measure("Hg", &st, None);
        // the shapes first, then the text over them (text drawing writes 0x00RRGGBB: the alpha is put back after)
        let mut labels: Vec<(i32, i32, String, u32, u32, bool)> = Vec::new();
        for b in &buttons {
            let hover = self.hover_x.is_some_and(|x| x >= b.x && x < b.x + b.w);
            match b.action {
                Action::Menu => {
                    let bleed = tb.start_bleed * k;
                    let r = if bleed > 0 { Rect::new(b.x - bleed, 0, b.w + bleed, h) } else { Rect::new(b.x, by, b.w, bh) };
                    tb.start.paint(self.menu_open, px, w as usize, all, r, k);
                    labels.push((b.x + 12 * k, r.y + (r.h - lh) / 2, b.label.clone(), tb.start_fg, tb.start_shadow, true));
                }
                _ => {
                    let down = matches!(b.action, Action::Activate(id) if id == self.focused);
                    let r = Rect::new(b.x, by, b.w, bh);
                    tb.task.paint(down, px, w as usize, all, r, k);
                    if hover && !down && tb.hover.c[0] >> 24 != 0 {
                        tb.hover.scaled(k).paint(px, w as usize, all, r);
                    }
                    let push = if down && matches!(tb.task, ThemeButton::Bevel { .. }) { k } else { 0 };
                    labels.push((b.x + 12 * k + push, by + (bh - lh) / 2 + push, b.label.clone(), tb.task_fg, 0, false));
                }
            }
        }
        let st = self.style(tb.tray_fg);
        let cw = self.text.measure(&self.clock, &st, None).0;
        let pad = 12 * k;
        let tray = match tb.tray {
            // a shape reaches past the right edge (its right corners hidden) and fills the strip's height
            ThemeButton::Shape { .. } => Rect::new(w - cw - 2 * pad, 0, cw + 2 * pad + 8 * k, h),
            _ => Rect::new(w - cw - 2 * pad, by, cw + 2 * pad - 4 * k, bh),
        };
        tb.tray.paint(true, px, w as usize, all, tray, k);
        labels.push((tray.x + pad, tray.y + (tray.h - lh) / 2, self.clock.clone(), tb.tray_fg, 0, false));

        let alpha: Vec<u8> = px.iter().map(|p| (p >> 24) as u8).collect();
        let mut cv = Canvas::new(px, w as usize, h as usize, w as usize);
        for (x, y, text, fg, shadow, bold) in labels {
            let size = 13.0 * k as f32;
            let style = |c: u32| if bold { Style::new(SANS, size).color(c).bold() } else { Style::new(SANS, size).color(c) };
            if shadow >> 24 != 0 {
                self.text.draw(&mut cv, &text, &style(shadow & 0x00FF_FFFF), None, x + k, y + k);
            }
            self.text.draw(&mut cv, &text, &style(fg), None, x, y);
        }
        for (p, a) in px.iter_mut().zip(alpha) {
            *p = (*p & 0x00FF_FFFF) | (a as u32) << 24;
        }
    }

    /// The menu's geometry in the current theme.
    fn menu_layout(&mut self) -> MenuLayout {
        let k = self.k;
        let row = 24 * k;
        let pad = 6 * k;
        let napps = self.apps.len() as i32;
        let mut items = Vec::new();
        let m = &self.theme.menu;
        let inset = m.inset * k;
        let col_w = 190 * k;
        match m.side_bg {
            // Luna: header, the apps on the left, the themes in a second column, footer
            Some(_) => {
                let side_w = 150 * k;
                let hh = m.header_h * k;
                let col_h = (napps * row).max(row + THEME_ITEMS.len() as i32 * row) + 2 * pad;
                let w = 2 * inset + col_w + side_w;
                let h = 2 * inset + hh + col_h + m.footer_h * k;
                let top = inset + hh;
                let left = Rect::new(inset, top, col_w, col_h);
                let side = Rect::new(inset + col_w, top, side_w, col_h);
                for (i, a) in self.apps.iter().enumerate() {
                    items.push((Rect::new(left.x + pad, top + pad + i as i32 * row, col_w - 2 * pad, row), a.name.clone(), Item::App(i)));
                }
                for (i, (name, label)) in THEME_ITEMS.iter().enumerate() {
                    let y = top + pad + row + i as i32 * row;
                    items.push((Rect::new(side.x + pad, y, side_w - 2 * pad, row), String::from(*label), Item::Theme(name)));
                }
                MenuLayout {
                    w,
                    h,
                    items,
                    left,
                    side: Some(side),
                    header: Some(Rect::new(inset, inset, w - 2 * inset, hh)),
                    banner: None,
                    footer: Some(Rect::new(inset, top + col_h, w - 2 * inset, m.footer_h * k)),
                    separator: None,
                    side_title: Some((side.x + pad + 8 * k, top + pad)),
                }
            }
            // one column (9x, with its banner on the left): the apps, a line, the themes
            None => {
                let bw = m.banner.map_or(0, |_| m.banner_w * k);
                let x0 = inset + bw;
                let sep_h = 9 * k;
                let col_h = (napps + THEME_ITEMS.len() as i32) * row + sep_h + 2 * pad;
                let (w, h) = (x0 + col_w + inset, 2 * inset + col_h);
                let mut y = inset + pad;
                for (i, a) in self.apps.iter().enumerate() {
                    items.push((Rect::new(x0 + pad, y, col_w - 2 * pad, row), a.name.clone(), Item::App(i)));
                    y += row;
                }
                let separator = Rect::new(x0 + pad, y + sep_h / 2 - k, col_w - 2 * pad, k);
                y += sep_h;
                for (name, label) in THEME_ITEMS {
                    items.push((Rect::new(x0 + pad, y, col_w - 2 * pad, row), String::from(label), Item::Theme(name)));
                    y += row;
                }
                MenuLayout {
                    w,
                    h,
                    items,
                    left: Rect::new(x0, inset, col_w, col_h),
                    side: None,
                    header: None,
                    banner: (bw > 0).then(|| Rect::new(inset, inset, bw, h - 2 * inset)),
                    footer: None,
                    separator: Some(separator),
                    side_title: None,
                }
            }
        }
    }

    /// The menu into `px` (`l.w x l.h`, premultiplied ARGB), transparent where the compositor's frame shows (the inset).
    fn draw_menu(&mut self, px: &mut [u32], l: &MenuLayout) {
        let (w, h, k) = (l.w as usize, l.h as usize, self.k);
        let all = Rect::new(0, 0, l.w, l.h);
        let fill = |px: &mut [u32], r: Rect, c: u32| {
            let Some(r) = r.intersect(&all) else { return };
            for y in r.y..r.bottom() {
                px[y as usize * w + r.x as usize..][..r.w as usize].fill(0xFF00_0000 | c);
            }
        };
        // the backgrounds: the columns may be translucent (Luna's glass shows through), stored premultiplied
        let m = &self.theme.menu;
        px.fill(0);
        let column = |px: &mut [u32], r: Rect, c: u32| {
            let a = c >> 24;
            let p = a << 24 | ((c >> 16 & 255) * a / 255) << 16 | ((c >> 8 & 255) * a / 255) << 8 | (c & 255) * a / 255;
            let Some(r) = r.intersect(&all) else { return };
            for y in r.y..r.bottom() {
                px[y as usize * w + r.x as usize..][..r.w as usize].fill(p);
            }
        };
        column(px, l.left, m.items_bg);
        if let (Some(r), Some(c)) = (l.side, m.side_bg) {
            column(px, r, c);
        }
        if let (Some(r), Some(sh)) = (l.header, m.header) {
            sh.scaled(k).paint(px, w, all, r);
        }
        if let (Some(r), Some(sh)) = (l.footer, m.footer) {
            sh.scaled(k).paint(px, w, all, r);
        }
        if let (Some(r), Some(sh)) = (l.banner, m.banner) {
            sh.scaled(k).paint(px, w, all, r);
        }
        if let Some(r) = l.separator {
            fill(px, r, m.separator);
            fill(px, Rect::new(r.x, r.y + k, r.w, k), 0x00FF_FFFF); // the etched line's light half
        }
        let (items_fg, hover_fg) = (m.items_fg, m.hover_fg);
        let side_fg = m.side_fg;
        // the item under the pointer, and a dot at the current theme
        for (i, (r, _, item)) in l.items.iter().enumerate() {
            if self.menu_hover == Some(i) {
                m.hover.scaled(k).paint(px, w, all, *r);
            }
            if let Item::Theme(name) = item {
                if *name == self.theme.name {
                    let d = 6 * k;
                    let c = if self.menu_hover == Some(i) { hover_fg } else { side_fg };
                    let dot = Rect::new(r.x + 6 * k, r.y + (r.h - d) / 2, d, d);
                    theme::Shape::solid(0xFF00_0000 | c).radius(d as f32 / 2.0).paint(px, w, all, dot);
                }
            }
        }
        // the text, over opaque pixels only: drawing it writes 0x00RRGGBB, so the alpha is put back after
        let alpha: Vec<u8> = px.iter().map(|p| (p >> 24) as u8).collect();
        let size = 13.0 * k as f32;
        let (_, lh) = self.text.measure("Hg", &Style::new(SANS, size), None);
        let mut cv = Canvas::new(px, w, h, w);
        for (i, (r, label, item)) in l.items.iter().enumerate() {
            let in_side = matches!(item, Item::Theme(_)) && l.side.is_some();
            let fg = if self.menu_hover == Some(i) { hover_fg } else if in_side { side_fg } else { items_fg };
            let indent = if matches!(item, Item::Theme(_)) { 18 * k } else { 8 * k };
            self.text.draw(&mut cv, label, &Style::new(SANS, size).color(fg), None, r.x + indent, r.y + (r.h - lh) / 2);
        }
        if let (Some((x, y)), Some(m)) = (l.side_title, Some(m)) {
            self.text.draw(&mut cv, "Tema", &Style::new(SANS, size).color(m.side_fg).bold(), None, x, y + (24 * k - lh) / 2);
        }
        if let (Some(r), Some(m)) = (l.header, Some(m)) {
            let st = Style::new(SANS, 18.0 * k as f32).color(m.header_fg).bold();
            let (_, hl) = self.text.measure("constanos", &st, None);
            self.text.draw(&mut cv, "constanos", &st, None, r.x + 14 * k, r.y + (r.h - hl) / 2);
        }
        drop(cv);
        for (p, a) in px.iter_mut().zip(alpha) {
            *p = (*p & 0x00FF_FFFF) | (a as u32) << 24;
        }
        // 9x: the name down the banner, bottom to top (drawn on its side, then turned)
        if let (Some(r), Some(m)) = (l.banner, Some(m)) {
            let st = Style::new(SANS, 15.0 * k as f32).color(0x00FF_FFFF).bold();
            let (tw, tl) = self.text.measure("constanos", &st, None);
            let (cw, ch) = ((tw + 16 * k) as usize, r.w as usize);
            let mut cov = alloc::vec![0u32; cw * ch];
            let mut c2 = Canvas::new(&mut cov, cw, ch, cw);
            self.text.draw(&mut c2, "constanos", &st, None, 8 * k, (r.w - tl) / 2);
            for ty in 0..ch {
                for tx in 0..cw {
                    let a = (cov[ty * cw + tx] >> 8) & 255;
                    // (tx, ty) on its side -> x = banner.x + ty, y = banner.bottom - 1 - tx
                    let (x, y) = (r.x + ty as i32, r.bottom() - 1 - tx as i32);
                    if a == 0 || y < r.y {
                        continue;
                    }
                    let d = &mut px[y as usize * w + x as usize];
                    let mut out = 0xFF00_0000;
                    for sh in [16, 8, 0] {
                        let (dc, fc) = ((*d >> sh) & 255, (m.banner_fg >> sh) & 255);
                        out |= ((dc * (255 - a) + fc * a + 127) / 255) << sh;
                    }
                    *d = out;
                }
            }
        }
    }

    /// The item at (`x`, `y`) of the menu.
    fn menu_item_at(l: &MenuLayout, x: i32, y: i32) -> Option<usize> {
        l.items.iter().position(|(r, _, _)| r.contains(x, y))
    }

    fn button_at(&mut self, x: i32) -> Option<Action> {
        self.layout().into_iter().find(|b| x >= b.x && x < b.x + b.w).map(|b| b.action)
    }
}

/// `HH:MM`, UTC, and the ms until it next changes.
fn clock() -> (String, i64) {
    let (sec, nsec) = syscall::clock_gettime();
    let day = sec.rem_euclid(86_400);
    let s = alloc::format!("{:02}:{:02}", day / 3600, day / 60 % 60);
    (s, (60 - day % 60) * 1000 - nsec / 1_000_000)
}

fn send(fd: i32, reqs: &[Request]) -> bool {
    let mut e = Encoder::new();
    for r in reqs {
        r.encode(&mut e);
    }
    let (bytes, fds) = e.take();
    syscall::send_fds(fd, &bytes, &fds, 0) == bytes.len() as i64
}

fn connect() -> Option<i32> {
    let fd = syscall::socket(AF_UNIX as i32, SOCK_STREAM, 0) as i32;
    let (addr, alen) = syscall::SockAddrUn::path(b"/tmp/gui-0");
    for _ in 0..40 {
        if syscall::connect(fd, &addr, alen) == 0 {
            return Some(fd);
        }
        syscall::sleep_ms(50);
    }
    None
}

/// Reads events until the next `configure`.
fn wait_configure(fd: i32, dec: &mut Decoder) -> Option<(i32, i32)> {
    let mut buf = [0u8; 512];
    loop {
        while let Ok(Some(msg)) = dec.next_message() {
            if msg.object == SURFACE {
                if let Ok(Event::Configure { width, height, .. }) = Event::decode(Interface::Surface, &msg) {
                    return Some((width, height));
                }
            }
        }
        let n = syscall::recv(fd, &mut buf);
        if n <= 0 {
            return None;
        }
        dec.push_bytes(&buf[..n as usize]);
    }
}

/// The open menu: its popup surface, pool and buffer (fresh ids every time it opens: its size, and so its place above the strip, depend on
/// the theme) and its pixels.
struct OpenMenu {
    sid: u32,
    pool: u32,
    buf: u32,
    base: u64,
    size: u64,
    layout: MenuLayout,
}

impl OpenMenu {
    fn px(&self) -> &'static mut [u32] {
        unsafe { core::slice::from_raw_parts_mut(self.base as *mut u32, (self.layout.w * self.layout.h) as usize) }
    }
}

/// First id of the menu's objects; the menu takes three each time it opens.
const MENU_IDS: u32 = 1000;

/// Opens the menu: a popup right above the strip's left end.
fn open_menu(sock: i32, p: &mut Panel, next_id: &mut u32) -> Option<OpenMenu> {
    let layout = p.menu_layout();
    let size = (layout.w * layout.h * 4) as u64;
    let mfd = syscall::memfd_create(b"panel-menu\0", 0) as i32;
    let base = if mfd >= 0 && syscall::ftruncate(mfd, size) == 0 { syscall::mmap(0, size, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0) } else { -1 };
    if base <= 0 {
        println!("panel: cannot create the menu's pool");
        if mfd >= 0 {
            syscall::close(mfd);
        }
        return None;
    }
    let (sid, pool, buf) = (*next_id, *next_id + 1, *next_id + 2);
    *next_id += 3;
    let m = OpenMenu { sid, pool, buf, base: base as u64, size, layout };
    p.menu_hover = None;
    p.draw_menu(m.px(), &m.layout);
    let (w, h) = (m.layout.w, m.layout.h);
    let ok = send(sock, &[
        Request::CreateSurface { id: sid },
        Request::SetPopup { surface: sid, parent: SURFACE, x: 0, y: -h },
        Request::CreatePool { id: pool, fd: mfd, size: size as u32 },
        Request::CreateBuffer { pool, id: buf, offset: 0, width: w, height: h, stride: w * 4, format: FORMAT_ARGB8888 },
        Request::Attach { surface: sid, buffer: buf },
        Request::Damage { surface: sid, x: 0, y: 0, w, h },
        Request::Commit { surface: sid },
    ]);
    syscall::close(mfd);
    // the items' centres on the screen, for scripts (the popup sits at the strip's left end, right above it)
    let top = p.screen_h - p.h - h;
    let mut desc = String::new();
    for (r, label, _) in &m.layout.items {
        desc.push_str(&alloc::format!(" {}@{},{}", label, r.x + r.w / 2, top + r.y + r.h / 2));
    }
    println!("panel: menu{}", desc);
    ok.then_some(m)
}

/// Shows the menu's pixels again (the pointer moved to another item).
fn redraw_menu(sock: i32, p: &mut Panel, m: &OpenMenu) {
    p.draw_menu(m.px(), &m.layout);
    let (w, h) = (m.layout.w, m.layout.h);
    send(sock, &[
        Request::Attach { surface: m.sid, buffer: m.buf },
        Request::Damage { surface: m.sid, x: 0, y: 0, w, h },
        Request::Commit { surface: m.sid },
    ]);
}

/// Gives the menu's objects back (hiding it first if it still shows).
fn close_menu(sock: i32, m: OpenMenu, hide: bool) {
    if hide {
        send(sock, &[Request::Attach { surface: m.sid, buffer: 0 }, Request::Commit { surface: m.sid }]);
    }
    send(sock, &[Request::DestroyBuffer { buffer: m.buf }, Request::DestroyPool { pool: m.pool }, Request::DestroySurface { surface: m.sid }]);
    syscall::munmap(m.base, m.size);
}

fn main(_args: Args) -> i32 {
    let Some(sock) = connect() else {
        println!("panel: no compositor at /tmp/gui-0");
        return 1;
    };
    syscall::sigaction(syscall::SIGPIPE, 1); // a dead compositor is EPIPE, and our end

    let mut dec = Decoder::new();
    if !send(sock, &[Request::CreateSurface { id: SURFACE }]) {
        return 1;
    }
    // The first configure suggests half the screen: that gives its size,
    // and the scale; the panel role's own configure gives the strip.
    let Some((_, half_h)) = wait_configure(sock, &mut dec) else { return 1 };
    let k = scale_for(half_h * 2);
    if !send(sock, &[Request::SetPanel { surface: SURFACE, height: HEIGHT * k }]) {
        return 1;
    }
    let Some((w, h)) = wait_configure(sock, &mut dec) else {
        println!("panel: the compositor refused the panel role");
        return 1;
    };

    let size = (w * h * 4) as u64;
    let mfd = syscall::memfd_create(b"panel\0", 0) as i32;
    let base = if mfd >= 0 && syscall::ftruncate(mfd, size) == 0 {
        syscall::mmap(0, size, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0)
    } else {
        -1
    };
    if base <= 0 {
        println!("panel: cannot create the pool");
        return 1;
    }
    let px = unsafe { core::slice::from_raw_parts_mut(base as *mut u32, (w * h) as usize) };
    let ok = send(sock, &[
        Request::CreatePool { id: POOL, fd: mfd, size: size as u32 },
        Request::CreateBuffer { pool: POOL, id: BUFFER, offset: 0, width: w, height: h, stride: w * 4, format: FORMAT_ARGB8888 },
        Request::SetTitle { surface: SURFACE, title: "panel".into() },
    ]);
    syscall::close(mfd);
    if !ok {
        return 1;
    }

    let (c, mut next_tick) = clock();
    let mut p = Panel {
        k,
        w,
        h,
        text: Text::load(),
        apps: read_apps(),
        windows: Vec::new(),
        focused: 0,
        theme: theme::THEMES[0],
        menu_open: false,
        screen_h: half_h * 2,
        menu_hover: None,
        hover_x: None,
        clock: c,
        logged: String::new(),
    };
    println!("panel: {}x{} at scale {}, {} apps", w, h, k, p.apps.len());
    next_tick += syscall::uptime_ms();

    let mut dirty = true;
    let mut released = true;
    let mut menu: Option<OpenMenu> = None;
    let mut next_id = MENU_IDS;
    let mut buf = [0u8; 4096];
    loop {
        while syscall::reap_any() > 0 {} // what we launched and has exited
        if dirty && released {
            p.draw(px);
            if !send(sock, &[
                Request::Attach { surface: SURFACE, buffer: BUFFER },
                Request::Damage { surface: SURFACE, x: 0, y: 0, w, h },
                Request::Commit { surface: SURFACE },
            ]) {
                return 0;
            }
            dirty = false;
            released = false;
        }
        let timeout = (next_tick - syscall::uptime_ms()).max(0) as i32;
        let mut pfd = [syscall::PollFd { fd: sock, events: syscall::POLLIN, revents: 0 }];
        if syscall::poll(&mut pfd, timeout) > 0 {
            let n = syscall::recv(sock, &mut buf);
            if n <= 0 {
                println!("panel: compositor gone, bye");
                return 0;
            }
            dec.push_bytes(&buf[..n as usize]);
        }
        if syscall::uptime_ms() >= next_tick {
            let (c, wait) = clock();
            p.clock = c;
            next_tick = syscall::uptime_ms() + wait;
            dirty = true;
        }
        while let Ok(Some(msg)) = dec.next_message() {
            let o = msg.object;
            let iface = match o {
                1 => Interface::Compositor,
                BUFFER => Interface::Buffer,
                SURFACE => Interface::Surface,
                _ if o >= MENU_IDS && (o - MENU_IDS) % 3 == 0 => Interface::Surface,
                _ if o >= MENU_IDS && (o - MENU_IDS) % 3 == 2 => Interface::Buffer,
                _ => Interface::Callback,
            };
            // the menu's own events
            if let Some(m) = &menu {
                if o == m.sid {
                    match Event::decode(iface, &msg) {
                        Ok(Event::PopupDone { .. }) => {
                            println!("panel: menu closed");
                            close_menu(sock, menu.take().unwrap(), false);
                            p.menu_open = false;
                            dirty = true;
                        }
                        Ok(Event::Motion { x, y, .. }) => {
                            let at = Panel::menu_item_at(&m.layout, x, y);
                            if at != p.menu_hover {
                                p.menu_hover = at;
                                redraw_menu(sock, &mut p, m);
                            }
                        }
                        Ok(Event::Button { code: BTN_LEFT, pressed: true, .. }) => {
                            let item = p.menu_hover.map(|i| m.layout.items[i].2);
                            if let Some(item) = item {
                                match item {
                                    Item::App(i) => {
                                        let cmd = p.apps[i].cmd.clone();
                                        let pid = launch::spawn(cmd.as_bytes());
                                        println!("panel: launched {} (pid {})", cmd, pid);
                                    }
                                    Item::Theme(name) => {
                                        println!("panel: theme {} asked", name);
                                        send(sock, &[Request::SetTheme { surface: SURFACE, name: String::from(name) }]);
                                    }
                                }
                                close_menu(sock, menu.take().unwrap(), true);
                                p.menu_open = false;
                                dirty = true;
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
            }
            let Ok(ev) = Event::decode(iface, &msg) else { continue };
            match ev {
                Event::Release { .. } => released = true,
                Event::Toplevel { id, title, .. } => {
                    match p.windows.iter_mut().find(|(i, _)| *i == id) {
                        Some(w) => w.1 = title,
                        None => p.windows.push((id, title)),
                    }
                    dirty = true;
                }
                Event::ToplevelGone { id, .. } => {
                    p.windows.retain(|(i, _)| *i != id);
                    dirty = true;
                }
                Event::Theme { name, .. } => {
                    p.theme = theme::by_name(&name).unwrap_or(theme::THEMES[0]);
                    println!("panel: theme {}", p.theme.name);
                    // the menu's size is the theme's: it closes
                    if let Some(m) = menu.take() {
                        close_menu(sock, m, true);
                        p.menu_open = false;
                    }
                    dirty = true;
                }
                Event::ToplevelFocus { id, .. } => {
                    p.focused = id;
                    dirty = true;
                }
                Event::Motion { x, .. } => {
                    let before = p.button_at(p.hover_x.unwrap_or(-1)).is_some();
                    p.hover_x = Some(x);
                    // Redraw only when the highlight can have moved.
                    if before || p.button_at(x).is_some() {
                        dirty = true;
                    }
                }
                Event::Button { code: BTN_LEFT, pressed: true, .. } => {
                    let action = p.hover_x.and_then(|x| p.button_at(x));
                    // (with the menu open this click never arrives: the compositor closes the menu instead, popup_done)
                    match action {
                        Some(Action::Menu) => {
                            menu = open_menu(sock, &mut p, &mut next_id);
                            p.menu_open = menu.is_some();
                        }
                        Some(Action::Activate(id)) => {
                            send(sock, &[Request::Activate { surface: SURFACE, toplevel: id }]);
                        }
                        None => {}
                    }
                    dirty = true;
                }
                Event::Error { code, message, .. } => {
                    println!("panel: compositor error {}: {}", code, message);
                    return 1;
                }
                _ => {}
            }
        }
    }
}
