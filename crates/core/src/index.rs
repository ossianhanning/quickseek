//! In-memory index of one volume.
//!
//! Layout is chosen for the search loop, not for convenience:
//! * every folded (lowercased) name lives in one contiguous byte arena, each
//!   terminated by `\0`, so a substring query is a single SIMD `memmem` pass
//!   over a few tens of MB instead of millions of small string compares;
//! * entries are fixed-size POD structs with parent *indices* (not paths), so
//!   paths are only built for the results actually shown;
//! * `rec_map` maps an MFT record number straight to its entry, which is what
//!   makes applying USN journal records O(1) without asking the OS for paths.

use bytemuck::{Pod, Zeroable};
use rayon::prelude::*;

use crate::fold::fold_into;
use crate::ntfs::{self, Scan};

pub const NONE: u32 = u32::MAX;

pub const F_DIR: u16 = 1;
pub const F_DELETED: u16 = 2;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug)]
pub struct Entry {
    /// File size, or total size of everything below a directory.
    pub size: u64,
    pub modified: i64,
    pub created: i64,
    pub record: u32,
    /// Entry index of the parent directory (`NONE` for the root).
    pub parent: u32,
    pub name_off: u32,
    /// Index into `fold_starts` / `fold_owner`.
    pub fold_rec: u32,
    pub attrs: u32,
    /// Next entry for the same MFT record (hard links), or `NONE`.
    pub next_link: u32,
    pub name_len: u16,
    pub flags: u16,
    pub seq: u16,
    pub _pad: u16,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.flags & F_DIR != 0
    }
    pub fn is_deleted(&self) -> bool {
        self.flags & F_DELETED != 0
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
    pub fold_owner: Vec<u32>,
    /// MFT record → first entry of that record.
    pub rec_map: Vec<u32>,
    /// Position of each entry in name order (`NONE` if added/renamed since the
    /// last ranking). Lets name sorts compare integers instead of strings.
    pub name_rank: Vec<u32>,
    pub root: u32,
    pub has_sizes: bool,
    pub garbage: usize,
    /// Bumped on every mutation, so UIs know when to re-run a query.
    pub generation: u64,
    pub journal_id: u64,
    pub next_usn: i64,
    pub volume_serial: u32,
}

