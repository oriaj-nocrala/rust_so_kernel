// kernel/src/fs/procfs.rs
//
// Minimal /proc — meminfo, plus real per-process symlinks (/proc/self,
// /proc/<pid>/exe). Exists so real Unix tools/scripts (busybox `free`,
// `cat /proc/meminfo`, `ash`'s `execve("/proc/self/exe", ...)` re-exec
// trick for any applet that isn't NOFORK/NOEXEC) work here the same way
// they would on a real system, instead of leaning on kernel-specific
// syscalls or ad-hoc string special-casing.
//
// LAYOUT
// ──────
//   /proc/           (ProcDirInode)
//   ├── meminfo
//   ├── self         → symlink to /proc/<own pid>
//   └── <pid>/       (ProcPidDirInode, only for a pid that actually exists)
//       └── exe      → symlink to whatever ELF path that process is running
//
// Real Linux's /proc/<pid> has dozens of entries (cmdline, status, fd/,
// maps, ...) — only `exe` exists here, since that's the one thing
// anything in this kernel actually consumes (`ash`'s FEATURE_SH_STANDALONE
// re-exec). `readdir` on the root only lists the always-present entries
// (meminfo, self) — it does not enumerate live pids, so `ls /proc` won't
// show every process; direct lookup (`cat /proc/3/exe`, `cd /proc/3`)
// still works for any pid that's actually alive.
//
// Inode numbers: 200 = /proc directory, 201 = meminfo, 202 = self.
// Per-pid inodes are derived from the pid (see `pid_dir_ino`/`pid_exe_ino`).

use alloc::{boxed::Box, format, string::String, sync::Arc, vec::Vec};

use crate::fs::{
    types::{DirEntry, Errno, FileType, OpenFlags, Stat},
    vfs::{Filesystem, Inode},
};
use crate::process::file::{FileError, FileHandle, FileResult};

fn pid_dir_ino(pid: usize) -> u64 { 1000 + (pid as u64) * 3 }
fn pid_exe_ino(pid: usize) -> u64 { 1000 + (pid as u64) * 3 + 1 }
fn pid_stat_ino(pid: usize) -> u64 { 1000 + (pid as u64) * 3 + 2 }

// ── Filesystem ───────────────────────────────────────────────────────────────

pub struct ProcFs;

impl Filesystem for ProcFs {
    fn name(&self) -> &str { "procfs" }

    fn root(&self) -> Result<Arc<dyn Inode>, Errno> {
        Ok(Arc::new(ProcDirInode))
    }
}

/// Renders `/proc/meminfo` content as of right now — `MemTotal`/`MemFree`
/// only (no `MemAvailable`/`Buffers`/`Cached`: this kernel has no page
/// cache or reclaimable memory concept to report). Matches real
/// `/proc/meminfo`'s `"%-13s%8lu kB\n"` shape closely enough for tools
/// that grep/awk specific field names, which is the only thing that
/// actually matters for compatibility.
fn render_meminfo() -> String {
    let (total, free) = crate::allocator::mem_stats();
    let total_kb = total / 1024;
    let free_kb = free / 1024;
    format!(
        "MemTotal:       {:>8} kB\nMemFree:        {:>8} kB\nMemAvailable:   {:>8} kB\n",
        total_kb, free_kb, free_kb
    )
}

/// Renders `/proc/acpi` — a human-readable dump of `crate::acpi::topology()`
/// (Local APIC address, enabled CPUs, I/O APICs, interrupt source
/// overrides), regenerated fresh on every `open()`, same convention as
/// `/proc/meminfo` and `/proc/kdebug`. If ACPI parsing never succeeded at
/// boot (no RSDP, bad checksum, no MADT — see `acpi::init`), reports that
/// plainly instead of an empty file.
fn render_acpi() -> String {
    let Some(topo) = crate::acpi::topology() else {
        return String::from("ACPI: not available\n");
    };
    let mut out = format!("Local APIC: {:#010x}\n", topo.local_apic_addr);
    out.push_str(&format!("CPUs: {}\n", topo.cpus.len()));
    for cpu in &topo.cpus {
        out.push_str(&format!(
            "  processor_id={} apic_id={}\n",
            cpu.processor_id, cpu.apic_id
        ));
    }
    for io in &topo.io_apics {
        out.push_str(&format!(
            "I/O APIC {} @ {:#010x} gsi_base={}\n",
            io.id, io.address, io.gsi_base
        ));
    }
    for iso in &topo.overrides {
        out.push_str(&format!(
            "override: bus {} IRQ {} -> GSI {} (flags {:#x})\n",
            iso.bus, iso.source, iso.gsi, iso.flags
        ));
    }
    out
}

