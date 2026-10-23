//! The demo PC used for screenshots builds, looks like a real disk and its
//! history agrees with what the tree computes.

mod common;

use quickseek_core::engine::Engine;
use quickseek_core::tree::Tree;

const GB: u64 = 1 << 30;

/// (path, size, online-only, files) of every folder, sorted.
fn rows(v: &quickseek_core::history::SnapVolume) -> Vec<(String, u64, u64, u64)> {
    let mut full: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for f in &v.folders {
        let p = match f.parent {
            u32::MAX => f.name.clone(),
            i => format!("{}\\{}", full[i as usize], f.name),
        };
        out.push((p.clone(), f.size, f.cloud, f.files));
        full.push(p);
    }
    out.sort();
    out
}

#[test]
fn demo_pc_is_consistent() {
    let now = 1_792_000_000; // 2026-10-15
    let pc = common::demo::build(now);
    for v in &pc.vols {
        let free = v.free_now() as f64 / v.capacity as f64;
        assert!(
            (0.04..0.25).contains(&free),
            "{}: free {:.0}%",
            v.letter,
            free * 100.0
        );
    }
    let engine = Engine::from_indexes(pc.indexes());
    // The snapshot the demo makes for now matches the tree's own.
    let mut t = Tree::build(&engine, "", Default::default()).unwrap();
    let real = t
        .snapshot(&engine, "", quickseek_core::history::MIN_FOLDER, 1_000_000)
        .unwrap();
    let ours = pc.snapshot(now, "");
    for (a, b) in real.volumes.iter().zip(&ours.volumes) {
        assert_eq!(rows(a), rows(b), "{}", a.key);
        let mut fa: Vec<_> = a.files.iter().map(|f| (f.name.clone(), f.size)).collect();
        let mut fb: Vec<_> = b.files.iter().map(|f| (f.name.clone(), f.size)).collect();
        fa.sort();
        fb.sort();
        assert_eq!(fa, fb, "{}", a.key);
        println!(
            "{}: {} folders, {} big files",
            a.key,
            a.folders.len(),
            a.files.len()
        );
    }
    for v in &pc.vols {
        println!(
            "{}: used {:.1} GiB, free {:.1} GiB of {:.1}",
            v.letter,
            v.used_now() as f64 / GB as f64,
            v.free_now() as f64 / GB as f64,
            v.capacity as f64 / GB as f64
        );
    }
    // History: a month of dailies and some weeklies, with the trends.
    let times = pc.snapshot_times();
    assert!((30..=60).contains(&times.len()), "{}", times.len());
    let c_then = pc.snapshot(now - 30 * 86_400, "").volumes[0].clone();
    let m = quickseek_core::history::Snapshot {
        volumes: vec![c_then],
        ..pc.snapshot(now, "")
    }
    .paths();
    let target = m.get(r"C:\Users\alex\dev\tilemap-editor\target").unwrap();
    assert!(target < 4 * GB, "target grew this month: {target}");
    let game = pc.snapshot(now - 25 * 86_400, "").paths();
    assert!(
        game.get(r"D:\SteamLibrary\steamapps\common\Starfall Odyssey")
            .is_none(),
        "installed later"
    );
    assert!(
        game.get(r"D:\SteamLibrary\steamapps\common\Iron Harbor")
            .unwrap()
            > 70 * GB,
        "uninstalled since"
    );
    // Made up: nothing from the machine that built it, like its user name.
    let me = std::env::var("USERNAME").unwrap_or_default().to_lowercase();
    for ix in pc.indexes() {
        for i in 0..ix.entries.len() as u32 {
            let n = ix.name(i).to_lowercase();
            assert!(me.len() < 3 || me == "alex" || !n.contains(&me), "{n}");
        }
    }
}
