//! Folder-size tree ("where did my disk space go"): the children of a folder
//! with the space and file count under each, over the whole index or only
//! over what matches a query.
//!
//! A [`Tree`] is built per client session and holds, per volume:
//! * a child list (CSR: offsets + entry ids of visible children),
//! * the number of visible files under every folder,
//! * for a query, the size and file count of matches under each folder.
//!
//! That's ~12 bytes per entry while a tree is open (40 MB for 3.3M entries),
//! dropped when the client closes it. Folder sizes come straight from the
//! index (kept current from the journal); child lists and counts are a
//! snapshot, rebuilt when the index is compacted or after
//! [`REFRESH_AFTER`] once it has changed.
//!
//! Matched folders count with everything inside them (so `folder:
//! ww:node_modules` shows where the node_modules weight is), and nothing
//! is counted twice: matches inside a matched folder are skipped.

use std::collections::HashMap;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::engine::{Engine, VolumeKind};
use crate::fold::fold_mode;
use crate::index::{Index, NONE};
use crate::query::SearchFlags;
use crate::search::{SearchOptions, SortKey};

/// Child lists and counts older than this are rebuilt if the index changed.
pub const REFRESH_AFTER: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TreeSort {
    Name,
    #[default]
    Size,
    Files,
    Modified,
    /// Growth since the comparison snapshot (see [`Tree::base`]).
    Change,
}

impl TreeSort {
    pub fn to_u8(self) -> u8 {
        self as u8
    }
    pub fn from_u8(v: u8) -> Option<TreeSort> {
        Some(match v {
            0 => TreeSort::Name,
            1 => TreeSort::Size,
            2 => TreeSort::Files,
            3 => TreeSort::Modified,
            4 => TreeSort::Change,
            _ => return None,
        })
    }
}

/// One child in a [`TreeLevel`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TreeRow {
    pub name: String,
    /// Containing folder (empty for a volume root).
    pub dir: String,
    /// Bytes stored on the volume under this folder (or the file's size);
    /// only matches when filtered. Online-only cloud files and extra hard
    /// links of a file add nothing here.
    pub size: u64,
    /// Bytes in online-only cloud files (OneDrive etc. placeholders).
    pub cloud: u64,
    /// A file that is another hard link of a file counted elsewhere.
    pub hardlink: bool,
    /// Files under this folder (matching ones when filtered); 1 for a file.
    pub files: u64,
    pub modified: i64,
    pub is_dir: bool,
    pub has_children: bool,
    pub has_size: bool,
    /// The query matched this entry itself.
    pub matched: bool,
    /// Volume key and entry, to ask for this row's children.
    pub vol: String,
    pub idx: u32,
    pub epoch: u64,
    /// Volume roots only: total and free bytes (0 if unknown).
    pub capacity: u64,
    pub free: u64,
    /// Comparing with a snapshot ([`Tree::base`]): `size` then. Folders not
    /// in the snapshot (new, or under its size threshold) count as 0; files
    /// as unchanged unless modified since, then 0 and `changed`.
    pub before: u64,
    pub has_before: bool,
    /// A file modified after the comparison snapshot.
    pub changed: bool,
    /// Volume roots: free space at the comparison snapshot (0 if unknown).
    pub free_before: u64,
}

impl TreeRow {
    /// Bytes added (negative: freed) since the comparison snapshot.
    pub fn change(&self) -> i64 {
        if self.has_before { self.size as i64 - self.before as i64 } else { 0 }
    }
}

