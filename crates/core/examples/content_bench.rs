//! Times content search on real files without the service (no elevation
//! needed): walks a folder into an index, then runs a query with
//! `content:` terms through the same scan the GUI and CLI use.
//!
//! `cargo run --release -p rapidfind-core --example content_bench -- S:\dev "*.rs content:unsafe" [runs]`

use std::sync::Arc;
use std::time::Instant;

use rapidfind_core::content::{self, ContentOptions};
use rapidfind_core::engine::Engine;
use rapidfind_core::fmt;
use rapidfind_core::fold::FoldMode;
use rapidfind_core::ipc::{Backend, Local};
use rapidfind_core::query::SearchFlags;
use rapidfind_core::search::SortKey;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (Some(root), Some(query)) = (args.get(1), args.get(2)) else {
        eprintln!("usage: content_bench <folder> <query> [runs]");
        std::process::exit(2);
    };
    let runs: usize = args.get(3).and_then(|r| r.parse().ok()).unwrap_or(3);
    let t = Instant::now();
    let ix = rapidfind_core::folder::build(root, FoldMode::Lower);
    println!(
        "indexed {} entries under {root} in {:.1} s",
        ix.len_live(),
        t.elapsed().as_secs_f64()
    );
    let mut be = Local::new(Arc::new(Engine::from_indexes(vec![ix])));
    let flags = SearchFlags::default();
    let (names, cq) = content::split_query(query, flags).expect("query");
    let cq = Arc::new(cq.expect("the query has no content: term"));
    for run in 1..=runs {
        let t = Instant::now();
        let page = be
            .search(&names, flags, SortKey::Name, false, 1)
            .expect("search");
        let name_ms = t.elapsed().as_secs_f64() * 1000.0;
        let (rows, st) = content::matching_rows(
            &mut |r| be.call(r),
            cq.clone(),
            ContentOptions::default(),
            None,
        )
        .expect("scan");
        let secs = st.elapsed.as_secs_f64();
        println!(
            "run {run}: {} name matches in {name_ms:.1} ms; read {} files ({}) in {secs:.2} s = {:.0} files/s, {}/s; {} contain it{}",
            page.total,
            st.checked - st.folders,
            fmt::size(st.bytes),
            (st.checked - st.folders) as f64 / secs,
            fmt::size((st.bytes as f64 / secs) as u64),
            rows.len(),
            if st.skipped().is_empty() {
                String::new()
            } else {
                format!("; skipped {}", st.skipped())
            },
        );
    }
}
