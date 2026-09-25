//! USB descriptor parsing — pure logic, no seam at all.
//!
//! Everything a host controller driver has to understand *about the device
//! it just addressed* is a byte blob it read over a control transfer: the
//! device descriptor, then the configuration descriptor with its interface
//! and endpoint descriptors packed one after another. Parsing that blob
//! needs no hardware, so it lives here where `cargo test` reaches it —
//! same reasoning as `hal::acpi`'s table parser, and with the same
//! anti-OOB discipline: **every length field is bounds-checked before it is
//! trusted for anything**, including for advancing the parse cursor.
//!
//! That discipline is not hypothetical here. The Quake WAV parser in this
//! repo shipped exactly the bug this module is written to avoid (a
//! declared chunk size trusted before being checked against the remaining
//! buffer), and a USB configuration blob is strictly more hostile: it is
//! supplied by whatever hardware the user plugged in.

// ── Descriptor types (USB 2.0 §9.4, table 9-5) ───────────────────────────────

pub const DESC_DEVICE: u8 = 0x01;
pub const DESC_CONFIGURATION: u8 = 0x02;
pub const DESC_INTERFACE: u8 = 0x04;
pub const DESC_ENDPOINT: u8 = 0x05;
/// SuperSpeed Endpoint Companion (USB 3.2 §9.6.7) — follows each endpoint
/// descriptor of a USB 3 device and carries `bMaxBurst`.
pub const DESC_SS_ENDPOINT_COMPANION: u8 = 0x30;

/// USB class/subclass/protocol triples identifying the two boot-protocol
/// HID devices (USB HID 1.11 §4.3 + Appendix B: "Boot Interface
/// Subclass").
pub const CLASS_HID: u8 = 0x03;
pub const SUBCLASS_BOOT: u8 = 0x01;
pub const PROTOCOL_KEYBOARD: u8 = 0x01;
pub const PROTOCOL_MOUSE: u8 = 0x02;

/// Endpoint transfer types (`bmAttributes & 0x03`).
pub const XFER_BULK: u8 = 0x02;
pub const XFER_INTERRUPT: u8 = 0x03;

// ── Device descriptor ────────────────────────────────────────────────────────

/// The handful of device-descriptor fields this kernel actually uses. Not
/// a faithful `#[repr(C)]` mirror of the 18-byte on-wire structure on
/// purpose — decoding into owned fields with `from_le_bytes` is what makes
/// the parse host-safe and alignment-independent, the same choice
/// `hal::acpi` made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceDescriptor {
    /// `bMaxPacketSize0`. For full-speed devices this is a *byte count*
    /// (8/16/32/64); for SuperSpeed it is an exponent (9 → 512 bytes).
    pub max_packet0: u8,
    pub vendor: u16,
    pub product: u16,
    pub num_configurations: u8,
}

/// Parses the first 8 bytes of a device descriptor (the short read every
/// enumeration does first, since `bMaxPacketSize0` is the last byte of that
/// prefix and is needed before a longer transfer can be issued safely).
/// A full 18-byte descriptor parses through the same function — the extra
/// fields are read only when present.
pub fn parse_device_descriptor(buf: &[u8]) -> Option<DeviceDescriptor> {
    if buf.len() < 8 || buf[1] != DESC_DEVICE {
        return None;
    }
    let max_packet0 = buf[7];
    let (vendor, product, num_configurations) = if buf.len() >= 18 {
        (
            u16::from_le_bytes([buf[8], buf[9]]),
            u16::from_le_bytes([buf[10], buf[11]]),
            buf[17],
        )
    } else {
        (0, 0, 1)
    };
    Some(DeviceDescriptor { max_packet0, vendor, product, num_configurations })
}

// ── Configuration descriptor walk ────────────────────────────────────────────

/// `wTotalLength` out of a configuration descriptor's 9-byte header — the
/// size of the whole interface/endpoint blob that follows it, which is what
/// the second (long) `GET_DESCRIPTOR(Configuration)` has to ask for.
pub fn config_total_length(buf: &[u8]) -> Option<u16> {
    if buf.len() < 9 || buf[1] != DESC_CONFIGURATION {
        return None;
    }
    Some(u16::from_le_bytes([buf[2], buf[3]]))
}

