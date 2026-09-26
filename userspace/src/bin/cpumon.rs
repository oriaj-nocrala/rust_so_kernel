//! A CPU monitor: one live graph per CPU, split into user and system time,
//! plus total load, memory, the load averages and the busiest processes
//! (with their resident set).
//!
//! Everything it shows comes from the files a Linux monitor reads, in the
//! formats Linux writes them — `/proc/stat` (per-CPU ticks, parsed with
//! the same `sched::cputime` code the kernel renders it with),
//! `/proc/<pid>/stat`, `/proc/meminfo`, `/proc/loadavg`, `/proc/uptime`,
//! `/proc/cpuinfo` — so it is also a running check that they say something
//! true. A sample every half second; the picture is a 640x460 layout drawn
//! with `draw`, its text proportional Noto Sans through `userspace::text`
//! (parley + swash; `draw::smooth`'s bitmap Noto Mono without the fonts on
//! `/mnt`) at the screen's scale (`gfx::HIDPI`: 1280x920 on 1080p, fonts
//! rasterised at that size, never replicated), and shown through
//! `userspace::gfx`: a window under the
//! compositor, the whole screen on the console. Esc or Q quits.
//!
//! Disk-resident (`DISK_RUST_PROGRAMS`): the text engine makes it ~1.7 MB.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use draw::color::{hsv, mix, scale};
use draw::Canvas;
use sched::cputime::{parse_stat_line, permille, CpuTimes};
use userspace::args::Args;
use userspace::gfx::{Gfx, EV_KEY, HIDPI};
use userspace::text::{Style, Text, SANS};
use userspace::{entry, println, syscall};

entry!(main);

const W: usize = 640;
const H: usize = 460;
/// Samples of history kept per graph (a minute at `PERIOD_MS`).
const HIST: usize = 120;
const PERIOD_MS: i64 = 500;
/// Processes listed.
const TOP_N: usize = 5;

const KEY_ESC: u16 = 1;
const KEY_Q: u16 = 16;

// ── Palette ──────────────────────────────────────────────────────────────

const BG: u32 = 0x0D1117;
const PANEL: u32 = 0x161B22;
const EDGE: u32 = 0x2A313C;
const GRID: u32 = 0x21262D;
const TEXT: u32 = 0xE6EDF3;
const DIM: u32 = 0x8B949E;
const ACCENT: u32 = 0x58A6FF;
const SYS: u32 = 0xB48CFF;

const SMALL: Font = Font::Small;
const SMALL_BOLD: Font = Font::SmallBold;
const TITLE: Font = Font::Title;

// ── Text ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Font {
    Small,
    SmallBold,
    Title,
}

/// Text through `userspace::text`. The layout below was drawn for
/// `draw::smooth`'s cells (16 and 20 px tall, `y` their top), so every
/// string is centred on the cell it used to fill: `dy` shifts the line
/// box up by half of what it is taller — 0 with the bitmap fallback,
/// whose line box *is* the cell.
///
/// Everything is drawn at the window's scale `k` (`gfx::HIDPI`): the
/// layout is in 640x460 logical pixels, multiplied by `k` on the way to
/// the canvas, and the fonts are rasterised at `k` times their size —
/// not drawn at 14 px and then replicated, which is what made them look
/// pixelated.
struct Ui {
    t: Text,
    k: i32,
    /// Per font: line height, and the shift from cell top to line top,
    /// in canvas pixels.
    line: [(i32, i32); 3],
}

impl Ui {
    fn new(k: i32) -> Ui {
        let mut ui = Ui { t: Text::load(), k, line: [(0, 0); 3] };
        for (f, cell) in [(Font::Small, 16), (Font::SmallBold, 16), (Font::Title, 20)] {
            let st = ui.style(f, 0);
            let (_, h) = ui.t.measure("Hg", &st, None);
            ui.line[f as usize] = (h, (cell * k - h) / 2);
        }
        ui
    }

