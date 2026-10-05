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
};

/// Every theme, in the order the theme key cycles through them.
pub static THEMES: [&Theme; 3] = [&FLAT, &LUNA, &NINES];

/// The theme called `name`, if there is one.
pub fn by_name(name: &str) -> Option<&'static Theme> {
    THEMES.iter().copied().find(|t| t.name == name)
}
