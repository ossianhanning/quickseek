//! In-memory index of one volume (or one indexed folder tree).
//!
//! Layout is chosen for the search loop, not for convenience:
//! * every folded (lowercased) name lives in one contiguous byte arena, each
//!   terminated by `\0`, so a substring query is a single SIMD `memmem` pass
//!   over a few tens of MB instead of millions of small string compares;
//! * entries are fixed-size POD structs with parent *indices* (not paths), so
//!   paths are only built for the results actually shown;
//! * `rec_map` maps an MFT record number straight to its entry, which is what
//!   makes applying USN journal records O(1) without asking the OS for paths;
//! * per-key sort ranks (Everything's "fast sort") turn sorting millions of
//!   results into integer compares.

use std::collections::HashMap;
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};
use parking_lot::Mutex;
use rayon::prelude::*;

use crate::config::{InclusionState, Rules};
use crate::fold::{FoldMode, fold_into_mode};
use crate::ntfs::{self, Scan};

pub const NONE: u32 = u32::MAX;

pub const F_DIR: u16 = 1;
pub const F_DELETED: u16 = 2;
/// Hidden by the exclude rules (still indexed, so rule changes are instant).
pub const F_EXCLUDED: u16 = 4;
/// Folder that is only visible because an include-only folder lies below it.
pub const F_INCLUDE_ANCESTOR: u16 = 8;
/// The name equals its folded form, so it's stored once (in the folded arena).
pub const F_NAME_IN_FOLD: u16 = 16;

/// File attributes kept in the spare bits of `flags` (directory = `F_DIR`):
/// R H S A T P(sparse) L(reparse) C O I(not content indexed) E.
const ATTR_BITS: [(u32, u16); 11] = [
    (0x1, 1 << 5),
    (0x2, 1 << 6),
    (0x4, 1 << 7),
    (0x20, 1 << 8),
    (0x100, 1 << 9),
    (0x200, 1 << 10),
    (0x400, 1 << 11),
    (0x800, 1 << 12),
    (0x1000, 1 << 13),
    (0x2000, 1 << 14),
    (0x4000, 1 << 15),
];
const ATTR_MASK: u16 = 0xFFE0;

pub fn pack_attrs(a: u32) -> u16 {
    ATTR_BITS.iter().filter(|(w, _)| a & w != 0).fold(0, |f, (_, b)| f | b)
}

/// 40 bytes per file. Optional data (date created) and rare data (extra
/// hard links) live in side tables so they cost nothing when unused.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct Entry {
    /// File size, or total size of everything below a directory.
    pub size: u64,
    pub modified: i64,
    pub record: u32,
    /// Entry index of the parent directory (`NONE` for the root).
    pub parent: u32,
    /// Offset in `names` (unused when `F_NAME_IN_FOLD`).
    pub name_off: u32,
    /// Fold record: index into `fold_starts`.
    pub fold_rec: u32,
    pub name_len: u16,
    pub flags: u16,
    pub seq: u16,
    pub _pad: u16,
}

impl Entry {
    /// Win32 file attributes.
    pub fn attrs(&self) -> u32 {
        let mut a = ATTR_BITS.iter().filter(|(_, b)| self.flags & b != 0).fold(0, |a, (w, _)| a | w);
        if self.is_dir() {
            a |= ntfs::usn::FILE_ATTRIBUTE_DIRECTORY;
        }
        a
    }
    pub fn set_attrs(&mut self, a: u32) {
        self.flags = (self.flags & !ATTR_MASK) | pack_attrs(a);
    }
    pub fn is_dir(&self) -> bool {
        self.flags & F_DIR != 0
    }
    pub fn is_deleted(&self) -> bool {
        self.flags & F_DELETED != 0
    }
    pub fn is_visible(&self) -> bool {
        self.flags & (F_DELETED | F_EXCLUDED) == 0
    }
    pub fn is_live(&self) -> bool {
        !self.is_deleted()
    }
}

/// Sort keys with a precomputed rank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankKey {
    Name = 0,
    Size = 1,
    Modified = 2,
    Path = 3,
}

/// What an index stores beyond the basics (Everything's "Indexes" options).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexOptions {
    /// Keep date created (8 bytes per file; needed for `dc:` and that sort).
    pub created: bool,
    /// Keep a precomputed rank per sort key (4 bytes per file each). Name
    /// is always ranked (other ranks tie-break on it).
    pub fast_sort: [bool; 4],
}

impl Default for IndexOptions {
    fn default() -> Self {
        IndexOptions { created: true, fast_sort: [true; 4] }
    }
}

/// Position of each entry in each sort order. `NONE` = changed since the
/// ranks were computed; comparisons fall back to the real key for those.
#[derive(Default)]
pub struct Ranks {
    pub by: [Vec<u32>; 4],
    /// A folder was renamed/moved: path ranks of its subtree are unknown.
    pub path_stale: bool,
    /// Ranks invalidated since the last computation.
    pub dirty: usize,
}

impl Ranks {
    #[inline]
    pub fn get(&self, key: RankKey, i: u32) -> u32 {
        if key == RankKey::Path && self.path_stale {
            return NONE;
        }
        self.by[key as usize].get(i as usize).copied().unwrap_or(NONE)
    }
}

#[derive(Default)]
pub struct Index {
    pub letter: char,
    pub entries: Vec<Entry>,
    /// Original UTF-8 names.
    pub names: Vec<u8>,
    /// Folded names, `\0`-terminated, in `fold_starts` order.
    pub fold: Vec<u8>,
    pub fold_starts: Vec<u32>,
    /// Fold records below `fold_base` belong to the entry with the same
    /// index unless marked dead; later ones (renames, additions) are owned
    /// per `fold_extra`. Saves a 4-byte owner table for the common case.
    pub fold_base: u32,
    pub fold_dead: Vec<u64>,
    pub fold_extra: Vec<u32>,
    pub fold_mode: FoldMode,
    pub opts: IndexOptions,
    /// Date created per entry (empty unless `opts.created`).
    pub created: Vec<i64>,
    /// Entry → next entry of the same MFT record (extra hard links only).
    pub more_links: HashMap<u32, u32>,
    /// MFT record → first entry of that record.
    pub rec_map: Vec<u32>,
    /// For volumes with 128-bit file ids (ReFS) or synthetic ones (folder
    /// indexes): file id → dense record number used everywhere else.
    pub id_map: Option<HashMap<u128, u32>>,
    pub ranks: Ranks,
    /// Entry → time (FILETIME) of the last change seen in the journal.
    pub recent: HashMap<u32, i64>,
    pub rules: Rules,
    /// Bitset mirror of F_EXCLUDED: a cache-friendly check for hot loops.
    pub excl: Vec<u64>,
    /// A folder moved under exclude rules: flags need recomputing.
    pub rules_stale: bool,
    pub root: u32,
    pub has_sizes: bool,
    pub garbage: usize,
    /// Bumped on every mutation, so UIs know when to re-run a query.
    pub generation: u64,
    /// Bumped when entries are renumbered (compaction); result sets holding
    /// entry indices from an older epoch are invalid.
    pub epoch: u64,
    pub journal_id: u64,
    pub next_usn: i64,
    pub volume_serial: u32,
    pub(crate) child_count_cache: Mutex<Option<(u64, Arc<Vec<u32>>)>>,
}

