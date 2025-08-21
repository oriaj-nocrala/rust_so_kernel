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
pub fn initialize() {
    let pic1_mask = inb(PIC1_DATA);
    let pic2_mask = inb(PIC2_DATA);

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

    // Restaurar máscaras
    outb(PIC1_DATA, pic1_mask);
    outb(PIC2_DATA, pic2_mask);
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
