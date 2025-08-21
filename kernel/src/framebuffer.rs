use font8x8::legacy::BASIC_LEGACY;

pub struct Framebuffer<'a> {
    buffer: &'a mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    bytes_per_pixel: usize,
}

impl<'a> Framebuffer<'a> {
    pub fn new(
        buffer: &'a mut [u8],
        width: usize,
        height: usize,
        stride: usize,
        bytes_per_pixel: usize,
    ) -> Self {
        Self {
            buffer,
            width,
            height,
            stride,
            bytes_per_pixel,
        }
    }

    /// Limpia toda la pantalla con el color especificado
    pub fn clear(&mut self, color: [u8; 3]) {
        let total_pixels = self.stride * self.height;
        for i in 0..total_pixels {
            let idx = i * self.bytes_per_pixel;
            if idx + 3 < self.buffer.len() {
                self.buffer[idx + 0] = color[0]; // B
                self.buffer[idx + 1] = color[1]; // G
                self.buffer[idx + 2] = color[2]; // R
                self.buffer[idx + 3] = 0x00;     // A/reserved
            }
        }
    }

    /// Dibuja un píxel en las coordenadas especificadas
    pub fn draw_pixel(&mut self, x: usize, y: usize, color: [u8; 3]) {
        if x >= self.width || y >= self.height {
            return;
        }
        
        let idx = (y * self.stride + x) * self.bytes_per_pixel;
        if idx + 3 < self.buffer.len() {
            self.buffer[idx + 0] = color[0]; // B
            self.buffer[idx + 1] = color[1]; // G
            self.buffer[idx + 2] = color[2]; // R
            self.buffer[idx + 3] = 0x00;     // A/reserved
        }
    }

    /// Dibuja un carácter en las coordenadas especificadas
    pub fn draw_char(
        &mut self,
        x: usize,
        y: usize,
        ascii: u8,
        fg_color: [u8; 3],
        bg_color: [u8; 3],
        scale: usize,
    ) {
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
                        self.draw_pixel(px, py, color);
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
        fg_color: [u8; 3],
        bg_color: [u8; 3],
        scale: usize,
    ) {
        let char_width = 8 * scale;
        
        for (i, &byte) in text.as_bytes().iter().enumerate() {
            let char_x = x + i * char_width;
            self.draw_char(char_x, y, byte, fg_color, bg_color, scale);
        }
    }

    /// Obtiene las dimensiones del framebuffer
    pub fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }
}