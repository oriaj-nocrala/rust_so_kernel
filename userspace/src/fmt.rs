//! Small no-alloc formatting helpers: a fixed stack buffer that implements
//! `core::fmt::Write`, plus a `print!`/`println!`-style macro that writes
//! straight to a file descriptor via `syscall::write`.

use core::fmt::{self, Write};

pub struct FdWriter {
    pub fd: i32,
}

impl Write for FdWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut off = 0;
        let bytes = s.as_bytes();
        while off < bytes.len() {
            let n = crate::syscall::write(self.fd, &bytes[off..]);
            if n <= 0 {
                return Err(fmt::Error);
            }
            off += n as usize;
        }
        Ok(())
    }
}

/// Formats into a fixed 256-byte stack buffer and writes the result to `fd`.
pub fn fprint(fd: i32, args: fmt::Arguments) {
    let mut w = FdWriter { fd };
    let _ = w.write_fmt(args);
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::fmt::fprint(1, format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => {
        $crate::fmt::fprint(1, format_args!($($arg)*));
        $crate::fmt::fprint(1, format_args!("\n"));
    };
}

#[macro_export]
macro_rules! eprintln {
    ($($arg:tt)*) => {
        $crate::fmt::fprint(2, format_args!($($arg)*));
        $crate::fmt::fprint(2, format_args!("\n"));
    };
}
