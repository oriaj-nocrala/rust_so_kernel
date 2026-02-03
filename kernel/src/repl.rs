// kernel/src/repl.rs

use alloc::string::String;
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
            "heap" => self.cmd_heap(),
            "paging" => self.cmd_paging(),
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

    fn cmd_alloc_test(&mut self) {
        use alloc::vec::Vec;

        crate::allocator::expand_heap(65536).ok();
        
        // Intentar allocar mucho
        let mut big_vec: Vec<u8> = Vec::new();
        
        for i in 0..200_000 {
            big_vec.push((i % 256) as u8);
            
            if i % 50_000 == 0 {
                let (used, total) = crate::allocator::bump::heap_stats();
                self.println(&alloc::format!(
                    "Allocated {}KB, heap: {}KB / {}KB",
                    i / 1024,
                    used / 1024,
                    total / 1024
                ));
            }
        }
        
        self.println("Success! Allocated 200KB");
    }

    fn cmd_help(&mut self) {
        self.println("Available commands:");
        self.println("  alloc  - Test dynamic allocation");
        self.println("  help  - Show this message");
        self.println("  clear - Clear screen");
        self.println("  heap  - Show heap stats");
        self.println("  paging - Show page mappings");
        self.println("  echo <text> - Print text");
        self.println("  panic - Test panic handler");
    }

    fn cmd_clear(&mut self) {
        let mut fb = FRAMEBUFFER.lock();
        if let Some(fb) = fb.as_mut() {
            fb.clear(Color::rgb(0, 0, 0));
        }
        self.x = 10;
        self.y = 10;
    }

    fn cmd_heap(&mut self) {
        let (used, total) = crate::allocator::bump::heap_stats();
        let used_kb = used / 1024;
        let total_kb = total / 1024;
        
        self.println(&alloc::format!("Heap: {} KB / {} KB used", used_kb, total_kb));
    }

    // fn cmd_memory(&mut self) {
    //     use bootloader_api::info::MemoryRegionKind;
        
    //     // Necesitas pasar boot_info.memory_regions de alguna forma
    //     // Por ahora, asumamos que lo guardaste globalmente
        
    //     self.println("Memory Map:");
        
    //     for (i, region) in boot_info.memory_regions.iter().enumerate() {
    //         let kind = match region.kind {
    //             MemoryRegionKind::Usable => "Usable",
    //             MemoryRegionKind::Bootloader => "Bootloader",
    //             MemoryRegionKind::UnknownBios(_) => "BIOS",
    //             MemoryRegionKind::UnknownUefi(_) => "UEFI",
    //             _ => "Other",
    //         };
            
    //         let size_kb = (region.end - region.start) / 1024;
            
    //         self.println(&alloc::format!(
    //             "  {}: {:#x}-{:#x} ({} KB) - {}",
    //             i, region.start, region.end, size_kb, kind
    //         ));
    //     }
    // }

    fn cmd_paging(&mut self) {
        use x86_64::VirtAddr;
        use crate::memory::paging::ActivePageTable;
        
        // Accedemos a la dirección REAL de la memoria del heap
        // Usamos una referencia a HEAP_MEMORY para obtener su puntero
        let heap_ptr = unsafe { 
            crate::allocator::bump::HEAP_MEMORY.as_ptr() as u64 
        };

        unsafe {
            let phys_offset = crate::memory::physical_memory_offset();
            let page_table = ActivePageTable::new(phys_offset);
            
            let addrs = [
                0x1000,             // Probablemente Unmapped
                heap_ptr,           // ¡ESTA DEBERÍA ESTAR MAPEADA!
                0xb8000,            // Dirección del buffer VGA (si estás en modo texto)
            ];
            
            for &addr in &addrs {
                let virt = VirtAddr::new(addr);
                match page_table.translate(virt) {
                    Some(phys) => {
                        self.println(&alloc::format!(
                            "V:{:#x} -> P:{:#x}", addr, phys.as_u64()
                        ));
                    }
                    None => {
                        self.println(&alloc::format!("V:{:#x} -> Not mapped", addr));
                    }
                }
            }
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