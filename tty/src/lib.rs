//! `tty` — host-testable core of this kernel's terminals.
//!
//! Eighth crate in the line of `hal`, `ext2`, `mm`, `vfs`, `diag`, `sched`
//! and `usock`, for the same reason: `kernel` cannot run `cargo test` on
//! the host (CLAUDE.md, "QEMU integration tests"), so logic that can speak
//! in plain types moves out here. Phase 3.1 of `docs/gui/gui-plan.md`.
//!
//! - [`termios`]: `struct termios`/`struct winsize` in **this port's** ABI
//!   (not Linux's flag values — see that module).
//! - [`ldisc`]: the line discipline — canonical editing, echo, `ISIG`,
//!   `VMIN`/`VTIME`, output processing.
//! - [`jobctl`]: who may read, write and reconfigure a terminal
//!   (`SIGTTIN`/`SIGTTOU`/`EIO`), `TIOCSCTTY`, `tcsetpgrp`.
//! - [`pty`]: a master/slave pair — hangup, `EIO`/`POLLHUP`, `SIGWINCH`.
//!
//! As in `usock`, **nothing blocks** (an operation that cannot complete
//! says so: [`TtyError::Again`], [`ldisc::Read::Wait`]) and **consequences
//! come back as data** ([`pty::Effects`]: wakeups, signals and their
//! targets). The kernel adapter parks, wakes and signals.

#![no_std]

extern crate alloc;

pub mod jobctl;
pub mod ldisc;
pub mod pty;
pub mod termios;

/// The signals a terminal sends. The kernel maps them to its numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Hup,
    Int,
    Quit,
    Tstp,
    Ttin,
    Ttou,
    Cont,
    Winch,
}

/// Errors, mapped to errno by the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TtyError {
    /// Would block (`EAGAIN` for a non-blocking caller).
    Again,
    /// `EIO`.
    Io,
    /// `EPERM`.
    Perm,
    /// `ENOTTY`.
    NotTty,
}
