//! Loads a saved index (any supported format), saves it in the current
//! format, loads that back and checks every entry is the same; prints the
//! file sizes and load times. For checking a format change on real data:
//! `cargo run --release -p quickseek-core --example index_roundtrip -- C.db`

use std::time::Instant;

use quickseek_core::persist;

fn main() {
    let src = std::env::args().nth(1).expect("an index file");
    let src = std::path::Path::new(&src);
    let t = Instant::now();
    let mut a = persist::load(src).expect("load");
    let load_a = t.elapsed();
    let out = std::env::temp_dir().join("quickseek-roundtrip.db");
    let t = Instant::now();
    persist::save(&mut a, &out).expect("save");
    let save = t.elapsed();
    let t = Instant::now();
    let b = persist::load(&out).expect("reload");
    let load_b = t.elapsed();
    assert_eq!(a.entries.len(), b.entries.len());
    let mut diffs = 0;
    for i in 0..a.entries.len() as u32 {
        let (x, y) = (&a.entries[i as usize], &b.entries[i as usize]);
        if a.is_live(i) != b.is_live(i)
            || ({ x.size }, { x.mtime }, { x.parent }) != ({ y.size }, { y.mtime }, { y.parent })
            || a.name(i) != b.name(i)
        {
            diffs += 1;
        }
    }
    let size = |p: &std::path::Path| std::fs::metadata(p).map_or(0, |m| m.len()) as f64 / 1048576.0;
    println!(
        "{}: {} entries; {:.1} MB, loaded in {:.2} s -> saved as {:.1} MB in {:.2} s, loaded back in {:.2} s; {diffs} entries differ",
        src.display(),
        a.entries.len(),
        size(src),
        load_a.as_secs_f64(),
        size(&out),
        save.as_secs_f64(),
        load_b.as_secs_f64(),
    );
    std::fs::remove_file(&out).ok();
    assert_eq!(diffs, 0);
}
