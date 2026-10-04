// kernel/src/network/mod.rs
//
// Kernel side of the network stack. The stack itself is the `net` crate
// (smoltcp behind a `Nic` seam, host-tested); this module owns the NIC
// driver, the one global stack, the AF_INET socket table and the polling.
// Design and status: docs/net/net-plan.md, docs/reference/net.md.
//
// ── Locking ──────────────────────────────────────────────────────────────
// `NET` is an `IrqMutex`: it is taken from process context (syscalls) and
// from the BSP's 100 Hz timer ISR (`tick`, with `try_with`: a tick that finds
// it held just skips). It comes before `SCHEDULER` in the lock order, like
// `SOCKETS`: every operation computes the `Wakes` inside the lock and
// `unix::dispatch_wakes` applies them after it is released.
//
// ── Socket ids ───────────────────────────────────────────────────────────
// AF_INET sockets live in the same id space the blocking and poll machinery
// already uses for AF_UNIX (`SocketId` = usize), offset by `INET_BASE`, so
// `unix::block_on`, `dispatch_wakes` and poll's fd→socket snapshot work
// unchanged. Ids below `INET_BASE` are AF_UNIX.

pub mod virtio_net;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use diag::IrqMutex;
use net::smoltcp::time::Instant;
use net::smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address};
use net::{Handle, NetError, Stack};
use usock::{PollMask, Wakes};

use crate::allocator::KernelIrq;
use crate::ipc::unix;
use crate::process::file::{FileError, FileHandle, FileResult};
use virtio_net::VirtioNet;

pub const INET_BASE: usize = 1 << 32;

pub fn is_inet(id: usize) -> bool {
    id >= INET_BASE
}

type KStack = Stack<net::NicDevice<VirtioNet>>;

struct Sock {
    handle: Handle,
    /// Open file descriptions sharing this socket (`dup`, `fork`).
    refs: usize,
}

struct Net {
    stack: KStack,
    /// Slot `i` is socket id `INET_BASE + i`.
    socks: Vec<Option<Sock>>,
}

static NET: IrqMutex<Option<Net>, KernelIrq> = IrqMutex::new(None);

fn now() -> Instant {
    Instant::from_millis((crate::time::ktime_get() / 1_000_000) as i64)
}

impl Net {
    fn handle(&self, id: usize) -> Result<Handle, NetError> {
        id.checked_sub(INET_BASE)
            .and_then(|i| self.socks.get(i))
            .and_then(|s| s.as_ref())
            .map(|s| s.handle)
            .ok_or(NetError::BadHandle)
    }

    fn id_of(&self, h: Handle) -> Option<usize> {
        self.socks.iter().position(|s| s.as_ref().is_some_and(|s| s.handle == h)).map(|i| INET_BASE + i)
    }

    /// Drives the stack and reports the sockets whose readiness changed.
    /// Both lists get every id: a waiter re-executes its syscall and finds
    /// out which of readable/writable it was.
    fn poll(&mut self) -> Wakes {
        let woken = self.stack.poll(now());
        let ids: Vec<usize> = woken.into_iter().filter_map(|h| self.id_of(h)).collect();
        Wakes { readable: ids.clone(), writable: ids, acceptable: Vec::new() }
    }
}

/// Runs `f` on the stack, then polls it so anything `f` queued goes out now,
/// and wakes whoever that made ready. `None` when there is no NIC.
fn with_net<R>(f: impl FnOnce(&mut Net) -> R) -> Option<R> {
    let (r, wakes) = NET.with(|n| {
        let n = n.as_mut()?;
        let r = f(n);
        Some((r, n.poll()))
    })?;
    unix::dispatch_wakes(&wakes);
    Some(r)
}

