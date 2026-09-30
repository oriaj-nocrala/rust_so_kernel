use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn stage(n: &str) { println!("TK stage {n}"); }

async fn timers() {
    let t = Instant::now();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let e = t.elapsed().as_millis();
    println!("TK timers sleep100 -> {e} ms ok={}", (95..400).contains(&e));
    let r = tokio::time::timeout(Duration::from_millis(50), std::future::pending::<()>()).await;
    println!("TK timers timeout elapsed={}", r.is_err());
    let mut iv = tokio::time::interval(Duration::from_millis(20));
    for _ in 0..3 { iv.tick().await; }
    println!("TK timers interval ok");
}

async fn channels() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(4);
    let p = tokio::spawn(async move { for i in 0..10 { tx.send(i).await.unwrap(); } });
    let mut sum = 0; while let Some(v) = rx.recv().await { sum += v; }
    p.await.unwrap();
    println!("TK channels sum={sum} ok={}", sum == 45);
    let (otx, orx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move { tokio::time::sleep(Duration::from_millis(20)).await; otx.send(7).unwrap(); });
    println!("TK channels oneshot={}", orx.await.unwrap());
}

async fn unix_echo() {
    let dir = "/tmp/tk.sock";
    let _ = std::fs::remove_file(dir);
    let l = tokio::net::UnixListener::bind(dir).unwrap();
    let srv = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut s, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 64];
                loop {
                    let n = s.read(&mut buf).await.unwrap();
                    if n == 0 { break; }
                    s.write_all(&buf[..n]).await.unwrap();
                }
            });
        }
    });
    let mut cl = vec![];
    for i in 0..3 {
        cl.push(tokio::spawn(async move {
            let mut c = tokio::net::UnixStream::connect(dir).await.unwrap();
            let msg = format!("hello-{i}");
            c.write_all(msg.as_bytes()).await.unwrap();
            let mut b = vec![0u8; msg.len()];
            c.read_exact(&mut b).await.unwrap();
            String::from_utf8(b).unwrap() == msg
        }));
    }
    let mut ok = true; for c in cl { ok &= c.await.unwrap(); }
    srv.await.unwrap();
    println!("TK unix echo x3 ok={ok}");
}

async fn files() {
    tokio::fs::write("/tmp/tk.txt", b"tokio file").await.unwrap();
    let s = tokio::fs::read_to_string("/tmp/tk.txt").await.unwrap();
    println!("TK fs read={s:?}");
}

async fn blocking() {
    let r = tokio::task::spawn_blocking(|| { std::thread::sleep(Duration::from_millis(30)); 42 }).await.unwrap();
    println!("TK spawn_blocking={r}");
}

async fn process() {
    let o = tokio::process::Command::new("/tmp/bin/echo").arg("from-child").output().await.unwrap();
    println!("TK process out={:?} status={}", String::from_utf8_lossy(&o.stdout).trim(), o.status.success());
}

async fn signals() {
    let mut s = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1()).unwrap();
    unsafe { libc_kill(); }
    let got = tokio::time::timeout(Duration::from_secs(2), s.recv()).await;
    println!("TK signal usr1 received={}", got.is_ok());
}
extern "C" { fn getpid() -> i32; fn kill(pid: i32, sig: i32) -> i32; }
unsafe fn libc_kill() { kill(getpid(), 10); }

fn main() {
    stage("current_thread");
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async { timers().await; channels().await; });
    stage("current_thread unix/fs");
    rt.block_on(async { unix_echo().await; files().await; });
    drop(rt);

    stage("multi_thread");
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(3).enable_all().build().unwrap();
    rt.block_on(async {
        timers().await;
        channels().await;
        unix_echo().await;
        blocking().await;
        let hs: Vec<_> = (0..20).map(|i| tokio::spawn(async move { tokio::time::sleep(Duration::from_millis(10 + i)).await; i })).collect();
        let mut s = 0; for h in hs { s += h.await.unwrap(); }
        println!("TK multi 20 tasks sum={s} ok={}", s == 190);
    });
    stage("process");
    rt.block_on(process());
    stage("signal");
    rt.block_on(signals());
    println!("TK DONE");
}