    fn style(&self, f: Font, color: u32) -> Style<'static> {
        let k = self.k as f32;
        let st = match f {
            Font::Small => Style::new(SANS, 14.0 * k),
            Font::SmallBold => Style::new(SANS, 14.0 * k).bold(),
            Font::Title => Style::new(SANS, 18.0 * k).bold(),
        };
        st.color(color)
    }

    /// Draws `s` at `(x, y)` (the top of its old cell); returns the x just
    /// past it, for continuing on the same line.
    /// Coordinates here and in the other text methods are canvas pixels.
    fn text(&mut self, cv: &mut Canvas, f: Font, x: i32, y: i32, s: &str, c: u32) -> i32 {
        let st = self.style(f, c);
        let (w, _) = self.t.draw(cv, s, &st, None, x, y + self.line[f as usize].1);
        x + w
    }

    /// `text` right-aligned: it ends at `right`.
    fn right(&mut self, cv: &mut Canvas, f: Font, right: i32, y: i32, s: &str, c: u32) -> i32 {
        let w = self.width(f, s);
        self.text(cv, f, right - w, y, s, c)
    }

    fn width(&mut self, f: Font, s: &str) -> i32 {
        let st = self.style(f, 0);
        self.t.measure(s, &st, None).0
    }

    fn height(&self, f: Font) -> i32 {
        self.line[f as usize].0
    }

    /// `s` cut to fit `max` pixels, with an ellipsis if it was cut (three
    /// dots with the bitmap fallback, which has no `…`).
    fn fit(&mut self, f: Font, s: &str, max: i32) -> String {
        if self.width(f, s) <= max {
            return String::from(s);
        }
        let dots = if self.t.fonts() { "…" } else { "..." };
        let mut cut: String = s.into();
        while !cut.is_empty() {
            cut.pop();
            let t = format!("{}{}", cut.trim_end(), dots);
            if self.width(f, &t) <= max {
                return t;
            }
        }
        String::new()
    }
}

/// User time's colour: green when light, through yellow, to orange-red at
/// 100% (hue 512 down to 64; system time is violet, so the two never meet).
fn load_color(pm: u32) -> u32 {
    hsv(512 - (pm.min(1000) as i32) * 448 / 1000, 170, 235)
}

// ── Reading /proc ────────────────────────────────────────────────────────

/// The whole of `path` into `buf` (cleared first). False if it can't be
/// opened.
fn read_file(path: &str, buf: &mut Vec<u8>) -> bool {
    buf.clear();
    let fd = syscall::with_cstr(path, |p| syscall::open(p, syscall::O_RDONLY)) as i32;
    if fd < 0 {
        return false;
    }
    let mut chunk = [0u8; 4096];
    loop {
        let n = syscall::read(fd, &mut chunk);
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
    }
    syscall::close(fd);
    true
}

fn text_of(buf: &[u8]) -> &str {
    core::str::from_utf8(buf).unwrap_or("")
}

/// The number after `key` in a `/proc/meminfo`-style "Key:   123 kB" line.
fn field_kb(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find(|l| l.starts_with(key))
        .and_then(|l| l[key.len()..].split_ascii_whitespace().next())
        .and_then(|v| v.parse().ok())
}

