//! The socket-facing half of the stack: one smoltcp `Interface` and its
//! sockets, behind an API that never blocks. Operations that cannot complete
//! return [`NetError::Again`]; `poll` returns the sockets whose readiness
//! changed so the kernel adapter can wake their waiters. No globals here.

use alloc::vec::Vec;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::Device;
use smoltcp::socket::{dhcpv4, tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint, Ipv4Address};

pub type Handle = SocketHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    /// Would block (`EAGAIN`).
    Again,
    InvalidArg,
    AddrInUse,
    /// No default destination for a send on an unconnected socket.
    DestAddrRequired,
    NetUnreachable,
    MsgSize,
    BadHandle,
    NotConnected,
    ConnRefused,
    ConnReset,
    /// A non-blocking `connect` is under way (`EINPROGRESS`).
    InProgress,
    AlreadyConnected,
    /// Write to a connection that is no longer writable (`EPIPE`).
    BrokenPipe,
    /// The operation does not apply to this socket's state (e.g. `accept` on a
    /// socket that is not listening): `EINVAL`.
    NotListening,
}

impl NetError {
    /// Linux errno (positive).
    pub fn errno(self) -> i32 {
        match self {
            NetError::Again => 11,
            NetError::InvalidArg => 22,
            NetError::AddrInUse => 98,
            NetError::DestAddrRequired => 89,
            NetError::NetUnreachable => 101,
            NetError::MsgSize => 90,
            NetError::BadHandle => 9,
            NetError::NotConnected => 107,
            NetError::ConnRefused => 111,
            NetError::ConnReset => 104,
            NetError::InProgress => 115,
            NetError::AlreadyConnected => 106,
            NetError::BrokenPipe => 32,
            NetError::NotListening => 22,
        }
    }
}

/// What DHCP (or `set_static`) configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lease {
    pub addr: Ipv4Address,
    pub prefix: u8,
    pub router: Option<Ipv4Address>,
    pub dns: Option<Ipv4Address>,
}

/// Identifies a TCP socket. Our own number, not smoltcp's `SocketHandle`:
/// a listener is a group of smoltcp sockets that outlives any one of them,
/// and smoltcp reuses handles of removed sockets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TcpId(u32);

/// A socket whose readiness changed (see `Stack::poll`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Udp(Handle),
    Tcp(TcpId),
}

/// Result of `tcp_connect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connect {
    /// The handshake is in flight; ask again after the socket's next event.
    Pending,
    /// Established (reported once; asking again is `AlreadyConnected`).
    Done,
}

const EPHEMERAL_FIRST: u16 = 49152;
const TCP_RX_BYTES: usize = 64 * 1024;
const TCP_TX_BYTES: usize = 64 * 1024;
/// A closed socket still waiting for the peer's ACK/FIN gives up after this.
const TCP_ORPHAN_TIMEOUT_MS: u64 = 30_000;
const MAX_BACKLOG: usize = 16;
const UDP_QUEUE_PACKETS: usize = 16;
const UDP_QUEUE_BYTES: usize = 32 * 1024;

struct UdpInfo {
    handle: Handle,
    peer: Option<IpEndpoint>,
    /// Readiness last reported by `poll`, to detect edges.
    was_readable: bool,
    was_writable: bool,
}

struct Listener {
    /// smoltcp sockets in LISTEN (or already handshaking) on the port.
    members: Vec<SocketHandle>,
}