/// `bConfigurationValue` — the argument `SET_CONFIGURATION` takes. Not
/// necessarily 1, despite nearly every device using 1.
pub fn config_value(buf: &[u8]) -> Option<u8> {
    if buf.len() < 9 || buf[1] != DESC_CONFIGURATION {
        return None;
    }
    Some(buf[5])
}

/// Everything needed to drive one boot-protocol HID interface (keyboard or
/// mouse), found by walking a configuration blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootHidInterface {
    /// `bConfigurationValue` to pass to `SET_CONFIGURATION`.
    pub config_value: u8,
    /// `bInterfaceNumber` — the `wIndex` of the HID class requests
    /// (`SET_PROTOCOL`, `SET_IDLE`).
    pub interface: u8,
    /// `bEndpointAddress` of the interrupt IN endpoint, including its
    /// direction bit (0x80).
    pub ep_address: u8,
    /// `wMaxPacketSize` of that endpoint (8 for a boot keyboard, 3-8 for a
    /// boot mouse — read rather than assumed).
    pub ep_max_packet: u16,
    /// `bInterval`, in the encoding of the device's own speed — see
    /// `hal::xhci::endpoint_interval`.
    pub ep_interval: u8,
}

impl BootHidInterface {
    /// Endpoint number without the direction bit (1..=15).
    pub fn ep_number(&self) -> u8 {
        self.ep_address & 0x0F
    }
}

/// Walks a configuration descriptor blob looking for a HID boot-protocol
/// keyboard interface and its interrupt IN endpoint.
pub fn find_boot_keyboard(config: &[u8]) -> Option<BootHidInterface> {
    find_boot_interface(config, PROTOCOL_KEYBOARD)
}

/// Same walk as [`find_boot_keyboard`], for a boot-protocol mouse.
///
/// One device can carry both: a wireless keyboard+mouse receiver exposes
/// one interface of each, and gaming mice commonly declare a boot
/// *keyboard* interface next to the mouse one to send their macro
/// buttons' keystrokes (the HyperX Pulsefire Core on the target machine
/// does exactly that — interface 0 mouse, interface 1 keyboard). So the
/// two are looked up independently, never "first HID interface wins".
pub fn find_boot_mouse(config: &[u8]) -> Option<BootHidInterface> {
    find_boot_interface(config, PROTOCOL_MOUSE)
}

/// The walk behind [`find_boot_keyboard`] / [`find_boot_mouse`]: the first
/// boot-subclass HID interface with bInterfaceProtocol `protocol`.
///
/// The walk is the classic "descriptor soup" iteration: each descriptor
/// starts with `[bLength, bDescriptorType]` and the next one begins
/// `bLength` bytes later. Three guards keep a malformed (or hostile) blob
/// from running off the end or spinning forever:
///
/// 1. a descriptor whose header doesn't fit in what remains ends the walk;
/// 2. `bLength < 2` ends the walk (0 would never advance the cursor — an
///    infinite loop, the classic form of this bug);
/// 3. `bLength` past the end of the buffer ends the walk *before* it is
///    used to index or advance.
///
/// Only interfaces with alternate setting 0 are considered: a boot
/// keyboard has no reason to need another, and taking one would require a
/// `SET_INTERFACE` this driver doesn't issue.
fn find_boot_interface(config: &[u8], wanted_protocol: u8) -> Option<BootHidInterface> {
    let config_value = config_value(config)?;

    let mut pos = 0usize;
    // Set once an interface header matches; cleared when a *different*
    // interface starts, so an endpoint is only ever attributed to the
    // interface that actually precedes it.
    let mut current_if: Option<u8> = None;

    while pos + 2 <= config.len() {
        let len = config[pos] as usize;
        let ty = config[pos + 1];
        if len < 2 || pos + len > config.len() {
            break; // guards 2 and 3 — never trust `len` past this point
        }
        let desc = &config[pos..pos + len];

        match ty {
            DESC_INTERFACE if len >= 9 => {
                let alt = desc[3];
                let class = desc[5];
                let subclass = desc[6];
                let protocol = desc[7];
                current_if = if alt == 0
                    && class == CLASS_HID
                    && subclass == SUBCLASS_BOOT
                    && protocol == wanted_protocol
                {
                    Some(desc[2])
                } else {
                    None
                };
            }
            DESC_ENDPOINT if len >= 7 => {
                if let Some(interface) = current_if {
                    let ep_address = desc[2];
                    let attributes = desc[3];
                    let is_in = ep_address & 0x80 != 0;
                    if is_in && attributes & 0x03 == XFER_INTERRUPT {
                        return Some(BootHidInterface {
                            config_value,
                            interface,
                            ep_address,
                            ep_max_packet: u16::from_le_bytes([desc[4], desc[5]]) & 0x07FF,
                            ep_interval: desc[6],
                        });
                    }
                }
            }
            _ => {}
        }

        pos += len;
    }

    None
}

