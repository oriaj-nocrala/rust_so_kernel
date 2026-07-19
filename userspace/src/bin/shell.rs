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
#[no_mangle]
extern "C" fn _start() -> ! {
    loop {
        let pid = syscall::fork();
        if pid == 0 {
            let argv: [&[u8]; 2] = [b"busybox\0", b"ash\0"];
            syscall::exec_argv(b"/bin/busybox\0", &argv, &[]);
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
