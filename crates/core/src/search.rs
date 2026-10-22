//! Query execution.
//!
//! 1. If the query has a literal that must appear in the name, scan the folded
//!    name arena with SIMD `memmem` in parallel slices. Only names containing
//!    the literal become candidates; this is the bulk of Everything-like speed.
//! 2. Otherwise fall back to a parallel scan over all entries.
//! 3. Candidates are checked against the full query tree.
//! 4. A [`ResultSet`] keeps the (unsorted) matches. The first page is the
//!    top-k by `select_nth_unstable` with rank compares (integer compares for
//!    every key); a full sort only happens when someone scrolls past it.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::index::{Index, NONE, RankKey};
use crate::query::{Ctx, Dupe, PathGlob, Prep, Query};
use crate::runs::RunHistory;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    #[default]
    Name,
    Path,
    Size,
    Modified,
    Created,
    RunCount,
}

#[derive(Debug, Clone, Copy)]
pub struct SearchOptions {
    pub sort: SortKey,
    pub descending: bool,
    /// Results sorted up front (0 = all). More are sorted on demand by
    /// [`ResultSet::page`].
    pub limit: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions {
            sort: SortKey::Name,
            descending: false,
            limit: 100,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hit {
    pub vol: u16,
    pub idx: u32,
}

#[derive(Debug, Default)]
pub struct Hits {
    pub total: usize,
    pub items: Vec<Hit>,
    pub elapsed: Duration,
}

/// Extra state some queries/sorts need (owned by the engine).
#[derive(Default, Clone, Copy)]
pub struct Extras<'a> {
    pub runs: Option<&'a RunHistory>,
}

/// All live entries whose folded name contains `needle`.
pub fn arena_candidates(ix: &Index, needle: &[u8]) -> Vec<u32> {
    ix.name_candidates(needle)
}

pub fn build_prep(ix: &Index, q: &Query, runs: Option<&RunHistory>, need_runs: bool) -> Prep {
    Prep {
        parent_sets: q
            .parents
            .iter()
            .map(|p| ix.dirs_with_path(p).into_iter().collect())
            .collect(),
        child_counts: q.uses_child_counts.then(|| ix.child_counts()),
        runs: match runs {
            Some(r) if q.uses_runs || need_runs => r.for_index(ix),
            _ => HashMap::new(),
        },
        recent: if q.uses_recent {
            ix.recent.clone()
        } else {
            HashMap::new()
        },
        path_globs: q
            .path_globs
            .iter()
            .map(|(p, case)| PathGlob::new(p, *case, ix.entries.len()))
            .collect(),
    }
}

/// The entries `0..n` that `keep` accepts, in order. Fixed chunks that each
/// fill a vector, then one copy: rayon's `filter().collect()` builds and
/// joins a list of vectors, several times slower for millions of hits.
pub fn filter_range(n: usize, keep: impl Fn(u32) -> bool + Sync) -> Vec<u32> {
    const CHUNK: usize = 1 << 15;
    let parts: Vec<Vec<u32>> = (0..n.div_ceil(CHUNK))
        .into_par_iter()
        .map(|c| {
            (c * CHUNK..((c + 1) * CHUNK).min(n))
                .map(|i| i as u32)
                .filter(|&i| keep(i))
                .collect()
        })
        .collect();
    concat(parts)
}

/// The candidates `keep` accepts, in order. Rayon's adaptive splitting
/// balances uneven checks (path patterns climb folders) best here.
fn filter_list(ids: &[u32], keep: impl Fn(u32) -> bool + Sync) -> Vec<u32> {
    ids.par_iter().copied().filter(|&i| keep(i)).collect()
}

fn concat(parts: Vec<Vec<u32>>) -> Vec<u32> {
    let mut out = Vec::with_capacity(parts.iter().map(Vec::len).sum());
    for p in parts {
        out.extend_from_slice(&p);
    }
    out
}

/// A required path term names another drive (`f:\...` on C:): nothing on
/// this volume can match. (A folder index's root can be a folder,
/// `g:\photos`: compare the drive.)
fn other_drive(ix: &Index, q: &Query) -> bool {
    let root = ix.fold_name(ix.root);
    q.required_drives()
        .iter()
        .any(|&d| !(root.len() >= 2 && root[0] == d && root[1] == b':'))
}

fn matching(ix: &Index, q: &Query, prep: &Prep) -> Vec<u32> {
    if q.is_empty() {
        return filter_range(ix.entries.len(), |i| ix.is_visible(i));
    }
    let check = |i: u32| ix.is_visible(i) && q.root.eval(&mut Ctx::new(ix, i, prep));
    match q.driver() {
        // A lone substring term: the arena scan already is the answer.
        Some(needle) if q.is_single_name_substring() => {
            let mut c = arena_candidates(ix, &needle);
            if !ix.excl.is_empty() {
                c.retain(|&i| !ix.is_excluded_fast(i));
            }
            c
        }
        Some(needle) => filter_list(&arena_candidates(ix, &needle), check),
        None => filter_range(ix.entries.len(), check),
    }
}

/// Comparison context: indexes plus per-volume run counts.
struct Cmp<'a> {
    ixs: &'a [&'a Index],
    runs: &'a [HashMap<u32, u32>],
    key: SortKey,
    desc: bool,
}

