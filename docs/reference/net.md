# Networking (`net/`, `hal/src/virtio.rs`, `kernel/src/network/`)

Status: steps 1-2a of `docs/net/net-plan.md`. A polled virtio-net driver, DHCP and **AF_INET `SOCK_DGRAM` (UDP) sockets** work. TCP, `sendmsg`/`recvmsg` on inet sockets and `listen`/`accept` do not (`EPROTONOSUPPORT` / `EOPNOTSUPP`).

## Layers

- `net/` (own workspace, host tests): `Nic` trait (`recv`/`send` of raw Ethernet frames, never blocks) and `NicDevice<N>`, a smoltcp `Device` over it. Checksums are done in software both ways. smoltcp is re-exported as `net::smoltcp`.
- `hal/src/virtio.rs` (host tests): virtio-PCI capability decoding, feature negotiation, split-virtqueue layout and `SplitQueue::{push,pop_used}` over a plain `&mut [u8]`, virtio-net header size. No pointers inside.
- `kernel/src/network/virtio_net.rs`: the part that needs hardware: BAR mapping (`memory::mmio::map`, boot-only), `DmaBuf` for the rings and buffers, the reset/feature handshake, doorbells. Implements `net::Nic`. `network::init()` runs at boot after USB and stores the device in `network::NIC`.
- Kernel module is `network`, not `net`, so it does not shadow the crate.

## Sockets (`net/src/stack.rs`, `kernel/src/network/mod.rs`, `process/syscall/inet.rs`)

- `net::Stack<D>`: one smoltcp `Interface` + `SocketSet`, a non-blocking API (`udp_open/bind/connect/send/recv/mask/close`, `poll`). `poll` returns the sockets whose readiness *changed*, which is what the kernel turns into wakeups. DHCP is opt-in (`enable_dhcp`); `set_static` stops it (a running client reports `Deconfigured` on its first poll and would wipe a static address).
- `network::NET` (`IrqMutex<Option<Net>>`) holds the stack and the AF_INET socket table. Socket ids are `INET_BASE + slot` (`INET_BASE = 1<<32`) in the **same id space as AF_UNIX**, so `unix::block_on`, `dispatch_wakes`, `register_retry` and poll's fd -> socket snapshot work unchanged; `unix::poll_mask` routes ids >= `INET_BASE` to `network::poll_mask`.
- Every `network::udp_*` call runs the op under `NET`, polls the interface (so a `send` leaves immediately), and applies the wakeups after releasing the lock (`NET` comes before `SCHEDULER`, like `SOCKETS`).
- **Polling source**: `network::tick()` from the BSP's 100 Hz branch of `timer_preempt_handler` (global work, CPU 0), next to `usb::poll`, before the scheduler lock. It `try_with`s `NET`: a busy lock just skips the tick. Without it nothing receives (`udp_test` fails at `poll`).
- Syscalls: `ipc.rs` calls `inet.rs` as soon as `socket_of_fd` says the id is inet. Supported: `socket(AF_INET, SOCK_DGRAM)` (+`NONBLOCK`/`CLOEXEC`), `bind`, `connect`, `sendto`/`recvfrom` (`MSG_PEEK`/`MSG_DONTWAIT`/`MSG_TRUNC`), `read`/`write`, `getsockname`/`getpeername`, `poll`/`epoll`, the common `SO_*` (accepted and ignored except `SO_TYPE`/`SO_ERROR`).
- A send before the lease arrives fails with `ENETUNREACH`; callers retry (`udp_test` does).
- smoltcp drops a datagram whose source address it cannot pick, so an unconfigured interface loses sends silently: check `network::lease()` first.

## Rules

- One descriptor per buffer; a 2 KiB slot holds the 12-byte virtio-net header followed by the frame. No offloads, no `MRG_RXBUF`, no MSI-X yet (`queue_msix_vector = 0xFFFF`).
- RX buffers are re-posted inside `recv` (which maps head -> slot); TX slots are reclaimed lazily in `send`. A full TX ring drops the frame (TCP resends).
- The kernel's `NIC` lock is a real `sync::Mutex`: nothing takes it from an ISR yet. When an interrupt path is added it must become an `IrqMutex` and follow the lock order in `CLAUDE.md`.

## Testing

- `cd net && cargo test`, `cd hal && cargo test virtio`.
- `scripts/run-kernel-tests.sh`: `hw_tests::dhcp_lease_from_qemu` (10.0.2.15/24, router .2, DNS .3) and `hw_tests::virtio_net_pings_the_gateway` pings QEMU's user-mode gateway 10.0.2.2 (ARP + ICMP through both queues). The runner, `cargo run` and `scripts/qemu-debug.sh` (opt out with `QEMU_DEBUG_NO_NET=1`) all attach `-netdev user -device virtio-net-pci,disable-legacy=on`.
- `udp_test` (guest, `userspace/c/udp_test.c`, run it from the shell): sockets through mlibc, EAGAIN/blocking/`poll`, and a real DNS round trip to QEMU's resolver (needs the host's DNS to answer; SERVFAIL is fine, it checks the id and QR bit).
- Sabotage that fails a test: remove the TX doorbell (`rx 0 tx N`); remove `network::tick()` from the timer (`udp_test` fails at `poll`).
- `qemu-debug.sh send/key/screendump` need `socat` on the host.
