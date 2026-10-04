//! Network stack core: pure, host-testable. Nothing blocks; effects come back as data.
//!
//! The kernel's NIC driver implements [`Nic`] (move one raw Ethernet frame in
//! or out, never block); [`NicDevice`] turns that into a smoltcp `Device`.
#![cfg_attr(not(test), no_std)]
extern crate alloc;

pub use smoltcp;

pub mod stack;
pub use stack::{Handle, Lease, NetError, Stack};

use alloc::vec::Vec;
use smoltcp::phy::{self, Checksum, ChecksumCapabilities, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

/// Ethernet MTU the stack assumes (payload, excluding the 14-byte header).
pub const MTU: usize = 1500;
/// Largest frame: MTU + Ethernet header.
pub const MAX_FRAME: usize = MTU + 14;

/// What a NIC driver provides. Both calls are non-blocking.
pub trait Nic {
    /// Copies the next received frame into `buf` and returns its length, or
    /// `None` when none is waiting. Frames longer than `buf` are dropped by
    /// the driver, never truncated.
    fn recv(&mut self, buf: &mut [u8]) -> Option<usize>;
    /// Queues one frame for sending. `false` when the device has no room
    /// (the stack retries on its next poll).
    fn send(&mut self, frame: &[u8]) -> bool;
}

/// smoltcp `Device` over a [`Nic`]. Checksums are computed in software on
/// both directions: the virtio device is configured with no offloads.
pub struct NicDevice<N: Nic> {
    pub nic: N,
}

impl<N: Nic> NicDevice<N> {
    pub fn new(nic: N) -> Self {
        NicDevice { nic }
    }
}

pub struct RxToken(Vec<u8>);
pub struct TxToken<'a, N: Nic>(&'a mut N);

impl phy::RxToken for RxToken {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl<'a, N: Nic> phy::TxToken for TxToken<'a, N> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = alloc::vec![0u8; len];
        let r = f(&mut buf);
        // A full device drops the frame, as a real wire would; TCP resends.
        let _ = self.0.send(&buf);
        r
    }
}

impl<N: Nic> phy::Device for NicDevice<N> {
    type RxToken<'a> = RxToken where N: 'a;
    type TxToken<'a> = TxToken<'a, N> where N: 'a;

    fn receive(&mut self, _ts: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut buf = alloc::vec![0u8; MAX_FRAME];
        let n = self.nic.recv(&mut buf)?;
        buf.truncate(n);
        Some((RxToken(buf), TxToken(&mut self.nic)))
    }

