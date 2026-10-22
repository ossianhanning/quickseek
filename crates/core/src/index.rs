//! In-memory index of one volume (or one indexed folder tree).
//!
//! Layout is chosen for the search loop, not for convenience:
//! * every folded (lowercased) name lives in one contiguous byte arena, each
//!   terminated by `\0`, so a substring query is a single SIMD `memmem` pass
//!   over a few tens of MB instead of millions of small string compares;
//! * entries are fixed-size POD structs with parent *indices* (not paths), so
//!   paths are only built for the results actually shown;
//! * entries are kept sorted by MFT record number, so a small block index
//!   (4 bytes per 64 records) finds a record's entries in a few probes, which
//!   is what makes applying USN journal records cheap without asking the OS
//!   for paths;
//! * per-key sort ranks (Everything's "fast sort") turn sorting millions of
//!   results into integer compares; they're bit-packed to the width the
//!   volume needs (22 bits for 3M files).

use std::collections::HashMap;
use std::sync::Arc;

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
    ATTR_BITS
        .iter()
        .filter(|(w, _)| a & w != 0)
        .fold(0, |f, (_, b)| f | b)
}

/// Longest stored name in bytes (NTFS allows 255 UTF-16 units: 765 bytes).
pub const MAX_NAME: usize = 1023;

/// 28 bytes per file (4-byte aligned; x86 reads the unaligned u64s at full
/// speed). The MFT record number isn't stored: entries are sorted by it, so
/// the block index gives its high bits and `len_rec` the low 6. Optional data
/// (date created) and rarely read data (sequence numbers, late hard links)
/// live in side tables.
#[repr(C, packed(4))]
#[derive(Clone, Copy, Debug)]
pub struct Entry {
    /// File size, or total size of everything below a directory.
    pub size: u64,
    pub modified: i64,
    /// Entry index of the parent directory (`NONE` for the root).
    pub parent: u32,
    /// With `F_NAME_IN_FOLD`: offset of the name in `fold`. Otherwise offset
    /// in `names` of a 4-byte fold offset followed by the original name.
    pub name_ref: u32,
    /// Name length in bytes (low 10 bits), record number mod 64 (high 6).
    pub len_rec: u16,
    pub flags: u16,
}

impl Entry {
    #[inline]
    pub fn name_len(&self) -> usize {
        (self.len_rec & 0x3FF) as usize
    }
    fn len_rec(len: usize, record: u32) -> u16 {
        debug_assert!(len <= MAX_NAME);
        len as u16 | ((record % 64) as u16) << 10
    }
    fn rec_lo(&self) -> u32 {
        (self.len_rec >> 10) as u32
    }
    /// Win32 file attributes.
    pub fn attrs(&self) -> u32 {
        let mut a = ATTR_BITS
            .iter()
            .filter(|(_, b)| self.flags & b != 0)
            .fold(0, |a, (w, _)| a | w);
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
        IndexOptions {
            created: true,
            fast_sort: [true; 4],
        }
    }
}

/// Unsigned integers below `2^bits - 1` packed at a fixed bit width; the
/// all-ones value reads back as `NONE`. Eight bytes of tail padding let every
/// read be one unaligned 8-byte load.
#[derive(Default, Clone, PartialEq, Eq, Debug)]
pub struct Packed {
    bits: u32,
    len: usize,
    data: Vec<u8>,
}

impl Packed {
    /// `len` slots, all `NONE`, wide enough for values `0..max`.
    pub fn new(len: usize, max: usize) -> Packed {
        let bits = (usize::BITS - max.leading_zeros()).clamp(1, 32);
        Packed {
            bits,
            len,
            data: vec![0xFF; Self::bytes(bits, len)],
        }
    }

    fn bytes(bits: u32, len: usize) -> usize {
        (len * bits as usize).div_ceil(8) + 8
    }

    fn mask(&self) -> u64 {
        (1u64 << self.bits) - 1
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn get(&self, i: usize) -> u32 {
        if i >= self.len {
            return NONE;
        }
        let bit = i * self.bits as usize;
        // SAFETY: `data` holds 8 bytes past the last slot's first byte.
        let w = unsafe { (self.data.as_ptr().add(bit / 8) as *const u64).read_unaligned() };
        let v = (u64::from_le(w) >> (bit % 8)) & self.mask();
        if v == self.mask() { NONE } else { v as u32 }
    }

    #[inline]
    pub fn set(&mut self, i: usize, v: u32) {
        assert!(i < self.len);
        let m = self.mask();
        let v = if v == NONE { m } else { v as u64 & m };
        let (bit, at) = (i * self.bits as usize, i * self.bits as usize / 8);
        let mut w = u64::from_le_bytes(self.data[at..at + 8].try_into().unwrap());
        w = (w & !(m << (bit % 8))) | v << (bit % 8);
        self.data[at..at + 8].copy_from_slice(&w.to_le_bytes());
    }

    /// Appends a `NONE` slot.
    pub fn push_none(&mut self) {
        self.len += 1;
        let need = Self::bytes(self.bits, self.len);
        if self.data.len() < need {
            let extra = need - self.data.len();
            Index::headroom(&mut self.data, extra);
            self.data.resize(need, 0xFF);
        }
    }

    pub fn capacity_bytes(&self) -> usize {
        self.data.capacity()
    }

    pub fn shrink_to_fit(&mut self) {
        self.data.shrink_to_fit();
    }
}

/// Position of each entry in each sort order. `NONE` = changed since the
/// ranks were computed; comparisons fall back to the real key for those.
#[derive(Default)]
pub struct Ranks {
    pub by: [Packed; 4],
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
        self.by[key as usize].get(i as usize)
    }
}