/// Renders `/proc/<pid>/stat` in the classic Linux `"pid (comm) state
/// ppid pgid sid tty tpgid flags minflt cminflt majflt cmajflt utime stime
/// cutime cstime priority nice ..."` shape — this is what BusyBox
/// `ps`/`top` (`libbb/procps.c::procps_scan`) actually parses: split on the
/// last `)` to pull `comm` out (so it's safe even if `comm` itself
/// contained spaces, though ours never does), then a fixed-position
/// `sscanf` over everything after. Fields this kernel has no real data for
/// (page fault counts, per-process cpu ticks, start time, memory size) are
/// reported as `0` — enough for `ps`/`top` to run and show real pid/name/
/// state/ppid/pgid/priority without crashing on a short field list, not
/// enough for their CPU%/MEM%/VSZ/RSS columns to mean anything yet.
fn render_proc_stat(pid: usize, snap: &crate::process::scheduler::ProcStatSnapshot) -> String {
    let end = snap.name.iter().position(|&b| b == 0).unwrap_or(snap.name.len());
    let comm = String::from_utf8_lossy(&snap.name[..end]);
    let comm = if comm.is_empty() { "?" } else { comm.as_ref() };
    let state = match snap.state {
        crate::process::ProcessState::Ready | crate::process::ProcessState::Running => 'R',
        crate::process::ProcessState::Blocked => 'S',
        crate::process::ProcessState::Zombie => 'Z',
        crate::process::ProcessState::Stopped => 'T',
    };
    format!(
        "{pid} ({comm}) {state} {ppid} {pgid} {pgid} 0 -1 0 0 0 0 0 0 0 0 0 {priority} 0 0 0 0 0 0\n",
        pid = pid, comm = comm, state = state,
        ppid = snap.ppid, pgid = snap.pgid, priority = snap.priority,
    )
}

