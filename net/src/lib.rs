//! Network stack core: pure, host-testable. Nothing blocks; effects come back as data.
#![cfg_attr(not(test), no_std)]
extern crate alloc;

pub use smoltcp;

#[cfg(test)]
mod tests {
    use smoltcp::wire::{EthernetAddress, Ipv4Address, Ipv4Cidr};

    #[test]
    fn smoltcp_links() {
        let _ = EthernetAddress([2, 0, 0, 0, 0, 1]);
        let c = Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24);
        assert_eq!(c.prefix_len(), 24);
    }
}