impl Cmp<'_> {
    /// Total order (folded name, name, volume, index); within one volume the
    /// name rank encodes exactly that.
    fn by_name(&self, a: Hit, b: Hit) -> Ordering {
        let (ia, ib) = (self.ixs[a.vol as usize], self.ixs[b.vol as usize]);
        if a.vol == b.vol {
            let (ra, rb) = (
                ia.ranks.get(RankKey::Name, a.idx),
                ia.ranks.get(RankKey::Name, b.idx),
            );
            if ra != NONE && rb != NONE {
                return ra.cmp(&rb);
            }
        }
        ia.fold_name(a.idx)
            .cmp(ib.fold_name(b.idx))
            .then_with(|| ia.with_name_bytes(a.idx, |x| ib.with_name_bytes(b.idx, |y| x.cmp(y))))
            .then(a.vol.cmp(&b.vol))
            .then(a.idx.cmp(&b.idx))
    }

    fn ranked(&self, k: RankKey, a: Hit, b: Hit) -> Option<Ordering> {
        if a.vol != b.vol {
            return None;
        }
        let ix = self.ixs[a.vol as usize];
        let (ra, rb) = (ix.ranks.get(k, a.idx), ix.ranks.get(k, b.idx));
        (ra != NONE && rb != NONE).then(|| ra.cmp(&rb))
    }

    /// Component-wise comparison of the containing folders, then name.
    fn by_path(&self, a: Hit, b: Hit) -> Ordering {
        let (ia, ib) = (self.ixs[a.vol as usize], self.ixs[b.vol as usize]);
        let chain = |ix: &Index, i: u32| {
            let p = ix.entries[i as usize].parent;
            let mut c = if p == NONE {
                Vec::new()
            } else {
                ix.ancestors(p)
            };
            c.reverse();
            c
        };
        let (ca, cb) = (chain(ia, a.idx), chain(ib, b.idx));
        for (x, y) in ca.iter().zip(&cb) {
            let o = self.by_name(
                Hit {
                    vol: a.vol,
                    idx: *x,
                },
                Hit {
                    vol: b.vol,
                    idx: *y,
                },
            );
            if o != Ordering::Equal {
                return o;
            }
        }
        ca.len().cmp(&cb.len()).then_with(|| self.by_name(a, b))
    }

    fn cmp(&self, a: &Hit, b: &Hit) -> Ordering {
        let (a, b) = (*a, *b);
        let e = |h: Hit| &self.ixs[h.vol as usize].entries[h.idx as usize];
        let o = match self.key {
            SortKey::Name => self.by_name(a, b),
            SortKey::Size => self.ranked(RankKey::Size, a, b).unwrap_or_else(|| {
                { e(a).size }
                    .cmp(&{ e(b).size })
                    .then_with(|| self.by_name(a, b))
            }),
            SortKey::Modified => self.ranked(RankKey::Modified, a, b).unwrap_or_else(|| {
                { e(a).mtime }
                    .cmp(&{ e(b).mtime })
                    .then_with(|| self.by_name(a, b))
            }),
            SortKey::Path => self
                .ranked(RankKey::Path, a, b)
                .unwrap_or_else(|| self.by_path(a, b)),
            SortKey::Created => {
                let c = |h: Hit| self.ixs[h.vol as usize].created(h.idx).unwrap_or(0);
                c(a).cmp(&c(b)).then_with(|| self.by_name(a, b))
            }
            SortKey::RunCount => {
                let rc = |h: Hit| {
                    self.runs
                        .get(h.vol as usize)
                        .and_then(|m| m.get(&h.idx))
                        .copied()
                        .unwrap_or(0)
                };
                rc(a).cmp(&rc(b)).then_with(|| self.by_name(a, b))
            }
        };
        if self.desc { o.reverse() } else { o }
    }
}

/// Whether reading a name-sorted page off the stored name order beats
/// gathering and selecting the matches' ranks. Walking costs a bit test per
/// position until `k` hits turn up, about `k * n / m` of them for `m`
/// matches spread evenly; gathering costs a rank lookup per match. All of
/// them (`k` = None) is a full walk against a sort of `m` keys.
fn walk_pays(ix: &Index, m: usize, k: Option<usize>) -> bool {
    let n = ix.entries.len();
    if ix.ranks.name_order.is_empty() || ix.ranks_missing() || m < 4096 {
        return false;
    }
    match k {
        Some(k) => (m as u128) * (m as u128) >= (k as u128) * (n as u128) / 4,
        None => m * 8 >= n,
    }
}

