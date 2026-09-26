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
    assert_eq!(h.px(176, 147), BACKGROUND);
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
    h.comp.pointer_button(BTN_LEFT, true);
    h.compose();
    assert_eq!(h.px(close_a.0 - TITLE_H / 2 + 1, 41), CLOSE_PRESSED);
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
    assert_eq!(h.px(300, H - 1), BACKGROUND);
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
    assert_eq!(h.px(45, 45), TITLE_FOCUSED, "padding before the text");
    assert_ne!(h.px(140 - TITLE_H / 2, 40 + TITLE_H / 2), 0x00AB_CDEF, "the close button is not text");
    // A title change repaints the bar.
    h.send(c, &[R::SetTitle { surface: 4, title: "adiós".into() }]);
    assert!(h.comp.damage().contains(50, 45));
}