impl Index {
    pub fn from_scan(letter: char, scan: &Scan) -> Index {
        Self::from_scan_named(letter, &format!("{letter}:"), scan, FoldMode::Lower)
    }

    /// `root_name` is how the root shows in paths (`C:` or `G:\Photos`).
    pub fn from_scan_named(letter: char, root_name: &str, scan: &Scan, fold_mode: FoldMode) -> Index {
        Self::from_scan_with(letter, root_name, scan, fold_mode, IndexOptions::default())
    }

    /// `root_name` is how the root shows in paths (`C:` or `G:` plus a folder).
    pub fn from_scan_with(letter: char, root_name: &str, scan: &Scan, fold_mode: FoldMode, opts: IndexOptions) -> Index {
        let mut ix = Index { letter, has_sizes: scan.has_sizes, root: NONE, fold_mode, opts, ..Default::default() };
        if opts.created {
            ix.created.reserve(scan.names.len() + 1);
        }
        ix.rec_map = vec![NONE; scan.meta.len()];
        ix.entries.reserve(scan.names.len() + 1);
        ix.names.reserve(scan.strings.len() + 4);

        let mut parent_rec: Vec<u32> = Vec::with_capacity(scan.names.len() + 1);
        for n in &scan.names {
            let Some(m) = scan.meta.get(n.record as usize) else { continue };
            if !m.in_use || ntfs::is_hidden_metafile(n.record, n.parent) {
                continue;
            }
            let is_root = n.record == ntfs::ROOT_RECORD;
            if is_root && ix.root != NONE {
                continue;
            }
            let name = if is_root { root_name } else { scan.name(n) };
            let idx = ix.entries.len() as u32;
            ix.entries.push(Entry {
                size: if m.is_dir { 0 } else { m.size },
                modified: m.modified,
                record: n.record,
                parent: NONE,
                name_off: ix.names.len() as u32,
                fold_rec: idx,
                name_len: name.len().min(u16::MAX as usize) as u16,
                flags: (if m.is_dir { F_DIR } else { 0 }) | pack_attrs(m.attrs),
                seq: m.seq,
                _pad: 0,
            });
            if opts.created {
                ix.created.push(m.created);
            }
            ix.names.extend_from_slice(&name.as_bytes()[..name.len().min(u16::MAX as usize)]);
            let prev = ix.rec_map[n.record as usize];
            if prev != NONE {
                ix.more_links.insert(idx, prev);
            }
            ix.rec_map[n.record as usize] = idx;
            parent_rec.push(if is_root { NONE } else { n.parent });
            if is_root {
                ix.root = idx;
            }
        }
        if ix.root == NONE {
            // Volume without a visible root (shouldn't happen): synthesize one.
            let idx = ix.entries.len() as u32;
            ix.entries.push(Entry {
                size: 0, modified: 0, record: ntfs::ROOT_RECORD, parent: NONE,
                name_off: ix.names.len() as u32, fold_rec: idx,
                name_len: root_name.len() as u16, flags: F_DIR, seq: 0, _pad: 0,
            });
            if opts.created {
                ix.created.push(0);
            }
            ix.names.extend_from_slice(root_name.as_bytes());
            if ix.rec_map.len() <= ntfs::ROOT_RECORD as usize {
                ix.rec_map.resize(ntfs::ROOT_RECORD as usize + 1, NONE);
            }
            ix.rec_map[ntfs::ROOT_RECORD as usize] = idx;
            parent_rec.push(NONE);
            ix.root = idx;
        }

        // Resolve parent records to entry indices; drop orphans.
        let rec_map = &ix.rec_map;
        let mut orphans = 0usize;
        for (e, &pr) in ix.entries.iter_mut().zip(&parent_rec) {
            if pr == NONE {
                continue;
            }
            match rec_map.get(pr as usize).copied() {
                Some(p) if p != NONE => e.parent = p,
                _ => {
                    e.flags |= F_DELETED;
                    orphans += 1;
                }
            }
        }
        ix.garbage = orphans;

        ix.rebuild_fold();
        if ix.has_sizes {
            ix.compute_dir_sizes();
        }
        ix.compute_ranks();
        ix.shrink();
        ix
    }

    pub fn shrink(&mut self) {
        self.entries.shrink_to_fit();
        self.names.shrink_to_fit();
        self.fold.shrink_to_fit();
        self.fold_starts.shrink_to_fit();
        self.fold_extra.shrink_to_fit();
        self.fold_dead.shrink_to_fit();
        self.created.shrink_to_fit();
        self.rec_map.shrink_to_fit();
        for r in &mut self.ranks.by {
            r.shrink_to_fit();
        }
    }

    // ---- folding -------------------------------------------------------------

    /// Rebuilds both name arenas from the current names (in parallel): one
    /// fold record per entry in entry order, and the original name stored
    /// separately only when it differs from the folded one.
    pub fn rebuild_fold(&mut self) {
        const CHUNK: usize = 1 << 15;
        let mode = self.fold_mode;
        let me = &*self;
        // Per chunk: folded bytes, fold starts, differing names, per-entry name offset (NONE = in fold).
        type Part = (Vec<u8>, Vec<u32>, Vec<u8>, Vec<u32>);
        let parts: Vec<Part> = (0..me.entries.len().div_ceil(CHUNK))
            .into_par_iter()
            .map(|c| {
                let range = c * CHUNK..((c + 1) * CHUNK).min(me.entries.len());
                let mut fold = Vec::with_capacity(range.len() * 16);
                let mut starts = Vec::with_capacity(range.len());
                let mut names = Vec::new();
                let mut offs = Vec::with_capacity(range.len());
                for i in range {
                    let s = me.name(i as u32);
                    let at = fold.len();
                    starts.push(at as u32);
                    fold_into_mode(s, &mut fold, mode);
                    if &fold[at..] == s.as_bytes() {
                        offs.push(NONE);
                    } else {
                        offs.push(names.len() as u32);
                        names.extend_from_slice(s.as_bytes());
                    }
                    fold.push(0);
                }
                (fold, starts, names, offs)
            })
            .collect();
        let (mut fold, mut fold_starts, mut names) = (Vec::new(), Vec::with_capacity(self.entries.len()), Vec::new());
        fold.reserve(parts.iter().map(|p| p.0.len()).sum());
        names.reserve(parts.iter().map(|p| p.2.len()).sum());
        let mut i = 0usize;
        for (f, st, nm, offs) in parts {
            let (fb, nb) = (fold.len() as u32, names.len() as u32);
            fold_starts.extend(st.into_iter().map(|s| s + fb));
            fold.extend_from_slice(&f);
            names.extend_from_slice(&nm);
            for o in offs {
                let e = &mut self.entries[i];
                e.fold_rec = i as u32;
                if o == NONE {
                    e.flags |= F_NAME_IN_FOLD;
                    e.name_off = 0;
                } else {
                    e.flags &= !F_NAME_IN_FOLD;
                    e.name_off = o + nb;
                }
                i += 1;
            }
        }
        self.fold = fold;
        self.fold_starts = fold_starts;
        self.names = names;
        self.fold_base = self.entries.len() as u32;
        self.fold_extra = Vec::new();
        let mut dead = vec![0u64; self.entries.len().div_ceil(64)];
        for (i, e) in self.entries.iter().enumerate() {
            if e.is_deleted() {
                dead[i / 64] |= 1 << (i % 64);
            }
        }
        self.fold_dead = dead;
    }

