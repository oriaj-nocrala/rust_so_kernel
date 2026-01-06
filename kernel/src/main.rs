#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

mod framebuffer;
mod keyboard;
pub mod interrupts;
mod pit;

use core::panic::PanicInfo;
use bootloader_api::{entry_point, BootInfo};
use framebuffer::Framebuffer;
use lazy_static::lazy_static;
use interrupts::idt::InterruptDescriptorTable;

use crate::interrupts::exception::ExceptionStackFrame;

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        // Registramos el manejador del teclado
        idt.add_handler(interrupts::pic::Irq::Keyboard.as_u8(), keyboard_interrupt_handler);
        idt
    };
}

extern "x86-interrupt" fn keyboard_interrupt_handler(stack_frame: &mut ExceptionStackFrame) {
    keyboard::process_scancode();
    unsafe {
        interrupts::pic::end_of_interrupt(interrupts::pic::Irq::Keyboard.as_u8());
    }
}

entry_point!(kernel_main);

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    let fb = boot_info.framebuffer.as_mut().expect("No framebuffer");
    let info = fb.info();
    let buffer = fb.buffer_mut();

    let mut framebuffer = Framebuffer::new(
        buffer,
        info.width as usize,
        info.height as usize,
        info.stride as usize,
        info.bytes_per_pixel as usize,
    );

    // Colores
    let fg = [0xFF, 0xFF, 0xFF]; // Blanco (B, G, R)
    let bg = [0x00, 0x00, 0x00]; // Negro

    // Limpiar pantalla
    framebuffer.clear(bg);

    // Dibujar texto inicial
    framebuffer.draw_text(10, 10, "Escribe algo:", fg, bg, 2);

    // Inicializar interrupciones
    interrupts::pic::initialize();
    interrupts::pic::enable_irq(1); // Habilitar IRQ1 (teclado)
    IDT.load();
    unsafe {
        core::arch::asm!("sti");
    }

    // Inicializar el PIT
    pit::init(100); // 100 Hz

    let mut x = 10;
    let mut y = 50;
    let scale = 2;
    let char_width = 8 * scale;

    loop {
        // Deshabilitamos las interrupciones temporalmente para leer el buffer de forma segura
        unsafe {
            core::arch::asm!("cli");
        }

        let key = keyboard::read_from_buffer();

        // Volvemos a habilitar las interrupciones
        unsafe {
            core::arch::asm!("sti");
        }

        if let Some(character) = key {
            match character {
                '\n' => {
                    x = 10;
                    y += 20; // Nueva línea
                }
                '\u{08}' => { // Backspace
                    if x > 10 {
                        x -= char_width;
                        framebuffer.draw_char(x, y, ' ' as u8, fg, bg, scale);
                    }
                }
                _ => {
                    let mut text = [0u8; 4];
                    character.encode_utf8(&mut text);
                    framebuffer.draw_text(x, y, core::str::from_utf8(&text).unwrap_or(""), fg, bg, scale);
                    x += char_width;
                }
            }
        }
    }
}