struct TcpInfo {
    id: TcpId,
    /// The smoltcp socket; `None` for a listener (its sockets are `members`).
    sock: Option<SocketHandle>,
    local_addr: Option<Ipv4Address>,
    local_port: u16,
    listener: Option<Listener>,
    /// `connect` started and its outcome is not yet reported.
    connecting: bool,
    /// The handshake failed; reported by the next `connect`/`take_error`.
    connect_err: Option<NetError>,
    /// The connection reached ESTABLISHED at some point.
    was_connected: bool,
    last: TcpReady,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct TcpReady {
    readable: bool,
    writable: bool,
    hup: bool,
    state: tcp::State,
}

pub struct Stack<D: Device> {
    dev: D,
    iface: Interface,
    sockets: SocketSet<'static>,
    /// The DHCP client, while one is running (`enable_dhcp`).
    dhcp: Option<Handle>,
    lease: Option<Lease>,
    udp: Vec<UdpInfo>,
    tcp: Vec<TcpInfo>,
    /// Closed by the application, still finishing the connection.
    orphans: Vec<SocketHandle>,
    next_tcp: u32,
    next_port: u16,
}

fn new_udp_socket() -> udp::Socket<'static> {
    let rx = udp::PacketBuffer::new(
        alloc::vec![udp::PacketMetadata::EMPTY; UDP_QUEUE_PACKETS],
        alloc::vec![0; UDP_QUEUE_BYTES],
    );
    let tx = udp::PacketBuffer::new(
        alloc::vec![udp::PacketMetadata::EMPTY; UDP_QUEUE_PACKETS],
        alloc::vec![0; UDP_QUEUE_BYTES],
    );
    udp::Socket::new(rx, tx)
}

