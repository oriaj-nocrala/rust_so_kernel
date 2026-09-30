//! A heavier tokio workload: many tasks, many timers, fan-in channels, many concurrent AF_UNIX connections, bulk throughput,
//! child processes, locks and broadcast, `select!`, blocking-pool and `tokio::fs` storms. Every scenario checks its result and
//! prints `TK <name> ... ok=<bool> (<ms> ms)`.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, watch, Mutex, RwLock, Semaphore};

/// VMAs the process holds (`/proc/self/maps` lines): the kernel caps them per process.
fn vmas() -> usize {
    std::fs::read_to_string("/proc/self/maps").map(|s| s.lines().count()).unwrap_or(0)
}

fn report(name: &str, t: Instant, ok: bool, extra: &str) {
    println!("TK {name} {extra} ok={ok} ({} ms, {} vmas)", t.elapsed().as_millis(), vmas());
}

async fn many_tasks() {
    let t = Instant::now();
    let total = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::with_capacity(10_000);
    for i in 0..10_000u64 {
        let total = total.clone();
        hs.push(tokio::spawn(async move {
            for _ in 0..3 { tokio::task::yield_now().await; }
            total.fetch_add(i, Ordering::Relaxed);
            i
        }));
        if i % 2000 == 1999 { println!("TK   spawned {} tasks, {} vmas", i + 1, vmas()); }
    }
    let mut sum = 0; for h in hs { sum += h.await.unwrap(); }
    report("tasks_10k", t, sum == 49_995_000 && total.load(Ordering::Relaxed) == sum, &format!("sum={sum}"));
}

async fn many_timers() {
    let t = Instant::now();
    let hs: Vec<_> = (0..1000u64).map(|i| tokio::spawn(async move { tokio::time::sleep(Duration::from_millis(10 + i % 190)).await; })).collect();
    for h in hs { h.await.unwrap(); }
    let e = t.elapsed().as_millis();
    report("timers_1000", t, (195..1500).contains(&e), "");
}

async fn fan_in() {
    let t = Instant::now();
    let (tx, mut rx) = mpsc::channel::<u64>(128);
    for p in 0..64u64 {
        let tx = tx.clone();
        tokio::spawn(async move { for i in 0..1000u64 { tx.send(p * 1000 + i).await.unwrap(); } });
    }
    drop(tx);
    let (mut n, mut sum) = (0u64, 0u64);
    while let Some(v) = rx.recv().await { n += 1; sum += v; }
    let want: u64 = (0..64_000u64).sum();
    report("mpsc_fanin_64x1000", t, n == 64_000 && sum == want, &format!("n={n}"));
}

async fn many_connections() {
    let t = Instant::now();
    let path = "/tmp/tk-stress.sock";
    let _ = std::fs::remove_file(path);
    let l = tokio::net::UnixListener::bind(path).unwrap();
    const CLIENTS: usize = 100;
    let srv = tokio::spawn(async move {
        let mut hs = vec![];
        for _ in 0..CLIENTS {
            let (mut s, _) = l.accept().await.unwrap();
            hs.push(tokio::spawn(async move {
                let mut buf = [0u8; 64];
                let mut echoed = 0usize;
                loop {
                    let n = s.read(&mut buf).await.unwrap();
                    if n == 0 { break; }
                    s.write_all(&buf[..n]).await.unwrap();
                    echoed += n;
                }
                echoed
            }));
        }
        let mut total = 0; for h in hs { total += h.await.unwrap(); }
        total
    });
    let cl: Vec<_> = (0..CLIENTS).map(|i| tokio::spawn(async move {
        let mut c = tokio::net::UnixStream::connect(path).await.unwrap();
        let mut ok = true;
        for m in 0..100u32 {
            let msg = [(i as u8) ^ (m as u8); 64];
            c.write_all(&msg).await.unwrap();
            let mut b = [0u8; 64];
            c.read_exact(&mut b).await.unwrap();
            ok &= b == msg;
        }
        ok
    })).collect();
    let mut ok = true; for c in cl { ok &= c.await.unwrap(); }
    let echoed = srv.await.unwrap();
    report("unix_100_clients_x100_msgs", t, ok && echoed == CLIENTS * 100 * 64, &format!("echoed={echoed}"));
}