    /// Entry owning fold record `rec` (`NONE` if dead).
    #[inline]
    pub fn fold_owner(&self, rec: usize) -> u32 {
        if rec < self.fold_base as usize {
            if self.fold_dead.get(rec / 64).is_some_and(|w| w >> (rec % 64) & 1 == 1) { NONE } else { rec as u32 }
        } else {
            self.fold_extra.get(rec - self.fold_base as usize).copied().unwrap_or(NONE)
        }
    }

    fn kill_fold_rec(&mut self, rec: usize) {
        if rec < self.fold_base as usize {
            if let Some(w) = self.fold_dead.get_mut(rec / 64) {
                *w |= 1 << (rec % 64);
            }
        } else if let Some(o) = self.fold_extra.get_mut(rec - self.fold_base as usize) {
            *o = NONE;
        }
    }

    /// Changes what's stored. Turning date created on needs the data from a
    /// rescan (the caller does that); turning things off frees memory now.
    pub fn set_options(&mut self, opts: IndexOptions) {
        if !opts.created {
            self.created = Vec::new();
        }
        for k in 1..4 {
            if !opts.fast_sort[k] {
                self.ranks.by[k] = Vec::new();
            }
        }
        self.opts = opts;
        self.generation += 1;
    }

    pub fn created(&self, i: u32) -> Option<i64> {
        self.created.get(i as usize).copied()
    }

    /// Switches diacritic handling; refolds names and recomputes ranks.
    pub fn set_fold_mode(&mut self, mode: FoldMode) {
        if mode != self.fold_mode {
            self.fold_mode = mode;
            self.rebuild_fold();
            self.compute_ranks();
            self.generation += 1;
        }
    }

    // ---- ranks ---------------------------------------------------------------

    /// Recomputes every sort rank.
    pub fn compute_ranks(&mut self) {
        self.ranks = self.ranks_snapshot();
    }

    /// Computes every sort rank without modifying the index (so it can run
    /// under a read lock). Name first; the others tie-break on it.
    pub fn ranks_snapshot(&self) -> Ranks {
        let n = self.entries.len();
        let live: Vec<u32> = (0..n as u32).into_par_iter().filter(|&i| self.is_live(i)).collect();
        let to_rank = |order: &[u32]| {
            let mut rank = vec![NONE; n];
            for (r, &i) in order.iter().enumerate() {
                rank[i as usize] = r as u32;
            }
            rank
        };

        let keys: Vec<&[u8]> = (0..n as u32).into_par_iter().map(|i| self.fold_name(i)).collect();
        let mut order = live.clone();
        order.par_sort_unstable_by(|&a, &b| {
            keys[a as usize].cmp(keys[b as usize]).then_with(|| self.name(a).cmp(self.name(b))).then(a.cmp(&b))
        });
        drop(keys);
        let name = to_rank(&order);

        let on = self.opts.fast_sort;
        let (size, (modified, path)) = rayon::join(
            || {
                if !on[RankKey::Size as usize] {
                    return Vec::new();
                }
                let mut o = live.clone();
                o.par_sort_unstable_by_key(|&i| (self.entries[i as usize].size, name[i as usize]));
                to_rank(&o)
            },
            || {
                rayon::join(
                    || {
                        if !on[RankKey::Modified as usize] {
                            return Vec::new();
                        }
                        let mut o = live.clone();
                        o.par_sort_unstable_by_key(|&i| (self.entries[i as usize].modified, name[i as usize]));
                        to_rank(&o)
                    },
                    || if on[RankKey::Path as usize] { self.path_rank(&live, &name) } else { Vec::new() },
                )
            },
        );
        Ranks { by: [name, size, modified, path], path_stale: false, dirty: 0 }
    }

    /// Path order = (containing folder in depth-first order with siblings by
    /// name, then name). Comparing full paths component-wise gives exactly
    /// this order, which is what the fallback comparison does.
    fn path_rank(&self, live: &[u32], name: &[u32]) -> Vec<u32> {
        let n = self.entries.len();
        // Directory children, sorted by name rank.
        let mut dirs: Vec<(u32, u32, u32)> = live
            .iter()
            .filter(|&&i| self.entries[i as usize].is_dir() && self.entries[i as usize].parent != NONE)
            .map(|&i| (self.entries[i as usize].parent, name[i as usize], i))
            .collect();
        dirs.par_sort_unstable();
        let mut first_child: HashMap<u32, usize> = HashMap::new();
        for (k, d) in dirs.iter().enumerate() {
            first_child.entry(d.0).or_insert(k);
        }
        let mut dfs = vec![u32::MAX; n];
        let mut next = 0u32;
        let mut stack = vec![self.root];
        while let Some(d) = stack.pop() {
            if d == NONE || dfs[d as usize] != u32::MAX {
                continue;
            }
            dfs[d as usize] = next;
            next += 1;
            if let Some(&start) = first_child.get(&d) {
                let end = dirs[start..].iter().position(|x| x.0 != d).map_or(dirs.len(), |p| start + p);
                for c in dirs[start..end].iter().rev() {
                    stack.push(c.2);
                }
            }
        }
        let mut o = live.to_vec();
        o.par_sort_unstable_by_key(|&i| {
            let p = self.entries[i as usize].parent;
            let pk = if p == NONE { 0u64 } else { dfs[p as usize] as u64 + 1 };
            (pk, name[i as usize])
        });
        let mut rank = vec![NONE; n];
        for (r, &i) in o.iter().enumerate() {
            rank[i as usize] = r as u32;
        }
        rank
    }

    fn unrank(&mut self, i: u32, keys: &[RankKey]) {
        for &k in keys {
            if let Some(r) = self.ranks.by[k as usize].get_mut(i as usize)
                && *r != NONE
            {
                *r = NONE;
                self.ranks.dirty += 1;
            }
        }
    }

