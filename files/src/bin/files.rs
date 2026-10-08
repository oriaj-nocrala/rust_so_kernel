//! Files v1: places, a path bar, the folder as a virtualized list (name, size, type, modified), and an inspector with the selection's
//! details and preview (`docs/gui/files-plan.md`).
//!
//!     files [PATH]        (under the compositor: from its command line, the launcher, or term)
//!
//! - Keys: arrows/Page/Home/End and typing move the selection (the `ui` list), Enter opens (a folder here, a file with its app from
//!   `/mnt/etc/gui/open`), Backspace goes up, Space shows the preview over the whole window (Space or Esc again to go back), Tab moves
//!   between the path bar, the places and the list. Mouse: click, double click, drag the splits.
//! - Previews come from `files-preview`, started through `cap-exec` with only the file (read) and an output memfd (write): a decoder
//!   that crashes or hangs costs that process, and the inspector says what happened (P1.1, P6.4). Where the preview goes (right of the
//!   list, or under it for a folder of pictures) is decided per folder, never per selection (P2.3).
//! - Sizes, dates and permissions are read only for the rows shown.
//! - Every step is logged on stdout (`files: ...`); `scripts/gui-e2e.sh files` reads it and the semantic tree.

use std::cell::RefCell;
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use files::entry::{self, Entry, Kind, Layout};
use files::open_with;
use files::preview::{self, Body};
use gui_client::{Event, Window};
use ui::render::{Painter, FONT_FILES};
use ui::{Action, Column, Font, Input, List, ListStyle, Row, Size, State, Widget};

const FONT_DIR: &str = "/mnt/usr/share/fonts";
const OPEN_TABLE: &str = "/mnt/etc/gui/open";
const PLACES: [&str; 5] = ["/", "/mnt", "/tmp", "/proc", "/dev"];
/// The box previews are fitted in.
const PREVIEW_W: usize = 360;
const PREVIEW_H: usize = 360;
/// A selection held this long gets its preview (arrowing through a folder starts no processes).
const SETTLE: Duration = Duration::from_millis(120);
/// A provider that has not answered in this long is stopped (`FILES_PREVIEW_TIMEOUT_MS` overrides it, for tests).
const TIMEOUT_MS: u64 = 5000;

const UP: u32 = 2;
const PATH: u32 = 3;
const SPLIT: u32 = 4;
const PLACES_PANE: u32 = 5;
const PLACES_LIST: u32 = 6;
const MAIN: u32 = 7;
const LIST: u32 = 8;
const INSPECT_SCROLL: u32 = 9;
const INSPECTOR: u32 = 10;
const STATUS: u32 = 11;
const QUICK_SCROLL: u32 = 12;
const QUICK: u32 = 13;
const NAME: u32 = 20;
const LINES: u32 = 1000;

const KEY_ESC: u32 = 1;
const KEY_BACKSPACE: u32 = 14;
const KEY_SPACE: u32 = 57;

const COLS: [Column; 4] = [
    Column { title: "Name", width: Size::Fill, right: false },
    Column { title: "Size", width: Size::Fixed(64), right: true },
    Column { title: "Type", width: Size::Fixed(92), right: false },
    Column { title: "Modified", width: Size::Fixed(122), right: false },
];

#[derive(Clone, Copy)]
struct Meta {
    size: u64,
    mtime: i64,
    mode: u32,
}

struct Folder {
    path: String,
    entries: Vec<Entry>,
    /// `stat`ed when first shown: `Some(None)` when it failed.
    meta: RefCell<Vec<Option<Option<Meta>>>>,
    layout: Layout,
}

impl Folder {
    fn meta(&self, i: usize) -> Option<Meta> {
        let mut m = self.meta.borrow_mut();
        *m[i].get_or_insert_with(|| {
            std::fs::symlink_metadata(entry::join(&self.path, &self.entries[i].name))
                .ok()
                .map(|md| Meta { size: md.len(), mtime: md.mtime(), mode: md.mode() })
        })
    }

