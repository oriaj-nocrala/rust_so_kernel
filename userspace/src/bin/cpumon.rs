//! A CPU monitor: one live graph per CPU, split into user and system time,
//! plus total load, memory, the load averages and the busiest processes.
//!
//! Everything it shows comes from the files a Linux monitor reads, in the
//! formats Linux writes them — `/proc/stat` (per-CPU ticks, parsed with
//! the same `sched::cputime` code the kernel renders it with),
//! `/proc/<pid>/stat`, `/proc/meminfo`, `/proc/loadavg`, `/proc/uptime`,
//! `/proc/cpuinfo` — so it is also a running check that they say something
//! true. A sample every half second; the picture is a 640x400 frame drawn
//! with `draw` (antialiased Noto text, `draw::smooth`) and shown through
//! `userspace::gfx`: a window under the compositor, the whole screen on
//! the console. Esc or Q quits.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use draw::color::{hsv, mix, scale};
use draw::smooth::{FontWeight, RasterHeight, Smooth};
use draw::Canvas;
use sched::cputime::{parse_stat_line, permille, CpuTimes};
use userspace::args::Args;
use userspace::gfx::{Gfx, EV_KEY};
use userspace::{entry, println, syscall};

entry!(main);

const W: usize = 640;
const H: usize = 400;
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

const SMALL: Smooth = Smooth::new(RasterHeight::Size16, FontWeight::Regular);
const SMALL_BOLD: Smooth = Smooth::new(RasterHeight::Size16, FontWeight::Bold);
const TITLE: Smooth = Smooth::new(RasterHeight::Size20, FontWeight::Bold);

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

/// `(comm, utime + stime, last CPU)` from a `/proc/<pid>/stat` line.
fn parse_pid_stat(line: &str) -> Option<(String, u64, usize)> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    let comm = String::from(&line[open + 1..close]);
    // Field 3 (state) is the first after ") "; utime is 14, stime 15,
    // processor 39.
    let f: Vec<&str> = line[close + 2..].split_ascii_whitespace().collect();
    let num = |k: usize| f.get(k - 3).and_then(|v| v.parse::<u64>().ok());
    Some((comm, num(14)? + num(15)?, num(39).unwrap_or(0) as usize))
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
}