impl TreeRow {
    pub fn full_path(&self) -> String {
        match self.dir.as_str() {
            "" if self.name.ends_with('\\') => self.name.clone(),
            "" => format!("{}\\", self.name),
            d if d.ends_with('\\') => format!("{d}{}", self.name),
            d => format!("{d}\\{}", self.name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TreeLevel {
    /// Size, cloud-only bytes and file count of the folder listed (sum of
    /// the volumes at the top).
    pub size: u64,
    pub cloud: u64,
    pub files: u64,
    /// Children in total (`rows` may be a page of them).
    pub total: u64,
    pub rows: Vec<TreeRow>,
    /// Comparing: the listed folder's size then (`has_before`).
    pub before: u64,
    pub has_before: bool,
}

/// Which folder to list: `vol`/`idx`/`epoch` from a row, or a path when
/// those are stale or unknown. All empty = the volume roots.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NodeRef {
    pub vol: String,
    pub idx: u32,
    pub epoch: u64,
    pub path: String,
}

struct VolTree {
    key: String,
    kind: VolumeKind,
    epoch: u64,
    generation: u64,
    /// `kids[start[d]..start[d + 1]]` are the live children of `d` (listing
    /// shows the visible ones; sizes count all, as they're on the disk).
    start: Vec<u32>,
    kids: Vec<u32>,
    /// Folder → slot in the per-folder totals (`NONE` for files).
    slot: Vec<u32>,
    /// Per folder slot: files, bytes on the volume, bytes online-only.
    files: Vec<u32>,
    local: Vec<u64>,
    cloud: Vec<u64>,
    /// With a query: (bytes, files, cloud bytes) of matches under each folder
    /// that has any, and for each counted match itself.
    agg: Option<HashMap<u32, (u64, u64, u64)>>,
    /// With a query: entries the query matched.
    matched: Vec<u64>,
}

fn bit(v: &[u64], i: u32) -> bool {
    v.get(i as usize / 64).is_some_and(|w| w >> (i % 64) & 1 == 1)
}

const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;

/// What a file adds to its folders: (bytes on the volume, cloud bytes,
/// counts). Extra hard links add nothing (the record's first listed link
/// counts), online-only placeholders only cloud bytes.
fn contribution(ix: &Index, i: u32) -> (u64, u64, bool) {
    let e = &ix.entries[i as usize];
    if ix.links(ix.record(i)).next().is_some_and(|head| head != i) {
        return (0, 0, false);
    }
    if e.attrs() & FILE_ATTRIBUTE_OFFLINE != 0 { (0, e.size, true) } else { (e.size, 0, true) }
}

impl VolTree {
    /// All passes are parallel: they are bound by random access to parents,
    /// which many threads hide well.
    fn build(key: &str, kind: VolumeKind, ix: &Index, matches: Option<&[u32]>) -> VolTree {
        let n = ix.entries.len();
        let entries = &ix.entries;
        let atomics = |len: usize| -> Vec<AtomicU32> { (0..len).into_par_iter().map(|_| AtomicU32::new(0)).collect() };
        let atomics64 = |len: usize| -> Vec<AtomicU64> { (0..len).into_par_iter().map(|_| AtomicU64::new(0)).collect() };
        let plain = |v: Vec<AtomicU32>| -> Vec<u32> { v.into_par_iter().map(AtomicU32::into_inner).collect() };
        let plain64 = |v: Vec<AtomicU64>| -> Vec<u64> { v.into_par_iter().map(AtomicU64::into_inner).collect() };
        let live = |e: &crate::index::Entry| !e.is_deleted() && e.parent != NONE;

        // Child lists: count, prefix-sum, fill.
        let counts = atomics(n + 1);
        entries.par_iter().for_each(|e| {
            if live(e) {
                counts[e.parent as usize + 1].fetch_add(1, Relaxed);
            }
        });
        let mut start = plain(counts);
        for i in 0..n {
            start[i + 1] += start[i];
        }
        let fill: Vec<AtomicU32> = start.par_iter().map(|&s| AtomicU32::new(s)).collect();
        let kids_a = atomics(start[n] as usize);
        entries.par_iter().enumerate().for_each(|(i, e)| {
            if live(e) {
                let at = fill[e.parent as usize].fetch_add(1, Relaxed);
                kids_a[at as usize].store(i as u32, Relaxed);
            }
        });
        drop(fill);
        let kids = plain(kids_a);

        // Folders level by level (breadth-first, each level in parallel).
        let mut levels: Vec<Vec<u32>> = Vec::new();
        let mut frontier: Vec<u32> = if (ix.root as usize) < n { vec![ix.root] } else { Vec::new() };
        while !frontier.is_empty() && levels.len() < 1024 {
            let next: Vec<u32> = frontier
                .par_iter()
                .flat_map_iter(|&d| kids[start[d as usize] as usize..start[d as usize + 1] as usize].iter().copied().filter(|&c| entries[c as usize].is_dir()))
                .collect();
            levels.push(std::mem::replace(&mut frontier, next));
        }
        let mut slot = vec![NONE; n];
        let mut next_slot = 0u32;
        for level in &levels {
            for &d in level {
                slot[d as usize] = next_slot;
                next_slot += 1;
            }
        }
        let dirs = next_slot as usize;

        // Per folder: direct files and bytes, folded up deepest level first.
        let (files_a, local_a, cloud_a) = (atomics(dirs), atomics64(dirs), atomics64(dirs));
        (0..n as u32).into_par_iter().for_each(|i| {
            let e = &entries[i as usize];
            if !live(e) || e.is_dir() {
                return;
            }
            let s = slot[e.parent as usize];
            if s == NONE {
                return; // not reachable from the root
            }
            let (l, c, counts) = contribution(ix, i);
            if counts {
                files_a[s as usize].fetch_add(1, Relaxed);
            }
            local_a[s as usize].fetch_add(l, Relaxed);
            cloud_a[s as usize].fetch_add(c, Relaxed);
        });
        for level in levels.iter().rev() {
            level.par_iter().for_each(|&d| {
                let p = entries[d as usize].parent;
                if p != NONE && slot[p as usize] != NONE {
                    let (from, to) = (slot[d as usize] as usize, slot[p as usize] as usize);
                    files_a[to].fetch_add(files_a[from].load(Relaxed), Relaxed);
                    local_a[to].fetch_add(local_a[from].load(Relaxed), Relaxed);
                    cloud_a[to].fetch_add(cloud_a[from].load(Relaxed), Relaxed);
                }
            });
        }
        drop(levels);
        let (files, local, cloud) = (plain(files_a), plain64(local_a), plain64(cloud_a));
        let mut t = VolTree { key: key.to_string(), kind, epoch: ix.epoch, generation: ix.generation, start, kids, slot, files, local, cloud, agg: None, matched: Vec::new() };

        if let Some(m) = matches {
            let mut matched = vec![0u64; n.div_ceil(64)];
            for &i in m {
                matched[i as usize / 64] |= 1 << (i % 64);
            }
            // Dense totals while counting (parallel), kept sparse afterwards.
            let (sizes, counts, clouds) = (atomics64(n), atomics(n), atomics64(n));
            m.par_iter().for_each(|&i| {
                let e = &entries[i as usize];
                let mut p = e.parent;
                let mut guard = 0;
                while p != NONE && guard < 1024 {
                    if bit(&matched, p) && entries[p as usize].is_dir() {
                        return; // inside a matched folder: already counted
                    }
                    p = entries[p as usize].parent;
                    guard += 1;
                }
                let (l, c, n) = if e.is_dir() {
                    let (l, f, c) = t.totals(i);
                    (l, c, f as u32)
                } else {
                    let (l, c, counts) = contribution(ix, i);
                    (l, c, counts as u32)
                };
                let mut a = i;
                let mut guard = 0;
                while a != NONE && guard < 1024 {
                    sizes[a as usize].fetch_add(l, Relaxed);
                    clouds[a as usize].fetch_add(c, Relaxed);
                    counts[a as usize].fetch_add(n, Relaxed);
                    a = entries[a as usize].parent;
                    guard += 1;
                }
            });
            let agg: HashMap<u32, (u64, u64, u64)> = (0..n as u32)
                .into_par_iter()
                .filter_map(|i| {
                    let (l, f, c) = (sizes[i as usize].load(Relaxed), counts[i as usize].load(Relaxed), clouds[i as usize].load(Relaxed));
                    (f > 0 || l > 0 || c > 0 || bit(&matched, i)).then_some((i, (l, f as u64, c)))
                })
                .collect();
            t.agg = Some(agg);
            t.matched = matched;
        }
        t
    }

    /// (bytes, files, cloud bytes) under folder `d`, over everything.
    fn totals(&self, d: u32) -> (u64, u64, u64) {
        match self.slot.get(d as usize).copied().filter(|&s| s != NONE) {
            Some(s) => (self.local[s as usize], self.files[s as usize] as u64, self.cloud[s as usize]),
            None => (0, 0, 0),
        }
    }

    fn children(&self, d: u32) -> &[u32] {
        match (self.start.get(d as usize), self.start.get(d as usize + 1)) {
            (Some(&a), Some(&b)) => &self.kids[a as usize..b as usize],
            _ => &[],
        }
    }

    /// Is `i` (a folder) a match or inside one? Then everything below counts.
    fn full(&self, ix: &Index, mut i: u32) -> bool {
        if self.agg.is_none() {
            return true;
        }
        let mut guard = 0;
        while i != NONE && guard < 1024 {
            if bit(&self.matched, i) && ix.entries[i as usize].is_dir() {
                return true;
            }
            i = ix.entries[i as usize].parent;
            guard += 1;
        }
        false
    }

    /// (bytes, files, cloud bytes) for an entry, given whether its folder counts fully.
    fn value(&self, ix: &Index, i: u32, full: bool) -> Option<(u64, u64, u64)> {
        let all = || {
            if ix.entries[i as usize].is_dir() {
                self.totals(i)
            } else {
                let (l, c, counts) = contribution(ix, i);
                (l, counts as u64, c)
            }
        };
        match &self.agg {
            _ if full => Some(all()),
            None => Some(all()),
            Some(agg) => agg.get(&i).copied(),
        }
    }

    fn row(&self, ix: &Index, i: u32, dir: &str, full: bool) -> Option<TreeRow> {
        let e = &ix.entries[i as usize];
        if !e.is_visible() {
            return None;
        }
        let (size, files, cloud) = self.value(ix, i, full)?;
        let full_here = full || (e.is_dir() && bit(&self.matched, i));
        let visible_kids = || self.children(i).iter().any(|&c| ix.entries[c as usize].is_visible());
        Some(TreeRow {
            name: ix.name(i).to_string(),
            dir: dir.to_string(),
            size,
            cloud,
            hardlink: !e.is_dir() && files == 0 && size == 0 && cloud == 0 && e.size > 0,
            files,
            modified: e.modified,
            is_dir: e.is_dir(),
            has_children: e.is_dir() && if full_here || self.agg.is_none() { visible_kids() } else { files > 0 || size > 0 || cloud > 0 },
            has_size: ix.has_sizes,
            matched: bit(&self.matched, i),
            vol: self.key.clone(),
            idx: i,
            epoch: self.epoch,
            capacity: 0,
            free: 0,
            before: 0,
            has_before: false,
            changed: false,
            free_before: 0,
        })
    }

    /// Folders of at least `min` bytes, parents before children, at most
    /// `cap` of them (the biggest: a size cut-off keeps the set ancestor-closed).
    fn snapshot_folders(&self, ix: &Index, min: u64, cap: usize) -> Vec<crate::history::SnapFolder> {
        use crate::history::SnapFolder;
        let root = ix.root;
        if (root as usize) >= ix.entries.len() {
            return Vec::new();
        }
        // Raise the cut-off if more than `cap` folders would qualify.
        let mut sizes: Vec<u64> = self.local.iter().zip(&self.cloud).map(|(l, c)| l + c).filter(|&s| s >= min).collect();
        let min = if sizes.len() > cap {
            let k = sizes.len() - cap;
            *sizes.select_nth_unstable(k).1
        } else {
            min
        };
        drop(sizes);
        let (l, f, c) = self.totals(root);
        let mut out = vec![SnapFolder { parent: u32::MAX, name: ix.name(root).trim_end_matches('\\').to_string(), size: l, cloud: c, files: f }];
        let mut stack = vec![(root, 0u32)];
        while let Some((d, at)) = stack.pop() {
            for &k in self.children(d) {
                if !ix.entries[k as usize].is_dir() {
                    continue;
                }
                let (l, f, c) = self.totals(k);
                if l + c >= min && out.len() < cap {
                    out.push(SnapFolder { parent: at, name: ix.name(k).to_string(), size: l, cloud: c, files: f });
                    stack.push((k, out.len() as u32 - 1));
                }
            }
        }
        out
    }

    /// Finds `rel` (components below the root) by walking child lists.
    fn resolve(&self, ix: &Index, rel: &str) -> Option<u32> {
        let mut cur = ix.root;
        for comp in rel.split(['\\', '/']).filter(|c| !c.is_empty()) {
            let f = fold_mode(comp, ix.fold_mode);
            cur = *self.children(cur).iter().find(|&&c| ix.fold_name(c) == f.as_bytes())?;
        }
        Some(cur)
    }
}

/// A folder-size tree over all volumes, optionally restricted to a query.
pub struct Tree {
    pub query: String,
    pub flags: SearchFlags,
    vols: Vec<VolTree>,
    built: Instant,
    /// Compare with this snapshot: rows get `before` and [`TreeSort::Change`] works.
    pub base: Option<Arc<crate::history::PathMap>>,
}

fn disk_space(root: &str) -> (u64, u64) {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut path = root.to_string();
    if !path.ends_with('\\') {
        path.push('\\');
    }
    let w = crate::ntfs::win::wide(&path);
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    if unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) } == 0 {
        return (0, 0);
    }
    (total, free)
}

impl Tree {
    /// Builds child lists and counts for every volume; with a non-empty
    /// query, also what it matches.
    pub fn build(eng: &Engine, query: &str, flags: SearchFlags) -> Result<Tree, String> {
        let filtered = !query.trim().is_empty();
        let rs = if filtered { Some(eng.query(query, flags, &SearchOptions { sort: SortKey::Name, descending: false, limit: 1 })?) } else { None };
        let vols = eng.with_volume_indexes(|vols| {
            vols.iter()
                .enumerate()
                .map(|(v, (key, kind, ix))| {
                    let m = rs.as_ref().and_then(|rs| rs.volume_hits(v, ix.epoch));
                    // A volume that changed shape since the search counts nothing.
                    let m = if filtered { Some(m.unwrap_or(&[])) } else { None };
                    VolTree::build(key, *kind, ix, m)
                })
                .collect()
        });
        Ok(Tree { query: query.to_string(), flags, vols, built: Instant::now(), base: None })
    }

