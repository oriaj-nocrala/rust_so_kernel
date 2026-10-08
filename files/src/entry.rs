//! A folder's entries as the list shows them. Reading the folder gives names and kinds only (`getdents64`'s `d_type`); sizes, dates and
//! permissions are `stat`ed when a row is shown (the app caches them), never for the whole folder.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Dir,
    File,
    Link,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
}

/// Folders first, then by name ignoring case (ties by the exact bytes, so the order is total).
pub fn sort(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        let dir = |e: &Entry| e.kind != Kind::Dir;
        dir(a).cmp(&dir(b)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())).then_with(|| a.name.cmp(&b.name))
    });
}

/// The extension, lower case, without the dot; `None` for none (or a dotfile's leading dot only).
pub fn extension(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty() && !ext.is_empty()).then(|| ext.to_lowercase())
}

const TYPES: [(&str, &str); 14] = [
    ("png", "PNG image"),
    ("txt", "Text"),
    ("md", "Markdown text"),
    ("rs", "Rust source"),
    ("c", "C source"),
    ("h", "C header"),
    ("sh", "Shell script"),
    ("conf", "Configuration"),
    ("ttf", "TrueType font"),
    ("wad", "DOOM data"),
    ("pak", "Quake data"),
    ("json", "JSON"),
    ("toml", "TOML"),
    ("log", "Log"),
];

/// What the Type column says.
pub fn type_name(name: &str, kind: Kind) -> String {
    match kind {
        Kind::Dir => return "Folder".into(),
        Kind::Link => return "Link".into(),
        Kind::Other => return "Special file".into(),
        Kind::File => {}
    }
    match extension(name) {
        Some(e) => TYPES.iter().find(|(x, _)| *x == e).map_or_else(|| format!("{} file", e.to_uppercase()), |(_, t)| (*t).into()),
        None => "File".into(),
    }
}

pub fn is_image(name: &str) -> bool {
    extension(name).as_deref() == Some("png")
}

/// Where the preview goes, for a whole folder (P2.3: never per selection, so arrowing through mixed files does not move the list).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// Beside the list: text and code read in tall columns.
    Right,
    /// Under the list: pictures are mostly wider than tall.
    Bottom,
}

/// Bottom when images are more than half of the folder's files, right otherwise.
pub fn layout(entries: &[Entry]) -> Layout {
    let files = entries.iter().filter(|e| e.kind == Kind::File).count();
    let images = entries.iter().filter(|e| e.kind == Kind::File && is_image(&e.name)).count();
    if files > 0 && images * 2 > files {
        Layout::Bottom
    } else {
        Layout::Right
    }
}

/// `1023 B`, `1.5 KiB`, `12 MiB`: three significant figures at most.
pub fn size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    if n < 1024 {
        return format!("{} B", n);
    }
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if v < 10.0 {
        format!("{:.1} {}", (v * 10.0).floor() / 10.0, UNITS[u])
    } else {
        format!("{} {}", v.floor() as u64, UNITS[u])
    }
}

/// Seconds since the epoch as `YYYY-MM-DD HH:MM` UTC (there is no time zone database).
pub fn time(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{:04}-{:02}-{:02} {:02}:{:02}", y, m, d, rem / 3600, rem % 3600 / 60)
}

/// `drwxr-xr-x` from `st_mode`.
pub fn mode(m: u32) -> String {
    let t = match m & 0o170_000 {
        0o040_000 => 'd',
        0o120_000 => 'l',
        0o020_000 => 'c',
        0o060_000 => 'b',
        0o010_000 => 'p',
        0o140_000 => 's',
        _ => '-',
    };
    let mut s = String::from(t);
    for shift in [6, 3, 0] {
        let b = m >> shift;
        s.push(if b & 4 != 0 { 'r' } else { '-' });
        s.push(if b & 2 != 0 { 'w' } else { '-' });
        s.push(if b & 1 != 0 { 'x' } else { '-' });
    }
    s
}

/// The row of the entry called `name`.
pub fn position(entries: &[Entry], name: &str) -> Option<usize> {
    entries.iter().position(|e| e.name == name)
}

/// The parent of an absolute path (`/` for `/`).
pub fn parent(path: &str) -> String {
    match path.trim_end_matches('/').rsplit_once('/') {
        Some(("", _)) | None => "/".into(),
        Some((p, _)) => p.into(),
    }
}

/// `dir` joined with `name`.
pub fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{}{}", dir, name)
    } else {
        format!("{}/{}", dir, name)
    }
}