/// Every pid in `/proc` (its numeric entries).
fn pids(buf: &mut [u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let fd = syscall::with_cstr("/proc", |p| syscall::open(p, syscall::O_RDONLY)) as i32;
    if fd < 0 {
        return out;
    }
    loop {
        let n = syscall::getdents64(fd, buf);
        if n <= 0 {
            break;
        }
        let mut off = 0;
        while off < n as usize {
            let Some(d) = syscall::parse_dirent(&buf[off..n as usize]) else { break };
            if let Some(pid) = core::str::from_utf8(d.name).ok().and_then(|s| s.parse().ok()) {
                out.push(pid);
            }
            off += d.record_len;
        }
    }
    syscall::close(fd);
    out
}

/// `(comm, utime + stime, last CPU, rss in pages)` from a
/// `/proc/<pid>/stat` line.
fn parse_pid_stat(line: &str) -> Option<(String, u64, usize, u64)> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    let comm = String::from(&line[open + 1..close]);
    // Field 3 (state) is the first after ") "; utime is 14, stime 15,
    // rss 24, processor 39.
    let f: Vec<&str> = line[close + 2..].split_ascii_whitespace().collect();
    let num = |k: usize| f.get(k - 3).and_then(|v| v.parse::<u64>().ok());
    Some((comm, num(14)? + num(15)?, num(39).unwrap_or(0) as usize, num(24).unwrap_or(0)))
}

/// `(processor, MHz)` for every block of a `/proc/cpuinfo`: the frequency
/// each core last ran at, where the kernel measures it (see `cpu MHz` in
/// `fs::procfs::render_cpuinfo`).
fn parse_cpu_mhz(text: &str) -> Vec<(usize, u32)> {
    let mut out = Vec::new();
    let mut cpu = None;
    for line in text.lines() {
        let Some((k, v)) = line.split_once(':') else { continue };
        match k.trim() {
            "processor" => cpu = v.trim().parse().ok(),
            "cpu MHz" => {
                if let (Some(c), Some(m)) = (cpu, v.trim().split('.').next().and_then(|m| m.parse().ok())) {
                    out.push((c, m));
                }
            }
            _ => {}
        }
    }
    out
}

/// One value from a `/proc/sensors` (`chip<TAB>type<TAB>label<TAB>value`
/// per line, see `hal::k10temp::render` and `hal::amd_power::
/// render_energy`). `None` where the machine has no such sensor.
fn sensor(text: &str, kind: &str, label: &str) -> Option<i64> {
    text.lines().find_map(|l| {
        let mut f = l.split('\t');
        let (_chip, k, name, v) = (f.next()?, f.next()?, f.next()?, f.next()?);
        if k == kind && name == label { v.trim().parse().ok() } else { None }
    })
}

/// `45.3 C` for millidegrees (the console font has no degree sign).
fn celsius(mdeg: i32) -> String {
    let t = (mdeg + if mdeg < 0 { -50 } else { 50 }) / 100;
    format!("{}{}.{} C", if t < 0 { "-" } else { "" }, t.abs() / 10, t.abs() % 10)
}

/// `4.62 GHz` / `4.62` for a frequency in MHz.
fn ghz(mhz: u32, unit: bool) -> String {
    let s = format!("{}.{:02}", mhz / 1000, mhz % 1000 / 10);
    if unit { format!("{} GHz", s) } else { s }
}

/// What to call a process, as `ps` does: its command line (arguments
/// NUL-separated in `/proc/<pid>/cmdline`, the program by its basename),
/// or `[comm]` when there is none.
fn command_of(cmdline: &[u8], comm: &str) -> String {
    let mut out = String::new();
    for (i, arg) in cmdline.split(|&b| b == 0).filter(|a| !a.is_empty()).enumerate() {
        let arg = core::str::from_utf8(arg).unwrap_or("?");
        if i == 0 {
            out.push_str(arg.rsplit('/').next().unwrap_or(arg));
        } else {
            out.push(' ');
            out.push_str(arg);
        }
    }
    if out.is_empty() {
        out = format!("[{}]", comm);
    }
    out
}

// ── Sampled state ────────────────────────────────────────────────────────

/// One graph's history: user and system time per sample, in permille,
/// oldest first.
struct History {
    user: [u16; HIST],
    sys: [u16; HIST],
}

impl History {
    fn new() -> Self {
        History { user: [0; HIST], sys: [0; HIST] }
    }

