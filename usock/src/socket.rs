//! One socket object: its type, its name, its connection state, its queue.
//!
//! This is deliberately just the state — every operation that has to look at
//! *two* sockets at once (send writes into the peer's queue, close notifies
//! the peer, connect pairs two of them) lives in [`crate::table`], which owns
//! both and can therefore reason about them together. A socket that could
//! reach its peer on its own would need a back-pointer, and back-pointers
//! between heap objects are exactly what this kernel's `Arc`-free,
//! index-addressed tables (`ChannelTable`, `FileDescriptorTable`) avoid.

use alloc::vec::Vec;

use crate::addr::UnixAddr;
use crate::queue::RecvQueue;
use crate::SocketId;

/// `SOCK_STREAM` / `SOCK_DGRAM`, the only two types AF_UNIX needs here.
/// (`SOCK_SEQPACKET` is a real third one; it is not implemented — see the
/// crate doc's scope note.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SockType {
    Stream,
    Dgram,
}

/// `shutdown(2)`'s `how`, with Linux's real values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shutdown {
    Read = 0,
    Write = 1,
    Both = 2,
}

impl Shutdown {
    pub fn from_raw(how: i32) -> Option<Self> {
        match how {
            0 => Some(Self::Read),
            1 => Some(Self::Write),
            2 => Some(Self::Both),
            _ => None,
        }
    }

    pub fn shuts_read(self) -> bool {
        matches!(self, Self::Read | Self::Both)
    }

    pub fn shuts_write(self) -> bool {
        matches!(self, Self::Write | Self::Both)
    }
}

/// Default receive-queue capacity, i.e. the effective `SO_RCVBUF`.
///
/// 16 KiB rather than `pipe.rs`'s 4 KiB: a stream socket's queue is a `Vec`
/// that grows only with real traffic, not a fixed array in a struct, so the
/// cost of a larger ceiling is paid only by sockets that actually fill it.
pub const DEFAULT_BUF: usize = 16 * 1024;

/// `SOMAXCONN` — the ceiling `listen()` clamps its backlog to.
pub const SOMAXCONN: usize = 128;

/// What a listening socket has accumulated: connections that completed on the
/// client side and are waiting for `accept()`.
pub(crate) struct Listener {
    pub(crate) backlog: usize,
    pub(crate) pending: Vec<SocketId>,
}

pub struct Socket<F> {
    pub(crate) ty: SockType,
    /// The address `bind()` gave this socket, if any.
    pub(crate) bound: Option<UnixAddr>,
    /// Set by `listen()`; a socket either listens or connects, never both.
    pub(crate) listener: Option<Listener>,
    /// Stream: the connected peer. Dgram: the default destination set by
    /// `connect()`.
    pub(crate) peer: Option<SocketId>,
    /// The peer's address, remembered at connect time so `getpeername()`
    /// still answers after the peer has gone away.
    pub(crate) peer_addr: UnixAddr,
    /// True once the peer socket was closed outright (as opposed to merely
    /// shut down in one direction).
    pub(crate) peer_gone: bool,
    /// `shutdown(SHUT_RD)` / `shutdown(SHUT_WR)` on *this* socket.
    pub(crate) rd_shut: bool,
    pub(crate) wr_shut: bool,
    /// What the peer has written to us.
    pub(crate) rx: RecvQueue<F>,
    /// `SO_SNDBUF`, remembered for `getsockopt` only: with a single queue per
    /// socket the send side's real limit is the *receiver's* `SO_RCVBUF`,
    /// which is where flow control is actually enforced.
    pub(crate) sndbuf: usize,
    /// How many file descriptors refer to this socket (`dup`, `fork`).
    pub(crate) refs: usize,
    /// `SO_ERROR`: a pending error, taken by the next `getsockopt`.
    pub(crate) so_error: i32,
}

impl<F> Socket<F> {
    pub(crate) fn new(ty: SockType) -> Self {
        Self {
            ty,
            bound: None,
            listener: None,
            peer: None,
            peer_addr: UnixAddr::Unnamed,
            peer_gone: false,
            rd_shut: false,
            wr_shut: false,
            rx: RecvQueue::new(DEFAULT_BUF),
            sndbuf: DEFAULT_BUF,
            refs: 1,
            so_error: 0,
        }
    }

    pub fn sock_type(&self) -> SockType {
        self.ty
    }

    pub fn is_listening(&self) -> bool {
        self.listener.is_some()
    }

    pub fn is_connected(&self) -> bool {
        self.peer.is_some()
    }

    /// The address `getsockname()` reports.
    pub fn sockname(&self) -> UnixAddr {
        self.bound.clone().unwrap_or(UnixAddr::Unnamed)
    }

    pub fn peername(&self) -> UnixAddr {
        self.peer_addr.clone()
    }

    pub fn queued_bytes(&self) -> usize {
        self.rx.bytes()
    }

    /// The socket this one is paired with, if any. The kernel adapter needs
    /// it to park a blocked sender on the queue that actually has to drain —
    /// the *peer's*, not its own.
    pub fn peer_id(&self) -> Option<SocketId> {
        self.peer
    }
}

/// What `poll`/`epoll` needs to know about one socket, in the crate's own
/// vocabulary — the kernel adapter turns this into `POLLIN`/`POLLOUT`/
/// `POLLHUP` bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PollMask {
    /// Data queued, an EOF to report, or (on a listener) a connection to accept.
    pub readable: bool,
    /// A `send()` would make progress right now.
    pub writable: bool,
    /// The peer is gone or both directions are shut down.
    pub hup: bool,
    /// A pending `SO_ERROR`.
    pub err: bool,
}
