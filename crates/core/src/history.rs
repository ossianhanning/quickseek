//! Folder-size history: periodic snapshots of how big every sizeable folder
//! is, so "what grew since last week" can be answered.
//!
//! A snapshot keeps, per volume, the folders of at least [`MIN_FOLDER`]
//! bytes (an ancestor-closed set, since folder sizes include their
//! contents) with their size, online-only bytes and file count, and (since
//! format 3) the files of at least that size in them, such as the page file
//! directly on `C:\`. Folders and files below the threshold count as empty
//! then, so a change is off by at most that much. Format 2 snapshots (folders
//! only) still load; a comparison with one can't tell how a file that changed
//! since then has grown. Files on disk: `<unix seconds>-<a|s>.snap` in the history
//! folder: an uncompressed header (so listing is cheap) and a zstd body of
//! varint columns, typically well under a megabyte.
//!
//! Retention ([`keep`]): every snapshot of the last 2 days, then one per day
//! for 31 days, one per week for 26 weeks, one per month for 24 months.
//! Save points (named, taken on request) are kept for a year.

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Folders smaller than this aren't stored.
pub const MIN_FOLDER: u64 = 10 << 20;
/// At most this many folders per volume (the biggest).
pub const MAX_FOLDERS: usize = 60_000;
/// At most this many files per volume (the biggest).
pub const MAX_FILES: usize = 20_000;
const MAGIC: &[u8; 8] = b"RFSNAP\x00\x03";
/// Before files were stored.
const MAGIC_V2: &[u8; 8] = b"RFSNAP\x00\x02";

fn strip_magic(data: &[u8]) -> Option<(&[u8], bool)> {
    match data.get(..8) {
        Some(m) if m == MAGIC => Some((&data[8..], true)),
        Some(m) if m == MAGIC_V2 => Some((&data[8..], false)),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SnapFolder {
    /// Index of the parent in the same list (`u32::MAX` for the root, which is first).
    pub parent: u32,
    pub name: String,
    pub size: u64,
    pub cloud: u64,
    pub files: u64,
}

/// A file of at least [`MIN_FOLDER`] bytes.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SnapFile {
    /// Index of its folder in [`SnapVolume::folders`].
    pub parent: u32,
    pub name: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SnapVolume {
    pub key: String,
    pub capacity: u64,
    pub free: u64,
    pub folders: Vec<SnapFolder>,
    pub files: Vec<SnapFile>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    /// Unix seconds.
    pub time: i64,
    /// Save points have a name; daily snapshots don't.
    pub name: String,
    pub volumes: Vec<SnapVolume>,
    /// Big files were recorded (format 3): a file missing from it was smaller then.
    pub has_files: bool,
}

/// What listing shows about a snapshot.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SnapInfo {
    /// File stem, e.g. `1760900000-a`.
    pub id: String,
    pub time: i64,
    pub name: String,
    /// Folders stored, over all volumes.
    pub folders: u64,
    /// Size of the snapshot file.
    pub bytes: u64,
}

impl SnapInfo {
    pub fn is_save_point(&self) -> bool {
        self.id.ends_with("-s")
    }
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

// ---- encoding ----------------------------------------------------------------------

fn put(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

struct Rd<'a>(&'a [u8]);

fn bad() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "damaged snapshot")
}

impl Rd<'_> {
    fn u(&mut self) -> io::Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let (&b, rest) = self.0.split_first().ok_or_else(bad)?;
            self.0 = rest;
            v |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(bad())
    }
    fn s(&mut self) -> io::Result<String> {
        let n = self.u()? as usize;
        if self.0.len() < n {
            return Err(bad());
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        String::from_utf8(a.to_vec()).map_err(|_| bad())
    }
}

