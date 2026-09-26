//! Against the real Noto fonts `scripts/fetch-fonts.sh` installs into
//! `disk-image-root/usr/share/fonts/` — the files programs will load on
//! constanos. Missing fonts fail loudly: a skipped test proves nothing.

use draw::Canvas;
use text::{Fonts, GlyphCache, Style, MONO, SANS};

fn fonts() -> Fonts {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../disk-image-root/usr/share/fonts");
    let mut f = Fonts::new();
    for name in ["NotoSans-Regular", "NotoSans-Bold", "NotoSansMono-Regular", "NotoSansMono-Bold"] {
        let path = format!("{dir}/{name}.ttf");
        let bytes = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("{path}: {e} — run scripts/fetch-fonts.sh first"));
        assert!(!f.add(bytes).is_empty(), "{path} registered no family");
    }
    f
}

const BG: u32 = 0x00_0000;
const FG: u32 = 0xFF_FFFF;

/// Draws `s` on a fresh black `w`x`h` canvas at (x, y); returns the pixels.
fn render(f: &mut Fonts, cache: &mut GlyphCache, s: &str, style: &Style, max: Option<f32>, w: usize, h: usize, x: i32, y: i32) -> Vec<u32> {
    let layout = f.layout(s, style, max);
    let mut px = vec![BG; w * h];
    cache.draw(&mut Canvas::new(&mut px, w, h, w), &layout, x, y);
    px
}

/// Bounding box `(x0, y0, x1, y1)` (exclusive ends) of non-background pixels.
fn ink(px: &[u32], w: usize) -> Option<(i32, i32, i32, i32)> {
    let mut b: Option<(i32, i32, i32, i32)> = None;
    for (i, &p) in px.iter().enumerate() {
        if p != BG {
            let (x, y) = ((i % w) as i32, (i / w) as i32);
            b = Some(match b {
                None => (x, y, x + 1, y + 1),
                Some((a, c, d, e)) => (a.min(x), c.min(y), d.max(x + 1), e.max(y + 1)),
            });
        }
    }
    b
}

#[test]
fn both_families_register_under_their_names() {
    let mut f = fonts();
    let fams = f.families();
    assert!(fams.iter().any(|n| n == SANS), "{fams:?}");
    assert!(fams.iter().any(|n| n == MONO), "{fams:?}");
}

#[test]
fn measure_is_the_box_of_what_is_drawn() {
    let mut f = fonts();
    let mut c = GlyphCache::default();
    for (s, size) in [("Hamburgefonstiv", 24.0), ("¿Él pidió ñandú y kiwi? ¡Sí!", 17.0), ("Wave To", 48.0)] {
        let st = Style::new(SANS, size).color(FG);
        let (w, h) = f.measure(s, &st, None);
        let (ox, oy) = (20, 20);
        let px = render(&mut f, &mut c, s, &st, None, 800, 200, ox, oy);
        let (x0, y0, x1, y1) = ink(&px, 800).expect("nothing drawn");
        // Ink inside the measured box (a pixel of side-bearing slack)...
        assert!(x0 >= ox - 1 && x1 <= ox + w + 1, "{s}: ink x {x0}..{x1}, box {ox}..{}", ox + w);
        assert!(y0 >= oy && y1 <= oy + h, "{s}: ink y {y0}..{y1}, box {oy}..{}", oy + h);
        // ...and filling its width: measure is not a loose over-estimate.
        assert!(x1 >= ox + w - (size as i32) / 4, "{s}: ink ends at {x1}, box at {}", ox + w);
        assert!(x0 <= ox + (size as i32) / 8, "{s}: ink starts at {x0}");
    }
}

