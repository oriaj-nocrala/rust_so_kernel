//! `struct termios` and `struct winsize` as this port's userspace sees them.
//!
//! The flag values are **this port's**, not Linux's:
//! `mlibc-port/constanos-sysdeps/include/abi-bits/termios.h` came from
//! mlibc's old generic ABI (`ISIG = 0x40`, where Linux has `1`), and
//! `cc_t`/`tcflag_t`/`speed_t` are all `unsigned int` — 68 bytes, no
//! padding. Every constant here has to agree with that header, because the
//! kernel copies this struct straight to and from user memory.

/// Number of control characters. Eleven, so there is no `VWERASE`,
/// `VREPRINT` or `VLNEXT` in this ABI.
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

// c_iflag
pub const BRKINT: u32 = 0x0001;
pub const ICRNL: u32 = 0x0002;
pub const IGNBRK: u32 = 0x0004;
pub const IGNCR: u32 = 0x0008;
pub const IGNPAR: u32 = 0x0010;
pub const INLCR: u32 = 0x0020;
pub const INPCK: u32 = 0x0040;
pub const ISTRIP: u32 = 0x0080;
pub const IXANY: u32 = 0x0100;
pub const IXOFF: u32 = 0x0200;
pub const IXON: u32 = 0x0400;
pub const PARMRK: u32 = 0x0800;

// c_oflag
pub const OPOST: u32 = 0x0001;
pub const ONLCR: u32 = 0x0002;
pub const OCRNL: u32 = 0x0004;
pub const ONOCR: u32 = 0x0008;
pub const ONLRET: u32 = 0x0010;

// c_cflag
pub const CS8: u32 = 0x0003;
pub const CREAD: u32 = 0x0008;

// c_lflag
pub const ECHO: u32 = 0x0001;
pub const ECHOE: u32 = 0x0002;
pub const ECHOK: u32 = 0x0004;
pub const ECHONL: u32 = 0x0008;
pub const ICANON: u32 = 0x0010;
pub const IEXTEN: u32 = 0x0020;
pub const ISIG: u32 = 0x0040;
pub const NOFLSH: u32 = 0x0080;
pub const TOSTOP: u32 = 0x0100;

/// `_POSIX_VDISABLE`: a control character set to this value is off. Linux
/// uses `'\0'` too.
pub const VDISABLE: u32 = 0;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Termios {
    pub c_iflag: u32,
    pub c_oflag: u32,
    pub c_cflag: u32,
    pub c_lflag: u32,
    pub c_cc: [u32; NCCS],
    pub ibaud: u32,
    pub obaud: u32,
}

const _: () = assert!(core::mem::size_of::<Termios>() == 68);

impl Termios {
    /// What a freshly opened terminal starts with: canonical, echoing,
    /// signals on, `\r` → `\n` in and `\n` → `\r\n` out. The same values
    /// the console has always reported.
    pub const fn sane() -> Self {
        let mut cc = [VDISABLE; NCCS];
        cc[VEOF] = 0x04; // ^D
        cc[VERASE] = 0x7f; // DEL
        cc[VINTR] = 0x03; // ^C
        cc[VKILL] = 0x15; // ^U
        cc[VMIN] = 1;
        cc[VQUIT] = 0x1c; // ^\
        cc[VSTART] = 0x11; // ^Q
        cc[VSTOP] = 0x13; // ^S
        cc[VSUSP] = 0x1a; // ^Z
        Termios {
            c_iflag: ICRNL | IXON,
            c_oflag: OPOST | ONLCR,
            c_cflag: CS8 | CREAD,
            c_lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | IEXTEN,
            c_cc: cc,
            ibaud: 0,
            obaud: 0,
        }
    }

    pub fn iflag(&self, f: u32) -> bool {
        self.c_iflag & f != 0
    }
    pub fn oflag(&self, f: u32) -> bool {
        self.c_oflag & f != 0
    }
    pub fn lflag(&self, f: u32) -> bool {
        self.c_lflag & f != 0
    }

    /// Whether `byte` is control character `idx`, which must not be
    /// disabled. A `VEOL` of 0 must not turn every NUL into a line end.
    pub fn is_cc(&self, idx: usize, byte: u8) -> bool {
        let v = self.c_cc[idx];
        v != VDISABLE && v == byte as u32
    }
}

impl Default for Termios {
    fn default() -> Self {
        Self::sane()
    }
}

/// `struct winsize`, Linux's layout (four `unsigned short`s).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Winsize {
    pub rows: u16,
    pub cols: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

const _: () = assert!(core::mem::size_of::<Winsize>() == 8);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sane_matches_what_the_console_always_reported() {
        let t = Termios::sane();
        assert_eq!(t.c_iflag, 0x0402);
        assert_eq!(t.c_oflag, 0x0003);
        assert_eq!(t.c_cflag, 0x000B);
        assert_eq!(t.c_lflag, 0x0077);
    }

    #[test]
    fn a_disabled_control_character_matches_nothing() {
        let t = Termios::sane();
        assert_eq!(t.c_cc[VEOL], VDISABLE);
        assert!(!t.is_cc(VEOL, 0));
        assert!(t.is_cc(VINTR, 0x03));
    }
}