impl Snapshot {
    pub fn encode(&self) -> Vec<u8> {
        // Without files it's written as format 2, which it then is.
        let mut head = if self.has_files { MAGIC } else { MAGIC_V2 }.to_vec();
        put(&mut head, self.time as u64);
        put_str(&mut head, &self.name);
        put(
            &mut head,
            self.volumes.iter().map(|v| v.folders.len() as u64).sum(),
        );
        let mut body = Vec::new();
        put(&mut body, self.volumes.len() as u64);
        for v in &self.volumes {
            put_str(&mut body, &v.key);
            put(&mut body, v.capacity);
            put(&mut body, v.free);
            put(&mut body, v.folders.len() as u64);
            // Columns compress better than rows: parents as distance back, names, then numbers.
            for (i, f) in v.folders.iter().enumerate() {
                put(
                    &mut body,
                    if f.parent == u32::MAX {
                        0
                    } else {
                        (i as u64) - f.parent as u64
                    },
                );
            }
            for f in &v.folders {
                put_str(&mut body, &f.name);
            }
            for f in &v.folders {
                put(&mut body, f.size);
            }
            for f in &v.folders {
                put(&mut body, f.cloud);
            }
            for f in &v.folders {
                put(&mut body, f.files);
            }
            if self.has_files {
                put(&mut body, v.files.len() as u64);
                for f in &v.files {
                    put(&mut body, f.parent as u64);
                    put_str(&mut body, &f.name);
                    put(&mut body, f.size);
                }
            }
        }
        head.extend(zstd::encode_all(&body[..], 9).unwrap_or_default());
        head
    }

    pub fn decode(data: &[u8]) -> io::Result<Snapshot> {
        let (rest, has_files) = strip_magic(data).ok_or_else(bad)?;
        let mut r = Rd(rest);
        let time = r.u()? as i64;
        let name = r.s()?;
        let _folders = r.u()?;
        let body = zstd::decode_all(r.0)?;
        let mut r = Rd(&body);
        let nv = r.u()? as usize;
        let mut volumes = Vec::with_capacity(nv.min(64));
        for _ in 0..nv {
            let key = r.s()?;
            let (capacity, free) = (r.u()?, r.u()?);
            let n = r.u()? as usize;
            if n > 10_000_000 {
                return Err(bad());
            }
            let mut folders: Vec<SnapFolder> = (0..n)
                .map(|i| {
                    let back = r.u()?;
                    Ok(SnapFolder {
                        parent: if back == 0 || back as usize > i {
                            u32::MAX
                        } else {
                            (i as u64 - back) as u32
                        },
                        ..Default::default()
                    })
                })
                .collect::<io::Result<_>>()?;
            for f in &mut folders {
                f.name = r.s()?;
            }
            for f in &mut folders {
                f.size = r.u()?;
            }
            for f in &mut folders {
                f.cloud = r.u()?;
            }
            for f in &mut folders {
                f.files = r.u()?;
            }
            let mut files = Vec::new();
            if has_files {
                let k = r.u()? as usize;
                if k > 10_000_000 {
                    return Err(bad());
                }
                for _ in 0..k {
                    let parent = r.u()?;
                    if parent as usize >= n {
                        return Err(bad());
                    }
                    files.push(SnapFile {
                        parent: parent as u32,
                        name: r.s()?,
                        size: r.u()?,
                    });
                }
            }
            volumes.push(SnapVolume {
                key,
                capacity,
                free,
                folders,
                files,
            });
        }
        Ok(Snapshot {
            time,
            name,
            volumes,
            has_files,
        })
    }

