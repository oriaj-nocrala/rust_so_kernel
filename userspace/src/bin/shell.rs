#![no_std]
#![no_main]

use userspace::{println, syscall};

/// PID 1. The only process spawned automatically at boot (see
/// `kernel/src/init/processes.rs::create_user_processes` — it's still
/// looked up by the literal name `"shell"`, unchanged even though this is
/// no longer an interactive shell itself). All it does is fork+exec
/// BusyBox `ash` (real job control, line editing, standalone/nofork applet
/// dispatch — see the busybox-readiness session notes) and wait for it —
/// respawning it if it ever exits, whether from its own `exit`, Ctrl-D, or
/// a crash, so a bug in `ash` costs a fresh shell instead of a system with
/// no way to type anything short of a reboot. This replaces the old
/// dual-purpose version of this file, which also carried a hand-rolled
/// fallback REPL (`cmd_ls`/`cmd_write`/`cmd_sh`/...); that duplicated what
/// `ash` already does, and bit-rotted from disuse once `ash` became the
/// stable default. `ash` is also the tty's foreground process group
/// (`kernel/src/init/processes.rs` sets it to this process's own pid,
/// which `fork()` preserves for the child), so Ctrl-C/Ctrl-Z reach it
/// correctly.
/// Real BusyBox `--install`, run once before the first `ash`: creates an
/// actual `symlink()` per compiled-in applet under `/tmp/bin` (the one
/// writable mount — `/bin` itself is initramfs, read-only, backed by
/// embedded ELFs baked in at kernel build time). This is the *real*
/// mechanism a real Linux install uses (one multi-call binary + real
/// symlinks + argv[0] dispatch) — not the synthetic, compile-time-computed
/// symlinks `fs::initramfs::BusyboxAppletInode` serves under `/bin`
/// (those exist because `/bin` can't be written to at runtime; this is
/// what "actually installing it" looks like on the one mount that can be).
/// `busybox --install` itself doesn't create the target directory, so
/// `mkdir` runs first — ignoring the error is fine, since a fresh boot's
/// ramfs is always empty (the failure mode here would only ever be
/// `EEXIST`, and even that can't happen before the first `ash` starts).
fn install_busybox_symlinks() {
    syscall::mkdir(b"/tmp/bin\0");

    let pid = syscall::fork();
    if pid == 0 {
        let argv: [&[u8]; 4] = [b"busybox\0", b"--install\0", b"-s\0", b"/tmp/bin\0"];
        syscall::exec_argv(b"/bin/busybox\0", &argv, &[]);
        println!("init: exec busybox --install failed");
        syscall::exit(1);
    } else if pid > 0 {
        syscall::waitpid(pid);
    } else {
        println!("init: fork failed ({}) installing busybox symlinks", pid);
    }
}

#[no_mangle]
extern "C" fn _start() -> ! {
    install_busybox_symlinks();

    loop {
        let pid = syscall::fork();
        if pid == 0 {
            let argv: [&[u8]; 2] = [b"busybox\0", b"ash\0"];
            // /mnt/bin holds the userspace programs that were moved off the
            // kernel binary onto the ext2 disk image (doom, quake, and most
            // of the old C test programs — see kernel/build.rs's module doc
            // comment and CLAUDE.md's Userspace Programs section). Listed
            // last: /tmp/bin (busybox applet symlinks) and /bin (initramfs)
            // should win on any name collision, same as before.
            let envp: [&[u8]; 1] = [b"PATH=/tmp/bin:/bin:/mnt/bin\0"];
            syscall::exec_argv(b"/bin/busybox\0", &argv, &envp);
            // Only reached if exec failed.
            println!("init: exec /bin/busybox failed");
            syscall::exit(1);
        } else if pid > 0 {
            syscall::waitpid(pid);
            println!("init: ash exited, respawning");
        } else {
            println!("init: fork failed ({}), retrying", pid);
            syscall::nanosleep(500_000_000);
        }
    }
}
