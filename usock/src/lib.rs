//! `usock` — host-testable core of this kernel's AF_UNIX sockets.
//!
//! Sixth crate in the same line as `hal`, `ext2`, `mm`, `vfs`, `diag` and
//! `sched`, and for the same reason: `kernel` itself cannot run `cargo test`
//! on the host (see CLAUDE.md's "QEMU integration tests" — `-Z build-std`
//! plus a double build of the `kernel` bin target collides on `core`'s lang
//! items), so logic that can speak in plain types instead of this kernel's
//! concrete globals moves out here, where `cargo test` reaches it.
//!
//! ## What this replaces
//!
//! `kernel/src/ipc/channel.rs`: a bespoke IPC of 64-byte fixed messages whose
//! `socket()` took no arguments at all — no domain, no type, no `sockaddr`,
//! no `listen()`. Nothing about it was AF_UNIX except the syscall numbers it
//! borrowed. What lives here is the real thing: `SOCK_STREAM` byte streams
//! and `SOCK_DGRAM` datagrams, `bind`/`listen`/`accept` with a backlog,
//! `socketpair`, half-close, the abstract namespace, and `SCM_RIGHTS`
//! descriptor passing.
//!
//! ## The shape of the API
//!
//! Everything hangs off [`SocketTable`], an index-addressed table of
//! [`Socket`]s — no `Arc`, no back-pointers, the same shape as the kernel's
//! `FileDescriptorTable`. Operations that need two sockets at once (a send
//! writes into the peer's queue; a close notifies the peer) are methods on
//! the table, which owns both.
//!
//! Two deliberate properties make this testable without a kernel:
//!
//! 1. **Nothing here blocks.** An operation that cannot complete returns
//!    [`SockError::Again`]; the kernel adapter is what turns that into a
//!    `FileError::WouldBlock` and parks the process, exactly as
//!    `process/pipe.rs` already does. A test just observes the `Again`.
//! 2. **Wakeups come back as data.** Every mutating operation returns
//!    [`Wakes`] — which sockets became readable, writable, or acceptable —
//!    instead of calling into a scheduler. Same technique as `mm`'s
//!    `PhantomEvent`/`AllocEvent`: the condition is reported, the reaction
//!    belongs to the adapter.
//!
//! The `F` type parameter is the file-descriptor payload carried by
//! `SCM_RIGHTS`. The kernel instantiates `SocketTable<Box<dyn FileHandle>>`;
//! tests here use integers. Nothing in this crate interprets an `F`, which is
//! what lets fd passing be tested at all — the same genericity seam
//! `sched::SchedCore<Process>` uses for `Process`.
//!
//! ## Out of scope, on purpose
//!
//! `SOCK_SEQPACKET`, `SO_PEERCRED`/`SCM_CREDENTIALS` (this kernel has no uid
//! model — every process is root), `MSG_OOB` (AF_UNIX has no out-of-band
//! data in Linux either), and non-blocking `connect()` handshakes
//! (`EINPROGRESS`): an AF_UNIX stream connect completes or fails
//! immediately here, because the server side of the pair is created by
//! `connect()` itself rather than by `accept()`, exactly as Linux's
//! `unix_stream_connect` does.

#![no_std]

extern crate alloc;

pub mod addr;
pub mod queue;
pub mod socket;
pub mod table;

pub use addr::{UnixAddr, AF_UNIX, SOCKADDR_UN_LEN, SUN_PATH_LEN};
pub use queue::DgramRead;
pub use socket::{PollMask, Shutdown, SockType, Socket, DEFAULT_BUF, SOMAXCONN};
pub use table::{
    AcceptOutcome, CloseOutcome, ConnectOutcome, RecvOutcome, SendOutcome, SocketTable, Wakes,
};

/// Index of a socket in a [`SocketTable`].
///
/// Ids start at 1 so that 0 can stay the "not a socket" sentinel the kernel's
/// fd side tables use.
pub type SocketId = usize;

/// Every failure this crate can report, named after the errno the kernel
/// returns for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SockError {
    /// `EAGAIN` — would block. The kernel turns this into a real block.
    Again,
    /// `EINVAL`
    Inval,
    /// `EBADF` — no such socket id.
    BadF,
    /// `ENOTSOCK`
    NotSock,
    /// `ENOTCONN`
    NotConn,
    /// `EISCONN`
    IsConn,
    /// `ECONNREFUSED` — nothing listening at that address.
    ConnRefused,
    /// `EADDRINUSE`
    AddrInUse,
    /// `EADDRNOTAVAIL`
    AddrNotAvail,
    /// `ENOENT` — the address names nothing at all.
    NoEnt,
    /// `EPIPE` — writing to a socket whose read side is gone.
    Pipe,
    /// `ENOMEM`
    NoMem,
    /// `EOPNOTSUPP` — e.g. `listen()` on a datagram socket.
    OpNotSupp,
    /// `EPROTOTYPE` — connecting a stream socket to a datagram one.
    ProtoType,
    /// `EAFNOSUPPORT` — an address whose family is not `AF_UNIX`.
    AfNoSupport,
    /// `EDESTADDRREQ` — an unconnected datagram send with no destination.
    DestAddrReq,
    /// `EMSGSIZE` — a datagram larger than the receive buffer can ever hold.
    MsgSize,
}

impl SockError {
    /// The positive errno value. The kernel negates it for its syscall ABI.
    pub fn errno(self) -> i32 {
        match self {
            Self::Again => 11,
            Self::Inval => 22,
            Self::BadF => 9,
            Self::NotSock => 88,
            Self::NotConn => 107,
            Self::IsConn => 106,
            Self::ConnRefused => 111,
            Self::AddrInUse => 98,
            Self::AddrNotAvail => 99,
            Self::NoEnt => 2,
            Self::Pipe => 32,
            Self::NoMem => 12,
            Self::OpNotSupp => 95,
            Self::ProtoType => 91,
            Self::AfNoSupport => 97,
            Self::DestAddrReq => 89,
            Self::MsgSize => 90,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errno_values_are_the_linux_ones() {
        // These cross the syscall boundary into mlibc's <errno.h>; a BSD-style
        // table (ENOTSOCK = 38) silently makes every socket error mean
        // something else in userspace. Pin the handful this crate can return.
        assert_eq!(SockError::Again.errno(), 11);
        assert_eq!(SockError::NotSock.errno(), 88);
        assert_eq!(SockError::AddrInUse.errno(), 98);
        assert_eq!(SockError::NotConn.errno(), 107);
        assert_eq!(SockError::ConnRefused.errno(), 111);
        assert_eq!(SockError::OpNotSupp.errno(), 95);
    }
}
