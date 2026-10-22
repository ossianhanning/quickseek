//! A made-up but believable Windows PC for screenshots and demos: a user
//! `alex` with photos, game captures, downloads, Rust and web projects, WSL
//! and Docker disks, OneDrive files kept online only, Steam games and video
//! projects on D:, and an external backup drive E:, plus 6 weeks of
//! folder-size history with real trends (a game installed, a growing Rust
//! `target`, a cleanup that freed space, a video project that grew).
//!
//! Nothing here comes from a real machine: names are generated or written
//! by hand, sizes are budgets spread over the files. Served by
//! `cargo run --release -p rapidfind-gui --example demo_service`.

use std::collections::HashMap;

use rapidfind_core::history::{MIN_FOLDER, SnapFile, SnapFolder, SnapVolume, Snapshot};
use rapidfind_core::index::Index;
use rapidfind_core::ntfs::{Meta, Scan, ScanName};

const KB: u64 = 1 << 10;
const MB: u64 = 1 << 20;
const GB: u64 = 1 << 30;
/// FILETIME ticks per day.
const DAY_FT: i64 = 864_000_000_000;
const FT_UNIX: i64 = 11_644_473_600;
const OFFLINE: u32 = 0x1000;
const RECALL: u32 = 0x0040_0000;
const HIDDEN_SYSTEM: u32 = 0x2 | 0x4;

/// Days ago the cleanup happened (Downloads, Temp and the Recycle Bin).
pub const CLEANUP_DAYS: f64 = 6.2;
/// Days ago the game on D: was installed.
pub const GAME_DAYS: f64 = 12.4;

fn gib(x: f64) -> u64 {
    (x * GB as f64) as u64
}

/// Deterministic xorshift, so every run builds the same PC.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.f()
    }
    fn int(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.int(xs.len())]
    }
    fn hex(&mut self, n: usize) -> String {
        (0..n)
            .map(|_| char::from_digit((self.next() % 16) as u32, 16).unwrap())
            .collect()
    }
    /// A spread of weights (roughly log-normal), for sizes that look natural.
    fn weight(&mut self, spread: f64) -> f64 {
        let z = (self.f() + self.f() + self.f() - 1.5) * 2.0;
        (z * spread).exp()
    }
}

/// A file as the history sees it.
struct Rec {
    dir: usize,
    name: String,
    size: u64,
    /// Unix seconds it appeared.
    created: i64,
    /// Grew linearly from this size since this time (unix) to `size` now.
    grew: Option<(i64, u64)>,
    online_only: bool,
}

/// Something that existed and was deleted: it only shows in the history.
struct Ghost {
    dir: usize,
    size: u64,
    created: i64,
    deleted: i64,
}

struct Dir {
    name: String,
    parent: usize,
    rec: u32,
    kids: Vec<usize>,
}

/// One drive being built.
pub struct Vol {
    pub letter: char,
    pub capacity: u64,
    /// Space taken by things outside the index (system metadata, shadow copies).
    overhead: u64,
    scan: Scan,
    meta: Vec<Meta>,
    next_rec: u32,
    dirs: Vec<Dir>,
    by_path: HashMap<String, usize>,
    files: Vec<Rec>,
    ghosts: Vec<Ghost>,
    now: i64,
    rng: Rng,
}

impl Vol {
    fn new(letter: char, capacity: u64, overhead: u64, now: i64, seed: u64) -> Vol {
        let mut v = Vol {
            letter,
            capacity,
            overhead,
            scan: Scan {
                has_sizes: true,
                ..Default::default()
            },
            meta: vec![Meta::default(); 6],
            next_rec: 24,
            dirs: vec![Dir {
                name: format!("{letter}:"),
                parent: usize::MAX,
                rec: 5,
                kids: Vec::new(),
            }],
            by_path: HashMap::new(),
            files: Vec::new(),
            ghosts: Vec::new(),
            now,
            rng: Rng(seed),
        };
        v.meta[5] = Meta {
            in_use: true,
            is_dir: true,
            attrs: 0x10,
            ..Default::default()
        };
        v.scan.names.push(ScanName {
            record: 5,
            parent: 5,
            off: 0,
            len: 1,
        });
        v.scan.strings.push(b'.');
        v
    }

    fn ft(&self, days_ago: f64) -> i64 {
        (self.now + FT_UNIX) * 10_000_000 - (days_ago * DAY_FT as f64) as i64
    }

    fn unix(&self, days_ago: f64) -> i64 {
        self.now - (days_ago * 86_400.0) as i64
    }

    fn push(&mut self, parent: u32, name: &str, m: Meta) -> u32 {
        let r = self.next_rec;
        self.next_rec += 1;
        if self.meta.len() <= r as usize {
            self.meta.resize(r as usize + 1, Meta::default());
        }
        self.meta[r as usize] = m;
        self.scan.names.push(ScanName {
            record: r,
            parent,
            off: self.scan.strings.len() as u32,
            len: name.len() as u32,
        });
        self.scan.strings.extend_from_slice(name.as_bytes());
        r
    }

    /// The folder at `path` (below the root, `\`-separated), made if needed.
    fn dir(&mut self, path: &str) -> usize {
        if let Some(&d) = self.by_path.get(path) {
            return d;
        }
        let (parent, name) = match path.rsplit_once('\\') {
            Some((p, n)) => (self.dir(p), n),
            None => (0, path),
        };
        let hidden = name.starts_with('$') || name == "System Volume Information";
        let touched = self.rng.range(0.0, 30.0);
        let m = Meta {
            in_use: true,
            is_dir: true,
            attrs: 0x10 | if hidden { HIDDEN_SYSTEM } else { 0 },
            modified: self.ft(touched),
            created: self.ft(400.0),
            ..Default::default()
        };
        let rec = self.push(self.dirs[parent].rec, name, m);
        self.dirs.push(Dir {
            name: name.to_string(),
            parent,
            rec,
            kids: Vec::new(),
        });
        let d = self.dirs.len() - 1;
        self.dirs[parent].kids.push(d);
        self.by_path.insert(path.to_string(), d);
        d
    }

    /// A file that appeared `age` days ago and hasn't changed since.
    fn file(&mut self, dir: usize, name: &str, size: u64, age: f64) {
        self.file_with(dir, name, size, age, None, 0x20);
    }

    fn file_with(
        &mut self,
        dir: usize,
        name: &str,
        size: u64,
        age: f64,
        grew: Option<(f64, u64)>,
        attrs: u32,
    ) {
        let modified = match grew {
            Some(_) => {
                let t = self.rng.range(0.0, 0.3);
                self.ft(t)
            }
            None => self.ft(age),
        };
        let m = Meta {
            in_use: true,
            attrs,
            size,
            modified,
            created: self.ft(age),
            ..Default::default()
        };
        self.push(self.dirs[dir].rec, name, m);
        self.files.push(Rec {
            dir,
            name: name.to_string(),
            size,
            created: self.unix(age),
            grew: grew.map(|(d, s)| (self.unix(d), s)),
            online_only: attrs & OFFLINE != 0,
        });
    }

    /// Something in `dir` from `created` to `deleted` days ago.
    fn ghost(&mut self, dir: usize, size: u64, created: f64, deleted: f64) {
        self.ghosts.push(Ghost {
            dir,
            size,
            created: self.unix(created),
            deleted: self.unix(deleted),
        });
    }

    /// `n` files in `path` named by `name`, `total` bytes between them,
    /// appearing between `ages.0` and `ages.1` days ago.
    fn spread(
        &mut self,
        path: &str,
        n: usize,
        total: u64,
        ages: (f64, f64),
        mut name: impl FnMut(&mut Rng, usize, f64) -> String,
    ) {
        self.spread_with(path, n, total, ages, 0x20, &mut name)
    }