    /// Every stored folder's and file's size by path (see [`PathMap`]).
    /// Volume roots are found by their root name (`C:`).
    pub fn paths(&self) -> PathMap {
        let mut pairs: Vec<(u64, u64)> = Vec::with_capacity(
            self.volumes
                .iter()
                .map(|v| v.folders.len() + v.files.len())
                .sum(),
        );
        let mut roots = HashMap::new();
        for v in &self.volumes {
            let mut full: Vec<String> = Vec::with_capacity(v.folders.len());
            for f in &v.folders {
                let p = match f.parent {
                    u32::MAX => f.name.clone(),
                    i => format!("{}\\{}", full[i as usize], f.name),
                };
                pairs.push((path_hash(&p), f.size));
                full.push(p);
            }
            for f in &v.files {
                if let Some(dir) = full.get(f.parent as usize) {
                    pairs.push((path_hash(&format!("{dir}\\{}", f.name)), f.size));
                }
            }
            if let Some(root) = full.first() {
                roots.insert(path_key(root), (v.capacity, v.free));
            }
        }
        pairs.sort_unstable();
        pairs.dedup_by_key(|p| p.0);
        let (hashes, sizes) = pairs.into_iter().unzip();
        PathMap {
            hashes,
            sizes,
            roots,
            time: self.time,
            has_files: self.has_files,
        }
    }
}

/// One snapshot's folder sizes, looked up by path. Kept compact (16 bytes a
/// folder, sorted path hashes) since a timeline needs every snapshot at once.
#[derive(Debug, Default)]
pub struct PathMap {
    hashes: Vec<u64>,
    sizes: Vec<u64>,
    /// Volume root → (capacity, free).
    roots: HashMap<String, (u64, u64)>,
    pub time: i64,
    /// Big files are in it too (see [`Snapshot::has_files`]).
    pub has_files: bool,
}

