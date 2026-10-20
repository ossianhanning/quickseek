//! Query execution.
//!
//! 1. If the query has a literal that must appear in the name, scan the folded
//!    name arena with SIMD `memmem` in parallel slices. Only names containing
//!    the literal become candidates; this is the bulk of Everything-like speed.
//! 2. Otherwise fall back to a parallel scan over all entries.
//! 3. Candidates are checked against the full query tree.
//! 4. Only the top `limit` results are sorted (`select_nth_unstable`), so a
//!    query matching a million files still returns the first page instantly.

use std::cmp::Ordering;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::index::{Index, NONE};
use crate::query::{Ctx, Query};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    #[default]
    Name,
    Path,
    Size,
    Modified,
    Created,
}

#[derive(Debug, Clone, Copy)]
pub struct SearchOptions {
    pub sort: SortKey,
    pub descending: bool,
    /// Maximum results to return sorted (0 = all).
    pub limit: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        SearchOptions { sort: SortKey::Name, descending: false, limit: 100 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Arena slices smaller than this aren't worth splitting further.
const MIN_SLICE: usize = 1 << 20;

/// All live entries whose folded name contains `needle`.
pub fn arena_candidates(ix: &Index, needle: &[u8]) -> Vec<u32> {
    let hay = &ix.fold;
    if hay.is_empty() || needle.is_empty() {
        return Vec::new();
    }
    // Split at `\0` boundaries so no match can straddle two slices.
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
            let mut rec = ix.fold_starts.partition_point(|&s| (s as usize) <= start).saturating_sub(1);
            while pos < end {
                let Some(off) = finder.find(&hay[pos..end]) else { break };
                let at = pos + off;
                // Fold record containing `at`: gallop forward from the previous one,
                // since matches arrive in increasing order.
                let starts = &ix.fold_starts;
                let (mut lo, mut step) = (rec, 1usize);
                while lo + step < starts.len() && starts[lo + step] as usize <= at {
                    lo += step;
                    step *= 2;
                }
                let hi = (lo + step + 1).min(starts.len());
                rec = lo + starts[lo..hi].partition_point(|&s| s as usize <= at) - 1;
                let owner = ix.fold_owner[rec];
                if owner != NONE {
                    out.push(owner);
                }
                // Skip the rest of this name.
                pos = ix.fold_starts.get(rec + 1).map_or(end, |&s| s as usize);
            }
            out
        })
        .collect()
}

fn matching(ix: &Index, q: &Query) -> Vec<u32> {
    let check = |i: u32| ix.is_live(i) && q.root.eval(&mut Ctx::new(ix, i));
    match q.driver() {
        // A lone substring term: the arena scan already is the answer.
        Some(needle) if q.is_single_name_substring() => arena_candidates(ix, &needle),
        Some(needle) => {
            let cands = arena_candidates(ix, &needle);
            cands.into_par_iter().filter(|&i| check(i)).collect()
        }
        None => (0..ix.entries.len() as u32).into_par_iter().filter(|&i| check(i)).collect(),
    }
}

fn compare(ixs: &[&Index], key: SortKey, a: Hit, b: Hit) -> Ordering {
    let (ia, ib) = (ixs[a.vol as usize], ixs[b.vol as usize]);
    let (ea, eb) = (&ia.entries[a.idx as usize], &ib.entries[b.idx as usize]);
    // Total order (folded name, name, volume, index). Within one volume the
    // precomputed rank encodes exactly that order, so it's an integer compare.
    let by_name = || {
        if a.vol == b.vol {
            let (ra, rb) = (ia.name_rank[a.idx as usize], ia.name_rank[b.idx as usize]);
            if ra != NONE && rb != NONE {
                return ra.cmp(&rb);
            }
        }
        ia.fold_name(a.idx)
            .cmp(ib.fold_name(b.idx))
            .then_with(|| ia.name(a.idx).cmp(ib.name(b.idx)))
            .then(a.vol.cmp(&b.vol))
            .then(a.idx.cmp(&b.idx))
    };
    match key {
        SortKey::Name => by_name(),
        SortKey::Path => {
            let (mut pa, mut pb) = (Vec::new(), Vec::new());
            ia.fold_path_into(a.idx, &mut pa);
            ib.fold_path_into(b.idx, &mut pb);
            pa.cmp(&pb).then_with(by_name)
        }
        SortKey::Size => ea.size.cmp(&eb.size).then_with(by_name),
        SortKey::Modified => ea.modified.cmp(&eb.modified).then_with(by_name),
        SortKey::Created => ea.created.cmp(&eb.created).then_with(by_name),
    }
}