    fn push(&mut self, user: u32, sys: u32) {
        self.user.copy_within(1.., 0);
        self.sys.copy_within(1.., 0);
        self.user[HIST - 1] = user as u16;
        self.sys[HIST - 1] = sys as u16;
    }

    fn last(&self) -> (u32, u32) {
        (self.user[HIST - 1] as u32, self.sys[HIST - 1] as u32)
    }
}

struct Cpu {
    id: usize,
    /// The frequency it last ran at, MHz; 0 where it is not measured.
    mhz: u32,
    prev: CpuTimes,
    hist: History,
}

struct Proc {
    pid: usize,
    name: String,
    ticks: u64,
    /// Thousandths of one CPU over the last period.
    pm: u32,
    cpu: usize,
    /// Resident set, KiB.
    rss_kb: u64,
}

struct State {
    model: String,
    /// The TSC's frequency, shown only where cores are not measured.
    mhz: String,
    /// `/proc/cpuinfo` lists `aperfmperf`: `cpu MHz` is each core's
    /// measured frequency, not the TSC's.
    measured: bool,
    cpus: Vec<Cpu>,
    total_prev: CpuTimes,
    total: History,
    mem_total_kb: u64,
    mem_free_kb: u64,
    loadavg: String,
    tasks: String,
    /// Tctl, millidegrees, where `/proc/sensors` has it: the temperature
    /// the CPU's own cooling control goes by, what `sensors` headlines.
    tctl: Option<i32>,
    /// The package energy counter (µJ) and when it was read (ms), for the
    /// next sample's difference.
    energy_prev: Option<(i64, i64)>,
    /// Package power over the last period, milliwatts.
    power_mw: Option<i64>,
    uptime_s: u64,
    procs: Vec<Proc>,
    samples: u64,
    buf: Vec<u8>,
    dents: Vec<u8>,
}

impl State {
    fn new() -> State {
        let mut buf = Vec::new();
        let (mut model, mut mhz) = (String::from("unknown CPU"), String::new());
        let mut measured = false;
        if read_file("/proc/cpuinfo", &mut buf) {
            let t = text_of(&buf);
            let value = |key: &str| {
                t.lines().find(|l| l.starts_with(key)).and_then(|l| l.split_once(':')).map(|(_, v)| String::from(v.trim()))
            };
            if let Some(m) = value("model name") {
                model = m;
            }
            if let Some(m) = value("cpu MHz") {
                mhz = format!("{} MHz", m.split('.').next().unwrap_or(""));
            }
            measured = value("flags").is_some_and(|f| f.split(' ').any(|x| x == "aperfmperf"));
        }
        State {
            model,
            mhz,
            measured,
            cpus: Vec::new(),
            total_prev: CpuTimes::default(),
            total: History::new(),
            mem_total_kb: 0,
            mem_free_kb: 0,
            loadavg: String::new(),
            tasks: String::new(),
            tctl: None,
            energy_prev: None,
            power_mw: None,
            uptime_s: 0,
            procs: Vec::new(),
            samples: 0,
            buf,
            dents: vec![0u8; 4096],
        }
    }

