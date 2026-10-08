//! `gui_client` against a fake compositor on the other end of a socket pair: the requests it sends (decoded with `gui::protocol`, the
//! compositor's own decoder), the pool it passes (mapped here, its pixels read back), and the events it turns into `Event`s.

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::Duration;

use gui::protocol::{Event as Wire, Interface, Request};
use gui::wire::{Decoder, Encoder};
use gui_client::sys::{recv_with_fds, Mapping};
use gui_client::{Event, Window};

const SURFACE: u32 = 4;

/// The compositor's side: reads requests (with their fds) and writes events.
struct Fake {
    sock: UnixStream,
    dec: Decoder,
    pools: Vec<(u32, OwnedFd, u32)>,
}

impl Fake {
    fn new(sock: UnixStream) -> Fake {
        Fake { sock, dec: Decoder::new(), pools: Vec::new() }
    }

    /// The next request, decoded by the interface the client gave its object (ids as gui_client picks them: 2 pool, 3 buffer, 4 surface).
    fn request(&mut self) -> Request {
        loop {
            if let Ok(Some(msg)) = self.dec.next_message() {
                let iface = match msg.object {
                    1 => Interface::Compositor,
                    2 => Interface::Pool,
                    3 => Interface::Buffer,
                    _ => Interface::Surface,
                };
                let r = Request::decode(iface, &msg, &mut self.dec).expect("a request the compositor can decode");
                if let Request::CreatePool { id, fd, size } = r {
                    self.pools.push((id, unsafe { OwnedFd::from_raw_fd(fd) }, size));
                }
                return r;
            }
            let mut buf = [0u8; 4096];
            let (n, fds) = recv_with_fds(self.sock.as_raw_fd(), &mut buf).unwrap();
            assert!(n > 0, "the client hung up");
            self.dec.push_bytes(&buf[..n]);
            self.dec.push_fds(&fds);
        }
    }

    fn send(&mut self, evs: &[Wire]) {
        let mut e = Encoder::new();
        for ev in evs {
            ev.encode(&mut e);
        }
        let (bytes, _) = e.take();
        self.sock.write_all(&bytes).unwrap();
    }

    /// The last pool's pixels.
    fn pool_pixels(&self) -> Vec<u32> {
        let (_, fd, size) = self.pools.last().unwrap();
        let mut m = Mapping::new(fd.as_raw_fd(), *size as usize).unwrap();
        m.pixels().to_vec()
    }
}

/// Plays the start of a session: create_surface, configure 640x400, set_title, the pool and the buffer.
fn handshake(f: &mut Fake) -> (i32, i32) {
    assert_eq!(f.request(), Request::CreateSurface { id: SURFACE });
    f.send(&[Wire::Configure { surface: SURFACE, width: 640, height: 400 }]);
    assert_eq!(f.request(), Request::SetTitle { surface: SURFACE, title: "test".into() });
    let Request::CreatePool { id: 2, size, .. } = f.request() else { panic!("no create_pool") };
    match f.request() {
        Request::CreateBuffer { pool: 2, id: 3, offset: 0, width, height, stride, format: 1 } => {
            assert_eq!(stride, width * 4);
            assert_eq!(size, (stride * height) as u32);
            (width, height)
        }
        r => panic!("expected create_buffer, got {:?}", r),
    }
}

#[test]
fn opens_presents_and_reads_events() {
    let (a, b) = UnixStream::pair().unwrap();
    let server = thread::spawn(move || {
        let mut f = Fake::new(b);
        assert_eq!(handshake(&mut f), (100, 50));
        assert_eq!(f.request(), Request::Attach { surface: SURFACE, buffer: 3 });
        assert_eq!(f.request(), Request::Damage { surface: SURFACE, x: 0, y: 0, w: 100, h: 50 });
        assert_eq!(f.request(), Request::Commit { surface: SURFACE });
        let px = f.pool_pixels();
        assert_eq!(px.len(), 100 * 50);
        assert_eq!(px[0], 0x0012_3456);
        assert_eq!(px[100 * 50 - 1], 0x00ab_cdef);
        f.send(&[
            Wire::Focus { surface: SURFACE, focused: true },
            Wire::Key { surface: SURFACE, code: 30, pressed: true },
            Wire::Motion { surface: SURFACE, x: 7, y: 9 },
            Wire::Button { surface: SURFACE, code: 0x110, pressed: true },
            Wire::Toplevel { surface: SURFACE, id: 1, title: "not for us".into() },
            Wire::RelativeMotion { surface: SURFACE, dx: -3, dy: 4 },
            Wire::Axis { surface: SURFACE, steps: -2 },
            Wire::Close { surface: SURFACE },
        ]);
        // Not released yet: the client's next frame() must wait for this.
        thread::sleep(Duration::from_millis(100));
        f.send(&[Wire::Release { buffer: 3 }]);
        assert_eq!(f.request(), Request::Attach { surface: SURFACE, buffer: 3 });
        f
    });
    let mut w = Window::with_stream(a, "test", Some((100, 50))).unwrap();
    assert_eq!(w.size(), (100, 50));
    assert_eq!(w.suggested_size(), (640, 400));
    let px = w.frame().unwrap();
    px.fill(0x0012_3456);
    px[100 * 50 - 1] = 0x00ab_cdef;
    w.present().unwrap();
    let t = std::time::Instant::now();
    w.frame().unwrap(); // blocks until release, queueing the events
    assert!(t.elapsed() >= Duration::from_millis(80), "frame() did not wait for the release");
    let mut got = Vec::new();
    while let Some(e) = w.next_event(Some(Duration::ZERO)).unwrap() {
        got.push(e);
    }
    assert_eq!(
        got,
        [
            Event::Focus(true),
            Event::Key { code: 30, pressed: true },
            Event::Motion { x: 7, y: 9 },
            Event::Button { code: 0x110, pressed: true },
            Event::RelativeMotion { dx: -3, dy: 4 },
            Event::Wheel { steps: -2 },
            Event::Close,
        ]
    );
    w.present().unwrap();
    server.join().unwrap();
}