async fn throughput() {
    let t = Instant::now();
    let (mut a, mut b) = tokio::net::UnixStream::pair().unwrap();
    const TOTAL: usize = 32 * 1024 * 1024;
    let w = tokio::spawn(async move {
        let chunk: Vec<u8> = (0..65536u32).map(|i| (i % 251) as u8).collect();
        let mut sent = 0;
        while sent < TOTAL { a.write_all(&chunk).await.unwrap(); sent += chunk.len(); }
        a.shutdown().await.unwrap();
    });
    let mut buf = vec![0u8; 65536];
    let (mut got, mut sum) = (0usize, 0u64);
    loop {
        let n = b.read(&mut buf).await.unwrap();
        if n == 0 { break; }
        got += n;
        sum += buf[..n].iter().map(|&x| x as u64).sum::<u64>();
    }
    w.await.unwrap();
    let per_chunk: u64 = (0..65536u32).map(|i| (i % 251) as u64).sum();
    let ms = t.elapsed().as_millis().max(1);
    report("throughput_32MiB", t, got == TOTAL && sum == per_chunk * (TOTAL / 65536) as u64, &format!("{} MiB/s", 32 * 1000 / ms));
}

async fn processes() {
    let t = Instant::now();
    let mut child = tokio::process::Command::new("/tmp/bin/seq").args(["1", "20000"]).stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let (mut n, mut sum) = (0u64, 0u64);
    while let Some(l) = lines.next_line().await.unwrap() { n += 1; sum += l.parse::<u64>().unwrap(); }
    let st = child.wait().await.unwrap();
    report("process_seq_20000_lines", t, n == 20_000 && sum == 200_010_000 && st.success(), &format!("lines={n}"));

    let t = Instant::now();
    let hs: Vec<_> = (0..12).map(|i| tokio::spawn(async move {
        let o = tokio::process::Command::new("/tmp/bin/echo").arg(format!("child-{i}")).output().await.unwrap();
        String::from_utf8_lossy(&o.stdout).trim() == format!("child-{i}") && o.status.success()
    })).collect();
    let mut ok = true; for h in hs { ok &= h.await.unwrap(); }
    report("process_12_concurrent", t, ok, "");

    let t = Instant::now();
    let mut sl = tokio::process::Command::new("/tmp/bin/sleep").arg("30").spawn().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    sl.kill().await.unwrap();
    let st = sl.wait().await.unwrap();
    report("process_kill", t, !st.success(), &format!("status={st}"));
}