/// One bit per entry, set for `ids` (in parallel for big sets).
fn bitset(n: usize, ids: &[u32]) -> Vec<u64> {
    let words = n.div_ceil(64);
    if ids.len() < 1 << 16 {
        let mut b = vec![0u64; words];
        for &i in ids {
            b[i as usize / 64] |= 1 << (i % 64);
        }
        return b;
    }
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    let b: Vec<AtomicU64> = (0..words).map(|_| AtomicU64::new(0)).collect();
    ids.par_chunks(1 << 14).for_each(|c| {
        for &i in c {
            b[i as usize / 64].fetch_or(1 << (i % 64), Relaxed);
        }
    });
    b.into_iter().map(AtomicU64::into_inner).collect()
}

fn merge(mut lists: Vec<Vec<Hit>>, c: &Cmp) -> Vec<Hit> {
    while lists.len() > 1 {
        let b = lists.pop().unwrap();
        let a = lists.pop().unwrap();
        let mut out = Vec::with_capacity(a.len() + b.len());
        let (mut i, mut j) = (0, 0);
        while i < a.len() && j < b.len() {
            if c.cmp(&a[i], &b[j]) != Ordering::Greater {
                out.push(a[i]);
                i += 1;
            } else {
                out.push(b[j]);
                j += 1;
            }
        }
        out.extend_from_slice(&a[i..]);
        out.extend_from_slice(&b[j..]);
        lists.push(out);
    }
    lists.pop().unwrap_or_default()
}

#[derive(Debug, PartialEq, Eq)]
pub struct Stale;

/// Matches of one query, sorted lazily.
pub struct ResultSet {
    pub total: usize,
    pub elapsed: Duration,
    pub opts: SearchOptions,
    per_vol: Vec<Vec<u32>>,
    runs: Vec<HashMap<u32, u32>>,
    epochs: Vec<u64>,
    top: Vec<Hit>,
    full: Option<Vec<Hit>>,
    /// Content search: the sorted matches as they were when it started, and
    /// which of them were dropped since (a bitset over positions).
    snapshot: Option<Vec<Hit>>,
    dropped: Vec<u64>,
}

impl ResultSet {
    pub fn new(ixs: &[&Index], q: &Query, opts: &SearchOptions, extras: Extras) -> ResultSet {
        let t0 = Instant::now();
        let need_runs = opts.sort == SortKey::RunCount;
        let mut per_vol: Vec<Vec<u32>> = Vec::with_capacity(ixs.len());
        let mut runs = Vec::with_capacity(ixs.len());
        for ix in ixs {
            // Before building matchers: path patterns hold a state per entry.
            if other_drive(ix, q) {
                per_vol.push(Vec::new());
                runs.push(HashMap::new());
                continue;
            }
            let prep = build_prep(ix, q, extras.runs, need_runs);
            per_vol.push(matching(ix, q, &prep));
            runs.push(prep.runs);
        }
        if let Some(d) = q.dupe {
            per_vol = dupes(ixs, per_vol, d);
        }
        let mut rs = ResultSet {
            total: per_vol.iter().map(Vec::len).sum(),
            elapsed: Duration::ZERO,
            opts: *opts,
            per_vol,
            runs,
            epochs: ixs.iter().map(|i| i.epoch).collect(),
            top: Vec::new(),
            full: None,
            snapshot: None,
            dropped: Vec::new(),
        };
        if opts.limit == 0 || opts.limit >= rs.total {
            rs.sort_all(ixs);
            rs.top = rs.full.clone().unwrap();
        } else {
            rs.top = rs.top_k(ixs, opts.limit);
        }
        rs.elapsed = t0.elapsed();
        rs
    }

