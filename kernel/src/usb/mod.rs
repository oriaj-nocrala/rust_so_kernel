// kernel/src/usb/mod.rs
//
// USB support: enough of an xHCI host controller driver to make a USB
// keyboard work as this kernel's keyboard.
//
// Why this exists: on the physical AM4/Ryzen machine this kernel is
// brought up on there is no PS/2 port at all. Every driver in
// `kernel/src/keyboard.rs` and the whole `/dev/input/event0` stack above
// it was reachable only through the 8042 controller, which on that board
// either does not exist or is not wired to anything — so the machine
// booted to a shell nobody could type into.
//
// The shape of the fix is the one design decision worth reading before the
// code: **a USB key press is translated into the PS/2 Set-1 scancode the
// same key would have produced, and fed into the existing
// `keyboard::process_scancode`.** Nothing downstream learns that USB
// exists. The Shift/Ctrl/CapsLock state machine, the ANSI escape
// sequences for the arrow keys, the tty line discipline's Ctrl-C handling,
// `/dev/kbd`, and `/dev/input/event0`'s evdev records all keep working,
// identically, for both keyboards — and a machine with both gets both,
// with no arbitration needed, because they merge into one stream at the
// same place two PS/2 keyboards would.
//
// What is deliberately not here:
//
// * **Hot-plug.** Ports are enumerated once, at boot. Enumerating a device
//   means issuing commands and waiting milliseconds for completions, which
//   cannot happen in the timer ISR where `poll` runs, and this kernel has
//   no kernel-thread context to defer it to. A keyboard plugged in after
//   boot is not seen; unplugging one stops its reports (and releases its
//   held keys) but does not free the slot.
// * **Hubs.** Only devices on root-hub ports are found. A keyboard behind
//   an external hub needs the hub class driver plus route-string handling
//   in the slot context, which is a second project.
// * **Mice, anything else.** A device that is neither a HID boot-protocol
//   keyboard nor Bulk-Only mass storage is addressed (so it is visible in
//   the boot log) and then left alone. `/dev/input/event1` remains the
//   PS/2 mouse.
//
// Mass storage (the boot pendrive, see `xhci/msc.rs`) is the second device
// class; `storage()` / `storage_read()` / `storage_write()` below are what
// `block::UsbBlockDevice` sits on.
// * **Interrupts.** Polled off the 100 Hz timer, for the reason `ac97.rs`
//   polls: the IDT is a `spin::Once` populated before PCI enumeration
//   exists. See `xhci.rs`'s header.

pub mod xhci;

use spin::Mutex;

use crate::hal::{Driver, DriverError};

/// At most this many controllers are brought up. A desktop board commonly
/// has two or three xHCI functions (chipset plus CPU-attached); the
/// keyboard is on exactly one of them and there is no way to know which
/// without enumerating, so all of them are.
const MAX_CONTROLLERS: usize = 4;

/// Scancodes one `poll()` can produce. Two bytes per key transition, and a
/// 10 ms tick cannot realistically carry more than a couple of keys.
const MAX_SCANCODES_PER_POLL: usize = 32;

static CONTROLLERS: Mutex<[Option<xhci::Xhci>; MAX_CONTROLLERS]> =
    Mutex::new([const { None }; MAX_CONTROLLERS]);

/// Number of HID boot keyboards found at boot — read by the boot summary
/// and by `/proc`-style introspection.
static KEYBOARDS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// A mass-storage device that finished bring-up: which controller, which
/// slot, and its size in 512-byte sectors.
#[derive(Debug, Clone, Copy)]
pub struct Storage {
    ctrl: usize,
    slot: u8,
    pub sectors: u64,
}

/// The first mass-storage device found at boot. Only one: the pendrive
/// this kernel boots from. A second stick is enumerated and logged but not
/// offered to the block layer.
static STORAGE: spin::Once<Storage> = spin::Once::new();

pub struct UsbDriver;

impl UsbDriver {
    pub fn new() -> Self {
        UsbDriver
    }
}

impl Default for UsbDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl Driver for UsbDriver {
    fn name(&self) -> &str {
        "usb-xhci"
    }