#[derive(Default)]
pub struct Index {
    pub letter: char,
    pub entries: Vec<Entry>,
    /// Original UTF-8 names that differ from their folded form, each behind
    /// the 4-byte offset of its folded form (see `Entry::name_ref`).
    pub names: Vec<u8>,
    /// Folded names, `\0`-terminated; the n-th name is "fold record" n.
    pub fold: Vec<u8>,
    /// Byte offset of every 64th fold record. A record's number is found by
    /// counting `\0`s from the nearest sample, so there's no per-name table.
    pub fold_samples: Vec<u32>,
    pub fold_records: u32,
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
    /// MFT sequence number per entry (tells a reused record from the old file).
    pub seqs: Vec<u16>,
    /// `entries[..rec_sorted]` are sorted by record; `rec_blocks[b]` is the
    /// first of them with record >= 64 * b.
    pub rec_sorted: u32,
    pub rec_blocks: Vec<u32>,
    /// Record number of each entry past `rec_sorted`.
    pub late_records: Vec<u32>,
    /// Entries past `rec_sorted` (added since the last compaction): record →
    /// newest such entry, chained to older ones of the record by `more_links`.
    pub late_links: HashMap<u32, u32>,
    pub more_links: HashMap<u32, u32>,
    /// One past the highest record number in use (or reserved by `map_id`).
    pub rec_count: u32,
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

/// Number of `\0` bytes (u8 lanes, so it vectorizes well).
#[inline]
fn count_zeros(b: &[u8]) -> usize {
    b.chunks(255)
        .map(|c| c.iter().fold(0u8, |n, &x| n + (x == 0) as u8) as usize)
        .sum()
}

/// One chunk of a fold arena rebuild.
struct FoldPart {
    fold: Vec<u8>,
    samples: Vec<u32>,
    /// Differing names, each behind 4 placeholder bytes for its fold offset.
    names: Vec<u8>,
    /// Per entry: (fold offset, names offset or `NONE` when stored in fold).
    refs: Vec<(u32, u32)>,
}

/// Folds `n` names in parallel chunks (a multiple of 64 records each, so
/// fold samples line up).
fn fold_parts<'a>(
    n: usize,
    mode: FoldMode,
    name: impl Fn(usize) -> &'a str + Sync,
) -> Vec<FoldPart> {
    const CHUNK: usize = 1 << 15;
    (0..n.div_ceil(CHUNK))
        .into_par_iter()
        .map(|c| {
            let range = c * CHUNK..((c + 1) * CHUNK).min(n);
            let mut p = FoldPart {
                fold: Vec::with_capacity(range.len() * 16),
                samples: Vec::with_capacity(range.len().div_ceil(64)),
                names: Vec::new(),
                refs: Vec::with_capacity(range.len()),
            };
            for i in range {
                let s = name(i);
                let at = p.fold.len();
                if i.is_multiple_of(64) {
                    p.samples.push(at as u32);
                }
                fold_into_mode(s, &mut p.fold, mode);
                if &p.fold[at..] == s.as_bytes() {
                    p.refs.push((at as u32, NONE));
                } else {
                    p.refs.push((at as u32, p.names.len() as u32));
                    p.names.extend_from_slice(&[0; 4]);
                    p.names.extend_from_slice(s.as_bytes());
                }
                p.fold.push(0);
            }
            p
        })
        .collect()
}

impl Index {
    pub fn from_scan(letter: char, scan: &Scan) -> Index {
        Self::from_scan_named(letter, &format!("{letter}:"), scan, FoldMode::Lower)
    }

    /// `root_name` is how the root shows in paths (`C:` or `G:\Photos`).
    pub fn from_scan_named(
        letter: char,
        root_name: &str,
        scan: &Scan,
        fold_mode: FoldMode,
    ) -> Index {
        Self::from_scan_with(letter, root_name, scan, fold_mode, IndexOptions::default())
    }

    /// `root_name` is how the root shows in paths (`C:` or `G:` plus a folder).
    pub fn from_scan_with(
        letter: char,
        root_name: &str,
        scan: &Scan,
        fold_mode: FoldMode,
        opts: IndexOptions,
    ) -> Index {
        let mut ix = Index {
            letter,
            has_sizes: scan.has_sizes,
            root: NONE,
            fold_mode,
            opts,
            ..Default::default()
        };
        if opts.created {
            ix.created.reserve(scan.names.len() + 1);
        }
        ix.entries.reserve(scan.names.len() + 1);
        ix.seqs.reserve(scan.names.len() + 1);
        // Entries go in record order (an MFT scan already is; folder walks aren't).
        let mut order: Vec<u32> = (0..scan.names.len() as u32).collect();
        if !scan.names.is_sorted_by_key(|n| n.record) {
            order.par_sort_by_key(|&k| scan.names[k as usize].record);
        }
        // Until rebuild_fold, `name_ref` is the offset into `tmp`.
        let mut tmp: Vec<u8> = Vec::with_capacity(scan.strings.len() + root_name.len());
        let mut rec_first = vec![NONE; scan.meta.len()];
        let mut parent_rec: Vec<u32> = Vec::with_capacity(scan.names.len() + 1);
        let mut records: Vec<u32> = Vec::with_capacity(scan.names.len() + 1);
        let mut push = |ix: &mut Index, record: u32, name: &str, m: &ntfs::Meta, parent: u32| {
            let len = name.floor_char_boundary(name.len().min(MAX_NAME));
            ix.entries.push(Entry {
                size: if m.is_dir { 0 } else { m.size },
                modified: m.modified,
                parent: NONE,
                name_ref: tmp.len() as u32,
                len_rec: Entry::len_rec(len, record),
                flags: (if m.is_dir { F_DIR } else { 0 }) | pack_attrs(m.attrs),
            });
            records.push(record);
            tmp.extend_from_slice(&name.as_bytes()[..len]);
            ix.seqs.push(m.seq);
            if opts.created {
                ix.created.push(m.created);
            }
            parent_rec.push(parent);
        };
        for &k in &order {
            let n = &scan.names[k as usize];
            let Some(m) = scan.meta.get(n.record as usize) else {
                continue;
            };
            if !m.in_use || ntfs::is_hidden_metafile(n.record, n.parent) {
                continue;
            }
            let is_root = n.record == ntfs::ROOT_RECORD;
            if is_root && ix.root != NONE {
                continue;
            }
            let idx = ix.entries.len() as u32;
            if rec_first[n.record as usize] == NONE {
                rec_first[n.record as usize] = idx;
            }
            if is_root {
                ix.root = idx;
            }
            push(
                &mut ix,
                n.record,
                if is_root { root_name } else { scan.name(n) },
                m,
                if is_root { NONE } else { n.parent },
            );
        }
        if ix.root == NONE {
            // Volume without a visible root (shouldn't happen): synthesize one,
            // in record order.
            let dir = ntfs::Meta {
                in_use: true,
                is_dir: true,
                ..Default::default()
            };
            push(&mut ix, ntfs::ROOT_RECORD, root_name, &dir, NONE);
            let last = ix.entries.len() - 1;
            let at = records[..last].partition_point(|&r| r < ntfs::ROOT_RECORD);
            ix.entries[at..].rotate_right(1);
            ix.seqs[at..].rotate_right(1);
            parent_rec[at..].rotate_right(1);
            records[at..].rotate_right(1);
            if opts.created {
                ix.created[at..].rotate_right(1);
            }
            for f in rec_first
                .iter_mut()
                .filter(|f| **f != NONE && **f as usize >= at && (**f as usize) < last)
            {
                *f += 1;
            }
            if rec_first.len() <= ntfs::ROOT_RECORD as usize {
                rec_first.resize(ntfs::ROOT_RECORD as usize + 1, NONE);
            }
            rec_first[ntfs::ROOT_RECORD as usize] = at as u32;
            ix.root = at as u32;
        }

        // Resolve parent records to entry indices; drop orphans.
        let mut orphans = 0usize;
        for (e, &pr) in ix.entries.iter_mut().zip(&parent_rec) {
            if pr == NONE {
                continue;
            }
            match rec_first.get(pr as usize).copied() {
                Some(p) if p != NONE => e.parent = p,
                _ => {
                    e.flags |= F_DELETED;
                    orphans += 1;
                }
            }
        }
        ix.garbage = orphans;
        ix.rec_count = rec_first.len() as u32;
        drop((rec_first, parent_rec));
        ix.rebuild_rec_index(&records);
        drop(records);

        ix.fold_raw_names(&tmp);
        drop(tmp);
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
        self.fold_samples.shrink_to_fit();
        self.fold_extra.shrink_to_fit();
        self.fold_dead.shrink_to_fit();
        self.created.shrink_to_fit();
        self.seqs.shrink_to_fit();
        self.rec_blocks.shrink_to_fit();
        self.late_records.shrink_to_fit();
        for r in &mut self.ranks.by {
            r.shrink_to_fit();
        }
    }