    /// A snapshot of every volume's folder sizes (folders of at least
    /// `min` bytes, at most `cap` per volume), for [`crate::history`].
    pub fn snapshot(&mut self, eng: &Engine, name: &str, min: u64, cap: usize) -> Result<crate::history::Snapshot, String> {
        self.refresh(eng)?;
        let vols = &self.vols;
        let volumes = eng.with_volume_indexes(|ixs| {
            vols.iter()
                .zip(ixs)
                .map(|(t, (_, kind, ix))| {
                    let folders = t.snapshot_folders(ix, min, cap);
                    let (capacity, free) = if *kind != VolumeKind::Folder { folders.first().map_or((0, 0), |r| disk_space(&r.name)) } else { (0, 0) };
                    crate::history::SnapVolume { key: t.key.clone(), capacity, free, folders }
                })
                .collect()
        });
        Ok(crate::history::Snapshot { time: crate::history::now_unix(), name: name.to_string(), volumes })
    }

    /// Rebuilds if any index was compacted, or changed and the snapshot is old.
    fn refresh(&mut self, eng: &Engine) -> Result<(), String> {
        let stale = eng.with_volume_indexes(|vols| {
            vols.len() != self.vols.len()
                || vols.iter().zip(&self.vols).any(|((k, _, ix), t)| {
                    *k != t.key || ix.epoch != t.epoch || (ix.generation != t.generation && self.built.elapsed() > REFRESH_AFTER)
                })
        });
        if stale {
            *self = Tree::build(eng, &self.query.clone(), self.flags)?;
        }
        Ok(())
    }