struct State {
    model: String,
    mhz: String,
    cpus: Vec<Cpu>,
    total_prev: CpuTimes,
    total: History,
    mem_total_kb: u64,
    mem_free_kb: u64,
    loadavg: String,
    tasks: String,
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
        }
        State {
            model,
            mhz,
            cpus: Vec::new(),
            total_prev: CpuTimes::default(),
            total: History::new(),
            mem_total_kb: 0,
            mem_free_kb: 0,
            loadavg: String::new(),
            tasks: String::new(),
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
                                self.cpus.push(Cpu { id, prev: now, hist: History::new() });
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
            let Some((comm, ticks, cpu)) = parse_pid_stat(text_of(&self.buf)) else { continue };
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
            now_procs.push(Proc { pid, name, ticks, pm, cpu });
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

fn pct(pm: u32) -> String {
    format!("{}.{}%", pm / 10, pm % 10)
}

fn panel(cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32) {
    cv.rect(x, y, w, h, PANEL);
    cv.frame(x, y, w, h, EDGE);
}

/// A stacked area graph of `hist` in the box: system time from the
/// bottom, user time on top of it, newest sample at the right edge. Each
/// sample is `step` pixels wide; older ones than fit are not drawn.
fn graph(cv: &mut Canvas, x: i32, y: i32, w: i32, h: i32, hist: &History) {
    for frac in [250, 500, 750] {
        let gy = y + h - h * frac / 1000;
        let mut gx = x;
        while gx < x + w {
            cv.hline(gx, gy, 2, GRID);
            gx += 5;
        }
    }
    let step = if w / 2 >= HIST as i32 { (w / HIST as i32).max(1) } else { 2 };
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
                cv.vline(cx, top, hu.min(2), mix(uc, 0xFFFFFF, 96));
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

fn draw(cv: &mut Canvas, st: &State) {
    cv.fill(BG);

    // Header.
    cv.rect(0, 0, W as i32, 34, PANEL);
    cv.hline(0, 34, W as i32, EDGE);
    let x = cv.smooth_text(TITLE, 12, 6, "CPU", ACCENT);
    let x = cv.smooth_text(SMALL, x + 12, 9, &st.model, TEXT);
    if !st.mhz.is_empty() {
        cv.smooth_text(SMALL, x + 10, 9, &st.mhz, DIM);
    }
    let up = st.uptime_s;
    let uptime = format!("up {}:{:02}:{:02}", up / 3600, up / 60 % 60, up % 60);
    cv.smooth_text_right(SMALL, W as i32 - 12, 9, &uptime, DIM);

    // One tile per CPU.
    let (gx, gy, gw, gh) = (8, 42, W as i32 - 16, 222);
    let (cols, rows) = grid_shape(st.cpus.len(), gw, gh);
    let (tw, th) = (gw / cols, gh / rows);
    for (i, c) in st.cpus.iter().enumerate() {
        let (tx, ty) = (gx + (i as i32 % cols) * tw, gy + (i as i32 / cols) * th);
        panel(cv, tx + 2, ty + 2, tw - 4, th - 4);
        let (u, s) = c.hist.last();
        cv.smooth_text(SMALL, tx + 8, ty + 4, &format!("cpu{}", c.id), DIM);
        cv.smooth_text_right(SMALL_BOLD, tx + tw - 8, ty + 4, &pct(u + s), load_color(u + s));
        let top = ty + 22;
        graph(cv, tx + 7, top, tw - 14, ty + th - 7 - top, &c.hist);
    }

    // Bottom left: all CPUs, memory, load.
    let (bx, by, bw, bh) = (8, 270, 312, 122);
    panel(cv, bx + 2, by + 2, bw - 4, bh - 4);
    let (u, s) = st.total.last();
    cv.smooth_text(SMALL_BOLD, bx + 10, by + 6, "All CPUs", TEXT);
    let x = cv.smooth_text(SMALL, bx + 110, by + 6, &format!("usr {}", pct(u)), load_color(u + s));
    cv.smooth_text(SMALL, x + 12, by + 6, &format!("sys {}", pct(s)), SYS);
    graph(cv, bx + 10, by + 26, bw - 20, 36, &st.total);

    let used = st.mem_total_kb.saturating_sub(st.mem_free_kb);
    let mem_pm = permille(used, st.mem_total_kb);
    cv.smooth_text(SMALL_BOLD, bx + 10, by + 68, "Memory", TEXT);
    let mem = format!("{} / {} MiB", used / 1024, st.mem_total_kb / 1024);
    cv.smooth_text_right(SMALL, bx + bw - 10, by + 68, &mem, DIM);
    let (mx, my, mw) = (bx + 90, by + 73, bw - 90 - 10 - SMALL.width(&mem) - 8);
    cv.rect(mx, my, mw, 8, GRID);
    let filled = mw * mem_pm as i32 / 1000;
    for dx in 0..filled {
        cv.vline(mx + dx, my, 8, mix(ACCENT, 0xBC8CFF, dx * 256 / mw.max(1)));
    }

    cv.smooth_text(SMALL_BOLD, bx + 10, by + 92, "Load", TEXT);
    cv.smooth_text(SMALL, bx + 90, by + 92, &st.loadavg, TEXT);
    cv.smooth_text_right(SMALL, bx + bw - 10, by + 92, &st.tasks, DIM);

    // Bottom right: the busiest processes.
    let (px, py, pw, ph) = (320, 270, W as i32 - 8 - 320, 122);
    panel(cv, px + 2, py + 2, pw - 4, ph - 4);
    let (c_pid, c_name, c_cpu, c_pct) = (px + 10, px + 62, px + pw - 110, px + pw - 10);
    cv.smooth_text(SMALL_BOLD, c_pid, py + 6, "PID", DIM);
    cv.smooth_text(SMALL_BOLD, c_name, py + 6, "COMMAND", DIM);
    cv.smooth_text(SMALL_BOLD, c_cpu, py + 6, "CPU", DIM);
    cv.smooth_text_right(SMALL_BOLD, c_pct, py + 6, "%CPU", DIM);
    for (i, p) in st.top().iter().enumerate() {
        let y = py + 26 + i as i32 * 18;
        let c = if p.pm >= 10 { TEXT } else { DIM };
        cv.smooth_text(SMALL, c_pid, y, &format!("{}", p.pid), c);
        let name: String = p.name.chars().take(((c_cpu - c_name - 8) / SMALL.cell().0) as usize).collect();
        cv.smooth_text(SMALL, c_name, y, &name, c);
        cv.smooth_text(SMALL, c_cpu, y, &format!("{}", p.cpu), DIM);
        cv.smooth_text_right(SMALL, c_pct, y, &pct(p.pm), if p.pm >= 10 { load_color(p.pm) } else { DIM });
    }
}

fn main(args: Args) -> i32 {
    let Some(mut gfx) = Gfx::open(args.env(b"GUI_DISPLAY"), "cpumon", W, H, 0) else {
        println!("cpumon: nothing to draw on (no /dev/fb, no compositor)");
        return 1;
    };
    let mut frame = vec![0u32; W * H];
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
            let mut cv = Canvas::new(&mut frame, W, H, W);
            draw(&mut cv, &st);
            gfx.present(&frame);
            dirty = false;
        }
        // Keys are looked at every 50 ms; nothing else happens between samples.
        syscall::sleep_ms(50);
    }
}
