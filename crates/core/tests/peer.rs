//! Two PCs searching each other, end to end in one process: engine A serves
//! its index ([`quickseek_core::peer`]), engine B pairs with it like with a
//! NAS agent and keeps a live replica. A serves a folder index here (an NTFS
//! one needs administrator rights); the path from index to replica is the
//! same: changes noted by the index, the agent's server and change log, TLS
//! with a pinned certificate, pairing, the replica on B.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use quickseek_core::config::{Config, FolderConfig, PeerConfig};
use quickseek_core::engine::{Engine, EngineOptions};
use quickseek_core::query::Query;
use quickseek_core::search::SearchOptions;
use serde_json::{Value, json};

fn tmp() -> PathBuf {
    let d = std::env::temp_dir().join(format!("qs-peer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn engine(config: &Path, c: &Config) -> Arc<Engine> {
    c.save(config).unwrap();
    let e = Arc::new(Engine::new(EngineOptions {
        config_path: Some(config.to_path_buf()),
        ..Default::default()
    }));
    e.sync_volumes();
    e
}

fn names(e: &Engine, q: &str) -> Vec<String> {
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

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !ok() {
        assert!(
            t0.elapsed() < Duration::from_secs(30),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn call(e: &Engine, a: Value) -> Value {
    let r = quickseek_core::remote::action(e, &a.to_string())
        .unwrap_or_else(|err| panic!("{a}: {err}"));
    serde_json::from_str(&r).unwrap()
}

#[test]
fn a_pc_follows_another_pcs_index() {
    let d = tmp();
    quickseek_core::persist::set_db_dir(d.join("db"));
    quickseek_core::peer::TEST_MODE.store(true, Ordering::Relaxed);
    let served = d.join("served");
    std::fs::create_dir_all(served.join("sub")).unwrap();
    std::fs::write(served.join("alpha.txt"), b"a").unwrap();
    std::fs::write(served.join("sub").join("beta.txt"), b"bb").unwrap();
    let port = free_port();

    // PC A: its folder index, shared.
    let a = engine(
        &d.join("a.json"),
        &Config {
            folders: vec![FolderConfig {
                path: served.display().to_string(),
                ..Default::default()
            }],
            auto_include_fixed: false,
            peer: PeerConfig {
                enabled: true,
                port,
                volumes: Vec::new(),
            },
            ..Default::default()
        },
    );
    wait_for("A's folder index", || names(&a, "beta.txt").len() == 1);
    quickseek_core::peer::keep(&a);
    let st = call(&a, json!({"op": "peer"}));
    assert_eq!(st["running"], true, "{st}");
    let code = call(&a, json!({"op": "peer_pair"}))["code"]
        .as_str()
        .unwrap()
        .to_string();

    // PC B pairs with A like with a NAS.
    let b = engine(
        &d.join("b.json"),
        &Config {
            auto_include_fixed: false,
            ..Default::default()
        },
    );
    let added = call(
        &b,
        json!({"op": "add", "host": "127.0.0.1", "port": port, "code": code}),
    );
    assert_eq!(added["shares"][0]["name"], "served$", "{added}");
    b.sync_volumes();
    let key = r"\\127.0.0.1\served$";
    wait_for("B's replica", || {
        names(&b, "beta.txt") == vec![format!(r"{key}\sub\beta.txt")]
    });
    assert_eq!(names(&b, "alpha.txt"), vec![format!(r"{key}\alpha.txt")]);
    // A paired PC shows on A.
    let st = call(&a, json!({"op": "peer"}));
    assert_eq!(st["clients"].as_array().unwrap().len(), 1, "{st}");

    // A change on A reaches B live.
    std::fs::write(served.join("sub").join("gamma.txt"), b"ccc").unwrap();
    std::fs::remove_file(served.join("alpha.txt")).unwrap();
    wait_for("the new file on B", || names(&b, "gamma.txt").len() == 1);
    wait_for("the deleted file gone on B", || {
        names(&b, "alpha.txt").is_empty()
    });

    // Only local volumes are served: B's replica of A isn't shared back.
    quickseek_core::peer::shut_down();
    let _ = std::fs::remove_dir_all(&d);
}
