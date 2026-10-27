//! The service's side of a NAS agent, end to end: a real `quickseek-agent`
//! on a temp folder (no change notification on Windows: the test asks it to
//! re-walk), paired, indexed as a `\\host\share` volume that replaces the
//! folder index of the same root, live changes, history from days the PC
//! missed, the saved replica serving while the agent is gone, and a
//! restart resuming without a full transfer.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use quickseek_agent::{Options, Running, watch::Kind};
use quickseek_core::config::{Config, FolderConfig};
use quickseek_core::engine::{Engine, EngineOptions, VolumeKind};
use quickseek_core::query::{Query, SearchFlags};
use quickseek_core::search::SearchOptions;
use quickseek_remote::wire::{AgentSnap, SnapFolder};

const KEY: &str = r"\\127.0.0.1\share";

fn start_agent(d: &Path, port: u16) -> Running {
    quickseek_agent::start(Options {
        shares: vec![("share".into(), d.join("nas"))],
        config: d.join("agent"),
        listen: "127.0.0.1".into(),
        port,
        watch: Kind::None,
        threads: 2,
        rewalk_hours: 0,
        name: "testnas".into(),
        ..Default::default()
    })
    .unwrap()
}

fn engine(d: &Path) -> Engine {
    let e = Engine::new(EngineOptions {
        config_path: Some(d.join("config.json")),
        ..Default::default()
    });
    e.enable_history(d.join("history"));
    e.sync_volumes();
    e
}

fn paths(e: &Engine, q: &str) -> Vec<String> {
    let hits = e.search(
        &Query::parse(q).unwrap(),
        &SearchOptions {
            limit: 0,
            ..Default::default()
        },
    );
    e.with_indexes(|ixs| {
        let mut v: Vec<String> = hits
            .items
            .iter()
            .map(|h| ixs[h.vol as usize].path(h.idx))
            .collect();
        v.sort();
        v
    })
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) -> Duration {
    let t0 = Instant::now();
    while !ok() {
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    t0.elapsed()
}

fn state(e: &Engine) -> String {
    e.status()
        .into_iter()
        .find(|s| s.key == KEY)
        .map(|s| s.state)
        .unwrap_or_default()
}

#[test]
fn nas_share_as_a_volume() {
    let d: PathBuf = std::env::temp_dir().join(format!("qs-remote-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("nas").join("Photos")).unwrap();
    std::fs::write(
        d.join("nas").join("Photos").join("beach.jpg"),
        vec![0u8; 3000],
    )
    .unwrap();
    std::fs::write(d.join("nas").join("todo.txt"), b"milk").unwrap();
    quickseek_core::persist::set_db_dir(d.join("db"));

    // The agent has a snapshot from two days ago (the PC was off then).
    let two_days_ago = quickseek_core::history::now_unix() - 2 * 86_400;
    quickseek_agent::snaps::save(
        &d.join("agent").join("history"),
        &AgentSnap {
            time: two_days_ago,
            share: "share".into(),
            capacity: 1000 << 30,
            free: 400 << 30,
            folders: vec![SnapFolder {
                parent: u32::MAX,
                name: String::new(),
                size: 77 << 20,
                files: 9,
            }],
            files: vec![],
        },
    )
    .unwrap();

    let agent = start_agent(&d, 0);
    let port = agent.port;
    let code = {
        let s = quickseek_agent::state::Store::new(&d.join("agent")).unwrap();
        s.code().unwrap().code
    };
    let rc = quickseek_core::remote::pair_with("127.0.0.1", port, &code).unwrap();
    assert_eq!(rc.agent, "testnas");
    assert_eq!(rc.shares[0].unc, KEY);
    // The same share was indexed over SMB before: the agent replaces it.
    let cfg = Config {
        auto_include_fixed: false,
        folders: vec![FolderConfig {
            path: KEY.into(),
            watch: false,
            rescan_minutes: 0,
        }],
        remotes: vec![rc],
        ..Default::default()
    };
    cfg.save(&d.join("config.json")).unwrap();

    let t0 = Instant::now();
    let e = engine(&d);
    let vols = e.status();
    assert_eq!(vols.len(), 1, "{vols:?}");
    assert_eq!(vols[0].kind, VolumeKind::Remote);
    wait_for("the full transfer", || paths(&e, "beach").len() == 1);
    eprintln!(
        "replica ready {} ms after the engine started",
        t0.elapsed().as_millis()
    );
    assert_eq!(paths(&e, "beach"), [format!(r"{KEY}\Photos\beach.jpg")]);
    assert_eq!(state(&e), "ready");
    // Folder sizes add up on the PC.
    let (_, ix) = e.lookup_path(KEY).unwrap();
    let size = e.with_indexes(|ixs| ixs[0].entries[ix as usize].size);
    assert_eq!(size, 3004);

    // A change on the NAS.
    std::fs::write(d.join("nas").join("Photos").join("sunset.jpg"), b"x").unwrap();
    let t1 = Instant::now();
    agent.rewalk_now.store(true, Ordering::Relaxed);
    wait_for("the new file", || paths(&e, "sunset").len() == 1);
    eprintln!(
        "a change on the NAS was searchable after {} ms (re-walk on request)",
        t1.elapsed().as_millis()
    );
    std::fs::remove_file(d.join("nas").join("todo.txt")).unwrap();
    agent.rewalk_now.store(true, Ordering::Relaxed);
    wait_for("the deletion", || paths(&e, "todo").is_empty());

    // History: the day the PC missed came from the agent.
    let h = e.history().unwrap();
    let series = e.series(&[KEY.to_string()]).unwrap();
    assert!(
        series.rows[0].sizes.contains(&(77 << 20)),
        "{:?} in {:?}",
        series.rows[0].sizes,
        h.list()
    );

    // The agent goes away: the replica keeps serving, the state says so.
    e.save_all(0).unwrap();
    drop(agent);
    wait_for("offline", || state(&e) == "offline");
    assert_eq!(paths(&e, "sunset").len(), 1);
    drop(e);

    // A restart without the agent serves the saved copy.
    let e = engine(&d);
    assert_eq!(paths(&e, "beach").len(), 1);
    wait_for("cached", || state(&e) == "cached");
    drop(e);

    // The agent comes back on the same port: a new epoch, so a full transfer.
    let agent = start_agent(&d, port);
    std::fs::write(d.join("nas").join("later.txt"), b"x").unwrap();
    let e = engine(&d);
    wait_for("reconnecting", || paths(&e, "later").len() == 1);
    assert!(paths(&e, "todo").is_empty());
    let _ = SearchFlags::default();
    drop(e);
    drop(agent);
    std::fs::remove_dir_all(&d).ok();
}