    fn row(&self, i: usize) -> Row {
        let e = &self.entries[i];
        let m = self.meta(i);
        let size = match (e.kind, m) {
            (Kind::File, Some(m)) => entry::size(m.size),
            _ => String::new(),
        };
        let modified = m.map_or(String::new(), |m| entry::time(m.mtime));
        // the key: the name's hash, so a row keeps its node id when the folder changes around it
        let key = e.name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
        Row { key, cells: vec![e.name.clone(), size, entry::type_name(&e.name, e.kind), modified] }
    }
}

/// What the inspector shows for the selection.
enum Shown {
    Nothing,
    Waiting,
    Ready { name: String, meta: Vec<(String, String)>, body: Shot },
    Failed { name: String, why: String },
}

enum Shot {
    /// Already over the field's colour, `0x00RRGGBB`.
    Image { w: usize, h: usize, px: Vec<u32> },
    Text(Vec<String>),
    None(String),
}

struct Job {
    child: Child,
    name: String,
    out: File,
    started: Instant,
}

struct App {
    folder: Folder,
    status: String,
    shown: Shown,
    job: Option<Job>,
    /// The selection to preview once it settles.
    pending: Option<(usize, Instant)>,
    quick: bool,
    dir: std::path::PathBuf,
    open: Vec<(String, String)>,
    timeout: Duration,
    /// Apps started with "open", reaped as they end.
    apps: Vec<Child>,
}

extern "C" {
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    fn dup2(old: i32, new: i32) -> i32;
}
const F_DUPFD_CLOEXEC: i32 = 1030;

fn read_folder(path: &str) -> std::io::Result<Folder> {
    let mut entries = Vec::new();
    for d in std::fs::read_dir(path)? {
        let d = d?;
        let kind = match d.file_type() {
            Ok(t) if t.is_dir() => Kind::Dir,
            Ok(t) if t.is_file() => Kind::File,
            Ok(t) if t.is_symlink() => Kind::Link,
            _ => Kind::Other,
        };
        entries.push(Entry { name: d.file_name().to_string_lossy().into_owned(), kind });
    }
    entry::sort(&mut entries);
    let layout = entry::layout(&entries);
    let n = entries.len();
    Ok(Folder { path: path.into(), entries, meta: RefCell::new(vec![None; n]), layout })
}

fn signal_name(s: i32) -> &'static str {
    match s {
        6 => "SIGABRT",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        15 => "SIGTERM",
        4 => "SIGILL",
        7 => "SIGBUS",
        8 => "SIGFPE",
        _ => "a signal",
    }
}

impl App {
    fn navigate(&mut self, st: &mut State, path: &str, select: Option<&str>) {
        let path = if path.len() > 1 { path.trim_end_matches('/') } else { path };
        match read_folder(path) {
            Ok(f) => {
                self.folder = f;
                let row = select.and_then(|n| entry::position(&self.folder.entries, n)).or(if self.folder.entries.is_empty() { None } else { Some(0) });
                st.select(LIST, row);
                st.set_text(PATH, path);
                st.select(PLACES_LIST, PLACES.iter().position(|p| *p == path));
                self.status = format!("{} — {} items", path, self.folder.entries.len());
                let layout = if self.folder.layout == Layout::Right { "right" } else { "bottom" };
                println!("files: at {} ({} items, layout {})", path, self.folder.entries.len(), layout);
                self.quick = false;
                self.selected(row);
            }
            Err(e) => {
                self.status = format!("cannot open {}: {}", path, e);
                println!("files: {}", self.status);
                st.set_text(PATH, &self.folder.path.clone());
            }
        }
    }

    /// The selection moved to `row`: the inspector waits for it to settle.
    fn selected(&mut self, row: Option<usize>) {
        self.stop_job();
        self.pending = row.map(|r| (r, Instant::now()));
        self.shown = match row {
            Some(_) => Shown::Waiting,
            None => Shown::Nothing,
        };
    }

    fn stop_job(&mut self) {
        if let Some(mut j) = self.job.take() {
            let _ = j.child.kill();
            let _ = j.child.wait();
        }
    }

