//! The compositor driven the way phase 2.5's program will drive it: real
//! encoded requests in, events and pixels out.

extern crate std;

use std::boxed::Box;
use std::collections::BTreeMap;
use std::vec;
use std::vec::Vec;

use super::*;
use crate::protocol::Request as R;
use crate::wire::Encoder;

const W: i32 = 320;
const H: i32 = 240;
const STRIDE: usize = 336; // wider than W on purpose: padding must stay untouched
const PAD: u32 = 0xDEAD_BEEF;

struct Mem {
    ptr: *const u8,
    len: usize,
}

impl PoolMem for Mem {
    fn as_ptr(&self) -> *const u8 {
        self.ptr
    }
    fn len(&self) -> usize {
        self.len
    }
}

/// Pools are host memory indexed by a fake fd; the compositor gets their
/// pointer, as it would get an mmap.
struct H_ {
    comp: Compositor<Mem>,
    pools: BTreeMap<i32, Box<[u8]>>,
    screen: Vec<u32>,
    mapped_fds: Vec<i32>,
}

impl H_ {
    fn new() -> Self {
        H_ { comp: Compositor::new(W, H), pools: BTreeMap::new(), screen: vec![PAD; STRIDE * H as usize], mapped_fds: vec![] }
    }

    fn pool(&mut self, fd: i32, size: usize) {
        self.pools.insert(fd, vec![0u8; size].into_boxed_slice());
    }

    /// Fills a `w x h` rectangle of pixels at (x, y) of a buffer that starts
    /// at `offset` with row `stride`, in pool `fd`.
    #[allow(clippy::too_many_arguments)]
    fn draw(&mut self, fd: i32, offset: usize, stride: usize, x: usize, y: usize, w: usize, h: usize, color: u32) {
        let p = self.pools.get_mut(&fd).unwrap();
        for row in y..y + h {
            for col in x..x + w {
                let at = offset + row * stride + col * 4;
                p[at..at + 4].copy_from_slice(&color.to_ne_bytes());
            }
        }
    }

    fn send_bytes(&mut self, c: ClientId, bytes: &[u8], fds: &[i32]) {
        let H_ { comp, pools, mapped_fds, .. } = self;
        comp.client_data(c, bytes, fds, &mut |fd, _size| {
            mapped_fds.push(fd);
            pools.get(&fd).map(|b| Mem { ptr: b.as_ptr(), len: b.len() })
        });
    }

    fn send(&mut self, c: ClientId, reqs: &[Request]) {
        let mut e = Encoder::new();
        for r in reqs {
            r.encode(&mut e);
        }
        let (b, f) = e.take();
        self.send_bytes(c, &b, &f);
    }

    fn compose(&mut self) -> Vec<Rect> {
        self.comp.compose(&mut self.screen, STRIDE)
    }

    fn px(&self, x: i32, y: i32) -> u32 {
        self.screen[y as usize * STRIDE + x as usize]
    }

    /// A client with one `w x h` window filled with `color`, committed.
    /// Pool fd = 100 + client id; ids: pool 2, buffer 3, surface 4.
    fn window(&mut self, w: i32, h: i32, color: u32) -> ClientId {
        let c = self.comp.add_client();
        let fd = 100 + c as i32;
        let size = (w * h * 4) as usize;
        self.pool(fd, size);
        self.draw(fd, 0, w as usize * 4, 0, 0, w as usize, h as usize, color);
        self.send(c, &[
            R::CreatePool { id: 2, fd, size: size as u32 },
            R::CreateBuffer { pool: 2, id: 3, offset: 0, width: w, height: h, stride: w * 4, format: FORMAT_XRGB8888 },
            R::CreateSurface { id: 4 },
            R::Attach { surface: 4, buffer: 3 },
            R::Damage { surface: 4, x: 0, y: 0, w, h },
            R::Commit { surface: 4 },
        ]);
        c
    }

    /// The desktop's pixel at (x, y) in the current look (its background shape over anything: it is opaque).
    fn desktop(&self, x: i32, y: i32) -> u32 {
        let mut p = [0u32];
        self.comp.theme().background.paint(&mut p, 1, Rect::new(0, 0, 1, 1), Rect::new(-x, -y, W, H));
        p[0] & 0x00FF_FFFF
    }

    /// The title bar's pixel at (x, y) of `c`'s window (surface 4), drawn focused or not: past the bar's border and corners it is
    /// opaque, so this is what the screen shows there when nothing covers it.
    fn bar(&self, c: ClientId, focused: bool, x: i32, y: i32) -> u32 {
        let f = self.comp.window_frame(c, 4).unwrap();
        let t = self.comp.theme();
        let b = t.title[if focused { 0 } else { 1 }];
        let fw = t.frame_w;
        let r = Rect::new(f.x - fw, f.y - fw, f.w + 2 * fw, fw + TITLE_H + b.radius as i32 + 1);
        let mut p = [0u32];
        b.paint(&mut p, 1, Rect::new(0, 0, 1, 1), Rect::new(r.x - x, r.y - y, r.w, r.h));
        p[0] & 0x00FF_FFFF
    }

    fn events_for(&mut self, c: ClientId) -> Vec<Event> {
        self.comp.take_events().into_iter().filter(|(k, _)| *k == c).map(|(_, e)| e).collect()
    }

    fn padding_untouched(&self) -> bool {
        (0..H as usize).all(|y| self.screen[y * STRIDE + W as usize..(y + 1) * STRIDE].iter().all(|p| *p == PAD))
    }
}

#[test]
fn a_window_appears_with_its_title_bar_and_the_cursor() {
    let mut h = H_::new();
    let c = h.window(100, 50, 0x00FF_0000);
    let evs = h.events_for(c);
    assert_eq!(evs, vec![
        Event::Configure { surface: 4, width: W / 2, height: H / 2 },
        Event::Release { buffer: 3 },
        Event::Focus { surface: 4, focused: true },
    ]);
    assert_eq!(h.mapped_fds, vec![101]);
    assert_eq!(h.comp.take_fds_to_close(), vec![101], "the pool fd is closed once mapped");
    let f = h.comp.window_frame(c, 4).unwrap();
    assert_eq!(f, Rect::new(40, 40, 100, 50 + TITLE_H));
    h.compose();
    assert_eq!(h.px(0, 0), h.desktop(0, 0));
    assert_eq!(h.px(50, 45), h.bar(c, true, 50, 45));
    assert_eq!(h.px(40, 40 + TITLE_H), 0x00FF_0000);
    assert_eq!(h.px(139, 40 + TITLE_H + 49), 0x00FF_0000);
    assert_ne!(h.px(140, 40 + TITLE_H), 0x00FF_0000, "the content ends at 139 (the frame is next)");
    let (px, py) = h.comp.pointer();
    assert_eq!(h.px(px, py), 0, "cursor tip is black");
    assert_eq!(h.px(px + 1, py + 2), 0x00FF_FFFF, "cursor body is white");
    assert!(h.padding_untouched());
    assert!(!h.comp.has_damage());
}

#[test]
fn only_damaged_pixels_are_taken_and_flushed() {
    let mut h = H_::new();
    let c = h.window(100, 50, 0x0000_00FF);
    h.compose();
    h.comp.take_events();
    // The client repaints everything but damages only a 10x10 square.
    h.draw(101, 0, 400, 0, 0, 100, 50, 0x0000_FF00);
    h.send(c, &[R::Attach { surface: 4, buffer: 3 }, R::Damage { surface: 4, x: 5, y: 5, w: 10, h: 10 }, R::Commit { surface: 4 }]);
    let rects = h.compose();
    assert_eq!(rects, vec![Rect::new(45, 45 + TITLE_H, 10, 10)]);
    assert_eq!(h.px(45, 45 + TITLE_H), 0x0000_FF00);
    assert_eq!(h.px(44, 45 + TITLE_H), 0x0000_00FF, "outside the damage: the old content");
    assert_eq!(h.events_for(c), vec![Event::Release { buffer: 3 }]);
}

#[test]
fn released_buffers_can_be_reused_without_changing_the_screen() {
    let mut h = H_::new();
    let _c = h.window(60, 30, 0x0011_1111);
    h.compose();
    h.draw(101, 0, 240, 0, 0, 60, 30, 0x0022_2222); // no commit
    h.comp.pointer_motion(100, 100); // forces a repaint elsewhere too
    h.compose();
    // Uncover-and-repaint the whole window from its store.
    h.comp.damage.add(Rect::new(0, 0, W, H));
    h.compose();
    assert_eq!(h.px(40, 40 + TITLE_H), 0x0011_1111);
}

#[test]
fn stacking_click_to_raise_and_focus() {
    let mut h = H_::new();
    let a = h.window(100, 100, 0x00AA_0000); // at (40, 40)
    let b = h.window(100, 100, 0x0000_BB00); // at (72, 72), on top
    h.compose();
    assert_eq!(h.comp.stack(), &[(a, 4), (b, 4)]);
    assert_eq!(h.comp.focus(), Some((b, 4)));
    assert_eq!(h.px(80, 100), 0x0000_BB00);
    assert_eq!(h.px(45, 70), 0x00AA_0000);
    assert_eq!(h.px(82, 77), h.bar(b, true, 82, 77));
    h.comp.take_events();

    // Click a's visible content at (45, 70).
    let (px, py) = h.comp.pointer();
    h.comp.pointer_motion(45 - px, 70 - py);
    h.comp.take_events();
    h.comp.pointer_button(BTN_LEFT, true);
    h.comp.pointer_button(BTN_LEFT, false);
    assert_eq!(h.comp.stack(), &[(b, 4), (a, 4)]);
    assert_eq!(h.comp.focus(), Some((a, 4)));
    let evs = h.comp.take_events();
    assert!(evs.contains(&(b, Event::Focus { surface: 4, focused: false })));
    assert!(evs.contains(&(a, Event::Focus { surface: 4, focused: true })));
    assert!(evs.contains(&(a, Event::Button { surface: 4, code: BTN_LEFT, pressed: true })));
    assert!(evs.contains(&(a, Event::Button { surface: 4, code: BTN_LEFT, pressed: false })));
    h.compose();
    assert_eq!(h.px(100, 130), 0x00AA_0000, "a now covers the overlap");
    assert_eq!(h.px(72, 72), 0x00AA_0000);
    assert_eq!(h.px(165, 188), 0x0000_BB00, "b, past a's shadow");
    assert_ne!(h.px(150, 150), 0x0000_BB00, "a's shadow falls on b");
    assert_eq!(h.px(50, 45), h.bar(a, true, 50, 45));
}

#[test]
fn dragging_the_title_bar_moves_the_window() {
    let mut h = H_::new();
    let c = h.window(50, 40, 0x0012_3456);
    h.compose();
    let (px, py) = h.comp.pointer();
    h.comp.pointer_motion(45 - px, 45 - py); // on the title bar
    h.comp.pointer_button(BTN_LEFT, true);
    h.comp.take_events();
    h.comp.pointer_motion(100, 30);
    h.comp.pointer_button(BTN_LEFT, false);
    assert_eq!(h.comp.window_frame(c, 4), Some(Rect::new(140, 70, 50, 40 + TITLE_H)));
    assert!(h.events_for(c).is_empty(), "a title-bar drag reaches no client");
    h.comp.pointer_motion(5, 5); // released: the window stays put
    assert_eq!(h.comp.window_frame(c, 4), Some(Rect::new(140, 70, 50, 40 + TITLE_H)));
    h.comp.pointer_motion(-5, -5);
    h.comp.take_events();
    h.compose();
    assert_eq!(h.px(60, 60 + TITLE_H), h.desktop(60, 60 + TITLE_H), "where it was");
    assert_eq!(h.px(160, 70 + TITLE_H + 5), 0x0012_3456, "where it is");
    assert!(h.padding_untouched());

    // Dragged partly off screen: composing clips instead of panicking.
    h.comp.pointer_button(BTN_LEFT, true);
    h.comp.pointer_motion(-1000, -1000);
    h.comp.pointer_button(BTN_LEFT, false);
    h.compose();
    assert!(h.padding_untouched());
}

#[test]
fn keys_go_to_the_focused_window_and_ctrl_alt_backspace_quits() {
    let mut h = H_::new();
    let c = h.window(10, 10, 1);
    h.comp.take_events();
    h.comp.key(30, true);
    h.comp.key(30, false);
    assert_eq!(h.events_for(c), vec![
        Event::Key { surface: 4, code: 30, pressed: true },
        Event::Key { surface: 4, code: 30, pressed: false },
    ]);
    h.comp.key(29, true);
    h.comp.key(56, true);
    h.comp.take_events();
    h.comp.key(14, true);
    assert!(h.comp.quit_requested());
    assert!(h.events_for(c).is_empty(), "the combination is the compositor's own");
}

#[test]
fn motion_is_surface_local_and_only_over_content() {
    let mut h = H_::new();
    let c = h.window(100, 100, 1);
    h.comp.take_events();
    let (px, py) = h.comp.pointer();
    h.comp.pointer_motion(50 - px, 45 - py); // title bar
    assert!(h.events_for(c).is_empty());
    h.comp.pointer_motion(0, 30); // content (50, 75) -> local (10, 15)
    assert_eq!(h.events_for(c), vec![Event::Motion { surface: 4, x: 10, y: 15 }]);
    h.comp.pointer_motion(0, 1000); // clamped to the screen, outside the window
    assert!(h.events_for(c).is_empty());
    assert_eq!(h.comp.pointer().1, H - 1);
}

#[test]
fn frame_callbacks_fire_after_the_frame_and_free_their_id() {
    let mut h = H_::new();
    let c = h.window(10, 10, 1);
    h.comp.take_events();
    h.send(c, &[R::Frame { surface: 4, id: 9 }]);
    h.comp.frame_done(5);
    assert!(h.events_for(c).is_empty(), "not committed yet");
    h.send(c, &[R::Commit { surface: 4 }]);
    assert!(h.comp.has_frame_callbacks());
    h.comp.frame_done(16);
    assert_eq!(h.events_for(c), vec![Event::Done { callback: 9, ms: 16 }, Event::DeleteId { id: 9 }]);
    // The id is free again.
    h.send(c, &[R::Sync { id: 9 }]);
    assert_eq!(h.events_for(c), vec![Event::Done { callback: 9, ms: 0 }, Event::DeleteId { id: 9 }]);
    assert!(h.comp.has_client(c));
}