    /// Whether enough ranks went stale that recomputing is worthwhile.
    pub fn ranks_need_refresh(&self) -> bool {
        self.ranks_missing() || self.ranks.path_stale || self.ranks.dirty * 200 > self.entries.len().max(1)
    }

    /// Ranks haven't been computed for the current entries (e.g. just loaded).
    pub fn ranks_missing(&self) -> bool {
        self.ranks.by[0].len() != self.entries.len()
    }

    // ---- basic accessors -------------------------------------------------------

    /// Rolls file sizes up into every ancestor directory.
    fn compute_dir_sizes(&mut self) {
        let depth = self.depths();
        let mut order: Vec<u32> = (0..self.entries.len() as u32).collect();
        order.par_sort_unstable_by_key(|&i| std::cmp::Reverse(depth[i as usize]));
        for &i in &order {
            let e = self.entries[i as usize];
            if e.is_deleted() || e.parent == NONE {
                continue;
            }
            self.entries[e.parent as usize].size += e.size;
        }
    }

    /// Depth of every entry (root = 0).
    pub fn depths(&self) -> Vec<u16> {
        let n = self.entries.len();
        let mut depth = vec![u16::MAX; n];
        for i in 0..n {
            if depth[i] != u16::MAX {
                continue;
            }
            let mut chain = Vec::new();
            let mut cur = i as u32;
            let base = loop {
                if cur == NONE {
                    break 0u16;
                }
                if depth[cur as usize] != u16::MAX {
                    break depth[cur as usize] + 1;
                }
                if chain.len() > 4096 {
                    break 0; // cycle guard
                }
                chain.push(cur);
                cur = self.entries[cur as usize].parent;
            };
            for (k, &c) in chain.iter().rev().enumerate() {
                depth[c as usize] = base.saturating_add(k as u16);
            }
        }
        depth
    }

    pub fn depth(&self, i: u32) -> usize {
        self.ancestors(i).len().saturating_sub(1)
    }

    pub fn len_live(&self) -> usize {
        self.entries.len() - self.garbage
    }

    pub fn len_visible(&self) -> usize {
        self.entries.par_iter().filter(|e| e.is_visible()).count()
    }