/// Renders `/proc/fbinfo` — the framebuffer console's instrument panel:
/// real geometry, the memory type its mapping actually has, and the cost
/// of every drawing primitive since boot.
///
/// WHY THIS FILE EXISTS. The console is imperceptibly fast in QEMU, where
/// the framebuffer is host RAM, and was measured at roughly a second to
/// clear the screen on the physical AM4 machine, where it lives across
/// PCIe. Nothing about that gap was observable from inside the kernel:
/// the resolution was assumed (a comment in `framebuffer.rs` said "1280 x
/// 800 en qemu"), the memory type was assumed, and "slow" was a feeling.
/// On a machine with no serial capture, the only way any of it becomes a
/// number is a file somebody can `cat` and photograph.
///
/// The memory type is reported as its two raw inputs — what the MTRRs say
/// about the physical range, and which PAT entry the page's own bits
/// select — rather than as a single combined verdict, because the
/// MTRR x PAT combination table is exactly the thing that is easy to get
/// wrong from memory. The ground truth is the measured `MB/s` below it,
/// which needs no table at all.
fn render_fbinfo() -> String {
    use crate::framebuffer::FRAMEBUFFER;

    let mut out = String::new();

    let geometry = {
        let guard = FRAMEBUFFER.lock();
        guard.as_ref().map(|fb| {
            let (w, h) = fb.dimensions();
            (w, h, fb.stride(), fb.bytes_per_pixel(), fb.virt_addr(), fb.byte_len())
        })
    };

    let Some((w, h, stride, bpp, virt, len)) = geometry else {
        out.push_str("framebuffer: none (headless boot)\n");
        out.push_str(&crate::debug::render_fb_report());
        return out;
    };

    let (cols, rows) = crate::drivers::framebuffer_console::text_dimensions();
    out.push_str(&format!(
        "width: {}\nheight: {}\nstride: {} px\nbytes_per_pixel: {}\n\
         size: {} bytes ({} KiB)\ntext_grid: {}x{} cells ({}x{} px each)\n\
         virt: {:#x}\n",
        w, h, stride, bpp, len, len / 1024, cols, rows,
        crate::framebuffer::GLYPH_W, crate::framebuffer::GLYPH_H + 1, virt,
    ));

    let r = crate::memory::memtype::report_for(x86_64::VirtAddr::new(virt));
    match r.phys {
        Some(p) => out.push_str(&format!("phys: {:#x}\n", p)),
        None => out.push_str("phys: <not mapped?>\n"),
    }
    out.push_str(&format!(
        "pte_cache_bits: {} page, PAT={} PCD={} PWT={} -> pat_index={}\n",
        r.page_size.unwrap_or("?"), r.pat_bit as u8, r.pcd as u8, r.pwt as u8, r.pat_index,
    ));
    if let Some(p) = r.phys {
        out.push_str(&render_physmap_alias(p, len as u64, r.pat_msr));
    }
    out.push_str(&format!(
        "pat_msr: {:#018x} (entry {} = {}, WC entry present: {})\n",
        r.pat_msr,
        r.pat_index,
        r.pat_type.map(|t| t.name()).unwrap_or("?"),
        r.pat_has_wc,
    ));
    out.push_str(&format!(
        "mtrrcap: {:#x} (variable={} wc_supported={}) def_type: {:#x}\n",
        r.mtrrcap, r.range_count, r.mtrr_wc_supported, r.def_type,
    ));
    out.push_str(&match r.mtrr {
        None => String::from("mtrr_type: <no physical address>\n"),
        Some(hal::memtype::MtrrResolution::Disabled) => {
            String::from("mtrr_type: UC (MTRRs disabled)\n")
        }
        Some(hal::memtype::MtrrResolution::Default(t)) => {
            format!("mtrr_type: {} (no range covers it; default type)\n", t.name())
        }
        Some(hal::memtype::MtrrResolution::Matched(t)) => {
            format!("mtrr_type: {}\n", t.name())
        }
        Some(hal::memtype::MtrrResolution::Overlapping { resolved, conflicting }) => format!(
            "mtrr_type: {} (overlapping ranges{})\n",
            resolved.name(),
            if conflicting { ", UNDEFINED combination" } else { "" },
        ),
    });
    out.push_str(&format!("max_phys_addr_bits: {}\n", r.max_phys_addr_bits));
    for (i, e) in r.ranges.iter().enumerate().take(r.range_count) {
        if let Some((base, mask, t)) = e {
            out.push_str(&format!(
                "  mtrr[{}]: base={:#x} mask={:#x} type={}\n",
                i, base, mask, t.name(),
            ));
        }
    }

    out.push('\n');
    out.push_str(&format!("tsc_hz: {}\n", crate::cpu::tsc::freq_hz()));
    out.push_str(&format!(
        "instrument_overhead: {} cycles (min of 64 back-to-back TSC reads)\n",
        tsc_pair_cost(),
    ));
    out.push_str(&crate::debug::render_fb_report());
    out
}

/// Whether the bootloader's physical-memory window also maps the
/// framebuffer aperture, and with which caching bits: one line for the
/// aperture's first byte and one for its last.
///
/// Phase 2 of `docs/fb/wc-shadow-plan.md` maps the framebuffer WC. If the
/// same frames are also mapped WB here, the two mappings disagree on the
/// memory type, which the SDM leaves undefined. Whether that alias exists
/// depends on how far the bootloader's window reaches, which on the target
/// machine is not known yet, so it is read here rather than assumed. Two
/// ends because the aperture can straddle the end of the window.
fn render_physmap_alias(fb_phys: u64, len: u64, pat_msr: u64) -> String {
    let offset = crate::memory::physical_memory_offset().as_u64();
    let mut out = String::new();
    for (label, phys) in [("first", fb_phys), ("last", fb_phys + len.saturating_sub(1))] {
        let virt = x86_64::VirtAddr::new(offset + phys);
        out.push_str(&match crate::memory::memtype::leaf_for(virt) {
            None => format!("physmap_alias_{}: {:#x} not mapped\n", label, virt.as_u64()),
            Some(l) => {
                let (pat, pcd, pwt) = l.cache_bits();
                let idx = hal::memtype::pat_index(pat, pcd, pwt);
                format!(
                    "physmap_alias_{}: {:#x} -> {:#x}, {} page, PAT={} PCD={} PWT={} -> pat_index={} ({})\n",
                    label, virt.as_u64(), l.phys, l.size_name(),
                    pat as u8, pcd as u8, pwt as u8, idx,
                    hal::memtype::pat_entry(pat_msr, idx).map(|t| t.name()).unwrap_or("?"),
                )
            }
        });
    }
    out
}