#[test]
fn takes_the_suggested_size_and_resizes() {
    let (a, b) = UnixStream::pair().unwrap();
    let server = thread::spawn(move || {
        let mut f = Fake::new(b);
        assert_eq!(handshake(&mut f), (640, 400));
        assert_eq!(f.request(), Request::SetResizable { surface: SURFACE, min_w: 10, min_h: 20 });
        f.send(&[Wire::Resize { surface: SURFACE, width: 200, height: 30 }]);
        assert_eq!(f.request(), Request::DestroyBuffer { buffer: 3 });
        assert_eq!(f.request(), Request::DestroyPool { pool: 2 });
        let Request::CreatePool { id: 2, size, .. } = f.request() else { panic!("no new pool") };
        assert_eq!(size, 200 * 30 * 4);
        assert_eq!(f.request(), Request::CreateBuffer { pool: 2, id: 3, offset: 0, width: 200, height: 30, stride: 800, format: 1 });
        assert_eq!(f.request(), Request::Attach { surface: SURFACE, buffer: 3 });
        assert_eq!(f.request(), Request::Damage { surface: SURFACE, x: 0, y: 0, w: 200, h: 30 });
        assert_eq!(f.request(), Request::Commit { surface: SURFACE });
        assert!(f.pool_pixels().iter().all(|&p| p == 0x00ff_0000));
    });
    let mut w = Window::with_stream(a, "test", None).unwrap();
    assert_eq!(w.size(), (640, 400));
    w.set_resizable(10, 20).unwrap();
    assert_eq!(w.next_event(None).unwrap(), Some(Event::Resize { width: 200, height: 30 }));
    w.resize(200, 30).unwrap();
    w.frame().unwrap().fill(0x00ff_0000);
    w.present().unwrap();
    server.join().unwrap();
}

#[test]
fn says_why_it_failed() {
    // The compositor refuses the surface.
    let (a, b) = UnixStream::pair().unwrap();
    let server = thread::spawn(move || {
        let mut f = Fake::new(b);
        f.request();
        f.send(&[Wire::Error { object: 1, code: 7, message: "no".into() }]);
    });
    let e = Window::with_stream(a, "test", None).err().unwrap();
    assert!(e.to_string().contains("refused a request (error 7): no"), "{}", e);
    server.join().unwrap();

    // The compositor goes away after the window is up.
    let (a, b) = UnixStream::pair().unwrap();
    let server = thread::spawn(move || {
        let mut f = Fake::new(b);
        handshake(&mut f);
    });
    let mut w = Window::with_stream(a, "test", None).unwrap();
    server.join().unwrap();
    let e = w.next_event(None).err().unwrap();
    assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
    assert!(e.to_string().contains("compositor closed"), "{}", e);
    // Sending to it is the same error, not a SIGPIPE.
    assert_eq!(w.present().err().unwrap().kind(), std::io::ErrorKind::UnexpectedEof);

    // No compositor at all.
    std::env::remove_var("GUI_DISPLAY");
    let e = Window::open("test", None).err().unwrap();
    assert!(e.to_string().contains("GUI_DISPLAY is not set"), "{}", e);
    std::env::set_var("GUI_DISPLAY", "/nonexistent/gui.sock");
    let e = Window::open("test", None).err().unwrap();
    assert!(e.to_string().contains("cannot connect to the compositor at /nonexistent/gui.sock"), "{}", e);
}
