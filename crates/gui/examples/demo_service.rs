//! The demo PC (see `tests/common/demo.rs`) as a quickseek service, for
//! screenshots and trying the GUI or the MCP server without your own files:
//!
//! ```text
//! cargo run --release -p quickseek-gui --example demo_service
//! set QUICKSEEK_PIPE=\\.\pipe\quickseek-demo
//! quickseek-gui        (or quickseek-mcp, quickseek search ...)
//! ```
//!
//! `--pipe <name>` serves on another pipe. Drive sizes and free space are the
//! demo PC's, not this machine's.

#[path = "../tests/common/mod.rs"]
mod common;

use std::sync::Arc;

use quickseek_core::engine::Engine;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let pipe = args
        .iter()
        .position(|a| a == "--pipe")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| r"\\.\pipe\quickseek-demo".into());
    let t = std::time::Instant::now();
    let pc = common::demo::build(quickseek_core::history::now_unix());
    quickseek_core::tree::override_disk_space(pc.drives());
    let engine = Arc::new(Engine::from_indexes(pc.indexes()));
    let history = std::env::temp_dir().join("quickseek-demo-history");
    let _ = std::fs::remove_dir_all(&history);
    engine.enable_history(history.clone());
    pc.seed_history(engine.history().expect("history on"));
    common::serve(&pipe, engine.clone());
    let entries: usize = engine
        .volumes()
        .iter()
        .map(|v| v.index.read().entries.len())
        .sum();
    println!(
        "demo PC: {entries} entries on C:, D:, E:, {} snapshots, built in {:.1} s; serving on {pipe}",
        engine.history().map_or(0, |h| h.list().len()),
        t.elapsed().as_secs_f64()
    );
    loop {
        std::thread::park();
    }
}
