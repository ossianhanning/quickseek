//! A real agent on a temp folder, talked to over TLS like the PC does:
//! pairing, the full transfer, live changes (inotify and, as root,
//! fanotify on Linux; re-walks elsewhere), resuming, refusals. The
//! `bench` test (ignored) measures the walk and event latency.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use quickseek_agent::watch::Kind;
use quickseek_agent::{Options, Running};
use quickseek_remote::client::{self, Error, Session};
use quickseek_remote::wire::{Msg, Op, Record, decode_records};

const T: Duration = Duration::from_secs(10);

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("qs-live-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("share").join("docs")).unwrap();
    std::fs::write(d.join("share").join("docs").join("a.txt"), b"hello").unwrap();
    std::fs::write(d.join("share").join("b.bin"), vec![1u8; 4096]).unwrap();
    settle();
    d
}

/// On btrfs a tree search sees new entries and sizes once they're committed
/// (the agent's minute-by-minute catch-ups pick up later ones); tests that
/// index right after writing sync first.
fn settle() {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn sync();
        }
        unsafe { sync() };
    }
}

fn start(d: &Path, watch: Kind) -> Running {
    // The agent's log, when RUST_LOG asks for it.
    let _ = env_logger::builder().is_test(true).try_init();
    quickseek_agent::start(Options {
        shares: vec![("share".into(), d.join("share"))],
        config: d.join("config"),
        listen: "127.0.0.1".into(),
        port: 0,
        watch,
        threads: 2,
        rewalk_hours: 0,
        name: "testnas".into(),
        ..Default::default()
    })
    .unwrap()
}

fn code(d: &Path) -> String {
    let s = quickseek_agent::state::Store::new(&d.join("config")).unwrap();
    s.code()
        .map(|c| c.code)
        .unwrap_or_else(|| s.new_code().unwrap().code)
}

fn pair(d: &Path, r: &Running) -> client::Pairing {
    client::pair("127.0.0.1", r.port, &code(d), "TESTPC", "test", T).unwrap()
}

fn connect(r: &Running, p: &client::Pairing) -> Session {
    client::connect(
        "127.0.0.1",
        r.port,
        p.fingerprint,
        p.token,
        "TESTPC",
        "test",
        T,
    )
    .unwrap()
}

/// Subscribes and reads the full transfer (or nothing if resuming works).
fn subscribe(s: &mut Session, epoch: u64, seq: u64) -> Option<(u64, u64, Vec<Record>)> {
    s.send(&Msg::Subscribe {
        share: "share".into(),
        epoch,
        seq,
    })
    .unwrap();
    loop {
        match s.recv().unwrap() {
            Msg::Full { epoch, seq, count } => {
                let mut z = Vec::new();
                loop {
                    match s.recv().unwrap() {
                        Msg::Chunk(c) => z.extend(c),
                        Msg::FullEnd => break,
                        m => panic!("in a full transfer: {m:?}"),
                    }
                }
                return Some((epoch, seq, decode_records(&z, count).unwrap()));
            }
            Msg::Changes { .. } => return None,
            Msg::State(_) | Msg::Disk { .. } | Msg::Ping => {
                if epoch != 0 {
                    return None;
                }
            }
            m => panic!("unexpected {m:?}"),
        }
    }
}

/// Reads changes until `pred` holds for one op; returns ops seen and the last seq.
fn until(s: &mut Session, mut pred: impl FnMut(&Op) -> bool) -> (Vec<Op>, u64) {
    let t0 = Instant::now();
    let mut seen = Vec::new();
    while t0.elapsed() < T {
        if let Msg::Changes { seq, ops } = s.recv().unwrap() {
            let hit = ops.iter().any(&mut pred);
            seen.extend(ops);
            if hit {
                return (seen, seq);
            }
        }
    }
    panic!("no matching change within {T:?}: {seen:?}");
}

fn created(name: &str) -> impl FnMut(&Op) -> bool + '_ {
    move |o| matches!(o, Op::Create(r) if r.name == name)
}

