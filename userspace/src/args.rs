//! argv for Rust programs.
//!
//! The kernel builds a SysV initial stack (`memory/elf_loader.rs::
//! build_initial_stack`): at entry `rsp` points at `argc`, then `argc`
//! pointers to NUL-terminated strings. A plain `extern "C" fn _start` has
//! already moved `rsp` in its prologue by the time it runs, so programs
//! that want their arguments use [`entry!`](crate::entry), whose `_start`
//! is naked and hands the original `rsp` over.

/// The program's arguments, `argv[0]` included.
#[derive(Clone, Copy)]
pub struct Args {
    argc: usize,
    argv: *const *const u8,
}

impl Args {
    /// # Safety
    /// `sp` must be the `rsp` the kernel entered the program with.
    pub unsafe fn from_stack(sp: *const usize) -> Args {
        Args { argc: *sp, argv: sp.add(1) as *const *const u8 }
    }

    pub fn len(&self) -> usize {
        self.argc
    }

    pub fn is_empty(&self) -> bool {
        self.argc == 0
    }

    /// Argument `i` with its terminating NUL — ready for `exec_argv`.
    pub fn get_cstr(&self, i: usize) -> Option<&'static [u8]> {
        if i >= self.argc {
            return None;
        }
        unsafe {
            let p = *self.argv.add(i);
            let mut n = 0;
            while *p.add(n) != 0 {
                n += 1;
            }
            Some(core::slice::from_raw_parts(p, n + 1))
        }
    }

    /// Argument `i` without the NUL.
    pub fn get(&self, i: usize) -> Option<&'static [u8]> {
        self.get_cstr(i).map(|s| &s[..s.len() - 1])
    }
}

/// Defines `_start` for a program whose entry point is
/// `fn main(args: Args) -> i32`; its return value is the exit status.
#[macro_export]
macro_rules! entry {
    ($main:path) => {
        #[no_mangle]
        extern "C" fn __userspace_start(sp: *const usize) -> ! {
            let args = unsafe { $crate::args::Args::from_stack(sp) };
            $crate::syscall::exit($main(args))
        }

        #[unsafe(naked)]
        #[no_mangle]
        pub extern "C" fn _start() -> ! {
            core::arch::naked_asm!(
                "mov rdi, rsp",
                "and rsp, -16",
                "call {start}",
                "ud2",
                start = sym __userspace_start,
            )
        }
    };
}