    fn sample(&mut self) {
        // CPUs.
        let mut period = CpuTimes::default();
        if read_file("/proc/stat", &mut self.buf) {
            for line in text_of(&self.buf).lines() {
                let Some((cpu, now)) = parse_stat_line(line) else { continue };
                let (prev, hist) = match cpu {
                    None => (&mut self.total_prev, &mut self.total),
                    Some(id) => {
                        let i = match self.cpus.iter().position(|c| c.id == id) {
                            Some(i) => i,
                            None => {
                                self.cpus.push(Cpu { id, mhz: 0, prev: now, hist: History::new() });
                                self.cpus.len() - 1
                            }
                        };
                        let c = &mut self.cpus[i];
                        (&mut c.prev, &mut c.hist)
                    }
                };
                let d = now.since(prev);
                *prev = now;
                if cpu.is_none() {
                    period = d;
                }
                if self.samples > 0 {
                    let t = d.total();
                    hist.push(permille(d.user + d.nice, t), permille(d.system + d.irq + d.softirq, t));
                }
            }
        }

        // Each core's frequency.
        if self.measured && read_file("/proc/cpuinfo", &mut self.buf) {
            for (id, mhz) in parse_cpu_mhz(text_of(&self.buf)) {
                if let Some(c) = self.cpus.iter_mut().find(|c| c.id == id) {
                    c.mhz = mhz;
                }
            }
        }

        // Temperature and package power.
        let now_ms = syscall::uptime_ms();
        let (tctl, energy) = if read_file("/proc/sensors", &mut self.buf) {
            let t = text_of(&self.buf);
            (sensor(t, "temp", "Tctl").map(|v| v as i32), sensor(t, "energy", "Esocket0"))
        } else {
            (None, None)
        };
        self.tctl = tctl;
        self.power_mw = match (self.energy_prev, energy) {
            (Some((e0, t0)), Some(e1)) if now_ms > t0 && e1 >= e0 => Some((e1 - e0) / (now_ms - t0)),
            _ => None,
        };
        self.energy_prev = energy.map(|e| (e, now_ms));

        // Memory, load, uptime.
        if read_file("/proc/meminfo", &mut self.buf) {
            let t = text_of(&self.buf);
            self.mem_total_kb = field_kb(t, "MemTotal:").unwrap_or(0);
            self.mem_free_kb = field_kb(t, "MemAvailable:").or_else(|| field_kb(t, "MemFree:")).unwrap_or(0);
        }
        if read_file("/proc/loadavg", &mut self.buf) {
            let f: Vec<&str> = text_of(&self.buf).split_ascii_whitespace().collect();
            if f.len() >= 4 {
                self.loadavg = format!("{}  {}  {}", f[0], f[1], f[2]);
                let (run, all) = f[3].split_once('/').unwrap_or(("?", "?"));
                self.tasks = format!("tasks {}/{}", run, all);
            }
        }
        if read_file("/proc/uptime", &mut self.buf) {
            self.uptime_s = text_of(&self.buf).split('.').next().and_then(|s| s.parse().ok()).unwrap_or(0);
        }

        // Processes: CPU over the period, as a share of one CPU.
        let ticks_per_cpu = period.total() / (self.cpus.len().max(1) as u64);
        let mut now_procs = Vec::new();
        for pid in pids(&mut self.dents) {
            if !read_file(&format!("/proc/{}/stat", pid), &mut self.buf) {
                continue;
            }
            let Some((comm, ticks, cpu, rss)) = parse_pid_stat(text_of(&self.buf)) else { continue };
            let name = if read_file(&format!("/proc/{}/cmdline", pid), &mut self.buf) {
                command_of(&self.buf, &comm)
            } else {
                comm
            };
            let before = self.procs.iter().find(|p| p.pid == pid).map(|p| p.ticks);
            let pm = match before {
                Some(b) if self.samples > 0 => permille(ticks.saturating_sub(b), ticks_per_cpu),
                _ => 0,
            };
            now_procs.push(Proc { pid, name, ticks, pm, cpu, rss_kb: rss * 4 });
        }
        self.procs = now_procs;
        self.samples += 1;
    }

    fn top(&self) -> Vec<&Proc> {
        let mut v: Vec<&Proc> = self.procs.iter().collect();
        v.sort_by(|a, b| b.pm.cmp(&a.pm).then(b.ticks.cmp(&a.ticks)));
        v.truncate(TOP_N);
        v
    }
}

// ── Drawing ──────────────────────────────────────────────────────────────

/// A size in KiB the way `top` prints one: `824K`, `3.4M`, `1.2G`.
fn size_kb(kb: u64) -> String {
    if kb < 1024 {
        format!("{}K", kb)
    } else if kb < 1024 * 1024 {
        format!("{}.{}M", kb / 1024, kb % 1024 * 10 / 1024)
    } else {
        format!("{}.{}G", kb >> 20, (kb & 0xFFFFF) * 10 >> 20)
    }
}