#[test]
fn a_buffer_outside_its_pool_is_an_error_not_a_read() {
    for (offset, w, hh, stride) in [(0, 10, 11, 40), (4, 10, 10, 40), (-4, 1, 1, 4), (0, 10, 10, 36), (0, 0, 5, 40)] {
        let mut h = H_::new();
        let c = h.comp.add_client();
        h.pool(7, 400);
        h.send(c, &[
            R::CreatePool { id: 2, fd: 7, size: 400 },
            R::CreateBuffer { pool: 2, id: 3, offset, width: w, height: hh, stride, format: FORMAT_XRGB8888 },
        ]);
        let evs = h.comp.take_events();
        assert_eq!(evs.len(), 1, "{offset} {w}x{hh}/{stride}");
        assert!(matches!(evs[0], (k, Event::Error { code, .. }) if k == c && code == ErrorCode::InvalidBuffer as u32));
        assert_eq!(h.comp.take_disconnects(), vec![c]);
        assert!(!h.comp.has_client(c));
    }
    // Exactly fitting is fine.
    let mut h = H_::new();
    let c = h.comp.add_client();
    h.pool(7, 400);
    h.send(c, &[
        R::CreatePool { id: 2, fd: 7, size: 400 },
        R::CreateBuffer { pool: 2, id: 3, offset: 0, width: 10, height: 10, stride: 40, format: FORMAT_XRGB8888 },
    ]);
    assert!(h.comp.take_events().is_empty());
}

#[test]
fn protocol_errors_disconnect() {
    let cases: Vec<(Vec<Request>, ErrorCode)> = vec![
        (vec![R::Commit { surface: 4 }], ErrorCode::InvalidObject),
        (vec![R::CreateSurface { id: 0 }], ErrorCode::InvalidId),
        (vec![R::CreateSurface { id: 1 }], ErrorCode::InvalidId),
        (vec![R::CreateSurface { id: 4 }, R::CreateSurface { id: 4 }], ErrorCode::InvalidId),
        (vec![R::CreateSurface { id: 4 }, R::Attach { surface: 4, buffer: 4 }], ErrorCode::InvalidObject),
        (vec![R::CreatePool { id: 2, fd: 55, size: 64 }], ErrorCode::BadPool), // fd 55 unknown: map fails
    ];
    for (reqs, code) in cases {
        let mut h = H_::new();
        let c = h.comp.add_client();
        h.send(c, &reqs);
        let err = h.comp.take_events().into_iter().find_map(|(_, e)| match e {
            Event::Error { code, .. } => Some(code),
            _ => None,
        });
        assert_eq!(err, Some(code as u32), "{reqs:?}");
        assert!(!h.comp.has_client(c));
    }
    // Wrong format (neither XRGB8888 = 1 nor ARGB8888 = 0).
    let mut h = H_::new();
    let c = h.comp.add_client();
    h.pool(7, 400);
    h.send(c, &[R::CreatePool { id: 2, fd: 7, size: 400 }, R::CreateBuffer { pool: 2, id: 3, offset: 0, width: 1, height: 1, stride: 4, format: 7 }]);
    assert!(h.comp.take_events().iter().any(|(_, e)| matches!(e, Event::Error { code, .. } if *code == ErrorCode::InvalidFormat as u32)));
    // Garbage bytes.
    let mut h = H_::new();
    let c = h.comp.add_client();
    h.send_bytes(c, &[1, 0, 0, 0, 0, 0, 3, 0], &[]);
    assert!(!h.comp.has_client(c));
}

#[test]
fn byte_at_a_time_gives_the_same_screen() {
    let mut whole = H_::new();
    whole.window(30, 20, 0x0055_5555);
    whole.compose();

    let mut h = H_::new();
    let c = h.comp.add_client();
    h.pool(101, 30 * 20 * 4);
    h.draw(101, 0, 120, 0, 0, 30, 20, 0x0055_5555);
    let mut e = Encoder::new();
    for r in [
        R::CreatePool { id: 2, fd: 101, size: 2400 },
        R::CreateBuffer { pool: 2, id: 3, offset: 0, width: 30, height: 20, stride: 120, format: FORMAT_XRGB8888 },
        R::CreateSurface { id: 4 },
        R::Attach { surface: 4, buffer: 3 },
        R::Commit { surface: 4 },
    ] {
        r.encode(&mut e);
    }
    let (bytes, fds) = e.take();
    for (i, b) in bytes.iter().enumerate() {
        h.send_bytes(c, &[*b], if i == 0 { &fds } else { &[] });
    }
    h.compose();
    assert_eq!(h.screen, whole.screen);
}

#[test]
fn disconnect_removes_windows_and_closes_unclaimed_fds() {
    let mut h = H_::new();
    let a = h.window(50, 50, 0x0000_0077);
    let b = h.window(50, 50, 0x0000_0088);
    h.compose();
    h.comp.take_fds_to_close();
    h.send_bytes(b, &[], &[66, 67]); // fds with no message to claim them
    h.comp.take_events();
    h.comp.remove_client(b);
    assert_eq!(h.comp.take_fds_to_close(), vec![66, 67]);
    assert_eq!(h.comp.stack(), &[(a, 4)]);
    assert_eq!(h.comp.focus(), Some((a, 4)));
    assert_eq!(h.events_for(a), vec![Event::Focus { surface: 4, focused: true }]);
    h.compose();
    assert_eq!(h.px(115, 130), h.desktop(115, 130), "where b was (past a's shadow)");
    assert_eq!(h.px(45, 45 + TITLE_H), 0x0000_0077);
    // Data for a gone client: its fds are closed, nothing else happens.
    h.send_bytes(b, &[0; 8], &[70]);
    assert_eq!(h.comp.take_fds_to_close(), vec![70]);
}

#[test]
fn null_attach_unmaps_and_destroyed_pool_keeps_buffers_alive() {
    let mut h = H_::new();
    let c = h.window(20, 20, 0x0000_0099);
    h.compose();
    h.send(c, &[R::DestroyPool { pool: 2 }]);
    // The buffer outlives its pool: attach it again after redrawing.
    h.draw(101, 0, 80, 0, 0, 20, 20, 0x0000_00AA);
    h.send(c, &[R::Attach { surface: 4, buffer: 3 }, R::Damage { surface: 4, x: 0, y: 0, w: 20, h: 20 }, R::Commit { surface: 4 }]);
    h.compose();
    assert_eq!(h.px(40, 40 + TITLE_H), 0x0000_00AA);
    assert!(h.comp.take_events().contains(&(c, Event::DeleteId { id: 2 })));

    h.send(c, &[R::Attach { surface: 4, buffer: 0 }, R::Commit { surface: 4 }]);
    assert!(h.comp.stack().is_empty());
    assert_eq!(h.comp.focus(), None);
    h.compose();
    assert_eq!(h.px(40, 40 + TITLE_H), h.desktop(40, 40 + TITLE_H));
    // Mapping it again places it anew.
    h.send(c, &[R::Attach { surface: 4, buffer: 3 }, R::Commit { surface: 4 }]);
    assert_eq!(h.comp.stack(), &[(c, 4)]);
}

#[test]
fn resize_repaints_the_old_and_new_area() {
    let mut h = H_::new();
    let c = h.window(80, 80, 0x0000_0011);
    h.compose();
    // A smaller second buffer in the same pool.
    h.draw(101, 0, 80, 0, 0, 20, 20, 0x0000_0022);
    h.send(c, &[
        R::CreateBuffer { pool: 2, id: 5, offset: 0, width: 20, height: 20, stride: 80, format: FORMAT_XRGB8888 },
        R::Attach { surface: 4, buffer: 5 },
        R::Commit { surface: 4 },
    ]);
    h.compose();
    assert_eq!(h.comp.window_frame(c, 4), Some(Rect::new(40, 40, 20, 20 + TITLE_H)));
    assert_eq!(h.px(45, 45 + TITLE_H), 0x0000_0022);
    assert_eq!(h.px(100, 100), h.desktop(100, 100), "the old area is gone");
}

#[test]
fn titles_are_kept_and_capped() {
    let mut h = H_::new();
    let c = h.window(10, 10, 1);
    h.send(c, &[R::SetTitle { surface: 4, title: "ñ".repeat(100) }]);
    let t = h.comp.window_title(c, 4).unwrap();
    assert!(t.len() <= crate::protocol::MAX_TITLE && t.chars().all(|ch| ch == 'ñ'));
}

// ── pointer lock ──────────────────────────────────────────────────────────

/// Moves the pointer to (x, y) and drops the events that caused.
fn pointer_to(h: &mut H_, x: i32, y: i32) {
    let (px, py) = h.comp.pointer();
    h.comp.pointer_motion(x - px, y - py);
    h.comp.take_events();
}

#[test]
fn a_lock_on_the_focused_window_turns_motion_relative() {
    let mut h = H_::new();
    let c = h.window(100, 80, 0x0012_3456);
    h.compose();
    pointer_to(&mut h, 60, 70); // over its content
    h.compose();
    h.send(c, &[R::LockPointer { surface: 4, on: true }]);
    assert_eq!(h.comp.pointer_locked(), Some((c, 4)));
    h.comp.pointer_motion(5, -3);
    h.comp.pointer_motion(0, 0);
    assert_eq!(h.events_for(c), vec![Event::RelativeMotion { surface: 4, dx: 5, dy: -3 }]);
    assert_eq!(h.comp.pointer(), (60, 70), "the pointer stays put");
    assert!(h.compose().is_empty(), "nothing moved on screen");
}

#[test]
fn a_locked_window_gets_every_button_and_no_drag_starts() {
    let mut h = H_::new();
    let c = h.window(100, 80, 0x0012_3456);
    h.compose();
    pointer_to(&mut h, 45, 45); // on the title bar
    h.send(c, &[R::LockPointer { surface: 4, on: true }]);
    h.comp.pointer_button(BTN_LEFT, true);
    h.comp.pointer_motion(30, 30);
    h.comp.pointer_button(BTN_LEFT, false);
    assert_eq!(
        h.events_for(c),
        vec![
            Event::Button { surface: 4, code: BTN_LEFT, pressed: true },
            Event::RelativeMotion { surface: 4, dx: 30, dy: 30 },
            Event::Button { surface: 4, code: BTN_LEFT, pressed: false },
        ]
    );
    assert_eq!(h.comp.window_frame(c, 4), Some(Rect::new(40, 40, 100, 100)), "not dragged");
}

#[test]
fn ctrl_alt_is_the_way_out_and_a_click_takes_the_lock_back() {
    let mut h = H_::new();
    let c = h.window(100, 80, 0x0012_3456);
    h.compose();
    pointer_to(&mut h, 60, 70);
    h.send(c, &[R::LockPointer { surface: 4, on: true }]);
    h.comp.key(29, true); // Ctrl
    assert!(h.comp.pointer_locked().is_some(), "Ctrl alone keeps it");
    h.comp.key(56, true); // Alt
    assert_eq!(h.comp.pointer_locked(), None);
    h.comp.key(56, false);
    h.comp.key(29, false);
    let evs = h.events_for(c);
    assert_eq!(evs.len(), 4, "the keys still reach the window: {evs:?}");
    h.comp.pointer_motion(10, 0);
    assert_eq!(h.comp.pointer(), (70, 70), "the pointer moves again");
    assert_eq!(h.events_for(c), vec![Event::Motion { surface: 4, x: 30, y: 10 }]);
    h.comp.pointer_button(BTN_LEFT, true);
    assert_eq!(h.comp.pointer_locked(), Some((c, 4)), "a click in the content takes it back");
}

#[test]
fn the_lock_follows_the_focus() {
    let mut h = H_::new();
    let a = h.window(100, 80, 0x00AA_0000);
    h.send(a, &[R::LockPointer { surface: 4, on: true }]);
    assert_eq!(h.comp.pointer_locked(), Some((a, 4)));
    let b = h.window(60, 40, 0x0000_BB00); // mapped on top, takes the focus
    assert_eq!(h.comp.focus(), Some((b, 4)));
    assert_eq!(h.comp.pointer_locked(), None, "the focus left, so did the lock");
    h.send(b, &[R::DestroySurface { surface: 4 }]);
    assert_eq!(h.comp.focus(), Some((a, 4)));
    assert_eq!(h.comp.pointer_locked(), Some((a, 4)), "focus back on a surface that wants it");
}

#[test]
fn a_lock_asked_without_focus_waits_and_can_be_given_up() {
    let mut h = H_::new();
    let a = h.window(100, 80, 0x00AA_0000);
    let b = h.window(60, 40, 0x0000_BB00);
    h.send(a, &[R::LockPointer { surface: 4, on: true }]);
    assert_eq!(h.comp.pointer_locked(), None, "a has no focus");
    h.send(b, &[R::LockPointer { surface: 4, on: true }]);
    assert_eq!(h.comp.pointer_locked(), Some((b, 4)));
    h.send(b, &[R::LockPointer { surface: 4, on: false }]);
    assert_eq!(h.comp.pointer_locked(), None);
    h.comp.pointer_motion(1, 1);
    assert!(!h.events_for(b).iter().any(|e| matches!(e, Event::RelativeMotion { .. })));
}

#[test]
fn a_locked_client_that_goes_away_releases_the_pointer() {
    let mut h = H_::new();
    let a = h.window(100, 80, 0x00AA_0000);
    h.send(a, &[R::LockPointer { surface: 4, on: true }]);
    h.comp.remove_client(a);
    assert_eq!(h.comp.pointer_locked(), None);
    h.comp.take_events();
    h.comp.pointer_motion(3, 3);
    assert!(h.comp.take_events().is_empty());
}

// ── window management (phase 4) ───────────────────────────────────────────

/// A client whose surface 4 takes the panel role, `ph` pixels tall, with a
/// `W x ph` buffer committed. Pool fd = 100 + client id.
fn panel(h: &mut H_, ph: i32, color: u32) -> ClientId {
    let c = h.comp.add_client();
    let fd = 100 + c as i32;
    let size = (W * ph * 4) as usize;
    h.pool(fd, size);
    h.draw(fd, 0, W as usize * 4, 0, 0, W as usize, ph as usize, color);
    h.send(c, &[R::CreateSurface { id: 4 }, R::SetPanel { surface: 4, height: ph }]);
    h.send(c, &[
        R::CreatePool { id: 2, fd, size: size as u32 },
        R::CreateBuffer { pool: 2, id: 3, offset: 0, width: W, height: ph, stride: W * 4, format: FORMAT_XRGB8888 },
        R::Attach { surface: 4, buffer: 3 },
        R::Damage { surface: 4, x: 0, y: 0, w: W, h: ph },
        R::Commit { surface: 4 },
    ]);
    c
}

/// Commits a new `w x h` buffer (a fresh pool, fd 200 + client id) to
/// client `c`'s surface 4, filled with `color` — a client answering
/// `resize`.
fn recommit(h: &mut H_, c: ClientId, w: i32, hh: i32, color: u32) {
    let fd = 200 + c as i32;
    let size = (w * hh * 4) as usize;
    h.pool(fd, size);
    h.draw(fd, 0, w as usize * 4, 0, 0, w as usize, hh as usize, color);
    h.send(c, &[
        R::CreatePool { id: 20, fd, size: size as u32 },
        R::CreateBuffer { pool: 20, id: 21, offset: 0, width: w, height: hh, stride: w * 4, format: FORMAT_XRGB8888 },
        R::Attach { surface: 4, buffer: 21 },
        R::Damage { surface: 4, x: 0, y: 0, w, h: hh },
        R::Commit { surface: 4 },
    ]);
}

