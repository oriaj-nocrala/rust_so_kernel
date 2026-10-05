//! Looks for the GPU draw list (step 2 of `docs/gui/compositor-visual-plan.md`): what [`Compositor::draw_list`] makes of a window's
//! decorations and of the desktop, as data. The geometry does not change with the look (the title bar is [`TITLE_H`] × scale tall in every
//! theme, the buttons are where they always were), so a theme can be switched at any moment and nothing but pixels moves.
//!
//! [`FLAT`] is the look of [`Compositor::compose`], the CPU painter, which knows no other: under it the draw list is the same picture
//! `compose` paints (a test checks it pixel for pixel). The others are made of [`Shape`]s, which only a GPU host draws (`CR_SHAPE` in
//! `probes/nvk/comp_api.h`). All sizes here are at scale 1; the compositor multiplies them by its scale.
//!
//! [`Compositor::draw_list`]: crate::compositor::Compositor::draw_list
//! [`Compositor::compose`]: crate::compositor::Compositor::compose
//! [`TITLE_H`]: crate::compositor::TITLE_H

use crate::region::Rect;

/// A rounded box with a gradient, a border and a drop shadow, computed per pixel by the host (`struct cr_shape`). Colours are `0xAARRGGBB`
/// with straight alpha (`0xFF` opaque).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Shape {
    pub radius: f32,
    /// Inside the box; 0 = none.
    pub border: f32,
    /// `c[0] -> c[1]` over `[0, split]` of the height (or width), `c[2] -> c[3]` over `[split, 1]`: a step at `split` is the glossy look.
    pub split: f32,
    pub shadow_blur: f32,
    pub c: [u32; 4],
    pub border_color: u32,
    /// Alpha 0: no shadow.
    pub shadow_color: u32,
    pub shadow_dx: i32,
    pub shadow_dy: i32,
    /// The gradient runs left to right.
    pub horizontal: bool,
}

impl Shape {
    pub const fn solid(color: u32) -> Shape {
        Shape::gradient(color, color, color, color, 1.0)
    }

    /// Two segments, `a -> b` then `c -> d`, the step at `split`, top to bottom.
    pub const fn gradient(a: u32, b: u32, c: u32, d: u32, split: f32) -> Shape {
        Shape {
            radius: 0.0,
            border: 0.0,
            split,
            shadow_blur: 0.0,
            c: [a, b, c, d],
            border_color: 0,
            shadow_color: 0,
            shadow_dx: 0,
            shadow_dy: 0,
            horizontal: false,
        }
    }

    pub const fn radius(mut self, r: f32) -> Shape {
        self.radius = r;
        self
    }

    pub const fn border(mut self, width: f32, color: u32) -> Shape {
        self.border = width;
        self.border_color = color;
        self
    }

    pub const fn shadow(mut self, blur: f32, dx: i32, dy: i32, color: u32) -> Shape {
        self.shadow_blur = blur;
        self.shadow_dx = dx;
        self.shadow_dy = dy;
        self.shadow_color = color;
        self
    }

    pub const fn horizontal(mut self) -> Shape {
        self.horizontal = true;
        self
    }

    /// Lengths multiplied by `s` (the compositor's scale); colours and the split unchanged.
    pub fn scaled(&self, s: i32) -> Shape {
        let f = s as f32;
        Shape {
            radius: self.radius * f,
            border: self.border * f,
            shadow_blur: self.shadow_blur * f,
            shadow_dx: self.shadow_dx * s,
            shadow_dy: self.shadow_dy * s,
            ..*self
        }
    }
}

impl Shape {
    /// The shape with its box at `rect`, "over" the premultiplied `0xAARRGGBB` pixels `px` (rows `stride` long), touching only `clip`
    /// (which must lie inside `px`): the software twin of `comp.frag`'s `shape()`, the same maths per pixel, for a client that draws a
    /// theme's pieces itself (the panel) and for `compose`.
    pub fn paint(&self, px: &mut [u32], stride: usize, clip: Rect, rect: Rect) {
        if rect.w <= 0 || rect.h <= 0 {
            return;
        }
        let reach = if self.shadow_color >> 24 != 0 {
            self.shadow_blur as i32 + 1 + self.shadow_dx.abs().max(self.shadow_dy.abs())
        } else {
            0
        };
        let area = Rect::new(rect.x - reach, rect.y - reach, rect.w + 2 * reach, rect.h + 2 * reach);
        let Some(a) = area.intersect(&clip) else { return };
        for y in a.y..a.bottom() {
            for x in a.x..a.right() {
                let c = self.pixel(rect, x, y);
                if c[3] > 0.0 {
                    let d = &mut px[y as usize * stride + x as usize];
                    *d = over_f(*d, c);
                }
            }
        }
    }

