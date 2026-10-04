// kernel/src/network/mod.rs
//
// Kernel side of the network stack. The stack itself is the `net` crate
// (smoltcp behind a `Nic` seam, host-tested); this module owns the NIC
// driver and, later, the sockets. Step 1: a polled virtio-net driver.
// Design and status: docs/net/net-plan.md, docs/reference/net.md.

pub mod virtio_net;

use virtio_net::VirtioNet;

/// The one NIC, if the machine has a virtio-net function. A real lock, not
/// IF=0: nothing takes it from an ISR (the driver is polled).
pub static NIC: crate::sync::Mutex<Option<VirtioNet>> = crate::sync::Mutex::new(None);

/// Best-effort, bounded boot step: absent hardware is not an error (the
/// Ryzen has no virtio device; its NIC driver is a later step).
pub fn init() {
    match VirtioNet::probe() {
        Ok(nic) => *NIC.lock() = Some(nic),
        Err(virtio_net::InitError::NoDevice) => {}
        Err(e) => crate::serial_println!("virtio-net: init failed: {:?}", e),
    }
}
