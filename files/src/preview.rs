//! Previews: what the provider makes of a file ([`make`], run by `files-preview` in capability mode on bytes it read through its only
//! file descriptor), how it travels back in the output memfd ([`encode`]), and the app's checks on what came back ([`decode`]). The
//! provider is untrusted (it parses arbitrary files), so `decode` takes nothing on faith: every length is bounded and checked.
//!
//! The memfd: a 32-byte header — `b"FPV1"`, then little-endian u32s: kind (1 image, 2 text, 3 none), width, height, metadata length,
//! body length, two reserved — then the metadata (UTF-8, `key\tvalue` lines), then the body: `w * h` premultiplied `0xAARRGGBB` pixels
//! (little-endian u32s), the text (UTF-8, `\n`-separated lines), or why there is no preview (UTF-8).

pub const MAGIC: &[u8; 4] = b"FPV1";
pub const HEADER: usize = 32;
pub const MAX_META: usize = 4096;
/// The most text a preview carries, and lines of it.
pub const MAX_TEXT: usize = 64 * 1024;
pub const MAX_LINES: usize = 200;
/// Longest line kept, in characters.
pub const MAX_LINE: usize = 240;
/// Largest file the provider decodes as an image.
pub const MAX_FILE: usize = 16 << 20;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    /// Premultiplied `0xAARRGGBB`, fitted inside the box the app asked for.
    Image { w: usize, h: usize, px: Vec<u32> },
    Text(Vec<String>),
    /// Why there is no preview (not an error: a binary file has none).
    None(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    /// Shown in the inspector: ("Image", "640 × 480 pixels").
    pub meta: Vec<(String, String)>,
    pub body: Body,
}

/// The memfd size that holds any answer for a `max_w x max_h` box.
pub fn capacity(max_w: usize, max_h: usize) -> usize {
    HEADER + MAX_META + MAX_TEXT.max(max_w * max_h * 4)
}

pub fn encode(p: &Preview) -> Vec<u8> {
    let mut meta = String::new();
    for (k, v) in &p.meta {
        let line = format!("{}\t{}\n", k.replace(['\t', '\n'], " "), v.replace(['\t', '\n'], " "));
        if meta.len() + line.len() > MAX_META {
            break;
        }
        meta.push_str(&line);
    }
    let (kind, w, h, body): (u32, usize, usize, Vec<u8>) = match &p.body {
        Body::Image { w, h, px } => (1, *w, *h, px.iter().flat_map(|c| c.to_le_bytes()).collect()),
        Body::Text(lines) => (2, 0, 0, lines.join("\n").into_bytes()),
        Body::None(why) => (3, 0, 0, why.as_bytes().to_vec()),
    };
    let mut out = Vec::with_capacity(HEADER + meta.len() + body.len());
    out.extend_from_slice(MAGIC);
    for v in [kind, w as u32, h as u32, meta.len() as u32, body.len() as u32, 0, 0] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(meta.as_bytes());
    out.extend_from_slice(&body);
    out
}

