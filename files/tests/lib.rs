//! The library's pieces: entries and how they are shown, the per-folder layout, the open-with table, and previews — made from real
//! PNG and text bytes, encoded, and decoded with every way a hostile provider could lie checked.

use files::entry::{self, Entry, Kind, Layout};
use files::open_with;
use files::preview::{self, Body, Preview};

fn e(name: &str, kind: Kind) -> Entry {
    Entry { name: name.into(), kind }
}

#[test]
fn folders_first_then_names_ignoring_case() {
    let mut v = vec![e("b.txt", Kind::File), e("Zeta", Kind::Dir), e("A.png", Kind::File), e("alpha", Kind::Dir), e("a.png", Kind::File)];
    entry::sort(&mut v);
    let names: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["alpha", "Zeta", "A.png", "a.png", "b.txt"]);
}

#[test]
fn types_sizes_dates_modes() {
    assert_eq!(entry::type_name("x.PNG", Kind::File), "PNG image");
    assert_eq!(entry::type_name("notes.txt", Kind::File), "Text");
    assert_eq!(entry::type_name("a.xyz", Kind::File), "XYZ file");
    assert_eq!(entry::type_name("Makefile", Kind::File), "File");
    assert_eq!(entry::type_name(".bashrc", Kind::File), "File");
    assert_eq!(entry::type_name("bin", Kind::Dir), "Folder");
    assert_eq!(entry::size(0), "0 B");
    assert_eq!(entry::size(1023), "1023 B");
    assert_eq!(entry::size(1536), "1.5 KiB");
    assert_eq!(entry::size(10 * 1024 * 1024 + 1), "10 MiB");
    assert_eq!(entry::size(1024 * 1024 - 1), "1023 KiB");
    assert_eq!(entry::time(0), "1970-01-01 00:00");
    assert_eq!(entry::time(1_791_460_800), "2026-10-08 12:00");
    assert_eq!(entry::time(951_782_400), "2000-02-29 00:00");
    assert_eq!(entry::time(-1), "1969-12-31 23:59");
    assert_eq!(entry::mode(0o040_755), "drwxr-xr-x");
    assert_eq!(entry::mode(0o100_640), "-rw-r-----");
    assert_eq!(entry::mode(0o120_777), "lrwxrwxrwx");
    assert_eq!(entry::parent("/mnt/bin"), "/mnt");
    assert_eq!(entry::parent("/mnt"), "/");
    assert_eq!(entry::parent("/"), "/");
    assert_eq!(entry::join("/", "tmp"), "/tmp");
    assert_eq!(entry::join("/tmp", "f"), "/tmp/f");
}

#[test]
fn layout_is_the_folders_not_the_selections() {
    let mixed = [e("a.png", Kind::File), e("b.txt", Kind::File), e("c.txt", Kind::File), e("sub", Kind::Dir)];
    assert_eq!(entry::layout(&mixed), Layout::Right);
    let pics = [e("a.png", Kind::File), e("b.png", Kind::File), e("c.txt", Kind::File)];
    assert_eq!(entry::layout(&pics), Layout::Bottom);
    let half = [e("a.png", Kind::File), e("b.txt", Kind::File)];
    assert_eq!(entry::layout(&half), Layout::Right, "exactly half is not more than half");
    let dirs_only = [e("a.png", Kind::Dir), e("b", Kind::Dir)];
    assert_eq!(entry::layout(&dirs_only), Layout::Right);
    assert_eq!(entry::layout(&[]), Layout::Right);
}

#[test]
fn open_with_table() {
    let t = open_with::parse("# comment\npng\timgview\n\n.TXT\tterm\nbroken line\nwad\t\n");
    assert_eq!(t, vec![("png".into(), "imgview".into()), ("txt".into(), "term".into())]);
    assert_eq!(open_with::command(&t, "Photo.PNG"), Some("imgview"));
    assert_eq!(open_with::command(&t, "a.txt"), Some("term"));
    assert_eq!(open_with::command(&t, "doom.wad"), None);
    assert_eq!(open_with::command(&t, "README"), None);
}

/// A real PNG: 300 x 100, a red left half and a half-transparent blue right half.
fn png() -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 300, 100);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        let mut data = Vec::new();
        for _y in 0..100 {
            for x in 0..300 {
                data.extend_from_slice(if x < 150 { &[255, 0, 0, 255] } else { &[0, 0, 255, 128] });
            }
        }
        w.write_image_data(&data).unwrap();
    }
    out
}

#[test]
fn a_png_previews_fitted_to_the_box() {
    let bytes = png();
    let p = preview::make(&bytes, bytes.len() as u64, 150, 150);
    assert_eq!(p.meta, vec![("Image".into(), "300 × 100 pixels".into())]);
    let Body::Image { w, h, px } = &p.body else { panic!("{:?}", p.body) };
    assert_eq!((*w, *h), (150, 50), "scaled down, shape kept");
    assert_eq!(px[25 * 150 + 10], 0xFFFF_0000);
    assert_eq!(px[25 * 150 + 140] >> 24, 128, "premultiplied, alpha kept");
    // through the memfd and back
    let back = preview::decode(&preview::encode(&p), 150, 150).unwrap();
    assert_eq!(back, p);
    // small images are not scaled up
    let p = preview::make(&bytes, bytes.len() as u64, 512, 512);
    assert!(matches!(p.body, Body::Image { w: 300, h: 100, .. }));
    // a broken one says so
    let mut bad = bytes.clone();
    bad.truncate(60);
    let p = preview::make(&bad, bad.len() as u64, 512, 512);
    assert!(matches!(&p.body, Body::None(why) if why.starts_with("not a valid PNG")), "{:?}", p.body);
    // one the provider only got the start of
    let p = preview::make(&bytes[..100], 1 << 30, 512, 512);
    assert!(matches!(&p.body, Body::None(why) if why.starts_with("too large")));
}

