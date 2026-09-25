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
    assert_eq!(h.px(0, 0), BACKGROUND);
    assert_eq!(h.px(40, 40), TITLE_FOCUSED);
    assert_eq!(h.px(40, 40 + TITLE_H), 0x00FF_0000);
    assert_eq!(h.px(139, 40 + TITLE_H + 49), 0x00FF_0000);
    assert_eq!(h.px(140, 40 + TITLE_H), BACKGROUND);
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
    assert_eq!(h.px(72, 72), TITLE_FOCUSED);
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
    assert_eq!(h.px(150, 150), 0x0000_BB00);
    assert_eq!(h.px(40, 40), TITLE_FOCUSED);
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
    assert_eq!(h.px(60, 60 + TITLE_H), BACKGROUND, "where it was");
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
    // Wrong format.
    let mut h = H_::new();
    let c = h.comp.add_client();
    h.pool(7, 400);
    h.send(c, &[R::CreatePool { id: 2, fd: 7, size: 400 }, R::CreateBuffer { pool: 2, id: 3, offset: 0, width: 1, height: 1, stride: 4, format: 0 }]);
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
    assert_eq!(h.px(100, 100 + TITLE_H), BACKGROUND);
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
    assert_eq!(h.px(40, 40 + TITLE_H), BACKGROUND);
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
    assert_eq!(h.px(100, 100), BACKGROUND, "the old area is gone");
}

#[test]
fn titles_are_kept_and_capped() {
    let mut h = H_::new();
    let c = h.window(10, 10, 1);
    h.send(c, &[R::SetTitle { surface: 4, title: "ñ".repeat(100) }]);
    let t = h.comp.window_title(c, 4).unwrap();
    assert!(t.len() <= crate::protocol::MAX_TITLE && t.chars().all(|ch| ch == 'ñ'));
}