#[test]
fn lines_never_exceed_the_width() {
    let mut f = fonts();
    let st = Style::new(SANS, 16.0);
    let para = "El veloz murciélago hindú comía feliz cardillo y kiwi. La cigüeña tocaba el saxofón detrás del palenque de paja.";
    for max in [80.0f32, 150.0, 333.0] {
        let l = f.layout(para, &st, Some(max));
        assert!(l.lines().count() > 1);
        for line in l.lines() {
            assert!(line.width <= max + 0.01, "line {:?} wider than {max}", &para[line.start..line.end]);
        }
        // Every byte lands on exactly one line, in order.
        let mut at = 0;
        for line in l.lines() {
            assert_eq!(line.start, at);
            at = line.end;
        }
        assert_eq!(at, para.len());
    }
}

#[test]
fn a_word_longer_than_the_line_is_broken_inside() {
    let mut f = fonts();
    let st = Style::new(SANS, 16.0);
    let l = f.layout("Supercalifragilisticoespialidoso", &st, Some(60.0));
    assert!(l.lines().count() >= 3);
    for line in l.lines() {
        assert!(line.width <= 60.01);
    }
}

#[test]
fn spaces_newlines_and_empty_text() {
    let mut f = fonts();
    let st = Style::new(SANS, 16.0);
    // Runs of spaces are kept (no CSS collapsing) and never start a line.
    let one = f.layout("a", &st, None).width();
    let spaced = f.layout("a     b", &st, None);
    assert_eq!(spaced.lines().count(), 1);
    assert!(spaced.width() > 3.0 * one);
    // Trailing spaces do not count towards the width.
    assert!((f.layout("a   ", &st, None).width() - one).abs() < 0.01);
    // `\n` forces a line; two in a row leave an empty one.
    assert_eq!(f.layout("a\nb", &st, None).lines().count(), 2);
    assert_eq!(f.layout("a\n\nb", &st, None).lines().count(), 3);
    // Empty text measures zero wide, draws nothing, and does not panic.
    let (w, _) = f.measure("", &st, None);
    assert_eq!(w, 0);
    let px = render(&mut f, &mut GlyphCache::default(), "", &st, None, 50, 50, 5, 5);
    assert!(ink(&px, 50).is_none());
}

#[test]
fn kerning_pulls_pairs_together() {
    let mut f = fonts();
    let st = Style::new(SANS, 40.0);
    // Noto's GPOS pulls these in by 1.6, 2.8 and 0.8 px at 40 px (measured).
    for (a, b, min) in [("A", "V", 1.2), ("T", "o", 2.0), ("V", "a", 0.5)] {
        let apart = f.layout(a, &st, None).width() + f.layout(b, &st, None).width();
        let pair = f.layout(&format!("{a}{b}"), &st, None).width();
        assert!(pair < apart - min, "{a}{b}: {pair} vs {apart} unkerned");
    }
}

#[test]
fn accented_and_inverted_marks_have_glyphs() {
    let mut f = fonts();
    for fam in [SANS, MONO] {
        for bold in [false, true] {
            let mut st = Style::new(fam, 20.0);
            if bold {
                st = st.bold();
            }
            let ids = f.layout("ñÑáéíóúüÁÉÍÓÚÜ¿¡çÇ€«»", &st, None).glyph_ids();
            assert!(!ids.is_empty());
            assert!(ids.iter().all(|&g| g != 0), "{fam} bold={bold}: .notdef in {ids:?}");
        }
    }
}

#[test]
fn mono_is_monospaced_and_sans_is_not() {
    let mut f = fonts();
    let m = Style::new(MONO, 20.0);
    let s = Style::new(SANS, 20.0);
    assert_eq!(f.layout("iiiii", &m, None).width(), f.layout("MMMMM", &m, None).width());
    assert!(f.layout("iiiii", &s, None).width() * 2.0 < f.layout("MMMMM", &s, None).width());
}

#[test]
fn bold_is_wider_and_darker() {
    let mut f = fonts();
    let mut c = GlyphCache::default();
    let r = Style::new(SANS, 24.0).color(FG);
    let b = r.bold();
    assert!(f.layout("Negrita", &b, None).width() > f.layout("Negrita", &r, None).width());
    let sum = |px: Vec<u32>| px.iter().map(|&p| (p & 0xFF) as u64).sum::<u64>();
    let lr = sum(render(&mut f, &mut c, "Negrita", &r, None, 300, 60, 5, 5));
    let lb = sum(render(&mut f, &mut c, "Negrita", &b, None, 300, 60, 5, 5));
    assert!(lb > lr * 5 / 4, "bold coverage {lb} vs regular {lr}");
}