    fn start_preview(&mut self, row: usize) {
        let e = self.folder.entries[row].clone();
        if e.kind != Kind::File {
            self.shown = Shown::Ready { name: e.name.clone(), meta: vec![], body: Shot::None(format!("{}: no preview", entry::type_name(&e.name, e.kind))) };
            return;
        }
        let path = entry::join(&self.folder.path, &e.name);
        match self.spawn_provider(&path, &e.name) {
            Ok(j) => {
                println!("files: preview {}: files-preview started (pid {})", e.name, j.child.id());
                self.job = Some(j);
            }
            Err(why) => {
                println!("files: preview {}: failed: {}", e.name, why);
                self.shown = Shown::Failed { name: e.name, why };
            }
        }
    }

    /// `cap-exec` with the file on fd 3 (read) and a fresh memfd on fd 4 (write) running `files-preview`.
    fn spawn_provider(&self, path: &str, name: &str) -> Result<Job, String> {
        let file = File::open(path).map_err(|e| format!("cannot open {}: {}", path, e))?;
        let mfd = gui_client::sys::memfd(c"files-preview").map_err(|e| format!("memfd: {}", e))?;
        let out = File::from(unsafe { OwnedFd::from_raw_fd(mfd) });
        out.set_len(preview::capacity(PREVIEW_W, PREVIEW_H) as u64).map_err(|e| format!("sizing the memfd: {}", e))?;
        // above 3 and 4, so the dup2s below cannot hit one of them
        let hi = |fd: i32| unsafe { fcntl(fd, F_DUPFD_CLOEXEC, 100) };
        let (a, b) = (hi(file.as_raw_fd()), hi(out.as_raw_fd()));
        if a < 0 || b < 0 {
            return Err("cannot duplicate the descriptors".into());
        }
        let (a, b) = unsafe { (OwnedFd::from_raw_fd(a), OwnedFd::from_raw_fd(b)) };
        let (ra, rb) = (a.as_raw_fd(), b.as_raw_fd());
        let mut cmd = Command::new(self.dir.join("cap-exec"));
        cmd.args(["--fd", "3:read+seek+fstat", "--fd", "4:write+seek+fstat", "--"])
            .arg(self.dir.join("files-preview"))
            .args([PREVIEW_W.to_string(), PREVIEW_H.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // SAFETY: dup2 is async-signal-safe; nothing else runs between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                if dup2(ra, 3) < 0 || dup2(rb, 4) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().map_err(|e| format!("cannot start cap-exec: {}", e))?;
        drop((a, b));
        Ok(Job { child, name: name.into(), out, started: Instant::now() })
    }

    /// Checks the running provider: done, failed, or too slow. True when the inspector changed.
    fn poll_job(&mut self, face: u32) -> bool {
        let Some(j) = self.job.as_mut() else { return false };
        let status = match j.child.try_wait() {
            Ok(Some(s)) => s,
            Ok(None) if j.started.elapsed() < self.timeout => return false,
            Ok(None) => {
                let _ = j.child.kill();
                let _ = j.child.wait();
                let j = self.job.take().unwrap();
                let why = format!("files-preview did not answer in {} ms; it was stopped", self.timeout.as_millis());
                println!("files: preview {}: failed: {}", j.name, why);
                self.shown = Shown::Failed { name: j.name, why };
                return true;
            }
            Err(e) => {
                let j = self.job.take().unwrap();
                self.shown = Shown::Failed { name: j.name, why: format!("waiting for files-preview: {}", e) };
                return true;
            }
        };
        let mut j = self.job.take().unwrap();
        let mut err = String::new();
        if let Some(mut e) = j.child.stderr.take() {
            let _ = e.read_to_string(&mut err);
        }
        for l in err.lines() {
            println!("files: files-preview said: {}", l);
        }
        let last = err.lines().last().unwrap_or("").trim_start_matches("files-preview: ").to_string();
        let failed = |why: String| Shown::Failed { name: j.name.clone(), why };
        self.shown = if let Some(sig) = status.signal() {
            failed(format!("files-preview was killed by signal {} ({})", sig, signal_name(sig)))
        } else if !status.success() {
            failed(format!("files-preview failed (exit {}){}", status.code().unwrap_or(-1), if last.is_empty() { String::new() } else { format!(": {}", last) }))
        } else {
            let mut buf = vec![0u8; preview::capacity(PREVIEW_W, PREVIEW_H)];
            match j.out.read_exact_at(&mut buf, 0).map_err(|e| e.to_string()).and_then(|_| preview::decode(&buf, PREVIEW_W, PREVIEW_H)) {
                Err(e) => failed(format!("files-preview's answer was refused: {}", e)),
                Ok(p) => {
                    let body = match p.body {
                        Body::Image { w, h, px } => Shot::Image { w, h, px: px.into_iter().map(|c| over(face, c)).collect() },
                        Body::Text(l) => Shot::Text(l),
                        Body::None(why) => Shot::None(why),
                    };
                    Shown::Ready { name: j.name.clone(), meta: p.meta, body }
                }
            }
        };
        match &self.shown {
            Shown::Ready { name, body: Shot::Image { w, h, .. }, .. } => println!("files: preview {}: image {}x{}", name, w, h),
            Shown::Ready { name, body: Shot::Text(l), .. } => println!("files: preview {}: text, {} lines", name, l.len()),
            Shown::Ready { name, body: Shot::None(why), .. } => println!("files: preview {}: none: {}", name, why),
            Shown::Failed { name, why } => println!("files: preview {}: failed: {}", name, why),
            _ => {}
        }
        true
    }

    fn open(&mut self, st: &mut State, row: usize) {
        let e = self.folder.entries[row].clone();
        let path = entry::join(&self.folder.path, &e.name);
        let is_dir = e.kind == Kind::Dir || (e.kind == Kind::Link && std::fs::metadata(&path).is_ok_and(|m| m.is_dir()));
        if is_dir {
            self.navigate(st, &path, None);
            return;
        }
        let Some(cmd) = open_with::command(&self.open, &e.name).map(String::from) else {
            self.status = format!(
                "no app opens {} (add one to {}); Space shows its preview",
                entry::type_name(&e.name, e.kind),
                OPEN_TABLE
            );
            println!("files: {}", self.status);
            return;
        };
        let mut words = cmd.split_whitespace();
        let prog = words.next().unwrap_or("");
        let local = self.dir.join(prog);
        let prog = if !prog.contains('/') && local.exists() { local } else { prog.into() };
        match Command::new(&prog).args(words).arg(&path).stdin(Stdio::null()).spawn() {
            Ok(c) => {
                println!("files: opened {} with {} (pid {})", path, cmd, c.id());
                self.apps.push(c);
                self.status = format!("opened {} with {}", e.name, cmd);
            }
            Err(err) => {
                self.status = format!("cannot start {} for {}: {}", prog.display(), e.name, err);
                println!("files: {}", self.status);
            }
        }
    }

    fn act(&mut self, st: &mut State, a: Action) -> bool {
        match a {
            Action::Selected { list: LIST, row } => self.selected(Some(row)),
            Action::Activated { list: LIST, row } => self.open(st, row),
            Action::Selected { list: PLACES_LIST, row } | Action::Activated { list: PLACES_LIST, row } => {
                if self.folder.path != PLACES[row] {
                    self.navigate(st, PLACES[row], None);
                }
            }
            Action::Clicked(UP) => self.up(st),
            Action::Submitted(PATH) => {
                let p = st.text(PATH).trim().to_string();
                self.navigate(st, &p, None);
                st.set_focus(Some(LIST));
            }
            Action::Key { code: KEY_BACKSPACE, focus: Some(LIST) } => self.up(st),
            Action::Key { code: KEY_SPACE, focus: Some(LIST) } => {
                self.quick = !self.quick;
                println!("files: quick look {}", if self.quick { "on" } else { "off" });
            }
            Action::Key { code: KEY_ESC, .. } if self.quick => {
                self.quick = false;
                println!("files: quick look off");
            }
            _ => return false,
        }
        true
    }

    fn up(&mut self, st: &mut State) {
        let from = self.folder.path.clone();
        if from == "/" {
            return;
        }
        let name = from.rsplit('/').next().unwrap_or("").to_string();
        self.navigate(st, &entry::parent(&from), Some(&name));
    }
}

/// Premultiplied `src` over the opaque `dst`.
fn over(dst: u32, src: u32) -> u32 {
    let a = src >> 24;
    let ch = |s: u32| (((dst >> s) & 0xff) * (255 - a) / 255 + ((src >> s) & 0xff)).min(255) << s;
    ch(16) | ch(8) | ch(0)
}

/// Labels for the inspector: built before the tree, which borrows them.
struct Texts {
    name: String,
    lines: Vec<String>,
    info: Vec<String>,
}

fn texts(app: &App, st: &State) -> Texts {
    let mut info = Vec::new();
    let mut lines = Vec::new();
    let mut name = String::new();
    if let Some(r) = st.selected(LIST).filter(|&r| r < app.folder.entries.len()) {
        let e = &app.folder.entries[r];
        name = e.name.clone();
        info.push(entry::type_name(&e.name, e.kind));
        if let Some(m) = app.folder.meta(r) {
            if e.kind == Kind::File {
                info.push(format!("{} ({} bytes)", entry::size(m.size), m.size));
            }
            info.push(format!("Modified {} UTC", entry::time(m.mtime)));
            info.push(entry::mode(m.mode));
        }
    }
    match &app.shown {
        Shown::Nothing => {}
        Shown::Waiting => info.push("Preview: …".into()),
        Shown::Ready { meta, body, .. } => {
            for (k, v) in meta {
                info.push(format!("{}: {}", k, v));
            }
            match body {
                Shot::Text(l) => lines = l.clone(),
                Shot::None(why) => info.push(format!("Preview: {}", why)),
                Shot::Image { .. } => {}
            }
        }
        Shown::Failed { why, .. } => info.push(format!("Preview failed: {}", why)),
    }
    Texts { name, lines, info }
}

fn preview_widgets<'a>(app: &'a App, t: &'a Texts, base: u32) -> Vec<(Size, Widget<'a>)> {
    let mut v: Vec<(Size, Widget)> = Vec::new();
    if let Shown::Ready { body: Shot::Image { w, h, px }, .. } = &app.shown {
        v.push((Size::Auto, Widget::Image { id: base, name: "preview", pixels: px, w: *w, h: *h }));
    }
    for (i, l) in t.lines.iter().enumerate() {
        v.push((Size::Auto, Widget::Label { id: base + 1 + i as u32, text: l, font: Font::Mono }));
    }
    v
}

fn tree<'a>(app: &'a App, t: &'a Texts, row: &'a dyn Fn(usize) -> Row, place: &'a dyn Fn(usize) -> Row) -> Widget<'a> {
    let places = Widget::pane(
        PLACES_PANE,
        "Places",
        Widget::List(List { id: PLACES_LIST, name: "Places", len: PLACES.len(), columns: &[], style: ListStyle::Sidebar, row: place }),
    );
    let main: Widget = if app.quick {
        let mut col = vec![(Size::Auto, Widget::Label { id: NAME, text: &t.name, font: Font::SansBold })];
        col.extend(preview_widgets(app, t, LINES));
        Widget::scroll(QUICK_SCROLL, Widget::pane(QUICK, "Preview", Widget::Column(col)))
    } else {
        let list = Widget::List(List { id: LIST, name: "Files", len: app.folder.entries.len(), columns: &COLS, style: ListStyle::Table, row });
        let mut col = vec![(Size::Auto, Widget::Label { id: NAME, text: &t.name, font: Font::SansBold })];
        for (i, s) in t.info.iter().enumerate() {
            col.push((Size::Auto, Widget::label(NAME + 1 + i as u32, s)));
        }
        col.push((Size::Fixed(6), Widget::Space));
        col.extend(preview_widgets(app, t, LINES));
        let inspector = Widget::scroll(INSPECT_SCROLL, Widget::pane(INSPECTOR, "Inspector", Widget::Column(col)));
        match app.folder.layout {
            Layout::Right => Widget::split(MAIN, list, inspector, 440, 120),
            Layout::Bottom => Widget::vsplit(MAIN, list, inspector, 150, 60),
        }
    };
    Widget::Column(vec![
        (
            Size::Auto,
            Widget::Row(vec![
                (Size::Auto, Widget::button(UP, "Up")),
                (Size::Fixed(6), Widget::Space),
                (Size::Fill, Widget::field(PATH, "Location", "a folder's path")),
            ]),
        ),
        (Size::Fixed(4), Widget::Space),
        (Size::Fill, Widget::split(SPLIT, places, main, 110, 60)),
        (Size::Auto, Widget::label(STATUS, &app.status)),
    ])
}

fn run() -> std::io::Result<()> {
    let start = std::env::args().nth(1).unwrap_or_else(|| "/".into());
    let mut fonts = Vec::new();
    for f in FONT_FILES {
        let path = format!("{}/{}", FONT_DIR, f);
        fonts.push(std::fs::read(&path).map_err(|e| std::io::Error::new(e.kind(), format!("cannot read the font {}: {}", path, e)))?);
    }
    let mut p = Painter::new(fonts, 15.0).ok_or_else(|| std::io::Error::other("the font files are not fonts"))?;
    let exe = std::env::current_exe()?;
    let dir = exe.parent().map(|d| d.to_path_buf()).unwrap_or_else(|| "/mnt/bin".into());
    let open = std::fs::read_to_string(OPEN_TABLE).map(|t| open_with::parse(&t)).unwrap_or_default();
    let timeout = Duration::from_millis(std::env::var("FILES_PREVIEW_TIMEOUT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(TIMEOUT_MS));
    let mut win = Window::open("Files", Some((780, 470)))?;
    win.set_resizable(360, 240)?;
    let theme = &gui::theme::LUNA;
    let face = theme.widgets.face;
    let mut app = App {
        folder: Folder { path: String::new(), entries: vec![], meta: RefCell::new(vec![]), layout: Layout::Right },
        status: String::new(),
        shown: Shown::Nothing,
        job: None,
        pending: None,
        quick: false,
        dir,
        open,
        timeout,
        apps: Vec::new(),
    };
    let mut st = State::new(theme, 1);
    app.navigate(&mut st, &start, None);
    if app.folder.path.is_empty() {
        app.navigate(&mut st, "/", None);
    }
    st.set_focus(Some(LIST));
    let t0 = Instant::now();
    let mut first = true;
    // drawn again only when something changed: an idle window costs nothing
    let mut dirty = true;
    loop {
        let (w, h) = win.size();
        if dirty {
            let t = texts(&app, &st);
            let folder = &app.folder;
            let row = |i| folder.row(i);
            let place = |i: usize| Row { key: i as u64, cells: vec![PLACES[i].into()] };
            let tr = tree(&app, &t, &row, &place);
            let title = format!("Files: {}", app.folder.path);
            let frame = st.render(&title, &tr, (w as i32, h as i32), &mut p);
            p.paint(&frame.paint, win.frame()?, w, h);
            win.set_semantics(&frame.nodes)?;
            win.present()?;
            dirty = false;
        }
        if first {
            println!("files: ready {}x{} (pid {})", w, h, std::process::id());
            first = false;
        }
        // previews and apps run while we wait for input
        let busy = app.job.is_some() || app.pending.is_some();
        let ev = win.next_event(Some(if busy { Duration::from_millis(40) } else { Duration::from_millis(1000) }))?;
        app.apps.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_))));
        if let Some((row, at)) = app.pending {
            if at.elapsed() >= SETTLE {
                app.pending = None;
                app.start_preview(row);
                dirty = true;
            }
        }
        dirty |= app.poll_job(face);
        let Some(ev) = ev else { continue };
        dirty = true;
        let input = match ev {
            Event::Close => break,
            Event::Resize { width, height } => {
                win.resize(width, height)?;
                continue;
            }
            Event::Key { code, pressed } => Input::Key { code, pressed },
            Event::Motion { x, y } => Input::Motion { x, y },
            Event::Button { code, pressed } => Input::Button { code, pressed },
            Event::Focus(f) => Input::Focus(f),
            // the wheel's notches up are the list's rows up
            Event::Wheel { steps } => Input::Wheel { dy: -steps },
            Event::RelativeMotion { .. } => continue,
        };
        let now = t0.elapsed().as_millis() as u32;
        let actions = {
            let t = texts(&app, &st);
            let folder = &app.folder;
            let row = |i| folder.row(i);
            let place = |i: usize| Row { key: i as u64, cells: vec![PLACES[i].into()] };
            let tr = tree(&app, &t, &row, &place);
            st.handle(&tr, (w as i32, h as i32), input, now, &mut p)
        };
        for a in actions {
            app.act(&mut st, a);
        }
    }
    app.stop_job();
    println!("files: bye");
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("files: {}", e);
            ExitCode::FAILURE
        }
    }
}