/// The noise floor of every `cycles` figure below it: what a measurement
/// costs when the thing being measured does nothing at all.
///
/// Measured here, live, rather than assumed, because it is not a constant
/// — `cpu::tsc::read` fences before `rdtsc`, and under QEMU's TCG that is
/// a translation-block break rather than a couple of cycles. Without this
/// line a reader has no way to tell an operation that is genuinely cheap
/// from one whose cost is the instrument, which is the exact trap this
/// kernel has a rule about (`measure-the-instrument-first`): a counter
/// nobody can calibrate is worse than no counter.
///
/// `min` of 64, not the mean: the cheapest pair is the one that was not
/// interrupted.
fn tsc_pair_cost() -> u64 {
    let mut best = u64::MAX;
    for _ in 0..64 {
        let a = crate::cpu::tsc::read();
        let b = crate::cpu::tsc::read();
        best = best.min(b.wrapping_sub(a));
    }
    best
}

// ── Directory inode ──────────────────────────────────────────────────────────

struct ProcDirInode;

impl Inode for ProcDirInode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        Stat::dir(200)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        Ok(Box::new(ProcDirHandle { offset: 0 }))
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        match name {
            "meminfo" => Ok(Arc::new(MeminfoInode)),
            "kdebug" => Ok(Arc::new(KdebugInode)),
            "acpi" => Ok(Arc::new(AcpiInode)),
            "fbinfo" => Ok(Arc::new(FbInfoInode)),
            "dmesg" => Ok(Arc::new(DmesgInode)),
            "self" => Ok(Arc::new(SelfInode)),
            _ => {
                let pid: usize = name.parse().map_err(|_| Errno::ENOENT)?;
                if crate::process::scheduler::exe_name_for_pid(pid).is_some() {
                    Ok(Arc::new(ProcPidDirInode { pid }))
                } else {
                    Err(Errno::ENOENT)
                }
            }
        }
    }

    fn readdir(&self, offset: u64) -> Result<Option<DirEntry>, Errno> {
        match offset {
            0 => Ok(Some(DirEntry::new(200, FileType::Directory, b"."))),
            1 => Ok(Some(DirEntry::new(200, FileType::Directory, b".."))),
            2 => Ok(Some(DirEntry::new(201, FileType::Regular, b"meminfo"))),
            3 => Ok(Some(DirEntry::new(202, FileType::Symlink, b"self"))),
            4 => Ok(Some(DirEntry::new(203, FileType::Regular, b"kdebug"))),
            5 => Ok(Some(DirEntry::new(204, FileType::Regular, b"acpi"))),
            6 => Ok(Some(DirEntry::new(205, FileType::Regular, b"dmesg"))),
            7 => Ok(Some(DirEntry::new(206, FileType::Regular, b"fbinfo"))),
            n => {
                // Live pids, appended after the always-present entries above
                // — this is what makes `ls /proc` / BusyBox `ps`'s
                // `opendir("/proc")` scan see every process (previously
                // direct lookup like `cat /proc/3/exe` worked but nothing
                // enumerated them, see this module's top doc comment).
                let idx = (n - 8) as usize;
                let pids = crate::process::scheduler::all_pids();
                let Some(&pid) = pids.get(idx) else { return Ok(None); };
                let name = format!("{}", pid);
                Ok(Some(DirEntry::new(pid_dir_ino(pid), FileType::Directory, name.as_bytes())))
            }
        }
    }
}

// ── meminfo file inode ───────────────────────────────────────────────────────

struct MeminfoInode;

impl Inode for MeminfoInode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        Stat::regular(201, render_meminfo().len() as i64)
    }

    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        if flags.is_write() {
            return Err(Errno::EROFS);
        }
        Ok(Box::new(ProcFile { data: render_meminfo().into_bytes(), offset: 0 }))
    }
}

// ── kdebug file inode ────────────────────────────────────────────────────────
//
// Read-only report of `crate::debug`'s state: which tracepoint subsystems
// are currently enabled, plus the permanent lifecycle counters (forks,
// execs, reaps, COW faults resolved/failed) — regenerated fresh on every
// open(), same convention as `/proc/meminfo`.
struct KdebugInode;

impl Inode for KdebugInode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        Stat::regular(203, crate::debug::render_report().len() as i64)
    }

    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        if flags.is_write() {
            return Err(Errno::EROFS);
        }
        Ok(Box::new(ProcFile { data: crate::debug::render_report().into_bytes(), offset: 0 }))
    }
}

