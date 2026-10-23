//! Synthetic index benchmark (no admin needed): `cargo run --release --example synth`.

use std::time::Instant;

use quickseek_core::index::Index;
use quickseek_core::ntfs::{Meta, Scan, ScanName};
use quickseek_core::query::Query;
use quickseek_core::search::{SearchOptions, search};

fn main() {
    let n: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(3_300_000);
    let words = [
        "Program",
        "Files",
        "windows",
        "system32",
        "users",
        "data",
        "cache",
        "node_modules",
        "src",
        "build",
        "Microsoft",
        "Google",
        "report",
        "image",
        "photo",
        "backup",
        "config",
        "library",
        "assets",
        "temp",
    ];
    let exts = [
        "dll", "exe", "txt", "png", "jpg", "json", "rs", "js", "h", "cpp", "mui", "dat",
    ];
    let mut rng = 0x9E3779B97F4A7C15u64;
    let mut next = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let mut s = Scan {
        has_sizes: true,
        ..Default::default()
    };
    s.meta = vec![Meta::default(); n as usize + 30];
    s.meta[5] = Meta {
        in_use: true,
        is_dir: true,
        ..Default::default()
    };
    s.names.push(ScanName {
        record: 5,
        parent: 5,
        off: 0,
        len: 1,
    });
    s.strings.push(b'.');
    let mut dirs = vec![5u32];
    for r in 30..n + 30 {
        let is_dir = next() % 10 == 0;
        let parent = dirs[(next() % dirs.len() as u64) as usize];
        let mut name = format!(
            "{}_{}{}",
            words[(next() % 20) as usize],
            words[(next() % 20) as usize],
            next() % 10000
        );
        if !is_dir {
            name.push('.');
            name.push_str(exts[(next() % exts.len() as u64) as usize]);
        }
        s.meta[r as usize] = Meta {
            in_use: true,
            is_dir,
            size: next() % 1_000_000,
            modified: (next() % 1_000_000) as i64,
            ..Default::default()
        };
        s.names.push(ScanName {
            record: r,
            parent,
            off: s.strings.len() as u32,
            len: name.len() as u32,
        });
        s.strings.extend_from_slice(name.as_bytes());
        if is_dir {
            dirs.push(r);
        }
    }
    let t = Instant::now();
    let ix = Index::from_scan('Z', &s);
    drop(s);
    println!(
        "built {} entries in {} ms ({}; {:.0} MB total)",
        ix.len_live(),
        t.elapsed().as_millis(),
        ix.memory_breakdown(),
        ix.memory_bytes() as f64 / 1048576.0
    );
    let mut ix = ix;
    let p = std::env::temp_dir().join("quickseek-synth.db");
    let t = Instant::now();
    quickseek_core::persist::save(&mut ix, &p).unwrap();
    let size = std::fs::metadata(&p).unwrap().len();
    println!(
        "saved {:.1} MB in {} ms",
        size as f64 / 1048576.0,
        t.elapsed().as_millis()
    );
    let t = Instant::now();
    let back = quickseek_core::persist::load(&p).unwrap();
    println!(
        "loaded in {} ms ({:.0} MB before ranks)",
        t.elapsed().as_millis(),
        back.memory_bytes() as f64 / 1048576.0
    );
    std::fs::remove_file(&p).ok();
    let ix = ix;
    for q in [
        "a",
        "txt",
        "*.dll",
        "",
        "report photo",
        "ext:png size:>500000",
        r"users\data",
    ] {
        let query = Query::parse(q).unwrap();
        let mut times = Vec::new();
        let mut total = 0;
        for _ in 0..15 {
            let h = search(&[&ix], &query, &SearchOptions::default());
            total = h.total;
            times.push(h.elapsed.as_secs_f64() * 1000.0);
        }
        times.sort_by(f64::total_cmp);
        println!(
            "{q:>24}: {total:>8} hits, min {:.2} ms, median {:.2} ms",
            times[0],
            times[times.len() / 2]
        );
    }
    for q in [r"users\*\*.txt", r"*\cache*\*.png", r"photo*\*\report*"] {
        let query = Query::parse(q).unwrap();
        let t = Instant::now();
        let h = search(&[&ix], &query, &SearchOptions::default());
        println!(
            "{q:>24}: {:>8} hits in {:.1} ms",
            h.total,
            t.elapsed().as_secs_f64() * 1000.0
        );
    }
    use quickseek_core::index::IndexOptions;
    use quickseek_core::search::{Extras, ResultSet, SortKey};
    use quickseek_core::tree::{NodeRef, Tree, TreeSort};
    let ix = {
        let e = quickseek_core::engine::Engine::from_indexes(vec![ix]);
        for q in ["", "*.png", "folder: cache*"] {
            let t = Instant::now();
            let mut tree = Tree::build(&e, q, Default::default()).unwrap();
            let built = t.elapsed().as_secs_f64() * 1000.0;
            let t = Instant::now();
            let roots = tree
                .children(&e, &NodeRef::default(), TreeSort::Size, true, 0, 100)
                .unwrap();
            let r = &roots.rows[0];
            let l = tree
                .children(
                    &e,
                    &NodeRef {
                        vol: r.vol.clone(),
                        idx: r.idx,
                        epoch: r.epoch,
                        path: String::new(),
                    },
                    TreeSort::Size,
                    true,
                    0,
                    100,
                )
                .unwrap();
            println!(
                "tree {q:>16?}: built in {built:.0} ms ({:.0} MB), root listing ({} children) {:.1} ms",
                tree.memory_bytes() as f64 / 1048576.0,
                l.total,
                t.elapsed().as_secs_f64() * 1000.0
            );
        }
        let mut v = e.volumes();
        drop(e);
        std::sync::Arc::try_unwrap(v.pop().unwrap())
            .ok()
            .unwrap()
            .index
            .into_inner()
    };
    let mut ix = ix;
    for (label, fast) in [("fast sort on ", true), ("fast sort off", false)] {
        ix.set_options(IndexOptions {
            created: true,
            fast_sort: [true, fast, fast, fast],
        });
        if fast {
            ix.compute_ranks();
        }
        for key in [SortKey::Size, SortKey::Modified, SortKey::Path] {
            for q in ["", "txt"] {
                let query = Query::parse(q).unwrap();
                // Best of a few runs (path sort without ranks is slow: once).
                let runs = if fast || key != SortKey::Path { 5 } else { 1 };
                let (mut first, mut deep, mut total) = (f64::MAX, f64::MAX, 0);
                for _ in 0..runs {
                    let t = Instant::now();
                    let mut rs = ResultSet::new(
                        &[&ix],
                        &query,
                        &SearchOptions {
                            sort: key,
                            descending: true,
                            limit: 512,
                        },
                        Extras::default(),
                    );
                    first = first.min(t.elapsed().as_secs_f64() * 1000.0);
                    let t = Instant::now();
                    let _ = rs.page(&[&ix], rs.total - 10, 10);
                    deep = deep.min(t.elapsed().as_secs_f64() * 1000.0);
                    total = rs.total;
                }
                println!(
                    "{label} {key:?} {q:>4?}: first page {first:7.1} ms, scroll to end {deep:8.1} ms ({total} hits)"
                );
            }
        }
    }
}

/// Cost of turning off a fast sort: sort times with and without ranks.
#[allow(dead_code)]
fn _sort_costs() {}
