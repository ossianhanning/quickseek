//! In-process search timing on a saved index, phase by phase, for profiling:
//! `cargo run --release -p rapidfind-core --example prof_search -- C.db "ext:dll" "a"`
//! Set RAPIDFIND_PROFILE=1 to print each phase of every search.

use std::time::Instant;

use rapidfind_core::engine::Engine;
use rapidfind_core::query::SearchFlags;
use rapidfind_core::search::SearchOptions;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (dbs, queries): (Vec<&String>, Vec<&String>) =
        args.iter().partition(|a| a.ends_with(".db"));
    let mut ixs = Vec::new();
    for p in dbs {
        let mut ix = rapidfind_core::persist::load(std::path::Path::new(p)).expect("load");
        ix.compute_ranks();
        if std::env::var_os("RAPIDFIND_PROFILE").is_some() {
            eprintln!(
                "{p}: {}; samples {} extra {} blocks {} late {} dead {} excl {} links {}+{} seqs {}",
                ix.memory_breakdown(),
                ix.fold_samples.capacity(),
                ix.fold_extra.capacity(),
                ix.rec_blocks.capacity(),
                ix.late_records.capacity(),
                ix.fold_dead.capacity(),
                ix.excl.capacity(),
                ix.more_links.capacity(),
                ix.late_links.capacity(),
                ix.seqs.capacity()
            );
        }
        ixs.push(ix);
    }
    let e = Engine::from_indexes(ixs);
    for q in queries {
        let mut t = Vec::new();
        let mut total = 0;
        for i in 0..16 {
            let s = Instant::now();
            let rs = e
                .query(q, SearchFlags::default(), &SearchOptions::default())
                .unwrap();
            total = rs.total;
            if i > 0 {
                t.push(s.elapsed().as_secs_f64() * 1000.0);
            }
        }
        t.sort_by(f64::total_cmp);
        println!(
            "{q:<28} {total:>9} hits  median {:>7.2} ms  min {:>7.2}",
            t[t.len() / 2],
            t[0]
        );
    }
}
