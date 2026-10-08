//! The painter (`--features render`) against the real Noto fonts (`scripts/fetch-fonts.sh` puts them in `disk-image-root/`): pixels
//! where `render` said, and nothing past a clip.

use gui::theme::LUNA;
use ui::render::{Painter, FONT_FILES};
use ui::*;

fn painter() -> Painter {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../disk-image-root/usr/share/fonts");
    let files = FONT_FILES.iter().map(|f| std::fs::read(format!("{dir}/{f}")).expect("fonts: run scripts/fetch-fonts.sh")).collect();
    Painter::new(files, 15.0).expect("fonts")
}

const W: usize = 320;
const H: usize = 200;

#[test]
fn paints_where_render_said() {
    let mut p = painter();
    let lh = p.line_height();
    assert!((14..=24).contains(&lh), "line height {lh}");
    let long = "W".repeat(60);
    let rows = |i: usize| Row { key: i as u64, cells: vec![if i == 1 { long.clone() } else { format!("row {i}") }, "9 B".into()] };
    let cols = [Column { title: "Name", width: Size::Fixed(150), right: false }, Column { title: "Size", width: Size::Fill, right: true }];
    let img: Vec<u32> = (0..16 * 8).map(|i| 0x0012_3400 + i as u32).collect();
    let t = Widget::Column(vec![
        (Size::Auto, Widget::Row(vec![(Size::Auto, Widget::button(2, "OK")), (Size::Fixed(16), Widget::Image { id: 3, name: "icon", pixels: &img, w: 16, h: 8 })])),
        (Size::Fill, Widget::List(List { id: 4, name: "L", len: 5, columns: &cols, style: ListStyle::Table, row: &rows })),
    ]);
    let mut st = State::new(&LUNA, 1);
    st.set_focus(Some(4));
    st.select(4, Some(2));
    let f = st.render("t", &t, (W as i32, H as i32), &mut p);
    let mut px = vec![0xDEAD_BEEFu32; W * H];
    p.paint(&f.paint, &mut px, W, H);
    let at = |x: i32, y: i32| px[y as usize * W + x as usize];
    let opts: Vec<&Node> = f.nodes.iter().filter(|n| n.role == Role::ListBoxOption).collect();
    // the face everywhere nothing else is
    assert!(px.iter().all(|&c| c != 0xDEAD_BEEF));
    // the selected row's background, past its text
    let sel = opts[2].bounds;
    assert_eq!(at(sel.x + sel.w - 60, sel.y + 2), LUNA.widgets.selection);
    // text left ink in the cell; the long row's ink stops at its column (150 - padding)
    let row1 = opts[1].bounds;
    let ink = |x0: i32, x1: i32, r: Rect| (x0..x1).any(|x| (r.y..r.y + r.h).any(|y| at(x, y) != LUNA.widgets.field));
    assert!(ink(row1.x + 4, row1.x + 40, row1), "no text in row 1");
    assert!(!ink(row1.x + 146, row1.x + 200, row1), "the long name ran past its column");
    // right-aligned size: ink near the right edge of the row, none in the middle of the second column
    let row0 = opts[0].bounds;
    assert!(ink(row0.x + row0.w - 30, row0.x + row0.w - 4, row0));
    assert!(!ink(row0.x + 160, row0.x + 250, row0));
    // the image, pixel for pixel
    let im = f.nodes.iter().find(|n| n.id == 3).unwrap().bounds;
    assert_eq!(at(im.x + 5, im.y + 3), img[3 * 16 + 5]);
    // the button: the theme's shape (a light face at its middle, left of the label)
    let b = f.nodes.iter().find(|n| n.id == 2).unwrap().bounds;
    let c = at(b.x + 3, b.y + b.h / 2);
    assert!(c & 0xFF > 0xC0 && (c >> 16) & 0xFF > 0xC0, "button face {c:06x}");
}
