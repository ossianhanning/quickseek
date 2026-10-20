//! Synthetic index benchmark (no admin needed): `cargo run --release --example synth`.

use std::time::Instant;

use rapidfind_core::index::Index;
use rapidfind_core::ntfs::{Meta, Scan, ScanName};
use rapidfind_core::query::Query;
use rapidfind_core::search::{SearchOptions, search};

fn main() {
    let n: u32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(3_300_000);
    let words = ["Program", "Files", "windows", "system32", "users", "data", "cache", "node_modules", "src", "build",
        "Microsoft", "Google", "report", "image", "photo", "backup", "config", "library", "assets", "temp"];
    let exts = ["dll", "exe", "txt", "png", "jpg", "json", "rs", "js", "h", "cpp", "mui", "dat"];
    let mut rng = 0x9E3779B97F4A7C15u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut s = Scan { has_sizes: true, ..Default::default() };
    s.meta = vec![Meta::default(); n as usize + 30];
    s.meta[5] = Meta { in_use: true, is_dir: true, ..Default::default() };
    s.names.push(ScanName { record: 5, parent: 5, off: 0, len: 1 });
    s.strings.push(b'.');
    let mut dirs = vec![5u32];
    for r in 30..n + 30 {
        let is_dir = next() % 10 == 0;
        let parent = dirs[(next() % dirs.len() as u64) as usize];
        let mut name = format!("{}_{}{}", words[(next() % 20) as usize], words[(next() % 20) as usize], next() % 10000);
        if !is_dir {
            name.push('.');
            name.push_str(exts[(next() % exts.len() as u64) as usize]);
        }
        s.meta[r as usize] = Meta { in_use: true, is_dir, size: next() % 1_000_000, modified: (next() % 1_000_000) as i64, ..Default::default() };
        s.names.push(ScanName { record: r, parent, off: s.strings.len() as u32, len: name.len() as u32 });
        s.strings.extend_from_slice(name.as_bytes());
        if is_dir {
            dirs.push(r);
        }
    }
    let t = Instant::now();
    let ix = Index::from_scan('Z', &s);
    drop(s);
    println!("built {} entries in {} ms ({})", ix.len_live(), t.elapsed().as_millis(), ix.memory_breakdown());
    for q in ["a", "txt", "*.dll", "", "report photo", "ext:png size:>500000", r"users\data"] {
        let query = Query::parse(q).unwrap();
        let mut times = Vec::new();
        let mut total = 0;
        for _ in 0..15 {
            let h = search(&[&ix], &query, &SearchOptions::default());
            total = h.total;
            times.push(h.elapsed.as_secs_f64() * 1000.0);
        }
        times.sort_by(f64::total_cmp);
        println!("{q:>24}: {total:>8} hits, min {:.2} ms, median {:.2} ms", times[0], times[times.len() / 2]);
    }
}
