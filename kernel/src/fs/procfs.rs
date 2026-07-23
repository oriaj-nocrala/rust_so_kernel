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
    let buddy = crate::allocator::buddy_allocator::BUDDY.lock();
    let total_kb = buddy.total_bytes() / 1024;
    let free_kb = buddy.free_bytes() / 1024;
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
            n => {
                // Live pids, appended after the always-present entries above
                // — this is what makes `ls /proc` / BusyBox `ps`'s
                // `opendir("/proc")` scan see every process (previously
                // direct lookup like `cat /proc/3/exe` worked but nothing
                // enumerated them, see this module's top doc comment).
                let idx = (n - 6) as usize;
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
