// use core::arch::asm;

use crate::debug_log;
use crate::keyboard_buffer::KEYBOARD_BUFFER;

// TODO: Event driven en vez de POLLED.

// Puertos del teclado PS/2
// const KEYBOARD_DATA_PORT: u16 = 0x60;
// const KEYBOARD_STATUS_PORT: u16 = 0x64;

// --- Lógica del Scancode ---

// /// Lee un byte del puerto de estado del teclado
// fn read_status() -> u8 {
//     let value: u8;
//     unsafe {
//         asm!("in al, dx", in("dx") KEYBOARD_STATUS_PORT, out("al") value, options(nomem, nostack, preserves_flags));
//     }
//     value
// }

// /// Lee un byte del puerto de datos del teclado
// fn read_data() -> u8 {
//     let value: u8;
//     unsafe {
//         asm!("in al, dx", in("dx") KEYBOARD_DATA_PORT, out("al") value, options(nomem, nostack, preserves_flags));
//     }
//     value
// }

/// Procesa el scancode del teclado si hay datos disponibles
/// Esta función es no bloqueante
pub fn process_scancode(scancode: u8) {
    if scancode < 0x80 {
        if let Some(character) = scancode_to_ascii(scancode) {
            // ✅ Escribir al buffer lock-free
            KEYBOARD_BUFFER.push(character);
        }
    }
}

pub fn read_key() -> Option<char> {
    KEYBOARD_BUFFER.pop()
}

fn debug_log_char(c: char) {
    unsafe {
        let mut port = x86_64::instructions::port::PortWriteOnly::<u8>::new(0x3F8);
        if c == '\n' {
            port.write(b'\\');
            port.write(b'n');
        } else if c == '\u{08}' {
            port.write(b'\\');
            port.write(b'b');
        } else {
            port.write(c as u8);
        }
    }
}

fn debug_log_hex(c: u8) {
    let mut port = x86_64::instructions::port::PortWriteOnly::<u8>::new(0x3F8);
    unsafe{
        port.write(c);
    }
}

/// Convierte un scancode (Set 1) a un carácter ASCII si es posible
fn scancode_to_ascii(scancode: u8) -> Option<char> {
    match scancode {
        0x02 => Some('1'), 0x03 => Some('2'), 0x04 => Some('3'), 0x05 => Some('4'),
        0x06 => Some('5'), 0x07 => Some('6'), 0x08 => Some('7'), 0x09 => Some('8'),
        0x0A => Some('9'), 0x0B => Some('0'),
        0x10 => Some('q'), 0x11 => Some('w'), 0x12 => Some('e'), 0x13 => Some('r'),
        0x14 => Some('t'), 0x15 => Some('y'), 0x16 => Some('u'), 0x17 => Some('i'),
        0x18 => Some('o'), 0x19 => Some('p'),
        0x1E => Some('a'), 0x1F => Some('s'), 0x20 => Some('d'), 0x21 => Some('f'),
        0x22 => Some('g'), 0x23 => Some('h'), 0x24 => Some('j'), 0x25 => Some('k'),
        0x26 => Some('l'),
        0x2C => Some('z'), 0x2D => Some('x'), 0x2E => Some('c'), 0x2F => Some('v'),
        0x30 => Some('b'), 0x31 => Some('n'), 0x32 => Some('m'),
        0x39 => Some(' '),
        0x1C => Some('\n'),      // Enter
        0x0E => Some(''), // Backspace
        _ => None,
    }
}
