//! A fake rapidfind service over a synthetic index, for trying the GUI
//! without the real service: `cargo run -p rapidfind-gui --example mock_service`
//! then start the GUI with `RAPIDFIND_PIPE=\\.\pipe\rapidfind-mock`.
//! With `--live`, a new file appears every two seconds; `--folder <dir>` also
//! serves a real folder (to try content search and the preview pane).

#[path = "../tests/common/mod.rs"]
mod common;

use std::sync::Arc;
use std::time::Duration;

use rapidfind_core::engine::Engine;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let pipe = args.iter().position(|a| a == "--pipe").and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| r"\\.\pipe\rapidfind-mock".into());
    let filler = args.iter().position(|a| a == "--files").and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(200_000);
    let mut ixs = vec![common::sample_index('C', filler), common::sample_index('D', filler / 4)];
    if let Some(dir) = args.iter().position(|a| a == "--folder").and_then(|i| args.get(i + 1)) {
        ixs.push(rapidfind_core::folder::build(dir, rapidfind_core::fold::FoldMode::Lower));
    }
    let engine = Arc::new(Engine::from_indexes(ixs));
    common::serve(&pipe, engine.clone());
    println!("serving {} entries on {pipe}", filler + filler / 4);
    let live = args.iter().any(|a| a == "--live");
    let mut n = 0;
    loop {
        std::thread::sleep(Duration::from_secs(2));
        if live {
            let v = &engine.volumes()[0];
            let mut ix = v.index.write();
            let parent = ix.root;
            let rec = ix.rec_count;
            ix.add_link(rec, 0, parent, &format!("live-{n}.txt"), 0x20, n * 100, common::BASE_TIME, common::BASE_TIME);
            n += 1;
        }
    }
}
