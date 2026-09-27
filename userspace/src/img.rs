//! Images from disk: the `img` crate (PNG → premultiplied `0xAARRGGBB`,
//! resampling) with the file read through [`crate::fs`]. Draw the result
//! with `draw::Canvas::blit_over`.
//!
//! Icons live under [`ICON_DIR`] on the data disk, never inside binaries,
//! laid out like a freedesktop theme: `<n>x<n>/<name>.png` for each size
//! the artwork exists at, and optionally `<name>.png` at the top as an
//! unsized fallback. [`load_icon`] turns a logical size and the screen's
//! `HIDPI` scale into pixels, picks the best source for that and
//! resamples it once — the result is blitted 1:1 every frame.

use alloc::format;
use alloc::string::String;
use core::fmt;

pub use img::{decode_png, pick_size, premultiply, Image, MAX_SIDE};

use crate::syscall;

pub const ICON_DIR: &str = "/mnt/usr/share/icons";

/// The `<n>x<n>` directories [`load_icon`] looks in.
pub const ICON_SIZES: [usize; 10] = [16, 22, 24, 32, 48, 64, 96, 128, 256, 512];

#[derive(Debug)]
pub enum LoadError {
    /// The negative errno of the `open`.
    Io(i64),
    Decode(img::Error),
    /// No `<n>x<n>/<name>.png` and no `<name>.png` under [`ICON_DIR`].
    NoIcon,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "open failed ({})", e),
            LoadError::Decode(e) => write!(f, "{}", e),
            LoadError::NoIcon => write!(f, "no such icon in {}", ICON_DIR),
        }
    }
}

/// Reads and decodes the PNG at `path`.
pub fn load(path: &str) -> Result<Image, LoadError> {
    let bytes = crate::fs::read_file(path).map_err(LoadError::Io)?;
    decode_png(&bytes).map_err(LoadError::Decode)
}

/// [`load`], then resampled to `w x h` (a copy-free no-op at its own size).
pub fn load_scaled(path: &str, w: usize, h: usize) -> Result<Image, LoadError> {
    let im = load(path)?;
    Ok(if (im.w, im.h) == (w, h) { im } else { im.resized(w, h) })
}

/// A loaded icon and where it came from, for logs.
pub struct Icon {
    pub image: Image,
    /// The file it was decoded from.
    pub path: String,
}

/// Icon `name` for a square of `size` logical pixels on a screen drawn at
/// `scale` (`Gfx::scale()`): `size * scale` pixels on each side. Takes the
/// smallest themed size at least that big (else the biggest), falls back
/// to the unsized `<name>.png`, and resamples if it isn't already exact.
pub fn load_icon(name: &str, size: usize, scale: usize) -> Result<Icon, LoadError> {
    let px = size * scale;
    let exists = |path: &str| syscall::with_cstr(path, |p| syscall::stat(p).is_ok());
    let mut have = [0usize; ICON_SIZES.len()];
    let mut n = 0;
    for s in ICON_SIZES {
        if exists(&format!("{}/{}x{}/{}.png", ICON_DIR, s, s, name)) {
            have[n] = s;
            n += 1;
        }
    }
    let path = match pick_size(&have[..n], px) {
        Some(s) => format!("{}/{}x{}/{}.png", ICON_DIR, s, s, name),
        None => {
            let p = format!("{}/{}.png", ICON_DIR, name);
            if !exists(&p) {
                return Err(LoadError::NoIcon);
            }
            p
        }
    };
    let image = load_scaled(&path, px, px)?;
    Ok(Icon { image, path })
}