impl Index {
    pub fn from_scan(letter: char, scan: &Scan) -> Index {
        let mut ix = Index { letter, has_sizes: scan.has_sizes, root: NONE, ..Default::default() };
        ix.rec_map = vec![NONE; scan.meta.len()];
        ix.entries.reserve(scan.names.len() + 1);
        ix.names.reserve(scan.strings.len() + 4);

        let root_name = format!("{letter}:");
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
            let name = if is_root { root_name.as_str() } else { scan.name(n) };
            let idx = ix.entries.len() as u32;
            ix.entries.push(Entry {
                size: if m.is_dir { 0 } else { m.size },
                modified: m.modified,
                created: m.created,
                record: n.record,
                parent: NONE,
                name_off: ix.names.len() as u32,
                fold_rec: idx,
                attrs: m.attrs | if m.is_dir { ntfs::usn::FILE_ATTRIBUTE_DIRECTORY } else { 0 },
                next_link: ix.rec_map[n.record as usize],
                name_len: name.len().min(u16::MAX as usize) as u16,
                flags: if m.is_dir { F_DIR } else { 0 },
                seq: m.seq,
                _pad: 0,
            });
            ix.names.extend_from_slice(&name.as_bytes()[..name.len().min(u16::MAX as usize)]);
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
                size: 0, modified: 0, created: 0, record: ntfs::ROOT_RECORD, parent: NONE,
                name_off: ix.names.len() as u32, fold_rec: idx, attrs: ntfs::usn::FILE_ATTRIBUTE_DIRECTORY,
                next_link: NONE, name_len: root_name.len() as u16, flags: F_DIR, seq: 0, _pad: 0,
            });
            ix.names.extend_from_slice(root_name.as_bytes());
            if (ntfs::ROOT_RECORD as usize) < ix.rec_map.len() {
                ix.rec_map[ntfs::ROOT_RECORD as usize] = idx;
            }
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
        ix.compute_rank();
        ix.shrink();
        ix
    }

    pub fn shrink(&mut self) {
        self.entries.shrink_to_fit();
        self.names.shrink_to_fit();
        self.fold.shrink_to_fit();
        self.fold_starts.shrink_to_fit();
        self.fold_owner.shrink_to_fit();
        self.rec_map.shrink_to_fit();
        self.name_rank.shrink_to_fit();
    }

    /// Sorts live entries by (folded name, name, index) and stores each one's position.
    pub fn compute_rank(&mut self) {
        let n = self.entries.len();
        let keys: Vec<&[u8]> = (0..n as u32).into_par_iter().map(|i| self.fold_name(i)).collect();
        let mut order: Vec<u32> = (0..n as u32).filter(|&i| self.is_live(i)).collect();
        order.par_sort_unstable_by(|&a, &b| {
            keys[a as usize]
                .cmp(keys[b as usize])
                .then_with(|| self.name(a).cmp(self.name(b)))
                .then(a.cmp(&b))
        });
        drop(keys);
        let mut rank = vec![NONE; n];
        for (r, &i) in order.iter().enumerate() {
            rank[i as usize] = r as u32;
        }
        self.name_rank = rank;
    }

    /// Rebuilds the folded-name arena from scratch (in parallel), one fold
    /// record per entry in entry order.
    pub fn rebuild_fold(&mut self) {
        const CHUNK: usize = 1 << 15;
        let names = &self.names;
        let parts: Vec<(Vec<u8>, Vec<u32>)> = self
            .entries
            .par_chunks(CHUNK)
            .map(|chunk| {
                let mut buf = Vec::with_capacity(chunk.len() * 16);
                let mut starts = Vec::with_capacity(chunk.len());
                for e in chunk {
                    starts.push(buf.len() as u32);
                    let s = unsafe {
                        std::str::from_utf8_unchecked(&names[e.name_off as usize..e.name_off as usize + e.name_len as usize])
                    };
                    fold_into(s, &mut buf);
                    buf.push(0);
                }
                (buf, starts)
            })
            .collect();
        self.fold.clear();
        self.fold_starts.clear();
        for (buf, starts) in parts {
            let base = self.fold.len() as u32;
            self.fold_starts.extend(starts.into_iter().map(|s| s + base));
            self.fold.extend_from_slice(&buf);
        }
        self.fold_owner = (0..self.entries.len() as u32)
            .map(|i| if self.entries[i as usize].is_deleted() { NONE } else { i })
            .collect();
        for (i, e) in self.entries.iter_mut().enumerate() {
            e.fold_rec = i as u32;
        }
    }

    /// Rolls file sizes up into every ancestor directory.
    fn compute_dir_sizes(&mut self) {
        let n = self.entries.len();
        let mut depth = vec![u16::MAX; n];
        for i in 0..n {
            if depth[i] != u16::MAX {
                continue;
            }
            // Walk up until a known depth, then fill back down.
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
        let mut order: Vec<u32> = (0..n as u32).collect();
        order.par_sort_unstable_by_key(|&i| std::cmp::Reverse(depth[i as usize]));
        for &i in &order {
            let e = self.entries[i as usize];
            if e.is_deleted() || e.parent == NONE {
                continue;
            }
            self.entries[e.parent as usize].size += e.size;
        }
    }

    pub fn len_live(&self) -> usize {
        self.entries.len() - self.garbage
    }

    #[inline]
    pub fn name(&self, i: u32) -> &str {
        let e = &self.entries[i as usize];
        unsafe { std::str::from_utf8_unchecked(&self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize]) }
    }

    #[inline]
    pub fn fold_name(&self, i: u32) -> &[u8] {
        let r = self.entries[i as usize].fold_rec as usize;
        let start = self.fold_starts[r] as usize;
        let end = memchr::memchr(0, &self.fold[start..]).map_or(self.fold.len(), |p| start + p);
        &self.fold[start..end]
    }

    fn ancestors(&self, i: u32) -> Vec<u32> {
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
        if chain.len() == 1 {
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

    pub fn is_live(&self, i: u32) -> bool {
        !self.entries[i as usize].is_deleted()
    }

    // ---- mutation (used by journal replay) ---------------------------------

    pub fn links(&self, record: u32) -> impl Iterator<Item = u32> + '_ {
        let mut cur = self.rec_map.get(record as usize).copied().unwrap_or(NONE);
        std::iter::from_fn(move || {
            if cur == NONE {
                return None;
            }
            let c = cur;
            cur = self.entries[c as usize].next_link;
            Some(c)
        })
    }

    pub fn dir_entry(&self, record: u32) -> Option<u32> {
        self.links(record).find(|&i| self.entries[i as usize].is_dir())
            .or_else(|| self.links(record).next())
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
            p = e.parent;
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

    fn push_name(&mut self, name: &str) -> (u32, u16, u32) {
        Self::headroom(&mut self.names, name.len());
        Self::headroom(&mut self.fold, name.len() * 3 + 1);
        Self::headroom(&mut self.fold_starts, 1);
        Self::headroom(&mut self.fold_owner, 1);
        let off = self.names.len() as u32;
        let len = name.len().min(u16::MAX as usize);
        self.names.extend_from_slice(&name.as_bytes()[..len]);
        let fold_rec = self.fold_starts.len() as u32;
        self.fold_starts.push(self.fold.len() as u32);
        fold_into(name, &mut self.fold);
        self.fold.push(0);
        (off, len as u16, fold_rec)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_link(&mut self, record: u32, seq: u16, parent: u32, name: &str, attrs: u32, size: u64, modified: i64, created: i64) -> u32 {
        let is_dir = attrs & ntfs::usn::FILE_ATTRIBUTE_DIRECTORY != 0;
        let (name_off, name_len, fold_rec) = self.push_name(name);
        let idx = self.entries.len() as u32;
        Self::headroom(&mut self.entries, 1);
        Self::headroom(&mut self.name_rank, 1);
        if self.rec_map.len() <= record as usize {
            self.rec_map.resize(record as usize + 1, NONE);
        }
        self.entries.push(Entry {
            size: if is_dir { 0 } else { size },
            modified,
            created,
            record,
            parent,
            name_off,
            fold_rec,
            attrs,
            next_link: self.rec_map[record as usize],
            name_len,
            flags: if is_dir { F_DIR } else { 0 },
            seq,
            _pad: 0,
        });
        self.fold_owner.push(idx);
        self.name_rank.push(NONE);
        self.rec_map[record as usize] = idx;
        if !is_dir {
            self.add_size_up(parent, size as i64);
        }
        self.generation += 1;
        idx
    }

    pub fn remove_entry(&mut self, i: u32) {
        let e = self.entries[i as usize];
        if e.is_deleted() {
            return;
        }
        // Unlink from the record chain.
        let rec = e.record as usize;
        if self.rec_map[rec] == i {
            self.rec_map[rec] = e.next_link;
        } else {
            let mut cur = self.rec_map[rec];
            while cur != NONE {
                let next = self.entries[cur as usize].next_link;
                if next == i {
                    self.entries[cur as usize].next_link = e.next_link;
                    break;
                }
                cur = next;
            }
        }
        self.add_size_up(e.parent, -(e.size as i64));
        self.fold_owner[e.fold_rec as usize] = NONE;
        let m = &mut self.entries[i as usize];
        m.flags |= F_DELETED;
        m.next_link = NONE;
        self.garbage += 1;
        self.generation += 1;
    }

    pub fn remove_record(&mut self, record: u32) {
        let links: Vec<u32> = self.links(record).collect();
        for l in links {
            self.remove_entry(l);
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
            self.fold_owner[e.fold_rec as usize] = NONE;
            let (off, len, fold_rec) = self.push_name(name);
            self.fold_owner.push(i);
            let m = &mut self.entries[i as usize];
            m.name_off = off;
            m.name_len = len;
            m.fold_rec = fold_rec;
            if let Some(r) = self.name_rank.get_mut(i as usize) {
                *r = NONE;
            }
        }
        self.entries[i as usize].parent = new_parent;
        self.generation += 1;
    }

    pub fn set_file_info(&mut self, record: u32, size: u64, modified: i64, created: i64) {
        let links: Vec<u32> = self.links(record).collect();
        for l in links {
            let e = self.entries[l as usize];
            if e.is_dir() {
                self.entries[l as usize].modified = modified;
                continue;
            }
            self.add_size_up(e.parent, size as i64 - e.size as i64);
            let m = &mut self.entries[l as usize];
            m.size = size;
            m.modified = modified;
            if created != 0 {
                m.created = created;
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
        let mut new_names = Vec::with_capacity(self.names.len());
        for i in 0..n {
            if alive[i] != 1 {
                continue;
            }
            remap[i] = new_entries.len() as u32;
            let mut e = self.entries[i];
            let (o, l) = (e.name_off as usize, e.name_len as usize);
            e.name_off = new_names.len() as u32;
            new_names.extend_from_slice(&self.names[o..o + l]);
            new_entries.push(e);
        }
        for e in &mut new_entries {
            if e.parent != NONE {
                e.parent = remap[e.parent as usize];
            }
            e.next_link = NONE;
        }
        self.rec_map.fill(NONE);
        for (i, e) in new_entries.iter_mut().enumerate() {
            let r = e.record as usize;
            if r >= self.rec_map.len() {
                self.rec_map.resize(r + 1, NONE);
            }
            e.next_link = self.rec_map[r];
            self.rec_map[r] = i as u32;
        }
        self.root = if self.root == NONE { NONE } else { remap[self.root as usize] };
        self.entries = new_entries;
        self.names = new_names;
        self.garbage = 0;
        self.rebuild_fold();
        self.compute_rank();
        self.shrink();
        self.generation += 1;
    }

    pub fn memory_bytes(&self) -> usize {
        self.entries.capacity() * size_of::<Entry>()
            + self.names.capacity()
            + self.fold.capacity()
            + (self.fold_starts.capacity() + self.fold_owner.capacity() + self.rec_map.capacity() + self.name_rank.capacity()) * 4
    }

    pub fn memory_breakdown(&self) -> String {
        let mb = |b: usize| format!("{:.0}", b as f64 / 1048576.0);
        format!(
            "entries {} MB, names {} MB, folded {} MB, maps {} MB",
            mb(self.entries.capacity() * size_of::<Entry>()),
            mb(self.names.capacity()),
            mb(self.fold.capacity()),
            mb((self.fold_starts.capacity() + self.fold_owner.capacity() + self.rec_map.capacity() + self.name_rank.capacity()) * 4)
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
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
            s.meta[r as usize] = Meta { in_use: true, is_dir: d, size, modified: r as i64, ..Default::default() };
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

    fn by_name(ix: &Index, n: &str) -> u32 {
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
    }

    #[test]
    fn add_remove_compact() {
        let mut ix = sample();
        let me = by_name(&ix, "Me");
        let new = ix.add_link(50, 1, me, "New.TXT", 0x20, 10, 0, 0);
        assert_eq!(ix.path(new), r"C:\Users\Me\New.TXT");
        assert_eq!(ix.entries[ix.root as usize].size, 1234);
        ix.remove_record(32);
        assert_eq!(ix.entries[ix.root as usize].size, 234);
        let users = by_name(&ix, "Users");
        ix.remove_entry(users); // children become unreachable and are compacted away
        ix.compact();
        assert_eq!(ix.garbage, 0);
        let names: Vec<_> = (0..ix.entries.len() as u32).map(|i| ix.name(i).to_string()).collect();
        assert_eq!(names, vec!["C:", "Windows", "notepad.exe"]);
        assert_eq!(ix.path(by_name(&ix, "notepad.exe")), r"C:\Windows\notepad.exe");
        assert_eq!(ix.links(35).count(), 1);
    }
}
