//! The socket table: every operation that spans two sockets.
//!
//! ## Why a table and not `Arc<Mutex<Socket>>` graphs
//!
//! A connected pair is a cycle: each side points at the other. With `Arc`
//! that cycle leaks unless one side is a `Weak`, and picking which is
//! arbitrary. Indices into one owning table make the cycle a non-issue, let
//! `close()` walk everything it has to notify, and keep the whole state of
//! the subsystem inspectable in one place — the same reasoning behind
//! `ChannelTable` before it and `FileDescriptorTable` beside it.
//!
//! ## Wakeups are returned, not performed
//!
//! Nothing here touches a scheduler. Operations hand back [`Wakes`]: the ids
//! of sockets that just became readable, writable, or acceptable. The kernel
//! adapter maps those to blocked processes. This is what makes "a send wakes
//! the blocked receiver" a host test rather than a QEMU boot.
//!
//! ## Where a stream connection is born
//!
//! `connect()` — not `accept()` — creates the server-side socket, pairs it
//! with the client, and queues it on the listener. That is how Linux's
//! `unix_stream_connect` works, and it is what makes a connect complete
//! immediately: a client may `write()` before the server has ever called
//! `accept()`, and those bytes are already sitting in the queued socket when
//! it does.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::addr::UnixAddr;
use crate::socket::{Listener, PollMask, Shutdown, SockType, Socket, SOMAXCONN};
use crate::{SockError, SocketId};

/// Sockets whose readiness changed, for the adapter to act on.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Wakes {
    /// Became readable — data arrived, or an EOF is now reportable.
    pub readable: Vec<SocketId>,
    /// Became writable — queue space freed, or the peer went away (so a
    /// blocked writer must wake up to see `EPIPE`).
    pub writable: Vec<SocketId>,
    /// A listener with a new pending connection.
    pub acceptable: Vec<SocketId>,
}

impl Wakes {
    fn readable(id: SocketId) -> Self {
        Self { readable: alloc::vec![id], ..Default::default() }
    }

    fn push_readable(&mut self, id: SocketId) {
        if !self.readable.contains(&id) {
            self.readable.push(id);
        }
    }

    fn push_writable(&mut self, id: SocketId) {
        if !self.writable.contains(&id) {
            self.writable.push(id);
        }
    }

    fn push_acceptable(&mut self, id: SocketId) {
        if !self.acceptable.contains(&id) {
            self.acceptable.push(id);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.readable.is_empty() && self.writable.is_empty() && self.acceptable.is_empty()
    }
}

pub struct ConnectOutcome {
    /// The socket this one is now paired with (a stream's server-side
    /// socket, or a datagram's default destination).
    pub peer: SocketId,
    pub wakes: Wakes,
}

pub struct AcceptOutcome {
    /// The new server-side socket, already connected.
    pub id: SocketId,
    /// The connecting socket's address — usually `Unnamed`, since a client
    /// rarely binds.
    pub peer_addr: UnixAddr,
    /// The listener, reported writable: accepting frees a backlog slot, and
    /// a `connect()` that blocked on a full backlog is waiting for exactly
    /// that.
    pub wakes: Wakes,
}

pub struct SendOutcome {
    pub written: usize,
    pub wakes: Wakes,
}

pub struct RecvOutcome<F> {
    pub n: usize,
    /// The whole message's length. Equal to `n` for a stream; larger for a
    /// truncated datagram (what `MSG_TRUNC` reports).
    pub full_len: usize,
    /// Descriptors received via `SCM_RIGHTS`.
    pub fds: Vec<F>,
    /// The sender's address (datagrams only).
    pub from: Option<UnixAddr>,
    pub wakes: Wakes,
}

pub struct CloseOutcome<F> {
    /// Descriptors that were still in flight and must now be closed by the
    /// caller — an undelivered `SCM_RIGHTS` otherwise leaks its open file
    /// description forever.
    pub fds: Vec<F>,
    pub wakes: Wakes,
    /// True when this was the last reference and the socket is really gone.
    pub released: bool,
}

pub struct SocketTable<F> {
    /// Slot 0 is never used: id 0 is the kernel's "not a socket" sentinel.
    slots: Vec<Option<Socket<F>>>,
    /// Bound addresses → the socket holding them. Filesystem paths and
    /// abstract names share one registry; they cannot collide, being
    /// different `UnixAddr` variants.
    binds: BTreeMap<UnixAddr, SocketId>,
    /// Counter behind Linux's autobind names (`\0` + 5 hex digits).
    autobind_next: u32,
}

impl<F> Default for SocketTable<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F> SocketTable<F> {
    pub const fn new() -> Self {
        Self {
            slots: Vec::new(),
            binds: BTreeMap::new(),
            autobind_next: 1,
        }
    }

    // ── lookup helpers ──────────────────────────────────────────────────

    pub fn get(&self, id: SocketId) -> Option<&Socket<F>> {
        self.slots.get(id).and_then(|s| s.as_ref())
    }

    fn sock(&self, id: SocketId) -> Result<&Socket<F>, SockError> {
        self.get(id).ok_or(SockError::BadF)
    }

    fn sock_mut(&mut self, id: SocketId) -> Result<&mut Socket<F>, SockError> {
        self.slots.get_mut(id).and_then(|s| s.as_mut()).ok_or(SockError::BadF)
    }

    /// Number of live sockets — for tests and for `/proc`-style reporting.
    pub fn live_count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    // ── lifecycle ───────────────────────────────────────────────────────

    pub fn create(&mut self, ty: SockType) -> Result<SocketId, SockError> {
        let sock = Socket::new(ty);
        if self.slots.is_empty() {
            self.slots.push(None); // reserve id 0
        }
        if let Some(id) = (1..self.slots.len()).find(|&i| self.slots[i].is_none()) {
            self.slots[id] = Some(sock);
            return Ok(id);
        }
        self.slots.push(Some(sock));
        Ok(self.slots.len() - 1)
    }

