// kernel/src/drivers/framebuffer_console.rs
//
// Framebuffer text console — write text to the screen.

use alloc::boxed::Box;
use crate::process::file::{FileHandle, FileError, FileResult};

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
            for line in text.lines() {
                fb.draw_text(
                    self.x,
                    self.y,
                    line,
                    Color::rgb(255, 255, 255),
                    Color::rgb(0, 0, 0),
                    1,
                );
                self.y += 10;

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

pub fn open() -> Box<dyn FileHandle> {
    Box::new(FramebufferConsole::new())
}