    /// Children of `node` (the volume roots for an empty one), sorted and paged.
    pub fn children(&mut self, eng: &Engine, node: &NodeRef, sort: TreeSort, desc: bool, offset: usize, limit: usize) -> Result<TreeLevel, String> {
        self.refresh(eng)?;
        let vols = &self.vols;
        eng.with_volume_indexes(|ixs| {
            let mut level = TreeLevel::default();
            let mut rows: Vec<TreeRow> = Vec::new();
            if node.vol.is_empty() && node.path.trim_matches(['\\', '/']).is_empty() {
                for (t, (_, _, ix)) in vols.iter().zip(ixs) {
                    // No row for a volume without matches.
                    let Some(mut r) = t.row(ix, ix.root, "", t.full(ix, ix.root)) else { continue };
                    if t.kind != VolumeKind::Folder {
                        (r.capacity, r.free) = disk_space(&r.name);
                    }
                    rows.push(r);
                }
                level.size = rows.iter().map(|r| r.size).sum();
                level.cloud = rows.iter().map(|r| r.cloud).sum();
                level.files = rows.iter().map(|r| r.files).sum();
            } else {
                let (v, d) = Self::locate(vols, ixs, node).ok_or_else(|| format!("not indexed: {}", if node.path.is_empty() { &node.vol } else { &node.path }))?;
                let (t, ix) = (&vols[v], ixs[v].2);
                if !ix.entries[d as usize].is_dir() {
                    return Err(format!("not a folder: {}", ix.path(d)));
                }
                let full = t.full(ix, d);
                let (s, f, c) = t.value(ix, d, full).unwrap_or((0, 0, 0));
                (level.size, level.files, level.cloud) = (s, f, c);
                let dir = ix.path(d);
                rows = t.children(d).iter().filter_map(|&c| t.row(ix, c, &dir, full)).collect();
            }
            if let Some(base) = &self.base {
                Self::fill_before(base, &mut rows);
                let me = if node.vol.is_empty() && node.path.trim_matches(['\\', '/']).is_empty() {
                    Some(rows.iter().map(|r| r.before).sum())
                } else {
                    let p = rows.first().map(|r| r.dir.clone()).unwrap_or_else(|| node.path.clone());
                    base.get(&p).or(Some(0))
                };
                (level.before, level.has_before) = (me.unwrap_or(0), true);
            }
            let key = |a: &TreeRow, b: &TreeRow| match sort {
                TreeSort::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                TreeSort::Size => a.size.cmp(&b.size),
                TreeSort::Files => a.files.cmp(&b.files),
                TreeSort::Modified => a.modified.cmp(&b.modified),
                TreeSort::Change => a.change().cmp(&b.change()),
            };
            rows.sort_by(|a, b| {
                let o = key(a, b);
                (if desc { o.reverse() } else { o }).then_with(|| a.name.cmp(&b.name))
            });
            level.total = rows.len() as u64;
            level.rows = rows.into_iter().skip(offset).take(limit).collect();
            Ok(level)
        })
    }