// ── dmesg file inode ─────────────────────────────────────────────────────────
//
// The kernel message ring (`crate::klog`) as a readable file: everything
// `serial_println!` has emitted since boot, oldest first. Same
// regenerate-on-every-`open()` convention as `/proc/meminfo` and
// `/proc/kdebug`.
//
// Deliberately *not* the whole answer to "read the boot messages" on the
// machine this was written for — reading it takes a shell, which takes a
// keyboard, which is the thing that was broken. See
// `init::show_boot_log_if_no_keyboard` for the half that needs no input.
struct DmesgInode;

impl Inode for DmesgInode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        Stat::regular(205, crate::klog::len() as i64)
    }

    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        if flags.is_write() {
            return Err(Errno::EROFS);
        }
        Ok(Box::new(ProcFile { data: crate::klog::render().into_bytes(), offset: 0 }))
    }
}

// ── acpi file inode ──────────────────────────────────────────────────────────
//
// Read-only report of `crate::acpi::topology()` — Local APIC address,
// enabled CPUs, I/O APICs, interrupt source overrides — regenerated fresh
// on every open(), same convention as `/proc/meminfo`/`/proc/kdebug`.
struct AcpiInode;

impl Inode for AcpiInode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        Stat::regular(204, render_acpi().len() as i64)
    }

    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        if flags.is_write() {
            return Err(Errno::EROFS);
        }
        Ok(Box::new(ProcFile { data: render_acpi().into_bytes(), offset: 0 }))
    }
}

// ── fbinfo file inode ────────────────────────────────────────────────────────
//
// Framebuffer geometry, memory type and per-operation cost counters —
// regenerated fresh on every open(), same convention as
// `/proc/meminfo`/`/proc/kdebug`. See `render_fbinfo` for why it is its
// own file rather than more lines in `/proc/kdebug`.
struct FbInfoInode;

impl Inode for FbInfoInode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        Stat::regular(206, render_fbinfo().len() as i64)
    }

    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        if flags.is_write() {
            return Err(Errno::EROFS);
        }
        Ok(Box::new(ProcFile { data: render_fbinfo().into_bytes(), offset: 0 }))
    }
}

// ── self symlink inode ───────────────────────────────────────────────────────

/// `/proc/self` — always resolves to the *calling* process's own pid, not
/// a fixed target: `readlink()` (and hence `open()`/`stat()` via
/// `resolve()`'s symlink-following) re-queries the current pid every time
/// it's traversed, exactly like the real thing.
struct SelfInode;

impl Inode for SelfInode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        Stat::symlink(202, self.readlink().map(|s| s.len()).unwrap_or(0) as i64)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        // Real Unix: open() on a symlink (without O_NOFOLLOW/O_PATH) opens
        // the target, not the link itself. Nothing in this kernel opens
        // /proc/self directly (only traverses through it, e.g.
        // /proc/self/exe), so this is unreachable in practice — EINVAL is
        // a reasonable stand-in for "not a regular file."
        Err(Errno::EINVAL)
    }

    fn readlink(&self) -> Result<String, Errno> {
        let pid = crate::process::scheduler::current_pid_safe().ok_or(Errno::ENOENT)?;
        Ok(format!("/proc/{}", pid))
    }
}

// ── /proc/<pid> directory inode ──────────────────────────────────────────────

struct ProcPidDirInode {
    pid: usize,
}

impl Inode for ProcPidDirInode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        Stat::dir(pid_dir_ino(self.pid))
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        Ok(Box::new(ProcPidDirHandle { pid: self.pid, offset: 0 }))
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn Inode>, Errno> {
        match name {
            "exe" => Ok(Arc::new(ProcExeInode { pid: self.pid })),
            "stat" => Ok(Arc::new(ProcStatInode { pid: self.pid })),
            _ => Err(Errno::ENOENT),
        }
    }

    fn readdir(&self, offset: u64) -> Result<Option<DirEntry>, Errno> {
        let ino = pid_dir_ino(self.pid);
        match offset {
            0 => Ok(Some(DirEntry::new(ino, FileType::Directory, b"."))),
            1 => Ok(Some(DirEntry::new(ino, FileType::Directory, b".."))),
            2 => Ok(Some(DirEntry::new(pid_exe_ino(self.pid), FileType::Symlink, b"exe"))),
            3 => Ok(Some(DirEntry::new(pid_stat_ino(self.pid), FileType::Regular, b"stat"))),
            _ => Ok(None),
        }
    }
}

