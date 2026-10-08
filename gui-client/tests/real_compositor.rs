//! A window publishes its semantic tree and another client reads it back, both through `gui::compositor::Compositor` itself (the state
//! machine both compositors run) served on socket pairs in a thread: the whole trip, encoder to compositor to decoder.

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::thread;

use gui::compositor::{Compositor, PoolMem};
use gui::protocol::Event;
use gui::wire::Encoder;
use gui_client::sys::{recv_with_fds, wait_readable, Mapping};
use gui_client::{semantic::flag, Node, Role, Window};
use gui::region::Rect;

struct Mem(Mapping, usize);

impl PoolMem for Mem {
    fn as_ptr(&self) -> *const u8 {
        self.0.as_ptr()
    }
    fn len(&self) -> usize {
        self.1
    }
}

/// Serves `socks` with a real compositor until every client has hung up.
fn serve(socks: Vec<UnixStream>) {
    let mut comp: Compositor<Mem> = Compositor::new(1280, 800);
    let mut clients: Vec<(u32, UnixStream, bool)> = socks.into_iter().map(|s| (comp.add_client(), s, true)).collect();
    while clients.iter().any(|c| c.2) {
        for (id, sock, live) in clients.iter_mut().filter(|c| c.2) {
            if !wait_readable(sock.as_raw_fd(), 5).unwrap() {
                continue;
            }
            let mut buf = [0u8; 65536];
            // a client closing with events unread is ECONNRESET here: a hang-up too
            let (n, fds) = recv_with_fds(sock.as_raw_fd(), &mut buf).unwrap_or((0, Vec::new()));
            if n == 0 {
                *live = false;
                comp.remove_client(*id);
                continue;
            }
            comp.client_data(*id, &buf[..n], &fds, &mut |fd, size| Mapping::new(fd, size).ok().map(|m| Mem(m, size)));
            for fd in comp.take_fds_to_close() {
                drop(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
        let evs: Vec<(u32, Event)> = comp.take_events();
        for (to, ev) in evs {
            let mut e = Encoder::new();
            ev.encode(&mut e);
            let (bytes, _) = e.take();
            if let Some((_, s, true)) = clients.iter().find(|c| c.0 == to) {
                let _ = (&*s).write_all(&bytes);
            }
        }
    }
}

#[test]
fn a_window_tree_reaches_another_client() {
    let (win_c, win_s) = UnixStream::pair().unwrap();
    let (ask_c, ask_s) = UnixStream::pair().unwrap();
    let server = thread::spawn(move || serve(vec![win_s, ask_s]));

    let mut w = Window::with_stream(win_c, "Files", Some((200, 100))).unwrap();
    let mut root = Node::new(1, 0, Role::Window, Rect::new(0, 0, 200, 100));
    root.name = "Files".into();
    let mut row = Node::new(0x8000_0005, 1, Role::ListBoxOption, Rect::new(0, 20, 200, 20));
    row.name = "notes.txt".into();
    row.value = "3 KiB".into();
    row.flags = flag::SELECTED;
    row.pos = 7;
    row.set_size = 10_000;
    // 300 nodes: more than one batch
    let mut nodes = vec![root, row];
    nodes.extend((0..298).map(|i| Node::new(100 + i, 1, Role::Label, Rect::new(0, 0, 1, 1))));
    w.set_semantics(&nodes).unwrap();
    w.frame().unwrap().fill(0x00ff_ffff);
    w.present().unwrap();
    w.frame().unwrap(); // the compositor released the buffer: the commit was handled

    let trees = gui_client::semantics_on(&ask_c).unwrap();
    assert_eq!(trees.len(), 1);
    let t = &trees[0];
    assert_eq!((t.title.as_str(), t.focused, t.content.w, t.content.h), ("Files", true, 200, 100));
    assert_eq!(t.nodes, nodes);
    drop(w);
    drop(ask_c);
    server.join().unwrap();
}