/// 64-bit FNV-1a of [`path_key`]: collisions among ~100k folders are
/// vanishingly unlikely, and a wrong match only mislabels one size.
pub fn path_hash(p: &str) -> u64 {
    path_key(p).bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Lookup key for a path: lower case, `/` → `\`, no trailing `\`.
pub fn path_key(p: &str) -> String {
    p.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

impl PathMap {
    /// The folder's (or big file's) size then, if the snapshot has it.
    pub fn get(&self, path: &str) -> Option<u64> {
        self.hashes
            .binary_search(&path_hash(path))
            .ok()
            .map(|i| self.sizes[i])
    }
    /// Capacity and free space of a volume root then.
    pub fn disk(&self, root: &str) -> Option<(u64, u64)> {
        self.roots.get(&path_key(root)).copied()
    }
    pub fn len(&self) -> usize {
        self.hashes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }
    pub fn memory_bytes(&self) -> usize {
        self.hashes.capacity() * 16 + self.roots.len() * 64
    }
}

// ---- retention ---------------------------------------------------------------------

/// Which snapshots to keep at `now` (unix seconds): see the module docs.
pub fn keep(snaps: &[SnapInfo], now: i64) -> Vec<bool> {
    const DAY: i64 = 86_400;
    let mut keep = vec![false; snaps.len()];
    let mut order: Vec<usize> = (0..snaps.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(snaps[i].time)); // newest first
    let mut buckets: HashMap<(u8, i64), ()> = HashMap::new();
    for &i in &order {
        let s = &snaps[i];
        let age = now - s.time;
        if s.is_save_point() {
            keep[i] = age < 365 * DAY;
            continue;
        }
        let bucket = if age < 2 * DAY {
            keep[i] = true;
            continue;
        } else if age < 31 * DAY {
            (0, s.time.div_euclid(DAY))
        } else if age < 26 * 7 * DAY {
            (1, s.time.div_euclid(7 * DAY))
        } else if age < 730 * DAY {
            (2, s.time.div_euclid(30 * DAY))
        } else {
            continue;
        };
        // The newest snapshot of each day/week/month bucket stays.
        if buckets.insert(bucket, ()).is_none() {
            keep[i] = true;
        }
    }
    keep
}

// ---- the store -----------------------------------------------------------------------

struct Loaded {
    map: Arc<PathMap>,
    used: Instant,
}

/// Snapshots in a folder, with decoded ones cached for a few minutes.
pub struct History {
    dir: PathBuf,
    cache: Mutex<HashMap<String, Loaded>>,
}

/// Decoded snapshots are dropped after this long unused.
const CACHE_FOR: Duration = Duration::from_secs(300);

impl History {
    pub fn new(dir: PathBuf) -> History {
        History {
            dir,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Snapshots, oldest first.
    pub fn list(&self) -> Vec<SnapInfo> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".snap") else {
                continue;
            };
            let Ok(mut f) = std::fs::File::open(e.path()) else {
                continue;
            };
            let mut head = [0u8; 512];
            let n = f.read(&mut head).unwrap_or(0);
            let Some((rest, _)) = strip_magic(&head[..n]) else {
                continue;
            };
            let mut r = Rd(rest);
            let (Ok(time), Ok(label), Ok(folders)) = (r.u(), r.s(), r.u()) else {
                continue;
            };
            let bytes = e.metadata().map_or(0, |m| m.len());
            out.push(SnapInfo {
                id: id.to_string(),
                time: time as i64,
                name: label,
                folders,
                bytes,
            });
        }
        out.sort_by_key(|s| s.time);
        out
    }

    /// Writes `s` (a save point if it has a name) and prunes old snapshots.
    pub fn save(&self, s: &Snapshot) -> io::Result<SnapInfo> {
        std::fs::create_dir_all(&self.dir)?;
        let id = format!("{}-{}", s.time, if s.name.is_empty() { "a" } else { "s" });
        let tmp = self.dir.join(format!("{id}.tmp"));
        let data = s.encode();
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, self.dir.join(format!("{id}.snap")))?;
        self.prune(now_unix().max(s.time));
        let folders = s.volumes.iter().map(|v| v.folders.len() as u64).sum();
        Ok(SnapInfo {
            id,
            time: s.time,
            name: s.name.clone(),
            folders,
            bytes: data.len() as u64,
        })
    }

    pub fn prune(&self, now: i64) {
        let list = self.list();
        for (s, k) in list.iter().zip(keep(&list, now)) {
            if !k {
                let _ = std::fs::remove_file(self.dir.join(format!("{}.snap", s.id)));
                self.cache.lock().remove(&s.id);
            }
        }
    }

    pub fn load(&self, id: &str) -> io::Result<Snapshot> {
        if id.contains(['/', '\\', '.']) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bad snapshot id",
            ));
        }
        Snapshot::decode(&std::fs::read(self.dir.join(format!("{id}.snap")))?)
    }

    /// A snapshot's path map (cached).
    pub fn paths(&self, id: &str) -> io::Result<Arc<PathMap>> {
        if let Some(l) = self.cache.lock().get_mut(id) {
            l.used = Instant::now();
            return Ok(l.map.clone());
        }
        let map = Arc::new(self.load(id)?.paths());
        self.cache.lock().insert(
            id.to_string(),
            Loaded {
                map: map.clone(),
                used: Instant::now(),
            },
        );
        Ok(map)
    }

    /// Forgets decoded snapshots nobody used for a while (call now and then).
    pub fn trim_cache(&self) {
        self.cache
            .lock()
            .retain(|_, l| l.used.elapsed() < CACHE_FOR);
    }

    pub fn cached(&self) -> usize {
        self.cache.lock().len()
    }

    /// Adds a volume as it was at `time`, measured elsewhere (a NAS agent's
    /// own daily snapshot), for a day this PC has no record of it: into
    /// this PC's snapshot of the same local day if there is one without it,
    /// else as a snapshot holding only that volume. Returns false if that
    /// day already has the volume.
    pub fn merge_volume(&self, time: i64, vol: SnapVolume) -> io::Result<bool> {
        let day = local_midnight(time);
        let same_day: Vec<SnapInfo> = self
            .list()
            .into_iter()
            .filter(|s| !s.is_save_point() && s.time >= day && s.time < day + 86_400)
            .collect();
        let mut first = None;
        for s in &same_day {
            let snap = self.load(&s.id)?;
            if snap
                .volumes
                .iter()
                .any(|v| v.key.eq_ignore_ascii_case(&vol.key))
            {
                return Ok(false);
            }
            first.get_or_insert((s.id.clone(), snap));
        }
        match first {
            Some((id, mut snap)) => {
                snap.volumes.push(vol);
                let tmp = self.dir.join(format!("{id}.tmp"));
                std::fs::write(&tmp, snap.encode())?;
                std::fs::rename(&tmp, self.dir.join(format!("{id}.snap")))?;
                self.cache.lock().remove(&id);
            }
            None => {
                self.save(&Snapshot {
                    time,
                    name: String::new(),
                    volumes: vec![vol],
                    has_files: true,
                })?;
            }
        }
        Ok(true)
    }

    /// The newest snapshot taken today (local date), if any.
    pub fn has_one_since(&self, t: i64) -> bool {
        self.list()
            .iter()
            .any(|s| s.time >= t && !s.is_save_point())
    }
}

/// Unix time of the most recent local midnight.
pub fn local_midnight(now: i64) -> i64 {
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    let mut tz: TIME_ZONE_INFORMATION = unsafe { std::mem::zeroed() };
    let mode = unsafe { GetTimeZoneInformation(&mut tz) };
    // Bias is minutes to add to local time to get UTC; daylight time adds DaylightBias.
    let bias = tz.Bias + if mode == 2 { tz.DaylightBias } else { 0 };
    let local = now - bias as i64 * 60;
    local - local.rem_euclid(86_400) + bias as i64 * 60
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Snapshot {
        let f = |parent: u32, name: &str, size: u64| SnapFolder {
            parent,
            name: name.into(),
            size,
            cloud: size / 10,
            files: size / 1000,
        };
        Snapshot {
            time: 1_760_000_000,
            name: "before cleanup".into(),
            volumes: vec![SnapVolume {
                key: "C".into(),
                capacity: 1000 << 30,
                free: 30 << 30,
                folders: vec![
                    f(u32::MAX, "C:", 900 << 30),
                    f(0, "Users", 600 << 30),
                    f(1, "Me", 500 << 30),
                    f(0, "Windows", 40 << 30),
                    f(2, "Ångström", 20 << 20),
                ],
                files: vec![SnapFile {
                    parent: 0,
                    name: "pagefile.sys".into(),
                    size: 36 << 30,
                }],
            }],
            has_files: true,
        }
    }

    #[test]
    fn roundtrip_and_paths() {
        let s = sample();
        let b = s.encode();
        assert_eq!(Snapshot::decode(&b).unwrap(), s);
        assert!(Snapshot::decode(&b[..b.len() - 3]).is_err());
        assert!(Snapshot::decode(b"nope").is_err());
        let m = s.paths();
        assert_eq!(m.get(r"c:\USERS\me\"), Some(500 << 30));
        assert_eq!(m.get(r"C:\Users\Me\ångström"), Some(20 << 20));
        assert_eq!(m.get("C:\\"), Some(900 << 30));
        assert_eq!(
            m.get(r"C:\pagefile.sys"),
            Some(36 << 30),
            "big files are kept"
        );
        assert!(m.has_files);
        assert_eq!(m.memory_bytes() / 16, 6 + 4);
        assert_eq!(m.disk(r"C:\"), Some((1000 << 30, 30 << 30)));
        assert!(m.get(r"C:\Temp").is_none());
    }

    /// Snapshots from before files were kept (format 2) still load.
    #[test]
    fn reads_folder_only_snapshots() {
        let mut old = sample();
        old.has_files = false;
        old.volumes[0].files.clear();
        let b = old.encode();
        assert!(b.starts_with(MAGIC_V2));
        assert_eq!(Snapshot::decode(&b).unwrap(), old);
        let m = old.paths();
        assert!(!m.has_files && m.get(r"C:\pagefile.sys").is_none());
        assert_eq!(m.get(r"C:\Users"), Some(600 << 30));
        let dir = std::env::temp_dir().join(format!("quickseek-history-v2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let h = History::new(dir.clone());
        let now = now_unix();
        h.save(&Snapshot {
            time: now - 60,
            ..old
        })
        .unwrap();
        h.save(&Snapshot {
            time: now,
            ..sample()
        })
        .unwrap();
        assert_eq!(h.list().len(), 2, "both formats are listed");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn store_list_prune() {
        let d = std::env::temp_dir().join(format!("quickseek-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let h = History::new(d.clone());
        let mut s = sample();
        s.name.clear();
        let now = now_unix();
        for back in [0i64, 1, 3, 3 * 86_400, 3 * 86_400 + 60, 40 * 86_400] {
            s.time = now - back;
            h.save(&s).unwrap();
        }
        let mut named = sample();
        named.time = now - 100 * 86_400;
        h.save(&named).unwrap();
        let list = h.list();
        // The two on day -3 collapse to one; 40 days ago is weekly; the save point stays.
        assert_eq!(list.len(), 6, "{list:?}");
        assert!(list.iter().any(SnapInfo::is_save_point));
        assert_eq!(
            list.iter().find(|i| i.is_save_point()).unwrap().name,
            "before cleanup"
        );
        let id = &list.last().unwrap().id;
        assert_eq!(h.paths(id).unwrap().len(), 6, "5 folders and a file");
        assert_eq!(h.cached(), 1);
        assert!(h.has_one_since(now - 10));
        assert!(h.load("../x").is_err());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn retention_buckets() {
        const DAY: i64 = 86_400;
        let now = 1_800_000_000;
        let mk = |age: i64, save: bool| SnapInfo {
            id: format!("{}-{}", now - age, if save { "s" } else { "a" }),
            time: now - age,
            ..Default::default()
        };
        let snaps: Vec<SnapInfo> = (0..800)
            .map(|d| mk(d * DAY, false))
            .chain([mk(200 * DAY, true), mk(400 * DAY, true)])
            .collect();
        let k = keep(&snaps, now);
        let kept = k.iter().filter(|&&x| x).count();
        // 2 recent + ~29 daily + ~22 weekly + ~18 monthly + 1 save point (the other is over a year old).
        assert!((60..=80).contains(&kept), "{kept}");
        assert!(k[800] && !k[801]);
    }

    #[test]
    fn merging_a_remote_volume_fills_its_day() {
        let d = std::env::temp_dir().join(format!("qs-hist-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let h = History::new(d.clone());
        let now = now_unix();
        let today = local_midnight(now);
        // The PC took a snapshot two days ago (noon) with C: only.
        let mut pc = sample();
        pc.name.clear();
        pc.time = today - 2 * 86_400 + 43_200;
        h.save(&pc).unwrap();
        let nas = || SnapVolume {
            key: r"\\nas\data".into(),
            capacity: 100,
            free: 40,
            folders: vec![SnapFolder {
                parent: u32::MAX,
                name: r"\\nas\data".into(),
                size: 60 << 20,
                cloud: 0,
                files: 3,
            }],
            files: vec![],
        };
        // Same day: goes into the PC's snapshot.
        assert!(h.merge_volume(pc.time + 3600, nas()).unwrap());
        assert_eq!(h.list().len(), 1);
        let id = h.list()[0].id.clone();
        assert_eq!(h.paths(&id).unwrap().get(r"\\nas\data"), Some(60 << 20));
        // Again: that day has it now.
        assert!(!h.merge_volume(pc.time + 7200, nas()).unwrap());
        // A day the PC was off: a snapshot of its own.
        let t = today - 86_400 + 600;
        assert!(h.merge_volume(t, nas()).unwrap());
        let list = h.list();
        assert_eq!(list.len(), 2);
        assert_eq!(
            h.paths(&list[1].id).unwrap().get(r"\\nas\data"),
            Some(60 << 20)
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn midnight_is_within_a_day() {
        let now = now_unix();
        let m = local_midnight(now);
        assert!(m <= now && now - m < 86_400);
    }
}