// ── /proc/<pid>/stat file inode ──────────────────────────────────────────────

/// See `render_proc_stat`'s doc comment for the format and what backs it.
struct ProcStatInode {
    pid: usize,
}

impl Inode for ProcStatInode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        let len = crate::process::scheduler::proc_stat_snapshot(self.pid)
            .map(|s| render_proc_stat(self.pid, &s).len())
            .unwrap_or(0);
        Stat::regular(pid_stat_ino(self.pid), len as i64)
    }

    fn open(&self, flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        if flags.is_write() {
            return Err(Errno::EROFS);
        }
        let snap = crate::process::scheduler::proc_stat_snapshot(self.pid)
            .ok_or(Errno::ENOENT)?;
        let data = render_proc_stat(self.pid, &snap).into_bytes();
        Ok(Box::new(ProcFile { data, offset: 0 }))
    }
}

struct ProcPidDirHandle {
    pid:    usize,
    offset: u64,
}

impl FileHandle for ProcPidDirHandle {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument)
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument)
    }

    fn getdents64(&mut self, buf: &mut [u8]) -> i64 {
        let dir = ProcPidDirInode { pid: self.pid };
        crate::fs::vfs::getdents64_via_readdir(&dir, &mut self.offset, buf)
    }

    fn stat(&self) -> Option<crate::fs::types::Stat> {
        Some(Stat::dir(pid_dir_ino(self.pid)))
    }

    fn name(&self) -> &str { "procfs/pid-dir" }
}

// ── /proc/<pid>/exe symlink inode ────────────────────────────────────────────

/// The real payload this whole module was added for: a symlink whose
/// target is whatever `PROGRAMS`-registered path `pid` is currently
/// running (`Process::exe_name`, kept up to date by every successful
/// `exec()` — see `syscall::sys_exec`). `execve("/proc/self/exe", ...)`
/// resolves through here via the VFS's normal symlink-following, the same
/// mechanism any other symlink gets — no special-casing left in `sys_exec`
/// itself.
struct ProcExeInode {
    pid: usize,
}

impl Inode for ProcExeInode {
    fn as_any(&self) -> &dyn core::any::Any { self }

    fn stat(&self) -> Stat {
        let len = self.readlink().map(|s| s.len()).unwrap_or(0);
        Stat::symlink(pid_exe_ino(self.pid), len as i64)
    }

    fn open(&self, _flags: OpenFlags) -> Result<Box<dyn FileHandle>, Errno> {
        Err(Errno::EINVAL) // see SelfInode::open's doc comment
    }

    fn readlink(&self) -> Result<String, Errno> {
        match crate::process::scheduler::exe_name_for_pid(self.pid) {
            Some(name) if !name.is_empty() => Ok(name),
            _ => Err(Errno::ENOENT), // pid gone, or never exec'd a real ELF (kernel process)
        }
    }
}

// ── Open file handles ────────────────────────────────────────────────────────

/// Read-only handle over a snapshot generated at `open()` time.
struct ProcFile {
    data:   Vec<u8>,
    offset: usize,
}

impl FileHandle for ProcFile {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        let remaining = &self.data[self.offset..];
        if remaining.is_empty() {
            return Ok(0); // EOF
        }
        let n = buf.len().min(remaining.len());
        buf[..n].copy_from_slice(&remaining[..n]);
        self.offset += n;
        Ok(n)
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::NotSupported)
    }

    fn stat(&self) -> Option<crate::fs::types::Stat> {
        Some(Stat::regular(201, self.data.len() as i64))
    }

    fn name(&self) -> &str { "procfs/meminfo" }
}

/// Directory handle: keeps a readdir cursor and serves `getdents64`.
struct ProcDirHandle {
    offset: u64,
}

impl FileHandle for ProcDirHandle {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument) // directories use getdents64
    }

    fn write(&mut self, _buf: &[u8]) -> FileResult<usize> {
        Err(FileError::InvalidArgument)
    }

    fn getdents64(&mut self, buf: &mut [u8]) -> i64 {
        crate::fs::vfs::getdents64_via_readdir(&ProcDirInode, &mut self.offset, buf)
    }

    fn stat(&self) -> Option<crate::fs::types::Stat> {
        Some(Stat::dir(200))
    }

    fn name(&self) -> &str { "procfs/dir" }
}