fn pct(pm: u32) -> String {
    format!("{}.{}%", pm / 10, pm % 10)
}

fn panel(cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32) {
    cv.rect(x, y, w, h, PANEL);
    cv.frame(x, y, w, h, EDGE);
}

/// A stacked area graph of `hist` in the box: system time from the
/// bottom, user time on top of it, newest sample at the right edge. Each
/// sample is `step` pixels wide; older ones than fit are not drawn. The
/// box is in canvas pixels, `k` the scale (dashes, edge, narrowest step).
fn graph(cv: &mut Canvas, k: i32, x: i32, y: i32, w: i32, h: i32, hist: &History) {
    for frac in [250, 500, 750] {
        let gy = y + h - h * frac / 1000;
        let mut gx = x;
        while gx < x + w {
            cv.rect(gx, gy, 2 * k, k, GRID);
            gx += 5 * k;
        }
    }
    let step = if w / 2 >= HIST as i32 * k { (w / HIST as i32).max(1) } else { 2 * k };
    let shown = (w / step).min(HIST as i32);
    for i in 0..shown {
        let s = HIST - 1 - i as usize;
        let (u, sy) = (hist.user[s] as i32, hist.sys[s] as i32);
        let hs = (h * sy + 500) / 1000;
        let hu = ((h * (u + sy) + 500) / 1000).min(h) - hs;
        let col = x + w - (i + 1) * step;
        let uc = load_color((u + sy) as u32);
        for dx in 0..step {
            let cx = col + dx;
            if hs > 0 {
                cv.vline(cx, y + h - hs, hs, scale(SYS, 170));
            }
            if hu > 0 {
                // A vertical gradient, dim at the bottom of the graph and
                // full colour at its top, with a bright edge on the curve.
                let top = y + h - hs - hu;
                for py in top..top + hu {
                    let t = 256 - (py - y) * 200 / h.max(1);
                    cv.put(cx, py, scale(uc, t.clamp(56, 256)));
                }
                cv.vline(cx, top, hu.min(2 * k), mix(uc, 0xFFFFFF, 96));
            }
        }
    }
}

/// Columns and rows for `n` tiles in a `w x h` area: whichever makes the
/// smallest tile largest, for tiles about 2.5 times wider than tall.
fn grid_shape(n: usize, w: i32, h: i32) -> (i32, i32) {
    let mut best = (1, n.max(1) as i32, 0);
    for cols in 1..=n.max(1) as i32 {
        let rows = (n as i32 + cols - 1) / cols;
        let (tw, th) = (w / cols, h / rows);
        let score = (tw * 2 / 5).min(th);
        if score > best.2 {
            best = (cols, rows, score);
        }
    }
    (best.0, best.1)
}