    /// This shape's premultiplied colour (r, g, b, a in 0..=1) at the pixel (`x`, `y`) for a box at `rect`.
    pub fn pixel(&self, rect: Rect, x: i32, y: i32) -> [f32; 4] {
        let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
        let (bx, by) = (rect.w as f32 * 0.5, rect.h as f32 * 0.5);
        let (cx, cy) = (rect.x as f32 + bx, rect.y as f32 + by);
        let r = self.radius.min(bx.min(by));
        let d = sd_round_box(px - cx, py - cy, bx, by, r);
        let cover = clamp(0.5 - d, 0.0, 1.0);
        let t = if self.horizontal { (px - rect.x as f32) / rect.w as f32 } else { (py - rect.y as f32) / rect.h as f32 };
        let t = clamp(t, 0.0, 1.0);
        let c = self.c.map(unpack);
        let mut fill = if t < self.split {
            mix(c[0], c[1], t / self.split)
        } else {
            mix(c[2], c[3], if self.split < 1.0 { (t - self.split) / (1.0 - self.split) } else { 1.0 })
        };
        if self.border > 0.0 {
            fill = mix(unpack(self.border_color), fill, clamp(0.5 - (d + self.border), 0.0, 1.0));
        }
        let mut o = fill.map(|v| v * cover);
        let sc = unpack(self.shadow_color);
        if sc[3] > 0.0 {
            let ds = sd_round_box(px - cx - self.shadow_dx as f32, py - cy - self.shadow_dy as f32, bx, by, r);
            let k = if self.shadow_blur > 0.0 { 1.0 - smoothstep(-self.shadow_blur, self.shadow_blur, ds) } else { clamp(0.5 - ds, 0.0, 1.0) };
            for i in 0..4 {
                o[i] += sc[i] * (k * (1.0 - cover));
            }
        }
        o
    }
}

fn clamp(x: f32, lo: f32, hi: f32) -> f32 {
    if x < lo {
        lo
    } else if x > hi {
        hi
    } else {
        x
    }
}

/// `core` has no `sqrt` without `std`: the bit trick, then Newton (three steps: within an ulp or two for what this crate measures).
fn sqrt(x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    let mut y = f32::from_bits((x.to_bits() >> 1) + 0x1FBD_1DF5);
    for _ in 0..3 {
        y = 0.5 * (y + x / y);
    }
    y
}