#[test]
fn the_cache_stays_bounded_and_evicted_glyphs_come_back_identical() {
    let mut f = fonts();
    let st = Style::new(SANS, 28.0).color(FG);
    let s = "The quick brown fox jumps over the lazy dog, ¿verdad? ¡Sí! 0123456789";
    let mut big = GlyphCache::default();
    let reference = render(&mut f, &mut big, s, &st, Some(500.0), 520, 200, 3, 3);
    let bs = big.stats();
    assert_eq!(bs.evictions, 0);
    assert!(bs.entries > 30);
    // Drawn again: every glyph a hit.
    let again = render(&mut f, &mut big, s, &st, Some(500.0), 520, 200, 3, 3);
    assert_eq!(again, reference);
    assert_eq!(big.stats().misses, bs.misses);

    // A cache a tenth the size of the working set: constant eviction.
    let cap = bs.bytes / 10;
    let mut small = GlyphCache::new(cap);
    for _ in 0..3 {
        let px = render(&mut f, &mut small, s, &st, Some(500.0), 520, 200, 3, 3);
        assert_eq!(px, reference, "a re-rasterised glyph differs");
        assert!(small.stats().bytes <= cap);
    }
    assert!(small.stats().evictions > 0);
}

#[test]
fn drawing_is_clipped_to_the_canvas_and_leaves_padding_alone() {
    let mut f = fonts();
    let mut c = GlyphCache::default();
    let st = Style::new(SANS, 32.0).color(FG);
    let l = f.layout("Recorte ¿ñ? WWW", &st, None);
    // A 100x40 canvas at offset (10, 5) of a 140x60 buffer: stride 140,
    // every pixel outside the canvas pre-filled with 0xAA.
    let (bw, bh, cw, ch, ox, oy) = (140usize, 60usize, 100usize, 40usize, 10usize, 5usize);
    for (x, y) in [(-15, -10), (40, 12), (-5, 20), (0, 0)] {
        let mut buf = vec![0xAAu32; bw * bh];
        for row in 0..ch {
            for col in 0..cw {
                buf[(oy + row) * bw + ox + col] = BG;
            }
        }
        let start = oy * bw + ox;
        let len = (ch - 1) * bw + cw;
        c.draw(&mut Canvas::new(&mut buf[start..start + len], cw, ch, bw), &l, x, y);
        let mut drew = false;
        for (i, &p) in buf.iter().enumerate() {
            let (col, row) = (i % bw, i / bw);
            let inside = (ox..ox + cw).contains(&col) && (oy..oy + ch).contains(&row);
            if inside {
                drew |= p != BG;
            } else {
                assert_eq!(p, 0xAA, "wrote outside the canvas at ({col}, {row}) drawing at ({x}, {y})");
            }
        }
        assert!(drew, "nothing visible drawing at ({x}, {y})");
    }
}

#[test]
fn text_blends_over_what_is_there() {
    let mut f = fonts();
    let mut c = GlyphCache::default();
    let l = f.layout("O", &Style::new(SANS, 40.0).color(0xFF_0000), None);
    let (w, h) = (60usize, 60usize);
    let mut px = vec![0x00_00FFu32; w * h];
    c.draw(&mut Canvas::new(&mut px, w, h, w), &l, 5, 5);
    // Fully covered pixels are the text colour, edges a mix of both, and
    // the background untouched away from the glyph.
    assert!(px.iter().any(|&p| p == 0xFF_0000));
    assert!(px.iter().any(|&p| p & 0xFF_0000 != 0 && p & 0xFF != 0 && p & 0xFF00 == 0));
    assert_eq!(px[0], 0x00_00FF);
}
