//! Starting graphical programs from the compositor and the panel.
//!
//! [`spawn`] runs a command line — a program name or path and its
//! arguments, split at spaces — as a client of the compositor: a bare name
//! is looked for in [`SEARCH`], the child gets a process group of its own
//! with the default `^C`/`^Z`, closes every fd above 2 and gets
//! `GUI_DISPLAY`. The caller reaps (`syscall::reap_any`).

use alloc::vec::Vec;

use crate::syscall;

/// Where a bare program name is looked for, in order — ash's `PATH` minus
/// `/tmp/bin` (BusyBox applets are not graphical): the embedded programs,
/// then the disk's, where everything linking `userspace::text` lives.
pub const SEARCH: [&[u8]; 2] = [b"/bin/", b"/mnt/bin/"];

/// How a program finds the compositor (read by `constanos_gfx.h`, handed
/// on by `term` to its shell), as `WAYLAND_DISPLAY` is.
pub const GUI_DISPLAY_ENV: &[u8] = b"GUI_DISPLAY=/tmp/gui-0\0";

/// `name` as a NUL-terminated path: as given if it contains a `/`, else
/// the first of [`SEARCH`] that has it. `None` if nothing is there.
pub fn find(name: &[u8]) -> Option<Vec<u8>> {
    let prefixes: &[&[u8]] = if name.contains(&b'/') { &[b""] } else { &SEARCH };
    for prefix in prefixes {
        let mut p = Vec::with_capacity(prefix.len() + name.len() + 1);
        p.extend_from_slice(prefix);
        p.extend_from_slice(name);
        p.push(0);
        if syscall::stat(&p).is_ok() {
            return Some(p);
        }
    }
    None
}

/// Starts `cmdline` (see the module doc). Returns the child's pid, or a
/// negative errno: `-2` (`ENOENT`) when the program is nowhere.
pub fn spawn(cmdline: &[u8]) -> i64 {
    let mut args: Vec<Vec<u8>> = cmdline
        .split(|&b| b == b' ' || b == b'\t')
        .filter(|a| !a.is_empty())
        .map(|a| {
            let mut v = a.to_vec();
            v.push(0);
            v
        })
        .collect();
    let Some(first) = args.first() else { return -2 };
    let Some(path) = find(&first[..first.len() - 1]) else { return -2 };
    args[0] = path.clone();
    let pid = syscall::fork();
    if pid == 0 {
        // A group of its own with the default ^C/^Z: the console's
        // foreground group is the compositor's, and a key typed into a
        // terminal window would otherwise reach the client twice — once
        // through the window and once as a console signal.
        syscall::setpgid(0, 0);
        syscall::sigaction(syscall::SIGINT, 0);
        syscall::sigaction(syscall::SIGTSTP, 0);
        // Nothing past stdio goes to the client: the kernel does not act on
        // close-on-exec, and a child holding the compositor's /dev/fb0 or
        // grabbed event0 would keep graphics mode and the keyboard after
        // it dies.
        for fd in 3..16 {
            syscall::close(fd);
        }
        let argv: Vec<&[u8]> = args.iter().map(|a| a.as_slice()).collect();
        syscall::exec_argv(&path, &argv, &[GUI_DISPLAY_ENV]);
        syscall::exit(127);
    }
    pid
}