    #[inline]
    pub fn name(&self, i: u32) -> &str {
        let e = &self.entries[i as usize];
        let bytes = if e.flags & F_NAME_IN_FOLD != 0 {
            let s = self.fold_starts[e.fold_rec as usize] as usize;
            &self.fold[s..s + e.name_len as usize]
        } else {
            &self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize]
        };
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }

    #[inline]
    pub fn fold_name(&self, i: u32) -> &[u8] {
        let r = self.entries[i as usize].fold_rec as usize;
        let start = self.fold_starts[r] as usize;
        let end = memchr::memchr(0, &self.fold[start..]).map_or(self.fold.len(), |p| start + p);
        &self.fold[start..end]
    }

    /// `i` and its ancestors, nearest first.
    pub fn ancestors(&self, i: u32) -> Vec<u32> {
        let mut chain = Vec::with_capacity(16);
        let mut cur = i;
        while cur != NONE && chain.len() < 1024 {
            chain.push(cur);
            cur = self.entries[cur as usize].parent;
        }
        chain
    }

    /// Full path, e.g. `C:\Users\me\file.txt`.
    pub fn path(&self, i: u32) -> String {
        let chain = self.ancestors(i);
        let mut s = String::with_capacity(chain.len() * 12);
        for (k, &c) in chain.iter().rev().enumerate() {
            if k > 0 {
                s.push('\\');
            }
            s.push_str(self.name(c));
        }
        if chain.len() == 1 && !s.ends_with('\\') {
            s.push('\\');
        }
        s
    }

    /// Path of the containing folder.
    pub fn parent_path(&self, i: u32) -> String {
        match self.entries[i as usize].parent {
            NONE => String::new(),
            p => self.path(p),
        }
    }

    /// Folded full path into `out`.
    pub fn fold_path_into(&self, i: u32, out: &mut Vec<u8>) {
        let chain = self.ancestors(i);
        for (k, &c) in chain.iter().rev().enumerate() {
            if k > 0 {
                out.push(b'\\');
            }
            out.extend_from_slice(self.fold_name(c));
        }
    }

    pub fn fold_path(&self, i: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity(96);
        self.fold_path_into(i, &mut v);
        v
    }

    pub fn is_live(&self, i: u32) -> bool {
        !self.entries[i as usize].is_deleted()
    }

    /// Live and not hidden by exclude rules: what searches see.
    #[inline]
    pub fn is_visible(&self, i: u32) -> bool {
        self.entries[i as usize].is_visible()
    }

    /// Excluded by rules, without touching the entry (bitset lookup).
    #[inline]
    pub fn is_excluded_fast(&self, i: u32) -> bool {
        self.excl.get(i as usize / 64).is_some_and(|w| w >> (i % 64) & 1 == 1)
    }

    fn set_excl(&mut self, i: u32, on: bool) {
        let w = i as usize / 64;
        if on {
            if self.excl.len() <= w {
                self.excl.resize(w + 1, 0);
            }
            self.excl[w] |= 1 << (i % 64);
        } else if let Some(x) = self.excl.get_mut(w) {
            *x &= !(1 << (i % 64));
        }
    }

    /// All live entries whose folded name contains `needle` (SIMD scan over
    /// the arena). Parallel over `\0`-aligned slices.
    pub fn name_candidates(&self, needle: &[u8]) -> Vec<u32> {
        const MIN_SLICE: usize = 1 << 20;
        let hay = &self.fold;
        if hay.is_empty() || needle.is_empty() {
            return Vec::new();
        }
        let parts = (hay.len() / MIN_SLICE).clamp(1, rayon::current_num_threads() * 4);
        let mut bounds = vec![0usize];
        for k in 1..parts {
            let target = hay.len() * k / parts;
            let b = memchr::memchr(0, &hay[target..]).map_or(hay.len(), |p| target + p + 1);
            if b > *bounds.last().unwrap() && b < hay.len() {
                bounds.push(b);
            }
        }
        bounds.push(hay.len());
        let finder = memchr::memmem::Finder::new(needle);
        bounds
            .par_windows(2)
            .flat_map_iter(|w| {
                let (start, end) = (w[0], w[1]);
                let mut out = Vec::new();
                let mut pos = start;
                let starts = &self.fold_starts;
                let mut rec = starts.partition_point(|&s| (s as usize) <= start).saturating_sub(1);
                while pos < end {
                    let Some(off) = finder.find(&hay[pos..end]) else { break };
                    let at = pos + off;
                    // Gallop forward to the fold record containing `at`.
                    let (mut lo, mut step) = (rec, 1usize);
                    while lo + step < starts.len() && starts[lo + step] as usize <= at {
                        lo += step;
                        step *= 2;
                    }
                    let hi = (lo + step + 1).min(starts.len());
                    rec = lo + starts[lo..hi].partition_point(|&s| s as usize <= at) - 1;
                    let owner = self.fold_owner(rec);
                    if owner != NONE {
                        out.push(owner);
                    }
                    pos = starts.get(rec + 1).map_or(end, |&s| s as usize);
                }
                out
            })
            .collect()
    }

    /// Folders whose folded full path equals `path` (no trailing `\`).
    pub fn dirs_with_path(&self, path: &[u8]) -> Vec<u32> {
        let root_fold = self.fold_name(self.root);
        if path == root_fold {
            return vec![self.root];
        }
        let last = path.rsplit(|&b| b == b'\\').next().unwrap_or(path);
        self.name_candidates(last)
            .into_iter()
            .filter(|&i| self.entries[i as usize].is_dir() && self.fold_name(i) == last && self.fold_path(i) == path)
            .collect()
    }

    /// Live child of `parent` with exactly this (unfolded) name.
    pub fn find_child(&self, parent: u32, name: &str) -> Option<u32> {
        let mut f = Vec::new();
        fold_into_mode(name, &mut f, self.fold_mode);
        self.name_candidates(&f)
            .into_iter()
            .find(|&i| self.entries[i as usize].parent == parent && self.name(i) == name)
    }

    /// Number of visible children per entry (cached per generation).
    pub fn child_counts(&self) -> Arc<Vec<u32>> {
        let mut cache = self.child_count_cache.lock();
        if let Some((g, c)) = &*cache
            && *g == self.generation
        {
            return c.clone();
        }
        let mut counts = vec![0u32; self.entries.len()];
        for e in &self.entries {
            if e.is_visible() && e.parent != NONE {
                counts[e.parent as usize] += 1;
            }
        }
        let c = Arc::new(counts);
        *cache = Some((self.generation, c.clone()));
        c
    }

    // ---- exclude rules -----------------------------------------------------------

    /// Recomputes F_EXCLUDED / F_INCLUDE_ANCESTOR for every entry, parents
    /// before children (one parallel pass per depth level).
    pub fn apply_rules(&mut self, rules: Rules) {
        self.rules = rules;
        self.rules_stale = false;
        let n = self.entries.len();
        for e in &mut self.entries {
            e.flags &= !(F_EXCLUDED | F_INCLUDE_ANCESTOR);
        }
        if self.rules.is_empty() {
            self.excl.clear();
            self.generation += 1;
            return;
        }
        let depth = self.depths();
        let max_d = depth.iter().filter(|&&d| d != u16::MAX).max().copied().unwrap_or(0) as usize;
        let mut levels: Vec<Vec<u32>> = vec![Vec::new(); max_d + 1];
        for (i, &d) in depth.iter().enumerate() {
            if d != u16::MAX && self.entries[i].is_live() {
                levels[d as usize].push(i as u32);
            }
        }
        let mut flags = vec![0u16; n];
        for level in &levels {
            let computed: Vec<(u32, u16)> =
                level.par_iter().map(|&i| (i, self.rule_flags(i, |p| flags[p as usize]))).collect();
            for (i, f) in computed {
                flags[i as usize] = f;
            }
        }
        for (e, f) in self.entries.iter_mut().zip(flags) {
            e.flags |= f;
        }
        self.rebuild_excl();
        self.generation += 1;
    }

    fn rebuild_excl(&mut self) {
        let mut v = vec![0u64; self.entries.len().div_ceil(64)];
        for (i, e) in self.entries.iter().enumerate() {
            if e.flags & F_EXCLUDED != 0 {
                v[i / 64] |= 1 << (i % 64);
            }
        }
        self.excl = v;
    }

    /// Exclusion flags for `i` given its parent's flags.
    fn rule_flags(&self, i: u32, parent_flags: impl Fn(u32) -> u16) -> u16 {
        let e = &self.entries[i as usize];
        let r = &self.rules;
        let pf = if e.parent == NONE { 0 } else { parent_flags(e.parent) };
        if pf & F_EXCLUDED != 0 {
            return F_EXCLUDED;
        }
        let mut own = 0;
        if !r.include_only.is_empty() && (e.parent == NONE || pf & F_INCLUDE_ANCESTOR != 0) {
            match r.include_only_allows(&self.fold_path(i)) {
                InclusionState::Inside => {}
                InclusionState::Ancestor if e.is_dir() => own = F_INCLUDE_ANCESTOR,
                _ => return F_EXCLUDED,
            }
        }
        if e.parent == NONE {
            return own; // the root itself is never excluded by name rules
        }
        if r.excludes(e.is_dir(), e.attrs(), self.fold_name(i), &mut || self.fold_path(i)) {
            return F_EXCLUDED;
        }
        own
    }

    fn apply_rules_to_new(&mut self, i: u32) {
        if self.rules.is_empty() {
            return;
        }
        let f = self.rule_flags(i, |p| self.entries[p as usize].flags);
        self.entries[i as usize].flags |= f;
        self.set_excl(i, f & F_EXCLUDED != 0);
    }

    // ---- id mapping (ReFS, folder indexes) -----------------------------------------

    /// Dense record for a 128-bit file id, allocating one when `create`.
    pub fn map_id(&mut self, id: u128, create: bool) -> Option<u32> {
        let next = (self.rec_map.len() as u32).max(ntfs::FIRST_USER_RECORD);
        let map = self.id_map.get_or_insert_with(HashMap::new);
        if let Some(&r) = map.get(&id) {
            return Some(r);
        }
        if !create {
            return None;
        }
        map.insert(id, next);
        self.rec_map.resize(next as usize + 1, NONE);
        Some(next)
    }

    // ---- recent changes --------------------------------------------------------

    pub fn note_recent(&mut self, i: u32, when: i64) {
        const MAX: usize = 200_000;
        self.recent.insert(i, when);
        if self.recent.len() > MAX {
            let mut v: Vec<(u32, i64)> = self.recent.drain().collect();
            v.sort_unstable_by_key(|&(_, t)| std::cmp::Reverse(t));
            v.truncate(MAX / 2);
            self.recent = v.into_iter().collect();
        }
    }

    // ---- mutation (used by journal replay) ---------------------------------

    pub fn links(&self, record: u32) -> impl Iterator<Item = u32> + '_ {
        let mut cur = self.rec_map.get(record as usize).copied().unwrap_or(NONE);
        std::iter::from_fn(move || {
            if cur == NONE {
                return None;
            }
            let c = cur;
            cur = self.more_links.get(&c).copied().unwrap_or(NONE);
            Some(c)
        })
    }

    pub fn dir_entry(&self, record: u32) -> Option<u32> {
        self.links(record).find(|&i| self.entries[i as usize].is_dir()).or_else(|| self.links(record).next())
    }

    pub fn find_link(&self, record: u32, parent: u32, name: &str) -> Option<u32> {
        self.links(record).find(|&i| self.entries[i as usize].parent == parent && self.name(i) == name)
    }

    fn add_size_up(&mut self, mut p: u32, delta: i64) {
        if !self.has_sizes || delta == 0 {
            return;
        }
        let mut guard = 0;
        while p != NONE && guard < 1024 {
            let e = &mut self.entries[p as usize];
            e.size = (e.size as i64 + delta).max(0) as u64;
            let next = e.parent;
            self.unrank(p, &[RankKey::Size]);
            p = next;
            guard += 1;
        }
    }

    /// Grows by a few percent instead of doubling, so one insert into a
    /// shrunk multi-hundred-MB vector doesn't double its memory.
    fn headroom<T>(v: &mut Vec<T>, extra: usize) {
        if v.capacity() - v.len() < extra {
            v.reserve_exact(v.len() / 64 + extra.max(4096));
        }
    }

    /// Appends a name; returns (name_off, name_len, fold_rec, stored_in_fold).
    /// The new fold record is owned by `owner`.
    fn push_name(&mut self, name: &str, owner: u32) -> (u32, u16, u32, bool) {
        let len = name.len().min(u16::MAX as usize);
        let name = &name[..name.floor_char_boundary(len)];
        Self::headroom(&mut self.fold, name.len() * 3 + 1);
        Self::headroom(&mut self.fold_starts, 1);
        Self::headroom(&mut self.fold_extra, 1);
        let fold_rec = self.fold_starts.len() as u32;
        let at = self.fold.len();
        self.fold_starts.push(at as u32);
        fold_into_mode(name, &mut self.fold, self.fold_mode);
        let in_fold = &self.fold[at..] == name.as_bytes();
        self.fold.push(0);
        // Records appended past the base are owned per `fold_extra`.
        while self.fold_extra.len() + (self.fold_base as usize) < fold_rec as usize {
            self.fold_extra.push(NONE);
        }
        self.fold_extra.push(owner);
        let off = if in_fold {
            0
        } else {
            Self::headroom(&mut self.names, name.len());
            let o = self.names.len() as u32;
            self.names.extend_from_slice(name.as_bytes());
            o
        };
        (off, name.len() as u16, fold_rec, in_fold)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_link(&mut self, record: u32, seq: u16, parent: u32, name: &str, attrs: u32, size: u64, modified: i64, created: i64) -> u32 {
        let is_dir = attrs & ntfs::usn::FILE_ATTRIBUTE_DIRECTORY != 0;
        let idx = self.entries.len() as u32;
        let (name_off, name_len, fold_rec, in_fold) = self.push_name(name, idx);
        Self::headroom(&mut self.entries, 1);
        for r in &mut self.ranks.by {
            // Only extend rank arrays that are in step with the entries
            // (they're empty until computed after a load).
            if r.len() == idx as usize {
                Self::headroom(r, 1);
                r.push(NONE);
            }
        }
        self.ranks.dirty += 1;
        if self.rec_map.len() <= record as usize {
            self.rec_map.resize(record as usize + 1, NONE);
        }
        self.entries.push(Entry {
            size: if is_dir { 0 } else { size },
            modified,
            record,
            parent,
            name_off,
            fold_rec,
            name_len,
            flags: (if is_dir { F_DIR } else { 0 }) | (if in_fold { F_NAME_IN_FOLD } else { 0 }) | pack_attrs(attrs),
            seq,
            _pad: 0,
        });
        if self.opts.created && self.created.len() == idx as usize {
            Self::headroom(&mut self.created, 1);
            self.created.push(created);
        }
        let prev = self.rec_map[record as usize];
        if prev != NONE {
            self.more_links.insert(idx, prev);
        }
        self.rec_map[record as usize] = idx;
        if !is_dir {
            self.add_size_up(parent, size as i64);
        }
        self.apply_rules_to_new(idx);
        self.generation += 1;
        idx
    }

    pub fn remove_entry(&mut self, i: u32) {
        let e = self.entries[i as usize];
        if e.is_deleted() {
            return;
        }
        // Unlink from the record's chain of hard links.
        let rec = e.record as usize;
        let next = self.more_links.remove(&i).unwrap_or(NONE);
        if self.rec_map[rec] == i {
            self.rec_map[rec] = next;
        } else {
            let prev = self.more_links.iter().find(|&(_, &n)| n == i).map(|(&p, _)| p);
            if let Some(p) = prev {
                if next == NONE {
                    self.more_links.remove(&p);
                } else {
                    self.more_links.insert(p, next);
                }
            }
        }
        self.add_size_up(e.parent, -(e.size as i64));
        self.kill_fold_rec(e.fold_rec as usize);
        self.recent.remove(&i);
        let m = &mut self.entries[i as usize];
        m.flags |= F_DELETED;
        self.garbage += 1;
        self.generation += 1;
    }

    pub fn remove_record(&mut self, record: u32) {
        let links: Vec<u32> = self.links(record).collect();
        for l in links {
            self.remove_entry(l);
        }
    }

    /// Removes `i` and everything below it (folder indexes, where a deleted
    /// folder produces a single notification).
    pub fn remove_subtree(&mut self, i: u32) {
        let mut doomed = vec![false; self.entries.len()];
        doomed[i as usize] = true;
        // Entries are created parent-first, so one forward pass suffices for
        // trees built by a walk; repeat until stable for anything else.
        loop {
            let mut changed = false;
            for k in 0..self.entries.len() {
                let p = self.entries[k].parent;
                if !doomed[k] && p != NONE && doomed[p as usize] && self.entries[k].is_live() {
                    doomed[k] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for k in (0..self.entries.len()).rev() {
            if doomed[k] {
                self.remove_entry(k as u32);
            }
        }
    }

    /// Renames/moves an entry in place, keeping its index so children's parent
    /// pointers stay valid when a directory is renamed.
    pub fn rename_entry(&mut self, i: u32, new_parent: u32, name: &str) {
        let e = self.entries[i as usize];
        if e.parent != new_parent {
            self.add_size_up(e.parent, -(e.size as i64));
            self.add_size_up(new_parent, e.size as i64);
        }
        if self.name(i) != name {
            self.kill_fold_rec(e.fold_rec as usize);
            let (off, len, fold_rec, in_fold) = self.push_name(name, i);
            let m = &mut self.entries[i as usize];
            m.name_off = off;
            m.name_len = len;
            m.fold_rec = fold_rec;
            m.flags = if in_fold { m.flags | F_NAME_IN_FOLD } else { m.flags & !F_NAME_IN_FOLD };
        }
        self.unrank(i, &[RankKey::Name, RankKey::Size, RankKey::Modified, RankKey::Path]);
        self.entries[i as usize].parent = new_parent;
        if e.is_dir() {
            self.ranks.path_stale = true;
            if !self.rules.is_empty() {
                self.rules_stale = true;
            }
        } else if !self.rules.is_empty() {
            self.entries[i as usize].flags &= !(F_EXCLUDED | F_INCLUDE_ANCESTOR);
            self.apply_rules_to_new(i);
        }
        self.generation += 1;
    }

    pub fn set_file_info(&mut self, record: u32, size: u64, modified: i64, created: i64) {
        let links: Vec<u32> = self.links(record).collect();
        for l in links {
            let e = self.entries[l as usize];
            self.unrank(l, &[RankKey::Modified]);
            if e.is_dir() {
                self.entries[l as usize].modified = modified;
                continue;
            }
            self.unrank(l, &[RankKey::Size]);
            self.add_size_up(e.parent, size as i64 - e.size as i64);
            let m = &mut self.entries[l as usize];
            m.size = size;
            m.modified = modified;
            if created != 0
                && let Some(c) = self.created.get_mut(l as usize)
            {
                *c = created;
            }
        }
        self.generation += 1;
    }

    /// Drops deleted entries and unreferenced arena bytes, renumbering entries.
    /// Entries whose parent chain is broken are dropped too.
    pub fn compact(&mut self) {
        let n = self.entries.len();
        // An entry survives if it and all of its ancestors are live.
        let mut alive = vec![0u8; n]; // 0 unknown, 1 alive, 2 dead
        for i in 0..n {
            let mut chain = Vec::new();
            let mut cur = i as u32;
            let verdict = loop {
                if cur == NONE {
                    break 1;
                }
                let c = cur as usize;
                if alive[c] != 0 {
                    break alive[c];
                }
                if self.entries[c].is_deleted() || chain.len() > 1024 {
                    break 2;
                }
                chain.push(c);
                cur = self.entries[c].parent;
            };
            for c in chain {
                alive[c] = verdict;
            }
        }
        let mut remap = vec![NONE; n];
        let mut new_entries = Vec::with_capacity(n - self.garbage.min(n));
        let mut new_created = Vec::new();
        for i in 0..n {
            if alive[i] != 1 {
                continue;
            }
            remap[i] = new_entries.len() as u32;
            // Names stay where they are for now; rebuild_fold below repacks them.
            new_entries.push(self.entries[i]);
            if let Some(c) = self.created.get(i) {
                new_created.push(*c);
            }
        }
        for e in &mut new_entries {
            if e.parent != NONE {
                e.parent = remap[e.parent as usize];
            }
        }
        self.rec_map.fill(NONE);
        self.more_links.clear();
        for (i, e) in new_entries.iter().enumerate() {
            let r = e.record as usize;
            if r >= self.rec_map.len() {
                self.rec_map.resize(r + 1, NONE);
            }
            if self.rec_map[r] != NONE {
                self.more_links.insert(i as u32, self.rec_map[r]);
            }
            self.rec_map[r] = i as u32;
        }
        self.recent = self
            .recent
            .drain()
            .filter(|&(i, _t)| remap[i as usize] != NONE).map(|(i, t)| (remap[i as usize], t))
            .collect();
        self.root = if self.root == NONE { NONE } else { remap[self.root as usize] };
        // The name arenas still hold the old layout, which name() reads by
        // the (copied) per-entry offsets; rebuild_fold repacks them.
        self.entries = new_entries;
        self.created = new_created;
        self.garbage = 0;
        self.rebuild_excl();
        self.rebuild_fold();
        self.compute_ranks();
        self.shrink();
        self.epoch += 1;
        self.generation += 1;
    }

    fn map_bytes(&self) -> usize {
        (self.fold_starts.capacity() + self.fold_extra.capacity() + self.rec_map.capacity()) * 4
            + self.fold_dead.capacity() * 8
            + self.excl.capacity() * 8
            + self.more_links.capacity() * 12
    }

    pub fn memory_bytes(&self) -> usize {
        self.entries.capacity() * size_of::<Entry>()
            + self.names.capacity()
            + self.fold.capacity()
            + self.created.capacity() * 8
            + self.map_bytes()
            + self.ranks.by.iter().map(|r| r.capacity() * 4).sum::<usize>()
    }

    pub fn memory_breakdown(&self) -> String {
        let mb = |b: usize| format!("{:.0}", b as f64 / 1048576.0);
        format!(
            "entries {} MB, names {} MB, folded {} MB, created {} MB, maps {} MB, ranks {} MB",
            mb(self.entries.capacity() * size_of::<Entry>()),
            mb(self.names.capacity()),
            mb(self.fold.capacity()),
            mb(self.created.capacity() * 8),
            mb(self.map_bytes()),
            mb(self.ranks.by.iter().map(|r| r.capacity() * 4).sum::<usize>()),
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::Config;
    use crate::ntfs::{Meta, ScanName};

    /// Builds a scan from (record, parent, name, is_dir, size).
    pub fn scan_of(items: &[(u32, u32, &str, bool, u64)]) -> Scan {
        let mut s = Scan { has_sizes: true, ..Default::default() };
        let max = items.iter().map(|i| i.0).max().unwrap_or(5).max(5) as usize;
        s.meta = vec![Meta::default(); max + 1];
        s.meta[5] = Meta { in_use: true, is_dir: true, ..Default::default() };
        s.names.push(ScanName { record: 5, parent: 5, off: 0, len: 1 });
        s.strings.push(b'.');
        for &(r, p, n, d, size) in items {
            s.meta[r as usize] = Meta { in_use: true, is_dir: d, size, modified: r as i64, created: 1000 - r as i64, ..Default::default() };
            s.names.push(ScanName { record: r, parent: p, off: s.strings.len() as u32, len: n.len() as u32 });
            s.strings.extend_from_slice(n.as_bytes());
        }
        s
    }

    pub fn sample() -> Index {
        Index::from_scan(
            'C',
            &scan_of(&[
                (30, 5, "Users", true, 0),
                (31, 30, "Me", true, 0),
                (32, 31, "Report.PDF", false, 1000),
                (33, 31, "notes.txt", false, 24),
                (34, 5, "Windows", true, 0),
                (35, 34, "notepad.exe", false, 200),
                (36, 11, "$UsnJrnl", false, 5), // metafile, hidden
                (37, 999, "orphan.txt", false, 1),
            ]),
        )
    }

    pub fn by_name(ix: &Index, n: &str) -> u32 {
        (0..ix.entries.len() as u32).find(|&i| ix.is_live(i) && ix.name(i) == n).unwrap()
    }

    #[test]
    fn builds_paths_and_sizes() {
        let ix = sample();
        assert_eq!(ix.path(by_name(&ix, "Report.PDF")), r"C:\Users\Me\Report.PDF");
        assert_eq!(ix.path(ix.root), r"C:\");
        assert_eq!(ix.entries[by_name(&ix, "Users") as usize].size, 1024);
        assert_eq!(ix.entries[ix.root as usize].size, 1224);
        assert!(!(0..ix.entries.len() as u32).any(|i| ix.is_live(i) && ix.name(i) == "$UsnJrnl"));
        assert!(!(0..ix.entries.len() as u32).any(|i| ix.is_live(i) && ix.name(i) == "orphan.txt"));
        assert_eq!(ix.fold_name(by_name(&ix, "Report.PDF")), b"report.pdf");
        assert_eq!(ix.depth(by_name(&ix, "Report.PDF")), 3);
        assert_eq!(ix.dirs_with_path(br"c:\users\me"), vec![by_name(&ix, "Me")]);
        assert_eq!(ix.find_child(by_name(&ix, "Me"), "notes.txt"), Some(by_name(&ix, "notes.txt")));
        assert_eq!(ix.child_counts()[by_name(&ix, "Me") as usize], 2);
    }

    #[test]
    fn ranks_order_by_each_key() {
        let ix = sample();
        let order = |k: RankKey| {
            let mut v: Vec<u32> = (0..ix.entries.len() as u32).filter(|&i| ix.is_live(i)).collect();
            v.sort_by_key(|&i| ix.ranks.get(k, i));
            v.iter().map(|&i| ix.name(i).to_string()).collect::<Vec<_>>()
        };
        assert_eq!(order(RankKey::Name), ["C:", "Me", "notepad.exe", "notes.txt", "Report.PDF", "Users", "Windows"]);
        assert_eq!(order(RankKey::Size), ["notes.txt", "notepad.exe", "Windows", "Report.PDF", "Me", "Users", "C:"]);
        // Path: by containing folder in tree order, then name.
        assert_eq!(order(RankKey::Path), ["C:", "Users", "Windows", "Me", "notes.txt", "Report.PDF", "notepad.exe"]);
    }

    #[test]
    fn sizes_rank_ties_break_by_name() {
        let ix = sample();
        let mut v: Vec<u32> = (0..ix.entries.len() as u32).filter(|&i| ix.is_live(i)).collect();
        v.sort_by_key(|&i| ix.ranks.get(RankKey::Size, i));
        let sizes: Vec<u64> = v.iter().map(|&i| ix.entries[i as usize].size).collect();
        assert!(sizes.windows(2).all(|w| w[0] <= w[1]));
    }

    #[test]
    fn rename_dir_keeps_children_and_moves_sizes() {
        let mut ix = sample();
        let me = by_name(&ix, "Me");
        let win = by_name(&ix, "Windows");
        ix.rename_entry(me, win, "You");
        assert_eq!(ix.path(by_name(&ix, "notes.txt")), r"C:\Windows\You\notes.txt");
        assert_eq!(ix.entries[by_name(&ix, "Users") as usize].size, 0);
        assert_eq!(ix.entries[win as usize].size, 1224);
        assert_eq!(ix.fold_name(me), b"you");
        assert!(ix.ranks.path_stale);
        assert_eq!(ix.ranks.get(RankKey::Name, me), NONE);
    }

    #[test]
    fn add_remove_compact() {
        let mut ix = sample();
        let me = by_name(&ix, "Me");
        let new = ix.add_link(50, 1, me, "New.TXT", 0x20, 10, 0, 0);
        assert_eq!(ix.path(new), r"C:\Users\Me\New.TXT");
        assert_eq!(ix.entries[ix.root as usize].size, 1234);
        ix.note_recent(new, 42);
        ix.remove_record(32);
        assert_eq!(ix.entries[ix.root as usize].size, 234);
        let users = by_name(&ix, "Users");
        let epoch = ix.epoch;
        ix.remove_entry(users); // children become unreachable and are compacted away
        ix.compact();
        assert_eq!(ix.garbage, 0);
        assert_eq!(ix.epoch, epoch + 1);
        let names: Vec<_> = (0..ix.entries.len() as u32).map(|i| ix.name(i).to_string()).collect();
        assert_eq!(names, vec!["C:", "Windows", "notepad.exe"]);
        assert_eq!(ix.path(by_name(&ix, "notepad.exe")), r"C:\Windows\notepad.exe");
        assert_eq!(ix.links(35).count(), 1);
        assert!(ix.recent.is_empty());
    }

    #[test]
    fn exclude_rules_hide_subtrees_and_apply_to_new_entries() {
        let mut ix = sample();
        let cfg = Config { exclude_folders: vec![r"C:\Users\Me".into()], exclude_files: vec!["*.exe".into()], ..Default::default() };
        ix.apply_rules(cfg.rules('C'));
        let vis: Vec<_> = (0..ix.entries.len() as u32).filter(|&i| ix.is_visible(i)).map(|i| ix.name(i).to_string()).collect();
        assert_eq!(vis, vec!["C:", "Users", "Windows"]);
        let me = by_name(&ix, "Me");
        let added = ix.add_link(60, 0, me, "inside.txt", 0x20, 1, 0, 0);
        assert!(!ix.is_visible(added));
        let win = by_name(&ix, "Windows");
        let ok = ix.add_link(61, 0, win, "ok.txt", 0x20, 1, 0, 0);
        assert!(ix.is_visible(ok));
        ix.apply_rules(Rules::default());
        assert!(ix.is_visible(me) && ix.is_visible(added));
    }

    #[test]
    fn include_only_keeps_ancestors_navigable() {
        let mut ix = sample();
        let mut cfg = Config::default();
        cfg.volumes.insert("C".into(), crate::config::VolumeConfig { include: true, include_only: vec![r"C:\Users\Me".into()] });
        ix.apply_rules(cfg.rules('C'));
        let vis: Vec<_> = (0..ix.entries.len() as u32).filter(|&i| ix.is_visible(i)).map(|i| ix.name(i).to_string()).collect();
        assert_eq!(vis, vec!["C:", "Users", "Me", "Report.PDF", "notes.txt"]);
    }

    #[test]
    fn fold_mode_switch_and_id_map() {
        let mut ix = Index::from_scan('C', &scan_of(&[(30, 5, "Café", false, 1)]));
        assert_eq!(ix.fold_name(by_name(&ix, "Café")), "café".as_bytes());
        ix.set_fold_mode(FoldMode::StripDiacritics);
        assert_eq!(ix.fold_name(by_name(&ix, "Café")), b"cafe");
        let a = ix.map_id(0xABCD_0000_0000_0000_0000_0000_0000_0001, true).unwrap();
        let b = ix.map_id(0xABCD_0000_0000_0000_0000_0000_0000_0002, true).unwrap();
        assert_ne!(a, b);
        assert_eq!(ix.map_id(0xABCD_0000_0000_0000_0000_0000_0000_0001, false), Some(a));
        assert!(a >= ntfs::FIRST_USER_RECORD);
    }

    #[test]
    fn remove_subtree() {
        let mut ix = sample();
        ix.remove_subtree(by_name(&ix, "Users"));
        let live: Vec<_> = (0..ix.entries.len() as u32).filter(|&i| ix.is_live(i)).map(|i| ix.name(i).to_string()).collect();
        assert_eq!(live, vec!["C:", "Windows", "notepad.exe"]);
        assert_eq!(ix.entries[ix.root as usize].size, 200);
    }
}
