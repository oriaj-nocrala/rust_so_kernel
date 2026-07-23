// panic.rs

use core::panic::PanicInfo;
use core::fmt::Write;
use crate::framebuffer::{Color, Framebuffer};

#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    unsafe { core::arch::asm!("cli"); }

    // Lock-free serial output FIRST: safe from any context (including
    // panics that originate inside an interrupt handler or while the
    // framebuffer lock is already held, which would otherwise deadlock
    // trying to draw the panic screen below).
    crate::serial_println_raw!("\n=== KERNEL PANIC ===");
    if let Some(location) = info.location() {
        crate::serial_println_raw!(
            "  at {}:{}:{}",
            location.file(), location.line(), location.column()
        );
    }
    crate::serial_println_raw!("  {}", info.message());

    // Dump the always-on debug counters (forks/execs/COW faults, lock
    // diagnostics, the cow.rs IF-invariant violation counter — see
    // `kernel::debug`) as part of every panic report. Uses
    // `debug::print_panic_snapshot`, NOT `debug::render_report` — the
    // latter builds an `alloc::string::String` via `format!`, and if
    // whatever corrupted state caused this panic also poisoned the heap,
    // that allocation could itself panic/fault recursively. The snapshot
    // printer does plain atomic loads straight into `serial_println_raw!`
    // (which formats lazily, no allocation) — safe even then, which is
    // exactly the situation a post-mortem needs it most: a hang/double-
    // fault reachable from this same panic path may never get to run
    // `cat /proc/kdebug` interactively again.
    crate::debug::print_panic_snapshot();

    // Best-effort: the framebuffer lock may already be held by whatever
    // code paniced (e.g. a fault inside a framebuffer-holding critical
    // section) — try_lock so we never deadlock the panic handler itself.
    let mut fb_lock = match crate::framebuffer::FRAMEBUFFER.try_lock() {
        Some(guard) => guard,
        None => {
            crate::serial_println_raw!("  (framebuffer locked — skipping panic screen)");
            loop { unsafe { core::arch::asm!("hlt"); } }
        }
    };

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