    /// Another descriptor now refers to this socket (`dup`, `fork`).
    pub fn retain(&mut self, id: SocketId) -> Result<(), SockError> {
        self.sock_mut(id)?.refs += 1;
        Ok(())
    }

    /// Drop one descriptor's reference. The socket only really goes away —
    /// and only then notifies its peer — when the last one is gone.
    pub fn close(&mut self, id: SocketId) -> CloseOutcome<F> {
        let mut out = CloseOutcome { fds: Vec::new(), wakes: Wakes::default(), released: false };

        match self.slots.get_mut(id).and_then(|s| s.as_mut()) {
            Some(s) => {
                s.refs -= 1;
                if s.refs > 0 {
                    return out;
                }
            }
            None => return out,
        }

        // Releasing a listener releases every connection queued on it that
        // nobody ever accepted, so this is a walk, not a single free.
        let mut stack = alloc::vec![id];
        while let Some(cur) = stack.pop() {
            let Some(sock) = self.slots.get_mut(cur).and_then(|s| s.take()) else { continue };
            out.released = true;

            if let Some(addr) = &sock.bound {
                if self.binds.get(addr) == Some(&cur) {
                    self.binds.remove(addr);
                }
            }

            let mut rx = sock.rx;
            out.fds.append(&mut rx.drain_fds());

            if let Some(l) = sock.listener {
                for child in l.pending {
                    // A pending connection is owned solely by the queue, so
                    // its single reference dies with the listener.
                    if let Some(c) = self.slots.get_mut(child).and_then(|s| s.as_mut()) {
                        c.refs -= 1;
                        if c.refs == 0 {
                            stack.push(child);
                        }
                    }
                }
            }

            // Anyone still pointing at this socket loses their peer. A
            // datagram socket may have been `connect()`ed by several others,
            // so this is a sweep rather than a single back-pointer — and it
            // is what keeps a recycled id from ever being mistaken for the
            // socket that used to hold it.
            for (other_id, slot) in self.slots.iter_mut().enumerate() {
                let Some(other) = slot.as_mut() else { continue };
                if other.peer == Some(cur) {
                    other.peer = None;
                    other.peer_gone = true;
                    out.wakes.push_readable(other_id);
                    out.wakes.push_writable(other_id);
                }
            }
        }

        out
    }

    // ── naming ──────────────────────────────────────────────────────────

    /// `bind(2)`. The caller (the kernel adapter) is responsible for the
    /// filesystem side of a pathname bind — creating the socket node, and
    /// rejecting a path that already exists — before calling this.
    pub fn bind(&mut self, id: SocketId, addr: UnixAddr) -> Result<(), SockError> {
        if !addr.is_named() {
            return Err(SockError::Inval);
        }
        {
            let s = self.sock(id)?;
            if s.bound.is_some() {
                return Err(SockError::Inval); // already bound: EINVAL, not EADDRINUSE
            }
        }
        if self.binds.contains_key(&addr) {
            return Err(SockError::AddrInUse);
        }
        self.binds.insert(addr.clone(), id);
        self.sock_mut(id)?.bound = Some(addr);
        Ok(())
    }

    /// Give an unbound datagram socket an automatic abstract address, so the
    /// datagrams it sends carry a usable return address. Linux does this
    /// inside `sendmsg`; the names it picks are `\0` plus five hex digits.
    fn autobind(&mut self, id: SocketId) -> Result<UnixAddr, SockError> {
        for _ in 0..0x10000 {
            let n = self.autobind_next;
            self.autobind_next = self.autobind_next.wrapping_add(1) & 0xFFFFF;
            let mut name = String::new();
            let mut v = n;
            for _ in 0..5 {
                let digit = (v >> 16) & 0xF;
                name.push(char::from_digit(digit, 16).unwrap_or('0'));
                v <<= 4;
            }
            let addr = UnixAddr::Abstract(Vec::from(name.as_bytes()));
            if !self.binds.contains_key(&addr) {
                self.binds.insert(addr.clone(), id);
                self.sock_mut(id)?.bound = Some(addr.clone());
                return Ok(addr);
            }
        }
        Err(SockError::AddrNotAvail)
    }

    pub fn sockname(&self, id: SocketId) -> Result<UnixAddr, SockError> {
        Ok(self.sock(id)?.sockname())
    }

    pub fn peername(&self, id: SocketId) -> Result<UnixAddr, SockError> {
        let s = self.sock(id)?;
        if s.peer.is_none() && !s.peer_addr.is_named() {
            return Err(SockError::NotConn);
        }
        Ok(s.peername())
    }

    /// Which socket, if any, currently holds `addr`.
    pub fn lookup(&self, addr: &UnixAddr) -> Option<SocketId> {
        self.binds.get(addr).copied()
    }

    // ── connection setup ────────────────────────────────────────────────

    pub fn listen(&mut self, id: SocketId, backlog: i32) -> Result<(), SockError> {
        let s = self.sock_mut(id)?;
        if s.ty != SockType::Stream {
            return Err(SockError::OpNotSupp);
        }
        if s.peer.is_some() {
            return Err(SockError::IsConn);
        }
        if s.bound.is_none() {
            // Linux refuses to listen on an unbound AF_UNIX socket: there
            // would be no address for anyone to connect to.
            return Err(SockError::Inval);
        }
        let backlog = (backlog.max(0) as usize).clamp(1, SOMAXCONN);
        match &mut s.listener {
            Some(l) => l.backlog = backlog,
            None => s.listener = Some(Listener { backlog, pending: Vec::new() }),
        }
        Ok(())
    }