fn draw(cv: &mut Canvas, ui: &mut Ui, st: &State) {
    let k = ui.k;
    let d = |v: i32| v * k;
    let (w, h) = (W as i32, H as i32);
    cv.fill(BG);

    // Header.
    cv.rect(0, 0, d(w), d(34), PANEL);
    cv.hline(0, d(34), d(w), EDGE);
    let x = ui.text(cv, TITLE, d(12), d(6), "CPU", ACCENT);
    let mut x = ui.text(cv, SMALL, x + d(12), d(9), &st.model, TEXT);
    if st.measured {
        // The range the cores span right now.
        let m = st.cpus.iter().map(|c| c.mhz).filter(|&m| m > 0);
        if let (Some(lo), Some(hi)) = (m.clone().min(), m.max()) {
            x = ui.text(cv, SMALL, x + d(10), d(9), &format!("{} - {}", ghz(lo, false), ghz(hi, true)), DIM);
        }
    } else if !st.mhz.is_empty() {
        x = ui.text(cv, SMALL, x + d(10), d(9), &st.mhz, DIM);
    }
    if let Some(t) = st.tctl {
        // Coloured like a load: 30 C cool, 90 C (a 5900X throttles at 90) red.
        let pm = ((t - 30_000).clamp(0, 60_000) / 60) as u32;
        x = ui.text(cv, SMALL_BOLD, x + d(16), d(9), &celsius(t), load_color(pm));
    }
    if let Some(mw) = st.power_mw {
        // Package power, from the energy counter's last difference.
        ui.text(cv, SMALL, x + d(12), d(9), &format!("{}.{} W", mw / 1000, mw % 1000 / 100), DIM);
    }
    let up = st.uptime_s;
    let uptime = format!("up {}:{:02}:{:02}", up / 3600, up / 60 % 60, up % 60);
    ui.right(cv, SMALL, d(w - 12), d(9), &uptime, DIM);

    // One tile per CPU.
    // The bottom panels take the last 130 rows; the tiles get the rest.
    let bottom = h - 130;
    let (gx, gy, gw, gh) = (8, 42, w - 16, bottom - 6 - 42);
    let (cols, rows) = grid_shape(st.cpus.len(), gw, gh);
    let (tw, th) = (gw / cols, gh / rows);
    for (i, c) in st.cpus.iter().enumerate() {
        let (tx, ty) = (gx + (i as i32 % cols) * tw, gy + (i as i32 / cols) * th);
        panel(cv, d(tx + 2), d(ty + 2), d(tw - 4), d(th - 4));
        let (u, s) = c.hist.last();
        ui.text(cv, SMALL, d(tx + 8), d(ty + 4), &format!("cpu{}", c.id), DIM);
        ui.right(cv, SMALL_BOLD, d(tx + tw - 8), d(ty + 4), &pct(u + s), load_color(u + s));
        let top = ty + 22;
        let gh = d(ty + th - 7 - top);
        graph(cv, k, d(tx + 7), d(top), d(tw - 14), gh, &c.hist);
        if c.mhz > 0 && gh >= ui.height(SMALL) + d(4) {
            // Over the graph's bottom left, where the graph is tall enough
            // to hold a line; the unit only where it fits.
            let full = ghz(c.mhz, true);
            let s = if ui.width(SMALL, &full) <= d(tw - 20) { full } else { ghz(c.mhz, false) };
            ui.text(cv, SMALL, d(tx + 10), d(ty + th - 9) - ui.height(SMALL), &s, TEXT);
        }
    }

    // Bottom left: all CPUs, memory, load.
    let (bx, by, bw, bh) = (8, bottom, 312, 122);
    panel(cv, d(bx + 2), d(by + 2), d(bw - 4), d(bh - 4));
    let (u, s) = st.total.last();
    ui.text(cv, SMALL_BOLD, d(bx + 10), d(by + 6), "All CPUs", TEXT);
    let x = ui.text(cv, SMALL, d(bx + 110), d(by + 6), &format!("usr {}", pct(u)), load_color(u + s));
    ui.text(cv, SMALL, x + d(12), d(by + 6), &format!("sys {}", pct(s)), SYS);
    graph(cv, k, d(bx + 10), d(by + 26), d(bw - 20), d(36), &st.total);

    let used = st.mem_total_kb.saturating_sub(st.mem_free_kb);
    let mem_pm = permille(used, st.mem_total_kb);
    ui.text(cv, SMALL_BOLD, d(bx + 10), d(by + 68), "Memory", TEXT);
    let mem = format!("{} / {} MiB", used / 1024, st.mem_total_kb / 1024);
    ui.right(cv, SMALL, d(bx + bw - 10), d(by + 68), &mem, DIM);
    let (mx, my, mw) = (d(bx + 90), d(by + 73), d(bw - 90 - 10 - 8) - ui.width(SMALL, &mem));
    cv.rect(mx, my, mw, d(8), GRID);
    let filled = mw * mem_pm as i32 / 1000;
    for dx in 0..filled {
        cv.vline(mx + dx, my, d(8), mix(ACCENT, 0xBC8CFF, dx * 256 / mw.max(1)));
    }

    ui.text(cv, SMALL_BOLD, d(bx + 10), d(by + 92), "Load", TEXT);
    ui.text(cv, SMALL, d(bx + 90), d(by + 92), &st.loadavg, TEXT);
    ui.right(cv, SMALL, d(bx + bw - 10), d(by + 92), &st.tasks, DIM);

    // Bottom right: the busiest processes.
    let (px, py, pw, ph) = (320, bottom, w - 8 - 320, 122);
    panel(cv, d(px + 2), d(py + 2), d(pw - 4), d(ph - 4));
    let (c_pid, c_name, c_cpu, c_rss, c_pct) =
        (d(px + 10), d(px + 62), d(px + pw - 150), d(px + pw - 80), d(px + pw - 10));
    ui.text(cv, SMALL_BOLD, c_pid, d(py + 6), "PID", DIM);
    ui.text(cv, SMALL_BOLD, c_name, d(py + 6), "COMMAND", DIM);
    ui.text(cv, SMALL_BOLD, c_cpu, d(py + 6), "CPU", DIM);
    ui.right(cv, SMALL_BOLD, c_rss, d(py + 6), "RSS", DIM);
    ui.right(cv, SMALL_BOLD, c_pct, d(py + 6), "%CPU", DIM);
    for (i, p) in st.top().iter().enumerate() {
        let y = d(py + 26 + i as i32 * 18);
        let c = if p.pm >= 10 { TEXT } else { DIM };
        ui.text(cv, SMALL, c_pid, y, &format!("{}", p.pid), c);
        let name = ui.fit(SMALL, &p.name, c_cpu - c_name - d(8));
        ui.text(cv, SMALL, c_name, y, &name, c);
        ui.text(cv, SMALL, c_cpu, y, &format!("{}", p.cpu), DIM);
        ui.right(cv, SMALL, c_rss, y, &size_kb(p.rss_kb), DIM);
        ui.right(cv, SMALL, c_pct, y, &pct(p.pm), if p.pm >= 10 { load_color(p.pm) } else { DIM });
    }
}

