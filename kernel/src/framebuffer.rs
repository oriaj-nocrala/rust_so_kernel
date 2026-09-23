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
    /// The VRAM mapping.
    buffer: NonNull<u8>,
    width: usize,
    height: usize,
    stride: usize,
    bytes_per_pixel: usize,
    /// RAM copy of the whole aperture, same layout (`stride` included),
    /// once `attach_shadow` has run. While it exists every primitive
    /// draws here and VRAM is only ever *written*, by `flush`.
    ///
    /// WHY: on the physical AM4 machine reading VRAM runs at ~4 MB/s, so
    /// one `scroll_up`, which reads the screen back, measured 2.18 s,
    /// against 391 MB/s for writes. See `docs/fb/wc-shadow-plan.md`.
    shadow: Option<NonNull<u8>>,
    /// What of `shadow` VRAM has not seen yet.
    dirty: hal::fbdirty::DirtyRect,
    /// Nesting depth of `begin_batch`. At 0 every primitive flushes its
    /// own rectangle before returning, so a caller that has never heard
    /// of the shadow still leaves the screen up to date.
    batch_depth: u32,
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
            shadow: None,
            dirty: hal::fbdirty::DirtyRect::new(width, height),
            batch_depth: 0,
        }
    }

    /// Switch to shadow mode, drawing into `shadow` from now on.
    ///
    /// `shadow` must be at least `byte_len()` bytes and all zero. VRAM's
    /// visible area is then cleared to match: black on both sides, with
    /// no VRAM read. Copying VRAM into the shadow instead would preserve
    /// whatever the firmware and bootloader left on screen, but that read
    /// alone is 2.2 s on the target machine, and `draw_boot_screen`
    /// clears the screen right after this anyway.
    ///
    /// Returns false, and changes nothing, if `shadow` is too small.
    pub fn attach_shadow(&mut self, shadow: &'static mut [u8]) -> bool {
        if shadow.len() < self.byte_len() || self.shadow.is_some() {
            return false;
        }
        self.shadow = NonNull::new(shadow.as_mut_ptr());
        self.dirty.mark_all();
        self.flush();
        true
    }

    pub fn has_shadow(&self) -> bool {
        self.shadow.is_some()
    }

    /// Where primitives draw: the shadow when there is one, else VRAM.
    fn draw_buffer(&mut self) -> &'static mut [u8] {
        let base = self.shadow.unwrap_or(self.buffer);
        // SAFETY: both the VRAM mapping and the shadow span `byte_len()`
        // bytes and live for the rest of the boot; `&mut self` is the
        // only way to reach either.
        unsafe { core::slice::from_raw_parts_mut(base.as_ptr(), self.byte_len()) }
    }

    /// Record that a primitive changed `[x, x+w) x [y, y+h)`, and outside
    /// a batch copy it to VRAM right away. Nothing to do without a shadow:
    /// the primitive already wrote VRAM directly.
    fn touched(&mut self, x: usize, y: usize, w: usize, h: usize) {
        if self.shadow.is_none() {
            // Direct mode: the primitive just wrote VRAM, which may be WC.
            sfence();
            return;
        }
        self.dirty.mark(x, y, w, h);
        if self.batch_depth == 0 {
            self.flush();
        }
    }

    /// Defer every primitive's VRAM copy until the matching `end_batch`.
    /// Nests. The console wraps each `write()` in one, so 400 lines that
    /// scroll 400 times cost 400 `memmove`s in RAM and one flush.
    pub fn begin_batch(&mut self) {
        self.batch_depth += 1;
    }

    /// Close a batch; the outermost one flushes.
    pub fn end_batch(&mut self) {
        self.batch_depth = self.batch_depth.saturating_sub(1);
        if self.batch_depth == 0 {
            self.flush();
        }
    }

    /// Copy the dirty rectangle from the shadow to VRAM, one contiguous
    /// span per scanline. Only the visible width is copied: `stride`
    /// padding is never written, same as every primitive. This is the
    /// only place VRAM is touched in shadow mode, so `fb_flush`'s MB/s in
    /// `/proc/fbinfo` is the real write bandwidth to the aperture.
    pub fn flush(&mut self) {
        let Some(shadow) = self.shadow else { return };
        let Some(r) = self.dirty.take() else { return };
        let t0 = crate::cpu::tsc::read();
        let bpp = self.bytes_per_pixel;
        let row_bytes = self.stride * bpp;
        let span = r.width() * bpp;
        for row in r.y0..r.y1 {
            let off = row * row_bytes + r.x0 * bpp;
            // SAFETY: `DirtyRect` clips to `width` x `height`, so
            // `off + span` stays inside `byte_len()` in both buffers,
            // and the shadow never overlaps the VRAM mapping.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    shadow.as_ptr().add(off),
                    self.buffer.as_ptr().add(off),
                    span,
                );
            }
        }
        // With the aperture WC the stores above may still sit in the
        // write-combining buffers; draining them here makes `fb_flush`
        // measure the data leaving the CPU, not just the stores retiring,
        // and puts the frame on screen before the caller moves on.
        sfence();
        crate::debug::FB_FLUSH.record(
            (r.height() * span) as u64,
            crate::cpu::tsc::read().wrapping_sub(t0),
        );
    }

    /// Limpia toda la pantalla con el color especificado
    pub fn clear(&mut self, color: Color) {
        self.fill_rect(0, 0, self.width, self.height, color);
    }

    /// Fills an axis-aligned rectangle, one contiguous byte range per
    /// scanline instead of one bounds-checked store per pixel.
    ///
    /// THE POINT OF THIS FUNCTION. On the physical machine this kernel is
    /// brought up on, the framebuffer sits behind PCIe and its mapping is
    /// (at time of writing — `/proc/fbinfo` reports what it actually is)
    /// uncacheable: every store is its own bus transaction, and the *size*
    /// of the store barely matters against that fixed cost. Clearing to
    /// the end of a 1920x1080 screen through `draw_char`-per-cell is ~1.4
    /// million individual 3-byte writes, which measured about a second —
    /// the whole reason backspace froze the console there while being
    /// imperceptible in QEMU, where the framebuffer is host RAM.
    ///
    /// Two paths, both writing whole spans:
    ///   * black (every byte of the pixel identical *and* zero) →
    ///     `slice::fill`, i.e. `memset`;
    ///   * any other colour → repeated `copy_from_slice` of a
    ///     pre-built pixel pattern.
    ///
    /// The all-zero restriction on the fast path is deliberate. A pixel is
    /// written `[b, g, r]` and the fourth byte at 32 bpp is padding this
    /// driver has always left alone; `memset`ting a non-zero grey would
    /// write that padding byte too. Black is the case that matters
    /// (`DEFAULT_BG`), so the other colours take the pattern path rather
    /// than the padding byte changing meaning.
    ///
    /// Clamped to the screen on all four sides, so a caller may pass a
    /// rectangle that runs off the edge — `ESC[J` on the bottom row does.
    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: Color) {
        let t0 = crate::cpu::tsc::read();

        let x0 = x.min(self.width);
        let x1 = x.saturating_add(w).min(self.width);
        let y0 = y.min(self.height);
        let y1 = y.saturating_add(h).min(self.height);
        if x0 >= x1 || y0 >= y1 {
            return;
        }

        let bpp = self.bytes_per_pixel;
        let row_bytes = self.stride * bpp;
        let span = (x1 - x0) * bpp;

        // Unusual pixel widths fall back to the original per-pixel path
        // rather than this function silently composing a wrong pattern.
        let buffer = self.draw_buffer();

        if bpp < 3 || bpp > 4 {
            for row in y0..y1 {
                for col in x0..x1 {
                    self.draw_pixel(buffer, col, row, color);
                }
            }
            self.touched(x0, y0, x1 - x0, y1 - y0);
            return;
        }

        if color.r == 0 && color.g == 0 && color.b == 0 {
            for row in y0..y1 {
                let start = row * row_bytes + x0 * bpp;
                buffer[start..start + span].fill(0);
            }
        } else {
            // 64 pixels is enough that the per-`copy_from_slice` call
            // overhead disappears against the copy itself, and small
            // enough to sit on the stack of a fault handler.
            const PATTERN_PX: usize = 64;
            let mut pattern = [0u8; PATTERN_PX * 4];
            for i in 0..PATTERN_PX {
                let o = i * bpp;
                pattern[o] = color.b;
                pattern[o + 1] = color.g;
                pattern[o + 2] = color.r;
            }
            let pattern = &pattern[..PATTERN_PX * bpp];

            for row in y0..y1 {
                let start = row * row_bytes + x0 * bpp;
                let dst = &mut buffer[start..start + span];
                let mut off = 0;
                while off < span {
                    // `span` and `pattern.len()` are both whole numbers of
                    // pixels, so every chunk — the short tail included —
                    // stays pixel-aligned.
                    let n = pattern.len().min(span - off);
                    dst[off..off + n].copy_from_slice(&pattern[..n]);
                    off += n;
                }
            }
        }

        crate::debug::FB_FILL_RECT.record(
            ((y1 - y0) * span) as u64,
            crate::cpu::tsc::read().wrapping_sub(t0),
        );
        self.touched(x0, y0, x1 - x0, y1 - y0);
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

    /// Invierte (XOR) los bytes de color de un rectángulo de píxeles.
    /// Auto-inverso: aplicarlo dos veces sobre la misma región restaura los
    /// píxeles originales sin necesidad de recordar qué había dibujado ahí
    /// — así es como el cursor parpadeante se dibuja/borra sin llevar un
    /// buffer de texto propio (este renderer es "immediate mode").
    /// Note the cost, measured by `FB_CURSOR` in `/proc/fbinfo`: this is
    /// the one console operation built on read-modify-write. A VRAM read
    /// is *non-posted* — the CPU stalls until the data comes back across
    /// the bus — where a write is posted and retires immediately. One
    /// cursor cell is only 8x8 pixels, so this stayed cheap enough to
    /// leave alone; the counter is here so that stops being an assumption.
    pub fn xor_rect(&mut self, x: usize, y: usize, w: usize, h: usize) {
        let t0 = crate::cpu::tsc::read();
        let buffer = self.draw_buffer();

        let mut touched = 0u64;
        for row in y..(y + h).min(self.height) {
            for col in x..(x + w).min(self.width) {
                let offset = (row * self.stride + col) * self.bytes_per_pixel;
                if offset + self.bytes_per_pixel <= buffer.len() {
                    buffer[offset] ^= 0xFF;
                    buffer[offset + 1] ^= 0xFF;
                    buffer[offset + 2] ^= 0xFF;
                    touched += 3;
                }
            }
        }

        crate::debug::FB_CURSOR.record(touched, crate::cpu::tsc::read().wrapping_sub(t0));
        self.touched(x, y, w, h);
    }

    /// Dibuja un carácter en las coordenadas especificadas
    /// Draw one antialiased glyph cell: `raster[row][col]` is coverage
    /// (0 = background, 255 = foreground), blended linearly between `bg`
    /// and `fg`. The cell is `cell_w` x `cell_h` pixels; anything the
    /// raster does not cover is background. Clipped to the screen.
    ///
    /// This is the console's text path (Noto Sans Mono, via
    /// `noto-sans-mono-bitmap`); `draw_char` stays for the 8x8 bitmap font
    /// the boot banner and the panic screen use. Same shape as
    /// `blit_scaled`: at opt-level 0 a plain aligned 32-bit store per
    /// pixel is what keeps this cheap, so that is the path for the real
    /// framebuffers (4 bytes per pixel, rows 4-aligned).
    pub fn draw_glyph(
        &mut self,
        x: usize,
        y: usize,
        cell_w: usize,
        cell_h: usize,
        raster: &[&[u8]],
        fg: Color,
        bg: Color,
    ) {
        if x >= self.width || y >= self.height || self.bytes_per_pixel < 3 {
            return;
        }
        let t0 = crate::cpu::tsc::read();
        let w = core::cmp::min(cell_w, self.width - x);
        let h = core::cmp::min(cell_h, self.height - y);
        let bpp = self.bytes_per_pixel;
        let row_bytes = self.stride * bpp;
        let fg_px = fg.packed();
        let bg_px = bg.packed();
        let buffer = self.draw_buffer();

        let mut ry = 0;
        while ry < h {
            let row = &mut buffer[(y + ry) * row_bytes + x * bpp..][..w * bpp];
            let cov: &[u8] = if ry < raster.len() { raster[ry] } else { &[] };
            let dst = row.as_mut_ptr();
            let aligned = bpp == 4 && dst as usize % 4 == 0;
            let mut rx = 0;
            while rx < w {
                let a = if rx < cov.len() { cov[rx] } else { 0 };
                let px = match a {
                    0 => bg_px,
                    255 => fg_px,
                    _ => blend(fg, bg, a),
                };
                // SAFETY: `rx < w`, so the pixel is inside `row`.
                unsafe {
                    if aligned {
                        *(dst as *mut u32).add(rx) = px;
                    } else {
                        core::ptr::copy_nonoverlapping(px.to_le_bytes().as_ptr(), dst.add(rx * bpp), 3);
                    }
                }
                rx += 1;
            }
            ry += 1;
        }

        crate::debug::FB_DRAW_CHAR.record(
            (w * h * bpp) as u64,
            crate::cpu::tsc::read().wrapping_sub(t0),
        );
        self.touched(x, y, w, h);
    }

    pub fn draw_char(
        &mut self,
        x: usize,
        y: usize,
        ascii: u8,
        fg_color: Color,
        bg_color: Color,
        scale: usize,
    ) {
        let buffer = self.draw_buffer();

        let glyph: [u8; 8] = BASIC_LEGACY[ascii as usize];

        // Fast path: compose one pixel row of the cell into a stack buffer
        // and write it as a single contiguous span, instead of 8 separate
        // bounds-checked stores. Same motivation as `fill_rect`: on an
        // uncacheable framebuffer the per-store bus transaction dominates,
        // so eight 32-byte spans beat sixty-four 3-byte ones.
        //
        // Conditions: a pixel width this composer understands, a scale the
        // stack buffer holds, and a cell entirely on screen (a glyph half
        // off the edge needs per-pixel clipping, and is rare enough not to
        // be worth a second clipped composer).
        const MAX_CELL_BYTES: usize = GLYPH_W * 8 * 4;
        let cell_w = GLYPH_W * scale;
        let cell_h = GLYPH_H * scale;
        if scale >= 1
            && scale <= 8
            && self.bytes_per_pixel >= 3
            && self.bytes_per_pixel <= 4
            && x + cell_w <= self.width
            && y + cell_h <= self.height
        {
            let t0 = crate::cpu::tsc::read();
            let bpp = self.bytes_per_pixel;
            let row_bytes = self.stride * bpp;
            let span = cell_w * bpp;
            let mut line = [0u8; MAX_CELL_BYTES];

            for (row, &bits) in glyph.iter().enumerate() {
                let mut o = 0usize;
                for col in 0..GLYPH_W {
                    let color = if (bits >> col) & 1 != 0 { fg_color } else { bg_color };
                    for _ in 0..scale {
                        line[o] = color.b;
                        line[o + 1] = color.g;
                        line[o + 2] = color.r;
                        o += bpp;
                    }
                }
                for sy in 0..scale {
                    let py = y + row * scale + sy;
                    let start = py * row_bytes + x * bpp;
                    buffer[start..start + span].copy_from_slice(&line[..span]);
                }
            }

            crate::debug::FB_DRAW_CHAR.record(
                (cell_h * span) as u64,
                crate::cpu::tsc::read().wrapping_sub(t0),
            );
            self.touched(x, y, cell_w, cell_h);
            return;
        }

        // Slow path — partially off-screen cells and exotic pixel formats.
        let t0 = crate::cpu::tsc::read();
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
        crate::debug::FB_DRAW_CHAR.record(0, crate::cpu::tsc::read().wrapping_sub(t0));
        self.touched(x, y, cell_w, cell_h);
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
        let buffer = self.draw_buffer();
        let t0 = crate::cpu::tsc::read();
        buffer.copy_within(skip..total, 0);
        // `fill`, not a per-byte loop: one `memset` instead of `skip`
        // separate stores, same reason as `fill_rect`.
        buffer[(total - skip)..].fill(0);
        // Bytes counted read-plus-written: a scroll reads back the entire
        // framebuffer, which is what makes it the most expensive thing the
        // console does on real hardware and why `/proc/fbinfo` reports it
        // separately from the fills.
        //
        // In shadow mode all of this is RAM, and what reaches VRAM is the
        // one write-only flush of the whole screen, now or at the end of
        // the batch.
        crate::debug::FB_SCROLL.record(
            ((total - skip) * 2 + skip) as u64,
            crate::cpu::tsc::read().wrapping_sub(t0),
        );
        if self.shadow.is_some() {
            self.dirty.mark_all();
            if self.batch_depth == 0 {
                self.flush();
            }
        }
    }

    /// Obtiene las dimensiones del framebuffer
    //1280 x 800 en qemu
    pub fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Pixels per scanline *as stored*, which is not necessarily `width` —
    /// firmware commonly pads rows. Reported by `/proc/fbinfo`: on a
    /// machine with no serial capture there was previously no way to find
    /// out any of this, and every cost here is proportional to it.
    pub fn stride(&self) -> usize {
        self.stride
    }

    pub fn bytes_per_pixel(&self) -> usize {
        self.bytes_per_pixel
    }

    /// Virtual address of the mapping, for `memory::memtype` to walk the
    /// page table with — the physical address and caching bits are the
    /// other half of what decides how fast any of this is.
    pub fn virt_addr(&self) -> u64 {
        self.buffer.as_ptr() as u64
    }

    /// Bytes the mapping spans (`height * stride * bytes_per_pixel`).
    pub fn byte_len(&self) -> usize {
        self.height * self.stride * self.bytes_per_pixel
    }

    /// Blits a `0x00RRGGBB`-packed `src_w`x`src_h` buffer onto the real
    /// framebuffer, nearest-neighbor scaled up by the largest integer
    /// factor that still fits (never distorts aspect ratio) and centered
    /// (letterboxed) — used by raw-pixel userspace clients (e.g. a ported
    /// game) that draw into their own small offscreen buffer instead of
    /// going through the text console's char/ANSI layer.
    pub fn blit_scaled(&mut self, src: &[u32], src_w: usize, src_h: usize) {
        // A pixel narrower than 3 bytes (16 bpp) has no B, G, R layout to
        // write; no firmware this kernel boots on hands one out.
        if src_w == 0 || src_h == 0 || src.len() < src_w * src_h || self.bytes_per_pixel < 3 {
            return;
        }
        let t0 = crate::cpu::tsc::read();
        let scale = core::cmp::max(1, core::cmp::min(self.width / src_w, self.height / src_h));
        let dst_w = src_w * scale;
        let dst_h = src_h * scale;
        let off_x = (self.width.saturating_sub(dst_w)) / 2;
        let off_y = (self.height.saturating_sub(dst_h)) / 2;

        // Clip to the visible area: a source larger than the screen
        // (scale forced to 1) is cut at `width`/`height`, never written
        // into `stride` padding or past the last scanline.
        let vis_w = core::cmp::min(dst_w, self.width.saturating_sub(off_x));
        let vis_h = core::cmp::min(dst_h, self.height.saturating_sub(off_y));
        let bpp = self.bytes_per_pixel;
        let row_bytes = self.stride * bpp;
        let span = vis_w * bpp;
        let buffer = self.draw_buffer();

        // One destination scanline per source row, then `scale - 1`
        // copies of it. The per-pixel version this replaced did
        // `scale * scale` bounds-checked 3-byte stores for every source
        // pixel: 96.7 M cycles (26 ms) per 320x200 frame on the Ryzen even
        // into RAM, 96% of `fbbench` C and most of DOOM's frame budget.
        // Here each destination pixel is written once, as one 32-bit
        // store, and the replicated rows are a `memcpy`.
        for sy in 0..src_h {
            let first = off_y + sy * scale;
            if first >= off_y + vis_h {
                break;
            }
            let row = &mut buffer[first * row_bytes + off_x * bpp..][..span];
            let src_row = &src[sy * src_w..(sy + 1) * src_w];
            // Plain `while` loops over a raw pointer: this kernel runs the
            // `dev` profile (opt-level 0), where an iterator chain costs a
            // few calls per pixel. The slice above already bounds-checked
            // the whole row once.
            let dst = row.as_mut_ptr();
            let mut i = 0; // destination pixel within the row
            let mut sx = 0;
            if bpp == 4 && dst as usize % 4 == 0 {
                // The real case: VRAM is page-aligned and the shadow's
                // `SHADOW_SKEW` is a multiple of 4. A plain aligned store;
                // `write_unaligned` at opt-level 0 is a call into
                // `copy_nonoverlapping` and its runtime UB checks per pixel.
                let d = dst as *mut u32;
                while sx < src_w && i < vis_w {
                    // Little-endian 0x00RRGGBB is exactly B, G, R, 0.
                    let px = src_row[sx] & 0x00FF_FFFF;
                    let end = core::cmp::min(i + scale, vis_w);
                    while i < end {
                        // SAFETY: `i < vis_w`, so the store is inside
                        // `row`, and `d` is 4-aligned (checked above).
                        unsafe { *d.add(i) = px };
                        i += 1;
                    }
                    sx += 1;
                }
            } else {
                while sx < src_w && i < vis_w {
                    let bytes = (src_row[sx] & 0x00FF_FFFF).to_le_bytes();
                    let end = core::cmp::min(i + scale, vis_w);
                    while i < end {
                        // SAFETY: `i < vis_w` and `bpp >= 3`, so the three
                        // bytes are inside `row`.
                        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.add(i * bpp), 3) };
                        i += 1;
                    }
                    sx += 1;
                }
            }
            let last = core::cmp::min(first + scale, off_y + vis_h);
            for dy in first + 1..last {
                buffer.copy_within(
                    first * row_bytes + off_x * bpp..first * row_bytes + off_x * bpp + span,
                    dy * row_bytes + off_x * bpp,
                );
            }
        }

        crate::debug::FB_BLIT.record(
            (vis_w * vis_h * self.bytes_per_pixel) as u64,
            crate::cpu::tsc::read().wrapping_sub(t0),
        );
        self.touched(off_x, off_y, vis_w, vis_h);
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

    /// The pixel as one little-endian `u32`: bytes B, G, R, 0 — the
    /// layout every primitive here writes.
    pub const fn packed(self) -> u32 {
        (self.r as u32) << 16 | (self.g as u32) << 8 | self.b as u32
    }
}