    pub fn connect(&mut self, id: SocketId, addr: &UnixAddr) -> Result<ConnectOutcome, SockError> {
        let (ty, already, listening) = {
            let s = self.sock(id)?;
            (s.ty, s.peer, s.listener.is_some())
        };
        if listening {
            return Err(SockError::Inval);
        }

        let target = self.binds.get(addr).copied().ok_or(SockError::ConnRefused)?;
        if self.sock(target)?.ty != ty {
            return Err(SockError::ProtoType);
        }

        match ty {
            SockType::Dgram => {
                // A datagram connect only records a default destination; it
                // can be redirected at any time, and `AF_UNSPEC` undoes it
                // (see `disconnect`).
                let peer_addr = addr.clone();
                let s = self.sock_mut(id)?;
                s.peer = Some(target);
                s.peer_addr = peer_addr;
                s.peer_gone = false;
                Ok(ConnectOutcome { peer: target, wakes: Wakes::default() })
            }
            SockType::Stream => {
                if already.is_some() {
                    return Err(SockError::IsConn);
                }
                {
                    let t = self.sock(target)?;
                    match &t.listener {
                        None => return Err(SockError::ConnRefused),
                        Some(l) => {
                            if l.pending.len() >= l.backlog {
                                // Linux blocks (or returns EAGAIN when the
                                // socket is non-blocking); the adapter turns
                                // Again into whichever applies.
                                return Err(SockError::Again);
                            }
                        }
                    }
                }

                let server_addr = self.sock(target)?.sockname();
                let client_addr = self.sock(id)?.sockname();

                let child = self.create(SockType::Stream)?;
                {
                    let c = self.sock_mut(child)?;
                    c.peer = Some(id);
                    c.peer_addr = client_addr;
                    // An accepted socket reports the listener's address as
                    // its own, exactly as Linux does.
                    c.bound = Some(server_addr.clone());
                }
                {
                    let s = self.sock_mut(id)?;
                    s.peer = Some(child);
                    s.peer_addr = server_addr;
                    s.peer_gone = false;
                }
                if let Some(l) = self.sock_mut(target)?.listener.as_mut() {
                    l.pending.push(child);
                }

                let mut wakes = Wakes::default();
                wakes.push_acceptable(target);
                wakes.push_readable(target); // a listener polls readable
                Ok(ConnectOutcome { peer: child, wakes })
            }
        }
    }

    /// `connect(fd, AF_UNSPEC)` on a datagram socket: forget the default
    /// destination.
    pub fn disconnect(&mut self, id: SocketId) -> Result<(), SockError> {
        let s = self.sock_mut(id)?;
        if s.ty != SockType::Dgram {
            return Err(SockError::OpNotSupp);
        }
        s.peer = None;
        s.peer_addr = UnixAddr::Unnamed;
        s.peer_gone = false;
        Ok(())
    }

    pub fn accept(&mut self, id: SocketId) -> Result<AcceptOutcome, SockError> {
        let child = {
            let s = self.sock_mut(id)?;
            let l = s.listener.as_mut().ok_or(SockError::Inval)?;
            if l.pending.is_empty() {
                return Err(SockError::Again);
            }
            l.pending.remove(0) // FIFO: oldest connection first
        };
        let peer_addr = self.sock(child)?.peername();
        let mut wakes = Wakes::default();
        wakes.push_writable(id);
        Ok(AcceptOutcome { id: child, peer_addr, wakes })
    }

    /// `socketpair(2)`: two sockets connected to each other, neither named.
    pub fn socketpair(&mut self, ty: SockType) -> Result<(SocketId, SocketId), SockError> {
        let a = self.create(ty)?;
        let b = match self.create(ty) {
            Ok(b) => b,
            Err(e) => {
                self.close(a);
                return Err(e);
            }
        };
        self.sock_mut(a)?.peer = Some(b);
        self.sock_mut(b)?.peer = Some(a);
        Ok((a, b))
    }

    // ── data transfer ───────────────────────────────────────────────────

    /// Send `data` (and any `SCM_RIGHTS` descriptors) on `id`.
    ///
    /// `fds` is taken by `&mut` on purpose: on any outcome that does not
    /// transfer them — an error, or a stream send that got no room — the
    /// descriptors are left in the caller's vector rather than dropped
    /// inside the socket layer. Losing them would leak an open file
    /// description with no fd naming it.
    pub fn send(
        &mut self,
        id: SocketId,
        data: &[u8],
        fds: &mut Vec<F>,
        dest: Option<&UnixAddr>,
    ) -> Result<SendOutcome, SockError> {
        let (ty, wr_shut, peer, peer_gone) = {
            let s = self.sock(id)?;
            (s.ty, s.wr_shut, s.peer, s.peer_gone)
        };
        if wr_shut {
            return Err(SockError::Pipe);
        }

        match ty {
            SockType::Stream => {
                if dest.is_some() {
                    // Linux ignores a destination on a connected stream
                    // socket; refusing it outright is the conservative read
                    // and matches sendto()'s documented EISCONN.
                    return Err(SockError::IsConn);
                }
                let peer = match peer {
                    Some(p) => p,
                    None => {
                        return Err(if peer_gone { SockError::Pipe } else { SockError::NotConn })
                    }
                };
                if self.sock(peer)?.rd_shut {
                    return Err(SockError::Pipe);
                }
                let batch = core::mem::take(fds);
                let (written, returned) = self.sock_mut(peer)?.rx.push_stream(data, batch);
                if written == 0 && !data.is_empty() {
                    *fds = returned;
                    return Err(SockError::Again);
                }
                *fds = returned;
                let mut wakes = Wakes::readable(peer);
                if !fds.is_empty() || written < data.len() {
                    // Caller still has bytes (or fds) to push once space frees.
                    wakes.push_writable(id);
                }
                Ok(SendOutcome { written, wakes })
            }
            SockType::Dgram => {
                let target = match dest {
                    Some(a) => self.binds.get(a).copied().ok_or(SockError::ConnRefused)?,
                    None => match peer {
                        Some(p) => p,
                        None => {
                            return Err(if peer_gone {
                                SockError::ConnRefused
                            } else {
                                SockError::DestAddrReq
                            })
                        }
                    },
                };
                if self.sock(target)?.ty != SockType::Dgram {
                    return Err(SockError::ProtoType);
                }
                if data.len() > self.sock(target)?.rx.capacity() {
                    return Err(SockError::MsgSize);
                }
                if self.sock(target)?.rd_shut {
                    return Err(SockError::Pipe);
                }

                // Give the sender a return address if it has none, so the
                // receiver's recvfrom() can actually reply.
                let from = match &self.sock(id)?.bound {
                    Some(a) => a.clone(),
                    None => self.autobind(id)?,
                };

                let batch = core::mem::take(fds);
                match self.sock_mut(target)?.rx.push_dgram(data, batch, from) {
                    Ok(()) => Ok(SendOutcome { written: data.len(), wakes: Wakes::readable(target) }),
                    Err(returned) => {
                        *fds = returned;
                        Err(SockError::Again)
                    }
                }
            }
        }
    }

