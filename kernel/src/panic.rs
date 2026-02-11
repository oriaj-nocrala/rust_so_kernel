// panic.rs

use core::panic::PanicInfo;
use core::fmt::Write;
use crate::framebuffer::{Color, Framebuffer};

#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let mut fb_lock = crate::framebuffer::FRAMEBUFFER.lock();
    unsafe { core::arch::asm!("cli"); }
    
    if let Some(fb) = fb_lock.as_mut()  {

        fb.clear(Color::rgb(0, 0, 170));
        
        let mut writer = FramebufferWriter::new(fb, 10, 10);
        
        let _ = writeln!(writer, "KERNEL PANIC!");
        let _ = writeln!(writer, "========================================");
        let _ = writeln!(writer, "");
        
        if let Some(location) = info.location() {
            let _ = writeln!(writer, "Location:");
            let _ = writeln!(writer, "  File: {}", location.file());
            let _ = writeln!(writer, "  Line: {}", location.line());
            let _ = writeln!(writer, "  Column: {}", location.column());
            let _ = writeln!(writer, "");
        }
        
        let message = info.message();
        let _ = writeln!(writer, "Message:");
        let _ = writeln!(writer, "  {}", message);

        // Agregar info del stack frame si quisieras (más avanzado)
        let _ = writeln!(writer, "");
        let _ = writeln!(writer, "Press any key to reboot (jk, reinicia manualmente)");
        
    }
    
    loop {
        unsafe { core::arch::asm!("hlt"); }
    }
}

// Helper para escribir línea por línea
struct FramebufferWriter<'a> {
    fb: &'a mut Framebuffer,
    x: usize,
    y: usize,
    line_height: usize,
}

impl<'a> FramebufferWriter<'a> {
    fn new(fb: &'a mut Framebuffer, x: usize, y: usize) -> Self {
        Self { fb, x, y, line_height: 10 }
    }
}

impl<'a> core::fmt::Write for FramebufferWriter<'a> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for line in s.lines() {
            self.fb.draw_text(self.x, self.y, line, Color::rgb(255, 255, 255), Color::rgb(0, 0, 170), 1);
            self.y += self.line_height;
        }
        if s.ends_with('\n') {
            self.y += self.line_height;
        }
        Ok(())
    }
}