/// Writes a file and waits for its create; returns the latency.
fn change_round(s: &mut Session, r: &Running, dir: &Path, name: &str, watched: bool) -> Duration {
    let t0 = Instant::now();
    std::fs::write(dir.join(name), b"new").unwrap();
    if !watched {
        settle();
        r.rewalk_now
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    until(s, created(name));
    t0.elapsed()
}

fn live_changes(watch: Kind, expect: &str) {
    let d = tmp(&format!("{watch:?}"));
    let r = start(&d, watch);
    let p = pair(&d, &r);
    assert_eq!(p.agent, "testnas");
    assert_eq!(p.shares[0].name, "share");
    let mut s = connect(&r, &p);
    let (epoch, _seq, recs) = subscribe(&mut s, 0, 0).expect("a full transfer");
    let mut names: Vec<&str> = recs.iter().map(|r| r.name.as_str()).collect();
    names.sort();
    assert_eq!(names, ["a.txt", "b.bin", "docs"]);
    assert_eq!(recs.iter().find(|r| r.name == "b.bin").unwrap().size, 4096);
    let watched = watch != Kind::None;
    let w = *r.agent.watcher.lock().unwrap();
    assert!(w.starts_with(expect), "watcher {w}, wanted {expect}");
    let share = d.join("share");
    let lat = change_round(&mut s, &r, &share.join("docs"), "c.txt", watched);
    eprintln!(
        "{watch:?}: change visible to the client after {} ms",
        lat.as_millis()
    );
    // A new folder with contents, then a rename of it, then a delete.
    std::fs::create_dir_all(share.join("new").join("deep")).unwrap();
    std::fs::write(share.join("new").join("deep").join("x.txt"), b"x").unwrap();
    if !watched {
        settle();
        r.rewalk_now
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let (ops, _) = until(&mut s, created("x.txt"));
    let new_id = ops
        .iter()
        .find_map(|o| match o {
            Op::Create(r) if r.name == "new" => Some(r.id),
            _ => None,
        })
        .unwrap();
    std::fs::rename(share.join("new"), share.join("docs").join("renamed")).unwrap();
    if !watched {
        settle();
        r.rewalk_now
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if watched {
        // A watcher sees a rename: the id survives.
        until(
            &mut s,
            |o| matches!(o, Op::Move { id, name, .. } if *id == new_id && name == "renamed"),
        );
    } else {
        // A re-walk sees a new folder; on btrfs the catch-up after a re-read
        // recognises the folder by its inode and moves it (keeping its id).
        until(&mut s, |o| match o {
            Op::Create(r) => r.name == "renamed",
            Op::Move { id, name, .. } => *id == new_id && name == "renamed",
            _ => false,
        });
    }
    std::fs::remove_file(share.join("b.bin")).unwrap();
    if !watched {
        settle();
        r.rewalk_now
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let b = recs.iter().find(|r| r.name == "b.bin").unwrap().id;
    let (_, seq) = until(&mut s, |o| *o == Op::Remove { id: b });
    drop(s);
    // Reconnecting with the last seq resumes: no full transfer.
    let mut s = connect(&r, &p);
    assert!(subscribe(&mut s, epoch, seq).is_none());
    let lat = change_round(&mut s, &r, &share, "after-resume.txt", watched);
    eprintln!(
        "{watch:?}: after resuming, a change took {} ms",
        lat.as_millis()
    );
    // A wrong epoch gets a full transfer with everything current.
    let mut s = connect(&r, &p);
    let (_, _, recs) = subscribe(&mut s, epoch ^ 1, seq).expect("full transfer");
    assert!(recs.iter().any(|r| r.name == "after-resume.txt"));
    assert!(recs.iter().any(|r| r.name == "renamed"));
    assert!(!recs.iter().any(|r| r.name == "b.bin"));
    drop(r);
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn rewalks_without_a_watcher() {
    live_changes(Kind::None, "none");
}

#[cfg(target_os = "linux")]
#[test]
fn inotify_live() {
    live_changes(Kind::Inotify, "inotify");
}

/// Needs CAP_SYS_ADMIN (root); skipped otherwise.
#[cfg(target_os = "linux")]
#[test]
fn fanotify_live() {
    if unsafe { libc_geteuid() } != 0 {
        eprintln!("fanotify_live: skipped (not root)");
        return;
    }
    live_changes(Kind::Fanotify, "fanotify");
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

/// The agent saves its index; after a restart it catches up (on btrfs from
/// its trees, else with a walk) and a PC in step with the save resumes:
/// only the changes made meanwhile arrive, no full transfer.
#[test]
fn restart_resumes_from_the_saved_index() {
    let d = tmp("restart");
    let r = start(&d, Kind::None);
    let p = pair(&d, &r);
    let mut s = connect(&r, &p);
    let (epoch, seq, recs) = subscribe(&mut s, 0, 0).expect("a full transfer");
    let a = recs.iter().find(|r| r.name == "a.txt").unwrap().id;
    // The first index is saved once it's done.
    let saved = d.join("config").join("index-share.idx");
    let t0 = Instant::now();
    while !saved.exists() {
        assert!(t0.elapsed() < T, "no saved index");
        std::thread::sleep(Duration::from_millis(20));
    }
    let port = r.port;
    drop(s);
    drop(r);
    // Changes while it's down.
    std::fs::write(d.join("share").join("while-down.txt"), b"new").unwrap();
    std::fs::remove_file(d.join("share").join("docs").join("a.txt")).unwrap();
    settle();
    let mut r = quickseek_agent::start(Options {
        shares: vec![("share".into(), d.join("share"))],
        config: d.join("config"),
        listen: "127.0.0.1".into(),
        port,
        watch: Kind::None,
        threads: 2,
        rewalk_hours: 0,
        name: "testnas".into(),
        ..Default::default()
    })
    .unwrap();
    let mut s = connect(&r, &p);
    assert!(
        subscribe(&mut s, epoch, seq).is_none(),
        "resumed, no full transfer"
    );
    let (ops, mut seq) = until(&mut s, created("while-down.txt"));
    if !ops.contains(&Op::Remove { id: a }) {
        let (ops2, q) = until(&mut s, |o| *o == Op::Remove { id: a });
        assert!(ops2.contains(&Op::Remove { id: a }));
        seq = q;
    }
    // Saved again (as SIGTERM does) and restarted with nothing changed: the
    // PC resumes again, and once more after a restart with no changes at all
    // (it's told the sequence number to resume from).
    for round in 0..2 {
        r.agent.shares[0].save(&d.join("config")).unwrap();
        drop(s);
        drop(r);
        let r2 = quickseek_agent::start(Options {
            shares: vec![("share".into(), d.join("share"))],
            config: d.join("config"),
            listen: "127.0.0.1".into(),
            port,
            watch: Kind::None,
            threads: 2,
            rewalk_hours: 0,
            name: "testnas".into(),
            ..Default::default()
        })
        .unwrap();
        s = connect(&r2, &p);
        s.send(&Msg::Subscribe {
            share: "share".into(),
            epoch,
            seq,
        })
        .unwrap();
        match s.recv().unwrap() {
            Msg::Changes { seq: q, ops } => {
                assert!(ops.is_empty(), "round {round}: {ops:?}");
                seq = q;
            }
            m => panic!("round {round}: wanted the resume point, got {m:?}"),
        }
        r = r2;
    }
    drop(r);
    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn refuses_wrong_codes_unpaired_tokens_and_other_certificates() {
    let d = tmp("refuse");
    let r = start(&d, Kind::None);
    let _ = code(&d);
    match client::pair("127.0.0.1", r.port, "AAAA-AAAA", "PC", "test", T) {
        Err(Error::Refused(e)) => assert!(e.contains("wrong pairing code"), "{e}"),
        other => panic!("{:?}", other.map(|p| p.agent)),
    }
    let p = pair(&d, &r);
    match client::connect("127.0.0.1", r.port, p.fingerprint, [0; 32], "PC", "test", T) {
        Err(Error::Refused(e)) => assert!(e.contains("not paired"), "{e}"),
        other => panic!("{:?}", other.map(|s| s.agent)),
    }
    let mut other = p.fingerprint;
    other[0] ^= 1;
    assert!(matches!(
        client::connect("127.0.0.1", r.port, other, p.token, "PC", "test", T),
        Err(Error::PinMismatch)
    ));
    // Revoked: refused.
    quickseek_agent::state::Store::new(&d.join("config"))
        .unwrap()
        .revoke("TESTPC")
        .unwrap();
    assert!(matches!(
        client::connect("127.0.0.1", r.port, p.fingerprint, p.token, "PC", "test", T),
        Err(Error::Refused(_))
    ));
    drop(r);
    std::fs::remove_dir_all(&d).ok();
}

/// Times the first walk, the full transfer and event latency on a
/// synthetic tree: `QS_BENCH_FILES` files (default 590,000) in folders of
/// 12, under `QS_BENCH_DIR` (default the temp folder).
/// `cargo test -p quickseek-agent --release --test live bench -- --ignored --nocapture`
#[test]
#[ignore]
fn bench() {
    let n: usize = std::env::var("QS_BENCH_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(590_000);
    let base = std::env::var("QS_BENCH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let d = base.join(format!("qs-bench-{n}"));
    let share = d.join("share");
    if !share.join(".complete").exists() {
        let t0 = Instant::now();
        let _ = std::fs::remove_dir_all(&share);
        for i in 0..n {
            let dir = share
                .join(format!("d{}", i / 1200))
                .join(format!("e{}", (i / 12) % 100));
            if i % 12 == 0 {
                std::fs::create_dir_all(&dir).unwrap();
            }
            std::fs::write(dir.join(format!("file {i}.dat")), b"").unwrap();
        }
        std::fs::write(share.join(".complete"), b"").unwrap();
        eprintln!("made {n} files in {:.1} s", t0.elapsed().as_secs_f64());
    }
    let watch = match std::env::var("QS_BENCH_WATCH").as_deref() {
        Ok("fanotify") => Kind::Fanotify,
        Ok("inotify") => Kind::Inotify,
        _ => Kind::Auto,
    };
    let _ = std::fs::remove_dir_all(d.join("config"));
    let t0 = Instant::now();
    let r = start(&d, watch);
    let p = pair(&d, &r);
    let mut s = connect(&r, &p);
    let (_, _, recs) = subscribe(&mut s, 0, 0).unwrap();
    let ready = t0.elapsed();
    eprintln!(
        "{} entries: agent start to full replica on the client in {:.2} s (two walks + transfer), watcher {}",
        recs.len(),
        ready.as_secs_f64(),
        r.agent.watcher.lock().unwrap()
    );
    let t1 = Instant::now();
    let (_, _, recs2) = subscribe(&mut connect(&r, &p), 0, 0).unwrap();
    eprintln!(
        "a second full transfer: {} entries in {} ms",
        recs2.len(),
        t1.elapsed().as_millis()
    );
    let mut lats = Vec::new();
    for i in 0..20 {
        lats.push(change_round(
            &mut s,
            &r,
            &share.join("d0").join("e0"),
            &format!("latency {i}.txt"),
            true,
        ));
        std::thread::sleep(Duration::from_millis(50));
    }
    lats.sort();
    eprintln!(
        "event latency (write → op at the client), 20 changes: median {} ms, max {} ms",
        lats[10].as_millis(),
        lats[19].as_millis()
    );
    // A re-walk of everything doesn't hold up live changes.
    r.rewalk_now
        .store(true, std::sync::atomic::Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(300));
    let mut during = Vec::new();
    for i in 0..5 {
        during.push(change_round(
            &mut s,
            &r,
            &share.join("d0").join("e0"),
            &format!("during walk {i}.txt"),
            true,
        ));
    }
    during.sort();
    eprintln!(
        "while a full re-walk runs, 5 changes: median {} ms, max {} ms",
        during[2].as_millis(),
        during[4].as_millis()
    );
    for i in 0..5 {
        std::fs::remove_file(
            share
                .join("d0")
                .join("e0")
                .join(format!("during walk {i}.txt")),
        )
        .ok();
    }
    let t2 = Instant::now();
    let burst = share.join("burst");
    std::fs::create_dir_all(&burst).unwrap();
    for i in 0..10_000 {
        std::fs::write(burst.join(format!("b{i}")), b"").unwrap();
    }
    until(&mut s, created("b9999"));
    eprintln!(
        "a 10,000-file burst reached the client {} ms after it started",
        t2.elapsed().as_millis()
    );
    std::fs::remove_dir_all(&burst).ok();
    for i in 0..20 {
        std::fs::remove_file(share.join("d0").join("e0").join(format!("latency {i}.txt"))).ok();
    }
}