fn click(h: &mut H_, x: i32, y: i32) {
    pointer_to(h, x, y);
    h.comp.pointer_button(BTN_LEFT, true);
    h.comp.pointer_button(BTN_LEFT, false);
}

fn resizes(evs: &[Event]) -> Vec<(i32, i32)> {
    evs.iter().filter_map(|e| if let Event::Resize { width, height, .. } = e { Some((*width, *height)) } else { None }).collect()
}

// A: the chain set_resizable → drag of the border → one resize, clamped.
#[test]
fn dragging_a_resizable_corner_sends_one_clamped_resize_on_release() {
    let mut h = H_::new();
    let c = h.window(100, 50, 0x0012_3456); // frame (40, 40, 100, 70)
    h.send(c, &[R::SetResizable { surface: 4, min_w: 60, min_h: 30 }]);
    h.compose();
    // Just outside the bottom-right corner: both edges.
    pointer_to(&mut h, 141, 111);
    assert_eq!(h.comp.hit(141, 111), Some(((c, 4), Zone::Edge(EDGE_RIGHT | EDGE_BOTTOM))));
    h.comp.pointer_button(BTN_LEFT, true);
    h.comp.pointer_motion(30, 20);
    h.comp.pointer_motion(10, 0);
    assert_eq!(h.comp.resize_outline(), Some(Rect::new(40, 40, 140, 90)));
    assert!(resizes(&h.events_for(c)).is_empty(), "nothing is sent while dragging");
    assert_eq!(h.comp.window_frame(c, 4), Some(Rect::new(40, 40, 100, 70)), "the window waits for the release");
    h.compose();
    assert_eq!(h.px(179, 100), OUTLINE, "the outline is drawn");
    h.comp.pointer_button(BTN_LEFT, false);
    assert_eq!(resizes(&h.events_for(c)), vec![(140, 70)], "exactly one resize");
    assert_eq!(h.comp.resize_outline(), None);
    assert_eq!(h.comp.window_content(c, 4), Some(Rect::new(40, 60, 140, 70)));
    h.compose();
    assert_eq!(h.px(139, 60), 0x0012_3456, "old content stays");
    assert_eq!(h.px(150, 60), WINDOW_BG, "the rest waits for the client");
    assert_ne!(h.px(179, 100), OUTLINE, "outline gone");

    // Shrinking past the minimum stops at it, from the left edge too.
    pointer_to(&mut h, 38, 90); // left border
    assert_eq!(h.comp.hit(38, 90), Some(((c, 4), Zone::Edge(EDGE_LEFT))));
    h.comp.pointer_button(BTN_LEFT, true);
    h.comp.pointer_motion(500, 0);
    h.comp.pointer_button(BTN_LEFT, false);
    assert_eq!(resizes(&h.events_for(c)), vec![(60, 70)]);
    assert_eq!(h.comp.window_content(c, 4), Some(Rect::new(120, 60, 60, 70)), "the right edge stayed");
    // And growing, as far as the pointer goes (it stops at the screen's
    // last column).
    pointer_to(&mut h, 181, 100); // right border
    h.comp.pointer_button(BTN_LEFT, true);
    h.comp.pointer_motion(5000, 0);
    h.comp.pointer_button(BTN_LEFT, false);
    assert_eq!(resizes(&h.events_for(c)), vec![(60 + (W - 1 - 181), 70)]);
    assert_eq!(h.comp.window_content(c, 4), Some(Rect::new(120, 60, 60 + (W - 1 - 181), 70)));
    assert!(h.padding_untouched());
}

// A: the answer arrives; a commit at the old size in between does not undo.
#[test]
fn the_frame_follows_the_client_once_it_answers() {
    let mut h = H_::new();
    let c = h.window(100, 50, 0x0000_0011);
    h.send(c, &[R::SetResizable { surface: 4, min_w: 1, min_h: 1 }]);
    pointer_to(&mut h, 141, 111);
    h.comp.pointer_button(BTN_LEFT, true);
    h.comp.pointer_motion(40, 40);
    h.comp.pointer_button(BTN_LEFT, false);
    assert_eq!(resizes(&h.events_for(c)), vec![(140, 90)]);
    // A frame drawn before the client saw the event.
    h.send(c, &[R::Attach { surface: 4, buffer: 3 }, R::Damage { surface: 4, x: 0, y: 0, w: 100, h: 50 }, R::Commit { surface: 4 }]);
    assert_eq!(h.comp.window_content(c, 4).unwrap().w, 140, "not snapped back");
    // Its answer, rounded down as a terminal rounds to whole cells.
    recommit(&mut h, c, 136, 88, 0x0000_0022);
    assert_eq!(h.comp.window_content(c, 4), Some(Rect::new(40, 60, 136, 88)), "the buffer has the last word");
    h.compose();
    assert_eq!(h.px(175, 147), 0x0000_0022);
    assert_ne!(h.px(176, 147), 0x0000_0022, "the content ends at 175");
}

// A: an answer that rounds back to the old size is still an answer.
#[test]
fn a_new_buffer_of_the_old_size_answers_too() {
    let mut h = H_::new();
    let c = h.window(100, 50, 0x0000_0011);
    h.send(c, &[R::SetResizable { surface: 4, min_w: 1, min_h: 1 }]);
    pointer_to(&mut h, 141, 111);
    h.comp.pointer_button(BTN_LEFT, true);
    h.comp.pointer_motion(5, 5); // less than a terminal cell
    h.comp.pointer_button(BTN_LEFT, false);
    assert_eq!(resizes(&h.events_for(c)), vec![(105, 55)]);
    assert_eq!(h.comp.window_content(c, 4).unwrap().w, 105);
    recommit(&mut h, c, 100, 50, 0x0000_0022);
    assert_eq!(h.comp.window_content(c, 4), Some(Rect::new(40, 60, 100, 50)));
}

// B: without set_resizable there are no borders and no maximize button.
#[test]
fn a_window_that_did_not_ask_cannot_be_resized_or_maximized() {
    let mut h = H_::new();
    let c = h.window(100, 50, 0x0012_3456);
    h.compose();
    assert_eq!(h.comp.hit(141, 111), None, "no border");
    assert_eq!(h.comp.hit(138, 108), Some(((c, 4), Zone::Content)), "no grip");
    assert_eq!(h.comp.hit(140 - TITLE_H - 2, 45), Some(((c, 4), Zone::Title)), "no maximize button");
    pointer_to(&mut h, 141, 111);
    h.comp.pointer_button(BTN_LEFT, true);
    h.comp.pointer_motion(40, 40);
    h.comp.pointer_button(BTN_LEFT, false);
    // Double click on the title: no maximize either.
    h.comp.set_time(1000);
    click(&mut h, 60, 45);
    h.comp.set_time(1100);
    click(&mut h, 60, 45);
    assert!(resizes(&h.events_for(c)).is_empty());
    assert_eq!(h.comp.window_frame(c, 4), Some(Rect::new(40, 40, 100, 70)));
    assert!(!h.comp.is_maximized(c, 4));
}

// A: maximize and restore, by button and by double click, with a panel.
#[test]
fn maximize_fills_the_work_area_and_restore_goes_back() {
    let mut h = H_::new();
    let c = h.window(100, 50, 0x0012_3456);
    h.send(c, &[R::SetResizable { surface: 4, min_w: 1, min_h: 1 }]);
    let max_btn = (140 - TITLE_H - TITLE_H / 2, 50);
    assert_eq!(h.comp.hit(max_btn.0, max_btn.1), Some(((c, 4), Zone::Maximize)));
    click(&mut h, max_btn.0, max_btn.1);
    assert!(h.comp.is_maximized(c, 4));
    assert_eq!(h.comp.window_frame(c, 4), Some(Rect::new(0, 0, W, H)));
    assert_eq!(resizes(&h.events_for(c)), vec![(W, H - TITLE_H)]);
    // Restore: the maximize button is now at the screen's right.
    click(&mut h, W - TITLE_H - TITLE_H / 2, 10);
    assert!(!h.comp.is_maximized(c, 4));
    assert_eq!(h.comp.window_frame(c, 4), Some(Rect::new(40, 40, 100, 70)));
    assert_eq!(resizes(&h.events_for(c)), Vec::<(i32, i32)>::new(), "back to the buffer's own size: nothing to ask");

    // With a panel, by double click.
    let _p = panel(&mut h, 30, 0x0099_9999);
    assert_eq!(h.comp.work_area(), Rect::new(0, 0, W, H - 30));
    h.comp.take_events();
    h.comp.set_time(5000);
    click(&mut h, 60, 45);
    h.comp.set_time(5300);
    click(&mut h, 60, 45);
    assert_eq!(h.comp.window_frame(c, 4), Some(Rect::new(0, 0, W, H - 30)), "the panel is not covered");
    assert_eq!(resizes(&h.events_for(c)), vec![(W, H - 30 - TITLE_H)]);
    h.comp.set_time(6000);
    click(&mut h, 60, 10);
    h.comp.set_time(6200);
    click(&mut h, 60, 10);
    assert_eq!(h.comp.window_frame(c, 4), Some(Rect::new(40, 40, 100, 70)));
    // Two slow clicks are not a double click.
    h.comp.set_time(7000);
    click(&mut h, 60, 45);
    h.comp.set_time(7000 + DOUBLE_CLICK_MS + 1);
    click(&mut h, 60, 45);
    assert!(!h.comp.is_maximized(c, 4));
}

// A: close reaches its owner only, on release over the button.
#[test]
fn close_goes_to_the_owner_only() {
    let mut h = H_::new();
    let a = h.window(100, 50, 0x00AA_0000); // (40, 40)
    let b = h.window(100, 50, 0x0000_BB00); // (72, 72)
    h.comp.take_events();
    let close_a = (140 - TITLE_H / 2, 50);
    assert_eq!(h.comp.hit(close_a.0, close_a.1), Some(((a, 4), Zone::Close)));
    // Pressed, then released elsewhere: cancelled.
    pointer_to(&mut h, close_a.0, close_a.1);
    h.compose();
    let up = h.px(124, 50); // inside the button, left of its glyph
    h.comp.pointer_button(BTN_LEFT, true);
    h.compose();
    assert_ne!(h.px(124, 50), up, "drawn pressed");
    pointer_to(&mut h, 60, 45);
    h.comp.pointer_button(BTN_LEFT, false);
    assert!(!h.comp.take_events().iter().any(|(_, e)| matches!(e, Event::Close { .. })));
    click(&mut h, close_a.0, close_a.1);
    let evs = h.comp.take_events();
    let closes: Vec<_> = evs.iter().filter(|(_, e)| matches!(e, Event::Close { .. })).collect();
    assert_eq!(closes, vec![&(a, Event::Close { surface: 4 })]);
    assert_eq!(h.comp.stack().len(), 2, "closing is the client's decision");
    let _ = b;
}

// A: the panel is above everything, outside the work area and never focused.
#[test]
fn the_panel_is_on_top_outside_the_work_area_and_never_focused() {
    let mut h = H_::new();
    let a = h.window(200, 200, 0x00AA_0000); // reaches y = 260 > H - 30
    let p = panel(&mut h, 30, 0x0099_9999);
    assert_eq!(h.comp.panel(), Some((p, 4)));
    assert_eq!(h.comp.stack(), &[(a, 4)], "the panel is not a window");
    assert_eq!(h.comp.focus(), Some((a, 4)), "mapping it took no focus");
    h.compose();
    assert_eq!(h.px(100, H - 1), 0x0099_9999, "above the window");
    assert_eq!(h.px(100, H - 31), 0x00AA_0000);
    // A click in it goes to it and leaves the focus alone.
    h.comp.take_events();
    click(&mut h, 100, H - 10);
    let evs = h.comp.take_events();
    assert!(evs.contains(&(p, Event::Button { surface: 4, code: BTN_LEFT, pressed: true })));
    assert!(evs.contains(&(p, Event::Button { surface: 4, code: BTN_LEFT, pressed: false })));
    assert_eq!(h.comp.focus(), Some((a, 4)));
    h.comp.key(30, true);
    assert!(h.events_for(p).is_empty(), "keys never go to the panel");
    // A later window is placed in the work area.
    let b = h.window(100, 150, 0x0000_BB00);
    assert_eq!(h.comp.window_frame(b, 4).unwrap().bottom(), H - 30, "pushed up to fit");
    // A second panel is a protocol error.
    let q = h.comp.add_client();
    h.send(q, &[R::CreateSurface { id: 4 }, R::SetPanel { surface: 4, height: 20 }]);
    assert!(h.comp.take_disconnects().contains(&q));
    // The panel gone, the work area is the screen again.
    h.comp.remove_client(p);
    assert_eq!(h.comp.work_area(), Rect::new(0, 0, W, H));
    h.compose();
    assert_eq!(h.px(300, H - 1), h.desktop(300, H - 1));
    assert_eq!(h.px(230, H - 5), 0x00AA_0000, "the window under it shows again");
}

// A: the window list, including a client that dies mid-resize.
#[test]
fn the_panel_hears_of_every_window_and_can_activate_one() {
    let mut h = H_::new();
    let a = h.window(100, 50, 0x00AA_0000);
    h.send(a, &[R::SetTitle { surface: 4, title: "a".into() }]);
    let p = panel(&mut h, 30, 0x0099_9999);
    let evs = h.events_for(p);
    assert!(evs.contains(&Event::Toplevel { surface: 4, id: 1, title: "a".into() }), "{evs:?}");
    assert!(evs.contains(&Event::ToplevelFocus { surface: 4, id: 1 }));
    let b = h.window(100, 50, 0x0000_BB00);
    h.send(b, &[R::SetTitle { surface: 4, title: "b".into() }]);
    let evs = h.events_for(p);
    assert_eq!(evs, vec![
        Event::Toplevel { surface: 4, id: 2, title: String::new() },
        Event::ToplevelFocus { surface: 4, id: 2 },
        Event::Toplevel { surface: 4, id: 2, title: "b".into() },
    ]);
    assert_eq!(h.comp.toplevels(), vec![(1, (a, 4)), (2, (b, 4))]);
    // Activate a from the panel.
    h.send(p, &[R::Activate { surface: 4, toplevel: 1 }]);
    assert_eq!(h.comp.focus(), Some((a, 4)));
    assert_eq!(h.comp.stack().last(), Some(&(a, 4)));
    assert!(h.events_for(p).contains(&Event::ToplevelFocus { surface: 4, id: 1 }));
    // Only the panel may: from b it does nothing.
    h.send(b, &[R::Activate { surface: 4, toplevel: 2 }]);
    assert_eq!(h.comp.focus(), Some((a, 4)));
    // a dies in the middle of a resize.
    h.send(a, &[R::SetResizable { surface: 4, min_w: 1, min_h: 1 }]);
    let f = h.comp.window_frame(a, 4).unwrap();
    pointer_to(&mut h, f.right() + 1, f.bottom() + 1);
    h.comp.pointer_button(BTN_LEFT, true);
    h.comp.pointer_motion(20, 20);
    assert!(h.comp.resize_outline().is_some());
    h.comp.remove_client(a);
    assert_eq!(h.comp.resize_outline(), None, "the drag ended with its window");
    h.comp.pointer_button(BTN_LEFT, false);
    let evs = h.events_for(p);
    assert!(evs.contains(&Event::ToplevelGone { surface: 4, id: 1 }), "{evs:?}");
    assert!(evs.contains(&Event::ToplevelFocus { surface: 4, id: 2 }));
    assert_eq!(h.comp.toplevels(), vec![(2, (b, 4))]);
    h.compose();
    assert!(h.padding_untouched());
    // Unmapping b (null attach) is gone too, and focus none.
    h.send(b, &[R::Attach { surface: 4, buffer: 0 }, R::Commit { surface: 4 }]);
    let evs = h.events_for(p);
    assert!(evs.contains(&Event::ToplevelGone { surface: 4, id: 2 }), "{evs:?}");
    assert!(evs.contains(&Event::ToplevelFocus { surface: 4, id: 0 }));
}

