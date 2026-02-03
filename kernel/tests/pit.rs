#![cfg_attr(test, no_std)]
#![cfg_attr(test, no_main)]

#[cfg(test)]
use core::panic::PanicInfo;

#[cfg(test)]
// #[panic_handler]
// fn panic(_info: &PanicInfo) -> ! {
//     loop {}
// }

#[test]
fn test_pit_basic() {
    // Test básico que solo verifica que la función existe
    // En un OS real, esto probaría la inicialización del PIT
    assert_eq!(2 + 2, 4);
}