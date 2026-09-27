//! Images from disk: the `img` crate (PNG → premultiplied `0xAARRGGBB`)
//! with the file read through [`crate::fs`]. Draw the result with
//! `draw::Canvas::blit_over`.
//!
//! Icons live under [`ICON_DIR`] on the data disk, not inside binaries.

use core::fmt;

pub use img::{decode_png, premultiply, Image, MAX_SIDE};

pub const ICON_DIR: &str = "/mnt/usr/share/icons";

#[derive(Debug)]
pub enum LoadError {
    /// The negative errno of the `open`.
    Io(i64),
    Decode(img::Error),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            LoadError::Io(e) => write!(f, "open failed ({})", e),
            LoadError::Decode(e) => write!(f, "{}", e),
        }
    }
}

/// Reads and decodes the PNG at `path`.
pub fn load(path: &str) -> Result<Image, LoadError> {
    let bytes = crate::fs::read_file(path).map_err(LoadError::Io)?;
    decode_png(&bytes).map_err(LoadError::Decode)
}