/// One bulk endpoint of a mass-storage interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BulkEndpoint {
    /// `bEndpointAddress`, direction bit included.
    pub address: u8,
    pub max_packet: u16,
    /// `bMaxBurst` from the SuperSpeed companion descriptor (0 = one
    /// packet per burst). Zero when the device sent no companion, which is
    /// every USB 2 device.
    pub max_burst: u8,
}

impl BulkEndpoint {
    /// Endpoint number without the direction bit (1..=15).
    pub fn number(&self) -> u8 {
        self.address & 0x0F
    }
}

/// A Bulk-Only Transport mass-storage interface and its two bulk
/// endpoints (USB MSC BOT 1.0 §4.3: exactly one bulk IN and one bulk OUT).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MassStorageInterface {
    pub config_value: u8,
    pub interface: u8,
    pub bulk_in: BulkEndpoint,
    pub bulk_out: BulkEndpoint,
}

/// Walks a configuration blob for a SCSI / Bulk-Only mass-storage
/// interface (class 0x08, subclass 0x06, protocol 0x50) at alternate
/// setting 0, and returns it once both of its bulk endpoints have been
/// seen.
///
/// Same three guards as [`find_boot_interface`]. One addition: a USB 3
/// device follows every endpoint descriptor with a SuperSpeed Endpoint
/// Companion, whose `bMaxBurst` belongs to the endpoint *just before it* —
/// so the walk remembers which endpoint it last recorded and patches that
/// one, never a guess.
pub fn find_mass_storage(config: &[u8]) -> Option<MassStorageInterface> {
    use crate::msc::{CLASS_MASS_STORAGE, PROTOCOL_BULK_ONLY, SUBCLASS_SCSI};

    let config_value = config_value(config)?;
    let mut pos = 0usize;
    let mut current_if: Option<u8> = None;
    let mut bulk_in: Option<BulkEndpoint> = None;
    let mut bulk_out: Option<BulkEndpoint> = None;
    // Which endpoint a following companion descriptor describes:
    // Some(true) = the IN one, Some(false) = the OUT one.
    let mut last_ep_in: Option<bool> = None;

    while pos + 2 <= config.len() {
        let len = config[pos] as usize;
        let ty = config[pos + 1];
        if len < 2 || pos + len > config.len() {
            break;
        }
        let desc = &config[pos..pos + len];

        match ty {
            DESC_INTERFACE if len >= 9 => {
                // A new interface ends the previous one. If that one was
                // complete it would already have been returned below.
                bulk_in = None;
                bulk_out = None;
                last_ep_in = None;
                current_if = if desc[3] == 0
                    && desc[5] == CLASS_MASS_STORAGE
                    && desc[6] == SUBCLASS_SCSI
                    && desc[7] == PROTOCOL_BULK_ONLY
                {
                    Some(desc[2])
                } else {
                    None
                };
            }
            DESC_ENDPOINT if len >= 7 => {
                last_ep_in = None;
                if current_if.is_some() && desc[3] & 0x03 == XFER_BULK {
                    let ep = BulkEndpoint {
                        address: desc[2],
                        max_packet: u16::from_le_bytes([desc[4], desc[5]]) & 0x07FF,
                        max_burst: 0,
                    };
                    if ep.address & 0x80 != 0 {
                        if bulk_in.is_none() {
                            bulk_in = Some(ep);
                            last_ep_in = Some(true);
                        }
                    } else if bulk_out.is_none() {
                        bulk_out = Some(ep);
                        last_ep_in = Some(false);
                    }
                }
            }
            DESC_SS_ENDPOINT_COMPANION if len >= 6 => {
                let burst = desc[2].min(15);
                match last_ep_in {
                    Some(true) => {
                        if let Some(ep) = bulk_in.as_mut() {
                            ep.max_burst = burst;
                        }
                    }
                    Some(false) => {
                        if let Some(ep) = bulk_out.as_mut() {
                            ep.max_burst = burst;
                        }
                    }
                    None => {}
                }
                last_ep_in = None;
            }
            _ => {}
        }

        pos += len;

        // Returned only once the interface is complete *and* any companion
        // right after the last endpoint has been consumed — returning at
        // the second endpoint would drop its bMaxBurst.
        if let (Some(interface), Some(i), Some(o)) = (current_if, bulk_in, bulk_out) {
            let next_is_companion =
                pos + 2 <= config.len() && config[pos + 1] == DESC_SS_ENDPOINT_COMPANION;
            if !next_is_companion {
                return Some(MassStorageInterface { config_value, interface, bulk_in: i, bulk_out: o });
            }
        }
    }

    None
}

