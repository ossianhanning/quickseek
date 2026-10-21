//! End-to-end checks against a real volume (run elevated).

use std::time::{Duration, Instant};

use anyhow::{bail, ensure};
use rapidfind_core::engine::{Engine, scan_volume};
use rapidfind_core::ntfs::{usn, win};
use rapidfind_core::query::Query;
use rapidfind_core::search::SearchOptions;

fn find(eng: &Engine, q: &str) -> Vec<String> {
    let q = Query::parse(q).unwrap();
    let h = eng.search(
        &q,
        &SearchOptions {
            limit: 0,
            ..Default::default()
        },
    );
    eng.with_indexes(|ixs| {
        h.items
            .iter()
            .map(|h| ixs[h.vol as usize].path(h.idx))
            .collect()
    })
}

fn wait_for(
    eng: &Engine,
    q: &str,
    want: impl Fn(&[String]) -> bool,
) -> anyhow::Result<Vec<String>> {
    let t = Instant::now();
    loop {
        let r = find(eng, q);
        if want(&r) {
            println!(
                "    ok after {} ms: {q:?} -> {r:?}",
                t.elapsed().as_millis()
            );
            return Ok(r);
        }
        if t.elapsed() > Duration::from_secs(5) {
            bail!("timed out waiting on {q:?}; last result {r:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub fn run(letter: char) -> anyhow::Result<()> {
    println!("[1] raw MFT scan of {letter}:");
    let (ix, report, _, _) = scan_volume(letter, &|_, _| {})?;
    println!("    {report}");
    println!("    {} live entries", ix.len_live());

    println!("[2] compare with FSCTL_ENUM_USN_DATA enumeration");
    let t = Instant::now();
    let h = win::open_volume(letter, 0)?;
    let en = usn::scan_enum(&h)?;
    let enum_ix = rapidfind_core::index::Index::from_scan(letter, &en);
    println!(
        "    enum: {} live entries in {} ms",
        enum_ix.len_live(),
        t.elapsed().as_millis()
    );
    let diff = ix.len_live() as i64 - enum_ix.len_live() as i64;
    println!("    difference (raw - enum, mostly extra hard links): {diff}");

    println!("[3] spot-check 3000 entries against the filesystem");
    let n = ix.entries.len() as u32;
    let (mut checked, mut missing, mut size_mismatch, mut dir_mismatch) = (0, 0, 0, 0);
    let mut examples = Vec::new();
    let step = (n / 3000).max(1);
    let mut i = 0;
    while i < n {
        if ix.is_live(i) {
            let p = ix.path(i);
            let e = &ix.entries[i as usize];
            checked += 1;
            match std::fs::symlink_metadata(&p) {
                Ok(m) if m.file_type().is_symlink() => {}
                Ok(m) => {
                    if m.is_dir() != e.is_dir() {
                        dir_mismatch += 1;
                    } else if m.is_file() && m.len() != e.size {
                        size_mismatch += 1;
                        if examples.len() < 5 {
                            examples.push(format!("size {} vs {}: {p}", { e.size }, m.len()));
                        }
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    missing += 1;
                    if examples.len() < 5 {
                        examples.push(format!("missing: {p}"));
                    }
                }
                Err(_) => {} // access denied etc.
            }
        }
        i += step;
    }
    println!(
        "    checked {checked}: {missing} missing, {size_mismatch} size mismatches, {dir_mismatch} dir/file mismatches"
    );
    for e in examples {
        println!("      {e}");
    }
    drop(ix);
    drop(enum_ix);

    println!("[4] live updates through the USN journal");
    let eng = Engine::open(&[letter], false, |l, r| {
        if let Ok(r) = r {
            println!("    {l}: {r}")
        }
    });
    ensure!(eng.volume_count() > 0, "engine failed to open {letter}:");
    let tag = format!("rftest{}", std::process::id());
    let base = std::path::PathBuf::from(format!("{letter}:\\"))
        .join("Users")
        .join("Public");
    let base = if base.exists() {
        base
    } else {
        std::env::temp_dir()
    };
    let dir = base.join(format!("{tag}_dir"));
    std::fs::create_dir(&dir)?;
    std::fs::write(dir.join(format!("{tag}_a.txt")), vec![b'x'; 12345])?;
    wait_for(&eng, &format!("{tag}_a.txt"), |r| r.len() == 1)?;
    wait_for(&eng, &format!("{tag}_a.txt size:12345"), |r| r.len() == 1)?;
    std::fs::rename(
        dir.join(format!("{tag}_a.txt")),
        dir.join(format!("{tag}_b.txt")),
    )?;
    wait_for(&eng, &tag, |r| {
        r.iter().any(|p| p.ends_with("_b.txt")) && !r.iter().any(|p| p.ends_with("_a.txt"))
    })?;
    let dir2 = base.join(format!("{tag}_moved"));
    std::fs::rename(&dir, &dir2)?;
    wait_for(&eng, &format!("{tag}_b.txt"), |r| {
        r.len() == 1 && r[0].contains("_moved\\")
    })?;
    std::fs::remove_dir_all(&dir2)?;
    wait_for(&eng, &tag, |r| r.is_empty())?;

    println!("all checks done");
    Ok(())
}