// A: titles go through the caller's closure, clipped, before the buttons.
#[test]
fn titles_are_painted_by_the_caller_inside_their_area() {
    let mut h = H_::new();
    let c = h.window(100, 50, 0x0012_3456);
    h.send(c, &[R::SetTitle { surface: 4, title: "hola".into() }]);
    let mut seen = Vec::new();
    h.comp.compose_with(&mut h.screen, STRIDE, &mut |t, clip, dst, stride| {
        seen.push((t.id, String::from(t.title), t.focused, t.area));
        for y in clip.y..clip.bottom() {
            for x in clip.x..clip.right() {
                dst[y as usize * stride + x as usize] = 0x00AB_CDEF;
            }
        }
    });
    assert_eq!(seen, vec![(1, String::from("hola"), true, Rect::new(46, 40, 140 - TITLE_H - 6 - 46, TITLE_H))]);
    assert_eq!(h.px(46, 45), 0x00AB_CDEF);
    assert_eq!(h.px(45, 45), h.bar(c, true, 45, 45), "padding before the text");
    assert_ne!(h.px(140 - TITLE_H / 2, 40 + TITLE_H / 2), 0x00AB_CDEF, "the close button is not text");
    // A title change repaints the bar.
    h.send(c, &[R::SetTitle { surface: 4, title: "adiós".into() }]);
    assert!(h.comp.damage().contains(50, 45));
}

// ── GPU buffers and the draw list (layer 4 of docs/gpu/g5-graphics-stack-plan.md) ──────────────────────────────────────────────────────

/// What a host's GPU would make of a draw list, in plain CPU memory: the oracle compares it with `compose`.
fn raster(ops: &[DrawOp], comp: &Compositor<Mem>, gpu: &BTreeMap<u64, (i32, Vec<u32>)>) -> Vec<u32> {
    let mut out = vec![PAD; STRIDE * H as usize];
    for op in ops {
        match op {
            DrawOp::Fill { rect, color } => fill(&mut out, STRIDE, *rect, *color),
            DrawOp::Gpu { handle, dst, sx, sy } => {
                let (w, px) = &gpu[handle];
                for y in 0..dst.h {
                    for x in 0..dst.w {
                        out[(dst.y + y) as usize * STRIDE + (dst.x + x) as usize] = px[((sy + y) * w + sx + x) as usize];
                    }
                }
            }
            DrawOp::Cpu { client, surface, dst, sx, sy, w, premul, .. } => {
                let px = comp.cpu_content(*client, *surface).unwrap();
                for y in 0..dst.h {
                    for x in 0..dst.w {
                        let (d, v) = ((dst.y + y) as usize * STRIDE + (dst.x + x) as usize, px[((sy + y) * w + sx + x) as usize]);
                        out[d] = if *premul { theme::over(out[d], v) } else { v };
                    }
                }
            }
            DrawOp::Title { .. } => {}
            DrawOp::Shape { rect, shape, clip } => {
                if let Some(c) = clip.unwrap_or(Rect::new(0, 0, W, H)).intersect(&Rect::new(0, 0, W, H)) {
                    shape.paint(&mut out, STRIDE, c, *rect)
                }
            }
            DrawOp::Cursor { x, y } => {
                for (cy, row) in CURSOR.iter().enumerate() {
                    for (cx, c) in row.iter().enumerate() {
                        let (px, py) = (x + cx as i32, y + cy as i32);
                        if px < 0 || py < 0 || px >= W || py >= H {
                            continue;
                        }
                        let v = match c {
                            b'X' => 0,
                            b'.' => 0x00FF_FFFF,
                            _ => continue,
                        };
                        out[py as usize * STRIDE + px as usize] = v;
                    }
                }
            }
        }
    }
    // XRGB, as compose leaves it
    for y in 0..H as usize {
        for p in &mut out[y * STRIDE..y * STRIDE + W as usize] {
            *p &= 0x00FF_FFFF;
        }
    }
    out
}

/// The screen rows of both pictures, without the padding columns.
fn same_picture(a: &[u32], b: &[u32]) -> Option<(i32, i32)> {
    for y in 0..H {
        for x in 0..W {
            if a[y as usize * STRIDE + x as usize] != b[y as usize * STRIDE + x as usize] {
                return Some((x, y));
            }
        }
    }
    None
}

#[test]
fn the_draw_list_paints_what_compose_paints() {
    let mut h = gpu_h();
    // three overlapping windows (one of them resizable, so a maximize button), a panel, a pressed close button, a resize outline's absence
    let a = h.window(120, 60, 0x00AA_0000);
    let b = h.window(100, 80, 0x0000_AA00);
    let c = h.window(90, 40, 0x0000_00AA);
    h.send(b, &[R::SetResizable { surface: 4, min_w: 50, min_h: 30 }]);
    let p = h.comp.add_client();
    h.pool(500, 320 * 16 * 4);
    h.draw(500, 0, 320 * 4, 0, 0, 320, 16, 0x0080_8080);
    h.send(p, &[
        R::CreatePool { id: 2, fd: 500, size: 320 * 16 * 4 },
        R::CreateBuffer { pool: 2, id: 3, offset: 0, width: 320, height: 16, stride: 320 * 4, format: FORMAT_XRGB8888 },
        R::CreateSurface { id: 4 },
        R::SetPanel { surface: 4, height: 16 },
        R::Attach { surface: 4, buffer: 3 },
        R::Commit { surface: 4 },
    ]);
    let _ = (a, c);
    // press the close button of the focused (top) window and park the pointer over a window
    let f = h.comp.window_frame(c, 4).unwrap();
    h.comp.pointer_motion(f.right() - 5 - h.comp.pointer().0, f.y + 5 - h.comp.pointer().1);
    h.comp.pointer_button(BTN_LEFT, true);
    h.compose();
    let want = h.screen.clone();
    let (epoch, ops) = h.comp.draw_list();
    assert_eq!(epoch, 1);
    let got = raster(&ops, &h.comp, &BTreeMap::new());
    assert_eq!(same_picture(&got, &want), None, "the first differing pixel");
    assert!(ops.iter().any(|o| matches!(o, DrawOp::Title { focused: true, .. })));
    let Button::Shape { pressed: close_down, .. } = h.comp.theme().close else { unreachable!() };
    assert!(ops.iter().any(|o| matches!(o, DrawOp::Shape { shape, .. } if *shape == close_down)), "the pressed close button is in the list");
    assert!(!h.comp.has_damage(), "a draw list takes the damage");
    // and again after the pointer moved and a window was dragged: the whole screen every time, the same as a full recompose
    h.comp.pointer_button(BTN_LEFT, false);
    h.comp.pointer_motion(-60, 30);
    h.comp.damage.add(Rect::new(0, 0, W, H));
    h.compose();
    let (epoch2, ops2) = h.comp.draw_list();
    assert_eq!(epoch2, 2);
    assert_eq!(same_picture(&raster(&ops2, &h.comp, &BTreeMap::new()), &h.screen), None);
}

#[test]
fn the_draw_list_clips_to_the_screen() {
    let mut h = gpu_h();
    let c = h.window(100, 50, 0x00FF_FF00);
    // drag the window half off the right edge
    let f = h.comp.window_frame(c, 4).unwrap();
    h.comp.pointer_motion(f.x + 10 - h.comp.pointer().0, f.y + 5 - h.comp.pointer().1);
    h.comp.pointer_button(BTN_LEFT, true);
    h.comp.pointer_motion(W - 60, 0);
    h.comp.pointer_button(BTN_LEFT, false);
    h.compose();
    let (_, ops) = h.comp.draw_list();
    let scr = Rect::new(0, 0, W, H);
    for op in &ops {
        let r = match op {
            DrawOp::Fill { rect, .. } => *rect,
            DrawOp::Gpu { dst, .. } | DrawOp::Cpu { dst, .. } => *dst,
            _ => continue,
        };
        assert_eq!(r.intersect(&scr), Some(r), "{:?} sticks out of the screen", op);
    }
    assert_eq!(same_picture(&raster(&ops, &h.comp, &BTreeMap::new()), &h.screen), None);
}

/// A GPU buffer for a client: its descriptor is the fake fd `fd`, ids: buffer `id`.
fn gpu_h() -> H_ {
    let mut h = H_::new();
    h.comp.enable_gpu_buffers();
    h
}

fn gpu_buffer(id: u32, fd: i32, w: i32, h: i32) -> Request {
    R::CreateGpuBuffer { id, fd, size: (w * h * 4) as u32, width: w, height: h, stride: w * 4, format: FORMAT_XRGB8888 }
}

fn import_of(ops: &[GpuOp], handle: u64) -> Option<&GpuOp> {
    ops.iter().find(|o| matches!(o, GpuOp::Import { handle: h, .. } if *h == handle))
}

#[test]
fn a_gpu_buffer_is_imported_by_the_host_and_is_not_a_pool() {
    let mut h = gpu_h();
    let c = h.comp.add_client();
    h.send(c, &[gpu_buffer(3, 77, 64, 32)]);
    assert_eq!(h.mapped_fds, Vec::<i32>::new(), "the pool callback is not asked to map it");
    assert_eq!(h.comp.take_fds_to_close(), Vec::<i32>::new(), "the host owns the descriptor now");
    assert_eq!(h.comp.take_gpu_ops(), vec![GpuOp::Import { handle: 1, fd: 77, size: 64 * 32 * 4, width: 64, height: 32, stride: 256 }]);
    assert_eq!(h.comp.take_gpu_ops(), vec![], "taken once");
    assert!(h.events_for(c).is_empty());
}

#[test]
fn a_gpu_window_is_drawn_from_its_buffer_and_not_released_at_commit() {
    let mut h = gpu_h();
    let c = h.comp.add_client();
    h.send(c, &[gpu_buffer(3, 77, 64, 32), R::CreateSurface { id: 4 }, R::Attach { surface: 4, buffer: 3 }, R::Commit { surface: 4 }]);
    assert_eq!(h.events_for(c), vec![Event::Configure { surface: 4, width: W / 2, height: H / 2 }, Event::Focus { surface: 4, focused: true }], "no release");
    assert!(h.comp.has_damage());
    let f = h.comp.window_frame(c, 4).unwrap();
    assert_eq!(f, Rect::new(40, 40, 64, 32 + TITLE_H));
    let (_, ops) = h.comp.draw_list();
    assert!(ops.contains(&DrawOp::Gpu { handle: 1, dst: Rect::new(40, 40 + TITLE_H, 64, 32), sx: 0, sy: 0 }));
    assert!(h.comp.cpu_content(c, 4).is_none(), "nothing was copied");
    // the picture, with the buffer's pixels
    let px: Vec<u32> = (0..64 * 32).map(|i| 0x0100_0000 + i as u32).collect();
    let mut gpu = BTreeMap::new();
    gpu.insert(1u64, (64, px.clone()));
    let pic = raster(&ops, &h.comp, &gpu);
    assert_eq!(pic[(40 + TITLE_H) as usize * STRIDE + 40], px[0] & 0x00FF_FFFF);
    assert_eq!(pic[(40 + TITLE_H + 31) as usize * STRIDE + 40 + 63], px[31 * 64 + 63] & 0x00FF_FFFF);
}

#[test]
fn a_replaced_gpu_buffer_is_released_only_when_the_frame_that_read_it_is_done() {
    let mut h = gpu_h();
    let c = h.comp.add_client();
    h.send(c, &[gpu_buffer(3, 77, 64, 32), gpu_buffer(5, 78, 64, 32), R::CreateSurface { id: 4 }, R::Attach { surface: 4, buffer: 3 }, R::Commit { surface: 4 }]);
    h.events_for(c);
    let (e1, _) = h.comp.draw_list(); // frame 1 reads buffer 3
    // the client moves on to buffer 5
    h.send(c, &[R::Attach { surface: 4, buffer: 5 }, R::Commit { surface: 4 }]);
    assert!(h.events_for(c).is_empty(), "frame {} may still be reading it", e1);
    let (e2, ops2) = h.comp.draw_list(); // frame 2 reads buffer 5
    assert!(ops2.iter().any(|o| matches!(o, DrawOp::Gpu { handle: 2, .. })));
    assert!(!ops2.iter().any(|o| matches!(o, DrawOp::Gpu { handle: 1, .. })));
    h.comp.gpu_frame_done(0);
    assert!(h.events_for(c).is_empty(), "nothing is done yet");
    h.comp.gpu_frame_done(e1);
    assert_eq!(h.events_for(c), vec![Event::Release { buffer: 3 }]);
    h.comp.gpu_frame_done(e1);
    h.comp.gpu_frame_done(e2);
    assert!(h.events_for(c).is_empty(), "once");
    // 3 is the client's again (its handle is dropped only when the object goes too: the surface let go, the client still has it)
    assert_eq!(h.comp.take_gpu_ops().iter().filter(|o| matches!(o, GpuOp::Drop { .. })).count(), 0);
}

#[test]
fn a_buffer_replaced_before_any_frame_was_started_is_released_at_once() {
    let mut h = gpu_h();
    let c = h.comp.add_client();
    h.send(c, &[gpu_buffer(3, 77, 64, 32), gpu_buffer(5, 78, 64, 32), R::CreateSurface { id: 4 }, R::Attach { surface: 4, buffer: 3 }, R::Commit { surface: 4 }]);
    h.events_for(c);
    h.send(c, &[R::Attach { surface: 4, buffer: 5 }, R::Commit { surface: 4 }]);
    assert_eq!(h.events_for(c), vec![Event::Release { buffer: 3 }], "no frame has read it: nothing to wait for");
}