    fn spread_with(
        &mut self,
        path: &str,
        n: usize,
        total: u64,
        ages: (f64, f64),
        attrs: u32,
        name: &mut dyn FnMut(&mut Rng, usize, f64) -> String,
    ) {
        let d = self.dir(path);
        let w: Vec<f64> = (0..n).map(|_| self.rng.weight(0.9)).collect();
        let sum: f64 = w.iter().sum();
        let mut seen = std::collections::HashSet::new();
        for (i, w) in w.iter().enumerate() {
            let age = self.rng.range(ages.0, ages.1);
            let mut nm = name(&mut self.rng, i, age);
            if !seen.insert(nm.to_lowercase()) {
                let (stem, ext) = nm.rsplit_once('.').unwrap_or((&nm, ""));
                nm = format!("{stem}_{i}.{ext}");
                seen.insert(nm.to_lowercase());
            }
            let size = ((total as f64) * w / sum) as u64;
            self.file_with(d, &nm, size.max(1), age, None, attrs);
        }
    }

    /// A tree of small files, `dirs` folders deep under `path`, for things
    /// like node_modules: many entries, modest size.
    fn tree(&mut self, path: &str, names: &[&str], files_per: usize, total: u64, ages: (f64, f64)) {
        let leaves = [
            "index.js",
            "package.json",
            "README.md",
            "LICENSE",
            "index.d.ts",
            "CHANGELOG.md",
            "dist\\index.js",
            "dist\\index.mjs",
            "dist\\index.cjs",
            "lib\\utils.js",
            "lib\\core.js",
            "src\\index.ts",
        ];
        let per = total / (names.len() * files_per).max(1) as u64;
        for pkg in names {
            for leaf in leaves.iter().take(files_per) {
                let (sub, file) = match leaf.rsplit_once('\\') {
                    Some((s, f)) => (format!("{path}\\{pkg}\\{s}"), f),
                    None => (format!("{path}\\{pkg}"), *leaf),
                };
                let d = self.dir(&sub);
                let size = (per as f64 * self.rng.weight(1.0)) as u64;
                let age = self.rng.range(ages.0, ages.1);
                self.file(d, file, size.max(64), age);
            }
        }
    }

    pub fn index(&self) -> Index {
        let mut s = Scan {
            has_sizes: true,
            meta: self.meta.clone(),
            names: self.scan.names.clone(),
            strings: self.scan.strings.clone(),
        };
        s.meta.resize(self.next_rec as usize + 1, Meta::default());
        let mut ix = Index::from_scan(self.letter, &s);
        ix.volume_serial = 0x5EED_0000 + self.letter as u32;
        ix
    }

    /// Bytes the index holds now (online-only files excluded).
    pub fn used_now(&self) -> u64 {
        self.files
            .iter()
            .filter(|f| !f.online_only)
            .map(|f| f.size)
            .sum()
    }

    pub fn free_now(&self) -> u64 {
        self.capacity
            .saturating_sub(self.used_now() + self.overhead)
    }

    /// The volume as a snapshot at unix time `t` would have recorded it.
    pub fn snapshot_at(&self, t: i64) -> SnapVolume {
        let n = self.dirs.len();
        let (mut local, mut cloud, mut count) = (vec![0u64; n], vec![0u64; n], vec![0u64; n]);
        let mut add = |mut d: usize, l: u64, c: u64| loop {
            local[d] += l;
            cloud[d] += c;
            count[d] += 1;
            if d == 0 {
                break;
            }
            d = self.dirs[d].parent;
        };
        let mut big: Vec<(usize, &str, u64)> = Vec::new();
        for f in self.files.iter().filter(|f| f.created <= t) {
            let size = match f.grew {
                Some((from, s0)) if t <= from => s0,
                Some((from, s0)) => {
                    let k = (t - from) as f64 / (self.now - from).max(1) as f64;
                    s0 + ((f.size as f64 - s0 as f64) * k.min(1.0)) as u64
                }
                None => f.size,
            };
            if f.online_only {
                add(f.dir, 0, size);
            } else {
                add(f.dir, size, 0);
                if size >= MIN_FOLDER {
                    big.push((f.dir, &f.name, size));
                }
            }
        }
        for g in self
            .ghosts
            .iter()
            .filter(|g| g.created <= t && t < g.deleted)
        {
            add(g.dir, g.size, 0);
        }
        // Folders of 10 MB or more, parents first (what Tree::snapshot keeps).
        let mut out: Vec<SnapFolder> = Vec::new();
        let mut at = vec![u32::MAX; n];
        let mut stack = vec![(0usize, u32::MAX)];
        while let Some((d, parent)) = stack.pop() {
            if d != 0 && local[d] + cloud[d] < MIN_FOLDER {
                continue;
            }
            at[d] = out.len() as u32;
            out.push(SnapFolder {
                parent,
                name: self.dirs[d].name.clone(),
                size: local[d],
                cloud: cloud[d],
                files: count[d],
            });
            for &k in self.dirs[d].kids.iter().rev() {
                stack.push((k, at[d]));
            }
        }
        let files = big
            .into_iter()
            .filter(|b| at[b.0] != u32::MAX)
            .map(|(d, name, size)| SnapFile {
                parent: at[d],
                name: name.to_string(),
                size,
            })
            .collect();
        let used_then = local[0];
        SnapVolume {
            key: self.letter.to_string(),
            capacity: self.capacity,
            free: (self.free_now() as i64 + self.used_now() as i64 - used_then as i64).max(0)
                as u64,
            folders: out,
            files,
        }
    }
}

/// The demo PC: its drives, built.
pub struct DemoPc {
    pub vols: Vec<Vol>,
    pub now: i64,
}

impl DemoPc {
    /// (root, capacity, free) for [`rapidfind_core::tree::override_disk_space`].
    pub fn drives(&self) -> Vec<(String, u64, u64)> {
        self.vols
            .iter()
            .map(|v| (format!("{}:", v.letter), v.capacity, v.free_now()))
            .collect()
    }

    pub fn indexes(&self) -> Vec<Index> {
        self.vols.iter().map(Vol::index).collect()
    }

    /// Snapshot times: daily for a month, then weekly back to 8 weeks.
    pub fn snapshot_times(&self) -> Vec<(i64, String)> {
        // Daily snapshots run a while after midnight local; ~03:10 is plausible.
        let midnight = rapidfind_core::history::local_midnight(self.now);
        let mut t: Vec<(i64, String)> = (1..=31)
            .chain([35, 42, 49, 56])
            .map(|d| (midnight - d * 86_400 + 3 * 3600 + 600, String::new()))
            .collect();
        let cleanup = self.now - (CLEANUP_DAYS * 86_400.0) as i64;
        t.push((cleanup - 1800, "before cleanup".into()));
        t.sort();
        t
    }

    pub fn snapshot(&self, t: i64, name: &str) -> Snapshot {
        Snapshot {
            time: t,
            name: name.to_string(),
            volumes: self.vols.iter().map(|v| v.snapshot_at(t)).collect(),
            has_files: true,
        }
    }

    pub fn seed_history(&self, h: &rapidfind_core::history::History) {
        for (t, name) in self.snapshot_times() {
            h.save(&self.snapshot(t, &name))
                .expect("saving a demo snapshot");
        }
    }
}

// ---- the data -------------------------------------------------------------------

const USER: &str = "Users\\alex";

pub fn build(now: i64) -> DemoPc {
    let mut c = Vol::new('C', 1_000_202_039_296, gib(7.5), now, 0xC0FF_EE11_D15C_0001);
    windows(&mut c);
    programs(&mut c);
    profile(&mut c);
    dev(&mut c);
    let mut d = Vol::new('D', 2_000_396_746_752, gib(2.0), now, 0xD15C_D00D_0002_0002);
    games_and_video(&mut d);
    let mut e = Vol::new('E', 1_000_169_533_440, gib(1.0), now, 0xBAC0_0000_0003_0003);
    backup_drive(&mut e);
    DemoPc {
        vols: vec![c, d, e],
        now,
    }
}