    fn init(&mut self) -> Result<(), DriverError> {
        let mut found: [Option<(u64, (u8, u8, u8))>; MAX_CONTROLLERS] = [None; MAX_CONTROLLERS];
        let mut n = 0usize;

        crate::pci::for_each_by_class(
            crate::pci::CLASS_SERIAL_BUS,
            crate::pci::SUBCLASS_USB,
            crate::pci::PROGIF_XHCI,
            MAX_CONTROLLERS,
            |f| {
                crate::serial_println!(
                    "usb: xHCI at {:02x}:{:02x}.{} [{:04x}:{:04x}] bar={:#x}",
                    f.bus,
                    f.device,
                    f.function,
                    f.vendor,
                    f.device_id,
                    f.bar0
                );
                if f.bar0 == 0 {
                    crate::serial_println!("usb: ... no memory BAR programmed, skipped");
                    return;
                }
                crate::pci::enable_mem_and_bus_master(f.bus, f.device, f.function);
                found[n] = Some((f.bar0, (f.bus, f.device, f.function)));
                n += 1;
            },
        );

        if n == 0 {
            crate::serial_println!("usb: no xHCI controller found");
            crate::kalert!("usb: NINGUN controlador xHCI en el bus PCI (clase 0C:03:30)");
            return Err(DriverError::NotFound);
        }

        let mut scan = xhci::PortScan::default();
        let mut live = 0usize;
        let mut init_failures = 0usize;
        {
            let mut slots = CONTROLLERS.lock();
            for &(bar, (bus, device, function)) in found.iter().flatten() {
                match xhci::Xhci::init(bar) {
                    Ok(mut ctrl) => {
                        crate::pci::claim(bus, device, function, "xhci");
                        scan.add(&ctrl.enumerate_ports());
                        for (slot, sectors) in ctrl.storage_devices() {
                            if STORAGE.get().is_none() {
                                STORAGE.call_once(|| Storage { ctrl: live, slot, sectors });
                            } else {
                                crate::serial_println!(
                                    "usb-storage: ignoring second device (ctrl {} slot {})", live, slot
                                );
                            }
                        }
                        slots[live] = Some(ctrl);
                        live += 1;
                    }
                    Err(e) => {
                        init_failures += 1;
                        crate::serial_println!("usb: controller at {:#x} failed: {:?}", bar, e);
                    }
                }
            }
        }

        KEYBOARDS.store(scan.keyboards, core::sync::atomic::Ordering::Relaxed);
        crate::serial_println!(
            "usb: {} controller(s) up ({} failed), {} port(s), {} connected, \
             {} addressed, {} setup error(s), {} keyboard(s), {} storage",
            live, init_failures, scan.ports, scan.connected, scan.addressed,
            scan.failed, scan.keyboards, scan.storage,
        );

        // On the actual screen, not just to serial. This driver exists for
        // a machine with no serial capture at all — see `kalert!`'s doc
        // comment, written for that same machine.
        //
        // Both numbers, not just the verdict: the first bare-metal attempt
        // reported nothing typeable and there was no way to tell whether
        // the controller was missing, the ports were empty, or the
        // keyboard had been addressed and then failed to deliver — three
        // problems with three different next steps. The counts separate
        // them in one line.
        if scan.keyboards > 0 {
            crate::kalert!(
                "usb: {} teclado(s) USB OK, {} almacenamiento  [{} ctrl, {} puertos, {} conectados]",
                scan.keyboards, scan.storage, live, scan.ports, scan.connected
            );
        } else if live > 0 {
            crate::kalert!(
                "usb: SIN teclado  [{} ctrl, {} puertos, {} conectados, {} direccionados, {} almac., {} otros, {} errores]",
                live, scan.ports, scan.connected, scan.addressed, scan.storage, scan.other_devices, scan.failed
            );
        } else if n > 0 {
            crate::kalert!("usb: {} controlador(es) xHCI hallados, ninguno arranco", n);
        } else {
            crate::kalert!("usb: NINGUN controlador xHCI en el bus PCI (clase 0C:03:30)");
        }

        if live == 0 {
            return Err(DriverError::Invalid);
        }
        Ok(())
    }
}

/// Number of USB keyboards being polled.
pub fn keyboard_count() -> usize {
    KEYBOARDS.load(core::sync::atomic::Ordering::Relaxed)
}

