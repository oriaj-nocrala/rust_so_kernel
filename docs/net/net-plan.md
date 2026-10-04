# Network stack plan

1. **virtio-net, polled** (done): `hal::virtio`, `net::NicDevice`, `kernel/src/network/virtio_net.rs`, ping test.
2. **Socket layer** (UDP, TCP client+server and DHCP done; `sendmsg`/`recvmsg` and virtio interrupts pending): a global stack (`smoltcp::Interface` + `SocketSet`) behind a lock that comes before `SCHEDULER`; `AF_INET` `FileHandle`s for UDP/TCP; `poll`/`epoll`; a poll source (timer tick on CPU 0, then virtio interrupts via MSI-X).
3. **Userland** (done: BusyBox wget/nc/nslookup/httpd, mlibc resolver, `/etc/resolv.conf` from the lease; pending: `setitimer`/`SIGALRM`, ICMP sockets for `ping`).
3b. **DHCP/DNS** in the kernel stack (or a userspace client), so `wget`/`nc` work under QEMU's user network.
4. **Real hardware**: the NIC on the AM4 board (driver written against its datasheet, validated with `metal-run`).

Design choice: smoltcp (no_std, heap-optional) instead of a hand-written stack; everything pure lives in `net/` with host tests.