const DLL_WORDS: &[&str] = &[
    "kernel",
    "user",
    "gdi",
    "shell",
    "ole",
    "com",
    "crypt",
    "net",
    "win",
    "d3d",
    "dx",
    "media",
    "audio",
    "print",
    "search",
    "storage",
    "power",
    "input",
    "speech",
    "ink",
    "font",
    "theme",
    "sync",
    "update",
    "wlan",
    "bluetooth",
    "usb",
    "hid",
    "camera",
    "edge",
    "xaml",
    "sec",
    "auth",
    "policy",
    "task",
    "event",
    "perf",
    "dwm",
    "dcomp",
    "msvc",
];

fn dll(r: &mut Rng, i: usize, _: f64) -> String {
    let ext = ["dll", "dll", "dll", "exe", "mui", "sys", "dll", "winmd"][i % 8];
    match i % 5 {
        0 => format!(
            "api-ms-win-{}-{}-l1-{}-{}.dll",
            r.pick(&["core", "crt", "shcore", "eventing", "security"]),
            r.pick(DLL_WORDS),
            r.int(3) + 1,
            r.int(3)
        ),
        1 => format!(
            "{}{}.{ext}",
            r.pick(DLL_WORDS),
            r.pick(&["32", "64", "svc", "core", "api", "ext", "host"])
        ),
        _ => format!(
            "{}{}{}.{ext}",
            r.pick(DLL_WORDS),
            r.pick(DLL_WORDS),
            r.pick(&["", "srv", "ui", "prov", "mgr", "dll"])
        ),
    }
}

fn windows(c: &mut Vol) {
    c.spread("Windows\\System32", 5200, gib(8.6), (40.0, 700.0), dll);
    c.spread(
        "Windows\\System32\\drivers",
        520,
        gib(0.45),
        (40.0, 700.0),
        |r, _, _a| {
            format!(
                "{}{}.sys",
                r.pick(DLL_WORDS),
                r.pick(&["", "flt", "bus", "port", "mini"])
            )
        },
    );
    for i in 0..640 {
        let inf = format!(
            "{}{}.inf_amd64_{}",
            c.rng.pick(DLL_WORDS),
            c.rng.pick(&["", "_ext", "dev", "cls"]),
            c.rng.hex(16)
        );
        let p = format!("Windows\\System32\\DriverStore\\FileRepository\\{inf}");
        let total = (6.4 * GB as f64 / 640.0 * c.rng.weight(1.2)) as u64;
        c.spread(&p, 4 + i % 3, total, (30.0, 600.0), move |_, k, _a| {
            [
                format!("{inf}.cat"),
                "driver.sys".into(),
                "setup.inf".into(),
                "firmware.bin".into(),
                "helper.dll".into(),
                "data.cab".into(),
            ][k]
                .clone()
        });
    }
    for _ in 0..2100 {
        let comp = format!(
            "amd64_microsoft-windows-{}-{}_31bf3856ad364e35_10.0.26100.{}_none_{}",
            c.rng.pick(DLL_WORDS),
            c.rng.pick(DLL_WORDS),
            4000 + c.rng.int(2600),
            c.rng.hex(16)
        );
        let total = (9.8 * GB as f64 / 2100.0 * c.rng.weight(1.0)) as u64;
        c.spread(
            &format!("Windows\\WinSxS\\{comp}"),
            3,
            total,
            (20.0, 700.0),
            dll,
        );
    }
    c.spread("Windows\\SysWOW64", 2600, gib(2.4), (40.0, 700.0), dll);
    c.spread(
        "Windows\\Installer",
        230,
        gib(3.5),
        (10.0, 600.0),
        |r, i, _a| format!("{}.{}", r.hex(7), if i % 4 == 0 { "msp" } else { "msi" }),
    );
    c.spread(
        "Windows\\SoftwareDistribution\\Download",
        46,
        gib(1.9),
        (1.0, 20.0),
        |r, _, _a| {
            format!(
                "windows11.0-kb50{}-x64_{}.cab",
                40000 + r.int(9999),
                r.hex(40)
            )
        },
    );
    c.spread(
        "Windows\\Fonts",
        420,
        gib(0.7),
        (300.0, 700.0),
        |r, _, _a| {
            format!(
                "{}{}.ttf",
                r.pick(&[
                    "seg", "arial", "cour", "times", "cons", "cal", "verd", "tahoma", "georg",
                    "bahn"
                ]),
                r.pick(&["oe", "ui", "bd", "i", "z", "sb", "l", ""])
            )
        },
    );
    c.spread("Windows\\Temp", 64, gib(0.35), (0.2, 25.0), |r, _, _a| {
        format!("{}.tmp", r.hex(8))
    });
    c.spread(
        "Windows\\Logs\\CBS",
        5,
        gib(0.5),
        (0.5, 20.0),
        |_, i, _a| {
            [
                "CBS.log",
                "CbsPersist_20260930.log",
                "CbsPersist_20260912.log",
                "FilterList.log",
                "DismHost.log",
            ][i]
                .into()
        },
    );
    c.spread(
        "Windows\\Microsoft.NET\\Framework64\\v4.0.30319",
        900,
        gib(0.9),
        (200.0, 600.0),
        dll,
    );
    let root = 0;
    c.file_with(
        root,
        "pagefile.sys",
        17_179_869_184,
        200.0,
        None,
        0x20 | HIDDEN_SYSTEM,
    );
    c.file_with(
        root,
        "hiberfil.sys",
        13_629_235_200,
        200.0,
        None,
        0x20 | HIDDEN_SYSTEM,
    );
    c.file_with(
        root,
        "swapfile.sys",
        268_435_456,
        200.0,
        None,
        0x20 | HIDDEN_SYSTEM,
    );
    c.file_with(
        root,
        "DumpStack.log.tmp",
        12 * KB,
        0.5,
        None,
        0x20 | HIDDEN_SYSTEM,
    );
    // The Recycle Bin: emptied in the cleanup, filling up again since.
    let rb = "$Recycle.Bin\\S-1-5-21-3623811015-3361044348-30300820-1001";
    c.spread(rb, 38, gib(4.6), (0.3, 5.5), |r, i, _a| {
        let ext = ["zip", "mp4", "jpg", "iso", "png", "docx", "exe"][i % 7];
        format!("$R{}.{ext}", r.hex(6).to_uppercase())
    });
    let rbd = c.dir(rb);
    c.ghost(rbd, gib(9.3), 40.0, CLEANUP_DAYS);
}

fn programs(c: &mut Vol) {
    let apps: &[(&str, f64, usize)] = &[
        ("Program Files\\Microsoft Office\\root\\Office16", 4.3, 1400),
        ("Program Files\\Adobe\\Adobe Premiere Pro 2026", 10.2, 2600),
        ("Program Files\\Adobe\\Adobe Media Encoder 2026", 4.6, 1300),
        ("Program Files\\Blender Foundation\\Blender 4.5", 1.5, 3800),
        ("Program Files\\Git", 0.45, 2300),
        ("Program Files\\nodejs", 0.12, 1900),
        ("Program Files\\PowerToys", 0.9, 1100),
        ("Program Files\\7-Zip", 0.006, 40),
        ("Program Files\\VideoLAN\\VLC", 0.25, 520),
        ("Program Files\\NVIDIA Corporation", 2.3, 900),
        ("Program Files\\Docker\\Docker", 2.6, 700),
        ("Program Files\\obs-studio", 0.6, 1200),
        ("Program Files\\Mozilla Firefox", 0.32, 160),
        ("Program Files\\rapidfind", 0.024, 6),
        (
            "Program Files\\Microsoft Visual Studio\\2022\\Community",
            5.8,
            9000,
        ),
        ("Program Files\\WindowsApps", 9.1, 7200),
        ("Program Files (x86)\\Steam", 1.3, 1500),
        (
            "Program Files (x86)\\Steam\\steamapps\\common\\Night Courier",
            41.0,
            900,
        ),
        (
            "Program Files (x86)\\Microsoft\\Edge\\Application",
            0.7,
            420,
        ),
        ("Program Files (x86)\\Windows Kits\\10", 4.1, 8800),
        (
            "Program Files (x86)\\Microsoft Visual Studio\\Installer",
            0.35,
            600,
        ),
        ("ProgramData\\Package Cache", 6.2, 800),
        (
            "ProgramData\\Microsoft\\Windows Defender\\Definition Updates",
            1.4,
            30,
        ),
        ("ProgramData\\NVIDIA Corporation\\Downloader", 2.2, 12),
        ("ProgramData\\Docker", 0.3, 200),
    ];
    for &(path, gb, n) in apps {
        let sub = [
            "",
            "\\bin",
            "\\resources",
            "\\lib",
            "\\plugins",
            "\\locales",
        ];
        let per = n / sub.len();
        for (k, s) in sub.iter().enumerate() {
            let p = format!("{path}{s}");
            c.spread(
                &p,
                per.max(1),
                (gib(gb) as f64 / sub.len() as f64 * [2.5, 1.2, 1.0, 0.6, 0.4, 0.3][k]) as u64,
                (20.0, 400.0),
                dll,
            );
        }
    }
    c.spread(
        "ProgramData\\Microsoft\\Windows\\WER\\ReportArchive",
        14,
        gib(1.1),
        (3.0, 80.0),
        |r, _, _a| {
            format!(
                "AppCrash_{}_{}\\Report.wer.dmp",
                r.pick(&["Starfall.exe", "Code.exe", "obs64.exe", "explorer.exe"]),
                r.hex(12)
            )
            .replace('\\', "_")
        },
    );
}

