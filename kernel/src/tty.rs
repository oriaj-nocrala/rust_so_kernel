// kernel/src/tty.rs
//
// Single global "console tty" state: real termios (get/set backs
// tcgetattr/tcsetattr via ioctl TCGETS/TCSETS*, see process::syscall::
// sys_ioctl) and the foreground process group (tcgetpgrp/tcsetpgrp via
// ioctl TIOCGPGRP/TIOCSPGRP) — this kernel has exactly one tty (the
// framebuffer+serial console), so a single global pair of statics is
// enough instead of a per-device table.
//
// ISIG line discipline: `feed_input` is the single choke point both the
// PS/2 keyboard ISR (`keyboard.rs`) and the COM1 serial ISR
// (`init::devices::serial_interrupt_handler`) route every incoming byte
// through before pushing it into `keyboard_buffer::KEYBOARD_BUFFER`. When
// ISIG is set and the byte matches VINTR/VQUIT/VSUSP, it's turned into a
// real signal delivered to the foreground process group instead of being
// queued as input — the same job a real Unix tty driver's line discipline
// does. ICANON/ECHO are stored (so tcgetattr/tcsetattr round-trip
// correctly and nothing errors out) but not actually implemented in the
// kernel: line editing and echo stay userspace's job, same as before this
// existed (see userspace/src/bin/shell.rs) — ash's own line editor does
// the same once it puts the tty in raw mode via tcsetattr.

use core::sync::atomic::{AtomicU32, Ordering};
use spin::Mutex;

pub const NCCS: usize = 11;

pub const VEOF: usize = 0;
pub const VEOL: usize = 1;
pub const VERASE: usize = 2;
pub const VINTR: usize = 3;
pub const VKILL: usize = 4;
pub const VMIN: usize = 5;
pub const VQUIT: usize = 6;
pub const VSTART: usize = 7;
pub const VSTOP: usize = 8;
pub const VSUSP: usize = 9;
pub const VTIME: usize = 10;

pub const ISIG: u32 = 0x0040;

/// Matches `mlibc-port/constanos-sysdeps/include/abi-bits/termios.h`'s
/// `struct termios` byte-for-byte (`cc_t`/`tcflag_t`/`speed_t` are all
/// `unsigned int` in this port's ABI, not `unsigned char` like real POSIX)
/// — 68 bytes, no padding, so this can be copied straight to/from a user
/// pointer via `sys_ioctl`'s TCGETS/TCSETS* handling.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Termios {
    pub c_iflag: u32,
    pub c_oflag: u32,
    pub c_cflag: u32,
    pub c_lflag: u32,
    pub c_cc: [u32; NCCS],
    pub ibaud: u32,
    pub obaud: u32,
}

const fn default_termios() -> Termios {
    let mut cc = [0u32; NCCS];
    cc[VEOF] = 0x04;   // Ctrl-D
    cc[VERASE] = 0x7f; // DEL
    cc[VINTR] = 0x03;  // Ctrl-C
    cc[VKILL] = 0x15;  // Ctrl-U
    cc[VMIN] = 1;
    cc[VQUIT] = 0x1c;  // Ctrl-\
    cc[VSTART] = 0x11; // Ctrl-Q
    cc[VSTOP] = 0x13;  // Ctrl-S
    cc[VSUSP] = 0x1a;  // Ctrl-Z
    Termios {
        c_iflag: 0x0402, // ICRNL | IXON
        c_oflag: 0x0003, // OPOST | ONLCR
        c_cflag: 0x000B, // CS8 | CREAD
        c_lflag: 0x0077, // ISIG | ICANON | ECHO | ECHOE | ECHOK | IEXTEN
        c_cc: cc,
        ibaud: 0,
        obaud: 0,
    }
}

pub static TERMIOS: Mutex<Termios> = Mutex::new(default_termios());

/// Foreground process group of the console tty (job control). Whatever
/// group is foreground gets SIGINT/SIGQUIT/SIGTSTP from the keyboard/serial
/// input line discipline; every other group is background. Set once at
/// boot to the shell's own pgid (`init::processes::create_user_processes`),
/// then only ever changed by `TIOCSPGRP` (the shell's own `tcsetpgrp()`,
/// e.g. around running a foreground job).
pub static FOREGROUND_PGID: AtomicU32 = AtomicU32::new(0);

/// Feed one raw input byte through the tty's line discipline. Returns
/// `true` if it should be queued as ordinary input (push into
/// `keyboard_buffer::KEYBOARD_BUFFER` as before), `false` if it was
/// consumed here as a signal.
pub fn feed_input(c: char) -> bool {
    let byte = c as u32;
    let (isig, intr, quit, susp) = {
        let t = TERMIOS.lock();
        (t.c_lflag & ISIG != 0, t.c_cc[VINTR], t.c_cc[VQUIT], t.c_cc[VSUSP])
    };
    if !isig {
        return true;
    }

    let sig = if byte == intr {
        crate::process::signal::SIGINT
    } else if byte == quit {
        crate::process::signal::SIGQUIT
    } else if byte == susp {
        crate::process::signal::SIGTSTP
    } else {
        return true;
    };

    let pgid = FOREGROUND_PGID.load(Ordering::Relaxed);
    if pgid != 0 {
        crate::process::syscall::send_to_group(pgid, sig);
    }
    false
}