    // ---- record lookup -----------------------------------------------------------

    /// Rebuilds the record lookup from every entry's record number: the
    /// block index over the record-sorted prefix of the entries, and
    /// `late_records` / `late_links` for the entries after it.
    pub(crate) fn rebuild_rec_index(&mut self, records: &[u32]) {
        let n = self.entries.len();
        debug_assert_eq!(records.len(), n);
        let sorted = 1 + records
            .windows(2)
            .position(|w| w[0] > w[1])
            .unwrap_or(n.saturating_sub(1));
        let sorted = sorted.min(n);
        self.rec_sorted = sorted as u32;
        let max = records[..sorted].last().map_or(0, |&r| r as usize);
        self.rec_blocks = Vec::with_capacity(max / 64 + 1);
        let mut i = 0usize;
        for b in 0..=max / 64 {
            while i < sorted && (records[i] as usize) < b * 64 {
                i += 1;
            }
            self.rec_blocks.push(i as u32);
        }
        for (e, &r) in self.entries.iter_mut().zip(records) {
            e.len_rec = Entry::len_rec(e.name_len(), r);
        }
        self.late_records = records[sorted..].to_vec();
        self.late_links.clear();
        self.more_links.clear();
        for i in sorted..n {
            if !self.entries[i].is_deleted() {
                self.link_late(i as u32);
            }
        }
        // `clear` keeps the capacity: after a compaction that had many late
        // entries, that's tens of MB of empty table for as long as it runs.
        self.late_links.shrink_to_fit();
        self.more_links.shrink_to_fit();
        if let Some(&r) = records.iter().max() {
            self.rec_count = self.rec_count.max(r + 1);
        }
    }

    fn link_late(&mut self, i: u32) {
        if let Some(prev) = self.late_links.insert(self.record(i), i) {
            self.more_links.insert(i, prev);
        }
    }

    /// MFT record number of entry `i`.
    pub fn record(&self, i: u32) -> u32 {
        if i >= self.rec_sorted {
            return self.late_records[(i - self.rec_sorted) as usize];
        }
        // The last block starting at or before `i` (empty blocks share starts).
        let b = self.rec_blocks.partition_point(|&s| s <= i) - 1;
        b as u32 * 64 + self.entries[i as usize].rec_lo()
    }