    fn cmp<'a>(&'a self, ixs: &'a [&'a Index]) -> Cmp<'a> {
        Cmp {
            ixs,
            runs: &self.runs,
            key: self.opts.sort,
            desc: self.opts.descending,
        }
    }

    /// Sorted (first `k`, or all when `k` is None) hits of one volume.
    ///
    /// Fast path: when every candidate has a valid rank for the sort key,
    /// gather `(rank << 32 | idx)` into a contiguous array in parallel and
    /// select/sort plain integers. Otherwise compare with the full key.
    fn sorted_volume(&self, ixs: &[&Index], v: usize, k: Option<usize>) -> Vec<Hit> {
        let ix = ixs[v];
        let vol = v as u16;
        let ids = &self.per_vol[v];
        let rank_key = match self.opts.sort {
            SortKey::Name => Some(RankKey::Name),
            SortKey::Size => Some(RankKey::Size),
            SortKey::Modified => Some(RankKey::Modified),
            SortKey::Path => Some(RankKey::Path),
            SortKey::Created | SortKey::RunCount => None,
        };
        let desc = self.opts.descending;
        if self.opts.sort == SortKey::Name && walk_pays(ix, ids.len(), k) {
            return self.walk_name_order(ixs, v, k);
        }
        if let Some(rk) = rank_key {
            let keys: Option<Vec<u64>> = ids
                .par_iter()
                .map(|&i| {
                    let r = ix.ranks.get(rk, i);
                    (r != NONE).then(|| {
                        let r = if desc { u32::MAX - r } else { r };
                        (r as u64) << 32 | i as u64
                    })
                })
                .collect();
            if let Some(mut keys) = keys {
                if let Some(k) = k
                    && k < keys.len()
                {
                    keys.select_nth_unstable(k);
                    keys.truncate(k);
                }
                keys.par_sort_unstable();
                return keys
                    .into_iter()
                    .map(|x| Hit { vol, idx: x as u32 })
                    .collect();
            }
        }
        // No rank for this key (fast sort off, created, run count): the
        // order is (value, name order), and the name rank always exists, so
        // gather (value, name rank, index) as one integer and select on that
        // instead of calling the comparator millions of times.
        let value: Option<Box<dyn Fn(u32) -> u64 + Sync>> = match self.opts.sort {
            SortKey::Size => Some(Box::new(|i| ix.entries[i as usize].size)),
            SortKey::Modified => Some(Box::new(|i| ix.entries[i as usize].mtime as u64)),
            SortKey::Created if !ix.created.is_empty() => Some(Box::new(|i| {
                (ix.created(i).unwrap_or(0) as u64) ^ (1 << 63)
            })),
            SortKey::RunCount => {
                let runs = &self.runs[v];
                Some(Box::new(move |i| runs.get(&i).copied().unwrap_or(0) as u64))
            }
            _ => None,
        };
        if let Some(value) = value {
            let keys: Option<Vec<u128>> = ids
                .par_iter()
                .map(|&i| {
                    let r = ix.ranks.get(RankKey::Name, i);
                    (r != NONE).then(|| {
                        let (val, r) = if desc {
                            (u64::MAX - value(i), u32::MAX - r)
                        } else {
                            (value(i), r)
                        };
                        (val as u128) << 64 | (r as u128) << 32 | i as u128
                    })
                })
                .collect();
            if let Some(mut keys) = keys {
                if let Some(k) = k
                    && k < keys.len()
                {
                    keys.select_nth_unstable(k);
                    keys.truncate(k);
                }
                keys.par_sort_unstable();
                return keys
                    .into_iter()
                    .map(|x| Hit { vol, idx: x as u32 })
                    .collect();
            }
        }
        let c = self.cmp(ixs);
        let mut h: Vec<Hit> = ids.iter().map(|&idx| Hit { vol, idx }).collect();
        if let Some(k) = k
            && k < h.len()
        {
            h.select_nth_unstable_by(k, |a, b| c.cmp(a, b));
            h.truncate(k);
        }
        h.par_sort_unstable_by(|a, b| c.cmp(a, b));
        h
    }

    /// Name-sorted hits of volume `v` (the first `k`, or all) read off the
    /// stored name order instead of sorted: the matches go into a bitset,
    /// then the order is walked from the start (or the end, descending)
    /// until `k` are found. Entries renamed or added since the order was
    /// computed are sorted on their own and merged in.
    fn walk_name_order(&self, ixs: &[&Index], v: usize, k: Option<usize>) -> Vec<Hit> {
        let ix = ixs[v];
        let vol = v as u16;
        let ids = &self.per_vol[v];
        let bits = bitset(ix.entries.len(), ids);
        let has = |i: u32| bits[i as usize / 64] & (1u64 << (i % 64)) != 0;
        let want = k.unwrap_or(ids.len());
        let order = &ix.ranks.name_order;
        // Valid at position p: still matched, and its rank is still p.
        let at = |p: usize| {
            let i = order.get(p);
            (i != NONE && has(i) && ix.ranks.get(RankKey::Name, i) == p as u32).then_some(i)
        };
        let walked: Vec<Hit> = if self.opts.descending {
            (0..order.len())
                .rev()
                .filter_map(at)
                .take(want)
                .map(|idx| Hit { vol, idx })
                .collect()
        } else {
            (0..order.len())
                .filter_map(at)
                .take(want)
                .map(|idx| Hit { vol, idx })
                .collect()
        };
        let mut late: Vec<Hit> = ix
            .ranks
            .unranked_names
            .iter()
            .filter(|&&i| {
                (i as usize) < ix.entries.len() && has(i) && ix.ranks.get(RankKey::Name, i) == NONE
            })
            .map(|&idx| Hit { vol, idx })
            .collect();
        if late.is_empty() {
            return walked;
        }
        let c = self.cmp(ixs);
        late.sort_unstable_by(|a, b| c.cmp(a, b));
        late.dedup();
        let mut m = merge(vec![walked, late], &c);
        m.truncate(want);
        m
    }

    /// Top `k` per volume, then merged.
    fn top_k(&self, ixs: &[&Index], k: usize) -> Vec<Hit> {
        let lists: Vec<Vec<Hit>> = (0..self.per_vol.len())
            .map(|v| self.sorted_volume(ixs, v, Some(k)))
            .collect();
        let mut m = merge(lists, &self.cmp(ixs));
        m.truncate(k);
        m
    }

    fn sort_all(&mut self, ixs: &[&Index]) {
        let lists: Vec<Vec<Hit>> = (0..self.per_vol.len())
            .map(|v| self.sorted_volume(ixs, v, None))
            .collect();
        let full = merge(lists, &self.cmp(ixs));
        self.full = Some(full);
    }

    /// All (unsorted) matches in volume `v`, if its index still has `epoch`.
    pub fn volume_hits(&self, v: usize, epoch: u64) -> Option<&[u32]> {
        (self.epochs.get(v) == Some(&epoch)).then(|| self.per_vol[v].as_slice())
    }

    pub fn first_page(&self) -> &[Hit] {
        &self.top
    }

    fn check_epochs(&self, ixs: &[&Index]) -> Result<(), Stale> {
        if ixs.len() != self.epochs.len()
            || ixs.iter().zip(&self.epochs).any(|(i, &e)| i.epoch != e)
        {
            return Err(Stale);
        }
        Ok(())
    }

    /// Hits `offset..offset+count` of the snapshot: the full sorted result
    /// as it was at the first call. Positions in it stay valid while rows
    /// are dropped.
    pub fn snapshot_page(
        &mut self,
        ixs: &[&Index],
        offset: usize,
        count: usize,
    ) -> Result<(usize, Vec<Hit>), Stale> {
        self.check_epochs(ixs)?;
        if self.snapshot.is_none() {
            if self.full.is_none() {
                self.sort_all(ixs);
            }
            self.snapshot = self.full.clone();
            self.dropped = vec![0; self.total.div_ceil(64)];
        }
        let snap = self.snapshot.as_ref().unwrap();
        let end = offset.saturating_add(count).min(snap.len());
        Ok((
            snap.len(),
            snap.get(offset..end).unwrap_or_default().to_vec(),
        ))
    }

    /// Removes snapshot positions from the result (content search rejects).
    pub fn drop_positions(&mut self, ixs: &[&Index], positions: &[u32]) -> Result<(), Stale> {
        self.check_epochs(ixs)?;
        let Some(snap) = &self.snapshot else {
            return Ok(());
        };
        for &p in positions {
            if let Some(w) = self.dropped.get_mut(p as usize / 64) {
                *w |= 1 << (p % 64);
            }
        }
        let dropped = &self.dropped;
        let full: Vec<Hit> = snap
            .par_iter()
            .enumerate()
            .filter(|(i, _)| dropped[i / 64] >> (i % 64) & 1 == 0)
            .map(|(_, h)| *h)
            .collect();
        let mut per_vol = vec![Vec::new(); self.per_vol.len()];
        for h in &full {
            per_vol[h.vol as usize].push(h.idx);
        }
        self.per_vol = per_vol;
        self.total = full.len();
        self.top = full[..full.len().min(self.opts.limit.max(1))].to_vec();
        self.full = Some(full);
        Ok(())
    }

    /// Rows `offset..offset+count` in sort order. Sorts everything the first
    /// time a page beyond the precomputed top is requested.
    pub fn page(&mut self, ixs: &[&Index], offset: usize, count: usize) -> Result<Vec<Hit>, Stale> {
        self.check_epochs(ixs)?;
        let end = (offset + count).min(self.total);
        if offset >= end {
            return Ok(Vec::new());
        }
        if self.full.is_none() && end <= self.top.len() {
            return Ok(self.top[offset..end].to_vec());
        }
        if self.full.is_none() {
            self.sort_all(ixs);
        }
        Ok(self.full.as_ref().unwrap()[offset..end].to_vec())
    }
}