// ── Control-transfer setup packets (USB 2.0 §9.3) ────────────────────────────

/// An 8-byte `SETUP` packet, built field by field. The xHCI Setup Stage
/// TRB carries these same 8 bytes inline in its first two dwords, so the
/// driver never has to DMA it separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetupPacket {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

pub const REQ_CLEAR_FEATURE: u8 = 0x01;
pub const REQ_GET_DESCRIPTOR: u8 = 0x06;
/// Feature selector ENDPOINT_HALT (USB 2.0 §9.4, table 9-6).
pub const FEATURE_ENDPOINT_HALT: u16 = 0;
pub const REQ_SET_CONFIGURATION: u8 = 0x09;
/// HID class requests (HID 1.11 §7.2).
pub const REQ_HID_SET_IDLE: u8 = 0x0A;
pub const REQ_HID_SET_PROTOCOL: u8 = 0x0B;

impl SetupPacket {
    pub fn get_descriptor(desc_type: u8, index: u8, length: u16) -> Self {
        SetupPacket {
            request_type: 0x80, // device-to-host, standard, device
            request: REQ_GET_DESCRIPTOR,
            value: ((desc_type as u16) << 8) | index as u16,
            index: 0,
            length,
        }
    }

    pub fn set_configuration(value: u8) -> Self {
        SetupPacket {
            request_type: 0x00, // host-to-device, standard, device
            request: REQ_SET_CONFIGURATION,
            value: value as u16,
            index: 0,
            length: 0,
        }
    }

    /// `SET_PROTOCOL(0)` = boot protocol, the whole reason this driver can
    /// avoid parsing HID report descriptors at all.
    pub fn set_boot_protocol(interface: u8) -> Self {
        SetupPacket {
            request_type: 0x21, // host-to-device, class, interface
            request: REQ_HID_SET_PROTOCOL,
            value: 0,
            index: interface as u16,
            length: 0,
        }
    }

    /// `SET_IDLE(0)` — report only on change, never on a timer. Without
    /// it some keyboards re-send the same report every idle period, which
    /// this driver would read as a fresh (identical) state and correctly
    /// ignore, but at a needless cost every poll.
    pub fn set_idle(interface: u8) -> Self {
        SetupPacket {
            request_type: 0x21,
            request: REQ_HID_SET_IDLE,
            value: 0,
            index: interface as u16,
            length: 0,
        }
    }

    /// `CLEAR_FEATURE(ENDPOINT_HALT)` on one endpoint — the device-side
    /// half of un-halting a bulk endpoint. The xHCI's Reset Endpoint only
    /// clears the *controller's* view; without this the device keeps
    /// stalling (and resets its data toggle only when told to).
    pub fn clear_endpoint_halt(ep_address: u8) -> Self {
        SetupPacket {
            request_type: 0x02, // host-to-device, standard, endpoint
            request: REQ_CLEAR_FEATURE,
            value: FEATURE_ENDPOINT_HALT,
            index: ep_address as u16,
            length: 0,
        }
    }

