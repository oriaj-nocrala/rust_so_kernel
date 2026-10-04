# Networking (`net/`, `hal/src/virtio.rs`, `kernel/src/network/`)

Status: steps 1-2b of `docs/net/net-plan.md`. A polled virtio-net driver, DHCP, **AF_INET UDP and TCP sockets** (client and server) work. Not yet: `sendmsg`/`recvmsg` on inet sockets (`EOPNOTSUPP`), raw sockets, IPv6, loopback (a packet to the machine's own address leaves through the NIC).

## Layers

- `net/` (own workspace, host tests): `Nic` trait (`recv`/`send` of raw Ethernet frames, never blocks) and `NicDevice<N>`, a smoltcp `Device` over it. Checksums are done in software both ways. smoltcp is re-exported as `net::smoltcp`.
- `hal/src/virtio.rs` (host tests): virtio-PCI capability decoding, feature negotiation, split-virtqueue layout and `SplitQueue::{push,pop_used}` over a plain `&mut [u8]`, virtio-net header size. No pointers inside.
- `kernel/src/network/virtio_net.rs`: the part that needs hardware: BAR mapping (`memory::mmio::map`, boot-only), `DmaBuf` for the rings and buffers, the reset/feature handshake, doorbells. Implements `net::Nic`. `network::init()` runs at boot after USB and stores the device in `network::NIC`.
- Kernel module is `network`, not `net`, so it does not shadow the crate.

## Sockets (`net/src/stack.rs`, `kernel/src/network/mod.rs`, `process/syscall/inet.rs`)

- `net::Stack<D>`: one smoltcp `Interface` + `SocketSet`, a non-blocking API (`udp_*`, `tcp_*`: `open/bind/listen/accept/connect/send/recv/shutdown_write/mask/close`, `poll`). `poll` returns the sockets whose readiness *changed*, which is what the kernel turns into wakeups. DHCP is opt-in (`enable_dhcp`); `set_static` stops it (a running client reports `Deconfigured` on its first poll and would wipe a static address).
- `network::NET` (`IrqMutex<Option<Net>>`) holds the stack and the AF_INET socket table. Socket ids are `INET_BASE + slot` (`INET_BASE = 1<<32`) in the **same id space as AF_UNIX**, so `unix::block_on`, `dispatch_wakes`, `register_retry` and poll's fd -> socket snapshot work unchanged; `unix::poll_mask` routes ids >= `INET_BASE` to `network::poll_mask`.
- Every `network::udp_*` call runs the op under `NET`, polls the interface (so a `send` leaves immediately), and applies the wakeups after releasing the lock (`NET` comes before `SCHEDULER`, like `SOCKETS`).
- **Polling source**: `network::tick()` from the BSP's 100 Hz branch of `timer_preempt_handler` (global work, CPU 0), next to `usb::poll`, before the scheduler lock. It `try_with`s `NET`: a busy lock just skips the tick. Without it nothing receives (`udp_test` fails at `poll`).
- Syscalls: `ipc.rs` calls `inet.rs` as soon as `socket_of_fd` says the id is inet. Supported: `socket(AF_INET, SOCK_DGRAM)` (+`NONBLOCK`/`CLOEXEC`), `bind`, `connect`, `sendto`/`recvfrom` (`MSG_PEEK`/`MSG_DONTWAIT`/`MSG_TRUNC`), `read`/`write`, `getsockname`/`getpeername`, `poll`/`epoll`, the common `SO_*` (accepted and ignored except `SO_TYPE`/`SO_ERROR`).
- **TCP** (`Stack::tcp_*`, ids are `TcpId`, not smoltcp handles): a *listener* is a group of smoltcp sockets (the backlog, <= 16) in LISTEN on the port; `tcp_accept` takes an established one off the group and adds a replacement. `SYN_RECEIVED` is not acceptable yet. `tcp_connect` is *restartable*: it answers `Pending` until the handshake ends, then `Done` once (a failed handshake is `ConnRefused`, once). The syscall layer relies on that: a blocking `connect` parks (`block_on`) and re-executes. `recv` `Ok(0)` is EOF (data queued before a FIN is still readable); a reset after the connection was up is `ConnReset`; `send` is partial (never 0 for non-empty data) and `Again` when the 64 KiB buffer is full.
- `close` of a live connection is graceful: FIN, then the socket is an *orphan* reaped by `poll` once closed (30 s timeout); closing a listener resets unaccepted connections. `Stack::tcp_orphans` counts them.
- poll bits for TCP: connecting = nothing; `Closed` (never connected or refused) = IN|OUT|HUP like Linux; peer FIN = IN (EOF). `SO_ERROR` returns the pending connect error once. `FileError::{NotConnected, ConnectionReset}` (vfs) map to `ENOTCONN`/`ECONNRESET` in `sys_read`/`sys_write`; there is no SIGPIPE, `write` after a shutdown is just `EPIPE`.
- Ignored options: `TCP_NODELAY`, timeouts (`SO_RCVTIMEO`/`SO_SNDTIMEO` do nothing), `SO_REUSEADDR`, linger, keepalive.
- A send before the lease arrives fails with `ENETUNREACH`; callers retry (`udp_test` does).
- smoltcp drops a datagram whose source address it cannot pick, so an unconfigured interface loses sends silently: check `network::lease()` first.

## Userland

- BusyBox has `wget` (HTTP only, no TLS), `nc` (client and `-l` server), `nslookup` and `httpd` (`busybox-config/minimal.config`). Not built: `telnet` (mlibc has no `arpa/telnet.h`), `ping` (needs `SOCK_RAW`/ICMP sockets, unsupported), `ifconfig`.
- Name resolution is mlibc's own (`lookup.cpp`): `/etc/resolv.conf` (first `nameserver`), `/etc/hosts`, `/etc/services`. They live in the initramfs `/etc` (`fs/initramfs.rs`): `hosts` and `services` are static; **`resolv.conf` is rendered on every open from the DHCP lease** (`network::resolv_conf`: its DNS server, else its router, else empty) through `procfs::rendered`. mlibc's resolver has no timeout: a dead DNS server blocks `getaddrinfo` forever.
- **`setitimer`/`alarm` are not implemented** (the kernel has no interval timers or `SIGALRM`, mlibc's sysdep is missing): `wget` and `httpd` call them for timeouts, and mlibc prints `__ensure(... missing sysdep) failed` for each call but carries on. Next kernel work item for a clean console.
- Guest programs should retry for a few seconds after boot: DHCP takes about a second, until then a send fails with `ENETUNREACH` and `/etc/resolv.conf` is empty.

## Rules

- One descriptor per buffer; a 2 KiB slot holds the 12-byte virtio-net header followed by the frame. No offloads, no `MRG_RXBUF`, no MSI-X yet (`queue_msix_vector = 0xFFFF`).
- RX buffers are re-posted inside `recv` (which maps head -> slot); TX slots are reclaimed lazily in `send`. A full TX ring drops the frame (TCP resends).
- The kernel's `NIC` lock is a real `sync::Mutex`: nothing takes it from an ISR yet. When an interrupt path is added it must become an `IrqMutex` and follow the lock order in `CLAUDE.md`.

## Testing

- `cd net && cargo test`, `cd hal && cargo test virtio`.
- `scripts/run-kernel-tests.sh`: `hw_tests::dhcp_lease_from_qemu` (10.0.2.15/24, router .2, DNS .3) and `hw_tests::virtio_net_pings_the_gateway` pings QEMU's user-mode gateway 10.0.2.2 (ARP + ICMP through both queues). The runner, `cargo run` and `scripts/qemu-debug.sh` (opt out with `QEMU_DEBUG_NO_NET=1`) all attach `-netdev user -device virtio-net-pci,disable-legacy=on`.
- `hw_tests::tcp_with_the_host`: `qemu-test-runner` starts two host peers (an echo server on 127.0.0.1:47001, reached as 10.0.2.2, and a client that reaches the guest's port 7777 through `hostfwd` from 127.0.0.1:47003); the test connects, echoes 200 KB with flow control, half-closes, then listens/accepts the host client. Ports 47001/47003 must be free on the machine running the tests.
- `tcp_test` (guest, `userspace/c/tcp_test.c`): needs the same peers by hand: a Python echo server on 127.0.0.1:47001, then `tcp_test` in the guest; for `tcp_test serve` start qemu-debug with `QEMU_DEBUG_HOSTFWD=tcp:127.0.0.1:47003-:7777` and connect from the host to 127.0.0.1:47003 (send `ping from host`, expect `pong`).
- `udp_test` (guest, `userspace/c/udp_test.c`, run it from the shell): sockets through mlibc, EAGAIN/blocking/`poll`, and a real DNS round trip to QEMU's resolver (needs the host's DNS to answer; SERVFAIL is fine, it checks the id and QR bit).
- Sabotage that fails a test: remove the TX doorbell (`rx 0 tx N`); remove `network::tick()` from the timer (`udp_test` fails at `poll`).
- `scripts/net-e2e.sh [--no-build]`: the whole userland check. It starts an echo server and an HTTP server on the host, boots headless with `hostfwd`, and runs in the guest `udp_test`, `tcp_test` (client and `serve`), `nc`, `nslookup`, `wget` of 300 KB (md5 against the host), and `httpd` fetched from the host. Needs `socat` and `python3`; ports 47001/47003/47010.
- `qemu-debug.sh send/key/screendump` need `socat` on the host.