/// Best-effort, bounded boot step: absent hardware is not an error (the
/// Ryzen has no virtio device; its NIC driver is a later step).
pub fn init() {
    let nic = match VirtioNet::probe() {
        Ok(nic) => nic,
        Err(virtio_net::InitError::NoDevice) => return,
        Err(e) => {
            crate::serial_println!("virtio-net: init failed: {:?}", e);
            return;
        }
    };
    let mac = nic.mac();
    let mut seed = [0u8; 8];
    crate::random::fill(&mut seed);
    let mut stack = Stack::new(net::NicDevice::new(nic), mac, u64::from_le_bytes(seed), now());
    stack.enable_dhcp();
    stack.poll(now());
    NET.with(|n| *n = Some(Net { stack, socks: Vec::new() }));
}

/// The BSP's 100 Hz timer tick: drives the stack (receive, retransmit
/// timers, DHCP) and wakes waiters. Runs in the ISR, so it only `try`s the
/// lock; the next tick catches up. Global work: CPU 0 only.
pub fn tick() {
    let wakes = NET.try_with(|n| n.as_mut().map(|n| n.poll())).flatten();
    if let Some(w) = wakes {
        unix::dispatch_wakes(&w);
    }
}

pub fn lease() -> Option<net::Lease> {
    NET.with(|n| n.as_ref().and_then(|n| n.stack.lease()))
}

// ── UDP sockets ──────────────────────────────────────────────────────────

pub fn udp_open() -> Result<usize, NetError> {
    with_net(|n| {
        let handle = n.stack.udp_open();
        let sock = Some(Sock { handle, refs: 1 });
        let slot = match n.socks.iter().position(|s| s.is_none()) {
            Some(i) => {
                n.socks[i] = sock;
                i
            }
            None => {
                n.socks.push(sock);
                n.socks.len() - 1
            }
        };
        INET_BASE + slot
    })
    .ok_or(NetError::NetUnreachable)
}

fn retain(id: usize) -> bool {
    NET.with(|n| {
        let slot = n.as_mut().and_then(|n| n.socks.get_mut(id.wrapping_sub(INET_BASE)));
        match slot {
            Some(Some(s)) => {
                s.refs += 1;
                true
            }
            _ => false,
        }
    })
}

fn release(id: usize) {
    NET.with(|n| {
        let Some(n) = n.as_mut() else { return };
        let Some(slot) = n.socks.get_mut(id.wrapping_sub(INET_BASE)) else { return };
        let done = match slot {
            Some(s) => {
                s.refs -= 1;
                s.refs == 0
            }
            None => false,
        };
        if done {
            let h = slot.take().unwrap().handle;
            n.stack.udp_close(h);
        }
    });
}

/// `op` on socket `id`'s UDP side. `Err(BadHandle)` for a dead id.
fn on_sock<R>(id: usize, op: impl FnOnce(&mut KStack, Handle) -> Result<R, NetError>) -> Result<R, NetError> {
    with_net(|n| n.handle(id).and_then(|h| op(&mut n.stack, h))).unwrap_or(Err(NetError::NetUnreachable))
}

pub fn udp_bind(id: usize, addr: Option<Ipv4Address>, port: u16) -> Result<u16, NetError> {
    on_sock(id, |s, h| s.udp_bind(h, addr, port))
}

pub fn udp_connect(id: usize, peer: IpEndpoint) -> Result<(), NetError> {
    on_sock(id, |s, h| s.udp_connect(h, peer))
}

pub fn udp_send(id: usize, data: &[u8], dest: Option<IpEndpoint>) -> Result<usize, NetError> {
    on_sock(id, |s, h| s.udp_send(h, data, dest))
}

pub fn udp_recv(id: usize, buf: &mut [u8], peek: bool) -> Result<(usize, usize, IpEndpoint), NetError> {
    on_sock(id, |s, h| s.udp_recv(h, buf, peek))
}

pub fn udp_local(id: usize) -> Result<(Ipv4Address, u16), NetError> {
    on_sock(id, |s, h| s.udp_local(h))
}

pub fn udp_peer(id: usize) -> Result<Option<IpEndpoint>, NetError> {
    on_sock(id, |s, h| s.udp_peer(h))
}