impl<D: Device> Stack<D> {
    /// `seed` feeds smoltcp's port/sequence randomisation.
    pub fn new(mut dev: D, mac: [u8; 6], seed: u64, now: Instant) -> Self {
        let mut cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(mac)));
        cfg.random_seed = seed;
        let iface = Interface::new(cfg, &mut dev, now);
        let sockets = SocketSet::new(Vec::new());
        Stack { dev, iface, sockets, dhcp: None, lease: None, udp: Vec::new(), tcp: Vec::new(), orphans: Vec::new(), next_tcp: 1, next_port: EPHEMERAL_FIRST }
    }

    pub fn device(&mut self) -> &mut D {
        &mut self.dev
    }

    pub fn lease(&self) -> Option<Lease> {
        self.lease
    }

    /// Starts the DHCP client; its lease configures the interface. The
    /// interface has no address until one arrives.
    pub fn enable_dhcp(&mut self) {
        if self.dhcp.is_none() {
            self.dhcp = Some(self.sockets.add(dhcpv4::Socket::new()));
        }
    }

    /// Uses this address now and stops DHCP (tests, or a failed lease).
    pub fn set_static(&mut self, addr: Ipv4Address, prefix: u8, router: Option<Ipv4Address>) {
        if let Some(h) = self.dhcp.take() {
            self.sockets.remove(h);
        }
        self.apply(Some(Lease { addr, prefix, router, dns: None }));
    }

    fn apply(&mut self, lease: Option<Lease>) {
        self.iface.update_ip_addrs(|a| {
            a.clear();
            if let Some(l) = lease {
                let _ = a.push(IpCidr::new(IpAddress::Ipv4(l.addr), l.prefix));
            }
        });
        self.iface.routes_mut().remove_default_ipv4_route();
        if let Some(Lease { router: Some(r), .. }) = lease {
            let _ = self.iface.routes_mut().add_default_ipv4_route(r);
        }
        self.lease = lease;
    }

    /// Drives the interface once and returns the sockets whose readiness (or
    /// TCP state) changed since the last call.
    pub fn poll(&mut self, now: Instant) -> Vec<Event> {
        self.iface.poll(now, &mut self.dev, &mut self.sockets);

        let event = match self.dhcp {
            Some(h) => self.sockets.get_mut::<dhcpv4::Socket>(h).poll(),
            None => None,
        };
        match event {
            Some(dhcpv4::Event::Configured(c)) => {
                let lease = Lease {
                    addr: c.address.address(),
                    prefix: c.address.prefix_len(),
                    router: c.router,
                    dns: c.dns_servers.first().copied(),
                };
                self.apply(Some(lease));
                // Config changed: let the new address go out right away.
                self.iface.poll(now, &mut self.dev, &mut self.sockets);
            }
            Some(dhcpv4::Event::Deconfigured) => self.apply(None),
            None => {}
        }

        let mut woken: Vec<Event> = Vec::new();
        for info in self.udp.iter_mut() {
            let s = self.sockets.get::<udp::Socket>(info.handle);
            let (r, w) = (s.can_recv(), s.can_send());
            if (r && !info.was_readable) || (w && !info.was_writable) {
                woken.push(Event::Udp(info.handle));
            }
            info.was_readable = r;
            info.was_writable = w;
        }
        self.poll_tcp(&mut woken);
        woken
    }

    // ── UDP ───────────────────────────────────────────────────────────────

    pub fn udp_open(&mut self) -> Handle {
        let handle = self.sockets.add(new_udp_socket());
        // A fresh socket is writable but nothing was ever reported: start
        // "writable" so the first transition to full/empty is an edge.
        self.udp.push(UdpInfo { handle, peer: None, was_readable: false, was_writable: true });
        handle
    }

    fn info(&mut self, h: Handle) -> Result<&mut UdpInfo, NetError> {
        self.udp.iter_mut().find(|i| i.handle == h).ok_or(NetError::BadHandle)
    }

    fn port_in_use(&self, port: u16, except: Handle) -> bool {
        self.udp.iter().any(|i| {
            i.handle != except && self.sockets.get::<udp::Socket>(i.handle).endpoint().port == port
        })
    }

    fn ephemeral(&mut self, h: Handle) -> Result<u16, NetError> {
        let span = u16::MAX - EPHEMERAL_FIRST + 1;
        for _ in 0..span {
            let p = self.next_port;
            self.next_port = if p == u16::MAX { EPHEMERAL_FIRST } else { p + 1 };
            if !self.port_in_use(p, h) {
                return Ok(p);
            }
        }
        Err(NetError::AddrInUse)
    }

    /// `port` 0 picks an ephemeral one. `addr` of `None` is the wildcard.
    pub fn udp_bind(&mut self, h: Handle, addr: Option<Ipv4Address>, port: u16) -> Result<u16, NetError> {
        self.info(h)?;
        if self.sockets.get::<udp::Socket>(h).is_open() {
            return Err(NetError::InvalidArg); // already bound
        }
        let port = if port == 0 { self.ephemeral(h)? } else { port };
        if self.port_in_use(port, h) {
            return Err(NetError::AddrInUse);
        }
        let ep = IpListenEndpoint { addr: addr.map(IpAddress::Ipv4), port };
        self.sockets.get_mut::<udp::Socket>(h).bind(ep).map_err(|_| NetError::InvalidArg)?;
        Ok(port)
    }

    /// Sets the default destination (and the only accepted source).
    pub fn udp_connect(&mut self, h: Handle, peer: IpEndpoint) -> Result<(), NetError> {
        self.info(h)?;
        if !self.sockets.get::<udp::Socket>(h).is_open() {
            let p = self.ephemeral(h)?;
            self.udp_bind(h, None, p)?;
        }
        self.info(h)?.peer = Some(peer);
        Ok(())
    }

    pub fn udp_peer(&mut self, h: Handle) -> Result<Option<IpEndpoint>, NetError> {
        Ok(self.info(h)?.peer)
    }

    /// The bound endpoint; the address is the interface's when bound to the wildcard.
    pub fn udp_local(&mut self, h: Handle) -> Result<(Ipv4Address, u16), NetError> {
        self.info(h)?;
        let ep = self.sockets.get::<udp::Socket>(h).endpoint();
        let addr = match ep.addr {
            Some(IpAddress::Ipv4(a)) => a,
            _ => Ipv4Address::UNSPECIFIED,
        };
        Ok((addr, ep.port))
    }

    pub fn udp_send(&mut self, h: Handle, data: &[u8], dest: Option<IpEndpoint>) -> Result<usize, NetError> {
        let peer = self.info(h)?.peer;
        let dest = dest.or(peer).ok_or(NetError::DestAddrRequired)?;
        if !self.sockets.get::<udp::Socket>(h).is_open() {
            let p = self.ephemeral(h)?;
            self.udp_bind(h, None, p)?;
        }
        match self.sockets.get_mut::<udp::Socket>(h).send_slice(data, dest) {
            Ok(()) => Ok(data.len()),
            Err(udp::SendError::BufferFull) => Err(NetError::Again),
            Err(udp::SendError::Unaddressable) => Err(NetError::NetUnreachable),
        }
    }

    /// Next datagram into `buf`: `(bytes copied, datagram length, sender)`.
    /// A short `buf` truncates, as `recvfrom` does. With `peek` the
    /// datagram stays queued.
    pub fn udp_recv(&mut self, h: Handle, buf: &mut [u8], peek: bool) -> Result<(usize, usize, IpEndpoint), NetError> {
        let peer = self.info(h)?.peer;
        let s = self.sockets.get_mut::<udp::Socket>(h);
        loop {
            // A connected socket only accepts its peer's datagrams.
            let accept = |from: IpEndpoint| peer.map_or(true, |p| p == from);
            if peek {
                return match s.peek() {
                    Ok((data, meta)) if accept(meta.endpoint) => {
                        let n = data.len().min(buf.len());
                        buf[..n].copy_from_slice(&data[..n]);
                        Ok((n, data.len(), meta.endpoint))
                    }
                    Ok(_) => {
                        let _ = s.recv(); // drop the stranger and look again
                        continue;
                    }
                    Err(_) => Err(NetError::Again),
                };
            }
            return match s.recv() {
                Ok((data, meta)) if accept(meta.endpoint) => {
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    Ok((n, data.len(), meta.endpoint))
                }
                Ok(_) => continue,
                Err(_) => Err(NetError::Again),
            };
        }
    }

    /// `(readable, writable)`.
    pub fn udp_mask(&mut self, h: Handle) -> Result<(bool, bool), NetError> {
        self.info(h)?;
        let s = self.sockets.get::<udp::Socket>(h);
        Ok((s.can_recv(), s.can_send()))
    }

    pub fn udp_close(&mut self, h: Handle) {
        self.udp.retain(|i| i.handle != h);
        self.sockets.remove(h);
    }

    /// Next instant smoltcp wants to be polled (timers, DHCP retries).
    pub fn poll_delay(&mut self, now: Instant) -> Option<smoltcp::time::Duration> {
        self.iface.poll_delay(now, &self.sockets)
    }
}


