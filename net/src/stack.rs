//! The socket-facing half of the stack: one smoltcp `Interface` and its
//! sockets, behind an API that never blocks. Operations that cannot complete
//! return [`NetError::Again`]; `poll` returns the sockets whose readiness
//! changed so the kernel adapter can wake their waiters. No globals here.

use alloc::vec::Vec;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::Device;
use smoltcp::socket::{dhcpv4, udp};
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

const EPHEMERAL_FIRST: u16 = 49152;
const UDP_QUEUE_PACKETS: usize = 16;
const UDP_QUEUE_BYTES: usize = 32 * 1024;

struct UdpInfo {
    handle: Handle,
    peer: Option<IpEndpoint>,
    /// Readiness last reported by `poll`, to detect edges.
    was_readable: bool,
    was_writable: bool,
}

pub struct Stack<D: Device> {
    dev: D,
    iface: Interface,
    sockets: SocketSet<'static>,
    /// The DHCP client, while one is running (`enable_dhcp`).
    dhcp: Option<Handle>,
    lease: Option<Lease>,
    udp: Vec<UdpInfo>,
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
        Stack { dev, iface, sockets, dhcp: None, lease: None, udp: Vec::new(), next_port: EPHEMERAL_FIRST }
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

    /// Drives the interface once and returns the sockets that became
    /// readable or writable since the last call.
    pub fn poll(&mut self, now: Instant) -> Vec<Handle> {
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

        let mut woken = Vec::new();
        for info in self.udp.iter_mut() {
            let s = self.sockets.get::<udp::Socket>(info.handle);
            let (r, w) = (s.can_recv(), s.can_send());
            if (r && !info.was_readable) || (w && !info.was_writable) {
                woken.push(info.handle);
            }
            info.was_readable = r;
            info.was_writable = w;
        }
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