/// Readiness for `poll`/`epoll`. Pure read of the stack's state: it runs
/// from wakeup paths, so it never polls the interface itself.
pub fn poll_mask(id: usize) -> Option<PollMask> {
    NET.with(|n| {
        let n = n.as_mut()?;
        let h = n.handle(id).ok()?;
        let (readable, writable) = n.stack.udp_mask(h).ok()?;
        Some(PollMask { readable, writable, hup: false, err: false })
    })
}

pub fn ipv4_of(ep: &IpEndpoint) -> Option<Ipv4Address> {
    match ep.addr {
        IpAddress::Ipv4(a) => Some(a),
    }
}

// ── The fd-facing handle ─────────────────────────────────────────────────

/// An AF_INET datagram socket behind a file descriptor. Same contract as
/// `UnixSocketHandle`: `read`/`write` never block internally, they return
/// `WouldBlock` after `register_retry` and let `sys_read`/`sys_write` park.
pub struct InetSocketHandle {
    id: usize,
    nonblock: Arc<AtomicBool>,
}

impl InetSocketHandle {
    /// Wraps a socket `udp_open` returned (the handle owns its one reference).
    pub fn new(id: usize) -> Self {
        Self { id, nonblock: Arc::new(AtomicBool::new(false)) }
    }

    pub fn set_nonblocking(&self, on: bool) {
        self.nonblock.store(on, Ordering::Relaxed);
    }
}

fn file_error_of(e: NetError) -> FileError {
    match e {
        NetError::Again => FileError::Again,
        NetError::InvalidArg | NetError::DestAddrRequired => FileError::InvalidArgument,
        NetError::BadHandle => FileError::BadFileDescriptor,
        _ => FileError::IOError,
    }
}

impl FileHandle for InetSocketHandle {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        loop {
            let epoch = unix::wake_epoch();
            return match udp_recv(self.id, buf, false) {
                Ok((n, _, _)) => Ok(n),
                Err(NetError::Again) if self.nonblock.load(Ordering::Relaxed) => Err(FileError::Again),
                Err(NetError::Again) => {
                    if !unix::register_retry(self.id, epoch) {
                        continue;
                    }
                    Err(FileError::WouldBlock)
                }
                Err(e) => Err(file_error_of(e)),
            };
        }
    }

    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        loop {
            let epoch = unix::wake_epoch();
            return match udp_send(self.id, buf, None) {
                Ok(n) => Ok(n),
                Err(NetError::Again) if self.nonblock.load(Ordering::Relaxed) => Err(FileError::Again),
                Err(NetError::Again) => {
                    if !unix::register_retry(self.id, epoch) {
                        continue;
                    }
                    Err(FileError::WouldBlock)
                }
                Err(e) => Err(file_error_of(e)),
            };
        }
    }

    fn name(&self) -> &str {
        "<inet socket>"
    }

    fn socket_id(&self) -> Option<usize> {
        Some(self.id)
    }

    fn dup(&self) -> Option<Box<dyn FileHandle>> {
        if !retain(self.id) {
            return None;
        }
        Some(Box::new(InetSocketHandle { id: self.id, nonblock: self.nonblock.clone() }))
    }

    fn stat(&self) -> Option<vfs::types::Stat> {
        Some(vfs::types::Stat::socket(self.id as u64))
    }

    fn nonblocking(&self) -> bool {
        self.nonblock.load(Ordering::Relaxed)
    }

    fn set_nonblocking(&self, on: bool) -> bool {
        self.nonblock.store(on, Ordering::Relaxed);
        true
    }
}

impl Drop for InetSocketHandle {
    // Reference counting lives in `Drop` (see `UnixSocketHandle`). Runs
    // inside `sys_exit` too: `release` only disables interrupts around the
    // lock, it never re-enables them.
    fn drop(&mut self) {
        release(self.id);
    }
}
