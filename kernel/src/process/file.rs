// kernel/src/process/file.rs
// VFS básico: File Descriptors y trait FileHandle

use alloc::boxed::Box;
use core::fmt;

// ============================================================================
// ERRORES
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileError {
    BadFileDescriptor,
    InvalidArgument,
    IOError,
    NotSupported,
    EndOfFile,
}

pub type FileResult<T> = Result<T, FileError>;

// ============================================================================
// TRAIT: FileHandle
// ============================================================================

/// Trait que representa cualquier "archivo" en el sistema
/// 
/// Puede ser:
/// - Un archivo real en disco
/// - Un dispositivo (teclado, pantalla)
/// - Un pipe
/// - Un socket
/// - /dev/null, /dev/zero, etc.
pub trait FileHandle: Send {
    /// Lee hasta `buf.len()` bytes
    /// Retorna el número de bytes leídos
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize>;
    
    /// Escribe hasta `buf.len()` bytes
    /// Retorna el número de bytes escritos
    fn write(&mut self, buf: &[u8]) -> FileResult<usize>;
    
    /// Cierra el archivo (opcional, por defecto no hace nada)
    fn close(&mut self) -> FileResult<()> {
        Ok(())
    }
    
    /// Nombre para debugging
    fn name(&self) -> &str {
        "<unknown>"
    }
}

// ============================================================================
// IMPLEMENTACIONES BÁSICAS
// ============================================================================

/// Serial Console (COM1) - para stdout/stderr
pub struct SerialConsole;

impl FileHandle for SerialConsole {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        // TODO: Implementar lectura del serial
        Err(FileError::NotSupported)
    }
    
    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        use x86_64::instructions::port::Port;
        
        unsafe {
            let mut port = Port::<u8>::new(0x3F8);
            for &byte in buf {
                port.write(byte);
            }
        }
        
        Ok(buf.len())
    }
    
    fn name(&self) -> &str {
        "serial"
    }
}

/// Framebuffer Console - escritura en pantalla
pub struct FramebufferConsole {
    x: usize,
    y: usize,
}

impl FramebufferConsole {
    pub fn new() -> Self {
        Self { x: 10, y: 100 }
    }
}

impl FileHandle for FramebufferConsole {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Err(FileError::NotSupported)
    }
    
    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        use crate::framebuffer::{FRAMEBUFFER, Color};
        
        let text = core::str::from_utf8(buf)
            .map_err(|_| FileError::InvalidArgument)?;
        
        let mut fb = FRAMEBUFFER.lock();
        if let Some(fb) = fb.as_mut() {
            // Dibujar el texto
            for line in text.lines() {
                fb.draw_text(
                    self.x, 
                    self.y, 
                    line,
                    Color::rgb(255, 255, 255),
                    Color::rgb(0, 0, 0),
                    1
                );
                self.y += 10;
                
                // Simple scroll si llegamos al final
                let (_, height) = fb.dimensions();
                if self.y + 10 > height {
                    self.y = 100;
                    fb.clear(Color::rgb(0, 0, 0));
                }
            }
        }
        
        Ok(buf.len())
    }
    
    fn name(&self) -> &str {
        "fb"
    }
}

/// /dev/null - descarta todo lo que se escribe
pub struct DevNull;

impl FileHandle for DevNull {
    fn read(&mut self, _buf: &mut [u8]) -> FileResult<usize> {
        Ok(0) // EOF inmediato
    }
    
    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        Ok(buf.len()) // Pretende escribir todo
    }
    
    fn name(&self) -> &str {
        "/dev/null"
    }
}

/// /dev/zero - retorna ceros infinitos
pub struct DevZero;

impl FileHandle for DevZero {
    fn read(&mut self, buf: &mut [u8]) -> FileResult<usize> {
        for byte in buf.iter_mut() {
            *byte = 0;
        }
        Ok(buf.len())
    }
    
    fn write(&mut self, buf: &[u8]) -> FileResult<usize> {
        Ok(buf.len())
    }
    
    fn name(&self) -> &str {
        "/dev/zero"
    }
}

// ============================================================================
// TABLA DE FILE DESCRIPTORS
// ============================================================================

const MAX_FILES: usize = 16;

/// Tabla de archivos abiertos por un proceso
pub struct FileDescriptorTable {
    files: [Option<Box<dyn FileHandle>>; MAX_FILES],
}

impl FileDescriptorTable {
    /// Crea una tabla vacía
    pub const fn new() -> Self {
        const NONE: Option<Box<dyn FileHandle>> = None;
        Self {
            files: [NONE; MAX_FILES],
        }
    }
    
    /// Crea una tabla con stdin/stdout/stderr por defecto
    pub fn new_with_stdio() -> Self {
        let mut table = Self::new();
        
        // FD 0: stdin (de momento, /dev/null)
        table.files[0] = Some(Box::new(DevNull));
        
        // FD 1: stdout (serial)
        table.files[1] = Some(Box::new(SerialConsole));
        
        // FD 2: stderr (serial también)
        table.files[2] = Some(Box::new(SerialConsole));
        
        table
    }
    
    /// Obtiene un file handle mutable
    pub fn get_mut(&mut self, fd: usize) -> FileResult<&mut (dyn FileHandle + '_)> {
        if fd >= MAX_FILES {
            return Err(FileError::BadFileDescriptor);
        }
        
        if let Some(ref mut boxed) = self.files[fd] {
            Ok(&mut **boxed)
        } else {
            Err(FileError::BadFileDescriptor)
        }
    }
    
    /// Obtiene un file handle inmutable
    pub fn get(&self, fd: usize) -> FileResult<&(dyn FileHandle + '_)> {
        if fd >= MAX_FILES {
            return Err(FileError::BadFileDescriptor);
        }
        
        self.files[fd]
            .as_ref()
            .map(|boxed| &**boxed)
            .ok_or(FileError::BadFileDescriptor)
    }
    
    /// Asigna un nuevo file handle al primer FD disponible
    /// Retorna el FD asignado
    pub fn allocate(&mut self, handle: Box<dyn FileHandle>) -> FileResult<usize> {
        for (i, slot) in self.files.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(handle);
                return Ok(i);
            }
        }
        
        Err(FileError::InvalidArgument) // Too many files open
    }
    
    /// Cierra un file descriptor
    pub fn close(&mut self, fd: usize) -> FileResult<()> {
        if fd >= MAX_FILES {
            return Err(FileError::BadFileDescriptor);
        }
        
        if let Some(mut handle) = self.files[fd].take() {
            handle.close()?;
        }
        
        Ok(())
    }
    
    /// Debug: lista todos los archivos abiertos
    pub fn debug_list(&self) {
        crate::serial_println!("Open file descriptors:");
        for (i, slot) in self.files.iter().enumerate() {
            if let Some(handle) = slot {
                crate::serial_println!("  FD {}: {}", i, handle.name());
            }
        }
    }
}

// No se puede derivar Clone para arrays con trait objects
// Implementamos manualmente
impl Clone for FileDescriptorTable {
    fn clone(&self) -> Self {
        let mut new_table = Self::new();
        
        // Por ahora, no clonamos los file handles reales
        // En un fork() real, tendrías que duplicar cada handle
        // De momento, solo copiamos stdin/stdout/stderr
        
        if self.files[0].is_some() {
            new_table.files[0] = Some(Box::new(DevNull));
        }
        if self.files[1].is_some() {
            new_table.files[1] = Some(Box::new(SerialConsole));
        }
        if self.files[2].is_some() {
            new_table.files[2] = Some(Box::new(SerialConsole));
        }
        
        new_table
    }
}