fn main(args: Args) -> i32 {
    let Some(mut gfx) = Gfx::open(args.env(b"GUI_DISPLAY"), "cpumon", W, H, HIDPI) else {
        println!("cpumon: nothing to draw on (no /dev/fb, no compositor)");
        return 1;
    };
    let k = gfx.scale();
    let (fw, fh) = (W * k, H * k);
    let mut frame = vec![0u32; fw * fh];
    let mut ui = Ui::new(k as i32);
    if !ui.t.fonts() {
        println!("cpumon: no fonts in {}, bitmap text", userspace::text::FONT_DIR);
    }
    let mut st = State::new();
    st.sample();
    let mut next = syscall::uptime_ms() + PERIOD_MS;
    let mut dirty = true;

    loop {
        while let Some(ev) = gfx.next_event() {
            if ev.kind == EV_KEY && ev.value != 0 && (ev.code == KEY_ESC || ev.code == KEY_Q) {
                return 0;
            }
        }
        let now = syscall::uptime_ms();
        if now >= next {
            st.sample();
            next += PERIOD_MS;
            if next <= now {
                next = now + PERIOD_MS; // fell behind (suspended, stopped): skip ahead
            }
            dirty = true;
        }
        if dirty {
            let mut cv = Canvas::new(&mut frame, fw, fh, fw);
            draw(&mut cv, &mut ui, &st);
            gfx.present(&frame);
            dirty = false;
        }
        // Keys are looked at every 50 ms; nothing else happens between samples.
        syscall::sleep_ms(50);
    }
}