    /// Every entry's record number (one pass, for saving and compaction).
    pub fn records(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.entries.len());
        let sorted = self.rec_sorted as usize;
        for (b, &lo) in self.rec_blocks.iter().enumerate() {
            let hi = self.rec_blocks.get(b + 1).map_or(sorted, |&h| h as usize);
            out.extend(
                self.entries[lo as usize..hi]
                    .iter()
                    .map(|e| b as u32 * 64 + e.rec_lo()),
            );
        }
        out.extend_from_slice(&self.late_records);
        out
    }

    /// The record-sorted entries of `record`.
    fn sorted_range(&self, record: u32) -> std::ops::Range<usize> {
        let b = record as usize / 64;
        let Some(&lo) = self.rec_blocks.get(b) else {
            return 0..0;
        };
        let hi = self.rec_blocks.get(b + 1).map_or(self.rec_sorted, |&h| h) as usize;
        let block = &self.entries[lo as usize..hi];
        let low = record % 64;
        let s = block.partition_point(|e| e.rec_lo() < low);
        let len = block[s..].iter().take_while(|e| e.rec_lo() == low).count();
        lo as usize + s..lo as usize + s + len
    }

    /// Entries ever added past the sorted prefix (compaction re-sorts them).
    pub fn late_count(&self) -> usize {
        self.entries.len() - self.rec_sorted as usize
    }

    // ---- folding -------------------------------------------------------------

    /// Rebuilds both name arenas from the current names (in parallel): one
    /// fold record per entry in entry order, and the original name stored
    /// separately only when it differs from the folded one.
    pub fn rebuild_fold(&mut self) {
        let me = &*self;
        let parts = fold_parts(me.entries.len(), me.fold_mode, |i| me.name(i as u32));
        self.install_fold(parts);
    }

    /// Builds the name arenas while each `name_ref` is still an offset into
    /// `raw`, a UTF-8 buffer of all names (during a build or load).
    pub(crate) fn fold_raw_names(&mut self, raw: &[u8]) {
        let entries = &self.entries;
        let parts = fold_parts(entries.len(), self.fold_mode, |i| {
            let e = &entries[i];
            // SAFETY: `raw` is UTF-8, cut only at char boundaries.
            unsafe {
                std::str::from_utf8_unchecked(
                    &raw[e.name_ref as usize..e.name_ref as usize + e.name_len()],
                )
            }
        });
        self.install_fold(parts);
    }

    fn install_fold(&mut self, parts: Vec<FoldPart>) {
        let (mut fold, mut samples, mut names) = (Vec::new(), Vec::new(), Vec::new());
        fold.reserve_exact(parts.iter().map(|p| p.fold.len()).sum());
        names.reserve_exact(parts.iter().map(|p| p.names.len()).sum());
        samples.reserve_exact(self.entries.len().div_ceil(64));
        let mut i = 0usize;
        for p in parts {
            let (fb, nb) = (fold.len() as u32, names.len() as u32);
            samples.extend(p.samples.iter().map(|s| s + fb));
            fold.extend_from_slice(&p.fold);
            names.extend_from_slice(&p.names);
            for (fo, no) in p.refs {
                let e = &mut self.entries[i];
                if no == NONE {
                    e.flags |= F_NAME_IN_FOLD;
                    e.name_ref = fo + fb;
                } else {
                    e.flags &= !F_NAME_IN_FOLD;
                    e.name_ref = no + nb;
                    names[e.name_ref as usize..e.name_ref as usize + 4]
                        .copy_from_slice(&(fo + fb).to_le_bytes());
                }
                i += 1;
            }
        }
        self.fold = fold;
        self.fold_samples = samples;
        self.names = names;
        self.fold_records = self.entries.len() as u32;
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
            if self
                .fold_dead
                .get(rec / 64)
                .is_some_and(|w| w >> (rec % 64) & 1 == 1)
            {
                NONE
            } else {
                rec as u32
            }
        } else {
            self.fold_extra
                .get(rec - self.fold_base as usize)
                .copied()
                .unwrap_or(NONE)
        }
    }

    /// Number of the fold record starting at byte `off`.
    fn fold_record_at(&self, off: usize) -> usize {
        let k = self
            .fold_samples
            .partition_point(|&s| s as usize <= off)
            .saturating_sub(1);
        let s = self.fold_samples.get(k).map_or(0, |&s| s as usize);
        k * 64 + count_zeros(&self.fold[s..off])
    }

    /// Byte offset of entry `i`'s folded name.
    #[inline]
    pub fn fold_off(&self, i: u32) -> usize {
        let e = &self.entries[i as usize];
        if e.flags & F_NAME_IN_FOLD != 0 {
            e.name_ref as usize
        } else {
            let at = e.name_ref as usize;
            u32::from_le_bytes(self.names[at..at + 4].try_into().unwrap()) as usize
        }
    }

    fn kill_fold_of(&mut self, i: u32) {
        let rec = self.fold_record_at(self.fold_off(i));
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
                self.ranks.by[k] = Packed::default();
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
        let live: Vec<u32> = (0..n as u32)
            .into_par_iter()
            .filter(|&i| self.is_live(i))
            .collect();
        let to_rank = |order: &[u32]| {
            let mut rank = Packed::new(n, order.len());
            for (r, &i) in order.iter().enumerate() {
                rank.set(i as usize, r as u32);
            }
            rank
        };

        let keys: Vec<&[u8]> = (0..n as u32)
            .into_par_iter()
            .map(|i| self.fold_name(i))
            .collect();
        let mut order = live.clone();
        order.par_sort_unstable_by(|&a, &b| {
            keys[a as usize]
                .cmp(keys[b as usize])
                .then_with(|| self.name(a).cmp(self.name(b)))
                .then(a.cmp(&b))
        });
        drop(keys);
        // Plain u32s while the other orders tie-break on them; packed after.
        let mut name = vec![NONE; n];
        for (r, &i) in order.iter().enumerate() {
            name[i as usize] = r as u32;
        }

        let on = self.opts.fast_sort;
        let (size, (modified, path)) = rayon::join(
            || {
                if !on[RankKey::Size as usize] {
                    return Packed::default();
                }
                let mut o = live.clone();
                o.par_sort_unstable_by_key(|&i| (self.entries[i as usize].size, name[i as usize]));
                to_rank(&o)
            },
            || {
                rayon::join(
                    || {
                        if !on[RankKey::Modified as usize] {
                            return Packed::default();
                        }
                        let mut o = live.clone();
                        o.par_sort_unstable_by_key(|&i| {
                            (self.entries[i as usize].modified, name[i as usize])
                        });
                        to_rank(&o)
                    },
                    || {
                        if on[RankKey::Path as usize] {
                            self.path_rank(&live, &name)
                        } else {
                            Packed::default()
                        }
                    },
                )
            },
        );
        drop(live);
        Ranks {
            by: [to_rank(&order), size, modified, path],
            path_stale: false,
            dirty: 0,
        }
    }

    /// Path order = (containing folder in depth-first order with siblings by
    /// name, then name). Comparing full paths component-wise gives exactly
    /// this order, which is what the fallback comparison does.
    fn path_rank(&self, live: &[u32], name: &[u32]) -> Packed {
        let n = self.entries.len();
        // Directory children, sorted by name rank.
        let mut dirs: Vec<(u32, u32, u32)> = live
            .iter()
            .filter(|&&i| {
                self.entries[i as usize].is_dir() && self.entries[i as usize].parent != NONE
            })
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
                let end = dirs[start..]
                    .iter()
                    .position(|x| x.0 != d)
                    .map_or(dirs.len(), |p| start + p);
                for c in dirs[start..end].iter().rev() {
                    stack.push(c.2);
                }
            }
        }
        let mut o = live.to_vec();
        o.par_sort_unstable_by_key(|&i| {
            let p = self.entries[i as usize].parent;
            let pk = if p == NONE {
                0u64
            } else {
                dfs[p as usize] as u64 + 1
            };
            (pk, name[i as usize])
        });
        let mut rank = Packed::new(n, o.len());
        for (r, &i) in o.iter().enumerate() {
            rank.set(i as usize, r as u32);
        }
        rank
    }

    fn unrank(&mut self, i: u32, keys: &[RankKey]) {
        for &k in keys {
            let r = &mut self.ranks.by[k as usize];
            if r.get(i as usize) != NONE {
                r.set(i as usize, NONE);
                self.ranks.dirty += 1;
            }
        }
    }

    /// Whether enough ranks went stale that recomputing is worthwhile.
    pub fn ranks_need_refresh(&self) -> bool {
        self.ranks_missing()
            || self.ranks.path_stale
            || self.ranks.dirty * 200 > self.entries.len().max(1)
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
        let (at, len) = (e.name_ref as usize, e.name_len());
        let bytes = if e.flags & F_NAME_IN_FOLD != 0 {
            &self.fold[at..at + len]
        } else {
            &self.names[at + 4..at + 4 + len]
        };
        unsafe { std::str::from_utf8_unchecked(bytes) }
    }

    #[inline]
    pub fn fold_name(&self, i: u32) -> &[u8] {
        let e = &self.entries[i as usize];
        if e.flags & F_NAME_IN_FOLD != 0 {
            return &self.fold[e.name_ref as usize..e.name_ref as usize + e.name_len()];
        }
        let start = self.fold_off(i);
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
        self.excl
            .get(i as usize / 64)
            .is_some_and(|w| w >> (i % 64) & 1 == 1)
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
                let samples = &self.fold_samples;
                // `pos` is the start of fold record `rec`.
                let (mut pos, mut rec) = (start, self.fold_record_at(start));
                while pos < end {
                    let Some(off) = finder.find(&hay[pos..end]) else {
                        break;
                    };
                    let at = pos + off;
                    // Number the record containing `at` by counting `\0`s, from
                    // `pos` when that's in the same 64-record block, else from
                    // the block's sample (found by galloping forward).
                    let (mut lo, mut step) = (rec / 64, 1usize);
                    while lo + step < samples.len() && samples[lo + step] as usize <= at {
                        lo += step;
                        step *= 2;
                    }
                    let hi = (lo + step + 1).min(samples.len());
                    let k = lo + samples[lo..hi].partition_point(|&s| s as usize <= at) - 1;
                    let (from, base) = if k == rec / 64 {
                        (pos, rec)
                    } else {
                        (samples[k] as usize, k * 64)
                    };
                    rec = base + count_zeros(&hay[from..at]);
                    let owner = self.fold_owner(rec);
                    if owner != NONE {
                        out.push(owner);
                    }
                    pos = memchr::memchr(0, &hay[at..end]).map_or(end, |p| at + p + 1);
                    rec += 1;
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
            .filter(|&i| {
                self.entries[i as usize].is_dir()
                    && self.fold_name(i) == last
                    && self.fold_path(i) == path
            })
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
        let max_d = depth
            .iter()
            .filter(|&&d| d != u16::MAX)
            .max()
            .copied()
            .unwrap_or(0) as usize;
        let mut levels: Vec<Vec<u32>> = vec![Vec::new(); max_d + 1];
        for (i, &d) in depth.iter().enumerate() {
            if d != u16::MAX && self.entries[i].is_live() {
                levels[d as usize].push(i as u32);
            }
        }
        let mut flags = vec![0u16; n];
        for level in &levels {
            let computed: Vec<(u32, u16)> = level
                .par_iter()
                .map(|&i| (i, self.rule_flags(i, |p| flags[p as usize])))
                .collect();
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
        let pf = if e.parent == NONE {
            0
        } else {
            parent_flags(e.parent)
        };
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
        if r.excludes(e.is_dir(), e.attrs(), self.fold_name(i), &mut || {
            self.fold_path(i)
        }) {
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
        let next = self.rec_count.max(ntfs::FIRST_USER_RECORD);
        let map = self.id_map.get_or_insert_with(HashMap::new);
        if let Some(&r) = map.get(&id) {
            return Some(r);
        }
        if !create {
            return None;
        }
        map.insert(id, next);
        self.rec_count = next + 1;
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

    /// Live entries (hard links) of `record`, newest additions first.
    pub fn links(&self, record: u32) -> impl Iterator<Item = u32> + '_ {
        let mut cur = self.late_links.get(&record).copied().unwrap_or(NONE);
        let late = std::iter::from_fn(move || {
            if cur == NONE {
                return None;
            }
            let c = cur;
            cur = self.more_links.get(&c).copied().unwrap_or(NONE);
            Some(c)
        });
        let sorted = self
            .sorted_range(record)
            .map(|i| i as u32)
            .filter(|&i| self.is_live(i));
        late.chain(sorted)
    }

    pub fn dir_entry(&self, record: u32) -> Option<u32> {
        self.links(record)
            .find(|&i| self.entries[i as usize].is_dir())
            .or_else(|| self.links(record).next())
    }

    pub fn find_link(&self, record: u32, parent: u32, name: &str) -> Option<u32> {
        self.links(record)
            .find(|&i| self.entries[i as usize].parent == parent && self.name(i) == name)
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

    /// Appends a name owned by entry `owner`; returns (name_ref, name_len,
    /// stored_in_fold).
    fn push_name(&mut self, name: &str, owner: u32) -> (u32, usize, bool) {
        let len = name.len().min(MAX_NAME);
        let name = &name[..name.floor_char_boundary(len)];
        Self::headroom(&mut self.fold, name.len() * 3 + 1);
        Self::headroom(&mut self.fold_extra, 1);
        let rec = self.fold_records as usize;
        let at = self.fold.len();
        if rec.is_multiple_of(64) {
            Self::headroom(&mut self.fold_samples, 1);
            self.fold_samples.push(at as u32);
        }
        self.fold_records += 1;
        fold_into_mode(name, &mut self.fold, self.fold_mode);
        let in_fold = &self.fold[at..] == name.as_bytes();
        self.fold.push(0);
        // Records appended past the base are owned per `fold_extra`.
        while self.fold_extra.len() + (self.fold_base as usize) < rec {
            self.fold_extra.push(NONE);
        }
        self.fold_extra.push(owner);
        let name_ref = if in_fold {
            at as u32
        } else {
            Self::headroom(&mut self.names, name.len() + 4);
            let o = self.names.len() as u32;
            self.names.extend_from_slice(&(at as u32).to_le_bytes());
            self.names.extend_from_slice(name.as_bytes());
            o
        };
        (name_ref, name.len(), in_fold)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_link(
        &mut self,
        record: u32,
        seq: u16,
        parent: u32,
        name: &str,
        attrs: u32,
        size: u64,
        modified: i64,
        created: i64,
    ) -> u32 {
        let is_dir = attrs & ntfs::usn::FILE_ATTRIBUTE_DIRECTORY != 0;
        let idx = self.entries.len() as u32;
        let (name_ref, name_len, in_fold) = self.push_name(name, idx);
        Self::headroom(&mut self.entries, 1);
        for r in &mut self.ranks.by {
            // Only extend rank arrays that are in step with the entries
            // (they're empty until computed after a load).
            if r.len() == idx as usize && !r.is_empty() {
                r.push_none();
            }
        }
        self.ranks.dirty += 1;
        self.rec_count = self.rec_count.max(record + 1);
        // Still in record order (always so for folder indexes, whose records
        // only grow): extend the sorted prefix instead of the late links.
        let sorted = self.rec_sorted == idx && (idx == 0 || self.record(idx - 1) <= record);
        self.entries.push(Entry {
            size: if is_dir { 0 } else { size },
            modified,
            parent,
            name_ref,
            len_rec: Entry::len_rec(name_len, record),
            flags: (if is_dir { F_DIR } else { 0 })
                | (if in_fold { F_NAME_IN_FOLD } else { 0 })
                | pack_attrs(attrs),
        });
        if self.seqs.len() == idx as usize {
            Self::headroom(&mut self.seqs, 1);
            self.seqs.push(seq);
        }
        if self.opts.created && self.created.len() == idx as usize {
            Self::headroom(&mut self.created, 1);
            self.created.push(created);
        }
        if sorted {
            self.rec_sorted += 1;
            while self.rec_blocks.len() <= record as usize / 64 {
                Self::headroom(&mut self.rec_blocks, 1);
                self.rec_blocks.push(idx);
            }
        } else {
            Self::headroom(&mut self.late_records, 1);
            self.late_records.push(record);
            self.link_late(idx);
        }
        if !is_dir {
            self.add_size_up(parent, size as i64);
        }
        self.apply_rules_to_new(idx);
        self.generation += 1;
        idx
    }

    pub fn seq(&self, i: u32) -> u16 {
        self.seqs.get(i as usize).copied().unwrap_or(0)
    }

    pub fn remove_entry(&mut self, i: u32) {
        let e = self.entries[i as usize];
        if e.is_deleted() {
            return;
        }
        if i >= self.rec_sorted {
            // Unlink from the record's chain of late links.
            let rec = self.record(i);
            let next = self.more_links.remove(&i);
            if self.late_links.get(&rec) == Some(&i) {
                match next {
                    Some(n) => self.late_links.insert(rec, n),
                    None => self.late_links.remove(&rec),
                };
            } else if let Some(p) = self
                .more_links
                .iter()
                .find(|&(_, &n)| n == i)
                .map(|(&p, _)| p)
            {
                match next {
                    Some(n) => self.more_links.insert(p, n),
                    None => self.more_links.remove(&p),
                };
            }
        }
        self.add_size_up(e.parent, -(e.size as i64));
        self.kill_fold_of(i);
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
            self.kill_fold_of(i);
            let (name_ref, len, in_fold) = self.push_name(name, i);
            let m = &mut self.entries[i as usize];
            m.name_ref = name_ref;
            m.len_rec = (m.len_rec & !0x3FF) | len as u16;
            m.flags = if in_fold {
                m.flags | F_NAME_IN_FOLD
            } else {
                m.flags & !F_NAME_IN_FOLD
            };
        }
        self.unrank(
            i,
            &[
                RankKey::Name,
                RankKey::Size,
                RankKey::Modified,
                RankKey::Path,
            ],
        );
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
        // Survivors, back in record order (stable, so hard links keep theirs).
        let mut keep: Vec<u32> = (0..n as u32).filter(|&i| alive[i as usize] == 1).collect();
        let records = self.records();
        keep.par_sort_by_key(|&i| records[i as usize]);
        let new_records: Vec<u32> = keep.iter().map(|&i| records[i as usize]).collect();
        drop(records);
        let mut remap = vec![NONE; n];
        for (k, &i) in keep.iter().enumerate() {
            remap[i as usize] = k as u32;
        }
        // Names stay where they are for now; rebuild_fold below repacks them.
        let mut new_entries: Vec<Entry> = keep.iter().map(|&i| self.entries[i as usize]).collect();
        let new_created: Vec<i64> = keep
            .iter()
            .filter_map(|&i| self.created.get(i as usize).copied())
            .collect();
        let new_seqs: Vec<u16> = keep.iter().map(|&i| self.seq(i)).collect();
        drop(keep);
        for e in &mut new_entries {
            if e.parent != NONE {
                e.parent = remap[e.parent as usize];
            }
        }
        self.recent = self
            .recent
            .drain()
            .filter(|&(i, _t)| remap[i as usize] != NONE)
            .map(|(i, t)| (remap[i as usize], t))
            .collect();
        self.root = if self.root == NONE {
            NONE
        } else {
            remap[self.root as usize]
        };
        // The name arenas still hold the old layout, which name() reads by
        // the (copied) per-entry offsets; rebuild_fold repacks them.
        self.entries = new_entries;
        self.created = new_created;
        self.seqs = new_seqs;
        self.garbage = 0;
        self.rebuild_rec_index(&new_records);
        drop(new_records);
        self.rebuild_excl();
        self.rebuild_fold();
        self.compute_ranks();
        self.shrink();
        self.epoch += 1;
        self.generation += 1;
    }

    fn map_bytes(&self) -> usize {
        (self.fold_samples.capacity()
            + self.fold_extra.capacity()
            + self.rec_blocks.capacity()
            + self.late_records.capacity())
            * 4
            + self.fold_dead.capacity() * 8
            + self.excl.capacity() * 8
            + (self.more_links.capacity() + self.late_links.capacity()) * 9
    }

    fn entry_bytes(&self) -> usize {
        self.entries.capacity() * size_of::<Entry>() + self.seqs.capacity() * 2
    }

    fn rank_bytes(&self) -> usize {
        self.ranks.by.iter().map(Packed::capacity_bytes).sum()
    }

    pub fn memory_bytes(&self) -> usize {
        self.entry_bytes()
            + self.names.capacity()
            + self.fold.capacity()
            + self.created.capacity() * 8
            + self.map_bytes()
            + self.rank_bytes()
    }

    pub fn memory_breakdown(&self) -> String {
        let mb = |b: usize| format!("{:.0}", b as f64 / 1048576.0);
        format!(
            "entries {} MB, names {} MB, folded {} MB, created {} MB, maps {} MB, ranks {} MB",
            mb(self.entry_bytes()),
            mb(self.names.capacity()),
            mb(self.fold.capacity()),
            mb(self.created.capacity() * 8),
            mb(self.map_bytes()),
            mb(self.rank_bytes()),
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
        let mut s = Scan {
            has_sizes: true,
            ..Default::default()
        };
        let max = items.iter().map(|i| i.0).max().unwrap_or(5).max(5) as usize;
        s.meta = vec![Meta::default(); max + 1];
        s.meta[5] = Meta {
            in_use: true,
            is_dir: true,
            ..Default::default()
        };
        s.names.push(ScanName {
            record: 5,
            parent: 5,
            off: 0,
            len: 1,
        });
        s.strings.push(b'.');
        for &(r, p, n, d, size) in items {
            s.meta[r as usize] = Meta {
                in_use: true,
                is_dir: d,
                size,
                modified: r as i64,
                created: 1000 - r as i64,
                ..Default::default()
            };
            s.names.push(ScanName {
                record: r,
                parent: p,
                off: s.strings.len() as u32,
                len: n.len() as u32,
            });
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
        (0..ix.entries.len() as u32)
            .find(|&i| ix.is_live(i) && ix.name(i) == n)
            .unwrap()
    }

    #[test]
    fn builds_paths_and_sizes() {
        let ix = sample();
        assert_eq!(
            ix.path(by_name(&ix, "Report.PDF")),
            r"C:\Users\Me\Report.PDF"
        );
        assert_eq!(ix.path(ix.root), r"C:\");
        assert_eq!({ ix.entries[by_name(&ix, "Users") as usize].size }, 1024);
        assert_eq!({ ix.entries[ix.root as usize].size }, 1224);
        assert!(!(0..ix.entries.len() as u32).any(|i| ix.is_live(i) && ix.name(i) == "$UsnJrnl"));
        assert!(!(0..ix.entries.len() as u32).any(|i| ix.is_live(i) && ix.name(i) == "orphan.txt"));
        assert_eq!(ix.fold_name(by_name(&ix, "Report.PDF")), b"report.pdf");
        assert_eq!(ix.depth(by_name(&ix, "Report.PDF")), 3);
        assert_eq!(ix.dirs_with_path(br"c:\users\me"), vec![by_name(&ix, "Me")]);
        assert_eq!(
            ix.find_child(by_name(&ix, "Me"), "notes.txt"),
            Some(by_name(&ix, "notes.txt"))
        );
        assert_eq!(ix.child_counts()[by_name(&ix, "Me") as usize], 2);
    }

    #[test]
    fn ranks_order_by_each_key() {
        let ix = sample();
        let order = |k: RankKey| {
            let mut v: Vec<u32> = (0..ix.entries.len() as u32)
                .filter(|&i| ix.is_live(i))
                .collect();
            v.sort_by_key(|&i| ix.ranks.get(k, i));
            v.iter()
                .map(|&i| ix.name(i).to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            order(RankKey::Name),
            [
                "C:",
                "Me",
                "notepad.exe",
                "notes.txt",
                "Report.PDF",
                "Users",
                "Windows"
            ]
        );
        assert_eq!(
            order(RankKey::Size),
            [
                "notes.txt",
                "notepad.exe",
                "Windows",
                "Report.PDF",
                "Me",
                "Users",
                "C:"
            ]
        );
        // Path: by containing folder in tree order, then name.
        assert_eq!(
            order(RankKey::Path),
            [
                "C:",
                "Users",
                "Windows",
                "Me",
                "notes.txt",
                "Report.PDF",
                "notepad.exe"
            ]
        );
    }

    #[test]
    fn sizes_rank_ties_break_by_name() {
        let ix = sample();
        let mut v: Vec<u32> = (0..ix.entries.len() as u32)
            .filter(|&i| ix.is_live(i))
            .collect();
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
        assert_eq!(
            ix.path(by_name(&ix, "notes.txt")),
            r"C:\Windows\You\notes.txt"
        );
        assert_eq!({ ix.entries[by_name(&ix, "Users") as usize].size }, 0);
        assert_eq!({ ix.entries[win as usize].size }, 1224);
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
        assert_eq!({ ix.entries[ix.root as usize].size }, 1234);
        ix.note_recent(new, 42);
        ix.remove_record(32);
        assert_eq!({ ix.entries[ix.root as usize].size }, 234);
        let users = by_name(&ix, "Users");
        let epoch = ix.epoch;
        ix.remove_entry(users); // children become unreachable and are compacted away
        ix.compact();
        assert_eq!(ix.garbage, 0);
        assert_eq!(ix.epoch, epoch + 1);
        let names: Vec<_> = (0..ix.entries.len() as u32)
            .map(|i| ix.name(i).to_string())
            .collect();
        assert_eq!(names, vec!["C:", "Windows", "notepad.exe"]);
        assert_eq!(
            ix.path(by_name(&ix, "notepad.exe")),
            r"C:\Windows\notepad.exe"
        );
        assert_eq!(ix.links(35).count(), 1);
        assert!(ix.recent.is_empty());
    }

    #[test]
    fn exclude_rules_hide_subtrees_and_apply_to_new_entries() {
        let mut ix = sample();
        let cfg = Config {
            exclude_folders: vec![r"C:\Users\Me".into()],
            exclude_files: vec!["*.exe".into()],
            ..Default::default()
        };
        ix.apply_rules(cfg.rules('C'));
        let vis: Vec<_> = (0..ix.entries.len() as u32)
            .filter(|&i| ix.is_visible(i))
            .map(|i| ix.name(i).to_string())
            .collect();
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
        cfg.volumes.insert(
            "C".into(),
            crate::config::VolumeConfig {
                include: true,
                include_only: vec![r"C:\Users\Me".into()],
            },
        );
        ix.apply_rules(cfg.rules('C'));
        let vis: Vec<_> = (0..ix.entries.len() as u32)
            .filter(|&i| ix.is_visible(i))
            .map(|i| ix.name(i).to_string())
            .collect();
        assert_eq!(vis, vec!["C:", "Users", "Me", "Report.PDF", "notes.txt"]);
    }

    #[test]
    fn fold_mode_switch_and_id_map() {
        let mut ix = Index::from_scan('C', &scan_of(&[(30, 5, "Café", false, 1)]));
        assert_eq!(ix.fold_name(by_name(&ix, "Café")), "café".as_bytes());
        ix.set_fold_mode(FoldMode::StripDiacritics);
        assert_eq!(ix.fold_name(by_name(&ix, "Café")), b"cafe");
        let a = ix
            .map_id(0xABCD_0000_0000_0000_0000_0000_0000_0001, true)
            .unwrap();
        let b = ix
            .map_id(0xABCD_0000_0000_0000_0000_0000_0000_0002, true)
            .unwrap();
        assert_ne!(a, b);
        assert_eq!(
            ix.map_id(0xABCD_0000_0000_0000_0000_0000_0000_0001, false),
            Some(a)
        );
        assert!(a >= ntfs::FIRST_USER_RECORD);
    }

    #[test]
    fn remove_subtree() {
        let mut ix = sample();
        ix.remove_subtree(by_name(&ix, "Users"));
        let live: Vec<_> = (0..ix.entries.len() as u32)
            .filter(|&i| ix.is_live(i))
            .map(|i| ix.name(i).to_string())
            .collect();
        assert_eq!(live, vec!["C:", "Windows", "notepad.exe"]);
        assert_eq!({ ix.entries[ix.root as usize].size }, 200);
    }

    #[test]
    fn packed_ranks() {
        let mut p = Packed::new(100, 70);
        assert_eq!(p.len(), 100);
        assert!((0..100).all(|i| p.get(i) == NONE));
        for i in 0..100 {
            p.set(i, (i * 7 % 70) as u32);
        }
        p.set(13, NONE);
        assert!((0..100).all(|i| p.get(i) == if i == 13 { NONE } else { (i * 7 % 70) as u32 }));
        p.push_none();
        assert_eq!((p.len(), p.get(100), p.get(5000)), (101, NONE, NONE));
        let wide = Packed::new(3, u32::MAX as usize);
        assert_eq!(wide.get(2), NONE);
    }

    fn brute(ix: &Index, needle: &[u8]) -> Vec<u32> {
        (0..ix.entries.len() as u32)
            .filter(|&i| ix.is_live(i) && memchr::memmem::find(ix.fold_name(i), needle).is_some())
            .collect()
    }

    fn check_lookups(ix: &Index) {
        for needle in [&b"file"[..], b"1", b"77", b".txt", b"ren", b"x"] {
            let mut got = ix.name_candidates(needle);
            got.sort_unstable();
            assert_eq!(got, brute(ix, needle), "{:?}", std::str::from_utf8(needle));
        }
        for i in 0..ix.entries.len() as u32 {
            if ix.is_live(i) {
                let r = ix.record(i);
                assert_eq!(r, ix.records()[i as usize]);
                assert!(
                    ix.links(r).any(|l| l == i),
                    "entry {i} not among links of {r}"
                );
                assert!(ix.links(r).all(|l| ix.record(l) == r && ix.is_live(l)));
                assert_eq!(fold_into_vec(ix, i), ix.fold_name(i));
            }
        }
    }

    fn fold_into_vec(ix: &Index, i: u32) -> Vec<u8> {
        let mut v = Vec::new();
        fold_into_mode(ix.name(i), &mut v, ix.fold_mode);
        v
    }

    #[test]
    fn unsorted_scans_hard_links_and_record_lookup() {
        // Out of record order (like a parallel folder walk), with a hard link.
        let mut items: Vec<(u32, u32, String, bool, u64)> = vec![(300, 5, "Dir".into(), true, 0)];
        for k in 0..700u32 {
            let r = 1000 - k;
            items.push((
                r,
                if k % 3 == 0 { 5 } else { 300 },
                format!("File{k}.TXT"),
                false,
                k as u64,
            ));
        }
        items.push((900, 5, "hardlink.txt".into(), false, 100)); // second name of record 900
        let refs: Vec<(u32, u32, &str, bool, u64)> = items
            .iter()
            .map(|(a, b, c, d, e)| (*a, *b, c.as_str(), *d, *e))
            .collect();
        let mut ix = Index::from_scan('C', &scan_of(&refs));
        assert!(ix.records().is_sorted());
        assert_eq!(ix.late_count(), 0);
        assert_eq!(ix.links(900).count(), 2);
        assert_eq!(ix.path(by_name(&ix, "File5.TXT")), r"C:\Dir\File5.TXT");
        check_lookups(&ix);

        // Renames append fold records; adds go late (reused record) or extend
        // the sorted prefix (new highest record).
        let dir = by_name(&ix, "Dir");
        for k in (0..700).step_by(7) {
            let i = by_name(&ix, &format!("File{k}.TXT"));
            ix.rename_entry(i, dir, &format!("ren{k}.txt"));
        }
        let late = ix.add_link(200, 1, dir, "Late.txt", 0x20, 1, 0, 0);
        let link = ix.add_link(900, 1, dir, "another link.txt", 0x20, 1, 0, 0);
        assert_eq!(ix.late_count(), 2);
        let top = ix.add_link(5000, 1, dir, "top.txt", 0x20, 1, 0, 0);
        assert_eq!(
            ix.late_count(),
            3,
            "after late entries the prefix stops growing"
        );
        assert_eq!(ix.links(900).count(), 3);
        ix.remove_entry(link);
        ix.remove_record(1000);
        assert_eq!(ix.links(900).count(), 2);
        assert_eq!(ix.links(200).collect::<Vec<_>>(), vec![late]);
        assert_eq!(ix.links(5000).collect::<Vec<_>>(), vec![top]);
        check_lookups(&ix);

        ix.compact();
        assert_eq!(ix.late_count(), 0);
        assert!(ix.records().is_sorted());
        assert_eq!(ix.path(by_name(&ix, "Late.txt")), r"C:\Dir\Late.txt");
        assert_eq!(ix.path(by_name(&ix, "ren14.txt")), r"C:\Dir\ren14.txt");
        assert_eq!(ix.links(1000).count(), 0);
        check_lookups(&ix);

        // Folder indexes: records only grow, so adds keep the prefix sorted.
        let r = ix.rec_count;
        ix.add_link(r, 0, dir, "walked.txt", 0x20, 1, 0, 0);
        assert_eq!(ix.late_count(), 0);
        check_lookups(&ix);
    }
}