/// Keeps only hits whose name (or size, for files) occurs more than once
/// across all volumes.
fn dupes(ixs: &[&Index], per_vol: Vec<Vec<u32>>, kind: Dupe) -> Vec<Vec<u32>> {
    let key = |v: usize, i: u32| -> Vec<u8> {
        let ix = ixs[v];
        match kind {
            Dupe::Name => ix.fold_name(i).to_vec(),
            Dupe::Size => {
                let e = &ix.entries[i as usize];
                if e.is_dir() {
                    Vec::new()
                } else {
                    e.size.to_le_bytes().to_vec()
                }
            }
        }
    };
    let mut counts: HashMap<Vec<u8>, u32> = HashMap::new();
    for (v, ids) in per_vol.iter().enumerate() {
        for &i in ids {
            let k = key(v, i);
            if !k.is_empty() {
                *counts.entry(k).or_default() += 1;
            }
        }
    }
    per_vol
        .into_iter()
        .enumerate()
        .map(|(v, ids)| {
            ids.into_iter()
                .filter(|&i| counts.get(&key(v, i)).is_some_and(|&c| c > 1))
                .collect()
        })
        .collect()
}

/// One-shot search returning the first `limit` sorted hits.
pub fn search(ixs: &[&Index], q: &Query, opts: &SearchOptions) -> Hits {
    let rs = ResultSet::new(ixs, q, opts, Extras::default());
    Hits {
        total: rs.total,
        items: rs.top.clone(),
        elapsed: rs.elapsed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::{by_name, sample};

    fn names(ixs: &[&Index], h: &[Hit]) -> Vec<String> {
        h.iter()
            .map(|h| ixs[h.vol as usize].name(h.idx).to_string())
            .collect()
    }

    #[test]
    fn arena_scan_finds_each_name_once() {
        let ix = sample();
        let mut n: Vec<_> = arena_candidates(&ix, b"e")
            .iter()
            .map(|&i| ix.name(i))
            .collect();
        n.sort();
        assert_eq!(
            n,
            vec!["Me", "Report.PDF", "Users", "notepad.exe", "notes.txt"]
        );
    }

    #[test]
    fn sorts_and_limits() {
        let ix = sample();
        let q = Query::parse("file:").unwrap();
        let h = search(
            &[&ix],
            &q,
            &SearchOptions {
                sort: SortKey::Size,
                descending: true,
                limit: 2,
            },
        );
        assert_eq!(h.total, 3);
        assert_eq!(names(&[&ix], &h.items), vec!["Report.PDF", "notepad.exe"]);
        let h = search(
            &[&ix],
            &q,
            &SearchOptions {
                sort: SortKey::Name,
                descending: false,
                limit: 0,
            },
        );
        assert_eq!(
            names(&[&ix], &h.items),
            vec!["notepad.exe", "notes.txt", "Report.PDF"]
        );
        let ix2 = sample();
        let h = search(
            &[&ix, &ix2],
            &q,
            &SearchOptions {
                sort: SortKey::Name,
                descending: false,
                limit: 3,
            },
        );
        let got: Vec<_> = h
            .items
            .iter()
            .map(|h| (h.vol, ix.name(h.idx).to_string()))
            .collect();
        assert_eq!(
            got,
            vec![
                (0, "notepad.exe".into()),
                (1, "notepad.exe".into()),
                (0, "notes.txt".into())
            ]
        );
        assert_eq!(h.total, 6);
    }

    /// A path term starting with a drive skips other volumes; a folder
    /// volume rooted below a drive still counts as that drive.
    #[test]
    fn drive_terms_pick_volumes() {
        use crate::fold::FoldMode;
        use crate::index::tests::scan_of;
        let c = sample();
        let photos = Index::from_scan_named(
            'G',
            r"G:\Photos",
            &scan_of(&[(30, 5, "2024", true, 0), (31, 30, "beach.jpg", false, 9)]),
            FoldMode::Lower,
        );
        let total = |q: &str| {
            search(
                &[&c, &photos],
                &Query::parse(q).unwrap(),
                &SearchOptions::default(),
            )
            .total
        };
        assert_eq!(total(r"d:\"), 0);
        assert_eq!(total(r"g:\ *.jpg"), 1);
        assert_eq!(total(r"g:\photos\2024\"), 1);
        assert_eq!(total(r"c:\users\ *.pdf"), 1);
        assert_eq!(total(r"C:\USERS\ *.PDF"), 1);
        assert_eq!(
            total(r"case:c:\users\ *.pdf"),
            0,
            "case-sensitive: C: is upper case"
        );
        assert_eq!(total(r"c:\*\*.txt"), 1);
        assert_eq!(total(r"*.jpg | d:\"), 1, "inside an OR it's not required");
    }

    /// Name-sorted pages read off the stored name order (big result sets)
    /// equal a full sort, also after renames, moves, additions and
    /// deletions the order doesn't know about yet.
    #[test]
    fn name_order_walk_matches_a_sort_with_live_changes() {
        use crate::index::tests::scan_of;
        let words = [
            "alpha",
            "Beta",
            "gamma",
            "Delta",
            "eps",
            "zeta",
            "Ångström",
            "a",
            "b",
        ];
        let exts = ["txt", "log", "dll", "TXT", "rs"];
        let mut items: Vec<(u32, u32, String, bool, u64)> = vec![
            (30, 5, "dir one".into(), true, 0),
            (31, 5, "Dir two".into(), true, 0),
        ];
        let mut r = 0x2545F4914F6CDD1Du64;
        for rec in 40..30_040u32 {
            r ^= r << 13;
            r ^= r >> 7;
            r ^= r << 17;
            let name = format!(
                "{}{}.{}",
                words[(r % 9) as usize],
                r % 997,
                exts[(r >> 20) as usize % 5]
            );
            items.push((rec, 30 + (rec % 2), name, false, r % 5000));
        }
        let refs: Vec<(u32, u32, &str, bool, u64)> = items
            .iter()
            .map(|i| (i.0, i.1, i.2.as_str(), i.3, i.4))
            .collect();
        let mk = || {
            let mut ix = Index::from_scan('C', &scan_of(&refs));
            assert!(!ix.ranks_missing());
            let dir2 = by_name(&ix, "Dir two");
            for (k, i) in (100..30_000u32).step_by(97).enumerate() {
                let to = if k % 3 == 0 {
                    dir2
                } else {
                    ix.entries[i as usize].parent
                };
                ix.rename_entry(i, to, &format!("{}renamed{k}.txt", ["A", "z", "m"][k % 3]));
            }
            for k in 0..400u32 {
                ix.add_link(
                    40_000 + k,
                    0,
                    dir2,
                    &format!("new{}.log", k * 7 % 400),
                    0x20,
                    k as u64,
                    7,
                    0,
                );
            }
            for i in (50..30_000u32).step_by(149) {
                ix.remove_entry(i);
            }
            ix
        };
        let stale = mk();
        let mut fresh = mk();
        fresh.compute_ranks();
        assert!(stale.ranks.dirty > 0 && !stale.ranks.unranked_names.is_empty());
        for text in ["", "a", "*.txt", "ext:log", "renamed"] {
            let q = Query::parse(text).unwrap();
            for desc in [false, true] {
                for limit in [100, 0, 5000] {
                    let o = SearchOptions {
                        sort: SortKey::Name,
                        descending: desc,
                        limit,
                    };
                    let a = search(&[&fresh], &q, &o);
                    let b = search(&[&stale], &q, &o);
                    assert!(
                        a.total >= 4096 || text == "renamed" || text == "ext:log",
                        "{text}: walk not used ({})",
                        a.total
                    );
                    assert_eq!(a.total, b.total, "{text}");
                    assert_eq!(
                        names(&[&fresh], &a.items),
                        names(&[&stale], &b.items),
                        "{text} desc={desc} limit={limit}"
                    );
                }
            }
        }
    }

    /// Ranked and fallback comparisons must agree, or partially-stale ranks
    /// would scramble (or panic) sorts.
    #[test]
    fn rank_and_fallback_orders_agree_for_every_key() {
        let mk = || {
            let mut s = sample();
            let me = by_name(&s, "Me");
            s.add_link(70, 0, me, "added.txt", 0x20, 500, 99, 0);
            let notes = by_name(&s, "notes.txt");
            s.rename_entry(notes, me, "znotes.txt");
            let win = by_name(&s, "Windows");
            s.rename_entry(me, win, "Me2"); // folder move: path ranks stale
            s.set_file_info(35, 1, 5, 0);
            s
        };
        let stale = mk();
        let mut fresh = mk();
        fresh.compute_ranks();
        assert!(stale.ranks.dirty > 0 && stale.ranks.path_stale);
        let q = Query::parse("").unwrap();
        for key in [
            SortKey::Name,
            SortKey::Size,
            SortKey::Modified,
            SortKey::Path,
            SortKey::Created,
        ] {
            for desc in [false, true] {
                for limit in [0, 3] {
                    let o = SearchOptions {
                        sort: key,
                        descending: desc,
                        limit,
                    };
                    let a = names(&[&fresh], &search(&[&fresh], &q, &o).items);
                    let b = names(&[&stale], &search(&[&stale], &q, &o).items);
                    assert_eq!(a, b, "{key:?} desc={desc} limit={limit}");
                }
            }
        }
        let o = SearchOptions {
            sort: SortKey::Path,
            descending: false,
            limit: 0,
        };
        let paths: Vec<String> = search(&[&stale], &q, &o)
            .items
            .iter()
            .map(|h| stale.path(h.idx))
            .collect();
        assert_eq!(
            paths[..4],
            [r"C:\", r"C:\Users", r"C:\Windows", r"C:\Windows\Me2"]
        );
    }

    #[test]
    fn paging_sorts_lazily_and_detects_stale_epochs() {
        let mut ix = sample();
        let q = Query::parse("").unwrap();
        let opts = SearchOptions {
            sort: SortKey::Name,
            descending: false,
            limit: 2,
        };
        let mut rs = ResultSet::new(&[&ix], &q, &opts, Extras::default());
        assert_eq!(rs.total, 7);
        assert_eq!(
            names(&[&ix], &rs.page(&[&ix], 0, 2).unwrap()),
            vec!["C:", "Me"]
        );
        assert_eq!(
            names(&[&ix], &rs.page(&[&ix], 2, 3).unwrap()),
            vec!["notepad.exe", "notes.txt", "Report.PDF"]
        );
        assert_eq!(
            names(&[&ix], &rs.page(&[&ix], 5, 10).unwrap()),
            vec!["Users", "Windows"]
        );
        assert!(rs.page(&[&ix], 50, 10).unwrap().is_empty());
        ix.compact();
        assert_eq!(rs.page(&[&ix], 0, 1), Err(Stale));
    }

    #[test]
    fn snapshot_positions_survive_drops() {
        let ix = sample();
        let q = Query::parse("").unwrap();
        let mut rs = ResultSet::new(
            &[&ix],
            &q,
            &SearchOptions {
                sort: SortKey::Name,
                descending: false,
                limit: 2,
            },
            Extras::default(),
        );
        let (n, first) = rs.snapshot_page(&[&ix], 0, 3).unwrap();
        assert_eq!(
            (n, names(&[&ix], &first)),
            (7, vec!["C:".to_string(), "Me".into(), "notepad.exe".into()])
        );
        rs.drop_positions(&[&ix], &[0, 2]).unwrap();
        assert_eq!(rs.total, 5);
        assert_eq!(names(&[&ix], rs.first_page()), vec!["Me", "notes.txt"]);
        // Positions still refer to the original order.
        assert_eq!(
            names(&[&ix], &rs.snapshot_page(&[&ix], 3, 1).unwrap().1),
            vec!["notes.txt"]
        );
        rs.drop_positions(&[&ix], &[3, 99]).unwrap();
        assert_eq!(
            names(&[&ix], &rs.page(&[&ix], 0, 10).unwrap()),
            vec!["Me", "Report.PDF", "Users", "Windows"]
        );
        assert_eq!(rs.volume_hits(0, ix.epoch).unwrap().len(), 4);
    }

    #[test]
    fn dupes_and_runs() {
        let ix = sample();
        let ix2 = sample();
        let q = Query::parse("dupe: note").unwrap();
        let h = search(
            &[&ix, &ix2],
            &q,
            &SearchOptions {
                limit: 0,
                ..Default::default()
            },
        );
        assert_eq!(
            names(&[&ix, &ix2], &h.items),
            vec!["notepad.exe", "notepad.exe", "notes.txt", "notes.txt"]
        );
        let q = Query::parse("dupe: note").unwrap();
        assert_eq!(search(&[&ix], &q, &SearchOptions::default()).total, 0);

        let mut ix = sample();
        ix.volume_serial = 1;
        let mut runs = RunHistory::default();
        runs.record(1, 33, 0);
        runs.record(1, 33, 0);
        runs.record(1, 32, 0);
        let q = Query::parse("file:").unwrap();
        let opts = SearchOptions {
            sort: SortKey::RunCount,
            descending: true,
            limit: 0,
        };
        let rs = ResultSet::new(&[&ix], &q, &opts, Extras { runs: Some(&runs) });
        assert_eq!(
            names(&[&ix], rs.first_page()),
            vec!["notes.txt", "Report.PDF", "notepad.exe"]
        );
        let q = Query::parse("runcount:>0").unwrap();
        let rs = ResultSet::new(
            &[&ix],
            &q,
            &SearchOptions {
                limit: 0,
                ..Default::default()
            },
            Extras { runs: Some(&runs) },
        );
        assert_eq!(rs.total, 2);
    }

    #[test]
    fn excluded_entries_are_not_found() {
        let mut ix = sample();
        ix.apply_rules(
            crate::config::Config {
                exclude_files: vec!["*.txt".into()],
                ..Default::default()
            }
            .rules('C'),
        );
        assert_eq!(
            search(
                &[&ix],
                &Query::parse("note").unwrap(),
                &SearchOptions::default()
            )
            .total,
            1
        );
        assert_eq!(
            search(
                &[&ix],
                &Query::parse("").unwrap(),
                &SearchOptions::default()
            )
            .total,
            6
        );
    }

    #[test]
    fn parent_query_resolves_folders() {
        let ix = sample();
        let q = Query::parse(r"parent:c:\users\me").unwrap();
        let h = search(
            &[&ix],
            &q,
            &SearchOptions {
                limit: 0,
                ..Default::default()
            },
        );
        assert_eq!(names(&[&ix], &h.items), vec!["notes.txt", "Report.PDF"]);
        let q = Query::parse("empty:").unwrap();
        assert_eq!(search(&[&ix], &q, &SearchOptions::default()).total, 0);
    }
}
