//! Folder-size history: periodic snapshots of how big every sizeable folder
//! is, so "what grew since last week" can be answered.
//!
//! A snapshot keeps, per volume, the folders of at least [`MIN_FOLDER`]
//! bytes (an ancestor-closed set, since folder sizes include their
//! contents) with their size, online-only bytes and file count. Folders
//! below the threshold count as empty then, so a change is off by at most
//! that much. Files on disk: `<unix seconds>-<a|s>.snap` in the history
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
const MAGIC: &[u8; 8] = b"RFSNAP\x00\x02";

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SnapFolder {
    /// Index of the parent in the same list (`u32::MAX` for the root, which is first).
    pub parent: u32,
    pub name: String,
    pub size: u64,
    pub cloud: u64,
    pub files: u64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SnapVolume {
    pub key: String,
    pub capacity: u64,
    pub free: u64,
    pub folders: Vec<SnapFolder>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    /// Unix seconds.
    pub time: i64,
    /// Save points have a name; daily snapshots don't.
    pub name: String,
    pub volumes: Vec<SnapVolume>,
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
        let mut head = MAGIC.to_vec();
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
        }
        head.extend(zstd::encode_all(&body[..], 9).unwrap_or_default());
        head
    }

    pub fn decode(data: &[u8]) -> io::Result<Snapshot> {
        let rest = data.strip_prefix(&MAGIC[..]).ok_or_else(bad)?;
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
            volumes.push(SnapVolume {
                key,
                capacity,
                free,
                folders,
            });
        }
        Ok(Snapshot {
            time,
            name,
            volumes,
        })
    }

    /// Every stored folder's size by path (see [`PathMap`]). Volume roots
    /// are found by their root name (`C:`).
    pub fn paths(&self) -> PathMap {
        let mut pairs: Vec<(u64, u64)> =
            Vec::with_capacity(self.volumes.iter().map(|v| v.folders.len()).sum());
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
    /// The folder's size then, if the snapshot has it.
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
            let Some(rest) = head[..n].strip_prefix(&MAGIC[..]) else {
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
            }],
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
        assert_eq!(m.memory_bytes() / 16, 5 + 4);
        assert_eq!(m.disk(r"C:\"), Some((1000 << 30, 30 << 30)));
        assert!(m.get(r"C:\Temp").is_none());
    }

    #[test]
    fn store_list_prune() {
        let d = std::env::temp_dir().join(format!("rapidfind-history-{}", std::process::id()));
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
        assert_eq!(h.paths(id).unwrap().len(), 5);
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
    fn midnight_is_within_a_day() {
        let now = now_unix();
        let m = local_midnight(now);
        assert!(m <= now && now - m < 86_400);
    }
}