// ── TCP ─────────────────────────────────────────────────────────────────────

fn new_tcp_socket() -> tcp::Socket<'static> {
    tcp::Socket::new(
        tcp::SocketBuffer::new(alloc::vec![0; TCP_RX_BYTES]),
        tcp::SocketBuffer::new(alloc::vec![0; TCP_TX_BYTES]),
    )
}

impl<D: Device> Stack<D> {
    fn tcp_info(&mut self, id: TcpId) -> Result<&mut TcpInfo, NetError> {
        self.tcp.iter_mut().find(|i| i.id == id).ok_or(NetError::BadHandle)
    }

    fn tcp_idx(&self, id: TcpId) -> Result<usize, NetError> {
        self.tcp.iter().position(|i| i.id == id).ok_or(NetError::BadHandle)
    }

    fn tcp_sock(&self, id: TcpId) -> Result<&tcp::Socket<'static>, NetError> {
        let i = &self.tcp[self.tcp_idx(id)?];
        i.sock.map(|h| self.sockets.get::<tcp::Socket>(h)).ok_or(NetError::NotConnected)
    }

    fn tcp_sock_mut(&mut self, id: TcpId) -> Result<&mut tcp::Socket<'static>, NetError> {
        let h = self.tcp_info(id)?.sock.ok_or(NetError::NotConnected)?;
        Ok(self.sockets.get_mut::<tcp::Socket>(h))
    }

    pub fn tcp_open(&mut self) -> TcpId {
        let id = TcpId(self.next_tcp);
        self.next_tcp += 1;
        let sock = self.sockets.add(new_tcp_socket());
        let last = TcpReady { readable: false, writable: false, hup: false, state: tcp::State::Closed };
        self.tcp.push(TcpInfo {
            id,
            sock: Some(sock),
            local_addr: None,
            local_port: 0,
            listener: None,
            connecting: false,
            connect_err: None,
            was_connected: false,
            last,
        });
        id
    }

    /// Whether `port` is claimed by another TCP socket that is listening or
    /// bound-but-idle. Connected sockets share a listener's port and do not
    /// count.
    fn tcp_port_claimed(&self, port: u16, except: TcpId) -> bool {
        self.tcp.iter().any(|i| {
            if i.id == except || i.local_port != port {
                return false;
            }
            let idle = i.sock.is_some_and(|h| self.sockets.get::<tcp::Socket>(h).state() == tcp::State::Closed);
            i.listener.is_some() || (idle && !i.connecting)
        })
    }

    fn tcp_ephemeral(&mut self, id: TcpId) -> Result<u16, NetError> {
        let span = u16::MAX - EPHEMERAL_FIRST + 1;
        for _ in 0..span {
            let p = self.next_port;
            self.next_port = if p == u16::MAX { EPHEMERAL_FIRST } else { p + 1 };
            if !self.tcp_port_claimed(p, id) {
                return Ok(p);
            }
        }
        Err(NetError::AddrInUse)
    }

    pub fn tcp_bind(&mut self, id: TcpId, addr: Option<Ipv4Address>, port: u16) -> Result<u16, NetError> {
        let info = self.tcp_info(id)?;
        if info.local_port != 0 || info.listener.is_some() {
            return Err(NetError::InvalidArg);
        }
        let port = if port == 0 { self.tcp_ephemeral(id)? } else { port };
        if self.tcp_port_claimed(port, id) {
            return Err(NetError::AddrInUse);
        }
        let info = self.tcp_info(id)?;
        info.local_addr = addr;
        info.local_port = port;
        Ok(port)
    }

    fn listen_member(&mut self, addr: Option<Ipv4Address>, port: u16) -> Result<SocketHandle, NetError> {
        let mut s = new_tcp_socket();
        let ep = IpListenEndpoint { addr: addr.map(IpAddress::Ipv4), port };
        s.listen(ep).map_err(|_| NetError::InvalidArg)?;
        Ok(self.sockets.add(s))
    }

    /// Turns the socket into a listener with room for `backlog` handshakes.
    pub fn tcp_listen(&mut self, id: TcpId, backlog: usize) -> Result<(), NetError> {
        let info = self.tcp_info(id)?;
        if info.listener.is_some() {
            return Ok(()); // listen() again only changes the backlog on Linux
        }
        if info.connecting || info.was_connected {
            return Err(NetError::InvalidArg);
        }
        let (addr, port) = (info.local_addr, info.local_port);
        let port = if port == 0 { self.tcp_bind(id, addr, 0)? } else { port };
        let backlog = backlog.clamp(1, MAX_BACKLOG);
        let mut members = Vec::new();
        for _ in 0..backlog {
            members.push(self.listen_member(addr, port)?);
        }
        let info = self.tcp_info(id)?;
        let old = info.sock.take();
        info.listener = Some(Listener { members });
        if let Some(h) = old {
            self.sockets.remove(h);
        }
        Ok(())
    }

    /// Takes one established connection off the listener's queue.
    pub fn tcp_accept(&mut self, id: TcpId) -> Result<(TcpId, IpEndpoint), NetError> {
        let idx = self.tcp_idx(id)?;
        let Some(l) = self.tcp[idx].listener.as_ref() else {
            return Err(NetError::NotListening);
        };
        let ready = l.members.iter().position(|&h| {
            !matches!(self.sockets.get::<tcp::Socket>(h).state(), tcp::State::Listen | tcp::State::SynReceived)
        });
        let Some(pos) = ready else { return Err(NetError::Again) };
        let (addr, port) = (self.tcp[idx].local_addr, self.tcp[idx].local_port);
        let replacement = self.listen_member(addr, port)?;
        let l = self.tcp[idx].listener.as_mut().unwrap();
        let h = l.members.swap_remove(pos);
        l.members.push(replacement);
        let remote = self.sockets.get::<tcp::Socket>(h).remote_endpoint().ok_or(NetError::ConnReset)?;

        let nid = TcpId(self.next_tcp);
        self.next_tcp += 1;
        let last = TcpReady { readable: false, writable: false, hup: false, state: tcp::State::Closed };
        self.tcp.push(TcpInfo {
            id: nid,
            sock: Some(h),
            local_addr: addr,
            local_port: port,
            listener: None,
            connecting: false,
            connect_err: None,
            was_connected: true,
            last,
        });
        Ok((nid, remote))
    }

    /// Starts (or continues) an active open. Call again after each wakeup
    /// until it answers `Done` or an error.
    pub fn tcp_connect(&mut self, id: TcpId, remote: IpEndpoint) -> Result<Connect, NetError> {
        let idx = self.tcp_idx(id)?;
        if self.tcp[idx].listener.is_some() {
            return Err(NetError::AlreadyConnected);
        }
        if let Some(e) = self.tcp[idx].connect_err.take() {
            self.tcp[idx].connecting = false;
            return Err(e);
        }
        let h = self.tcp[idx].sock.ok_or(NetError::NotConnected)?;
        let state = self.sockets.get::<tcp::Socket>(h).state();
        if self.tcp[idx].connecting {
            return match state {
                tcp::State::Established => {
                    self.tcp[idx].connecting = false;
                    Ok(Connect::Done)
                }
                tcp::State::SynSent | tcp::State::SynReceived => Ok(Connect::Pending),
                // Closed while connecting is recorded by `poll_tcp` as
                // `connect_err`; reaching here means it has not run yet.
                _ => Ok(Connect::Pending),
            };
        }
        if state != tcp::State::Closed || self.tcp[idx].was_connected {
            return Err(NetError::AlreadyConnected);
        }
        let port = if self.tcp[idx].local_port == 0 { self.tcp_bind(id, None, 0)? } else { self.tcp[idx].local_port };
        let local = IpListenEndpoint { addr: self.tcp[idx].local_addr.map(IpAddress::Ipv4), port };
        let cx = self.iface.context();
        self.sockets
            .get_mut::<tcp::Socket>(h)
            .connect(cx, remote, local)
            .map_err(|e| match e {
                tcp::ConnectError::Unaddressable => NetError::NetUnreachable,
                tcp::ConnectError::InvalidState => NetError::AlreadyConnected,
            })?;
        self.tcp[idx].connecting = true;
        Ok(Connect::Pending)
    }

    /// Queues as much of `data` as fits and returns how much (never 0 for
    /// non-empty `data`: a full buffer is `Again`).
    pub fn tcp_send(&mut self, id: TcpId, data: &[u8]) -> Result<usize, NetError> {
        let was = self.tcp_info(id)?.was_connected;
        let s = self.tcp_sock_mut(id)?;
        match s.send_slice(data) {
            Ok(0) if !data.is_empty() => Err(NetError::Again),
            Ok(n) => Ok(n),
            Err(_) => Err(match s.state() {
                tcp::State::SynSent | tcp::State::SynReceived => NetError::NotConnected,
                _ if was => NetError::BrokenPipe,
                _ => NetError::NotConnected,
            }),
        }
    }

    /// `Ok(0)` is end of stream; `Again` means nothing yet.
    pub fn tcp_recv(&mut self, id: TcpId, buf: &mut [u8], peek: bool) -> Result<usize, NetError> {
        let was = self.tcp_info(id)?.was_connected;
        let s = self.tcp_sock_mut(id)?;
        let r = if peek { s.peek_slice(buf) } else { s.recv_slice(buf) };
        match r {
            Ok(0) if !buf.is_empty() => Err(NetError::Again),
            Ok(n) => Ok(n),
            Err(tcp::RecvError::Finished) => Ok(0),
            Err(tcp::RecvError::InvalidState) => Err(match s.state() {
                tcp::State::SynSent | tcp::State::SynReceived => NetError::Again,
                // Reset (or timed out) after it was up: not a clean EOF.
                tcp::State::Closed if was => NetError::ConnReset,
                _ => NetError::NotConnected,
            }),
        }
    }

    pub fn tcp_local(&mut self, id: TcpId) -> Result<(Ipv4Address, u16), NetError> {
        let i = self.tcp_info(id)?;
        let (a, p) = (i.local_addr, i.local_port);
        let bound = match i.sock {
            Some(h) => self.sockets.get::<tcp::Socket>(h).local_endpoint(),
            None => None,
        };
        if let Some(IpEndpoint { addr: IpAddress::Ipv4(a), port }) = bound {
            return Ok((a, port));
        }
        Ok((a.unwrap_or(Ipv4Address::UNSPECIFIED), p))
    }

    pub fn tcp_peer(&mut self, id: TcpId) -> Result<IpEndpoint, NetError> {
        self.tcp_sock(id)?.remote_endpoint().ok_or(NetError::NotConnected)
    }

    /// Pending asynchronous error (`SO_ERROR`), cleared by reading it.
    pub fn tcp_take_error(&mut self, id: TcpId) -> Result<Option<NetError>, NetError> {
        let i = self.tcp_info(id)?;
        Ok(i.connect_err.take())
    }

    /// `(readable, writable, hup)`; see `tcp_ready`.
    pub fn tcp_mask(&mut self, id: TcpId) -> Result<(bool, bool, bool), NetError> {
        let idx = self.tcp_idx(id)?;
        let r = self.tcp_ready(idx);
        Ok((r.readable, r.writable, r.hup))
    }

    fn tcp_ready(&self, idx: usize) -> TcpReady {
        let i = &self.tcp[idx];
        if let Some(l) = &i.listener {
            let any = l.members.iter().any(|&h| {
                !matches!(self.sockets.get::<tcp::Socket>(h).state(), tcp::State::Listen | tcp::State::SynReceived)
            });
            return TcpReady { readable: any, writable: false, hup: false, state: tcp::State::Listen };
        }
        let s = self.sockets.get::<tcp::Socket>(i.sock.expect("non-listener has a socket"));
        let state = s.state();
        let (readable, writable, hup) = match state {
            tcp::State::SynSent | tcp::State::SynReceived => (false, false, false),
            // Never connected (or refused): POLLOUT|POLLHUP, like Linux.
            tcp::State::Closed => (true, true, true),
            tcp::State::TimeWait => (s.can_recv(), false, true),
            // Peer closed: EOF is readable, we may still write.
            tcp::State::CloseWait => (true, s.can_send(), false),
            _ => (s.can_recv() || !s.may_recv(), s.can_send(), false),
        };
        TcpReady { readable, writable, hup, state }
    }

    /// After an interface poll: record handshake outcomes, report changes,
    /// and reap closed orphans.
    fn poll_tcp(&mut self, woken: &mut Vec<Event>) {
        for idx in 0..self.tcp.len() {
            let ready = self.tcp_ready(idx);
            let i = &mut self.tcp[idx];
            if ready.state == tcp::State::Established || ready.state == tcp::State::CloseWait {
                i.was_connected = true;
            }
            if i.connecting && ready.state == tcp::State::Closed {
                i.connecting = false;
                i.connect_err = Some(NetError::ConnRefused);
            }
            if ready != i.last {
                i.last = ready;
                woken.push(Event::Tcp(i.id));
            }
        }
        let sockets = &mut self.sockets;
        self.orphans.retain(|&h| {
            let done = sockets.get::<tcp::Socket>(h).state() == tcp::State::Closed;
            if done {
                sockets.remove(h);
            }
            !done
        });
    }

    /// Closed sockets still finishing their connection (diagnostics).
    pub fn tcp_orphans(&self) -> usize {
        self.orphans.len()
    }

    /// Half-close: send FIN, keep receiving.
    pub fn tcp_shutdown_write(&mut self, id: TcpId) -> Result<(), NetError> {
        let s = self.tcp_sock_mut(id)?;
        match s.state() {
            tcp::State::Established | tcp::State::CloseWait => {
                s.close();
                Ok(())
            }
            tcp::State::Closed | tcp::State::Listen => Err(NetError::NotConnected),
            _ => Ok(()),
        }
    }

    /// Frees the socket. A live connection is closed gracefully in the
    /// background (FIN, then reaped once the stack is done with it).
    pub fn tcp_close(&mut self, id: TcpId) {
        let Ok(idx) = self.tcp_idx(id) else { return };
        let info = self.tcp.swap_remove(idx);
        if let Some(l) = info.listener {
            for h in l.members {
                let s = self.sockets.get_mut::<tcp::Socket>(h);
                match s.state() {
                    tcp::State::Listen | tcp::State::SynReceived | tcp::State::Closed => {
                        s.abort();
                        self.sockets.remove(h);
                    }
                    _ => {
                        s.abort(); // an unaccepted connection is reset, as on Linux
                        self.orphans.push(h);
                    }
                }
            }
        }
        if let Some(h) = info.sock {
            let s = self.sockets.get_mut::<tcp::Socket>(h);
            match s.state() {
                tcp::State::Closed => {
                    self.sockets.remove(h);
                }
                _ => {
                    s.set_timeout(Some(smoltcp::time::Duration::from_millis(TCP_ORPHAN_TIMEOUT_MS)));
                    s.close();
                    self.orphans.push(h);
                }
            }
        }
    }
}
