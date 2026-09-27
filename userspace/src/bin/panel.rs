#![no_std]
#![no_main]

//! The panel (phase 4.4 of `docs/gui/gui-plan.md`): a strip along the
//! bottom of the screen, a client of the compositor with the panel role
//! (`set_panel`) — undecorated, above every window, outside the work area.
//!
//! From left to right:
//!
//! - **Apps**, the launcher: a click opens the list read from
//!   `/mnt/etc/gui/apps` (one `name<TAB>command` per line) *in the strip
//!   itself*, in place of the window list — the protocol has no pop-up
//!   surfaces — and a click on one starts it (`userspace::launch::spawn`,
//!   as the compositor starts its arguments). A click anywhere else, or on
//!   Apps again, closes the list.
//! - **The window list**: a button per window (the `toplevel*` events the
//!   compositor sends the panel alone), the focused one highlighted; a
//!   click raises and focuses it (`activate`).
//! - **A clock**, `HH:MM` UTC (as `/etc/localtime`), redrawn each minute.
//!
//! Started by `compositor` without arguments. If it dies the compositor
//! goes on without it; it exits when the compositor goes.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use draw::Canvas;
use gui::compositor::scale_for;
use gui::protocol::{Event, Interface, Request, FORMAT_XRGB8888};
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

const BG: u32 = 0x0015_1A22;
const EDGE: u32 = 0x002A_313C;
const BUTTON: u32 = 0x0024_2B36;
const HOVER: u32 = 0x0033_3B48;
const FOCUSED: u32 = 0x003D_5A80;
const OPEN: u32 = 0x0058_A6FF;
const TEXT: u32 = 0x00E6_EDF3;
const DIM: u32 = 0x008B_949E;

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
    Launch(usize),
    Activate(u32),
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
    menu_open: bool,
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
        let st = self.style(TEXT);
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

    /// The buttons, left to right: Apps, then the apps (menu open) or the
    /// windows. Nothing reaches into the clock's space.
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
        let items: Vec<(String, Action)> = if self.menu_open {
            self.apps.iter().enumerate().map(|(i, a)| (a.name.clone(), Action::Launch(i))).collect()
        } else {
            self.windows
                .iter()
                .map(|(id, t)| (if t.is_empty() { String::from("(untitled)") } else { t.clone() }, Action::Activate(*id)))
                .collect()
        };
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
        let (w, h, k) = (self.w, self.h, self.k);
        let buttons = self.layout();
        let mut desc = String::new();
        for b in &buttons {
            desc.push_str(&alloc::format!(" {}@{}+{}", b.label, b.x, b.w));
        }
        if desc != self.logged {
            println!("panel: buttons{}", desc);
            self.logged = desc;
        }
        let st = self.style(TEXT);
        let (_, lh) = self.text.measure("Hg", &st, None);
        let mut cv = Canvas::new(px, w as usize, h as usize, w as usize);
        cv.fill(BG);
        cv.hline(0, 0, w, EDGE);
        let (by, bh) = (4 * k, h - 7 * k);
        for b in &buttons {
            let hover = self.hover_x.is_some_and(|x| x >= b.x && x < b.x + b.w);
            let (bg, fg) = match b.action {
                Action::Menu if self.menu_open => (OPEN, BG),
                Action::Activate(id) if id == self.focused => (FOCUSED, TEXT),
                _ if hover => (HOVER, TEXT),
                _ => (BUTTON, TEXT),
            };
            cv.rect(b.x, by, b.w, bh, bg);
            let st = self.style(fg);
            self.text.draw(&mut cv, &b.label, &st, None, b.x + 12 * k, by + (bh - lh) / 2);
        }
        let st = self.style(DIM);
        let cw = self.text.measure(&self.clock, &st, None).0;
        self.text.draw(&mut cv, &self.clock, &st, None, w - 12 * k - cw, by + (bh - lh) / 2);
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
        Request::CreateBuffer { pool: POOL, id: BUFFER, offset: 0, width: w, height: h, stride: w * 4, format: FORMAT_XRGB8888 },
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
        menu_open: false,
        hover_x: None,
        clock: c,
        logged: String::new(),
    };
    println!("panel: {}x{} at scale {}, {} apps", w, h, k, p.apps.len());
    next_tick += syscall::uptime_ms();

    let mut dirty = true;
    let mut released = true;
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
            let iface = match msg.object {
                1 => Interface::Compositor,
                BUFFER => Interface::Buffer,
                SURFACE => Interface::Surface,
                _ => Interface::Callback,
            };
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
                    match action {
                        Some(Action::Menu) => p.menu_open = !p.menu_open,
                        Some(Action::Launch(i)) => {
                            let cmd = p.apps[i].cmd.clone();
                            let pid = launch::spawn(cmd.as_bytes());
                            println!("panel: launched {} (pid {})", cmd, pid);
                            p.menu_open = false;
                        }
                        Some(Action::Activate(id)) => {
                            send(sock, &[Request::Activate { surface: SURFACE, toplevel: id }]);
                        }
                        None => p.menu_open = false,
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