/// `bg` moved `a / 255` of the way towards `fg`, per channel, packed.
/// Linear in sRGB rather than in light: gamma-correct blending would need
/// a table per channel, and at a glyph's size the difference is a hair of
/// stroke weight.
fn blend(fg: Color, bg: Color, a: u8) -> u32 {
    let mix = |f: u8, b: u8| -> u32 {
        let (f, b, a) = (f as u32, b as u32, a as u32);
        (f * a + b * (255 - a) + 127) / 255
    };
    mix(fg.r, bg.r) << 16 | mix(fg.g, bg.g) << 8 | mix(fg.b, bg.b)
}

/// Drain the write-combining buffers. WC stores are weakly ordered and
/// may be held back until a buffer fills; everything that writes VRAM ends
/// with one of these. A no-op cost on a UC or WB mapping.
#[inline(always)]
fn sfence() {
    // SAFETY: `sfence` only orders stores; valid on every x86-64 CPU and
    // independent of CR0/CR4's SSE enables.
    unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)) };
}

// Global framebuffer
pub static FRAMEBUFFER: Mutex<Option<Framebuffer>> = Mutex::new(None);

// Helper para inicializar
pub fn init_global_framebuffer(framebuffer: Framebuffer) {
    *FRAMEBUFFER.lock() = Some(framebuffer);
}