/// `IMG_20260814_183022.jpg` for something `age` days old.
fn photo_name(prefix: &str, ext: &str, age: f64, now: i64) -> String {
    let t = now - (age * 86_400.0) as i64;
    let (y, mo, d) = civil(t.div_euclid(86_400));
    let s = t.rem_euclid(86_400);
    format!(
        "{prefix}_{y:04}{mo:02}{d:02}_{:02}{:02}{:02}.{ext}",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// `2026-10-14 21-03-44` (date and time separators as given) for `age` days ago.
fn stamp(age: f64, now: i64, dsep: &str, tsep: &str) -> String {
    let t = now - (age * 86_400.0) as i64;
    let (y, mo, d) = civil(t.div_euclid(86_400));
    let s = t.rem_euclid(86_400);
    format!(
        "{y:04}{dsep}{mo:02}{dsep}{d:02} {:02}{tsep}{:02}{tsep}{:02}",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

/// Days since 1970 to (year, month, day).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn date(now: i64, days_ago: f64) -> String {
    let (y, m, d) = civil((now - (days_ago * 86_400.0) as i64).div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

fn profile(c: &mut Vol) {
    let now = c.now;
    let u = USER;
    // WSL and Docker disks: big single files; the Ubuntu one grew this month.
    let wsl = c.dir(&format!("{u}\\AppData\\Local\\Packages\\CanonicalGroupLimited.Ubuntu24.04LTS_79rhkp1fndgsc\\LocalState"));
    c.file_with(
        wsl,
        "ext4.vhdx",
        gib(61.8),
        380.0,
        Some((34.0, gib(48.6))),
        0x20,
    );
    let dock = c.dir(&format!("{u}\\AppData\\Local\\Docker\\wsl\\disk"));
    c.file_with(dock, "docker_data.vhdx", gib(38.4), 420.0, None, 0x20);
    let tmp = format!("{u}\\AppData\\Local\\Temp");
    c.spread(
        &tmp,
        820,
        gib(2.6),
        (0.1, CLEANUP_DAYS - 0.2),
        |r, i, _a| match i % 6 {
            0 => format!("{}.tmp", r.hex(8)),
            1 => format!("chrome_{}.crdownload", r.hex(10)),
            2 => format!("msedge_installer_{}.log", r.int(900)),
            3 => format!("vs_setup_bootstrapper_{}.json", r.hex(8)),
            4 => format!("~DF{}.TMP", r.hex(4).to_uppercase()),
            _ => format!("npm-{}-{}.log", 10000 + r.int(80000), r.hex(6)),
        },
    );
    let tmpd = c.dir(&tmp);
    c.ghost(tmpd, gib(5.4), 90.0, CLEANUP_DAYS);
    let local = format!("{u}\\AppData\\Local");
    for (p, gb, n) in [
        ("npm-cache\\_cacache\\content-v2\\sha512", 3.4, 6400usize),
        ("pip\\cache\\http-v2", 1.3, 1800),
        (
            "Google\\Chrome\\User Data\\Default\\Cache\\Cache_Data",
            1.7,
            4100,
        ),
        (
            "Google\\Chrome\\User Data\\Default\\Code Cache\\js",
            0.45,
            2600,
        ),
        (
            "Microsoft\\Edge\\User Data\\Default\\Cache\\Cache_Data",
            0.8,
            1900,
        ),
        ("NVIDIA\\DXCache", 1.1, 380),
        ("Programs\\Microsoft VS Code", 0.62, 2100),
        ("Programs\\Python\\Python313", 0.31, 4300),
        ("Microsoft\\Windows\\INetCache\\IE", 0.4, 900),
        ("Spotify\\Data", 4.1, 3100),
        ("JetBrains\\Toolbox\\apps", 2.9, 5200),
        ("D3DSCache", 0.9, 240),
        ("Microsoft\\Windows\\Explorer", 0.35, 60),
    ] {
        c.spread(
            &format!("{local}\\{p}"),
            n,
            gib(gb),
            (0.5, 120.0),
            |r, _, _a| r.hex(16),
        );
    }
    c.spread(
        &format!("{local}\\CrashDumps"),
        6,
        gib(2.3),
        (2.0, 70.0),
        |r, i, _a| {
            format!(
                "{}.{}.dmp",
                [
                    "Starfall.exe",
                    "Code.exe",
                    "obs64.exe",
                    "Blender.exe",
                    "Starfall.exe",
                    "Adobe Premiere Pro.exe"
                ][i],
                1000 + r.int(30000)
            )
        },
    );
    let roaming = format!("{u}\\AppData\\Roaming");
    for (p, gb, n) in [
        ("Code\\User\\workspaceStorage", 1.2, 900usize),
        ("Adobe\\Common\\Media Cache Files", 7.6, 1400),
        ("Adobe\\Common\\Peak Files", 1.4, 1400),
        ("discord\\Cache\\Cache_Data", 0.7, 1500),
        ("obs-studio\\logs", 0.09, 200),
        ("npm\\node_modules", 0.21, 3100),
        (
            "Blender Foundation\\Blender\\4.5\\scripts\\addons",
            0.38,
            1200,
        ),
    ] {
        c.spread(
            &format!("{roaming}\\{p}"),
            n,
            gib(gb),
            (0.5, 200.0),
            |r, _, _a| r.hex(12),
        );
    }
    c.spread(
        &format!("{u}\\.cargo\\registry\\cache\\index.crates.io-1949cf8c6b5b557f"),
        1400,
        gib(0.6),
        (2.0, 300.0),
        |r, _, _a| {
            format!(
                "{}-{}.{}.{}.crate",
                r.pick(&[
                    "serde", "tokio", "rand", "syn", "quote", "regex", "clap", "anyhow", "winit",
                    "wgpu", "image", "rayon", "bytes", "hyper", "glam", "egui"
                ]),
                r.int(3),
                r.int(40),
                r.int(20)
            )
        },
    );
    c.spread(
        &format!("{u}\\.cargo\\registry\\src\\index.crates.io-1949cf8c6b5b557f"),
        9000,
        gib(2.2),
        (2.0, 300.0),
        |r, _, _a| format!("{}.rs", r.pick(DLL_WORDS)),
    );
    c.spread(
        &format!("{u}\\.rustup\\toolchains\\stable-x86_64-pc-windows-msvc\\lib\\rustlib"),
        2600,
        gib(3.7),
        (12.0, 60.0),
        |r, _, _a| format!("lib{}-{}.rlib", r.pick(DLL_WORDS), r.hex(16)),
    );
    c.spread(
        &format!("{u}\\.vscode\\extensions"),
        4800,
        gib(1.4),
        (5.0, 200.0),
        |r, _, _a| format!("{}.js", r.pick(DLL_WORDS)),
    );
    let home = c.dir(u);
    c.file(home, "NTUSER.DAT", 18 * MB, 0.2);

    documents(c);

    // Pictures: phone imports keep coming.
    c.spread(
        &format!("{u}\\Pictures\\Camera Roll"),
        9800,
        gib(71.0),
        (0.5, 720.0),
        |_, _, _a| photo_name("IMG", "jpg", _a, now),
    );
    c.spread(
        &format!("{u}\\Pictures\\Camera Roll"),
        340,
        gib(96.0),
        (0.5, 720.0),
        |_, _, _a| photo_name("VID", "mp4", _a, now),
    );
    c.spread(
        &format!("{u}\\Pictures\\Camera Roll"),
        900,
        gib(4.6),
        (0.3, 28.0),
        |_, _, _a| photo_name("IMG", "jpg", _a, now),
    );
    c.spread(
        &format!("{u}\\Pictures\\Camera Roll"),
        26,
        gib(5.2),
        (0.3, 28.0),
        |_, _, _a| photo_name("VID", "mp4", _a, now),
    );
    c.spread(
        &format!("{u}\\Pictures\\Screenshots"),
        960,
        gib(1.2),
        (0.1, 500.0),
        |_, _, _a| format!("Screenshot {}.png", stamp(_a, now, "-", "")),
    );
    c.spread(
        &format!("{u}\\Pictures\\Lisbon 2025"),
        420,
        gib(2.5),
        (380.0, 381.0),
        |_, _, _a| photo_name("DSC", "JPG", _a, now).replace("DSC_", "DSC0"),
    );
    c.spread(
        &format!("{u}\\Pictures\\Wallpapers"),
        48,
        gib(0.21),
        (100.0, 900.0),
        |r, i, _a| {
            format!(
                "{}-{i}.jpg",
                r.pick(&[
                    "mountains",
                    "aurora",
                    "forest",
                    "city-night",
                    "coast",
                    "desert",
                    "nebula"
                ])
            )
        },
    );

    // Videos: game captures, mostly this month.
    let games = ["Starfall Odyssey", "Pixel Kart Racing", "Hollow Depths"];
    c.spread(
        &format!("{u}\\Videos\\Captures"),
        46,
        gib(38.0),
        (0.2, 26.0),
        |r, _, _a| format!("{} {}.mp4", r.pick(&games), stamp(_a, now, "-", "-")),
    );
    c.spread(
        &format!("{u}\\Videos\\Captures"),
        70,
        gib(82.0),
        (40.0, 400.0),
        |r, _, _a| format!("{} {}.mp4", r.pick(&games), stamp(_a, now, "-", "-")),
    );
    c.spread(
        &format!("{u}\\Videos\\OBS"),
        34,
        gib(61.0),
        (30.0, 300.0),
        |_, _, _a| format!("{}.mkv", stamp(_a, now, "-", "-")),
    );

    downloads(c);

    // OneDrive: recent documents local, old photos online only (no disk space).
    let od = format!("{u}\\OneDrive");
    c.spread(
        &format!("{od}\\Documents"),
        380,
        gib(2.4),
        (1.0, 400.0),
        |r, i, _a| {
            format!(
                "{} {}.{}",
                r.pick(&[
                    "Notes", "Scan", "Receipt", "Plan", "Draft", "Invoice", "Letter"
                ]),
                i,
                r.pick(&["pdf", "docx", "xlsx", "md"])
            )
        },
    );
    for y in 2019..=2023 {
        let n = 2800;
        let mut f = |r: &mut Rng, i: usize, _a: f64| {
            format!(
                "IMG_{y}{:02}{:02}_{:06}.jpg",
                1 + i % 12,
                1 + r.int(28),
                r.int(235959)
            )
        };
        c.spread_with(
            &format!("{od}\\Pictures\\Camera Roll {y}"),
            n,
            gib(26.0),
            (400.0, 1200.0),
            OFFLINE | RECALL | 0x20,
            &mut f,
        );
    }
    c.spread(
        &format!("{u}\\Videos\\Exports"),
        18,
        gib(26.0),
        (5.0, 200.0),
        |_, i, _a| format!("Highlights {:02}.mp4", i + 1),
    );
    c.spread(
        &format!("{od}\\Desktop"),
        30,
        gib(0.3),
        (2.0, 300.0),
        |r, _, _a| format!("{}.lnk", r.pick(DLL_WORDS)),
    );

    // Music, Desktop.
    for (album, n) in [
        ("Lo-fi Sessions Vol. 2", 14),
        ("Night Drive", 11),
        ("Field Recordings 2024", 22),
        ("Synthwave Essentials", 18),
    ] {
        c.spread(
            &format!("{u}\\Music\\{album}"),
            n,
            gib(0.9),
            (100.0, 800.0),
            |_, i, _a| format!("{:02} Track {:02}.flac", i + 1, i + 1),
        );
    }
    let desk = c.dir(&format!("{u}\\Desktop"));
    for (n, s, a) in [
        ("Q4 planning.pptx", 14 * MB, 1.5),
        ("todo.txt", 3 * KB, 0.1),
        ("Starfall Odyssey.url", KB, GAME_DAYS),
        ("Iceland trip shot list.docx", 220 * KB, 20.0),
    ] {
        c.file(desk, n, s, a);
    }
    c.spread(
        "Users\\Public\\Documents",
        20,
        40 * MB,
        (100.0, 700.0),
        |_, i, _a| format!("Shared {i}.pdf"),
    );
}

fn documents(c: &mut Vol) {
    let u = USER;
    let docs: &[(&str, &str, u64, f64)] = &[
        ("Finance", "Q3 budget final v2.xlsx", 186 * KB, 24.0),
        ("Finance", "Q3 budget final v2 (1).xlsx", 186 * KB, 24.0),
        ("Finance", "Q3 budget final.xlsx", 171 * KB, 31.0),
        ("Finance", "Q4 budget draft.xlsx", 142 * KB, 2.0),
        ("Finance", "Household expenses 2026.xlsx", 98 * KB, 0.4),
        (
            "Finance\\Taxes 2025",
            "Tax return 2025.pdf",
            1_240 * KB,
            190.0,
        ),
        ("Finance\\Taxes 2025", "Receipts 2025.zip", 312 * MB, 200.0),
        (
            "Finance\\Taxes 2025",
            "Charity donations 2025.pdf",
            420 * KB,
            200.0,
        ),
        ("Home", "Lease agreement 2025 signed.pdf", 2_900 * KB, 300.0),
        ("Home", "Insurance policy 2026.pdf", 1_700 * KB, 40.0),
        ("Home", "Car service invoice 2026-07.pdf", 380 * KB, 90.0),
        ("Home", "Garden studio quotes.xlsx", 64 * KB, 12.0),
        ("Work", "Resume - Alex 2026.docx", 92 * KB, 60.0),
        ("Work", "Resume - Alex 2026.pdf", 210 * KB, 60.0),
        (
            "Work",
            "Project proposal - garden studio.docx",
            1_800 * KB,
            15.0,
        ),
        (
            "Work\\Meetings",
            "Meeting notes 2026-09-12.md",
            9 * KB,
            40.0,
        ),
        (
            "Work\\Meetings",
            "Meeting notes 2026-10-03.md",
            11 * KB,
            18.0,
        ),
        ("Work\\Meetings", "Meeting notes 2026-10-17.md", 7 * KB, 4.0),
        (
            "Work\\Presentations",
            "Quarterly review Q3.pptx",
            48 * MB,
            25.0,
        ),
        (
            "Work\\Presentations",
            "Quarterly review Q3 (old).pptx",
            48 * MB,
            33.0,
        ),
        ("Travel", "Iceland itinerary.pdf", 680 * KB, 30.0),
        ("Travel", "Iceland trip budget.xlsx", 44 * KB, 28.0),
        ("Travel", "Passport scan.pdf", 1_100 * KB, 400.0),
        (
            "Adobe\\Premiere Pro\\26.0",
            "Iceland trip 2026 - edit.prproj",
            64 * MB,
            0.6,
        ),
    ];
    for &(f, n, s, a) in docs {
        let d = c.dir(&format!("{u}\\Documents\\{f}"));
        c.file(d, n, s, a);
    }
    c.spread(
        &format!("{u}\\Documents\\Archive"),
        1300,
        gib(6.3),
        (300.0, 2000.0),
        |r, i, _a| {
            format!(
                "{} {} {}.{}",
                r.pick(&[
                    "Report",
                    "Invoice",
                    "Letter",
                    "Statement",
                    "Notes",
                    "Contract",
                    "Manual",
                    "Photo scan"
                ]),
                2016 + i % 9,
                r.int(999),
                r.pick(&["pdf", "docx", "xlsx", "jpg", "zip"])
            )
        },
    );
    c.spread(
        &format!("{u}\\Documents\\Adobe\\Premiere Pro\\26.0\\Auto-Save"),
        40,
        gib(1.8),
        (0.1, 18.0),
        |_, i, _a| format!("Iceland trip 2026 - edit--{:02}.prproj", i),
    );
}

fn downloads(c: &mut Vol) {
    let now = c.now;
    let dl = c.dir(&format!("{USER}\\Downloads"));
    let items: &[(&str, u64, f64)] = &[
        ("Win11_25H2_English_x64.iso", 6_340_000_000, 3.1),
        ("ubuntu-24.04.3-desktop-amd64.iso", 6_224_000_000, 45.0),
        ("VSCodeUserSetup-x64-1.104.2.exe", 108 * MB, 9.0),
        ("VSCodeUserSetup-x64-1.103.1.exe", 105 * MB, 48.0),
        ("Git-2.51.0-64-bit.exe", 68 * MB, 40.0),
        ("rustup-init.exe", 9 * MB, 200.0),
        ("python-3.13.7-amd64.exe", 28 * MB, 52.0),
        ("node-v22.19.0-x64.msi", 31 * MB, 38.0),
        ("OBS-Studio-32.0-Windows-x64-Installer.exe", 151 * MB, 70.0),
        ("Blender-4.5.3-windows-x64.msi", 380 * MB, 33.0),
        ("Docker Desktop Installer.exe", 610 * MB, 95.0),
        ("SteamSetup.exe", 2 * MB, 400.0),
        ("setup.exe", 74 * MB, 120.0),
        ("setup (1).exe", 74 * MB, 120.0),
        ("setup (2).exe", 74 * MB, 119.0),
        ("photos-export-2026-08.zip", 3_870_000_000, 62.0),
        ("photos-export-2026-08 (1).zip", 3_870_000_000, 61.0),
        ("Iceland trip music license.pdf", 210 * KB, 19.0),
        ("invoice_2026_09.pdf", 160 * KB, 26.0),
        ("invoice_2026_10.pdf", 158 * KB, 1.0),
        (
            "Starfall Odyssey soundtrack.zip",
            1_480_000_000,
            GAME_DAYS - 1.0,
        ),
        ("drone-firmware-v2.4.1.bin", 410 * MB, 22.0),
        ("LUTs - cinematic pack.zip", 860 * MB, 21.0),
        ("Sample footage 8K.mov", 7_900_000_000, 150.0),
        ("dataset-street-signs.tar.gz", 11_400_000_000, 104.0),
        ("old-laptop-backup.zip", 18_200_000_000, 210.0),
        (
            "Fedora-Workstation-Live-x86_64-42.iso",
            2_300_000_000,
            160.0,
        ),
    ];
    for &(n, s, a) in items {
        c.file(dl, n, s, a);
    }
    c.spread(
        &format!("{USER}\\Downloads"),
        260,
        gib(1.4),
        (0.5, 600.0),
        |r, i, _a| {
            format!(
                "{} ({}).{}",
                r.pick(&[
                    "document",
                    "image",
                    "receipt",
                    "scan",
                    "export",
                    "statement",
                    "ticket"
                ]),
                i,
                r.pick(&["pdf", "png", "jpg", "csv", "zip"])
            )
        },
    );
    // What the cleanup removed: old ISOs, installers and zips.
    for (s, a) in [
        (5.9, 160.0),
        (4.7, 220.0),
        (6.1, 300.0),
        (14.5, 250.0),
        (3.2, 90.0),
        (2.9, 70.0),
        (1.1, 40.0),
        (0.8, 30.0),
    ] {
        c.ghost(dl, gib(s), a, CLEANUP_DAYS);
    }
    let _ = now;
}

fn rust_target(c: &mut Vol, path: &str, now_gb: f64, then: (f64, f64), age: (f64, f64)) {
    // `then`: (days ago, size then in GiB). Artifacts appear over time; older
    // ones were there before `then.0`.
    let n_old = 1600;
    let n_new = 4200;
    let old = gib(then.1);
    let new = gib(now_gb).saturating_sub(old);
    let names = |r: &mut Rng, i: usize, _a: f64| {
        let c = r.pick(&[
            "serde",
            "tokio",
            "wgpu",
            "winit",
            "image",
            "egui",
            "glam",
            "tilemap_editor",
            "naga",
            "rayon",
            "png",
            "toml",
            "syn",
            "proc_macro2",
        ]);
        let h = r.hex(16);
        match i % 5 {
            0 => format!("lib{c}-{h}.rlib"),
            1 => format!("lib{c}-{h}.rmeta"),
            2 => format!("{c}-{h}.d"),
            3 => format!("{c}-{h}.pdb"),
            _ => format!("{c}-{h}.exe"),
        }
    };
    c.spread(
        &format!("{path}\\debug\\deps"),
        n_old,
        old,
        (then.0 + 1.0, age.1),
        names,
    );
    c.spread(
        &format!("{path}\\debug\\incremental"),
        n_new,
        new / 2,
        (age.0, then.0),
        |r, _, _a| format!("s-{}-{}.bin", r.hex(10), r.hex(8)),
    );
    c.spread(
        &format!("{path}\\debug\\deps"),
        n_new / 3,
        new / 2,
        (age.0, then.0),
        names,
    );
    c.spread(
        &format!("{path}\\debug\\build"),
        300,
        gib(0.2),
        (then.0, age.1),
        |r, _, _a| format!("{}-{}.out", r.pick(DLL_WORDS), r.hex(16)),
    );
}

fn git(c: &mut Vol, path: &str, gb: f64) {
    c.spread(
        &format!("{path}\\.git\\objects\\pack"),
        6,
        gib(gb * 0.8),
        (1.0, 300.0),
        |r, i, _a| format!("pack-{}.{}", r.hex(40), ["pack", "idx", "rev"][i % 3]),
    );
    c.spread(
        &format!("{path}\\.git\\objects\\4f"),
        120,
        gib(gb * 0.2),
        (0.1, 30.0),
        |r, _, _a| r.hex(38),
    );
    let g = c.dir(&format!("{path}\\.git"));
    for n in ["HEAD", "config", "index", "description", "packed-refs"] {
        c.file(g, n, 2 * KB, 1.0);
    }
}

const NPM: &[&str] = &[
    "react",
    "react-dom",
    "next",
    "@babel\\core",
    "@babel\\parser",
    "@babel\\traverse",
    "typescript",
    "eslint",
    "prettier",
    "lodash",
    "date-fns",
    "zod",
    "postcss",
    "tailwindcss",
    "autoprefixer",
    "@types\\node",
    "@types\\react",
    "chart.js",
    "d3-scale",
    "d3-shape",
    "axios",
    "undici",
    "esbuild",
    "@esbuild\\win32-x64",
    "rollup",
    "vite",
    "@swc\\core",
    "@swc\\core-win32-x64-msvc",
    "sharp",
    "@img\\sharp-win32-x64",
    "caniuse-lite",
    "browserslist",
    "semver",
    "glob",
    "minimatch",
    "chalk",
    "debug",
    "ms",
    "uuid",
    "nanoid",
    "picocolors",
    "source-map",
    "acorn",
    "ajv",
    "tslib",
    "@next\\swc-win32-x64-msvc",
    "styled-jsx",
    "scheduler",
    "loose-envify",
    "js-tokens",
    "graceful-fs",
    "fast-glob",
    "micromatch",
    "braces",
    "fill-range",
    "to-regex-range",
    "is-number",
];

fn npm_names(r: &mut Rng, n: usize) -> Vec<String> {
    let mut v: Vec<String> = NPM.iter().map(|s| s.to_string()).collect();
    while v.len() < n {
        v.push(format!(
            "{}-{}",
            r.pick(DLL_WORDS),
            r.pick(&[
                "utils", "parser", "core", "plugin", "loader", "js", "types", "helpers", "compat"
            ])
        ));
        v.dedup();
    }
    v.truncate(n);
    v
}

fn dev(c: &mut Vol) {
    let d = format!("{USER}\\dev");
    // A Rust app whose target folder keeps growing.
    let te = format!("{d}\\tilemap-editor");
    c.spread(
        &format!("{te}\\src"),
        86,
        1_400 * KB,
        (0.1, 200.0),
        |r, _, _a| {
            format!(
                "{}.rs",
                r.pick(&[
                    "main", "app", "map", "tiles", "render", "input", "brush", "layers", "undo",
                    "ui", "io", "camera", "palette", "export", "grid", "shader"
                ])
            )
        },
    );
    for (n, s) in [
        ("Cargo.toml", 2 * KB),
        ("Cargo.lock", 96 * KB),
        ("README.md", 6 * KB),
        (".gitignore", KB),
    ] {
        let dd = c.dir(&te);
        c.file(dd, n, s, 3.0);
    }
    git(c, &te, 0.34);
    rust_target(
        c,
        &format!("{te}\\target"),
        14.6,
        (34.0, 2.3),
        (0.05, 120.0),
    );
    // An old Rust tool nobody built for months.
    let ps = format!("{d}\\photo-sorter");
    c.spread(
        &format!("{ps}\\src"),
        24,
        300 * KB,
        (140.0, 400.0),
        |_, i, _a| format!("mod{i}.rs"),
    );
    git(c, &ps, 0.05);
    c.spread(
        &format!("{ps}\\target\\release\\deps"),
        1900,
        gib(6.4),
        (140.0, 260.0),
        |r, _, _a| format!("lib{}-{}.rlib", r.pick(DLL_WORDS), r.hex(16)),
    );
    // Web projects with node_modules.
    for (p, pkgs, gb, age) in [
        ("weather-dashboard", 900usize, 0.95, (3.0, 90.0)),
        ("blog", 620, 0.55, (60.0, 300.0)),
        ("portfolio-site", 540, 0.48, (200.0, 500.0)),
    ] {
        let names = npm_names(&mut c.rng, pkgs);
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let base = format!("{d}\\{p}");
        c.tree(&format!("{base}\\node_modules"), &refs, 10, gib(gb), age);
        c.spread(
            &format!("{base}\\src"),
            60,
            900 * KB,
            (age.0, age.1),
            |r, _, _a| {
                format!(
                    "{}.tsx",
                    r.pick(&[
                        "App", "index", "Chart", "Forecast", "Header", "Card", "layout", "page",
                        "api", "hooks", "utils"
                    ])
                )
            },
        );
        c.spread(
            &format!("{base}\\.next\\cache\\webpack"),
            120,
            gib(0.4),
            (age.0, age.0 + 5.0),
            |r, _, _a| format!("{}.pack.gz", r.hex(8)),
        );
        let dd = c.dir(&base);
        for n in [
            "package.json",
            "package-lock.json",
            "tsconfig.json",
            "README.md",
            "next.config.mjs",
        ] {
            c.file(dd, n, 30 * KB, age.0);
        }
        git(c, &base, 0.08);
    }
    // Python home automation with a venv and caches.
    let ha = format!("{d}\\home-automation");
    c.spread(
        &format!("{ha}\\.venv\\Lib\\site-packages"),
        7400,
        gib(0.7),
        (40.0, 90.0),
        |r, _, _a| format!("{}.py", r.pick(DLL_WORDS)),
    );
    c.spread(
        &format!("{ha}\\src\\__pycache__"),
        60,
        3 * MB,
        (1.0, 40.0),
        |r, _, _a| format!("{}.cpython-313.pyc", r.pick(DLL_WORDS)),
    );
    c.spread(&format!("{ha}\\src"), 60, MB, (1.0, 200.0), |r, _, _a| {
        format!(
            "{}.py",
            r.pick(&[
                "lights",
                "thermostat",
                "scheduler",
                "mqtt",
                "sensors",
                "dashboard",
                "config",
                "main"
            ])
        )
    });
    git(c, &ha, 0.02);
    // ML experiments: datasets, a big venv, checkpoints written this month.
    let ml = format!("{d}\\ml-experiments");
    c.spread(
        &format!("{ml}\\.venv\\Lib\\site-packages"),
        21_000,
        gib(5.1),
        (30.0, 34.0),
        |r, _, _a| {
            format!(
                "{}.{}",
                r.pick(DLL_WORDS),
                r.pick(&["py", "pyd", "dll", "pyc"])
            )
        },
    );
    c.spread(
        &format!("{ml}\\datasets"),
        14,
        gib(9.2),
        (100.0, 105.0),
        |r, i, _a| {
            format!(
                "street-signs-{}.{}",
                ["train", "val", "test"][i % 3],
                r.pick(&["parquet", "tar"])
            )
        },
    );
    c.spread(
        &format!("{ml}\\checkpoints"),
        18,
        gib(30.0),
        (0.5, 19.0),
        |_, i, _a| format!("signs-resnet50-epoch{:03}.safetensors", (i + 1) * 5),
    );
    c.spread(
        &format!("{ml}\\runs"),
        600,
        gib(0.6),
        (0.5, 19.0),
        |r, _, _a| {
            format!(
                "events.out.tfevents.{}.ALEX-DESKTOP",
                1_780_000_000 + r.int(9_000_000)
            )
        },
    );
    c.spread(
        &format!("{ml}\\notebooks"),
        12,
        8 * MB,
        (0.5, 30.0),
        |_, i, _a| format!("experiment-{i:02}.ipynb"),
    );
    // A game jam project.
    let gj = format!("{d}\\game-jam-2025");
    c.spread(
        &format!("{gj}\\.godot\\imported"),
        2200,
        gib(2.1),
        (300.0, 310.0),
        |r, _, _a| {
            format!(
                "{}.png-{}.ctex",
                r.pick(&["tiles", "hero", "enemy", "ui", "font", "bg", "fx"]),
                r.hex(32)
            )
        },
    );
    c.spread(
        &format!("{gj}\\exports"),
        6,
        gib(1.2),
        (300.0, 302.0),
        |_, i, _a| format!("lantern-keeper-v{}.zip", i + 1),
    );
}

fn games_and_video(d: &mut Vol) {
    let now = d.now;
    // Steam games (made-up titles). Starfall Odyssey was installed this month;
    // Iron Harbor was uninstalled.
    let games: &[(&str, f64, f64)] = &[
        ("Starfall Odyssey", 96.4, GAME_DAYS),
        ("Pixel Kart Racing", 38.2, 300.0),
        ("Hollow Depths", 61.7, 500.0),
        ("Skyline Tycoon", 22.4, 420.0),
        ("Deep Blue Survey", 117.0, 260.0),
        ("Ember Knights Saga", 143.0, 180.0),
        ("Tiny Gardens", 4.1, 700.0),
    ];
    let lib = "SteamLibrary\\steamapps";
    for &(g, gb, age) in games {
        let base = format!("{lib}\\common\\{g}");
        let paks = 6 + d.rng.int(10);
        d.spread(
            &format!("{base}\\Content\\Paks"),
            paks,
            gib(gb * 0.93),
            (age - 0.05, age),
            |_, i, _a| format!("pakchunk{i}-Windows.pak"),
        );
        d.spread(
            &format!("{base}\\Binaries\\Win64"),
            40,
            gib(gb * 0.03),
            (age - 0.05, age),
            dll,
        );
        d.spread(
            &format!("{base}\\Engine\\Content"),
            300,
            gib(gb * 0.04),
            (age - 0.05, age),
            |r, _, _a| format!("{}.uasset", r.pick(DLL_WORDS)),
        );
        let m = d.dir(lib);
        let app = format!("appmanifest_{}.acf", 1_000_000 + d.rng.int(2_000_000));
        d.file(m, &app, 2 * KB, age);
    }
    d.spread(
        &format!("{lib}\\shadercache"),
        900,
        gib(8.5),
        (0.5, GAME_DAYS),
        |r, _, _a| r.hex(16),
    );
    d.spread(
        &format!("{lib}\\workshop\\content\\1830240"),
        320,
        gib(12.0),
        (20.0, 300.0),
        |r, _, _a| format!("{}.pak", r.hex(10)),
    );
    let ih = d.dir(&format!("{lib}\\common\\Iron Harbor"));
    d.ghost(ih, gib(74.0), 400.0, 20.5);

    // Video projects: the Iceland trip grew day by day as footage came in.
    let ice = "Video Projects\\Iceland trip 2026";
    for day in 1..=9usize {
        let age = 18.5 - day as f64 * 1.6;
        let clips = 8 + d.rng.int(9);
        let total = gib(d.rng.range(9.0, 17.0));
        let start = 1 + (day - 1) * 20;
        d.spread(
            &format!("{ice}\\Footage\\Day {day}"),
            clips,
            total,
            (age - 0.2, age),
            move |_, i, _a| format!("C{:04}.MP4", start + i),
        );
        d.spread(
            &format!("{ice}\\Proxies\\Day {day}"),
            clips,
            total / 9,
            (age - 0.25, age - 0.2),
            move |_, i, _a| format!("C{:04}_Proxy.mp4", start + i),
        );
    }
    d.spread(
        &format!("{ice}\\Audio"),
        40,
        gib(2.4),
        (2.0, 17.0),
        |_, i, _a| format!("ZOOM{:04}.WAV", i + 1),
    );
    d.spread(
        &format!("{ice}\\Renders"),
        4,
        gib(19.0),
        (0.3, 6.0),
        |_, i, _a| {
            [
                "Iceland trip 2026 - draft v1.mp4",
                "Iceland trip 2026 - draft v2.mp4",
                "Iceland trip 2026 - final v3.mp4",
                "Iceland trip 2026 - teaser.mp4",
            ][i]
                .into()
        },
    );
    let ip = d.dir(ice);
    d.file(ip, "Iceland trip 2026.prproj", 72 * MB, 0.4);
    let hc = "Video Projects\\Harbor Cafe promo 2025";
    d.spread(
        &format!("{hc}\\Footage"),
        120,
        gib(212.0),
        (330.0, 340.0),
        |_, i, _a| format!("A{:03}_C{:03}.MOV", 1 + i / 30, 1 + i % 30),
    );
    d.spread(
        &format!("{hc}\\Renders"),
        6,
        gib(14.0),
        (320.0, 325.0),
        |_, i, _a| format!("Harbor Cafe promo - v{}.mp4", i + 1),
    );
    d.spread(
        "Video Projects\\Drone tests",
        60,
        gib(48.0),
        (90.0, 200.0),
        |_, _, _a| photo_name("DJI", "MP4", _a, now),
    );

    // Backups, VMs and media.
    d.spread(
        "Backups\\Phone backup 2025-12\\DCIM\\Camera",
        6200,
        gib(58.0),
        (300.0, 301.0),
        |r, _, _a| photo_name("IMG", "jpg", _a + r.range(30.0, 800.0), now),
    );
    d.spread(
        "Backups\\Old laptop 2021\\Users\\alex\\Documents",
        2400,
        gib(14.0),
        (1500.0, 1501.0),
        |r, i, _a| {
            format!(
                "{} {} {}.{}",
                r.pick(&[
                    "Report",
                    "Invoice",
                    "Letter",
                    "Statement",
                    "Notes",
                    "Contract"
                ]),
                2016 + i % 6,
                r.int(999),
                r.pick(&["pdf", "docx", "xlsx"])
            )
        },
    );
    let ol = d.dir("Backups\\Old laptop 2021\\Users\\alex\\Documents\\Finance");
    d.file(ol, "Q3 budget final v2.xlsx", 186 * KB, 1500.0);
    d.spread(
        "Backups\\Photos archive",
        8,
        gib(180.0),
        (600.0, 900.0),
        |_, i, _a| format!("Photos {}.zip", 2014 + i),
    );
    d.spread(
        "Backups\\Game saves",
        300,
        gib(1.2),
        (1.0, 400.0),
        |r, _, _a| format!("{}.sav", r.hex(8)),
    );
    d.spread(
        "VMs\\Windows 11 dev",
        3,
        gib(71.0),
        (2.0, 200.0),
        |_, i, _a| {
            [
                "Windows 11 dev.vhdx",
                "Windows 11 dev.vmcx",
                "Windows 11 dev.vmrs",
            ][i]
                .into()
        },
    );
    d.spread(
        "VMs\\Ubuntu server",
        2,
        gib(38.0),
        (40.0, 300.0),
        |_, i, _a| ["ubuntu-server.vhdx", "ubuntu-server.vmcx"][i].into(),
    );
    d.spread(
        "Media\\Home movies",
        140,
        gib(96.0),
        (200.0, 1800.0),
        |_, i, _a| format!("Home movie {:03}.mkv", i + 1),
    );
    d.spread(
        "Media\\Music library\\FLAC",
        4200,
        gib(118.0),
        (200.0, 2000.0),
        |r, i, _a| {
            format!(
                "{:02} {}.flac",
                1 + i % 16,
                r.pick(&[
                    "Intro", "Morning", "Waves", "Neon", "Drift", "Outro", "Echo", "Glass",
                    "Harbor", "Lights"
                ])
            )
        },
    );
    d.spread(
        "Media\\Audiobooks",
        300,
        gib(21.0),
        (300.0, 900.0),
        |_, i, _a| format!("Chapter {:03}.m4b", i + 1),
    );
    d.spread(
        "$Recycle.Bin\\S-1-5-21-3623811015-3361044348-30300820-1001",
        12,
        gib(3.1),
        (1.0, 30.0),
        |r, _, _a| format!("$R{}.MP4", r.hex(6).to_uppercase()),
    );
    d.spread(
        "System Volume Information",
        4,
        gib(1.1),
        (1.0, 30.0),
        |r, _, _a| format!("{{{}}}", r.hex(32)),
    );
}

fn backup_drive(e: &mut Vol) {
    let now = e.now;
    e.spread(
        "WindowsImageBackup\\ALEX-DESKTOP\\Backup 2026-03-14 101507",
        4,
        gib(214.0),
        (222.0, 222.1),
        |r, i, _a| {
            if i < 2 {
                format!("{}.vhdx", r.hex(32))
            } else {
                format!("{}_Components.xml", r.hex(32))
            }
        },
    );
    e.spread(
        "Archive\\Photos 2015-2020",
        21_000,
        gib(148.0),
        (900.0, 901.0),
        |r, _, _a| photo_name("IMG", "jpg", _a + r.range(0.0, 2000.0), now),
    );
    e.spread(
        "Archive\\Projects 2019-2022",
        18_000,
        gib(42.0),
        (700.0, 701.0),
        |r, _, _a| {
            format!(
                "{}.{}",
                r.pick(DLL_WORDS),
                r.pick(&["psd", "blend", "png", "wav", "zip", "rs", "js"])
            )
        },
    );
    e.spread(
        "Archive\\Video exports",
        90,
        gib(262.0),
        (400.0, 800.0),
        |_, i, _a| format!("Export {:03}.mp4", i + 1),
    );
    e.spread(
        "Archive\\Phone backup 2023-06",
        4800,
        gib(39.0),
        (850.0, 851.0),
        |r, _, _a| photo_name("IMG", "jpg", _a + r.range(30.0, 700.0), now),
    );
    e.spread(
        "Archive\\Iceland trip 2026 (copy)\\Footage",
        60,
        gib(54.0),
        (8.0, 8.1),
        |_, i, _a| format!("C{:04}.MP4", i + 1),
    );
}
