// pic.rs
// Programmable Interrupt Controller

use core::arch::asm;

// Comandos del PIC
const CMD_INIT: u8 = 0x11;
const CMD_END_OF_INTERRUPT: u8 = 0x20;

// Puertos del PIC
const PIC1_COMMAND: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_COMMAND: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

// Offsets de los vectores de interrupción
pub const PIC1_OFFSET: u8 = 32;
pub const PIC2_OFFSET: u8 = PIC1_OFFSET + 8;

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum Irq {
    Timer = PIC1_OFFSET,
    Keyboard, // 33
    Com1 = PIC1_OFFSET + 4, // 36 — serial (COM1) receive
    Mouse = PIC2_OFFSET + 4, // 44 — PS/2 auxiliary device (IRQ12)
}

impl Irq {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Escribe un byte a un puerto
fn outb(port: u16, value: u8) {
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
    }
}

/// Lee un byte de un puerto
fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Inicializa los PICs 8259
///
/// Leaves **every** line masked. Each line this kernel handles is unmasked
/// explicitly afterwards (`enable_irq` in `init_hardware_interrupts` and
/// `mouse::init`); a line nobody asked for stays off. This used to restore
/// whatever masks the firmware had left, which on real hardware is not
/// guaranteed to be "all masked".
pub fn initialize() {
    // ICW1: Iniciar la secuencia de inicialización
    outb(PIC1_COMMAND, CMD_INIT);
    outb(PIC2_COMMAND, CMD_INIT);

    // ICW2: Offsets de los vectores
    outb(PIC1_DATA, PIC1_OFFSET);
    outb(PIC2_DATA, PIC2_OFFSET);

    // ICW3: Configuración maestro-esclavo
    outb(PIC1_DATA, 4); // PIC2 en IRQ2
    outb(PIC2_DATA, 2); // Identidad en cascada

    // ICW4: Modo 8086
    outb(PIC1_DATA, 1);
    outb(PIC2_DATA, 1);

    // Todo enmascarado; cada línea usada se habilita con `enable_irq`.
    outb(PIC1_DATA, 0xFF);
    outb(PIC2_DATA, 0xFF);
}

/// OCW3: read the In-Service Register on the next read of the command port.
const CMD_READ_ISR: u8 = 0x0B;

/// Is an interrupt on `irq_line` (7 or 15) a *spurious* one?
///
/// When a line drops before the CPU acknowledges it, the 8259 still has to
/// hand the CPU a vector and gives its lowest-priority one — IRQ7 on the
/// master, IRQ15 on the slave — without setting its ISR bit. Real hardware
/// does this routinely (the 2026-09-23 Ryzen panic was one: a GPF with
/// error code 0x13B, i.e. vector 39 delivered to an empty IDT slot);
/// QEMU practically never does. A spurious interrupt must NOT be EOI'd on
/// its own PIC — there is nothing in service there to end — but a spurious
/// IRQ15 still went through the master's cascade line, which is in service
/// and does need its EOI (`end_of_spurious`).
pub fn is_spurious(irq_line: u8) -> bool {
    let (cmd, bit) = if irq_line < 8 {
        (PIC1_COMMAND, irq_line)
    } else {
        (PIC2_COMMAND, irq_line - 8)
    };
    outb(cmd, CMD_READ_ISR);
    inb(cmd) & (1 << bit) == 0
}

/// The EOI a spurious interrupt on `irq_line` needs: none for the master's
/// IRQ7, the master only (for the cascade) for the slave's IRQ15.
pub fn end_of_spurious(irq_line: u8) {
    if irq_line >= 8 {
        outb(PIC1_COMMAND, CMD_END_OF_INTERRUPT);
    }
}

/// Envía la señal de fin de interrupción (EOI)
pub fn end_of_interrupt(irq: u8) {
    if irq >= PIC2_OFFSET {
        outb(PIC2_COMMAND, CMD_END_OF_INTERRUPT);
    }
    outb(PIC1_COMMAND, CMD_END_OF_INTERRUPT);
}

/// Habilita una línea de IRQ específica (0-15)
pub fn enable_irq(irq_line: u8) {
    let port = if irq_line < 8 {
        PIC1_DATA
    } else {
        PIC2_DATA
    };
    let irq_line = if irq_line < 8 { irq_line } else { irq_line - 8 };
    let mask = inb(port);
    outb(port, mask & !(1 << irq_line));
}
