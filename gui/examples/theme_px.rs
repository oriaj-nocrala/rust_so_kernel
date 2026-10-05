//! What a look paints at a pixel, for scripts that check screendumps (`scripts/gui-e2e.sh`): the same `Shape::paint` the compositor uses,
//! so the answer is exact. Prints `r,g,b`.
//!
//!   theme_px <theme> desktop <screen_w> <screen_h> <x> <y>
//!   theme_px <theme> bar <focused 0|1> <frame_x> <frame_y> <frame_w> <x> <y>     a title bar where nothing covers it (scale 1)
//!   theme_px <theme> strip <screen_w> <screen_h> <panel_h> <x> <y>              the taskbar's strip where no button is (over the desktop)

use gui::compositor::TITLE_H;
use gui::region::Rect;
use gui::theme::{self, Shape};

fn at(shape: &Shape, rect: Rect, x: i32, y: i32) -> u32 {
    let mut p = [0u32];
    shape.paint(&mut p, 1, Rect::new(0, 0, 1, 1), Rect::new(rect.x - x, rect.y - y, rect.w, rect.h));
    p[0] & 0x00FF_FFFF
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let n = |i: usize| -> i32 { a[i].parse().expect("a number") };
    let t = theme::by_name(&a[1]).expect("luna or 9x");
    let v = match a[2].as_str() {
        "desktop" => at(&t.background, Rect::new(0, 0, n(3), n(4)), n(5), n(6)),
        "bar" => {
            let b = t.title[if n(3) != 0 { 0 } else { 1 }];
            let fw = t.frame_w;
            let r = Rect::new(n(4) - fw, n(5) - fw, n(6) + 2 * fw, fw + TITLE_H + b.radius as i32 + 1);
            at(&b, r, n(7), n(8))
        }
        "strip" => {
            // the strip may be glass (translucent): over the desktop, as the CPU painter draws it (no blur)
            let (x, y) = (n(6), n(7));
            let mut p = [0u32];
            let one = Rect::new(0, 0, 1, 1);
            t.background.paint(&mut p, 1, one, Rect::new(-x, -y, n(3), n(4)));
            let r = Rect::new(0, n(4) - n(5), n(3), n(5));
            t.taskbar.bar.paint(&mut p, 1, one, Rect::new(r.x - x, r.y - y, r.w, r.h));
            p[0] & 0x00FF_FFFF
        }
        w => panic!("what is {w}?"),
    };
    println!("{},{},{}", v >> 16 & 255, v >> 8 & 255, v & 255);
}