    /// Receive into `buf`. Returns `n == 0` for a real end of stream, and
    /// `Err(Again)` when there is simply nothing there yet.
    pub fn recv(
        &mut self,
        id: SocketId,
        buf: &mut [u8],
        peek: bool,
    ) -> Result<RecvOutcome<F>, SockError> {
        let (ty, rd_shut, peer, peer_gone) = {
            let s = self.sock(id)?;
            (s.ty, s.rd_shut, s.peer, s.peer_gone)
        };
        let empty = self.sock(id)?.rx.is_empty();

        // A half-closed read side reports EOF regardless of what is queued.
        if rd_shut {
            return Ok(RecvOutcome {
                n: 0,
                full_len: 0,
                fds: Vec::new(),
                from: None,
                wakes: Wakes::default(),
            });
        }

        match ty {
            SockType::Stream => {
                if empty {
                    let peer_done = match peer {
                        Some(p) => self.sock(p)?.wr_shut,
                        None => true,
                    };
                    if peer.is_none() && !peer_gone {
                        return Err(SockError::NotConn);
                    }
                    if peer_done {
                        // EOF: the writer is gone or has shut its write side.
                        return Ok(RecvOutcome {
                            n: 0,
                            full_len: 0,
                            fds: Vec::new(),
                            from: None,
                            wakes: Wakes::default(),
                        });
                    }
                    if buf.is_empty() {
                        return Ok(RecvOutcome {
                            n: 0,
                            full_len: 0,
                            fds: Vec::new(),
                            from: None,
                            wakes: Wakes::default(),
                        });
                    }
                    return Err(SockError::Again);
                }

                let (n, fds) = self.sock_mut(id)?.rx.read_stream(buf, peek);
                let mut wakes = Wakes::default();
                if n > 0 && !peek {
                    if let Some(p) = peer {
                        wakes.push_writable(p); // space freed for a blocked writer
                    }
                }
                Ok(RecvOutcome { n, full_len: n, fds, from: None, wakes })
            }
            SockType::Dgram => {
                let read = self.sock_mut(id)?.rx.read_dgram(buf, peek);
                match read {
                    Some(d) => {
                        let mut wakes = Wakes::default();
                        if !peek {
                            if let Some(p) = peer {
                                wakes.push_writable(p);
                            }
                        }
                        Ok(RecvOutcome {
                            n: d.n,
                            full_len: d.full_len,
                            fds: d.fds,
                            from: Some(d.from),
                            wakes,
                        })
                    }
                    None => {
                        if peer_gone {
                            return Ok(RecvOutcome {
                                n: 0,
                                full_len: 0,
                                fds: Vec::new(),
                                from: None,
                                wakes: Wakes::default(),
                            });
                        }
                        Err(SockError::Again)
                    }
                }
            }
        }
    }

    // ── teardown of one direction ───────────────────────────────────────

    pub fn shutdown(&mut self, id: SocketId, how: Shutdown) -> Result<Wakes, SockError> {
        let (ty, peer) = {
            let s = self.sock(id)?;
            (s.ty, s.peer)
        };
        if ty == SockType::Stream && peer.is_none() {
            return Err(SockError::NotConn);
        }
        {
            let s = self.sock_mut(id)?;
            if how.shuts_read() {
                s.rd_shut = true;
            }
            if how.shuts_write() {
                s.wr_shut = true;
            }
        }

        let mut wakes = Wakes::default();
        // Our read shutdown makes the peer's writes fail; our write shutdown
        // is the peer's EOF. Either way the peer has to wake up and find out.
        wakes.push_readable(id);
        if let Some(p) = peer {
            wakes.push_readable(p);
            wakes.push_writable(p);
        }
        Ok(wakes)
    }

    // ── readiness ───────────────────────────────────────────────────────

    pub fn poll(&self, id: SocketId) -> Result<PollMask, SockError> {
        let s = self.sock(id)?;
        let mut m = PollMask { err: s.so_error != 0, ..Default::default() };

        if let Some(l) = &s.listener {
            m.readable = !l.pending.is_empty();
            return Ok(m);
        }

        let peer = s.peer.and_then(|p| self.get(p));

        match s.ty {
            SockType::Stream => {
                let eof = s.rd_shut
                    || s.peer_gone
                    || peer.map(|p| p.wr_shut).unwrap_or(false);
                m.readable = !s.rx.is_empty() || eof;
                m.writable = !s.wr_shut
                    && !s.peer_gone
                    && peer.map(|p| !p.rd_shut && p.rx.space() > 0).unwrap_or(false);
                m.hup = s.peer_gone || (s.rd_shut && s.wr_shut);
            }
            SockType::Dgram => {
                m.readable = !s.rx.is_empty() || s.rd_shut;
                // An unconnected datagram socket can always be sent from;
                // a connected one needs room at the other end.
                m.writable = !s.wr_shut
                    && match peer {
                        Some(p) => p.rx.space() > 0,
                        None => !s.peer_gone,
                    };
                m.hup = s.peer_gone;
            }
        }
        Ok(m)
    }