    /// Sizes at the comparison snapshot (see [`TreeRow::before`]).
    fn fill_before(base: &crate::history::PathMap, rows: &mut [TreeRow]) {
        // FILETIME of the snapshot, to tell changed files apart.
        let base_ft = (base.time + 11_644_473_600) * 10_000_000;
        for r in rows {
            r.has_before = true;
            if r.is_dir {
                r.before = base.get(&r.full_path()).unwrap_or(0);
                if r.dir.is_empty()
                    && let Some((_, free)) = base.disk(&r.name)
                {
                    r.free_before = free;
                }
            } else if r.modified > base_ft {
                r.changed = true;
            } else {
                r.before = r.size;
            }
        }
    }

    fn locate(vols: &[VolTree], ixs: &[(&str, VolumeKind, &Index)], node: &NodeRef) -> Option<(usize, u32)> {
        if !node.vol.is_empty()
            && let Some(v) = vols.iter().position(|t| t.key == node.vol)
            && vols[v].epoch == node.epoch
            && (node.idx as usize) < ixs[v].2.entries.len()
            && ixs[v].2.is_visible(node.idx)
        {
            return Some((v, node.idx));
        }
        let path = node.path.replace('/', "\\");
        let path = path.trim_end_matches('\\');
        // Longest matching root wins (folder indexes inside drives).
        let mut best: Option<(usize, usize)> = None;
        for (v, (_, _, ix)) in ixs.iter().enumerate() {
            let root = ix.name(ix.root).trim_end_matches('\\');
            let ok = path.len() >= root.len()
                && path[..root.len()].eq_ignore_ascii_case(root)
                && (path.len() == root.len() || path.as_bytes()[root.len()] == b'\\');
            if ok && best.is_none_or(|(_, l)| root.len() > l) {
                best = Some((v, root.len()));
            }
        }
        let (v, rl) = best?;
        vols[v].resolve(ixs[v].2, &path[rl..]).map(|i| (v, i))
    }