fn sd_round_box(px: f32, py: f32, bx: f32, by: f32, r: f32) -> f32 {
    let (qx, qy) = (px.abs() - bx + r, py.abs() - by + r);
    let (mx, my) = (qx.max(0.0), qy.max(0.0));
    sqrt(mx * mx + my * my) + qx.max(qy).min(0.0) - r
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = clamp((x - e0) / (e1 - e0), 0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// `0xAARRGGBB` straight -> premultiplied, 0..=1.
fn unpack(v: u32) -> [f32; 4] {
    let a = (v >> 24) as f32 / 255.0;
    [((v >> 16) & 255) as f32 / 255.0 * a, ((v >> 8) & 255) as f32 / 255.0 * a, (v & 255) as f32 / 255.0 * a, a]
}

fn mix(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    [0, 1, 2, 3].map(|i| a[i] * (1.0 - t) + b[i] * t)
}

/// Premultiplied `c` over the premultiplied pixel `d`, rounded to 8 bits.
fn over_f(d: u32, c: [f32; 4]) -> u32 {
    let k = 1.0 - c[3];
    let ch = |sh: u32, v: f32| -> u32 { (clamp(v + ((d >> sh) & 255) as f32 / 255.0 * k, 0.0, 1.0) * 255.0 + 0.5) as u32 };
    ch(24, c[3]) << 24 | ch(16, c[0]) << 16 | ch(8, c[1]) << 8 | ch(0, c[2])
}

/// Premultiplied `src` "over" `dst` (both `0xAARRGGBB`, integer, exact `/255` rounding): what [`Compositor::compose`] does with a window
/// whose buffer is `ARGB8888`. The result's top byte is the combined alpha.
///
/// [`Compositor::compose`]: crate::compositor::Compositor::compose
pub fn over(dst: u32, src: u32) -> u32 {
    let k = 255 - (src >> 24);
    let mut out = 0;
    for sh in [24, 16, 8, 0] {
        let v = ((src >> sh) & 255) + (((dst >> sh) & 255) * k + 127) / 255;
        out |= v.min(255) << sh;
    }
    out
}

/// How a title-bar button is drawn, inside its square (the hit box) less [`Theme::button_inset`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Button {
    /// Only the glyph; pressed, the square is filled (the flat look: [`FLAT`]).
    Flat { pressed: u32 },
    /// A shape, another one while pressed.
    Shape { normal: Shape, pressed: Shape },
    /// Windows 9x: a face with a light top-left edge and a dark bottom-right one, swapped while pressed. Opaque fills.
    Bevel { face: u32, light: u32, dark: u32 },
}

impl Button {
    /// The button at `rect`, pressed or not, into the premultiplied pixels `px` (rows `stride` long), touching only `clip`.
    /// [`Button::Flat`] draws only when pressed. `scale`: the shapes' scale and the bevel's line width.
    pub fn paint(&self, down: bool, px: &mut [u32], stride: usize, clip: Rect, rect: Rect, scale: i32) {
        let mut fill = |r: Rect, c: u32| {
            let Some(r) = r.intersect(&clip) else { return };
            for y in r.y..r.bottom() {
                for x in r.x..r.right() {
                    px[y as usize * stride + x as usize] = 0xFF00_0000 | c;
                }
            }
        };
        match *self {
            Button::Flat { pressed } => {
                if down {
                    fill(rect, pressed)
                }
            }
            Button::Shape { normal, pressed } => (if down { pressed } else { normal }).scaled(scale).paint(px, stride, clip, rect),
            Button::Bevel { face, light, dark } => {
                let (tl, br) = if down { (dark, light) } else { (light, dark) };
                let s = scale;
                if rect.w <= 2 * s || rect.h <= 2 * s {
                    return;
                }
                fill(rect, br);
                fill(Rect::new(rect.x, rect.y, rect.w - s, rect.h - s), tl);
                fill(Rect::new(rect.x + s, rect.y + s, rect.w - 2 * s, rect.h - 2 * s), face);
            }
        }
    }
}

/// The taskbar's look: the strip is drawn by the compositor under the panel's surface (so a later step can blur what is behind it); the
/// panel, told the theme's name (`theme` event), draws its buttons with the rest, into a premultiplied `ARGB8888` buffer that is
/// transparent elsewhere. Lengths at scale 1.
#[derive(Debug, PartialEq)]
pub struct Taskbar {
    /// The whole strip.
    pub bar: Shape,
    /// The launcher ("Apps"): drawn down while its list is open. Its box reaches `start_bleed` past the strip's left edge, so a rounded
    /// shape shows its right corners only.
    pub start: Button,
    pub start_bleed: i32,
    /// Its label, `0x00RRGGBB`, and a shadow under it (`0xAARRGGBB`, alpha 0 = none).
    pub start_fg: u32,
    pub start_shadow: u32,
    /// A window's button: down when it is the focused window.
    pub task: Button,
    /// Laid over a window's button under the pointer (alpha 0 = no hover look).
    pub hover: Shape,
    pub task_fg: u32,
    /// The clock's area at the right end, reaching past the strip's right edge by its radius; drawn down.
    pub tray: Button,
    pub tray_fg: u32,
}

/// One look. See the module's documentation.
#[derive(Debug, PartialEq)]
pub struct Theme {
    pub name: &'static str,
    /// The desktop, under everything: `None` is a flat [`crate::compositor::BACKGROUND`].
    pub background: Option<Shape>,
    /// A frame drawn under the whole window, `frame_w` wider than it on each side (bottom too); its shadow is the window's. Focused, unfocused.
    pub frame: Option<[Shape; 2]>,
    pub frame_w: i32,
    /// The title bar, focused and unfocused. `None`: [`FLAT`]'s fills. Its radius rounds the top corners only (the box reaches under the
    /// content, which covers the bottom ones).
    pub title: Option<[Shape; 2]>,
    /// The title's text, `0x00RRGGBB`, focused and unfocused.
    pub title_fg: [u32; 2],
    /// A shadow under the text, one pixel (× scale) down and right, `0xAARRGGBB`; alpha 0 = none.
    pub title_shadow: u32,
    pub close: Button,
    /// Maximize (and any other button).
    pub other: Button,
    pub button_inset: i32,
    /// The glyphs' colour (`0x00RRGGBB`) and stroke (× scale).
    pub glyph: u32,
    pub glyph_weight: i32,
    /// `None`: the panel draws its own flat look, opaque.
    pub taskbar: Option<Taskbar>,
}

/// The look `compose` paints: flat fills, no shapes.
pub static FLAT: Theme = Theme {
    name: "flat",
    background: None,
    frame: None,
    frame_w: 0,
    title: None,
    title_fg: [0x00F0_F0F0, 0x00B0_B0B8],
    title_shadow: 0,
    close: Button::Flat { pressed: crate::compositor::CLOSE_PRESSED },
    other: Button::Flat { pressed: crate::compositor::TITLE_UNFOCUSED },
    button_inset: 0,
    glyph: crate::compositor::BUTTON_FG,
    glyph_weight: 1,
    taskbar: None,
};

/// "Luna 2026": Windows XP's Luna redone with shaders: glossy blue title bars with rounded tops, a blue frame, soft shadows, a red close
/// button, a sky-and-hill desktop.
pub static LUNA: Theme = Theme {
    name: "luna",
    background: Some(Shape::gradient(0xFF2F_6FD0, 0xFF9C_CBF5, 0xFF5F_A835, 0xFF2D_6A19, 0.62)),
    frame: Some([
        Shape::solid(0xFF00_55E5).radius(8.0).border(1.0, 0xFF00_2D9A).shadow(14.0, 0, 6, 0x7000_0000),
        Shape::solid(0xFF7A_96DF).radius(8.0).border(1.0, 0xFF5A_76C0).shadow(10.0, 0, 4, 0x4000_0000),
    ]),
    frame_w: 3,
    title: Some([
        Shape::gradient(0xFF5C_A2FF, 0xFF2B_7CF2, 0xFF0A_5BDB, 0xFF0C_52CF, 0.45).radius(8.0).border(1.0, 0xFF00_2D9A),
        Shape::gradient(0xFFAE_C8F6, 0xFF97_B6EE, 0xFF83_A5E4, 0xFF8C_ACE7, 0.45).radius(8.0).border(1.0, 0xFF5A_76C0),
    ]),
    title_fg: [0x00FF_FFFF, 0x00E4_ECFA],
    title_shadow: 0x900A_1E5A,
    close: Button::Shape {
        normal: Shape::gradient(0xFFF2_A48E, 0xFFE2_5A3C, 0xFFC9_351D, 0xFFDA_552F, 0.45).radius(3.0).border(1.0, 0xE0FF_FFFF),
        pressed: Shape::gradient(0xFFB0_3A20, 0xFFA0_2C14, 0xFF90_2410, 0xFFA0_2C14, 0.45).radius(3.0).border(1.0, 0xC0FF_FFFF),
    },
    other: Button::Shape {
        normal: Shape::gradient(0xFF82_B8FF, 0xFF3E_88F3, 0xFF1F_66E0, 0xFF3A_80EA, 0.45).radius(3.0).border(1.0, 0xE0FF_FFFF),
        pressed: Shape::gradient(0xFF20_58C0, 0xFF18_4CB0, 0xFF10_40A0, 0xFF18_4CB0, 0.45).radius(3.0).border(1.0, 0xC0FF_FFFF),
    },
    button_inset: 2,
    glyph: 0x00FF_FFFF,
    glyph_weight: 2,
    taskbar: Some(Taskbar {
        bar: Shape::gradient(0xFF6A_A8F7, 0xFF31_6FDE, 0xFF26_5FD9, 0xFF1B_47B4, 0.14),
        start: Button::Shape {
            normal: Shape::gradient(0xFF79_C96A, 0xFF46_A33A, 0xFF31_8C28, 0xFF3D_9C33, 0.45).radius(10.0).border(1.0, 0xFF1F_6A18),
            pressed: Shape::gradient(0xFF2F_7E26, 0xFF2A_7422, 0xFF25_6A1E, 0xFF2A_7422, 0.45).radius(10.0).border(1.0, 0xFF1A_5A14),
        },
        start_bleed: 12,
        start_fg: 0x00FF_FFFF,
        start_shadow: 0xA010_3010,
        task: Button::Shape {
            normal: Shape::gradient(0xFF5C_9DF7, 0xFF3A_80EE, 0xFF2F_72E4, 0xFF3A_7DEB, 0.45).radius(3.0).border(1.0, 0x9018_3E9A),
            pressed: Shape::gradient(0xFF1D_4FB6, 0xFF1F_55BE, 0xFF22_5BC6, 0xFF26_62CE, 0.5).radius(3.0).border(1.0, 0xC010_3070),
        },
        hover: Shape::solid(0x30FF_FFFF).radius(3.0),
        task_fg: 0x00FF_FFFF,
        tray: Button::Shape {
            normal: Shape::gradient(0xFF2E_A8F5, 0xFF16_92E9, 0xFF10_86DF, 0xFF13_8CE4, 0.5).radius(4.0).border(1.0, 0xFF0B_4FAE),
            pressed: Shape::gradient(0xFF2E_A8F5, 0xFF16_92E9, 0xFF10_86DF, 0xFF13_8CE4, 0.5).radius(4.0).border(1.0, 0xFF0B_4FAE),
        },
        tray_fg: 0x00FF_FFFF,
    }),
};

/// "9x moderno": Windows 98's layout (grey bevelled frame and buttons, a navy-to-blue title running left to right, a teal desktop) with
/// modern light: soft shadows and a sheen on the desktop.
pub static NINES: Theme = Theme {
    name: "9x",
    background: Some(Shape::gradient(0xFF10_9494, 0xFF00_8080, 0xFF00_8080, 0xFF00_6868, 0.5)),
    frame: Some([
        Shape::solid(0xFFD4_D0C8).border(1.0, 0xFF40_4040).shadow(10.0, 3, 5, 0x6000_0000),
        Shape::solid(0xFFD4_D0C8).border(1.0, 0xFF80_8080).shadow(8.0, 2, 3, 0x4000_0000),
    ]),
    frame_w: 3,
    title: Some([
        Shape::gradient(0xFF0A_246A, 0xFFA6_CAF0, 0, 0, 1.0).horizontal(),
        Shape::gradient(0xFF80_8080, 0xFFC0_C0C0, 0, 0, 1.0).horizontal(),
    ]),
    title_fg: [0x00FF_FFFF, 0x00D4_D0C8],
    title_shadow: 0,
    close: Button::Bevel { face: 0x00D4_D0C8, light: 0x00FF_FFFF, dark: 0x0040_4040 },
    other: Button::Bevel { face: 0x00D4_D0C8, light: 0x00FF_FFFF, dark: 0x0040_4040 },
    button_inset: 2,
    glyph: 0x0000_0000,
    glyph_weight: 2,
    taskbar: Some(Taskbar {
        // a white line along the top, then the grey face
        bar: Shape::gradient(0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFD4_D0C8, 0xFFC8_C4BC, 0.05),
        start: Button::Bevel { face: 0x00D4_D0C8, light: 0x00FF_FFFF, dark: 0x0040_4040 },
        start_bleed: 0,
        start_fg: 0x0000_0000,
        start_shadow: 0,
        task: Button::Bevel { face: 0x00D4_D0C8, light: 0x00FF_FFFF, dark: 0x0040_4040 },
        hover: Shape::solid(0),
        task_fg: 0x0000_0000,
        tray: Button::Bevel { face: 0x00D4_D0C8, light: 0x00FF_FFFF, dark: 0x0080_8080 },
        tray_fg: 0x0000_0000,
    }),
};

/// Every theme, in the order the theme key cycles through them.
pub static THEMES: [&Theme; 3] = [&FLAT, &LUNA, &NINES];

/// The theme called `name`, if there is one.
pub fn by_name(name: &str) -> Option<&'static Theme> {
    THEMES.iter().copied().find(|t| t.name == name)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec;

    use super::*;

    #[test]
    fn sqrt_is_close_enough() {
        for i in 1..20_000 {
            let x = i as f32 * 0.37;
            let (a, b) = (sqrt(x), std::primitive::f32::sqrt(x));
            assert!((a - b).abs() <= b * 2e-6, "sqrt({x}) = {a}, want {b}");
        }
        assert_eq!(sqrt(0.0), 0.0);
        assert_eq!(sqrt(-1.0), 0.0);
    }

    #[test]
    fn over_is_exact_at_the_ends_and_rounds_in_between() {
        assert_eq!(over(0x0012_3456, 0xFF65_4321), 0xFF65_4321, "opaque replaces");
        assert_eq!(over(0x8012_3456, 0), 0x8012_3456, "transparent leaves");
        assert_eq!(over(0xFFFF_FFFF, 0x8000_0000), 0xFF7F_7F7F, "half black over white: 255 * 127 / 255 = 127");
        assert_eq!(over(0x00C8_6432, 0x8040_2010) & 0x00FF_FFFF, 0x00A4_5229, "0x40 + (200 * 127 + 127) / 255 = 164");
    }

    // A: Shape::paint is Shape::pixel "over" each pixel of the clip, and nothing outside the clip; the corners of a rounded box stay
    // empty, the inside is the gradient.
    #[test]
    fn paint_is_pixel_over_inside_the_clip() {
        let s = Shape::gradient(0xFF20_4080, 0xFF40_80C0, 0xFF10_2030, 0xFF30_6090, 0.5).radius(6.0).border(1.0, 0xFF00_0000);
        let (w, h) = (40usize, 30usize);
        let mut px = vec![0x1122_3344u32; w * h];
        let rect = Rect::new(5, 4, 30, 20);
        let clip = Rect::new(0, 0, 30, 30); // the box's right end is outside it
        s.paint(&mut px, w, clip, rect);
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let got = px[y as usize * w + x as usize];
                if !clip.contains(x, y) {
                    assert_eq!(got, 0x1122_3344, "outside the clip: untouched at ({x}, {y})");
                    continue;
                }
                let c = s.pixel(rect, x, y);
                let want = if c[3] > 0.0 { over_f(0x1122_3344, c) } else { 0x1122_3344 };
                assert_eq!(got, want, "({x}, {y})");
            }
        }
        assert_eq!(px[4 * w + 5], 0x1122_3344, "the rounded corner is empty");
        assert_eq!(px[4 * w + 15], 0xFF00_0000, "the top border, opaque");
        assert_eq!(px[8 * w + 15] >> 24, 0xFF, "the inside is opaque");
        let top = px[6 * w + 15];
        let bottom = px[20 * w + 15];
        assert_ne!(top, bottom, "a gradient");
    }

    // A: the 9x bevel: dark bottom-right line, light top-left, face inside; swapped pressed; all clipped and opaque.
    #[test]
    fn bevel_paints_its_three_rectangles() {
        let b = Button::Bevel { face: 0x00D4_D0C8, light: 0x00FF_FFFF, dark: 0x0040_4040 };
        let (w, h) = (20usize, 12usize);
        let mut px = vec![0u32; w * h];
        let r = Rect::new(2, 2, 10, 8);
        b.paint(false, &mut px, w, Rect::new(0, 0, w as i32, h as i32), r, 1);
        assert_eq!(px[2 * w + 2], 0xFFFF_FFFF, "top-left: light");
        assert_eq!(px[9 * w + 11], 0xFF40_4040, "bottom-right: dark");
        assert_eq!(px[5 * w + 6], 0xFFD4_D0C8, "face");
        assert_eq!(px[w + 1], 0, "outside: untouched");
        b.paint(true, &mut px, w, Rect::new(0, 0, w as i32, h as i32), r, 1);
        assert_eq!((px[2 * w + 2], px[9 * w + 11]), (0xFF40_4040, 0xFFFF_FFFF), "pressed: swapped");
        let mut px = vec![0u32; w * h];
        b.paint(false, &mut px, w, Rect::new(0, 0, 5, 5), r, 1);
        assert_eq!(px[5 * w + 6], 0, "clipped");
    }
}