/// The provider's answer in `bytes`, for a `max_w x max_h` box; `Err` says what was wrong with it.
pub fn decode(bytes: &[u8], max_w: usize, max_h: usize) -> Result<Preview, String> {
    if bytes.len() < HEADER || &bytes[..4] != MAGIC {
        return Err("no answer (the output does not start with FPV1)".into());
    }
    let u = |i: usize| u32::from_le_bytes(bytes[4 + 4 * i..8 + 4 * i].try_into().unwrap()) as usize;
    let (kind, w, h, meta_len, body_len) = (u(0), u(1), u(2), u(3), u(4));
    if meta_len > MAX_META {
        return Err(format!("{} bytes of metadata, at most {}", meta_len, MAX_META));
    }
    let end = HEADER.checked_add(meta_len).and_then(|n| n.checked_add(body_len)).ok_or("lengths overflow")?;
    if end > bytes.len() {
        return Err(format!("says {} bytes, the output has {}", end, bytes.len()));
    }
    let meta = std::str::from_utf8(&bytes[HEADER..HEADER + meta_len]).map_err(|_| "metadata is not UTF-8")?;
    let meta = meta
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let body = &bytes[HEADER + meta_len..end];
    let text = |what: &str| std::str::from_utf8(body).map_err(|_| format!("{} is not UTF-8", what));
    let body = match kind {
        1 => {
            if w == 0 || h == 0 || w > max_w || h > max_h {
                return Err(format!("an image of {}x{} for a {}x{} box", w, h, max_w, max_h));
            }
            if body_len != w * h * 4 {
                return Err(format!("{} bytes of pixels for {}x{}", body_len, w, h));
            }
            Body::Image { w, h, px: body.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).collect() }
        }
        2 => {
            if body_len > MAX_TEXT {
                return Err(format!("{} bytes of text, at most {}", body_len, MAX_TEXT));
            }
            let lines: Vec<String> = text("the text")?.split('\n').map(String::from).collect();
            if lines.len() > MAX_LINES {
                return Err(format!("{} lines, at most {}", lines.len(), MAX_LINES));
            }
            Body::Text(lines)
        }
        3 => Body::None(text("the reason")?.chars().take(MAX_LINE).collect()),
        k => return Err(format!("unknown kind {}", k)),
    };
    Ok(Preview { meta, body })
}

/// The preview of a file whose first bytes are `head` (all of it if `whole`), for a `max_w x max_h` box. PNG by its signature; text
/// when the bytes are UTF-8 without NULs; anything else has none.
pub fn make(head: &[u8], file_len: u64, max_w: usize, max_h: usize) -> Preview {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    if head.starts_with(PNG) {
        if (file_len as usize) > MAX_FILE || head.len() < file_len as usize {
            return Preview { meta: vec![], body: Body::None(format!("too large to preview ({} bytes, at most {})", file_len, MAX_FILE)) };
        }
        return match img::decode_png(head) {
            Ok(im) => {
                let meta = vec![("Image".to_string(), format!("{} × {} pixels", im.w, im.h))];
                let im = fit(im, max_w, max_h);
                Preview { meta, body: Body::Image { w: im.w, h: im.h, px: im.px } }
            }
            Err(e) => Preview { meta: vec![], body: Body::None(format!("not a valid PNG: {}", e)) },
        };
    }
    let probe = &head[..head.len().min(MAX_TEXT)];
    // the last character may be cut by the read
    let valid = match std::str::from_utf8(probe) {
        Ok(s) => Some(s),
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&probe[..e.valid_up_to()]).ok(),
        Err(_) => None,
    };
    let Some(s) = valid.filter(|s| !s.contains('\0')) else {
        return Preview { meta: vec![("Contents".into(), "binary".into())], body: Body::None("binary data: no preview for this type".into()) };
    };
    let mut lines: Vec<String> = Vec::new();
    let mut bytes = 0;
    for l in s.split('\n') {
        if lines.len() == MAX_LINES {
            break;
        }
        let clean: String =
            l.replace('\t', "    ").chars().filter(|c| !c.is_control()).take(MAX_LINE).collect();
        if bytes + clean.len() + 1 > MAX_TEXT {
            break;
        }
        bytes += clean.len() + 1;
        lines.push(clean);
    }
    if lines.last().is_some_and(|l| l.is_empty()) && s.ends_with('\n') {
        lines.pop();
    }
    let total = s.matches('\n').count() + usize::from(!s.ends_with('\n') && !s.is_empty());
    let more = if (file_len as usize) > probe.len() { "+" } else { "" };
    let meta = vec![("Text".to_string(), format!("{}{} lines, UTF-8", total, more))];
    Preview { meta, body: Body::Text(lines) }
}

/// `im` scaled down (never up) to fit `max_w x max_h`, keeping its shape.
fn fit(im: img::Image, max_w: usize, max_h: usize) -> img::Image {
    if im.w <= max_w && im.h <= max_h {
        return im;
    }
    // the larger factor: w/max_w or h/max_h, as a fraction
    let (w, h) = if im.w * max_h >= im.h * max_w {
        (max_w, (im.h * max_w / im.w).max(1))
    } else {
        ((im.w * max_h / im.h).max(1), max_h)
    };
    im.resized(w, h)
}