#[test]
fn the_same_gpu_buffer_committed_again_stays_and_is_not_released() {
    let mut h = gpu_h();
    let c = h.comp.add_client();
    h.send(c, &[gpu_buffer(3, 77, 64, 32), R::CreateSurface { id: 4 }, R::Attach { surface: 4, buffer: 3 }, R::Commit { surface: 4 }]);
    h.comp.draw_list();
    h.events_for(c);
    h.send(c, &[R::Attach { surface: 4, buffer: 3 }, R::Damage { surface: 4, x: 0, y: 0, w: 8, h: 8 }, R::Commit { surface: 4 }]);
    assert!(h.comp.has_damage(), "a new frame of the same buffer repaints");
    let (e, _) = h.comp.draw_list();
    h.comp.gpu_frame_done(e);
    assert!(h.events_for(c).is_empty());
    assert_eq!(h.comp.take_gpu_ops().iter().filter(|o| matches!(o, GpuOp::Drop { .. })).count(), 0);
}

#[test]
fn a_destroyed_gpu_buffer_gets_no_release_and_is_dropped_when_replaced() {
    let mut h = gpu_h();
    let c = h.comp.add_client();
    h.send(c, &[gpu_buffer(3, 77, 64, 32), gpu_buffer(5, 78, 64, 32), R::CreateSurface { id: 4 }, R::Attach { surface: 4, buffer: 3 }, R::Commit { surface: 4 }]);
    h.events_for(c);
    let (e1, _) = h.comp.draw_list();
    h.send(c, &[R::DestroyBuffer { buffer: 3 }]);
    assert_eq!(h.events_for(c), vec![Event::DeleteId { id: 3 }]);
    assert_eq!(h.comp.take_gpu_ops().len(), 2, "two imports so far");
    // the window still shows it (the compositor keeps what it needs), until the client commits another
    let (_, ops) = h.comp.draw_list();
    assert!(ops.iter().any(|o| matches!(o, DrawOp::Gpu { handle: 1, .. })));
    h.send(c, &[R::Attach { surface: 4, buffer: 5 }, R::Commit { surface: 4 }]);
    // the id 3 may be a new object by now: a release for the old one would be a lie
    h.send(c, &[gpu_buffer(3, 79, 8, 8)]);
    h.events_for(c);
    h.comp.gpu_frame_done(e1 + 1);
    assert!(h.events_for(c).is_empty(), "no release for a buffer the client destroyed");
    let ops = h.comp.take_gpu_ops();
    assert!(ops.contains(&GpuOp::Drop { handle: 1 }), "the old buffer's last reference went with the frame: {:?}", ops);
    assert!(import_of(&ops, 3).is_some());
}

#[test]
fn a_disconnected_client_drops_every_buffer_and_gets_nothing() {
    let mut h = gpu_h();
    let c = h.comp.add_client();
    h.send(c, &[gpu_buffer(3, 77, 64, 32), gpu_buffer(5, 78, 64, 32), R::CreateSurface { id: 4 }, R::Attach { surface: 4, buffer: 3 }, R::Commit { surface: 4 }]);
    h.comp.draw_list();
    h.send(c, &[R::Attach { surface: 4, buffer: 5 }, R::Commit { surface: 4 }]); // 3 waits for frame 1
    h.comp.take_gpu_ops();
    h.events_for(c);
    h.comp.remove_client(c);
    let mut drops: Vec<u64> = h.comp.take_gpu_ops().into_iter().filter_map(|o| if let GpuOp::Drop { handle } = o { Some(handle) } else { None }).collect();
    drops.sort();
    assert_eq!(drops, vec![1, 2], "both buffers, the retired one too");
    h.comp.gpu_frame_done(1);
    assert!(h.comp.take_events().is_empty(), "nobody to tell");
    assert!(h.comp.draw_list().1.iter().all(|o| !matches!(o, DrawOp::Gpu { .. })));
}

#[test]
fn unmapping_and_replacing_with_a_pool_buffer_retire_the_gpu_buffer() {
    let mut h = gpu_h();
    let c = h.comp.add_client();
    h.send(c, &[gpu_buffer(3, 77, 64, 32), R::CreateSurface { id: 4 }, R::Attach { surface: 4, buffer: 3 }, R::Commit { surface: 4 }]);
    let (e1, _) = h.comp.draw_list();
    h.events_for(c);
    h.pool(200, 64 * 32 * 4);
    h.draw(200, 0, 256, 0, 0, 64, 32, 0x0033_4455);
    h.send(c, &[
        R::CreatePool { id: 6, fd: 200, size: 64 * 32 * 4 },
        R::CreateBuffer { pool: 6, id: 7, offset: 0, width: 64, height: 32, stride: 256, format: FORMAT_XRGB8888 },
        R::Attach { surface: 4, buffer: 7 },
        R::Commit { surface: 4 },
    ]);
    assert_eq!(h.events_for(c), vec![Event::Release { buffer: 7 }], "the pool buffer is copied and released as always; the GPU one waits for frame 1");
    let (_, ops) = h.comp.draw_list();
    assert!(ops.iter().any(|o| matches!(o, DrawOp::Cpu { client, surface: 4, w: 64, h: 32, .. } if *client == c)));
    assert!(!ops.iter().any(|o| matches!(o, DrawOp::Gpu { .. })));
    assert_eq!(h.comp.cpu_content(c, 4).unwrap()[0], 0x0033_4455);
    h.comp.gpu_frame_done(e1);
    assert_eq!(h.events_for(c), vec![Event::Release { buffer: 3 }]);
    // a null attach unmaps and retires too
    h.send(c, &[gpu_buffer(8, 80, 64, 32), R::Attach { surface: 4, buffer: 8 }, R::Commit { surface: 4 }]);
    let (e3, _) = h.comp.draw_list();
    h.send(c, &[R::Attach { surface: 4, buffer: 0 }, R::Commit { surface: 4 }]);
    assert!(h.events_for(c).is_empty());
    h.comp.gpu_frame_done(e3);
    assert_eq!(h.events_for(c), vec![Event::Release { buffer: 8 }]);
    assert!(h.comp.window_frame(c, 4).is_none() || !h.comp.draw_list().1.iter().any(|o| matches!(o, DrawOp::Gpu { .. })));
}

#[test]
fn a_bad_gpu_buffer_is_a_protocol_error_and_the_descriptor_is_closed() {
    for (req, what) in [
        (R::CreateGpuBuffer { id: 3, fd: 90, size: 4096, width: 64, height: 32, stride: 256, format: 0 }, "format"),
        (R::CreateGpuBuffer { id: 3, fd: 90, size: 64 * 32 * 4 - 1, width: 64, height: 32, stride: 256, format: 1 }, "descriptor too small"),
        (R::CreateGpuBuffer { id: 3, fd: 90, size: 1 << 20, width: 64, height: 32, stride: 255, format: 1 }, "stride below the row"),
        (R::CreateGpuBuffer { id: 3, fd: 90, size: 1 << 20, width: 64, height: 32, stride: 258, format: 1 }, "stride not a multiple of 4"),
        (R::CreateGpuBuffer { id: 3, fd: 90, size: 1 << 20, width: 0, height: 32, stride: 256, format: 1 }, "width 0"),
        (R::CreateGpuBuffer { id: 3, fd: 90, size: u32::MAX, width: 9000, height: 32, stride: 36000, format: 1 }, "too wide"),
        (R::CreateGpuBuffer { id: 1, fd: 90, size: 1 << 20, width: 64, height: 32, stride: 256, format: 1 }, "id 1"),
    ] {
        let mut h = gpu_h();
        let c = h.comp.add_client();
        h.send(c, &[req]);
        let evs = h.comp.take_events();
        assert!(evs.iter().any(|(k, e)| *k == c && matches!(e, Event::Error { .. })), "{}: an error event", what);
        assert_eq!(h.comp.take_disconnects(), vec![c], "{}", what);
        assert!(h.comp.take_fds_to_close().contains(&90) || h.comp.take_gpu_ops().iter().any(|o| matches!(o, GpuOp::Import { fd: 90, .. })), "{}: the descriptor goes to the host or is closed", what);
    }
    // an id in use: the import was queued, so it is dropped again (pairs)
    let mut h = gpu_h();
    let c = h.comp.add_client();
    h.send(c, &[gpu_buffer(3, 77, 8, 8)]);
    h.comp.take_gpu_ops();
    h.send(c, &[gpu_buffer(3, 78, 8, 8)]);
    assert_eq!(h.comp.take_disconnects(), vec![c]);
    let ops = h.comp.take_gpu_ops();
    assert!(import_of(&ops, 2).is_some() && ops.contains(&GpuOp::Drop { handle: 2 }) && ops.contains(&GpuOp::Drop { handle: 1 }), "{:?}", ops);
}

#[test]
fn gpu_and_pool_windows_share_the_screen_in_stacking_order() {
    let mut h = gpu_h();
    let a = h.window(100, 50, 0x0012_3456);
    let b = h.comp.add_client();
    h.send(b, &[gpu_buffer(3, 77, 100, 50), R::CreateSurface { id: 4 }, R::Attach { surface: 4, buffer: 3 }, R::Commit { surface: 4 }]);
    let (_, ops) = h.comp.draw_list();
    let cpu_at = ops.iter().position(|o| matches!(o, DrawOp::Cpu { .. })).unwrap();
    let gpu_at = ops.iter().position(|o| matches!(o, DrawOp::Gpu { .. })).unwrap();
    assert!(cpu_at < gpu_at, "the pool window is below the GPU window that was mapped after it");
    // raising the first window flips them
    let fa = h.comp.window_frame(a, 4).unwrap();
    h.comp.pointer_motion(fa.x + 3 - h.comp.pointer().0, fa.y + 3 - h.comp.pointer().1);
    h.comp.pointer_button(BTN_LEFT, true);
    h.comp.pointer_button(BTN_LEFT, false);
    let (_, ops) = h.comp.draw_list();
    let cpu_at = ops.iter().position(|o| matches!(o, DrawOp::Cpu { .. })).unwrap();
    let gpu_at = ops.iter().position(|o| matches!(o, DrawOp::Gpu { .. })).unwrap();
    assert!(gpu_at < cpu_at);
}

#[test]
fn a_maximized_window_waiting_for_its_client_shows_the_rest_as_window_background() {
    // the frame is bigger than the buffer until the client answers: the draw list fills the rest like compose does
    let mut h = gpu_h();
    let c = h.window(100, 50, 0x0012_3456);
    h.send(c, &[R::SetResizable { surface: 4, min_w: 1, min_h: 1 }]);
    click(&mut h, 140 - TITLE_H - TITLE_H / 2, 50);
    assert!(h.comp.is_maximized(c, 4));
    h.compose();
    let (_, ops) = h.comp.draw_list();
    assert!(ops.iter().any(|o| matches!(o, DrawOp::Fill { color: WINDOW_BG, .. })), "the part the buffer does not cover");
    assert_eq!(same_picture(&raster(&ops, &h.comp, &BTreeMap::new()), &h.screen), None);
}

#[test]
fn a_window_dragged_past_the_left_and_top_edges_is_drawn_from_the_inside_of_its_buffer() {
    let mut h = gpu_h();
    let c = h.window(100, 50, 0x0012_3456);
    let g = h.comp.add_client();
    h.send(g, &[gpu_buffer(3, 77, 100, 50), R::CreateSurface { id: 4 }, R::Attach { surface: 4, buffer: 3 }, R::Commit { surface: 4 }]);
    // both windows to the top-left, past the edges: the GPU one is on top, take its bar and carry it to (-30, -10) - the pool one the same
    for (client, key) in [(g, 4u32), (c, 4u32)] {
        let f = h.comp.window_frame(client, key).unwrap();
        h.comp.pointer_motion(f.x + 10 - h.comp.pointer().0, f.y + 5 - h.comp.pointer().1);
        h.comp.pointer_button(BTN_LEFT, true);
        h.comp.pointer_motion(-40 - f.x + 0, -(f.y + 5) + 5 - 20);
        h.comp.pointer_button(BTN_LEFT, false);
    }
    h.compose();
    let (_, ops) = h.comp.draw_list();
    let cut = ops.iter().filter(|o| matches!(o, DrawOp::Gpu { sx, sy, .. } | DrawOp::Cpu { sx, sy, .. } if *sx > 0 || *sy > 0)).count();
    assert!(cut >= 1, "a window cut by the screen's edge starts inside its buffer: {:?}", ops.iter().filter(|o| matches!(o, DrawOp::Gpu { .. } | DrawOp::Cpu { .. })).collect::<Vec<_>>());
    let mut gpu = BTreeMap::new();
    gpu.insert(1u64, (100, (0..100 * 50).map(|i| 0x0200_0000 + i as u32).collect::<Vec<u32>>()));
    let pic = raster(&ops, &h.comp, &gpu);
    // the CPU window's pixels are right wherever they landed (the oracle compares them to compose); the GPU window's are the buffer's, shifted
    let f = h.comp.window_content(g, 4).unwrap();
    let (x, y) = (f.x.max(0), f.y.max(0));
    assert_eq!(pic[y as usize * STRIDE + x as usize], ((y - f.y) * 100 + (x - f.x)) as u32, "(the top byte is not shown: XRGB)");
}

#[test]
fn a_draw_list_takes_the_damage_and_a_gpu_commit_without_damage_still_asks_for_a_frame() {
    let mut h = gpu_h();
    let c = h.comp.add_client();
    h.send(c, &[gpu_buffer(3, 77, 64, 32), R::CreateSurface { id: 4 }, R::Attach { surface: 4, buffer: 3 }, R::Commit { surface: 4 }]);
    assert!(h.comp.has_damage());
    h.comp.draw_list();
    assert!(!h.comp.has_damage(), "taken");
    // no `damage` request at all: the host reads the whole buffer, so the whole content must be redrawn
    h.send(c, &[gpu_buffer(5, 78, 64, 32), R::Attach { surface: 4, buffer: 5 }, R::Commit { surface: 4 }]);
    assert!(h.comp.has_damage());
    assert_eq!(h.comp.damage().rects().len(), 1);
    assert_eq!(h.comp.damage().rects()[0], Rect::new(40, 40 + TITLE_H, 64, 32));
}

#[test]
fn a_pool_windows_version_moves_with_each_commit() {
    let mut h = gpu_h();
    let c = h.window(60, 30, 0x0011_1111);
    let v = |h: &mut H_| h.comp.draw_list().1.iter().find_map(|o| if let DrawOp::Cpu { version, .. } = o { Some(*version) } else { None }).unwrap();
    let v1 = v(&mut h);
    assert_eq!(v(&mut h), v1, "nothing committed: the same pixels");
    h.send(c, &[R::Attach { surface: 4, buffer: 3 }, R::Damage { surface: 4, x: 0, y: 0, w: 4, h: 4 }, R::Commit { surface: 4 }]);
    assert!(v(&mut h) > v1);
}