    // ── socket options ──────────────────────────────────────────────────

    pub fn sock_type(&self, id: SocketId) -> Result<SockType, SockError> {
        Ok(self.sock(id)?.ty)
    }

    pub fn take_error(&mut self, id: SocketId) -> Result<i32, SockError> {
        let s = self.sock_mut(id)?;
        Ok(core::mem::replace(&mut s.so_error, 0))
    }

    pub fn sndbuf(&self, id: SocketId) -> Result<usize, SockError> {
        Ok(self.sock(id)?.sndbuf)
    }

    pub fn set_sndbuf(&mut self, id: SocketId, n: usize) -> Result<(), SockError> {
        self.sock_mut(id)?.sndbuf = n;
        Ok(())
    }

    pub fn rcvbuf(&self, id: SocketId) -> Result<usize, SockError> {
        Ok(self.sock(id)?.rx.capacity())
    }

    pub fn set_rcvbuf(&mut self, id: SocketId, n: usize) -> Result<(), SockError> {
        self.sock_mut(id)?.rx.set_capacity(n);
        Ok(())
    }

    /// Bytes readable right now — `ioctl(FIONREAD)`.
    pub fn readable_bytes(&self, id: SocketId) -> Result<usize, SockError> {
        let s = self.sock(id)?;
        Ok(match s.ty {
            SockType::Stream => s.rx.bytes(),
            SockType::Dgram => s.rx.peek_dgram_len().unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    type T = SocketTable<u32>;

    fn addr(p: &str) -> UnixAddr {
        UnixAddr::Path(String::from(p))
    }

    /// A bound, listening stream server.
    fn server(t: &mut T, path: &str) -> SocketId {
        let s = t.create(SockType::Stream).unwrap();
        t.bind(s, addr(path)).unwrap();
        t.listen(s, 5).unwrap();
        s
    }

    /// A connected client/server pair: (client, accepted server socket).
    fn connected(t: &mut T, listener: SocketId, path: &str) -> (SocketId, SocketId) {
        let c = t.create(SockType::Stream).unwrap();
        t.connect(c, &addr(path)).unwrap();
        let a = t.accept(listener).unwrap();
        (c, a.id)
    }

    fn send(t: &mut T, id: SocketId, data: &[u8]) -> Result<usize, SockError> {
        let mut fds = vec![];
        t.send(id, data, &mut fds, None).map(|o| o.written)
    }

    fn recv(t: &mut T, id: SocketId, n: usize) -> Result<(usize, Vec<u8>), SockError> {
        let mut buf = vec![0u8; n];
        let o = t.recv(id, &mut buf, false)?;
        buf.truncate(o.n);
        Ok((o.n, buf))
    }

    // ── naming ──────────────────────────────────────────────────────────

    #[test]
    fn binding_the_same_address_twice_is_eaddrinuse() {
        let mut t = T::new();
        let a = t.create(SockType::Stream).unwrap();
        let b = t.create(SockType::Stream).unwrap();
        t.bind(a, addr("/tmp/s")).unwrap();
        assert_eq!(t.bind(b, addr("/tmp/s")), Err(SockError::AddrInUse));
    }

    #[test]
    fn binding_a_socket_twice_is_einval_not_eaddrinuse() {
        let mut t = T::new();
        let a = t.create(SockType::Stream).unwrap();
        t.bind(a, addr("/tmp/s")).unwrap();
        assert_eq!(t.bind(a, addr("/tmp/other")), Err(SockError::Inval));
    }

    #[test]
    fn abstract_and_path_namespaces_do_not_collide() {
        let mut t = T::new();
        let a = t.create(SockType::Stream).unwrap();
        let b = t.create(SockType::Stream).unwrap();
        t.bind(a, UnixAddr::Path(String::from("x"))).unwrap();
        t.bind(b, UnixAddr::Abstract(Vec::from(&b"x"[..]))).unwrap();
    }

    #[test]
    fn closing_a_socket_frees_its_name() {
        let mut t = T::new();
        let a = t.create(SockType::Stream).unwrap();
        t.bind(a, addr("/tmp/s")).unwrap();
        t.close(a);
        let b = t.create(SockType::Stream).unwrap();
        assert_eq!(t.bind(b, addr("/tmp/s")), Ok(()));
    }

    #[test]
    fn an_unnamed_address_cannot_be_bound() {
        let mut t = T::new();
        let a = t.create(SockType::Stream).unwrap();
        assert_eq!(t.bind(a, UnixAddr::Unnamed), Err(SockError::Inval));
    }

    // ── listen / connect / accept ───────────────────────────────────────

    #[test]
    fn listen_requires_a_bound_stream_socket() {
        let mut t = T::new();
        let unbound = t.create(SockType::Stream).unwrap();
        assert_eq!(t.listen(unbound, 5), Err(SockError::Inval));

        let d = t.create(SockType::Dgram).unwrap();
        t.bind(d, addr("/tmp/d")).unwrap();
        assert_eq!(t.listen(d, 5), Err(SockError::OpNotSupp));
    }

    #[test]
    fn connect_to_nothing_is_econnrefused() {
        let mut t = T::new();
        let c = t.create(SockType::Stream).unwrap();
        assert!(matches!(t.connect(c, &addr("/tmp/nope")), Err(SockError::ConnRefused)));
    }

    #[test]
    fn connect_to_a_bound_but_unlistening_socket_is_econnrefused() {
        let mut t = T::new();
        let s = t.create(SockType::Stream).unwrap();
        t.bind(s, addr("/tmp/s")).unwrap();
        let c = t.create(SockType::Stream).unwrap();
        assert_eq!(t.connect(c, &addr("/tmp/s")).err(), Some(SockError::ConnRefused));
    }

    #[test]
    fn connecting_a_stream_to_a_datagram_is_eprototype() {
        let mut t = T::new();
        let d = t.create(SockType::Dgram).unwrap();
        t.bind(d, addr("/tmp/d")).unwrap();
        let c = t.create(SockType::Stream).unwrap();
        assert_eq!(t.connect(c, &addr("/tmp/d")).err(), Some(SockError::ProtoType));
    }

    #[test]
    fn connect_makes_the_listener_acceptable() {
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        let c = t.create(SockType::Stream).unwrap();
        let out = t.connect(c, &addr("/tmp/s")).unwrap();
        assert_eq!(out.wakes.acceptable, vec![s]);
        assert!(t.poll(s).unwrap().readable);
    }

    #[test]
    fn accept_on_an_empty_backlog_would_block() {
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        assert_eq!(t.accept(s).err(), Some(SockError::Again));
    }

    #[test]
    fn accept_on_a_non_listening_socket_is_einval() {
        let mut t = T::new();
        let c = t.create(SockType::Stream).unwrap();
        assert_eq!(t.accept(c).err(), Some(SockError::Inval));
    }

    #[test]
    fn a_full_backlog_refuses_further_connects() {
        let mut t = T::new();
        let s = t.create(SockType::Stream).unwrap();
        t.bind(s, addr("/tmp/s")).unwrap();
        t.listen(s, 1).unwrap();

        let c1 = t.create(SockType::Stream).unwrap();
        t.connect(c1, &addr("/tmp/s")).unwrap();
        let c2 = t.create(SockType::Stream).unwrap();
        assert_eq!(t.connect(c2, &addr("/tmp/s")).err(), Some(SockError::Again));

        // Accepting drains one slot and lets the next connect through.
        t.accept(s).unwrap();
        assert!(t.connect(c2, &addr("/tmp/s")).is_ok());
    }

    #[test]
    fn connecting_an_already_connected_stream_is_eisconn() {
        let mut t = T::new();
        server(&mut t, "/tmp/s");
        let c = t.create(SockType::Stream).unwrap();
        t.connect(c, &addr("/tmp/s")).unwrap();
        assert_eq!(t.connect(c, &addr("/tmp/s")).err(), Some(SockError::IsConn));
    }

    #[test]
    fn connections_are_accepted_oldest_first() {
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        let c1 = t.create(SockType::Stream).unwrap();
        let c2 = t.create(SockType::Stream).unwrap();
        let p1 = t.connect(c1, &addr("/tmp/s")).unwrap().peer;
        let p2 = t.connect(c2, &addr("/tmp/s")).unwrap().peer;
        assert_eq!(t.accept(s).unwrap().id, p1);
        assert_eq!(t.accept(s).unwrap().id, p2);
    }

    #[test]
    fn a_client_may_write_before_the_server_ever_accepts() {
        // The point of creating the server-side socket in connect(): data
        // sent before accept() is already waiting when it happens.
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        let c = t.create(SockType::Stream).unwrap();
        t.connect(c, &addr("/tmp/s")).unwrap();
        assert_eq!(send(&mut t, c, b"early").unwrap(), 5);

        let a = t.accept(s).unwrap();
        assert_eq!(recv(&mut t, a.id, 16).unwrap().1, b"early");
    }

    #[test]
    fn an_accepted_socket_reports_the_listeners_address_as_its_own() {
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        let (c, a) = connected(&mut t, s, "/tmp/s");
        assert_eq!(t.sockname(a).unwrap(), addr("/tmp/s"));
        assert_eq!(t.peername(c).unwrap(), addr("/tmp/s"));
        // The client never bound, so the server sees an unnamed peer.
        assert_eq!(t.sockname(c).unwrap(), UnixAddr::Unnamed);
    }

    #[test]
    fn peername_on_an_unconnected_socket_is_enotconn() {
        let mut t = T::new();
        let c = t.create(SockType::Stream).unwrap();
        assert_eq!(t.peername(c).err(), Some(SockError::NotConn));
    }

    // ── stream data ─────────────────────────────────────────────────────

    #[test]
    fn a_stream_carries_bytes_both_ways() {
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        let (c, a) = connected(&mut t, s, "/tmp/s");

        send(&mut t, c, b"ping").unwrap();
        assert_eq!(recv(&mut t, a, 16).unwrap().1, b"ping");
        send(&mut t, a, b"pong").unwrap();
        assert_eq!(recv(&mut t, c, 16).unwrap().1, b"pong");
    }

    #[test]
    fn a_send_reports_the_receiver_as_newly_readable() {
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        let (c, a) = connected(&mut t, s, "/tmp/s");
        let mut fds = vec![];
        let out = t.send(c, b"x", &mut fds, None).unwrap();
        assert_eq!(out.wakes.readable, vec![a]);
    }

    #[test]
    fn an_empty_stream_would_block_but_a_dead_peer_is_eof() {
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        let (c, a) = connected(&mut t, s, "/tmp/s");

        assert_eq!(recv(&mut t, a, 4).err(), Some(SockError::Again));
        t.close(c);
        assert_eq!(recv(&mut t, a, 4).unwrap().0, 0, "EOF, not EAGAIN");
    }

    #[test]
    fn data_already_queued_survives_the_senders_close() {
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        let (c, a) = connected(&mut t, s, "/tmp/s");
        send(&mut t, c, b"last words").unwrap();
        t.close(c);
        assert_eq!(recv(&mut t, a, 32).unwrap().1, b"last words");
        assert_eq!(recv(&mut t, a, 32).unwrap().0, 0, "then EOF");
    }

    #[test]
    fn sending_to_a_closed_peer_is_epipe() {
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        let (c, a) = connected(&mut t, s, "/tmp/s");
        t.close(a);
        assert_eq!(send(&mut t, c, b"x").err(), Some(SockError::Pipe));
    }

    #[test]
    fn sending_on_an_unconnected_stream_is_enotconn() {
        let mut t = T::new();
        let c = t.create(SockType::Stream).unwrap();
        assert_eq!(send(&mut t, c, b"x").err(), Some(SockError::NotConn));
        assert_eq!(recv(&mut t, c, 4).err(), Some(SockError::NotConn));
    }

    #[test]
    fn a_full_receive_queue_backs_the_sender_off() {
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        let (c, a) = connected(&mut t, s, "/tmp/s");
        t.set_rcvbuf(a, 4).unwrap();

        assert_eq!(send(&mut t, c, b"abcdef").unwrap(), 4, "short write, not an error");
        assert_eq!(send(&mut t, c, b"gh").err(), Some(SockError::Again));

        // Draining the reader wakes the writer.
        let mut buf = [0u8; 2];
        let out = t.recv(a, &mut buf, false).unwrap();
        assert_eq!(out.wakes.writable, vec![c]);
        assert_eq!(send(&mut t, c, b"gh").unwrap(), 2);
    }

    #[test]
    fn closing_one_of_two_references_keeps_the_socket_alive() {
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        let (c, a) = connected(&mut t, s, "/tmp/s");
        t.retain(c).unwrap(); // a dup(), or a fork()

        let out = t.close(c);
        assert!(!out.released);
        assert_eq!(send(&mut t, a, b"still here").unwrap(), 10);
        assert_eq!(recv(&mut t, c, 16).unwrap().1, b"still here");

        assert!(t.close(c).released);
    }

    // ── half close ──────────────────────────────────────────────────────

    #[test]
    fn shutdown_write_is_the_peers_eof_but_keeps_the_reverse_direction() {
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        let (c, a) = connected(&mut t, s, "/tmp/s");

        t.shutdown(c, Shutdown::Write).unwrap();
        assert_eq!(recv(&mut t, a, 8).unwrap().0, 0, "reader sees EOF");
        assert_eq!(send(&mut t, c, b"x").err(), Some(SockError::Pipe));

        // The other direction still works.
        send(&mut t, a, b"reply").unwrap();
        assert_eq!(recv(&mut t, c, 8).unwrap().1, b"reply");
    }

    #[test]
    fn shutdown_read_makes_the_peers_writes_fail() {
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        let (c, a) = connected(&mut t, s, "/tmp/s");
        t.shutdown(c, Shutdown::Read).unwrap();
        assert_eq!(send(&mut t, a, b"x").err(), Some(SockError::Pipe));
        assert_eq!(recv(&mut t, c, 8).unwrap().0, 0);
    }

    #[test]
    fn shutdown_on_an_unconnected_stream_is_enotconn() {
        let mut t = T::new();
        let c = t.create(SockType::Stream).unwrap();
        assert_eq!(t.shutdown(c, Shutdown::Both).err(), Some(SockError::NotConn));
    }

    // ── socketpair ──────────────────────────────────────────────────────

    #[test]
    fn socketpair_is_connected_and_unnamed() {
        let mut t = T::new();
        let (a, b) = t.socketpair(SockType::Stream).unwrap();
        assert_eq!(t.sockname(a).unwrap(), UnixAddr::Unnamed);
        send(&mut t, a, b"hi").unwrap();
        assert_eq!(recv(&mut t, b, 8).unwrap().1, b"hi");
        send(&mut t, b, b"yo").unwrap();
        assert_eq!(recv(&mut t, a, 8).unwrap().1, b"yo");
    }

    #[test]
    fn a_datagram_socketpair_keeps_message_boundaries() {
        let mut t = T::new();
        let (a, b) = t.socketpair(SockType::Dgram).unwrap();
        send(&mut t, a, b"one").unwrap();
        send(&mut t, a, b"two").unwrap();
        assert_eq!(recv(&mut t, b, 16).unwrap().1, b"one");
        assert_eq!(recv(&mut t, b, 16).unwrap().1, b"two");
    }

    // ── datagrams ───────────────────────────────────────────────────────

    #[test]
    fn a_datagram_send_needs_a_destination() {
        let mut t = T::new();
        let c = t.create(SockType::Dgram).unwrap();
        assert_eq!(send(&mut t, c, b"x").err(), Some(SockError::DestAddrReq));
    }

    #[test]
    fn sendto_reaches_a_bound_datagram_socket_and_carries_a_reply_address() {
        let mut t = T::new();
        let srv = t.create(SockType::Dgram).unwrap();
        t.bind(srv, addr("/tmp/d")).unwrap();
        let cli = t.create(SockType::Dgram).unwrap();

        let mut fds = vec![];
        t.send(cli, b"hello", &mut fds, Some(&addr("/tmp/d"))).unwrap();

        let mut buf = [0u8; 16];
        let out = t.recv(srv, &mut buf, false).unwrap();
        assert_eq!(&buf[..out.n], b"hello");

        // The client was autobound, so the server can answer it.
        let from = out.from.unwrap();
        assert!(matches!(from, UnixAddr::Abstract(_)), "autobind gives an abstract name");
        assert_eq!(t.sockname(cli).unwrap(), from);

        let mut fds = vec![];
        t.send(srv, b"back", &mut fds, Some(&from)).unwrap();
        assert_eq!(recv(&mut t, cli, 16).unwrap().1, b"back");
    }

    #[test]
    fn a_connected_datagram_socket_needs_no_destination() {
        let mut t = T::new();
        let srv = t.create(SockType::Dgram).unwrap();
        t.bind(srv, addr("/tmp/d")).unwrap();
        let cli = t.create(SockType::Dgram).unwrap();
        t.connect(cli, &addr("/tmp/d")).unwrap();
        send(&mut t, cli, b"x").unwrap();
        assert_eq!(recv(&mut t, srv, 8).unwrap().1, b"x");

        t.disconnect(cli).unwrap();
        assert_eq!(send(&mut t, cli, b"y").err(), Some(SockError::DestAddrReq));
    }

    #[test]
    fn a_datagram_larger_than_the_receive_buffer_is_emsgsize() {
        let mut t = T::new();
        let srv = t.create(SockType::Dgram).unwrap();
        t.bind(srv, addr("/tmp/d")).unwrap();
        t.set_rcvbuf(srv, 4).unwrap();
        let cli = t.create(SockType::Dgram).unwrap();
        let mut fds = vec![];
        assert_eq!(
            t.send(cli, b"toolong", &mut fds, Some(&addr("/tmp/d"))).err(),
            Some(SockError::MsgSize),
            "a message that can never fit is a size error, not a wait"
        );
    }

    #[test]
    fn an_unreadable_datagram_queue_would_block() {
        let mut t = T::new();
        let d = t.create(SockType::Dgram).unwrap();
        assert_eq!(recv(&mut t, d, 8).err(), Some(SockError::Again));
    }

    // ── SCM_RIGHTS ──────────────────────────────────────────────────────

    #[test]
    fn descriptors_travel_with_the_bytes_they_were_sent_with() {
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        let (c, a) = connected(&mut t, s, "/tmp/s");

        let mut fds = vec![7, 8];
        t.send(c, b"hdr", &mut fds, None).unwrap();
        assert!(fds.is_empty(), "the socket took ownership of them");

        let mut buf = [0u8; 8];
        let out = t.recv(a, &mut buf, false).unwrap();
        assert_eq!((&buf[..out.n], out.fds), (&b"hdr"[..], vec![7, 8]));
    }

    #[test]
    fn a_refused_send_gives_the_descriptors_back() {
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        let (c, a) = connected(&mut t, s, "/tmp/s");
        t.set_rcvbuf(a, 2).unwrap();
        send(&mut t, c, b"ab").unwrap();

        let mut fds = vec![9];
        assert_eq!(t.send(c, b"c", &mut fds, None).err(), Some(SockError::Again));
        assert_eq!(fds, vec![9], "never swallow descriptors that did not go out");
    }

    #[test]
    fn descriptors_still_in_flight_come_back_when_the_socket_dies() {
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        let (c, a) = connected(&mut t, s, "/tmp/s");
        let mut fds = vec![4, 5];
        t.send(c, b"x", &mut fds, None).unwrap();

        let out = t.close(a);
        assert_eq!(out.fds, vec![4, 5], "the caller has to close these");
    }

    #[test]
    fn descriptors_can_ride_a_datagram_too() {
        let mut t = T::new();
        let (a, b) = t.socketpair(SockType::Dgram).unwrap();
        let mut fds = vec![3];
        t.send(a, b"msg", &mut fds, None).unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(t.recv(b, &mut buf, false).unwrap().fds, vec![3]);
    }

    // ── readiness ───────────────────────────────────────────────────────

    #[test]
    fn poll_tracks_a_streams_whole_life() {
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        let (c, a) = connected(&mut t, s, "/tmp/s");

        let m = t.poll(c).unwrap();
        assert_eq!((m.readable, m.writable, m.hup), (false, true, false));

        send(&mut t, c, b"x").unwrap();
        assert!(t.poll(a).unwrap().readable);

        t.close(c);
        let m = t.poll(a).unwrap();
        assert!(m.readable, "a pending EOF counts as readable");
        assert!(m.hup);
        assert!(!m.writable);
    }

    #[test]
    fn a_full_peer_queue_makes_a_socket_unwritable() {
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        let (c, a) = connected(&mut t, s, "/tmp/s");
        t.set_rcvbuf(a, 2).unwrap();
        send(&mut t, c, b"ab").unwrap();
        assert!(!t.poll(c).unwrap().writable);
    }

    #[test]
    fn readable_bytes_answers_per_socket_type() {
        let mut t = T::new();
        let (a, b) = t.socketpair(SockType::Stream).unwrap();
        send(&mut t, a, b"abc").unwrap();
        send(&mut t, a, b"de").unwrap();
        assert_eq!(t.readable_bytes(b).unwrap(), 5, "stream: every queued byte");

        let (c, d) = t.socketpair(SockType::Dgram).unwrap();
        send(&mut t, c, b"abc").unwrap();
        send(&mut t, c, b"de").unwrap();
        assert_eq!(t.readable_bytes(d).unwrap(), 3, "datagram: just the next message");
    }

    // ── teardown ────────────────────────────────────────────────────────

    #[test]
    fn closing_a_listener_takes_its_unaccepted_connections_with_it() {
        let mut t = T::new();
        let s = server(&mut t, "/tmp/s");
        let c = t.create(SockType::Stream).unwrap();
        let child = t.connect(c, &addr("/tmp/s")).unwrap().peer;

        let before = t.live_count();
        t.close(s);
        assert!(t.get(child).is_none(), "the queued connection is gone too");
        assert_eq!(t.live_count(), before - 2);
        // ...and the client finds out.
        assert_eq!(send(&mut t, c, b"x").err(), Some(SockError::Pipe));
    }

    #[test]
    fn a_recycled_id_is_never_mistaken_for_the_socket_that_held_it() {
        // close() sweeps every peer pointer, so a new socket landing in the
        // freed slot cannot inherit the old one's traffic.
        let mut t = T::new();
        let srv = t.create(SockType::Dgram).unwrap();
        t.bind(srv, addr("/tmp/d")).unwrap();
        let cli = t.create(SockType::Dgram).unwrap();
        t.connect(cli, &addr("/tmp/d")).unwrap();

        t.close(srv);
        let reused = t.create(SockType::Dgram).unwrap();
        assert_eq!(reused, srv, "the id really is recycled");
        assert_eq!(
            send(&mut t, cli, b"x").err(),
            Some(SockError::ConnRefused),
            "the old connection must not silently reattach"
        );
    }

    #[test]
    fn closing_an_unknown_id_is_harmless() {
        let mut t = T::new();
        let out = t.close(999);
        assert!(!out.released && out.fds.is_empty() && out.wakes.is_empty());
    }
}
