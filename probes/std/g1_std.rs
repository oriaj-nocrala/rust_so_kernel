// A plain Rust std program (x86_64-unknown-linux-musl) for what the last G1 slices added: hard links and statx through std::fs,
// Command with uid/gid (fork + exec, which needs socketpair(SOCK_SEQPACKET) for its error pipe, and setuid/setgid), and the exec
// error reaching the parent through that pipe. Asserts as it goes; scripts/run-std-probe.sh runs it in the guest.
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::process::Command;

fn main() {
    for dir in ["/tmp", "/mnt"] {
        let (a, b) = (format!("{dir}/g1std_a"), format!("{dir}/g1std_b"));
        let _ = fs::remove_file(&a);
        let _ = fs::remove_file(&b);
        fs::File::create(&a).unwrap().write_all(b"hello").unwrap();
        fs::hard_link(&a, &b).unwrap();
        let (ma, mb) = (fs::metadata(&a).unwrap(), fs::metadata(&b).unwrap());
        assert_eq!((ma.nlink(), ma.ino() == mb.ino(), mb.len()), (2, true, 5));
        fs::remove_file(&a).unwrap();
        assert_eq!(fs::metadata(&b).unwrap().nlink(), 1);
        assert_eq!(fs::read_to_string(&b).unwrap(), "hello");
        fs::remove_file(&b).unwrap();
        println!("G1 {dir} hard_link ok");
    }
    fs::write("/tmp/g1std_x", b"x").unwrap();
    let e = fs::hard_link("/tmp/g1std_x", "/mnt/g1std_x").unwrap_err();
    assert_eq!(e.raw_os_error(), Some(18), "EXDEV across filesystems");
    let _ = fs::remove_file("/tmp/g1std_x");
    assert!(fs::metadata("/tmp").unwrap().created().is_err(), "no birth time is reported");
    println!("G1 cross-fs and birth time ok");

    let id = |c: &mut Command| String::from_utf8_lossy(&c.arg("id").output().unwrap().stdout).trim().to_string();
    let plain = id(&mut Command::new("/bin/busybox"));
    assert!(plain.starts_with("uid=0(root) gid=0(root)"), "{plain}");
    assert!(id(Command::new("/bin/busybox").uid(1000).gid(1000)).starts_with("uid=1000 gid=1000"));
    assert!(id(Command::new("/bin/busybox").gid(1000)).contains("gid=1000"));
    println!("G1 Command uid/gid ok");

    // A failed exec after the fork path is reported by the child through the SEQPACKET pipe.
    let e = Command::new("/nonexistent/prog").uid(1000).spawn().unwrap_err();
    assert_eq!(e.raw_os_error(), Some(2), "{e}");
    println!("G1 exec error ok");
    println!("G1 DONE");
}