#[test]
fn the_host_telling_frames_done_out_of_order_never_goes_back() {
    let mut h = gpu_h();
    let c = h.comp.add_client();
    h.send(c, &[gpu_buffer(3, 77, 8, 8), gpu_buffer(5, 78, 8, 8), gpu_buffer(6, 79, 8, 8), R::CreateSurface { id: 4 }, R::Attach { surface: 4, buffer: 3 }, R::Commit { surface: 4 }]);
    h.comp.draw_list();
    let (e2, _) = h.comp.draw_list();
    h.comp.gpu_frame_done(e2);
    h.comp.gpu_frame_done(1); // a late report of an older frame
    h.events_for(c);
    h.send(c, &[R::Attach { surface: 4, buffer: 5 }, R::Commit { surface: 4 }]);
    assert_eq!(h.events_for(c), vec![Event::Release { buffer: 3 }], "frame 2 was done and nobody started another: nothing to wait for");
}

#[test]
fn every_windows_title_op_says_whether_it_is_focused() {
    let mut h = gpu_h();
    let a = h.window(100, 50, 0x0012_3456);
    let b = h.window(100, 50, 0x0065_4321);
    h.send(a, &[R::SetTitle { surface: 4, title: "a".into() }]);
    h.send(b, &[R::SetTitle { surface: 4, title: "b".into() }]);
    let (_, ops) = h.comp.draw_list();
    let t: Vec<(String, bool)> = ops.iter().filter_map(|o| if let DrawOp::Title { title, focused, .. } = o { Some((title.clone(), *focused)) } else { None }).collect();
    assert_eq!(t, vec![(String::from("a"), false), (String::from("b"), true)], "bottom to top: b was mapped last and has the focus");
}

#[test]
fn a_compositor_that_paints_on_the_cpu_refuses_gpu_buffers() {
    let mut h = H_::new();
    let c = h.comp.add_client();
    h.send(c, &[gpu_buffer(3, 77, 64, 32)]);
    assert!(h.comp.take_events().iter().any(|(k, e)| *k == c && matches!(e, Event::Error { code, .. } if *code == ErrorCode::InvalidMethod as u32)));
    assert_eq!(h.comp.take_disconnects(), vec![c]);
    assert_eq!(h.comp.take_fds_to_close(), vec![77], "the descriptor is closed, not leaked");
    assert_eq!(h.comp.take_gpu_ops(), vec![]);
}

#[test]
fn the_cpu_painter_survives_a_gpu_window() {
    // a host that enabled GPU buffers and still calls compose() must not index an empty store
    let mut h = gpu_h();
    let c = h.comp.add_client();
    h.send(c, &[gpu_buffer(3, 77, 64, 32), R::CreateSurface { id: 4 }, R::Attach { surface: 4, buffer: 3 }, R::Commit { surface: 4 }]);
    h.compose();
    assert_eq!(h.px(40, 40 + TITLE_H), WINDOW_BG, "nothing to show but the background");
}

// A: F11 makes the focused resizable window cover the whole screen without its bar, asks its client for the screen's size, and the second
// press puts everything back. The key is the compositor's: the client never sees it.
#[test]
fn f11_toggles_fullscreen_and_the_client_never_sees_the_key() {
    let mut h = H_::new();
    let c = h.window(100, 50, 0x0012_3456);
    h.send(c, &[R::SetResizable { surface: 4, min_w: 1, min_h: 1 }]);
    h.comp.take_events();
    h.comp.key(87, true);
    h.comp.key(87, false);
    assert!(h.comp.is_fullscreen(c, 4));
    assert_eq!(h.comp.window_frame(c, 4), Some(Rect::new(0, 0, W, H)), "the whole screen, no title bar");
    assert_eq!(h.comp.window_content(c, 4), Some(Rect::new(0, 0, W, H)));
    assert_eq!(h.comp.stack().last(), Some(&(c, 4)));
    let evs = h.events_for(c);
    assert_eq!(resizes(&evs), vec![(W, H)]);
    assert!(!evs.iter().any(|e| matches!(e, Event::Key { .. })), "F11 is not the client's: {evs:?}");
    h.comp.key(87, true);
    assert!(!h.comp.is_fullscreen(c, 4));
    assert_eq!(h.comp.window_frame(c, 4), Some(Rect::new(40, 40, 100, 70)));
    assert_eq!(resizes(&h.events_for(c)), Vec::<(i32, i32)>::new(), "back to the buffer's own size: nothing to ask");
    // other keys still reach it
    h.comp.key(30, true);
    assert_eq!(h.events_for(c), vec![Event::Key { surface: 4, code: 30, pressed: true }]);
}

// B: a window that did not ask to be resizable is left alone, as with maximize; and nothing is focused, nothing happens.
#[test]
fn f11_needs_a_resizable_window() {
    let mut h = H_::new();
    h.comp.key(87, true); // no window at all
    let c = h.window(100, 50, 0x0012_3456);
    h.comp.take_events();
    h.comp.key(87, true);
    assert!(!h.comp.is_fullscreen(c, 4));
    assert_eq!(h.comp.window_frame(c, 4), Some(Rect::new(40, 40, 100, 70)));
    assert!(resizes(&h.events_for(c)).is_empty());
}

// A: leaving fullscreen restores a maximized window as maximized (its own restore geometry kept), and a fullscreen window has no resize grip
// in its corner (a game's corner is the game's) and its pointer events are all content.
#[test]
fn leaving_fullscreen_restores_maximized_and_a_fullscreen_window_has_no_grip() {
    let mut h = H_::new();
    let c = h.window(100, 50, 0x0012_3456);
    h.send(c, &[R::SetResizable { surface: 4, min_w: 1, min_h: 1 }]);
    h.comp.key(87, true); // placed -> maximized would be a button; here straight to fullscreen from placed
    h.comp.key(87, true);
    let max_btn = (140 - TITLE_H - TITLE_H / 2, 50);
    click(&mut h, max_btn.0, max_btn.1);
    assert!(h.comp.is_maximized(c, 4));
    h.comp.take_events();
    h.comp.key(87, true);
    assert!(h.comp.is_fullscreen(c, 4));
    assert!(!h.comp.is_maximized(c, 4), "fullscreen is not the work area");
    assert_eq!(h.comp.hit(W - 2, H - 2), Some(((c, 4), Zone::Content)), "no grip");
    assert_eq!(h.comp.hit(5, 5), Some(((c, 4), Zone::Content)), "no title bar");
    h.comp.key(87, true);
    assert!(h.comp.is_maximized(c, 4), "maximized again");
    assert_eq!(h.comp.window_frame(c, 4), Some(Rect::new(0, 0, W, H)));
    assert_eq!(h.comp.hit(5, 5), Some(((c, 4), Zone::Title)), "its bar is back");
    assert_eq!(resizes(&h.events_for(c)).last(), Some(&(W, H - TITLE_H)), "its client is asked for the maximized size again");
}

// ── themes (step 2 of docs/gui/compositor-visual-plan.md) ─────────────────────────────────────────────────────────────────────────────

/// Where the pixels of windows go, and the titles' boxes: what a look must not move.
fn geometry(ops: &[DrawOp]) -> Vec<(u8, Rect, i32, i32)> {
    ops.iter()
        .filter_map(|o| match o {
            DrawOp::Gpu { dst, sx, sy, .. } => Some((0, *dst, *sx, *sy)),
            DrawOp::Cpu { dst, sx, sy, .. } => Some((1, *dst, *sx, *sy)),
            DrawOp::Title { area, clip, .. } => Some((2, *area, clip.x, clip.w)),
            _ => None,
        })
        .collect()
}

/// A frame shape's two halves as the draw list draws them: its shadow alone (no fill, no border, square) and its ring (square, no shadow).
fn frame_parts(f: Shape) -> (Shape, Shape) {
    (Shape { c: [0; 4], border: 0.0, radius: 0.0, ..f }, Shape { radius: 0.0, shadow_color: 0, ..f })
}

fn shapes(ops: &[DrawOp]) -> Vec<(Rect, Shape)> {
    ops.iter().filter_map(|o| if let DrawOp::Shape { rect, shape, .. } = o { Some((*rect, *shape)) } else { None }).collect()
}

// A: every look puts the windows' pixels and titles exactly where the flat one does, so switching is only a matter of pixels; and the
// hit boxes stay too (a click on the close button's square closes in every look).
#[test]
fn a_theme_moves_no_window() {
    let mut h = gpu_h();
    let a = h.window(120, 60, 0x00AA_0000);
    let b = h.window(100, 80, 0x0000_AA00);
    h.send(b, &[R::SetResizable { surface: 4, min_w: 50, min_h: 30 }]);
    let _ = a;
    let flat = geometry(&h.comp.draw_list().1);
    assert!(flat.len() >= 4);
    for t in theme::THEMES {
        h.comp.set_theme(t);
        assert_eq!(geometry(&h.comp.draw_list().1), flat, "theme {}", t.name);
        assert_eq!(h.comp.title_height(), TITLE_H, "theme {}", t.name);
    }
}

// A: Luna draws a window as frame (with its shadow), then the bar, then the title text, then the buttons, then the content, all above the
// desktop's gradient; the frame reaches past the window by frame_w, the bar's box reaches under the content so only its top corners round.
#[test]
fn luna_draws_frame_bar_title_buttons_then_content() {
    let mut h = gpu_h();
    let c = h.window(120, 60, 0x00AA_0000);
    h.send(c, &[R::SetResizable { surface: 4, min_w: 50, min_h: 30 }]);
    h.comp.set_theme(&theme::LUNA);
    let ops = h.comp.draw_list().1;
    let f = h.comp.window_frame(c, 4).unwrap();
    assert_eq!(ops[0], DrawOp::Shape { rect: Rect::new(0, 0, W, H), shape: theme::LUNA.background, clip: None }, "the desktop first");
    let sh = shapes(&ops);
    let fw = theme::LUNA.frame_w;
    let (shadow, ring) = frame_parts(theme::LUNA.frame[0]);
    let outer = Rect::new(f.x - fw, f.y - fw, f.w + 2 * fw, f.h + 2 * fw);
    assert_eq!(sh[1], (outer, shadow), "the focused window's shadow");
    assert!(shadow.shadow_color >> 24 != 0);
    assert_eq!(sh[2], (outer, ring), "its frame");
    assert_eq!(ops[2], DrawOp::Shape { rect: outer, shape: ring, clip: Some(Rect::new(outer.x, f.y + TITLE_H, outer.w, outer.h - fw - TITLE_H)) }, "below the bar");
    let bar = theme::LUNA.title[0];
    assert_eq!(sh[3].1, bar);
    assert_eq!(sh[3].0, Rect::new(f.x - fw, f.y - fw, f.w + 2 * fw, fw + TITLE_H + bar.radius as i32 + 1));
    let close = match theme::LUNA.close { Button::Shape { normal, .. } => normal, _ => unreachable!() };
    let max = match theme::LUNA.other { Button::Shape { normal, .. } => normal, _ => unreachable!() };
    let i = theme::LUNA.button_inset;
    assert_eq!(sh[4], (Rect::new(f.right() - TITLE_H + i, f.y + i, TITLE_H - 2 * i, TITLE_H - 2 * i), close), "the close button, inset");
    assert_eq!(sh[5], (Rect::new(f.right() - 2 * TITLE_H + i, f.y + i, TITLE_H - 2 * i, TITLE_H - 2 * i), max));
    let pos = |pred: &dyn Fn(&DrawOp) -> bool| ops.iter().position(|o| pred(o)).unwrap();
    let title_at = pos(&|o| matches!(o, DrawOp::Title { .. }));
    let content_at = pos(&|o| matches!(o, DrawOp::Cpu { .. }));
    let bar_at = pos(&|o| matches!(o, DrawOp::Shape { shape, .. } if *shape == bar));
    let close_at = pos(&|o| matches!(o, DrawOp::Shape { shape, .. } if *shape == close));
    assert!(bar_at < title_at && title_at < close_at && close_at < content_at, "{bar_at} {title_at} {close_at} {content_at}");
    match &ops[title_at] {
        DrawOp::Title { fg, shadow, focused: true, .. } => assert_eq!((*fg, *shadow), (theme::LUNA.title_fg[0], theme::LUNA.title_shadow)),
        o => panic!("{o:?}"),
    }
    assert!(ops.iter().any(|o| matches!(o, DrawOp::Fill { color, .. } if *color == theme::LUNA.glyph)), "the glyphs are in the theme's colour");
    // pressing the close button shows the pressed shape; the button still closes on release over it (the hit box is the flat one's)
    h.comp.pointer_motion(f.right() - TITLE_H / 2 - h.comp.pointer().0, f.y + TITLE_H / 2 - h.comp.pointer().1);
    h.comp.pointer_button(BTN_LEFT, true);
    let pressed = match theme::LUNA.close { Button::Shape { pressed, .. } => pressed, _ => unreachable!() };
    assert!(shapes(&h.comp.draw_list().1).iter().any(|(_, s)| *s == pressed));
    h.comp.take_events();
    h.comp.pointer_button(BTN_LEFT, false);
    assert!(h.events_for(c).iter().any(|e| matches!(e, Event::Close { .. })), "the close button closes in Luna too");
}

// A: a second window: the one below is drawn unfocused (the other frame, bar and title colour), and entirely before the one on top.
#[test]
fn luna_draws_the_unfocused_window_below_in_its_own_colours() {
    let mut h = gpu_h();
    let _a = h.window(120, 60, 0x00AA_0000);
    let _b = h.window(100, 80, 0x0000_AA00);
    h.comp.set_theme(&theme::LUNA);
    let ops = h.comp.draw_list().1;
    let rings = theme::LUNA.frame.map(|f| frame_parts(f).1);
    let frames: Vec<Shape> = shapes(&ops).iter().map(|(_, s)| *s).filter(|s| rings.contains(s)).collect();
    assert_eq!(frames, vec![rings[1], rings[0]], "unfocused below, focused on top");
    let titles: Vec<(bool, u32)> = ops.iter().filter_map(|o| if let DrawOp::Title { focused, fg, .. } = o { Some((*focused, *fg)) } else { None }).collect();
    assert_eq!(titles, vec![(false, theme::LUNA.title_fg[1]), (true, theme::LUNA.title_fg[0])]);
}