    fn transmit(&mut self, _ts: Instant) -> Option<Self::TxToken<'_>> {
        Some(TxToken(&mut self.nic))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = MAX_FRAME;
        caps.checksum = ChecksumCapabilities::default();
        caps.checksum.ipv4 = Checksum::Both;
        caps.checksum.udp = Checksum::Both;
        caps.checksum.tcp = Checksum::Both;
        caps.checksum.icmpv4 = Checksum::Both;
        caps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::VecDeque;
    use alloc::rc::Rc;
    use core::cell::RefCell;
    use smoltcp::iface::{Config, Interface, SocketSet};
    use smoltcp::socket::udp;
    use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address};

    type Wire = Rc<RefCell<VecDeque<Vec<u8>>>>;

    /// One end of a virtual cable.
    struct Cable {
        rx: Wire,
        tx: Wire,
        /// Frames the "device" refuses to take (full TX ring).
        tx_room: Option<usize>,
    }

    impl Nic for Cable {
        fn recv(&mut self, buf: &mut [u8]) -> Option<usize> {
            let f = self.rx.borrow_mut().pop_front()?;
            buf[..f.len()].copy_from_slice(&f);
            Some(f.len())
        }
        fn send(&mut self, frame: &[u8]) -> bool {
            if let Some(room) = self.tx_room.as_mut() {
                if *room == 0 {
                    return false;
                }
                *room -= 1;
            }
            self.tx.borrow_mut().push_back(frame.to_vec());
            true
        }
    }

    struct Host {
        dev: NicDevice<Cable>,
        iface: Interface,
        sockets: SocketSet<'static>,
    }

    fn host(last: u8, rx: Wire, tx: Wire) -> Host {
        let mut dev = NicDevice::new(Cable { rx, tx, tx_room: None });
        let mac = EthernetAddress([2, 0, 0, 0, 0, last]);
        let mut iface = Interface::new(Config::new(HardwareAddress::Ethernet(mac)), &mut dev, Instant::from_millis(0));
        iface.update_ip_addrs(|a| {
            a.push(IpCidr::new(IpAddress::v4(10, 0, 0, last), 24)).unwrap();
        });
        Host { dev, iface, sockets: SocketSet::new(Vec::new()) }
    }

    fn udp_socket(port: u16) -> udp::Socket<'static> {
        let rx = udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; 4], alloc::vec![0; 2048]);
        let tx = udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; 4], alloc::vec![0; 2048]);
        let mut s = udp::Socket::new(rx, tx);
        s.bind(port).unwrap();
        s
    }

    fn step(hosts: &mut [&mut Host], now: i64) {
        for _ in 0..4 {
            for h in hosts.iter_mut() {
                h.iface.poll(Instant::from_millis(now), &mut h.dev, &mut h.sockets);
            }
        }
    }

    #[test]
    fn two_stacks_resolve_arp_and_echo_udp() {
        let a_to_b: Wire = Default::default();
        let b_to_a: Wire = Default::default();
        let mut a = host(1, b_to_a.clone(), a_to_b.clone());
        let mut b = host(2, a_to_b.clone(), b_to_a.clone());
        let ha = a.sockets.add(udp_socket(4000));
        let hb = b.sockets.add(udp_socket(7));

        let dst = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::new(10, 0, 0, 2)), 7);
        a.sockets.get_mut::<udp::Socket>(ha).send_slice(b"hola red", dst).unwrap();
        step(&mut [&mut a, &mut b], 10);

        // B received it (ARP resolved on the way) and echoes it back.
        let (data, from) = {
            let s = b.sockets.get_mut::<udp::Socket>(hb);
            let (d, m) = s.recv().expect("B got the datagram");
            (d.to_vec(), m.endpoint)
        };
        assert_eq!(data, b"hola red");
        b.sockets.get_mut::<udp::Socket>(hb).send_slice(&data, from).unwrap();
        step(&mut [&mut a, &mut b], 20);

        let s = a.sockets.get_mut::<udp::Socket>(ha);
        let (d, m) = s.recv().expect("A got the echo");
        assert_eq!(d, b"hola red");
        assert_eq!(m.endpoint.addr, IpAddress::v4(10, 0, 0, 2));
    }

    #[test]
    fn corrupted_frames_are_dropped_by_checksum() {
        let a_to_b: Wire = Default::default();
        let b_to_a: Wire = Default::default();
        let mut a = host(1, b_to_a.clone(), a_to_b.clone());
        let mut b = host(2, a_to_b.clone(), b_to_a.clone());
        let ha = a.sockets.add(udp_socket(4000));
        let hb = b.sockets.add(udp_socket(7));
        let dst = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::new(10, 0, 0, 2)), 7);

        // Resolve ARP first with a good datagram.
        a.sockets.get_mut::<udp::Socket>(ha).send_slice(b"warm", dst).unwrap();
        step(&mut [&mut a, &mut b], 10);
        assert!(b.sockets.get_mut::<udp::Socket>(hb).recv().is_ok());

        // Now flip a payload bit in flight.
        a.sockets.get_mut::<udp::Socket>(ha).send_slice(b"bad!", dst).unwrap();
        a.iface.poll(Instant::from_millis(30), &mut a.dev, &mut a.sockets);
        for f in a_to_b.borrow_mut().iter_mut() {
            let n = f.len();
            f[n - 1] ^= 0x01;
        }
        step(&mut [&mut a, &mut b], 30);
        assert!(b.sockets.get_mut::<udp::Socket>(hb).recv().is_err(), "bad checksum must be dropped");
    }

    #[test]
    fn full_tx_ring_loses_the_frame_without_panicking() {
        let a_to_b: Wire = Default::default();
        let b_to_a: Wire = Default::default();
        let mut a = host(1, b_to_a, a_to_b.clone());
        a.dev.nic.tx_room = Some(0);
        let ha = a.sockets.add(udp_socket(4000));
        let dst = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::new(10, 0, 0, 2)), 7);
        a.sockets.get_mut::<udp::Socket>(ha).send_slice(b"x", dst).unwrap();
        a.iface.poll(Instant::from_millis(1), &mut a.dev, &mut a.sockets);
        assert!(a_to_b.borrow().is_empty());
    }

    // ── Stack (socket layer) ──────────────────────────────────────────────

    use crate::stack::{NetError, Stack};

    fn stack(last: u8, rx: Wire, tx: Wire) -> Stack<NicDevice<Cable>> {
        let dev = NicDevice::new(Cable { rx, tx, tx_room: None });
        let mut s = Stack::new(dev, [2, 0, 0, 0, 0, last], 7, Instant::from_millis(0));
        s.set_static(Ipv4Address::new(10, 0, 0, last), 24, None);
        s
    }

    fn pair() -> (Stack<NicDevice<Cable>>, Stack<NicDevice<Cable>>) {
        let a_to_b: Wire = Default::default();
        let b_to_a: Wire = Default::default();
        (stack(1, b_to_a.clone(), a_to_b.clone()), stack(2, a_to_b, b_to_a))
    }

    fn run(a: &mut Stack<NicDevice<Cable>>, b: &mut Stack<NicDevice<Cable>>, now: i64) -> Vec<crate::Handle> {
        let mut woken = Vec::new();
        for _ in 0..4 {
            woken.extend(a.poll(Instant::from_millis(now)));
            woken.extend(b.poll(Instant::from_millis(now)));
        }
        woken
    }

    fn ep(last: u8, port: u16) -> IpEndpoint {
        IpEndpoint::new(IpAddress::v4(10, 0, 0, last), port)
    }

    #[test]
    fn bind_rules() {
        let (mut a, _b) = pair();
        let s1 = a.udp_open();
        let s2 = a.udp_open();
        assert_eq!(a.udp_bind(s1, None, 5000), Ok(5000));
        assert_eq!(a.udp_bind(s2, None, 5000), Err(NetError::AddrInUse));
        assert_eq!(a.udp_bind(s1, None, 5001), Err(NetError::InvalidArg), "already bound");
        let p = a.udp_bind(s2, None, 0).unwrap();
        assert!(p >= 49152, "ephemeral port {}", p);
        assert_ne!(p, 5000);
        a.udp_close(s1);
        let s3 = a.udp_open();
        assert_eq!(a.udp_bind(s3, None, 5000), Ok(5000), "closing frees the port");
    }

    #[test]
    fn echo_between_stacks_with_the_socket_api() {
        let (mut a, mut b) = pair();
        let sa = a.udp_open();
        let sb = b.udp_open();
        b.udp_bind(sb, None, 7).unwrap();
        // Unbound send picks an ephemeral source port.
        assert_eq!(a.udp_send(sa, b"hola", Some(ep(2, 7))), Ok(4));
        let (_, port) = a.udp_local(sa).unwrap();
        assert!(port >= 49152);
        run(&mut a, &mut b, 10);

        let mut buf = [0u8; 64];
        let (n, full, from) = b.udp_recv(sb, &mut buf, false).unwrap();
        assert_eq!((&buf[..n], full), (&b"hola"[..], 4));
        assert_eq!(from, ep(1, port));
        assert_eq!(b.udp_recv(sb, &mut buf, false), Err(NetError::Again));

        b.udp_send(sb, &buf[..n], Some(from)).unwrap();
        run(&mut a, &mut b, 20);
        let (n, _, from) = a.udp_recv(sa, &mut buf, false).unwrap();
        assert_eq!(&buf[..n], b"hola");
        assert_eq!(from, ep(2, 7));
    }

    #[test]
    fn short_buffer_truncates_and_peek_keeps_the_datagram() {
        let (mut a, mut b) = pair();
        let sa = a.udp_open();
        let sb = b.udp_open();
        b.udp_bind(sb, None, 9).unwrap();
        a.udp_send(sa, b"0123456789", Some(ep(2, 9))).unwrap();
        run(&mut a, &mut b, 5);
        let mut small = [0u8; 4];
        let (n, full, _) = b.udp_recv(sb, &mut small, true).unwrap();
        assert_eq!((n, full, &small[..n]), (4, 10, &b"0123"[..]));
        let (n, full, _) = b.udp_recv(sb, &mut small, false).unwrap();
        assert_eq!((n, full), (4, 10), "peek left it queued; this read consumed it");
        assert_eq!(b.udp_recv(sb, &mut small, false), Err(NetError::Again));
    }

    #[test]
    fn connected_socket_sends_by_default_and_drops_strangers() {
        let (mut a, mut b) = pair();
        let sa = a.udp_open();
        let sb = b.udp_open();
        b.udp_bind(sb, None, 7).unwrap();
        assert_eq!(a.udp_send(sa, b"x", None), Err(NetError::DestAddrRequired));
        a.udp_connect(sa, ep(2, 7)).unwrap();
        assert_eq!(a.udp_peer(sa), Ok(Some(ep(2, 7))));
        a.udp_send(sa, b"to-peer", None).unwrap();
        run(&mut a, &mut b, 5);
        let mut buf = [0u8; 16];
        let (n, _, from) = b.udp_recv(sb, &mut buf, false).unwrap();
        assert_eq!(&buf[..n], b"to-peer");

        // B answers from a *different* port: A's connected socket ignores it.
        let other = b.udp_open();
        b.udp_bind(other, None, 8).unwrap();
        b.udp_send(other, b"stranger", Some(from)).unwrap();
        b.udp_send(sb, b"friend", Some(from)).unwrap();
        run(&mut a, &mut b, 10);
        let (n, _, _) = a.udp_recv(sa, &mut buf, false).unwrap();
        assert_eq!(&buf[..n], b"friend");
        assert_eq!(a.udp_recv(sa, &mut buf, false), Err(NetError::Again));
    }

    #[test]
    fn poll_reports_readiness_edges_once() {
        let (mut a, mut b) = pair();
        let sa = a.udp_open();
        let sb = b.udp_open();
        b.udp_bind(sb, None, 7).unwrap();
        assert_eq!(b.udp_mask(sb), Ok((false, true)));
        a.udp_send(sa, b"x", Some(ep(2, 7))).unwrap();
        let woken = run(&mut a, &mut b, 5);
        assert!(woken.contains(&sb), "arrival wakes the reader");
        assert_eq!(b.udp_mask(sb), Ok((true, true)));
        assert!(!run(&mut a, &mut b, 6).contains(&sb), "no new edge, no new wake");
        let mut buf = [0u8; 4];
        b.udp_recv(sb, &mut buf, false).unwrap();
        assert_eq!(b.udp_mask(sb), Ok((false, true)));
    }

    #[test]
    fn unknown_handle_is_ebadf() {
        let (mut a, _b) = pair();
        let s = a.udp_open();
        a.udp_close(s);
        let mut buf = [0u8; 1];
        assert_eq!(a.udp_recv(s, &mut buf, false), Err(NetError::BadHandle));
        assert_eq!(NetError::BadHandle.errno(), 9);
        assert_eq!(NetError::Again.errno(), 11);
    }
}
