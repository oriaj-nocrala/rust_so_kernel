# Networking (`net/`, `hal/src/virtio.rs`, `kernel/src/network/`)

Status: step 1 of `docs/net/net-plan.md`. A polled virtio-net driver and the smoltcp seam exist; **no sockets yet** (`AF_INET` is still `EAFNOSUPPORT`).

## Layers

- `net/` (own workspace, host tests): `Nic` trait (`recv`/`send` of raw Ethernet frames, never blocks) and `NicDevice<N>`, a smoltcp `Device` over it. Checksums are done in software both ways. smoltcp is re-exported as `net::smoltcp`.
- `hal/src/virtio.rs` (host tests): virtio-PCI capability decoding, feature negotiation, split-virtqueue layout and `SplitQueue::{push,pop_used}` over a plain `&mut [u8]`, virtio-net header size. No pointers inside.
- `kernel/src/network/virtio_net.rs`: the part that needs hardware: BAR mapping (`memory::mmio::map`, boot-only), `DmaBuf` for the rings and buffers, the reset/feature handshake, doorbells. Implements `net::Nic`. `network::init()` runs at boot after USB and stores the device in `network::NIC`.
- Kernel module is `network`, not `net`, so it does not shadow the crate.

## Rules

- One descriptor per buffer; a 2 KiB slot holds the 12-byte virtio-net header followed by the frame. No offloads, no `MRG_RXBUF`, no MSI-X yet (`queue_msix_vector = 0xFFFF`).
- RX buffers are re-posted inside `recv` (which maps head -> slot); TX slots are reclaimed lazily in `send`. A full TX ring drops the frame (TCP resends).
- The kernel's `NIC` lock is a real `sync::Mutex`: nothing takes it from an ISR yet. When an interrupt path is added it must become an `IrqMutex` and follow the lock order in `CLAUDE.md`.

## Testing

- `cd net && cargo test`, `cd hal && cargo test virtio`.
- `scripts/run-kernel-tests.sh`: `hw_tests::virtio_net_pings_the_gateway` pings QEMU's user-mode gateway 10.0.2.2 (ARP + ICMP through both queues). The runner, `cargo run` and `scripts/qemu-debug.sh` (opt out with `QEMU_DEBUG_NO_NET=1`) all attach `-netdev user -device virtio-net-pci,disable-legacy=on`.
- Sabotage that fails the test: remove the TX doorbell (`rx 0 tx N`).
