use core::{cell::UnsafeCell, sync::atomic::{AtomicUsize, Ordering}};

pub static KEYBOARD_BUFFER: KeyboardBuffer = KeyboardBuffer::new();

pub struct KeyboardBuffer {
    buffer: UnsafeCell<[Option<char>; 32]>,  // ✅ Explícito sobre interior mutability
    read: AtomicUsize,
    write: AtomicUsize,
}

unsafe impl Sync for KeyboardBuffer {}  // ✅ Documenta que es thread-safe bajo SPSC

impl KeyboardBuffer {
    pub const fn new() -> Self {
        Self {
            buffer: UnsafeCell::new([None; 32]),
            read: AtomicUsize::new(0),
            write: AtomicUsize::new(0),
        }
    }
    
    pub fn push(&self, c: char) {
        let write = self.write.load(Ordering::Acquire);
        let next_write = (write + 1) % 32;
        
        if next_write != self.read.load(Ordering::Acquire) {
            unsafe {
                let buf = &mut *self.buffer.get();
                buf[write] = Some(c);
            }
            self.write.store(next_write, Ordering::Release);
        }
    }
    
    pub fn pop(&self) -> Option<char> {
        let read = self.read.load(Ordering::Acquire);
        let write = self.write.load(Ordering::Acquire);
        
        if read == write {
            return None;
        }
        
        unsafe {
            let buf = &*self.buffer.get();
            let c = buf[read];
            self.read.store((read + 1) % 32, Ordering::Release);
            c
        }
    }
}