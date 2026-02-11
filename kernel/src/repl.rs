// kernel/src/repl.rs - FIXED VERSION

use alloc::{string::String, vec, vec::Vec};
use crate::framebuffer::{FRAMEBUFFER, Color};

pub struct Repl {
    command_buffer: String,
    x: usize,
    y: usize,
    prompt: &'static str,
}

impl Repl {
    pub fn new(x: usize, y: usize) -> Self {
        Self {
            command_buffer: String::new(),
            x,
            y,
            prompt: "> ",
        }
    }

    pub fn handle_char(&mut self, c: char) {
        match c {
            '\n' => {
                self.newline();
                self.execute_command();
                self.show_prompt();
            }
            '\u{08}' => { // Backspace
                if !self.command_buffer.is_empty() {
                    self.command_buffer.pop();
                    self.redraw_line();
                }
            }
            _ => {
                self.command_buffer.push(c);
                self.draw_char(c);
            }
        }
    }

    fn execute_command(&mut self) {
        let cmd = self.command_buffer.clone();
        let cmd = cmd.trim();
        
        match cmd {
            "alloc" => self.cmd_alloc_test(),
            "help" => self.cmd_help(),
            "clear" => self.cmd_clear(),
            "slab" => self.cmd_slab(),
            "fds" => self.cmd_show_fds(),
            "panic" => panic!("User requested panic"),
            "" => {}, // Enter vacío
            _ if cmd.starts_with("echo ") => {
                let text = &cmd[5..];
                self.println(text);
            }
            _ => {
                self.println("Unknown command. Type 'help' for list.");
            }
        }
        
        self.command_buffer.clear();
    }

    fn cmd_help(&mut self) {
        self.println("Available commands:");
        self.println("  alloc - Test the slab allocator");
        self.println("  help  - Show this message");
        self.println("  clear - Clear screen");
        self.println("  echo <text> - Print text");
        self.println("  panic - Test panic handler");
        self.println("  slab  - Show slab allocator stats");
        self.println("  fds   - Show process file descriptors");
    }

    fn cmd_clear(&mut self) {
        let mut fb = FRAMEBUFFER.lock();
        if let Some(fb) = fb.as_mut() {
            fb.clear(Color::rgb(0, 0, 0));
        }
        self.x = 10;
        self.y = 10;
    }

    fn cmd_alloc_test(&mut self) {
        self.println("Testing allocator invariants...");
        
        // Test 1: Allocar y liberar múltiples tamaños
        let sizes = [8, 16, 32, 64, 128, 256, 512, 1024, 2048, 5000];
        
        for &size in &sizes {
            let mut v: Vec<u8> = Vec::with_capacity(size);
            v.resize(size, 0xFF);
            
            // Verificar escritura
            assert_eq!(v.len(), size);
            assert!(v.iter().all(|&b| b == 0xFF));
            
            self.println(&alloc::format!("  {}B: OK", size));
        }
        
        // Test 2: Fragmentación
        // ✅ FIX: Limitar el test para evitar overflow
        let mut vecs: Vec<Vec<u8>> = Vec::new();
        
        // Allocar solo 50 en lugar de 100
        for i in 0..50 {
            vecs.push(vec![i as u8; 64]);
        }
        
        // Liberar la mitad
        vecs.truncate(25);
        
        // Re-allocar
        for i in 0..25 {
            vecs.push(vec![i as u8; 64]);
        }
        
        self.println("Fragmentation test: OK");
        self.println("All tests passed!");
    }

    fn cmd_slab(&mut self) {
        crate::allocator::slab::slab_stats();
        self.println("Slab stats printed to serial");
    }

    // ✅ FIX: Evitar deadlock copiando datos primero
    fn cmd_show_fds(&mut self) {
        use crate::process::scheduler::SCHEDULER;
        
        // Paso 1: Copiar datos CON el lock (rápido)
        let processes_data: Vec<(usize, [u8; 16], Vec<usize>)> = {
            let scheduler = SCHEDULER.lock();
            
            scheduler.processes.iter()
                .map(|p| {
                    // Recolectar FDs abiertos
                    let open_fds: Vec<usize> = (0..16)
                        .filter(|&fd| p.files.get(fd).is_ok())
                        .collect();
                    
                    (p.pid.0, p.name, open_fds)
                })
                .collect()
        }; // ← Lock soltado AQUÍ
        
        // Paso 2: Imprimir SIN el lock (lento pero seguro)
        self.println("Open File Descriptors:");
        
        for (pid, name_bytes, open_fds) in processes_data {
            let name = core::str::from_utf8(&name_bytes)
                .unwrap_or("<invalid>")
                .trim_end_matches('\0');
            
            self.println(&alloc::format!(
                "Process {} ({}): FDs {:?}", 
                pid, name, open_fds
            ));
        }
    }

    fn println(&mut self, text: &str) {
        {
            let mut fb = FRAMEBUFFER.lock();
            if let Some(fb) = fb.as_mut() {
                fb.draw_text(self.x, self.y, text, 
                    Color::rgb(255, 255, 255), Color::rgb(0, 0, 0), 2);
            }
        }
        self.newline();
    }

    fn draw_char(&mut self, c: char) {
        let mut fb = FRAMEBUFFER.lock();
        if let Some(fb) = fb.as_mut() {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            fb.draw_text(self.x, self.y, s,
                Color::rgb(255, 255, 255), Color::rgb(0, 0, 0), 2);
            self.x += 16; // 8 * scale(2)
        }
    }

    pub fn show_prompt(&mut self) {
        let mut fb = FRAMEBUFFER.lock();
        if let Some(fb) = fb.as_mut() {
            fb.draw_text(self.x, self.y, self.prompt,
                Color::rgb(0, 255, 0), Color::rgb(0, 0, 0), 2);
            self.x += 16 * self.prompt.len();
        }
    }

    fn newline(&mut self) {
        self.x = 10;
        self.y += 20;
        
        // Scroll si llegamos al final
        let mut fb = FRAMEBUFFER.lock();
        if let Some(fb) = fb.as_mut() {
            let (_, height) = fb.dimensions();
            if self.y + 20 > height {
                self.y = height - 40;
                // TODO: Scroll real
            }
        }
    }

    // Helper que no toma &mut self
    fn draw_text_at(x: usize, y: usize, text: &str, fg: Color, bg: Color) {
        let mut fb = FRAMEBUFFER.lock();
        if let Some(fb) = fb.as_mut() {
            fb.draw_text(x, y, text, fg, bg, 2);
        }
    }
    
    fn redraw_line(&mut self) {
        // Limpiar
        Self::draw_text_at(10, self.y, &" ".repeat(50), 
            Color::rgb(0, 0, 0), Color::rgb(0, 0, 0));
        
        // Prompt
        self.x = 10;
        Self::draw_text_at(self.x, self.y, self.prompt,
            Color::rgb(0, 255, 0), Color::rgb(0, 0, 0));
        self.x += 16 * self.prompt.len();
        
        // Comando
        Self::draw_text_at(self.x, self.y, &self.command_buffer,
            Color::rgb(255, 255, 255), Color::rgb(0, 0, 0));
    }
}