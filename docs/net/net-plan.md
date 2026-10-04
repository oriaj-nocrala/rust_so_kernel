# Network stack plan

1. **virtio-net, polled** (done): `hal::virtio`, `net::NicDevice`, `kernel/src/network/virtio_net.rs`, ping test.
2. **Socket layer** (UDP, TCP client+server and DHCP done; `sendmsg`/`recvmsg` and virtio MSI-X done): a global stack (`smoltcp::Interface` + `SocketSet`) behind a lock that comes before `SCHEDULER`; `AF_INET` `FileHandle`s for UDP/TCP; `poll`/`epoll`; a poll source (timer tick on CPU 0, then virtio interrupts via MSI-X).
3. **Userland** (done: BusyBox wget/nc/nslookup/httpd, mlibc resolver, `/etc/resolv.conf` from the lease; `setitimer`/`SIGALRM` and raw ICMP for `ping` done).
3b. **DHCP/DNS** in the kernel stack (or a userspace client), so `wget`/`nc` work under QEMU's user network.
4. **Real hardware**: the Realtek RTL8168 on the AM4 board: driver written (generic init, host-tested against a model), **not yet run** — see `docs/net/rtl8168.md` for the ladder and what to bring back.

Design choice: smoltcp (no_std, heap-optional) instead of a hand-written stack; everything pure lives in `net/` with host tests.

## Open issues

- **Latency floor (~6-9 ms).** `ping` RTT to QEMU's gateway is the same polled (10 ms tick) and with the MSI-X interrupt, so the tick is not what bounds it. Unmeasured suspects: QEMU's user network and TCG, and the wake-to-run path of a process sleeping on another (idle, `hlt`) CPU (does `dispatch_wakes` kick it with a reschedule IPI, or does it wait for that CPU's next tick?). To find out: stamp `ktime_get` in `network::irq`, at the wake in `dispatch_wakes`, and when the woken syscall restarts, and print the three deltas through `/proc/kdebug`. Until then, do not quote a latency gain for the interrupt.
