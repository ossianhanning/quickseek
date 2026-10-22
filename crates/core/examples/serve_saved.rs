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
//!
//! `file.db@D` serves a file as drive D (to serve one index twice, for
//! scale). `--churn N` renames N entries after ranking, as a live service has
//! entries changed since its last ranking (refreshed after 0.5% or so).

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
    let mut churn = 0u32;
    if let Some(i) = args.iter().position(|a| a == "--churn") {
        churn = args[i + 1].parse().expect("--churn <n>");
        args.drain(i..=i + 1);
    }
    let t = Instant::now();
    let mut ixs = Vec::new();
    for arg in &args {
        let (p, letter) = match arg.rsplit_once('@') {
            Some((p, l)) if l.len() == 1 => (p, l.chars().next()),
            _ => (arg.as_str(), None),
        };
        let t1 = Instant::now();
        let mut ix = persist::load(std::path::Path::new(p)).unwrap_or_else(|e| panic!("{p}: {e}"));
        if let Some(l) = letter {
            ix.letter = l;
        }
        let loaded = t1.elapsed();
        ix.compute_ranks();
        println!(
            "{p}: {} entries, loaded in {:.2} s, ranked in {:.2} s",
            ix.entries.len(),
            loaded.as_secs_f64(),
            (t1.elapsed() - loaded).as_secs_f64()
        );
        // Same entries in both builds: every (n / churn)-th live file.
        let n = ix.entries.len() as u32;
        if let Some(step) = n.checked_div(churn) {
            let step = step.max(1);
            let mut done = 0;
            let mut i = 1;
            while done < churn && i < n {
                if ix.is_live(i) && !ix.entries[i as usize].is_dir() {
                    let name = format!("{}~", ix.name(i));
                    let parent = ix.entries[i as usize].parent;
                    ix.rename_entry(i, parent, &name);
                    done += 1;
                }
                i += step;
            }
            println!("  renamed {done} entries after ranking");
        }
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
