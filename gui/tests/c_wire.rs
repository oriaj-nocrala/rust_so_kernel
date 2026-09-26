//! The C client's wire code (`userspace/c/include/constanos_gui_wire.h`,
//! used by fire, DOOM and Quake) checked against this crate, which is the
//! reference: what C encodes must decode here to the requests it meant,
//! and what this crate encodes must split into the same messages in C.
//! Compiled with the host's `cc`; a missing compiler fails the test rather
//! than skipping it.

use std::path::PathBuf;
use std::process::Command;

use gui::protocol::{Event, Interface, Request};
use gui::wire::{Decoder, Encoder};

fn driver(tag: &str) -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out = std::env::temp_dir().join(format!("gui-c-wire-{}-{tag}", std::process::id()));
    let st = Command::new("cc")
        .args(["-std=c11", "-Wall", "-Werror", "-Wno-unused-function", "-O1"])
        .arg("-I")
        .arg(root.join("../userspace/c/include"))
        .arg(root.join("tests/c_wire_main.c"))
        .arg("-o")
        .arg(&out)
        .status()
        .expect("cc is needed to test the C client");
    assert!(st.success(), "the C wire header does not compile");
    out
}

fn run(exe: &PathBuf, args: &[&str]) -> String {
    let o = Command::new(exe).args(args).output().unwrap();
    assert!(o.status.success(), "c_wire {:?} failed", args);
    String::from_utf8(o.stdout).unwrap()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn c_requests_decode_to_what_they_meant() {
    let exe = driver("enc");
    let out = run(&exe, &["enc"]);
    let mut lines = out.lines();
    let bytes = unhex(lines.next().unwrap());
    let fds: Vec<i32> = lines.next().unwrap().split_whitespace().map(|f| f.parse().unwrap()).collect();

    let want = [
        (Interface::Compositor, Request::CreatePool { id: 2, fd: 17, size: 4096 }),
        (Interface::Compositor, Request::CreateSurface { id: 4 }),
        (Interface::Pool, Request::CreateBuffer { pool: 2, id: 3, offset: 0, width: 320, height: -200, stride: 1280, format: 1 }),
        (Interface::Surface, Request::Attach { surface: 4, buffer: 3 }),
        (Interface::Surface, Request::Damage { surface: 4, x: -1, y: 2, w: 3, h: 4 }),
        (Interface::Surface, Request::Frame { surface: 4, id: 16 }),
        (Interface::Surface, Request::Commit { surface: 4 }),
        (Interface::Surface, Request::SetTitle { surface: 4, title: "doom".into() }),
        (Interface::Surface, Request::SetTitle { surface: 4, title: "abc".into() }),
        (Interface::Surface, Request::SetTitle { surface: 4, title: "".into() }),
        (Interface::Surface, Request::LockPointer { surface: 4, on: true }),
        (Interface::Surface, Request::LockPointer { surface: 4, on: false }),
    ];

    // Byte for byte what the Rust encoder makes of the same requests.
    let mut e = Encoder::new();
    for (_, r) in &want {
        r.encode(&mut e);
    }
    let (rust_bytes, rust_fds) = e.take();
    assert_eq!(hex(&bytes), hex(&rust_bytes));
    assert_eq!(fds, rust_fds);

    let mut d = Decoder::new();
    d.push_bytes(&bytes);
    d.push_fds(&fds);
    for (iface, r) in want {
        let m = d.next_message().unwrap().unwrap();
        assert_eq!(Request::decode(iface, &m, &mut d).unwrap(), r);
    }
    assert!(d.next_message().unwrap().is_none());
    let _ = std::fs::remove_file(exe);
}

#[test]
fn c_splits_rust_events_into_the_same_messages() {
    let exe = driver("dec");
    let evs = [
        Event::Configure { surface: 4, width: 640, height: 400 },
        Event::Focus { surface: 4, focused: true },
        Event::Key { surface: 4, code: 30, pressed: true },
        Event::Button { surface: 4, code: 0x110, pressed: false },
        Event::RelativeMotion { surface: 4, dx: -7, dy: 12 },
        Event::Release { buffer: 3 },
        Event::Error { object: 9, code: 5, message: "bad buffer".into() },
    ];
    let mut e = Encoder::new();
    for ev in &evs {
        ev.encode(&mut e);
    }
    let (bytes, _) = e.take();
    let out = run(&exe, &["dec", &hex(&bytes)]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "4 0 16 640 400");
    assert_eq!(lines[1], "4 1 12 1");
    assert_eq!(lines[2], "4 2 16 30 1");
    assert_eq!(lines[3], "4 4 16 272 0");
    assert_eq!(lines[4], "4 5 16 -7 12");
    assert_eq!(lines[5], "3 0 8");
    // "bad buffer\0" is 11 bytes, padded to 12: 8 + 4 + 4 + 4 + 12 = 32.
    assert!(lines[6].starts_with("1 0 32 9 5 11 "), "{}", lines[6]);
    assert_eq!(lines[7], "END");
    assert_eq!(lines.len(), 8);

    // A stream cut anywhere inside a message is PARTIAL, never a message.
    for cut in 1..16 {
        let out = run(&exe, &["dec", &hex(&bytes[..cut])]);
        assert_eq!(out.trim(), "PARTIAL", "cut at {cut}");
    }
    // Sizes the Rust decoder rejects, C rejects too.
    for size in [0u32, 4, 10, 4100] {
        let mut b = 1u32.to_ne_bytes().to_vec();
        b.extend_from_slice(&(size << 16).to_ne_bytes());
        b.resize(8.max(size as usize).min(5000), 0);
        assert_eq!(run(&exe, &["dec", &hex(&b)]).trim(), "BAD", "size {size}");
    }
    let _ = std::fs::remove_file(exe);
}
