// kernel/src/keyboard_buffer.rs

use core::sync::atomic::{AtomicUsize, Ordering};

const BUFFER_SIZE: usize = 128;

pub struct KeyboardBuffer {
    buffer: [Option<char>; BUFFER_SIZE],
    read_index: AtomicUsize,
    write_index: AtomicUsize,
}

// SAFETY: Solo accedemos a través de operaciones atómicas
unsafe impl Sync for KeyboardBuffer {}

impl KeyboardBuffer {
    pub const fn new() -> Self {
        Self {
            buffer: [None; BUFFER_SIZE],
            read_index: AtomicUsize::new(0),
            write_index: AtomicUsize::new(0),
        }
    }

    /// Escribe un carácter (llamado desde IRQ handler)
    pub fn push(&self, c: char) {
        let write = self.write_index.load(Ordering::Acquire);
        let read = self.read_index.load(Ordering::Acquire);
        
        let next_write = (write + 1) % BUFFER_SIZE;
        
        // Buffer lleno? Drop el carácter
        if next_write == read {
            return;
        }
        
        // SAFETY: Solo el handler escribe, y validamos que no overflow
        unsafe {
            let ptr = self.buffer.as_ptr() as *mut Option<char>;
            ptr.add(write).write(Some(c));
        }
        
        // Publicar la escritura
        self.write_index.store(next_write, Ordering::Release);
    }

    /// Lee un carácter (llamado desde main loop)
    pub fn pop(&self) -> Option<char> {
        let read = self.read_index.load(Ordering::Acquire);
        let write = self.write_index.load(Ordering::Acquire);
        
        // Buffer vacío?
        if read == write {
            return None;
        }
        
        // SAFETY: Solo el consumer lee, y validamos que hay datos
        let c = unsafe {
            let ptr = self.buffer.as_ptr() as *const Option<char>;
            ptr.add(read).read()
        };
        
        let next_read = (read + 1) % BUFFER_SIZE;
        self.read_index.store(next_read, Ordering::Release);
        
        c
    }
    
    /// Diagnóstico: cuántos elementos hay
    pub fn len(&self) -> usize {
        let write = self.write_index.load(Ordering::Relaxed);
        let read = self.read_index.load(Ordering::Relaxed);
        
        if write >= read {
            write - read
        } else {
            BUFFER_SIZE - read + write
        }
    }
}

// Global instance
pub static KEYBOARD_BUFFER: KeyboardBuffer = KeyboardBuffer::new();