    /// Bytes held by the tree's arrays (for diagnostics).
    pub fn memory_bytes(&self) -> usize {
        self.vols
            .iter()
            .map(|t| (t.start.len() + t.kids.len() + t.slot.len() + t.files.len()) * 4 + (t.local.len() + t.cloud.len() + t.matched.len()) * 8 + t.agg.as_ref().map_or(0, |a| a.len() * 40))
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::sample;

    fn names(l: &TreeLevel) -> Vec<(String, u64, u64)> {
        l.rows.iter().map(|r| (r.name.clone(), r.size, r.files)).collect()
    }

    #[test]
    fn sizes_counts_and_filters() {
        let e = Engine::from_indexes(vec![sample()]);
        let mut t = Tree::build(&e, "", SearchFlags::default()).unwrap();
        let top = t.children(&e, &NodeRef::default(), TreeSort::Size, true, 0, 100).unwrap();
        assert_eq!(top.rows.len(), 1);
        let c = &top.rows[0];
        assert_eq!((c.name.as_str(), c.files, c.has_children), ("C:", 3, true));
        let root = NodeRef { vol: c.vol.clone(), idx: c.idx, epoch: c.epoch, path: String::new() };
        let l = t.children(&e, &root, TreeSort::Size, true, 0, 100).unwrap();
        let sizes: Vec<_> = names(&l);
        // sample(): Users\Me\{notes.txt 24, Report.PDF 1000}, Windows\notepad.exe 200.
        assert_eq!(sizes, [("Users".to_string(), 1024, 2), ("Windows".to_string(), 200, 1)]);
        assert_eq!((l.size, l.files, l.total), (c.size, 3, 2));
        let by_path = t.children(&e, &NodeRef { path: r"c:\users\ME".into(), ..Default::default() }, TreeSort::Name, false, 0, 100).unwrap();
        assert_eq!(names(&by_path), [("notes.txt".to_string(), 24, 1), ("Report.PDF".to_string(), 1000, 1)]);
        let page = t.children(&e, &NodeRef { path: r"c:\users\me".into(), ..Default::default() }, TreeSort::Size, true, 1, 5).unwrap();
        assert_eq!((page.total, names(&page)), (2, vec![("notes.txt".to_string(), 24, 1)]));
        assert!(t.children(&e, &NodeRef { path: r"C:\nope".into(), ..Default::default() }, TreeSort::Name, false, 0, 9).is_err());

        // Filtered: only *.txt counts, folders without matches disappear.
        let mut f = Tree::build(&e, "*.txt", SearchFlags::default()).unwrap();
        let l = f.children(&e, &NodeRef { path: "C:".into(), ..Default::default() }, TreeSort::Size, true, 0, 100).unwrap();
        assert_eq!(names(&l), [("Users".to_string(), 24, 1)]);
        let top = f.children(&e, &NodeRef::default(), TreeSort::Size, true, 0, 100).unwrap();
        assert_eq!((top.rows[0].size, top.rows[0].files), (24, 1));
        // A matched folder counts fully, and matches inside it aren't counted twice.
        let mut g = Tree::build(&e, "users|notes", SearchFlags::default()).unwrap();
        let l = g.children(&e, &NodeRef { path: "C:".into(), ..Default::default() }, TreeSort::Size, true, 0, 100).unwrap();
        assert_eq!(names(&l), [("Users".to_string(), 1024, 2)]);
        assert!(l.rows[0].matched);
        let inside = g.children(&e, &NodeRef { path: r"C:\Users\Me".into(), ..Default::default() }, TreeSort::Size, true, 0, 100).unwrap();
        assert_eq!(inside.rows.len(), 2); // everything inside a matched folder is listed
        assert!(g.memory_bytes() > 0);
    }

    #[test]
    fn hard_links_count_once_and_cloud_files_apart() {
        use crate::index::tests::by_name;
        let mut ix = sample();
        // notepad.exe (record 35, 200 bytes) also linked from Users\Me; an
        // online-only 5000-byte OneDrive file next to it.
        let me = by_name(&ix, "Me");
        ix.add_link(35, 0, me, "notepad-link.exe", 0x20, 200, 0, 0);
        ix.add_link(80, 0, me, "cloud.mp4", 0x0040_1620, 5000, 0, 0);
        let e = Engine::from_indexes(vec![ix]);
        let mut t = Tree::build(&e, "", SearchFlags::default()).unwrap();
        let top = t.children(&e, &NodeRef { path: "C:".into(), ..Default::default() }, TreeSort::Name, false, 0, 100).unwrap();
        // 1024 + 200 bytes on disk, the link counted once, the cloud file apart.
        assert_eq!((top.size, top.cloud, top.files), (1224, 5000, 4)); // the cloud file is still a file
        let sum: u64 = top.rows.iter().map(|r| r.size).sum();
        assert_eq!(sum, 1224);
        let l = t.children(&e, &NodeRef { path: r"C:\Users\Me".into(), ..Default::default() }, TreeSort::Name, false, 0, 100).unwrap();
        let get = |n: &str| l.rows.iter().find(|r| r.name == n).unwrap().clone();
        assert_eq!((get("cloud.mp4").size, get("cloud.mp4").cloud), (0, 5000));
        let (a, b) = (get("notepad-link.exe"), t.children(&e, &NodeRef { path: r"C:\Windows".into(), ..Default::default() }, TreeSort::Name, false, 0, 9).unwrap().rows[0].clone());
        assert!(a.hardlink != b.hardlink, "exactly one link counts: {a:?} {b:?}");
        assert_eq!(a.size + b.size, 200);
        // Filtered: the cloud file shows up as cloud bytes only.
        let mut f = Tree::build(&e, "*.mp4", SearchFlags::default()).unwrap();
        let roots = f.children(&e, &NodeRef::default(), TreeSort::Size, true, 0, 9).unwrap();
        assert_eq!((roots.size, roots.cloud, roots.files), (0, 5000, 1));
        assert_eq!(roots.rows.len(), 1);
    }
}
