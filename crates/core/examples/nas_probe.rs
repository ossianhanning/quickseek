//! Tries a NAS agent from this PC without the service: pairs (or uses the
//! saved pairing), builds the replicas in-process exactly as the service
//! would, and reports timings, then prints changes as they arrive.
//!
//! cargo run --release -p quickseek-core --example nas_probe -- mynas.local --pair K7QF-3MZD
//! cargo run --release -p quickseek-core --example nas_probe -- mynas.local --watch 600
//!
//! State lives in %TEMP%\quickseek-nas-probe (or `--db DIR`).

use std::time::{Duration, Instant};

use quickseek_core::config::Config;
use quickseek_core::engine::{Engine, EngineOptions, VolumeKind};
use quickseek_core::query::Query;
use quickseek_core::search::SearchOptions;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut host = None;
    let mut code = None;
    let mut port = quickseek_remote::DEFAULT_PORT;
    let mut watch = 60u64;
    let mut db = std::env::temp_dir().join("quickseek-nas-probe");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pair" => code = it.next().cloned(),
            "--port" => port = it.next().unwrap().parse()?,
            "--watch" => watch = it.next().unwrap().parse()?,
            "--db" => db = it.next().unwrap().into(),
            h => host = Some(h.to_string()),
        }
    }
    let host = host.ok_or_else(|| {
        anyhow::anyhow!(
            "usage: nas_probe <host> [--pair CODE] [--port N] [--watch SECONDS] [--db DIR]"
        )
    })?;
    std::fs::create_dir_all(&db)?;
    quickseek_core::persist::set_db_dir(db.clone());
    let cfg_path = db.join("config.json");
    if let Some(code) = code {
        let t = Instant::now();
        let rc =
            quickseek_core::remote::pair_with(&host, port, &code).map_err(anyhow::Error::msg)?;
        println!(
            "paired with {} in {} ms; shares: {}",
            rc.agent,
            t.elapsed().as_millis(),
            rc.shares
                .iter()
                .map(|s| s.unc.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let cfg = Config {
            auto_include_fixed: false,
            remotes: vec![rc],
            ..Default::default()
        };
        cfg.save(&cfg_path)?;
    }
    let t0 = Instant::now();
    let e = Engine::new(EngineOptions {
        config_path: Some(cfg_path),
        ..Default::default()
    });
    e.enable_history(db.join("history"));
    e.sync_volumes();
    let remotes = || {
        e.status()
            .into_iter()
            .filter(|s| s.kind == VolumeKind::Remote)
            .collect::<Vec<_>>()
    };
    if remotes().is_empty() {
        anyhow::bail!("not paired: run with --pair CODE (from `quickseek-agent pair` on the NAS)");
    }
    let mut last = String::new();
    while !remotes().iter().all(|s| s.state == "ready") {
        let now: Vec<String> = remotes()
            .iter()
            .map(|s| format!("{} {} ({})", s.key, s.state, s.entries))
            .collect();
        let now = now.join("; ");
        if now != last {
            println!("{:>7} ms  {now}", t0.elapsed().as_millis());
            last = now;
        }
        std::thread::sleep(Duration::from_millis(20));
        if t0.elapsed() > Duration::from_secs(600) {
            anyhow::bail!("not ready after 10 minutes");
        }
    }
    println!("{:>7} ms  all ready", t0.elapsed().as_millis());
    for s in remotes() {
        println!("  {}  {} entries", s.key, s.entries);
    }
    let q = Instant::now();
    let hits = e.search(&Query::parse("*.mkv").unwrap(), &SearchOptions::default());
    println!(
        "search *.mkv: {} hits in {} ms",
        hits.total,
        q.elapsed().as_millis()
    );
    e.save_all(0)?;
    println!("watching for {watch} s");
    let mut seen = e.generation();
    let end = Instant::now() + Duration::from_secs(watch);
    while Instant::now() < end {
        std::thread::sleep(Duration::from_millis(10));
        let g = e.generation();
        if g != seen {
            seen = g;
            let recent = e.query(
                "",
                Default::default(),
                &SearchOptions {
                    sort: quickseek_core::search::SortKey::Modified,
                    descending: true,
                    ..Default::default()
                },
            );
            let first = recent.ok().and_then(|mut rs| {
                e.with_indexes(|ixs| {
                    rs.page(ixs, 0, 1)
                        .ok()
                        .and_then(|h| h.first().map(|h| ixs[h.vol as usize].path(h.idx)))
                })
            });
            let unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis());
            println!(
                "{unix_ms}  change received; newest entry: {}",
                first.unwrap_or_default()
            );
        }
    }
    e.save_all(0)?;
    Ok(())
}