// A: 9x's buttons are bevels, opaque fills: light top-left and dark bottom-right, swapped while pressed.
#[test]
fn nines_buttons_are_bevels_that_swap_when_pressed() {
    let mut h = gpu_h();
    let c = h.window(120, 60, 0x00AA_0000);
    h.comp.set_theme(&theme::NINES);
    let Button::Bevel { face, light, dark } = theme::NINES.close else { unreachable!() };
    let f = h.comp.window_frame(c, 4).unwrap();
    let i = theme::NINES.button_inset;
    let b = Rect::new(f.right() - TITLE_H + i, f.y + i, TITLE_H - 2 * i, TITLE_H - 2 * i);
    let bevel = |ops: &[DrawOp]| -> Vec<(Rect, u32)> {
        ops.iter().filter_map(|o| if let DrawOp::Fill { rect, color } = o { Some((*rect, *color)) } else { None }).filter(|(r, _)| r.x >= b.x && r.right() <= b.right() && r.y >= b.y && r.bottom() <= b.bottom() && r.w > 4).take(3).collect()
    };
    let up = bevel(&h.comp.draw_list().1);
    assert_eq!(up, vec![(b, dark), (Rect::new(b.x, b.y, b.w - 1, b.h - 1), light), (Rect::new(b.x + 1, b.y + 1, b.w - 2, b.h - 2), face)]);
    h.comp.pointer_motion(b.x + 2 - h.comp.pointer().0, b.y + 2 - h.comp.pointer().1);
    h.comp.pointer_button(BTN_LEFT, true);
    let down = bevel(&h.comp.draw_list().1);
    assert_eq!((down[0].1, down[1].1, down[2].1), (light, dark, face));
    assert!(shapes(&h.comp.draw_list().1).iter().any(|(_, s)| s.horizontal), "the title runs left to right");
}

// A: F12 is the compositor's: it cycles luna (the default) -> 9x -> luna, damages the whole screen, and no client sees it.
#[test]
fn f12_cycles_the_themes_and_the_client_never_sees_it() {
    let mut h = gpu_h();
    let c = h.window(100, 50, 0x0012_3456);
    let _ = h.comp.draw_list();
    h.comp.take_events();
    let mut seen = vec![];
    for _ in 0..3 {
        h.comp.key(88, true);
        h.comp.key(88, false);
        assert!(h.comp.damage().contains(0, 0) && h.comp.damage().contains(W - 1, H - 1), "the whole screen is damaged");
        let _ = h.comp.draw_list();
        seen.push(h.comp.theme().name);
    }
    assert_eq!(seen, vec!["9x", "luna", "9x"]);
    assert!(!h.events_for(c).iter().any(|e| matches!(e, Event::Key { .. })), "F12 is not the client's");
    assert_eq!(theme::by_name("luna").map(|t| t.name), Some("luna"));
    assert!(theme::by_name("aqua").is_none());
}

// A: a fullscreen window has no decorations in any look: only the desktop's shape (under it) and its pixels.
#[test]
fn a_fullscreen_window_has_no_shapes() {
    let mut h = gpu_h();
    let c = h.window(100, 50, 0x0012_3456);
    h.send(c, &[R::SetResizable { surface: 4, min_w: 1, min_h: 1 }]);
    h.comp.key(87, true);
    assert!(h.comp.is_fullscreen(c, 4));
    for t in [&theme::LUNA, &theme::NINES] {
        h.comp.set_theme(t);
        let ops = h.comp.draw_list().1;
        assert_eq!(shapes(&ops).len(), 1, "{}: only the background", t.name);
        assert!(!ops.iter().any(|o| matches!(o, DrawOp::Title { .. })));
    }
}

// A: scale 2 (a 1080p screen) doubles a theme's lengths, not its colours.
#[test]
fn shapes_scale_with_the_screen() {
    let s = theme::LUNA.frame[0];
    let d = s.scaled(2);
    assert_eq!((d.radius, d.border, d.shadow_blur, d.shadow_dy), (2.0 * s.radius, 2.0 * s.border, 2.0 * s.shadow_blur, 2 * s.shadow_dy));
    assert_eq!((d.c, d.split, d.shadow_color), (s.c, s.split, s.shadow_color));
    let g = theme::LUNA.taskbar.bar.scaled(2);
    assert_eq!(g.backdrop_blur, 2.0 * theme::LUNA.taskbar.bar.backdrop_blur, "the glass's blur too");
    assert!(theme::LUNA.menu.frame.backdrop_blur > 0.0 && theme::NINES.taskbar.bar.backdrop_blur == 0.0, "Luna is glass, 9x is not");
}

// A: on a 1080p screen (scale 2) every length of the look doubles where the draw list uses it: the frame's reach and shape, the bar, the
// buttons' inset, the desktop's shape.
#[test]
fn a_theme_at_scale_2() {
    let mut h = gpu_h();
    h.comp = Compositor::new(640, 1080);
    h.comp.enable_gpu_buffers();
    let c = h.window(120, 60, 0x00AA_0000);
    h.comp.set_theme(&theme::LUNA);
    assert_eq!(h.comp.title_height(), 2 * TITLE_H);
    let f = h.comp.window_frame(c, 4).unwrap();
    let sh = shapes(&h.comp.draw_list().1);
    let fw = 2 * theme::LUNA.frame_w;
    assert_eq!(sh[0].1, theme::LUNA.background.scaled(2));
    let (shadow, ring) = frame_parts(theme::LUNA.frame[0].scaled(2));
    assert_eq!(sh[1], (Rect::new(f.x - fw, f.y - fw, f.w + 2 * fw, f.h + 2 * fw), shadow));
    assert_eq!((sh[1].1.shadow_blur, sh[2].1.border), (28.0, 6.0));
    assert_eq!(sh[2].1, ring);
    assert_eq!(sh[3].1, theme::LUNA.title[0].scaled(2));
    assert_eq!(sh[3].1.radius, 16.0);
    let (th, i) = (2 * TITLE_H, 2 * theme::LUNA.button_inset);
    assert_eq!(sh[4].0, Rect::new(f.right() - th + i, f.y + i, th - 2 * i, th - 2 * i));
}

// ── the taskbar's look and premultiplied windows ──────────────────────────────────────────────────────────────────────────────────────

/// A client whose surface 4 takes the panel role, `ph` pixels tall, with a `W x ph` premultiplied `ARGB8888` buffer of `color`.
fn panel_argb(h: &mut H_, ph: i32, color: u32) -> ClientId {
    let c = h.comp.add_client();
    let fd = 100 + c as i32;
    let size = (W * ph * 4) as usize;
    h.pool(fd, size);
    h.draw(fd, 0, W as usize * 4, 0, 0, W as usize, ph as usize, color);
    h.send(c, &[R::CreateSurface { id: 4 }, R::SetPanel { surface: 4, height: ph }]);
    h.send(c, &[
        R::CreatePool { id: 2, fd, size: size as u32 },
        R::CreateBuffer { pool: 2, id: 3, offset: 0, width: W, height: ph, stride: W * 4, format: crate::protocol::FORMAT_ARGB8888 },
        R::Attach { surface: 4, buffer: 3 },
        R::Damage { surface: 4, x: 0, y: 0, w: W, h: ph },
        R::Commit { surface: 4 },
    ]);
    c
}

// A: a window whose buffer is ARGB8888 is drawn "over" what is under it, by compose (exact /255 rounding) and in the draw list (premul,
// the same picture as compose: the oracle), and goes back to opaque when an XRGB8888 buffer replaces it.
#[test]
fn an_argb_window_is_drawn_over_what_is_under_it() {
    let mut h = gpu_h();
    let _a = h.window(120, 60, 0x00AA_0000);
    let b = h.comp.add_client();
    h.pool(300, 160 * 40 * 4);
    h.draw(300, 0, 160 * 4, 0, 0, 160, 40, 0x8000_4000); // green at half alpha, premultiplied
    h.send(b, &[
        R::CreatePool { id: 2, fd: 300, size: 160 * 40 * 4 },
        R::CreateBuffer { pool: 2, id: 3, offset: 0, width: 160, height: 40, stride: 160 * 4, format: crate::protocol::FORMAT_ARGB8888 },
        R::CreateSurface { id: 4 },
        R::Attach { surface: 4, buffer: 3 },
        R::Commit { surface: 4 },
    ]);
    let fb = h.comp.window_content(b, 4).unwrap();
    let fa = h.comp.window_content(_a, 4).unwrap();
    h.compose();
    // over a's red, and (to its right, past a) over the desktop
    let (x_in, y_in) = (fb.x + 2, fb.y + 2);
    assert!(fa.contains(x_in, y_in));
    assert_eq!(h.px(x_in, y_in) & 0x00FF_FFFF, theme::over(0x00AA_0000, 0x8000_4000) & 0x00FF_FFFF);
    assert_eq!(h.px(x_in, y_in) & 0x00FF_FFFF, 0x0055_4000, "half of a's red under half-alpha green");
    // past a, its frame and its shadow: over the desktop (and not over b's own frame: that is a ring around it)
    let x_out = fa.right() + 30;
    assert!(x_out < fb.right());
    assert_eq!(h.px(x_out, y_in) & 0x00FF_FFFF, theme::over(h.desktop(x_out, y_in), 0x8000_4000) & 0x00FF_FFFF);
    let want = h.screen.clone();
    let (_, ops) = h.comp.draw_list();
    assert!(ops.iter().any(|o| matches!(o, DrawOp::Cpu { client, premul: true, .. } if *client == b)));
    assert_eq!(same_picture(&raster(&ops, &h.comp, &BTreeMap::new()), &want), None, "the draw list is compose's picture");
    // an XRGB buffer of the same size: opaque again (the top byte ignored)
    h.send(b, &[
        R::CreateBuffer { pool: 2, id: 5, offset: 0, width: 160, height: 40, stride: 160 * 4, format: FORMAT_XRGB8888 },
        R::Attach { surface: 4, buffer: 5 },
        R::Commit { surface: 4 },
    ]);
    h.compose();
    assert_eq!(h.px(x_in, y_in), 0x0000_4000, "XRGB: the pixel as it is, its top byte ignored");
    assert!(h.comp.draw_list().1.iter().any(|o| matches!(o, DrawOp::Cpu { client, premul: false, .. } if *client == b)));
}

// A: the panel is told the look when it takes the role and whenever it changes (F12 included); no other client is.
#[test]
fn the_panel_is_told_the_theme() {
    let mut h = gpu_h();
    let w = h.window(100, 50, 0x00AA_0000);
    let p = panel_argb(&mut h, 20, 0);
    assert!(h.events_for(p).contains(&Event::Theme { surface: 4, name: "luna".into() }));
    h.comp.key(88, true);
    let evs = h.comp.take_events();
    assert!(evs.contains(&(p, Event::Theme { surface: 4, name: "9x".into() })), "{evs:?}");
    assert!(!evs.iter().any(|(c, e)| *c == w && matches!(e, Event::Theme { .. })), "only the panel");
    h.comp.set_theme(&theme::LUNA);
    assert_eq!(h.events_for(p), vec![Event::Theme { surface: 4, name: "luna".into() }]);
}

// A: the taskbar's strip is a shape under the panel's surface, right before its pixels, covering exactly the panel's area.
#[test]
fn a_themed_taskbar_is_a_shape_under_the_panel() {
    let mut h = gpu_h();
    let _w = h.window(100, 50, 0x00AA_0000);
    let p = panel_argb(&mut h, 20, 0);
    let strip = Rect::new(0, H - 20, W, 20);
    for t in [&theme::LUNA, &theme::NINES] {
        h.comp.set_theme(t);
        let ops = h.comp.draw_list().1;
        let at = ops.iter().position(|o| matches!(o, DrawOp::Cpu { client, .. } if *client == p)).unwrap();
        assert_eq!(ops[at - 1], DrawOp::Shape { rect: strip, shape: t.taskbar.bar, clip: None }, "{}", t.name);
    }
}

// A: compose (the CPU painter) draws the same strip under a transparent panel, with the shape's own pixels (Shape::paint).
#[test]
fn compose_paints_the_strip_under_a_transparent_panel() {
    let mut h = H_::new();
    let _p = panel_argb(&mut h, 20, 0);
    h.compose();
    let strip = Rect::new(0, H - 20, W, 20);
    let bar = theme::LUNA.taskbar.bar;
    for (x, y) in [(10, H - 20), (10, H - 10), (W - 1, H - 1)] {
        // the strip is translucent (glass): over the desktop, as it is (the CPU painter does not blur)
        let mut p = [h.desktop(x, y)];
        bar.paint(&mut p, 1, Rect::new(0, 0, 1, 1), Rect::new(strip.x - x, strip.y - y, strip.w, strip.h));
        assert_eq!(h.px(x, y), p[0] & 0x00FF_FFFF, "({x}, {y})");
    }
    assert!(bar.c[0] >> 24 < 0xFF && bar.backdrop_blur > 0.0, "Luna's taskbar is glass");
    assert_ne!(h.px(10, H - 20), h.px(10, H - 2), "a gradient: the top differs from the bottom");
    assert_eq!(h.px(10, H - 21), h.desktop(10, H - 21), "nothing above the strip");
}

// ── popups (the start menu) ───────────────────────────────────────────────────────────────────────────────────────────────────────────

/// Gives client `c` a popup on its surface 4: surface 6, `w x h` of `color` (ARGB8888 when `argb`), at (`x`, `y`) of the parent's content,
/// committed. Pool fd 400 + c, pool 7, buffer 8.
fn popup(h: &mut H_, c: ClientId, (x, y): (i32, i32), (w, hh): (i32, i32), color: u32) {
    let fd = 400 + c as i32;
    let size = (w * hh * 4) as usize;
    h.pool(fd, size);
    h.draw(fd, 0, w as usize * 4, 0, 0, w as usize, hh as usize, color);
    h.send(c, &[
        R::CreateSurface { id: 6 },
        R::SetPopup { surface: 6, parent: 4, x, y },
        R::CreatePool { id: 7, fd, size: size as u32 },
        R::CreateBuffer { pool: 7, id: 8, offset: 0, width: w, height: hh, stride: w * 4, format: FORMAT_XRGB8888 },
        R::Attach { surface: 6, buffer: 8 },
        R::Commit { surface: 6 },
    ]);
}

fn recommit_popup(h: &mut H_, c: ClientId) {
    h.send(c, &[R::Attach { surface: 6, buffer: 8 }, R::Commit { surface: 6 }]);
}