async fn sync_primitives() {
    let t = Instant::now();
    let m = Arc::new(Mutex::new(0u64));
    let rw = Arc::new(RwLock::new(0u64));
    let sem = Arc::new(Semaphore::new(4));
    let peak = Arc::new(AtomicU64::new(0));
    let cur = Arc::new(AtomicU64::new(0));
    let hs: Vec<_> = (0..64).map(|_| {
        let (m, rw, sem, peak, cur) = (m.clone(), rw.clone(), sem.clone(), peak.clone(), cur.clone());
        tokio::spawn(async move {
            for _ in 0..100 {
                *m.lock().await += 1;
                *rw.write().await += 1;
                let _r = rw.read().await;
            }
            let _p = sem.acquire().await.unwrap();
            let c = cur.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(c, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
            cur.fetch_sub(1, Ordering::SeqCst);
        })
    }).collect();
    for h in hs { h.await.unwrap(); }
    let ok = *m.lock().await == 6400 && *rw.read().await == 6400 && peak.load(Ordering::SeqCst) <= 4;
    report("locks_semaphore_64_tasks", t, ok, &format!("peak_permits={}", peak.load(Ordering::SeqCst)));

    let t = Instant::now();
    let (btx, _) = broadcast::channel::<u32>(256);
    let rs: Vec<_> = (0..16).map(|_| { let mut r = btx.subscribe(); tokio::spawn(async move { let mut s = 0u64; while let Ok(v) = r.recv().await { s += v as u64; if v == 99 { break; } } s }) }).collect();
    for v in 0..100u32 { btx.send(v).unwrap(); tokio::task::yield_now().await; }
    let mut ok = true; for r in rs { ok &= r.await.unwrap() == 4950; }
    let (wtx, mut wrx) = watch::channel(0u32);
    let w = tokio::spawn(async move { while wrx.changed().await.is_ok() { if *wrx.borrow() == 50 { return true; } } false });
    for v in 1..=50 { wtx.send(v).unwrap(); tokio::time::sleep(Duration::from_millis(1)).await; }
    ok &= w.await.unwrap();
    report("broadcast_16_and_watch", t, ok, "");
}

async fn select_and_abort() {
    let t = Instant::now();
    let (tx, mut rx) = mpsc::channel::<u32>(1);
    let mut wins = (0, 0);
    tokio::spawn(async move { for i in 0..20 { tokio::time::sleep(Duration::from_millis(5)).await; let _ = tx.send(i).await; } });
    let mut tick = tokio::time::interval(Duration::from_millis(7));
    let mut got = 0;
    while got < 20 {
        tokio::select! {
            Some(_) = rx.recv() => { got += 1; wins.0 += 1; }
            _ = tick.tick() => { wins.1 += 1; }
        }
    }
    report("select_channel_vs_interval", t, got == 20 && wins.1 > 0, &format!("channel={} ticks={}", wins.0, wins.1));

    let t = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for i in 0..1000u32 { set.spawn(async move { tokio::time::sleep(Duration::from_millis(50 + (i % 50) as u64)).await; i }); }
    tokio::time::sleep(Duration::from_millis(20)).await;
    let mut kept = 0u32;
    let mut n = 0;
    while let Some(r) = set.join_next().await {
        n += 1;
        if n == 500 { set.abort_all(); }
        if r.is_ok() { kept += 1; }
    }
    report("joinset_abort_all", t, kept >= 500 && kept < 1000, &format!("completed={kept}"));
}

async fn blocking_and_fs() {
    let t = Instant::now();
    let hs: Vec<_> = (0..64u32).map(|i| tokio::task::spawn_blocking(move || { std::thread::sleep(Duration::from_millis(20)); i })).collect();
    let mut sum = 0; for h in hs { sum += h.await.unwrap(); }
    report("spawn_blocking_64", t, sum == 2016, "");

    let t = Instant::now();
    let dir = "/tmp/tk-fs";
    let _ = tokio::fs::remove_dir_all(dir).await;
    tokio::fs::create_dir_all(dir).await.unwrap();
    let hs: Vec<_> = (0..200u32).map(|i| tokio::spawn(async move {
        let p = format!("{dir}/f{i}");
        let data = vec![(i % 251) as u8; 1000 + i as usize];
        tokio::fs::write(&p, &data).await.unwrap();
        let back = tokio::fs::read(&p).await.unwrap();
        back == data
    })).collect();
    let mut ok = true; for h in hs { ok &= h.await.unwrap(); }
    let mut rd = tokio::fs::read_dir(dir).await.unwrap();
    let mut count = 0; while let Some(_) = rd.next_entry().await.unwrap() { count += 1; }
    tokio::fs::remove_dir_all(dir).await.unwrap();
    report("fs_200_files", t, ok && count == 200, &format!("entries={count}"));
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    rt.block_on(async {
        println!("TK stress start");
        many_tasks().await;
        many_timers().await;
        fan_in().await;
        many_connections().await;
        throughput().await;
        processes().await;
        sync_primitives().await;
        select_and_abort().await;
        blocking_and_fs().await;
        println!("TK DONE");
    });
}
