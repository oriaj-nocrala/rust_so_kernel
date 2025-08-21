use core::arch::asm;

// Puertos del PIT
const PIT_CHANNEL_0_DATA: u16 = 0x40;
const PIT_COMMAND: u16 = 0x43;

/// Inicializa el PIT a una frecuencia dada (en Hz)
pub fn init(frequency: u32) {
    let divisor = 1193182 / frequency;
    let l = (divisor & 0xFF) as u8;
    let h = ((divisor >> 8) & 0xFF) as u8;

    unsafe {
        // Comando para configurar el canal 0 en modo 2 (rate generator)
        asm!("out dx, al", in("dx") PIT_COMMAND, in("al") 0x34 as u8, options(nomem, nostack, preserves_flags));
        // Escribir el divisor (low byte, then high byte)
        asm!("out dx, al", in("dx") PIT_CHANNEL_0_DATA, in("al") l, options(nomem, nostack, preserves_flags));
        asm!("out dx, al", in("dx") PIT_CHANNEL_0_DATA, in("al") h, options(nomem, nostack, preserves_flags));
    }
}