    /// Bulk-Only Mass Storage Reset (BOT §3.1).
    pub fn bulk_only_reset(interface: u8) -> Self {
        SetupPacket {
            request_type: 0x21, // host-to-device, class, interface
            request: crate::msc::REQUEST_BOMS_RESET,
            value: 0,
            index: interface as u16,
            length: 0,
        }
    }

    /// Get Max LUN (BOT §3.2): one byte back, the highest LUN number.
    pub fn get_max_lun(interface: u8) -> Self {
        SetupPacket {
            request_type: 0xA1, // device-to-host, class, interface
            request: crate::msc::REQUEST_GET_MAX_LUN,
            value: 0,
            index: interface as u16,
            length: 1,
        }
    }

    /// The packet's 8 wire bytes, little-endian, as the Setup Stage TRB
    /// wants them.
    pub fn to_bytes(&self) -> [u8; 8] {
        let v = self.value.to_le_bytes();
        let i = self.index.to_le_bytes();
        let l = self.length.to_le_bytes();
        [self.request_type, self.request, v[0], v[1], i[0], i[1], l[0], l[1]]
    }

    /// True when the data stage (if any) moves device→host.
    pub fn is_in(&self) -> bool {
        self.request_type & 0x80 != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn interface_desc(number: u8, alt: u8, class: u8, subclass: u8, protocol: u8) -> [u8; 9] {
        [9, DESC_INTERFACE, number, alt, 1, class, subclass, protocol, 0]
    }

    fn endpoint_desc(address: u8, attributes: u8, max_packet: u16, interval: u8) -> [u8; 7] {
        let mp = max_packet.to_le_bytes();
        [7, DESC_ENDPOINT, address, attributes, mp[0], mp[1], interval]
    }

    fn config_header(total: u16, value: u8) -> [u8; 9] {
        let t = total.to_le_bytes();
        [9, DESC_CONFIGURATION, t[0], t[1], 1, value, 0, 0x80, 50]
    }

    /// A realistic keyboard: config → boot-keyboard interface → its
    /// interrupt IN endpoint.
    fn plain_keyboard() -> Vec<u8> {
        let mut blob = Vec::new();
        blob.extend_from_slice(&config_header(25, 1));
        blob.extend_from_slice(&interface_desc(0, 0, CLASS_HID, SUBCLASS_BOOT, PROTOCOL_KEYBOARD));
        blob.extend_from_slice(&endpoint_desc(0x81, XFER_INTERRUPT, 8, 10));
        blob
    }

    #[test]
    fn device_descriptor_short_read_yields_max_packet() {
        let buf = [18u8, DESC_DEVICE, 0x00, 0x02, 0, 0, 0, 64];
        let d = parse_device_descriptor(&buf).unwrap();
        assert_eq!(d.max_packet0, 64);
        assert_eq!(d.num_configurations, 1); // defaulted, not read
    }

    #[test]
    fn device_descriptor_full_read_yields_ids() {
        let mut buf = [0u8; 18];
        buf[0] = 18;
        buf[1] = DESC_DEVICE;
        buf[7] = 8;
        buf[8..10].copy_from_slice(&0x046Du16.to_le_bytes());
        buf[10..12].copy_from_slice(&0xC31Cu16.to_le_bytes());
        buf[17] = 1;
        let d = parse_device_descriptor(&buf).unwrap();
        assert_eq!((d.vendor, d.product, d.max_packet0), (0x046D, 0xC31C, 8));
    }

    #[test]
    fn device_descriptor_rejects_wrong_type_and_short_buffer() {
        assert!(parse_device_descriptor(&[18, DESC_CONFIGURATION, 0, 0, 0, 0, 0, 8]).is_none());
        assert!(parse_device_descriptor(&[18, DESC_DEVICE, 0]).is_none());
    }

    #[test]
    fn finds_the_boot_keyboard_endpoint() {
        let kb = find_boot_keyboard(&plain_keyboard()).unwrap();
        assert_eq!(kb.config_value, 1);
        assert_eq!(kb.interface, 0);
        assert_eq!(kb.ep_address, 0x81);
        assert_eq!(kb.ep_number(), 1);
        assert_eq!(kb.ep_max_packet, 8);
        assert_eq!(kb.ep_interval, 10);
    }

    /// The composite-device shape that actually ships on most keyboards:
    /// interface 0 is the boot keyboard, interface 1 is a consumer-control
    /// / mouse HID with no boot subclass. Picking the *first* HID endpoint
    /// rather than the first *boot keyboard* one would take the wrong
    /// interface on a device where the order is reversed, which is why the
    /// second case below matters.
    #[test]
    fn skips_non_boot_interfaces_before_and_after() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&config_header(50, 1));
        blob.extend_from_slice(&interface_desc(0, 0, CLASS_HID, 0x00, 0x00));
        blob.extend_from_slice(&endpoint_desc(0x82, XFER_INTERRUPT, 4, 8));
        blob.extend_from_slice(&interface_desc(1, 0, CLASS_HID, SUBCLASS_BOOT, PROTOCOL_KEYBOARD));
        blob.extend_from_slice(&endpoint_desc(0x81, XFER_INTERRUPT, 8, 10));
        let kb = find_boot_keyboard(&blob).unwrap();
        assert_eq!((kb.interface, kb.ep_address), (1, 0x81));
    }

    /// The HyperX Pulsefire Core on the target machine, read from its own
    /// Linux sysfs: interface 0 boot mouse (ep 0x81), interface 1 boot
    /// keyboard for the macro buttons (ep 0x82), interface 2 a vendor HID.
    /// Each lookup must find its own interface, not the first boot one.
    #[test]
    fn mouse_and_keyboard_on_one_device_are_found_separately() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&config_header(66, 1));
        blob.extend_from_slice(&interface_desc(0, 0, CLASS_HID, SUBCLASS_BOOT, PROTOCOL_MOUSE));
        blob.extend_from_slice(&endpoint_desc(0x81, XFER_INTERRUPT, 8, 1));
        blob.extend_from_slice(&interface_desc(1, 0, CLASS_HID, SUBCLASS_BOOT, PROTOCOL_KEYBOARD));
        blob.extend_from_slice(&endpoint_desc(0x82, XFER_INTERRUPT, 8, 1));
        blob.extend_from_slice(&interface_desc(2, 0, CLASS_HID, 0x00, 0x00));
        blob.extend_from_slice(&endpoint_desc(0x83, XFER_INTERRUPT, 64, 1));
        let m = find_boot_mouse(&blob).unwrap();
        assert_eq!((m.interface, m.ep_address, m.ep_max_packet), (0, 0x81, 8));
        let kb = find_boot_keyboard(&blob).unwrap();
        assert_eq!((kb.interface, kb.ep_address), (1, 0x82));
    }

    #[test]
    fn keyboard_only_device_has_no_mouse() {
        assert!(find_boot_mouse(&plain_keyboard()).is_none());
    }

    #[test]
    fn ignores_alternate_settings_and_out_endpoints() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&config_header(41, 1));
        // alt != 0 — not eligible
        blob.extend_from_slice(&interface_desc(0, 1, CLASS_HID, SUBCLASS_BOOT, PROTOCOL_KEYBOARD));
        blob.extend_from_slice(&endpoint_desc(0x81, XFER_INTERRUPT, 8, 10));
        // eligible, but its first endpoint is OUT and the second is bulk
        blob.extend_from_slice(&interface_desc(2, 0, CLASS_HID, SUBCLASS_BOOT, PROTOCOL_KEYBOARD));
        blob.extend_from_slice(&endpoint_desc(0x02, XFER_INTERRUPT, 8, 10));
        blob.extend_from_slice(&endpoint_desc(0x83, 0x02 /* bulk */, 64, 0));
        assert!(find_boot_keyboard(&blob).is_none());
    }

    /// Guard 2: a zero `bLength` must end the walk, not spin on it.
    #[test]
    fn zero_length_descriptor_terminates_the_walk() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&config_header(40, 1));
        blob.extend_from_slice(&[0u8, DESC_INTERFACE, 0, 0]);
        blob.extend_from_slice(&interface_desc(0, 0, CLASS_HID, SUBCLASS_BOOT, PROTOCOL_KEYBOARD));
        blob.extend_from_slice(&endpoint_desc(0x81, XFER_INTERRUPT, 8, 10));
        assert!(find_boot_keyboard(&blob).is_none()); // terminated, and returned
    }

    /// Guard 3: `bLength` longer than what remains must not be trusted —
    /// neither to slice nor to advance past the end.
    #[test]
    fn overlong_length_field_is_not_trusted() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&config_header(200, 1));
        blob.extend_from_slice(&[200u8, DESC_INTERFACE, 0, 0, 1, CLASS_HID]);
        assert!(find_boot_keyboard(&blob).is_none());
    }

    /// A truncated endpoint descriptor (correct `bLength`, buffer cut
    /// short) must not be read past its end either.
    #[test]
    fn truncated_tail_is_ignored() {
        let mut blob = plain_keyboard();
        blob.truncate(blob.len() - 3);
        assert!(find_boot_keyboard(&blob).is_none());
    }

    #[test]
    fn config_header_fields() {
        let blob = plain_keyboard();
        assert_eq!(config_total_length(&blob), Some(25));
        assert_eq!(config_value(&blob), Some(1));
        assert!(config_total_length(&blob[1..]).is_none()); // not a config header
    }

    #[test]
    fn setup_packets_match_the_wire_format() {
        // GET_DESCRIPTOR(Device, 0, 8): 80 06 00 01 00 00 08 00
        let s = SetupPacket::get_descriptor(DESC_DEVICE, 0, 8);
        assert_eq!(s.to_bytes(), [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x08, 0x00]);
        assert!(s.is_in());

        // SET_CONFIGURATION(1): 00 09 01 00 00 00 00 00
        let s = SetupPacket::set_configuration(1);
        assert_eq!(s.to_bytes(), [0x00, 0x09, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00]);
        assert!(!s.is_in());

        // SET_PROTOCOL(boot) on interface 1: 21 0B 00 00 01 00 00 00
        let s = SetupPacket::set_boot_protocol(1);
        assert_eq!(s.to_bytes(), [0x21, 0x0B, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00]);

        // SET_IDLE(0) on interface 1: 21 0A 00 00 01 00 00 00
        let s = SetupPacket::set_idle(1);
        assert_eq!(s.to_bytes(), [0x21, 0x0A, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00]);
    }

    // ── Mass storage ────────────────────────────────────────────────────

    /// A USB 2 pendrive: one BOT interface, bulk IN then bulk OUT.
    #[test]
    fn finds_a_usb2_mass_storage_interface() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&config_header(32, 1));
        blob.extend_from_slice(&interface_desc(0, 0, 0x08, 0x06, 0x50));
        blob.extend_from_slice(&endpoint_desc(0x81, XFER_BULK, 512, 0));
        blob.extend_from_slice(&endpoint_desc(0x02, XFER_BULK, 512, 0));
        let m = find_mass_storage(&blob).unwrap();
        assert_eq!(m.config_value, 1);
        assert_eq!(m.interface, 0);
        assert_eq!(m.bulk_in, BulkEndpoint { address: 0x81, max_packet: 512, max_burst: 0 });
        assert_eq!(m.bulk_out, BulkEndpoint { address: 0x02, max_packet: 512, max_burst: 0 });
        assert_eq!(m.bulk_in.number(), 1);
        assert_eq!(m.bulk_out.number(), 2);
        assert!(find_boot_keyboard(&blob).is_none());
    }

    fn companion(max_burst: u8) -> [u8; 6] {
        [6, DESC_SS_ENDPOINT_COMPANION, max_burst, 0, 0, 0]
    }

    /// A USB 3 stick (the SanDisk shape): each endpoint followed by its
    /// companion, and the *last* companion must still be attributed —
    /// returning as soon as the second endpoint is seen would lose it.
    #[test]
    fn usb3_companions_attach_to_the_right_endpoint() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&config_header(44, 1));
        blob.extend_from_slice(&interface_desc(0, 0, 0x08, 0x06, 0x50));
        blob.extend_from_slice(&endpoint_desc(0x81, XFER_BULK, 1024, 0));
        blob.extend_from_slice(&companion(15));
        blob.extend_from_slice(&endpoint_desc(0x02, XFER_BULK, 1024, 0));
        blob.extend_from_slice(&companion(3));
        let m = find_mass_storage(&blob).unwrap();
        assert_eq!((m.bulk_in.max_packet, m.bulk_in.max_burst), (1024, 15));
        assert_eq!((m.bulk_out.max_packet, m.bulk_out.max_burst), (1024, 3));
    }

    /// UAS sticks offer BOT at alt 0 and UAS (protocol 0x62) at alt 1. The
    /// UAS alternate has four bulk endpoints; none of them may leak into
    /// the BOT result, and a UAS-only interface must not match at all.
    #[test]
    fn uas_alternate_is_ignored() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&config_header(80, 1));
        blob.extend_from_slice(&interface_desc(0, 1, 0x08, 0x06, 0x62));
        blob.extend_from_slice(&endpoint_desc(0x83, XFER_BULK, 512, 0));
        blob.extend_from_slice(&endpoint_desc(0x04, XFER_BULK, 512, 0));
        blob.extend_from_slice(&interface_desc(0, 0, 0x08, 0x06, 0x50));
        blob.extend_from_slice(&endpoint_desc(0x81, XFER_BULK, 512, 0));
        blob.extend_from_slice(&endpoint_desc(0x02, XFER_BULK, 512, 0));
        let m = find_mass_storage(&blob).unwrap();
        assert_eq!((m.bulk_in.address, m.bulk_out.address), (0x81, 0x02));

        let uas_only = &blob[..9 + 9 + 7 + 7];
        assert!(find_mass_storage(uas_only).is_none());
    }

    #[test]
    fn mass_storage_needs_both_endpoints_of_one_interface() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&config_header(48, 1));
        blob.extend_from_slice(&interface_desc(0, 0, 0x08, 0x06, 0x50));
        blob.extend_from_slice(&endpoint_desc(0x81, XFER_BULK, 512, 0));
        // interrupt endpoint doesn't count as the OUT half
        blob.extend_from_slice(&endpoint_desc(0x02, XFER_INTERRUPT, 64, 1));
        // the OUT endpoint of a *different* interface doesn't either
        blob.extend_from_slice(&interface_desc(1, 0, 0xFF, 0, 0));
        blob.extend_from_slice(&endpoint_desc(0x03, XFER_BULK, 512, 0));
        assert!(find_mass_storage(&blob).is_none());
    }

    #[test]
    fn mass_storage_walk_survives_malformed_lengths() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&config_header(40, 1));
        blob.extend_from_slice(&interface_desc(0, 0, 0x08, 0x06, 0x50));
        blob.extend_from_slice(&[0u8, DESC_ENDPOINT, 0x81]);
        assert!(find_mass_storage(&blob).is_none());

        let mut blob = Vec::new();
        blob.extend_from_slice(&config_header(40, 1));
        blob.extend_from_slice(&interface_desc(0, 0, 0x08, 0x06, 0x50));
        blob.extend_from_slice(&endpoint_desc(0x81, XFER_BULK, 512, 0));
        blob.extend_from_slice(&[250u8, DESC_ENDPOINT, 0x02, XFER_BULK]);
        assert!(find_mass_storage(&blob).is_none());
    }

    #[test]
    fn mass_storage_setup_packets() {
        // Bulk-Only Mass Storage Reset, interface 0: 21 FF 00 00 00 00 00 00
        assert_eq!(
            SetupPacket::bulk_only_reset(0).to_bytes(),
            [0x21, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
        // Get Max LUN, interface 0: A1 FE 00 00 00 00 01 00
        let s = SetupPacket::get_max_lun(0);
        assert_eq!(s.to_bytes(), [0xA1, 0xFE, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00]);
        assert!(s.is_in());
        // CLEAR_FEATURE(ENDPOINT_HALT) on EP 0x81: 02 01 00 00 81 00 00 00
        assert_eq!(
            SetupPacket::clear_endpoint_halt(0x81).to_bytes(),
            [0x02, 0x01, 0x00, 0x00, 0x81, 0x00, 0x00, 0x00]
        );
    }
}
