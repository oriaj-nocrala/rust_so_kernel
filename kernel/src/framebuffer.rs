use font8x8::legacy::BASIC_LEGACY;
use spin::Mutex;
use core::ptr::NonNull;

/// Glyph pixel size at `scale == 1`, derived from `BASIC_LEGACY` itself
/// (one bit per pixel column, one `u8` row per pixel row) instead of a
/// hand-typed guess living separately in the console driver — if the font
/// ever changes, this can't silently drift out of sync with `draw_char`.
pub const GLYPH_W: usize = u8::BITS as usize;
pub const GLYPH_H: usize = BASIC_LEGACY[0].len();

pub struct Framebuffer {
    buffer: NonNull<u8>,
    width: usize,
    height: usize,
    stride: usize,
    bytes_per_pixel: usize,
}

// SAFETY: El framebuffer es solo memoria de video, podemos compartirlo
unsafe impl Send for Framebuffer {}
unsafe impl Sync for Framebuffer {}

impl Framebuffer {
    pub fn new(
        buffer: &'static mut [u8],
        width: usize,
        height: usize,
        stride: usize,
        bytes_per_pixel: usize,
    ) -> Self {
        Self {
            buffer: NonNull::new(buffer.as_mut_ptr()).unwrap(),
            width,
            height,
            stride,
            bytes_per_pixel,
        }
    }

    /// Limpia toda la pantalla con el color especificado
    pub fn clear(&mut self, color: Color) {
        let buffer = unsafe {
            core::slice::from_raw_parts_mut(self.buffer.as_ptr(), self.height * self.stride * self.bytes_per_pixel)
        };

        for y in 0..self.height {
            for x in 0..self.width {
                self.draw_pixel(buffer, x, y, color);
            }
        }
    }

    fn draw_pixel(&self, buffer: &mut [u8], x: usize, y: usize, color: Color) {
        if x >= self.width || y >= self.height {
            return;
        }

        let offset = (y * self.stride + x) * self.bytes_per_pixel;
        if offset + self.bytes_per_pixel <= buffer.len() {
            buffer[offset] = color.b;
            buffer[offset + 1] = color.g;
            buffer[offset + 2] = color.r;
            // buffer[offset + 3] = 0xFF; // Alpha si es necesario.
        }
    }

    /// Dibuja un carácter en las coordenadas especificadas
    pub fn draw_char(
        &mut self,
        x: usize,
        y: usize,
        ascii: u8,
        fg_color: Color,
        bg_color: Color,
        scale: usize,
    ) {
        let buffer = unsafe {
            core::slice::from_raw_parts_mut(self.buffer.as_ptr(), self.height * self.stride * self.bytes_per_pixel)
        };

        let glyph: [u8; 8] = BASIC_LEGACY[ascii as usize];
        
        for (row, &bits) in glyph.iter().enumerate() {
            for col in 0..8 {
                let bit_set = (bits >> col) & 1 != 0;
                let color = if bit_set { fg_color } else { bg_color };
                
                // Dibuja el píxel escalado
                for sy in 0..scale {
                    for sx in 0..scale {
                        let px = x + col * scale + sx;
                        let py = y + row * scale + sy;
                        self.draw_pixel(buffer, px, py, color);
                    }
                }
            }
        }
    }

    /// Dibuja texto en las coordenadas especificadas
    pub fn draw_text(
        &mut self,
        x: usize,
        y: usize,
        text: &str,
        fg_color: Color,
        bg_color: Color,
        scale: usize,
    ) {
        let char_width = 8 * scale;
        
        for (i, &byte) in text.as_bytes().iter().enumerate() {
            let char_x = x + i * char_width;
            self.draw_char(char_x, y, byte, fg_color, bg_color, scale);
        }
    }

    /// Desplaza el contenido de la pantalla `line_height` píxeles hacia arriba.
    /// Las filas inferiores vacías se ponen a cero.
    pub fn scroll_up(&mut self, line_height: usize) {
        let row_bytes = self.stride * self.bytes_per_pixel;
        let total = self.height * row_bytes;
        let skip = line_height * row_bytes;
        if skip >= total { return; }
        let buffer = unsafe {
            core::slice::from_raw_parts_mut(self.buffer.as_ptr(), total)
        };
        buffer.copy_within(skip..total, 0);
        for byte in &mut buffer[(total - skip)..] { *byte = 0; }
    }

    /// Obtiene las dimensiones del framebuffer
    //1280 x 800 en qemu
    pub fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Blits a `0x00RRGGBB`-packed `src_w`x`src_h` buffer onto the real
    /// framebuffer, nearest-neighbor scaled up by the largest integer
    /// factor that still fits (never distorts aspect ratio) and centered
    /// (letterboxed) — used by raw-pixel userspace clients (e.g. a ported
    /// game) that draw into their own small offscreen buffer instead of
    /// going through the text console's char/ANSI layer.
    pub fn blit_scaled(&mut self, src: &[u32], src_w: usize, src_h: usize) {
        if src_w == 0 || src_h == 0 || src.len() < src_w * src_h {
            return;
        }
        let scale = core::cmp::max(1, core::cmp::min(self.width / src_w, self.height / src_h));
        let dst_w = src_w * scale;
        let dst_h = src_h * scale;
        let off_x = (self.width.saturating_sub(dst_w)) / 2;
        let off_y = (self.height.saturating_sub(dst_h)) / 2;

        let buffer = unsafe {
            core::slice::from_raw_parts_mut(self.buffer.as_ptr(), self.height * self.stride * self.bytes_per_pixel)
        };

        for sy in 0..src_h {
            let src_row = sy * src_w;
            for sx in 0..src_w {
                let p = src[src_row + sx];
                let r = ((p >> 16) & 0xFF) as u8;
                let g = ((p >> 8) & 0xFF) as u8;
                let b = (p & 0xFF) as u8;
                for oy in 0..scale {
                    let dy = off_y + sy * scale + oy;
                    let row_off = dy * self.stride * self.bytes_per_pixel;
                    for ox in 0..scale {
                        let dx = off_x + sx * scale + ox;
                        let offset = row_off + dx * self.bytes_per_pixel;
                        if offset + self.bytes_per_pixel <= buffer.len() {
                            buffer[offset] = b;
                            buffer[offset + 1] = g;
                            buffer[offset + 2] = r;
                        }
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

// Global framebuffer
pub static FRAMEBUFFER: Mutex<Option<Framebuffer>> = Mutex::new(None);

// Helper para inicializar
pub fn init_global_framebuffer(framebuffer: Framebuffer) {
    *FRAMEBUFFER.lock() = Some(framebuffer);
}