/// Drains every controller's event ring and feeds any keyboard input into
/// the ordinary keyboard pipeline. Called once per timer tick.
///
/// Two deliberate properties, both about running in ISR context:
///
/// * **`try_lock`, never `lock`.** The lock is held during boot-time
///   enumeration, which waits on hardware for milliseconds. A tick that
///   lands in the middle of that skips its turn rather than spinning with
///   interrupts off, the same "skip this beat" strategy
///   `tick_cursor_blink` uses.
/// * **The lock is released before the scancodes are dispatched.**
///   `keyboard::process_scancode` runs the tty line discipline, which can
///   take the scheduler lock to deliver SIGINT. Holding a driver lock
///   across that would put this driver into the kernel's lock ordering for
///   no reason; collecting into a stack array first keeps it out.
pub fn poll() {
    let mut scancodes = [0u8; MAX_SCANCODES_PER_POLL];
    let mut count = 0usize;

    {
        let Some(mut slots) = CONTROLLERS.try_lock() else {
            return;
        };
        for ctrl in slots.iter_mut().flatten() {
            count += ctrl.poll(&mut scancodes[count..]);
        }
    }

    if count == 0 {
        return;
    }

    crate::debug::inc_usb_key_reports();
    for &sc in &scancodes[..count] {
        crate::keyboard::process_scancode(sc);
    }
    // Same wakeups the PS/2 keyboard ISR performs after pushing input —
    // without them a process blocked in `read(0, ...)` or `poll()` would
    // sleep through its own keystroke. See
    // `init::devices::keyboard_interrupt_handler`.
    crate::process::syscall::stdin_wakeup();
    crate::process::syscall::poll_wakeup_for_fd0();
}

/// The boot-time mass-storage device, if one was found.
pub fn storage() -> Option<Storage> {
    STORAGE.get().copied()
}

/// Reads `count` sectors at `lba` from the storage device into `buf`,
/// split into bounce-buffer-sized transfers.
///
/// **Each transfer runs with interrupts disabled and the `CONTROLLERS`
/// lock held**, released between transfers. Both halves matter:
///
/// * The lock makes this the ring's only reader for the transfer's whole
///   duration (see `Xhci::service_events`), so the timer's `poll` cannot
///   consume the disk's completion.
/// * Interrupts off means the lock holder can never be preempted. Without
///   it, a process preempted mid-read would leave the lock held while the
///   next process to read the disk spins on it — forever, if that one
///   spins with interrupts off. With it, the only other party ever to
///   touch the lock is the timer ISR, and that only `try_lock`s.
///
/// The cost is one transfer's worth of interrupt latency — about a
/// millisecond for 64 KiB on USB 2, the PIT tick it delays simply fires
/// late — bounded by `msc::BULK_TIMEOUT_MS` in the failure case. Keystrokes
/// typed meanwhile are decoded by this very transfer's event draining and
/// handed out by the next `poll`.
pub fn storage_read(dev: &Storage, lba: u32, count: usize, buf: &mut [u8]) -> Result<(), xhci::MscError> {
    let mut done = 0usize;
    while done < count {
        let n = (count - done).min(xhci::MAX_SECTORS);
        let at = lba + done as u32;
        let chunk = &mut buf[done * 512..(done + n) * 512];
        x86_64::instructions::interrupts::without_interrupts(|| {
            let mut slots = CONTROLLERS.lock();
            match slots[dev.ctrl].as_mut() {
                Some(ctrl) => ctrl.storage_read(dev.slot, at, n, chunk),
                None => Err(xhci::MscError::NotStorage),
            }
        })?;
        done += n;
    }
    Ok(())
}

/// Write counterpart of [`storage_read`], same locking.
pub fn storage_write(dev: &Storage, lba: u32, count: usize, buf: &[u8]) -> Result<(), xhci::MscError> {
    let mut done = 0usize;
    while done < count {
        let n = (count - done).min(xhci::MAX_SECTORS);
        let at = lba + done as u32;
        let chunk = &buf[done * 512..(done + n) * 512];
        x86_64::instructions::interrupts::without_interrupts(|| {
            let mut slots = CONTROLLERS.lock();
            match slots[dev.ctrl].as_mut() {
                Some(ctrl) => ctrl.storage_write(dev.slot, at, n, chunk),
                None => Err(xhci::MscError::NotStorage),
            }
        })?;
        done += n;
    }
    Ok(())
}

/// One write of at most [`xhci::MAX_SECTORS`] sectors that gives up
/// instead of waiting for the controller lock — `None` if it is held.
///
/// For the panic handler only (`block::logpart`): a panic can fire in the
/// middle of a transfer, with `CONTROLLERS` held by the code that just
/// died, and a blocking `lock()` there would turn the panic into a silent
/// hang before the panic screen is even drawn.
pub fn try_storage_write(dev: &Storage, lba: u32, count: usize, buf: &[u8]) -> Option<Result<(), xhci::MscError>> {
    if count > xhci::MAX_SECTORS {
        return Some(Err(xhci::MscError::OutOfRange));
    }
    x86_64::instructions::interrupts::without_interrupts(|| {
        let mut slots = CONTROLLERS.try_lock()?;
        Some(match slots[dev.ctrl].as_mut() {
            Some(ctrl) => ctrl.storage_write(dev.slot, lba, count, buf),
            None => Err(xhci::MscError::NotStorage),
        })
    })
}