#[test]
fn text_previews_clean_and_bounded() {
    let src = "fn main() {\n\tprintln!(\"hola\");\x1b[31m\n}\n";
    let p = preview::make(src.as_bytes(), src.len() as u64, 100, 100);
    assert_eq!(p.body, Body::Text(vec!["fn main() {".into(), "    println!(\"hola\");[31m".into(), "}".into()]));
    assert_eq!(p.meta, vec![("Text".into(), "3 lines, UTF-8".into())]);
    assert_eq!(preview::decode(&preview::encode(&p), 100, 100).unwrap(), p);
    // long files: MAX_LINES lines, each at most MAX_LINE characters; "+" when the provider read only the start
    let long: String = (0..1000).map(|i| format!("{}{}\n", i, "x".repeat(400))).collect();
    let head = &long.as_bytes()[..60_000];
    let p = preview::make(head, long.len() as u64, 100, 100);
    let Body::Text(lines) = &p.body else { panic!() };
    assert!(lines.len() <= preview::MAX_LINES);
    assert!(lines.iter().all(|l| l.chars().count() <= preview::MAX_LINE));
    assert!(p.meta[0].1.ends_with("+ lines, UTF-8"), "{:?}", p.meta);
    assert!(preview::decode(&preview::encode(&p), 1, 1).is_ok());
    // a cut multi-byte character at the end is still text
    let s = "año".as_bytes();
    let p = preview::make(&s[..2], 10, 100, 100);
    assert_eq!(p.body, Body::Text(vec!["a".into()]));
    // binary
    let p = preview::make(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0], 1000, 100, 100);
    assert!(matches!(&p.body, Body::None(why) if why.contains("binary")));
    let p = preview::make(&[0xff, 0xfe, 0x41], 3, 100, 100);
    assert!(matches!(p.body, Body::None(_)));
}

fn header(kind: u32, w: u32, h: u32, meta: u32, body: u32) -> Vec<u8> {
    let mut v = b"FPV1".to_vec();
    for x in [kind, w, h, meta, body, 0, 0] {
        v.extend_from_slice(&x.to_le_bytes());
    }
    v
}

#[test]
fn decode_refuses_a_lying_provider() {
    let err = |b: &[u8]| preview::decode(b, 64, 64).unwrap_err();
    assert!(err(b"").contains("FPV1"));
    assert!(err(&[0u8; 64]).contains("FPV1"));
    // an image bigger than the box
    let mut b = header(1, 65, 1, 0, 65 * 4);
    b.resize(b.len() + 65 * 4, 0);
    assert!(err(&b).contains("65x1 for a 64x64 box"), "{}", err(&b));
    // pixels that do not match the size
    let mut b = header(1, 2, 2, 0, 12);
    b.resize(b.len() + 12, 0);
    assert!(err(&b).contains("12 bytes of pixels for 2x2"));
    let mut b = header(1, 2, 2, 0, 20);
    b.resize(b.len() + 20, 0);
    assert!(err(&b).contains("20 bytes of pixels for 2x2"), "extra bytes are refused too");
    // lengths past the end, and ones that overflow
    assert!(err(&header(2, 0, 0, 10, 10)).contains("the output has 32"));
    assert!(err(&header(2, 0, 0, 4000, u32::MAX)).contains("says"));
    assert!(err(&header(2, 0, 0, 5000, 0)).contains("metadata"));
    // not UTF-8, too many lines, unknown kinds
    let mut b = header(2, 0, 0, 0, 2);
    b.extend_from_slice(&[0xff, 0xfe]);
    assert!(err(&b).contains("not UTF-8"));
    let lines = "\n".repeat(preview::MAX_LINES);
    let mut b = header(2, 0, 0, 0, lines.len() as u32);
    b.extend_from_slice(lines.as_bytes());
    assert!(err(&b).contains("lines, at most"));
    assert!(err(&header(9, 0, 0, 0, 0)).contains("unknown kind 9"));
    // a well-formed "none" is fine, and its reason is bounded
    let why = "y".repeat(1000);
    let mut b = header(3, 0, 0, 0, why.len() as u32);
    b.extend_from_slice(why.as_bytes());
    let Preview { body: Body::None(r), .. } = preview::decode(&b, 64, 64).unwrap() else { panic!() };
    assert_eq!(r.len(), preview::MAX_LINE);
    // capacity holds the largest answer
    assert!(preview::capacity(512, 512) >= preview::HEADER + preview::MAX_META + 512 * 512 * 4);
}
