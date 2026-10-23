//! Query latency as a client sees it: round trips to the Quickseek service
//! over its pipe (no admin needed), for tools/bench/bench.ps1.
//!
//! `cargo run --release -p quickseek-core --example ipc_bench -- --runs 15 "*.pdf" "ext:dll"`
//!
//! Prints one JSON line per query: total matches; the median, 10th and
//! 90th percentile, minimum and maximum of the client-measured round trip;
//! and the median of the service's own search time, in milliseconds. Results are sorted by name and the first
//! page is 100 rows, as a list view would ask.

use std::time::Instant;

use quickseek_core::ipc::{Backend, Client, Remote};
use quickseek_core::query::SearchFlags;
use quickseek_core::search::SortKey;

/// The `p`-th percentile of sorted `v` (nearest rank).
fn percentile(v: &[f64], p: f64) -> f64 {
    v[((v.len() - 1) as f64 * p / 100.0).round() as usize]
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut runs = 15;
    if let Some(i) = args.iter().position(|a| a == "--runs") {
        runs = args[i + 1].parse().expect("--runs <n>");
        args.drain(i..=i + 1);
    }
    let mut c = Remote(Client::connect().expect("the Quickseek service isn't running"));
    for q in &args {
        let (mut wall, mut server) = (Vec::new(), Vec::new());
        let mut total = 0;
        // One run first, not counted: it warms the service's caches as any
        // first query after start would.
        for i in 0..=runs {
            let t = Instant::now();
            let page = c
                .search(q, SearchFlags::default(), SortKey::Name, false, 100)
                .unwrap_or_else(|e| panic!("{q}: {e}"));
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            if i > 0 {
                wall.push(ms);
                server.push(page.elapsed_us as f64 / 1000.0);
            }
            total = page.total;
        }
        let med = median(&mut wall);
        let (lo, hi) = (wall[0], wall[wall.len() - 1]);
        let (p10, p90) = (percentile(&wall, 10.0), percentile(&wall, 90.0));
        println!(
            "{{\"query\":{:?},\"total\":{total},\"runs\":{runs},\"median_ms\":{med:.3},\"p10_ms\":{p10:.3},\"p90_ms\":{p90:.3},\"min_ms\":{lo:.3},\"max_ms\":{hi:.3},\"server_median_ms\":{:.3}}}",
            q,
            median(&mut server)
        );
    }
}
