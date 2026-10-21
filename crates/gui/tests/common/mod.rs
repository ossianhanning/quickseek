//! Synthetic index and mock service shared by the GUI tests and the
//! `mock_service` example.
#![allow(dead_code)]

use std::sync::{Arc, OnceLock};

use rapidfind_core::engine::Engine;
use rapidfind_core::index::Index;
use rapidfind_core::ipc;
use rapidfind_core::ntfs::{Meta, Scan, ScanName};

/// FILETIME for 2026-10-01 12:00 UTC.
pub const BASE_TIME: i64 = 134_037_072_000_000_000;
const DAY: i64 = 864_000_000_000;

/// Files every test can rely on: (folder, name, size, days before BASE_TIME).
pub const KNOWN: &[(&str, &str, u64, i64)] = &[
    ("Users\\me\\Documents", "report-2026.pdf", 1_234_567, 3),
    ("Users\\me\\Documents", "notes.txt", 2_048, 1),
    ("Users\\me\\Documents", "Budget.xlsx", 88_000, 30),
    ("Users\\me\\Pictures", "holiday.jpg", 3_500_000, 200),
    ("Users\\me\\Pictures", "Ångström.png", 64_000, 10),
    ("Users\\me\\Music", "song.mp3", 7_000_000, 400),
    ("Users\\me\\Videos", "movie.mkv", 4_000_000_000, 50),
    ("Users\\me\\Downloads", "setup.exe", 50_000_000, 2),
    ("Users\\me\\Downloads", "archive.zip", 900_000_000, 5),
    ("Users\\me\\Projects\\rapidfind", "README.md", 4_096, 0),
    ("Users\\me\\Projects\\rapidfind\\src", "main.rs", 12_000, 0),
];

struct Builder {
    s: Scan,
    meta: Vec<Meta>,
    rec: u32,
    dirs: Vec<(String, u32)>,
}

impl Builder {
    fn push(&mut self, parent: u32, name: &str, m: Meta) -> u32 {
        let r = self.rec;
        self.rec += 1;
        if self.meta.len() <= r as usize {
            self.meta.resize(r as usize + 1, Meta::default());
        }
        self.meta[r as usize] = m;
        self.s.names.push(ScanName {
            record: r,
            parent,
            off: self.s.strings.len() as u32,
            len: name.len() as u32,
        });
        self.s.strings.extend_from_slice(name.as_bytes());
        r
    }

    fn dir(&mut self, path: &str) -> u32 {
        let mut cur = String::new();
        let mut parent = 5;
        for comp in path.split('\\') {
            cur = if cur.is_empty() {
                comp.to_string()
            } else {
                format!("{cur}\\{comp}")
            };
            if let Some(&(_, r)) = self.dirs.iter().find(|(p, _)| *p == cur) {
                parent = r;
                continue;
            }
            let r = self.push(
                parent,
                comp,
                Meta {
                    in_use: true,
                    is_dir: true,
                    attrs: 0x10,
                    modified: BASE_TIME,
                    created: BASE_TIME,
                    ..Default::default()
                },
            );
            self.dirs.push((cur.clone(), r));
            parent = r;
        }
        parent
    }
}

/// The known files plus `filler` generated ones spread over the same folders.
pub fn sample_index(letter: char, filler: usize) -> Index {
    let mut b = Builder {
        s: Scan {
            has_sizes: true,
            ..Default::default()
        },
        meta: vec![Meta::default(); 6],
        rec: 24,
        dirs: Vec::new(),
    };
    b.meta[5] = Meta {
        in_use: true,
        is_dir: true,
        attrs: 0x10,
        ..Default::default()
    };
    b.s.names.push(ScanName {
        record: 5,
        parent: 5,
        off: 0,
        len: 1,
    });
    b.s.strings.push(b'.');
    for &(folder, name, size, age) in KNOWN {
        let p = b.dir(folder);
        let t = BASE_TIME - age * DAY;
        b.push(
            p,
            name,
            Meta {
                in_use: true,
                attrs: 0x20,
                size,
                modified: t,
                created: t - DAY,
                ..Default::default()
            },
        );
    }
    let exts = ["txt", "log", "rs", "png", "dll", "json"];
    for i in 0..filler {
        let p = b.dir(KNOWN[i % KNOWN.len()].0);
        let name = format!("file{i:06}.{}", exts[i % exts.len()]);
        let t = BASE_TIME - (i as i64 % 900) * DAY;
        b.push(
            p,
            &name,
            Meta {
                in_use: true,
                attrs: 0x20,
                size: (i as u64 * 7919) % 5_000_000,
                modified: t,
                created: t,
                ..Default::default()
            },
        );
    }
    b.s.meta = b.meta;
    let mut ix = Index::from_scan(letter, &b.s);
    ix.volume_serial = 0x1234;
    ix
}

/// Starts a mock service on `pipe` serving `engine`. Returns once the pipe
/// accepts connections.
pub fn serve(pipe: &str, engine: Arc<Engine>) {
    let slot = Arc::new(OnceLock::new());
    let _ = slot.set(engine);
    let p = pipe.to_string();
    std::thread::spawn(move || ipc::serve_on(&p, ipc::engine_handler(slot)));
    for _ in 0..200 {
        if ipc::Client::connect_to(pipe).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("mock service didn't start");
}

/// A month of made-up history for `engine` (which must have history on):
/// Videos grew steadily (+3.5 GB over 30 days), Downloads was cleaned up
/// 3 days ago (it held 2 GB more before), and a save point 10 days ago.
pub fn seed_history(engine: &Engine) {
    use rapidfind_core::history::Snapshot;
    use rapidfind_core::tree::Tree;
    let mut t = Tree::build(engine, "", Default::default()).unwrap();
    let now_snap = t.snapshot(engine, "", 0, 1_000_000).unwrap();
    let h = engine.history().expect("history enabled");
    // Adds `delta` to the folder named `name` and all its ancestors.
    fn adjust(s: &mut Snapshot, name: &str, delta: i64) {
        for v in &mut s.volumes {
            let Some(mut i) = v.folders.iter().position(|f| f.name == name) else {
                continue;
            };
            loop {
                let f = &mut v.folders[i];
                f.size = (f.size as i64 + delta).max(0) as u64;
                if f.parent == u32::MAX {
                    break;
                }
                i = f.parent as usize;
            }
            v.free = (v.free as i64 - delta).max(0) as u64;
        }
    }
    for days in [30i64, 21, 14, 10, 7, 5, 3, 2, 1] {
        let mut s = now_snap.clone();
        s.time = now_snap.time - days * 86_400;
        s.name = if days == 10 {
            "before cleanup".into()
        } else {
            String::new()
        };
        adjust(&mut s, "Videos", -(3_500_000_000 * days / 30));
        if days > 3 {
            adjust(&mut s, "Downloads", 2_000_000_000);
        }
        h.save(&s).unwrap();
    }
}