// A: the panel's popup shows above the panel, at its offset (kept on the screen), undecorated, not a window (no toplevel, not in the
// stack, never focused); it is drawn last but the cursor, in compose and in the draw list alike.
#[test]
fn a_popup_shows_above_everything_and_is_not_a_window() {
    let mut h = gpu_h();
    let w = h.window(100, 50, 0x00AA_0000);
    let p = panel(&mut h, 20, 0x0099_9999);
    h.comp.take_events();
    popup(&mut h, p, (10, -60), (80, 60), 0x0012_3456);
    assert_eq!(h.comp.popups(), vec![(p, 6)]);
    assert_eq!(h.comp.window_content(p, 6), Some(Rect::new(10, H - 20 - 60, 80, 60)));
    assert_eq!(h.comp.toplevels().len(), 1, "not in the window list");
    assert!(!h.comp.stack().contains(&(p, 6)));
    assert_eq!(h.comp.focus(), Some((w, 4)), "the focus stays");
    assert!(h.events_for(p).iter().all(|e| !matches!(e, Event::Toplevel { .. })));
    h.comp.damage.add(Rect::new(0, 0, W, H));
    h.compose();
    assert_eq!(h.px(15, H - 50), 0x0012_3456);
    let (_, ops) = h.comp.draw_list();
    let last_cpu = ops.iter().rposition(|o| matches!(o, DrawOp::Cpu { .. })).unwrap();
    assert!(matches!(ops[last_cpu], DrawOp::Cpu { client, surface: 6, .. } if client == p), "drawn after everything");
    assert_eq!(same_picture(&raster(&ops, &h.comp, &BTreeMap::new()), &h.screen), None);
    // asked past the left edge: kept on the screen
    let q = h.comp.add_client();
    h.send(q, &[R::CreateSurface { id: 4 }]);
    let _ = h.window(10, 10, 0); // (unrelated, just another client in between)
    let _ = q;
    let p2 = h.window(60, 40, 0x0000_00FF);
    popup(&mut h, p2, (-500, 0), (30, 30), 0x00FF_00FF);
    assert_eq!(h.comp.window_content(p2, 6).unwrap().x, 0);
}

// A: a click inside the popup is the popup's (no focus change); a click outside closes every popup, tells the client, and goes nowhere;
// a commit with a buffer shows it again; Escape closes it too and is nobody's key.
#[test]
fn a_click_outside_or_escape_closes_the_popup() {
    let mut h = gpu_h();
    let w = h.window(100, 50, 0x00AA_0000);
    let p = panel(&mut h, 20, 0x0099_9999);
    popup(&mut h, p, (10, -60), (80, 60), 0x0012_3456);
    h.comp.take_events();
    let r = h.comp.window_content(p, 6).unwrap();
    pointer_to(&mut h, r.x + 5, r.y + 5);
    h.comp.pointer_button(BTN_LEFT, true);
    h.comp.pointer_button(BTN_LEFT, false);
    let evs = h.events_for(p);
    assert!(evs.contains(&Event::Button { surface: 6, code: BTN_LEFT, pressed: true }), "{evs:?}");
    h.comp.pointer_motion(1, 2);
    assert!(h.events_for(p).contains(&Event::Motion { surface: 6, x: 6, y: 7 }), "motion over it is the popup's too");
    assert_eq!(h.comp.focus(), Some((w, 4)));
    // outside, over the window: closes, and the window gets nothing
    let f = h.comp.window_content(w, 4).unwrap();
    pointer_to(&mut h, f.x + 5, f.y + 5);
    h.comp.take_events();
    h.comp.pointer_button(BTN_LEFT, true);
    h.comp.pointer_button(BTN_LEFT, false);
    let evs = h.comp.take_events();
    assert_eq!(evs, vec![(p, Event::PopupDone { surface: 6 })], "only popup_done");
    assert!(h.comp.popups().is_empty());
    assert!(h.comp.damage().contains(r.x, r.y), "where it was is repainted");
    h.compose();
    assert_ne!(h.px(r.x + 5, r.y + 5), 0x0012_3456);
    // shown again by a commit; Escape closes it and reaches no client
    recommit_popup(&mut h, p);
    assert_eq!(h.comp.popups(), vec![(p, 6)]);
    h.comp.take_events();
    h.comp.key(1, true);
    h.comp.key(1, false);
    let evs = h.comp.take_events();
    assert_eq!(evs.iter().filter(|(_, e)| matches!(e, Event::Key { pressed: true, .. })).count(), 0, "{evs:?}");
    assert!(evs.contains(&(p, Event::PopupDone { surface: 6 })));
    // with no popup, Escape is the focused window's again
    h.comp.key(1, true);
    assert!(h.events_for(w).contains(&Event::Key { surface: 4, code: 1, pressed: true }));
}

// A: the popup goes when its parent does; bad set_popup requests are protocol errors; only the panel may set the theme.
#[test]
fn popup_parent_errors_and_set_theme() {
    let mut h = gpu_h();
    let p = panel(&mut h, 20, 0x0099_9999);
    popup(&mut h, p, (0, -40), (50, 40), 0x0012_3456);
    h.comp.take_events();
    h.send(p, &[R::DestroySurface { surface: 4 }]);
    assert!(h.comp.popups().is_empty());
    assert!(h.events_for(p).contains(&Event::PopupDone { surface: 6 }));
    // a mapped window cannot become a popup; nor can a surface name a parent it does not have
    for reqs in [
        vec![R::SetPopup { surface: 4, parent: 9, x: 0, y: 0 }],
        vec![R::CreateSurface { id: 9 }, R::SetPopup { surface: 4, parent: 9, x: 0, y: 0 }],
    ] {
        let mut h = gpu_h();
        let c = h.window(40, 30, 0x0012_3456);
        h.send(c, &reqs);
        assert!(h.comp.take_events().iter().any(|(_, e)| matches!(e, Event::Error { code, .. } if *code == ErrorCode::Role as u32)), "{reqs:?}");
    }
    let mut h = gpu_h();
    let c = h.window(40, 30, 0x0012_3456);
    h.send(c, &[R::SetTheme { surface: 4, name: "9x".into() }]);
    assert_eq!(h.comp.theme().name, "luna", "a window may not");
    let p = panel(&mut h, 20, 0);
    h.send(p, &[R::SetTheme { surface: 4, name: "9x".into() }]);
    assert_eq!(h.comp.theme().name, "9x", "the panel may");
    h.send(p, &[R::SetTheme { surface: 4, name: "aqua".into() }]);
    assert_eq!(h.comp.theme().name, "9x", "an unknown name changes nothing");
}

// A: in a look with a menu style the popup's frame (with its shadow) is a shape right under it, in the draw list and in compose.
#[test]
fn a_themed_popup_has_its_frame_under_it() {
    let mut h = gpu_h();
    let p = panel(&mut h, 20, 0x0099_9999);
    popup(&mut h, p, (10, -60), (80, 60), 0x0012_3456);
    let r = h.comp.window_content(p, 6).unwrap();
    for t in [&theme::LUNA, &theme::NINES] {
        h.comp.set_theme(t);
        let ops = h.comp.draw_list().1;
        let at = ops.iter().position(|o| matches!(o, DrawOp::Cpu { surface: 6, .. })).unwrap();
        assert_eq!(ops[at - 1], DrawOp::Shape { rect: r, shape: t.menu.frame, clip: None }, "{}", t.name);
    }
    h.comp.set_theme(&theme::LUNA);
    h.compose();
    let frame = theme::LUNA.menu.frame;
    // the shadow below and to the right of the menu, painted by compose
    let (sx, sy) = (r.right() + 2, r.y + 20);
    assert!(frame.pixel(r, sx, sy)[3] > 0.1);
    assert_ne!(h.px(sx, sy), h.desktop(sx, sy), "the shadow darkens the desktop");
}

// A: a fully transparent (ARGB8888) window shows the desktop through its content: neither its frame (a ring around it) nor its title bar
// (whose box reaches under the content to round only the top corners, kept out by its clip) is under it.
#[test]
fn a_transparent_window_shows_neither_its_frame_nor_its_bar_through() {
    let mut h = H_::new();
    let c = h.comp.add_client();
    h.pool(300, 100 * 60 * 4);
    h.send(c, &[
        R::CreatePool { id: 2, fd: 300, size: 100 * 60 * 4 },
        R::CreateBuffer { pool: 2, id: 3, offset: 0, width: 100, height: 60, stride: 100 * 4, format: crate::protocol::FORMAT_ARGB8888 },
        R::CreateSurface { id: 4 },
        R::Attach { surface: 4, buffer: 3 },
        R::Commit { surface: 4 },
    ]);
    let r = h.comp.window_content(c, 4).unwrap();
    h.compose();
    for (x, y) in [(r.x, r.y), (r.x + 5, r.y + 2), (r.right() - 1, r.y + 4), (r.x + 50, r.y + 30), (r.x, r.bottom() - 1)] {
        assert_eq!(h.px(x, y), h.desktop(x, y), "({x}, {y})");
    }
    assert_ne!(h.px(r.x + 50, r.y - 5), h.desktop(r.x + 50, r.y - 5), "the bar is above it");
    assert_ne!(h.px(r.x - 1, r.y + 30), h.desktop(r.x - 1, r.y + 30), "the frame is around it");
}

fn sem(id: u32, parent: u32, role: crate::semantic::Role, name: &str) -> Node {
    let mut n = Node::new(id, parent, role, Rect::new(1, 2, 30, 4));
    n.name = name.into();
    n
}

/// The `get_semantics` answer as (window title, focused, node names).
fn semantics_dump(h: &mut H_, asker: ClientId, cb: u32) -> Vec<(std::string::String, bool, Vec<std::string::String>)> {
    h.send(asker, &[R::GetSemantics { id: cb }]);
    let mut out: Vec<(std::string::String, bool, Vec<std::string::String>)> = Vec::new();
    let mut done = false;
    for e in h.events_for(asker) {
        match e {
            Event::SemanticsWindow { callback, title, focused, .. } if callback == cb => out.push((title, focused, Vec::new())),
            Event::SemanticsNode { callback, node } if callback == cb => out.last_mut().unwrap().2.push(node.name),
            Event::Done { callback, .. } if callback == cb => done = true,
            Event::DeleteId { id } if id == cb => assert!(done, "delete_id before done"),
            _ => {}
        }
    }
    assert!(done, "no done");
    out
}

#[test]
fn semantic_tree_applies_at_commit_and_is_dumped_per_window() {
    use crate::semantic::Role;
    let mut h = H_::new();
    let a = h.window(40, 30, 0x00FF_0000);
    let b = h.window(40, 30, 0x0000_FF00);
    h.send(a, &[R::SetTitle { surface: 4, title: "A".into() }]);
    h.send(b, &[R::SetTitle { surface: 4, title: "B".into() }]);
    h.send(a, &[
        R::SemanticsNode { surface: 4, node: sem(1, 0, Role::Window, "root") },
        R::SemanticsNode { surface: 4, node: sem(2, 1, Role::Button, "Open") },
    ]);
    let asker = h.comp.add_client();
    // not committed yet: nothing
    assert_eq!(semantics_dump(&mut h, asker, 9), vec![("A".into(), false, vec![]), ("B".into(), true, vec![])]);
    h.send(a, &[R::Commit { surface: 4 }]);
    let d = semantics_dump(&mut h, asker, 9); // the id is free again after delete_id
    assert_eq!(d[0], ("A".into(), false, vec!["root".into(), "Open".into()]));
    // a commit without nodes keeps the tree; new nodes replace it whole
    h.send(a, &[R::Commit { surface: 4 }]);
    assert_eq!(semantics_dump(&mut h, asker, 9)[0].2, vec!["root", "Open"]);
    h.send(a, &[R::SemanticsNode { surface: 4, node: sem(1, 0, Role::Window, "root2") }, R::Commit { surface: 4 }]);
    assert_eq!(semantics_dump(&mut h, asker, 9)[0].2, vec!["root2"]);
    // the whole node survives the trip
    h.send(asker, &[R::GetSemantics { id: 10 }]);
    let n = h.events_for(asker).into_iter().find_map(|e| if let Event::SemanticsNode { node, .. } = e { Some(node) } else { None });
    assert_eq!(n, Some(sem(1, 0, Role::Window, "root2")));
    // the window's box on the screen
    h.send(asker, &[R::GetSemantics { id: 11 }]);
    let wbox = h.events_for(asker).into_iter().find_map(|e| match e {
        Event::SemanticsWindow { x, y, w, h: hh, toplevel, .. } if toplevel == 1 => Some(Rect::new(x, y, w, hh)),
        _ => None,
    });
    assert_eq!(wbox, h.comp.window_content(a, 4));
}

#[test]
fn semantic_nodes_are_checked() {
    use crate::semantic::{Role, MAX_NODES};
    let mut h = H_::new();
    let a = h.window(40, 30, 0x00FF_0000);
    h.send(a, &[R::SemanticsNode { surface: 4, node: sem(0, 0, Role::Window, "zero") }]);
    assert!(h.events_for(a).iter().any(|e| matches!(e, Event::Error { message, .. } if message.contains("id 0"))));
    assert!(!h.comp.has_client(a));

    let a = h.window(40, 30, 0x00FF_0000);
    let many: Vec<Request> = (1..=MAX_NODES as u32).map(|i| R::SemanticsNode { surface: 4, node: sem(i, 0, Role::Label, "") }).collect();
    h.send(a, &many);
    assert!(h.comp.has_client(a), "MAX_NODES nodes are allowed");
    h.send(a, &[R::SemanticsNode { surface: 4, node: sem(1, 0, Role::Label, "") }]);
    assert!(h.events_for(a).iter().any(|e| matches!(e, Event::Error { code: 2, .. })));
    assert!(!h.comp.has_client(a));
}

#[test]
fn the_wheel_goes_to_the_content_under_the_pointer() {
    let mut h = H_::new();
    let a = h.window(100, 100, 0x00AA_0000); // at (40, 40)
    let b = h.window(100, 100, 0x0000_BB00); // at (72, 72), on top and focused
    h.compose();
    let axis = |h: &mut H_, steps| {
        h.comp.take_events();
        h.comp.pointer_axis(steps);
        h.comp.take_events()
    };
    // a's visible content, behind b: a gets it, nothing is raised or focused
    pointer_to(&mut h, 45, 70);
    assert_eq!(axis(&mut h, -2), vec![(a, Event::Axis { surface: 4, steps: -2 })]);
    assert_eq!(h.comp.stack(), &[(a, 4), (b, 4)]);
    assert_eq!(h.comp.focus(), Some((b, 4)));
    // b's content
    pointer_to(&mut h, 120, 150);
    assert_eq!(axis(&mut h, 1), vec![(b, Event::Axis { surface: 4, steps: 1 })]);
    // a title bar, the desktop, zero notches: nothing
    pointer_to(&mut h, 100, 77);
    assert_eq!(axis(&mut h, 1), vec![]);
    pointer_to(&mut h, 300, 230);
    assert_eq!(axis(&mut h, 1), vec![]);
    pointer_to(&mut h, 120, 150);
    assert_eq!(axis(&mut h, 0), vec![]);
    // locked: to the locked window wherever the pointer is (here over its title bar)
    pointer_to(&mut h, 100, 77);
    h.send(b, &[R::LockPointer { surface: 4, on: true }]);
    assert_eq!(h.comp.pointer_locked(), Some((b, 4)));
    assert_eq!(axis(&mut h, 3), vec![(b, Event::Axis { surface: 4, steps: 3 })]);
}
