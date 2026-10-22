//! Serves saved indexes (`*.db`, e.g. `%LOCALAPPDATA%\rapidfind\C.db`) on a
//! private pipe, the way the service would with the same data, so a
//! performance change can be measured on real data without administrator
//! rights or reinstalling the service. No journal: the index doesn't change.
//!
//! ```text
//! cargo run --release -p rapidfind-core --example serve_saved -- C.db F.db S.db
//! set RAPIDFIND_PIPE=\\.\pipe\rapidfind-bench
//! cargo run --release -p rapidfind-core --example ipc_bench -- "*.pdf"
//! ```
//!
//! Prints the load time (load + derived data + sort ranks) and the process's
//! memory once ready.

use std::sync::{Arc, OnceLock};
use std::time::Instant;

use rapidfind_core::engine::Engine;
use rapidfind_core::{ipc, persist};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut pipe = r"\\.\pipe\rapidfind-bench".to_string();
    if let Some(i) = args.iter().position(|a| a == "--pipe") {
        pipe = args[i + 1].clone();
        args.drain(i..=i + 1);
    }
    let t = Instant::now();
    let mut ixs = Vec::new();
    for p in &args {
        let t1 = Instant::now();
        let mut ix = persist::load(std::path::Path::new(p)).unwrap_or_else(|e| panic!("{p}: {e}"));
        let loaded = t1.elapsed();
        ix.compute_ranks();
        println!(
            "{p}: {} entries, loaded in {:.2} s, ranked in {:.2} s",
            ix.entries.len(),
            loaded.as_secs_f64(),
            (t1.elapsed() - loaded).as_secs_f64()
        );
        println!("  memory: {}", ix.memory_breakdown());
        ixs.push(ix);
    }
    let engine = Arc::new(Engine::from_indexes(ixs));
    println!("ready in {:.2} s", t.elapsed().as_secs_f64());
    let slot = Arc::new(OnceLock::new());
    let _ = slot.set(engine.clone());
    let p = pipe.clone();
    std::thread::spawn(move || {
        if let Err(e) = ipc::serve_on(&p, ipc::engine_handler(slot)) {
            eprintln!("serving {p}: {e}");
            std::process::exit(1);
        }
    });
    println!("serving on {pipe}");
    loop {
        std::thread::park();
    }
}