/// How far into its allocation the shadow starts: 2.5 pages plus one
/// cache line, so shadow byte `i` and VRAM byte `i` never share a page
/// index or the low 12 address bits.
///
/// WHY, measured: the large-object allocator hands back a block aligned to
/// its own size (4 MiB in QEMU), and the aperture's mapping is aligned
/// too, so without a skew `flush`'s source and destination for the same
/// offset sat at the same position within their pages *and* at page
/// numbers equal modulo any power-of-two table. QEMU's software TLB is
/// exactly such a table (direct-mapped by page number), and every
/// `movsq` of the copy then evicted the other side's entry: `fb_flush`
/// ran at 248 MB/s, 17x slower per byte than a direct-mode `scroll_up`
/// doing a VRAM-to-VRAM `memmove` of the same size. With this skew:
/// 2 100 MB/s, same build, same `fbbench`. Real CPUs have the analogous
/// hazard in 4K aliasing (a load whose low 12 bits match an in-flight
/// store's is held back as if it depended on it), which is why the skew
/// is not a whole number of pages.
const SHADOW_SKEW: usize = 0x2840;

/// Give the global framebuffer its RAM shadow. Needs the heap, so it runs
/// after `memory::init_core`; the framebuffer itself is registered before
/// that and works without one.
///
/// Best-effort: if the allocation fails (8.6 MB at 1920x1080 with a
/// 2048-px stride), the console stays in direct mode, slow on real
/// hardware but correct. `alloc_zeroed` rather than `vec!` because a
/// failed `vec!` is a panic, not a `None`.
///
/// The allocation runs with `FRAMEBUFFER` *released*: a panic inside the
/// allocator would otherwise find the lock held and skip the panic screen
/// (`panic.rs` only `try_lock`s it), which is exactly what a fault-injected
/// oversized request showed while this was being written.
pub fn attach_shadow() -> bool {
    let Some(len) = FRAMEBUFFER.lock().as_ref().map(|fb| fb.byte_len()) else {
        return false;
    };
    let Ok(layout) = core::alloc::Layout::from_size_align(len + SHADOW_SKEW, 4096) else {
        return false;
    };
    // SAFETY: `layout` has a nonzero size (a framebuffer has pixels). The
    // allocation is never freed: the shadow lives as long as the screen.
    let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
    if ptr.is_null() {
        return false;
    }
    // SAFETY: `ptr` is a fresh, zeroed allocation of `len + SHADOW_SKEW`
    // bytes nobody else references, so the skewed `len` bytes are inside it.
    let shadow = unsafe { core::slice::from_raw_parts_mut(ptr.add(SHADOW_SKEW), len) };
    // Called once, at boot, before anything else can attach one: the
    // `false` arm (and the leak it would imply) is unreachable in practice.
    FRAMEBUFFER.lock().as_mut().is_some_and(|fb| fb.attach_shadow(shadow))
}
/// What `map_write_combining` did, for the boot log and `/proc/fbinfo`.
#[derive(Clone, Copy)]
pub enum WcStatus {
    Mapped(crate::memory::memtype::Retyped),
    /// PAT entry `PAT_WC_INDEX` is not WC (see `pat_program:`), so pointing
    /// the aperture at it would not make it WC.
    PatNotWc,
    Failed(crate::memory::memtype::RetypeError),
    NoFramebuffer,
}