/// Searches several volume indexes at once.
pub fn search(ixs: &[&Index], q: &Query, opts: &SearchOptions) -> Hits {
    let t0 = Instant::now();
    let cmp = |a: &Hit, b: &Hit| {
        let o = compare(ixs, opts.sort, *a, *b);
        if opts.descending { o.reverse() } else { o }
    };
    let top_k = |hits: &mut Vec<Hit>| {
        if opts.limit > 0 && opts.limit < hits.len() {
            hits.select_nth_unstable_by(opts.limit, cmp);
            hits.truncate(opts.limit);
        }
    };
    // Top-k per volume first: within a volume, name order is an integer
    // compare on the precomputed rank. Only the few survivors are merged with
    // the slower cross-volume comparison.
    let mut total = 0;
    let mut hits: Vec<Hit> = Vec::new();
    for (v, ix) in ixs.iter().enumerate() {
        let vol = v as u16;
        let mut h: Vec<Hit> = if q.is_empty() {
            (0..ix.entries.len() as u32).into_par_iter().filter(|&i| ix.is_live(i)).map(|idx| Hit { vol, idx }).collect()
        } else {
            matching(ix, q).into_par_iter().map(|idx| Hit { vol, idx }).collect()
        };
        total += h.len();
        top_k(&mut h);
        hits.append(&mut h);
    }
    top_k(&mut hits);
    hits.par_sort_unstable_by(cmp);
    Hits { total, items: hits, elapsed: t0.elapsed() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::sample;

    fn names(ix: &Index, h: &Hits) -> Vec<String> {
        h.items.iter().map(|h| ix.name(h.idx).to_string()).collect()
    }

    #[test]
    fn arena_scan_finds_each_name_once() {
        let ix = sample();
        let mut c = arena_candidates(&ix, b"e");
        c.sort();
        let mut n: Vec<_> = c.iter().map(|&i| ix.name(i)).collect();
        n.sort();
        assert_eq!(n, vec!["Me", "Report.PDF", "Users", "notepad.exe", "notes.txt"]);
    }

    #[test]
    fn sorts_and_limits() {
        let ix = sample();
        let q = Query::parse("file:").unwrap();
        let h = search(&[&ix], &q, &SearchOptions { sort: SortKey::Size, descending: true, limit: 2 });
        assert_eq!(h.total, 3);
        assert_eq!(names(&ix, &h), vec!["Report.PDF", "notepad.exe"]);
        let h = search(&[&ix], &q, &SearchOptions { sort: SortKey::Name, descending: false, limit: 0 });
        assert_eq!(names(&ix, &h), vec!["notepad.exe", "notes.txt", "Report.PDF"]);
        // Two volumes: per-volume top-k then merge must equal a global sort.
        let ix2 = sample();
        let h = search(&[&ix, &ix2], &q, &SearchOptions { sort: SortKey::Name, descending: false, limit: 3 });
        let got: Vec<_> = h.items.iter().map(|h| (h.vol, ix.name(h.idx).to_string())).collect();
        assert_eq!(got, vec![(0, "notepad.exe".into()), (1, "notepad.exe".into()), (0, "notes.txt".into())]);
        assert_eq!(h.total, 6);
    }

    #[test]
    fn deleted_and_renamed_entries_are_not_found_by_old_name() {
        let mut ix = sample();
        let r = (0..ix.entries.len() as u32).find(|&i| ix.name(i) == "Report.PDF").unwrap();
        let parent = ix.entries[r as usize].parent;
        ix.rename_entry(r, parent, "Summary.pdf");
        let q = Query::parse("report").unwrap();
        assert_eq!(search(&[&ix], &q, &SearchOptions::default()).total, 0);
        let q = Query::parse("summary").unwrap();
        assert_eq!(search(&[&ix], &q, &SearchOptions::default()).total, 1);
    }
}