impl core::fmt::Display for WcStatus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WcStatus::Mapped(r) => write!(
                f,
                "write-combining ({} x 4K + {} large pages, PAT index {} -> {})",
                r.pages_4k,
                r.pages_large,
                r.old_index,
                hal::memtype::PAT_WC_INDEX,
            ),
            WcStatus::PatNotWc => write!(f, "not WC: the PAT has no WC entry at index 1"),
            WcStatus::Failed(e) => write!(f, "not WC: {}", e),
            WcStatus::NoFramebuffer => write!(f, "not WC: no framebuffer"),
        }
    }
}

static WC_STATUS: spin::Once<WcStatus> = spin::Once::new();

/// The outcome of the boot-time `map_write_combining`, if it has run.
pub fn wc_status() -> Option<WcStatus> {
    WC_STATUS.get().copied()
}

/// Map the aperture write-combining: point its page-table leaves at PAT
/// entry `hal::memtype::PAT_WC_INDEX`, which `memtype::program_pat` made
/// WC. Phase 3 of `docs/fb/wc-shadow-plan.md`.
///
/// WHY: with the shadow attached, `flush` is the only VRAM writer and is
/// ~98% of the console's time on the Ryzen, writing UC at 412 MB/s — one
/// bus transaction per store. WC merges them into bursts. On that machine
/// no MTRR covers the aperture (default type UC), and MTRR UC + PAT WC is
/// WC; the bootloader's physical window does not map it, so there is no
/// second, differently-typed alias. In QEMU there is one (WB PAT, but UC
/// MTRR, so effectively UC), which the SDM tolerates.
///
/// Reads of a WC mapping are uncached and slow, which is why this only
/// pays off with the shadow: nothing reads VRAM then. Without a shadow it
/// is still applied — direct mode's writes get the same win, and it only
/// reads VRAM in `scroll_up`/`xor_rect`, which were UC reads already.
pub fn map_write_combining() -> WcStatus {
    let status = map_write_combining_inner();
    WC_STATUS.call_once(|| status);
    status
}

fn map_write_combining_inner() -> WcStatus {
    use crate::memory::memtype::PatProgram;
    match crate::memory::memtype::pat_program_status() {
        Some(PatProgram::Programmed { .. }) | Some(PatProgram::AlreadyWc { .. }) => {}
        _ => return WcStatus::PatNotWc,
    }
    let Some((virt, len)) = FRAMEBUFFER.lock().as_ref().map(|fb| (fb.virt_addr(), fb.byte_len()))
    else {
        return WcStatus::NoFramebuffer;
    };
    // Nothing draws while this runs (boot, one CPU, interrupts off inside),
    // so the lock need not be held across the retype.
    match crate::memory::memtype::set_pat_index_range(
        virt,
        len as u64,
        hal::memtype::PAT_WC_INDEX,
    ) {
        Ok(r) => WcStatus::Mapped(r),
        Err(e) => WcStatus::Failed(e),
    }
}
