//! The agent's index of one share: entries by id, each change producing the
//! [`Op`]s the PC applies to its replica.
//!
//! Ids are dense and never reused while the agent runs (one epoch), so the
//! PC can use them as record numbers. Directory sizes aren't kept: the PC
//! adds them up.

use std::collections::HashMap;
use std::fs::Metadata;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use quickseek_remote::wire::{Op, Record};
use quickseek_remote::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_REPARSE_POINT, FIRST_ID,
    ROOT_ID, filetime_of,
};

/// What the PC needs to know about an entry, and which inode it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Meta {
    pub attrs: u32,
    pub size: u64,
    pub modified: i64,
    pub created: i64,
    /// Device (on btrfs: one per subvolume) and inode number: how a btrfs
    /// catch-up ([`crate::btrfs`]) finds the entries of a changed inode.
    pub dev: u64,
    pub ino: u64,
}

/// File attributes for a name and kind (Samba's default: dot files are hidden).
pub fn attrs_of(name: &str, dir: bool, symlink: bool) -> u32 {
    let mut attrs = 0;
    if dir {
        attrs |= FILE_ATTRIBUTE_DIRECTORY;
    }
    if symlink {
        attrs |= FILE_ATTRIBUTE_REPARSE_POINT;
    }
    if name.starts_with('.') {
        attrs |= FILE_ATTRIBUTE_HIDDEN;
    }
    attrs
}

impl Meta {
    pub fn of(name: &str, m: &Metadata) -> Meta {
        let ft = m.file_type();
        #[cfg(unix)]
        let (dev, ino) = {
            use std::os::unix::fs::MetadataExt;
            (m.dev(), m.ino())
        };
        #[cfg(not(unix))]
        let (dev, ino) = (0, 0);
        Meta {
            attrs: attrs_of(name, ft.is_dir(), ft.is_symlink()),
            size: if ft.is_file() { m.len() } else { 0 },
            modified: filetime_of(m.modified()),
            created: filetime_of(m.created()),
            dev,
            ino,
        }
    }

    /// The same entry as far as the PC can tell (not which inode it is).
    pub fn shown_eq(&self, o: &Meta) -> bool {
        self.attrs == o.attrs
            && self.size == o.size
            && self.modified == o.modified
            && self.created == o.created
    }

    pub fn is_dir(&self) -> bool {
        self.attrs & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    /// A directory the walk descends into (not a symlink to one).
    pub fn walks(&self) -> bool {
        self.is_dir() && self.attrs & FILE_ATTRIBUTE_REPARSE_POINT == 0
    }
}

#[derive(Debug, Clone, Default)]
pub struct Node {
    pub parent: u32,
    pub name: Box<str>,
    pub meta: Meta,
    pub children: Vec<u32>,
    pub live: bool,
}

pub struct Tree {
    pub root: PathBuf,
    pub nodes: Vec<Node>,
    live: usize,
    /// Don't produce ops (the first walk: every PC gets that in full anyway).
    pub quiet: bool,
}

impl Tree {
    pub fn new(root: &Path) -> Tree {
        let mut nodes = vec![Node::default(); FIRST_ID as usize];
        nodes[ROOT_ID as usize] = Node {
            parent: ROOT_ID,
            name: "".into(),
            meta: Meta {
                attrs: FILE_ATTRIBUTE_DIRECTORY,
                ..Default::default()
            },
            children: Vec::new(),
            live: true,
        };
        Tree {
            root: root.to_path_buf(),
            nodes,
            live: 0,
            quiet: false,
        }
    }

    /// Entries below the root.
    pub fn len(&self) -> usize {
        self.live
    }

    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// The id the next new entry gets.
    pub fn next_id(&self) -> u32 {
        self.nodes.len() as u32
    }

    pub fn get(&self, id: u32) -> Option<&Node> {
        self.nodes.get(id as usize).filter(|n| n.live)
    }

    pub fn is_dir(&self, id: u32) -> bool {
        self.get(id).is_some_and(|n| n.meta.is_dir())
    }

    pub fn path(&self, id: u32) -> PathBuf {
        let mut names = Vec::new();
        let mut cur = id;
        while cur != ROOT_ID {
            let Some(n) = self.nodes.get(cur as usize) else {
                break;
            };
            names.push(&*n.name);
            cur = n.parent;
            if names.len() > 4096 {
                break;
            }
        }
        let mut p = self.root.clone();
        for n in names.iter().rev() {
            p.push(n);
        }
        p
    }

    pub fn child(&self, dir: u32, name: &str) -> Option<u32> {
        let n = self.get(dir)?;
        n.children
            .iter()
            .copied()
            .find(|&c| &*self.nodes[c as usize].name == name)
    }

    fn record(&self, id: u32) -> Record {
        let n = &self.nodes[id as usize];
        Record {
            id,
            parent: n.parent,
            name: n.name.to_string(),
            attrs: n.meta.attrs,
            size: n.meta.size,
            modified: n.meta.modified,
            created: n.meta.created,
        }
    }

    /// Every folder (the root included) with its path, for a watcher that
    /// follows them all.
    pub fn all_dirs(&self) -> Vec<(u32, PathBuf)> {
        let mut out = vec![(ROOT_ID, self.root.clone())];
        out.extend(
            (FIRST_ID..self.next_id())
                .filter(|&i| self.nodes[i as usize].live && self.nodes[i as usize].meta.walks())
                .map(|i| (i, self.path(i))),
        );
        out
    }

    /// The whole tree for saving: next id, the root's metadata, then every
    /// entry as a record with its device and inode.
    pub fn save_to(&self, out: &mut Vec<u8>) {
        use quickseek_remote::wire::{put, put_i};
        put(out, self.next_id() as u64);
        let r = &self.nodes[ROOT_ID as usize].meta;
        for v in [r.modified, r.created] {
            put_i(out, v);
        }
        put(out, r.dev);
        put(out, r.ino);
        put(out, self.live as u64);
        let mut prev = 0;
        for id in FIRST_ID..self.next_id() {
            let n = &self.nodes[id as usize];
            if !n.live {
                continue;
            }
            let m = &n.meta;
            quickseek_remote::wire::put_record_fields(
                out, id, n.parent, &n.name, m.attrs, m.size, m.modified, m.created, prev,
            );
            put(out, m.dev);
            put(out, m.ino);
            prev = id;
        }
    }

    /// A tree saved by [`Tree::save_to`].
    pub fn load_from(root: &Path, rd: &mut quickseek_remote::wire::Rd) -> std::io::Result<Tree> {
        use quickseek_remote::wire::{bad, get_record};
        let next = rd.u32()?.max(FIRST_ID);
        let mut t = Tree::new(root);
        {
            let r = &mut t.nodes[ROOT_ID as usize].meta;
            r.modified = rd.i()?;
            r.created = rd.i()?;
            r.dev = rd.u()?;
            r.ino = rd.u()?;
        }
        let n = rd.u()? as usize;
        t.nodes.resize(next as usize, Node::default());
        let mut prev = 0;
        for _ in 0..n {
            let rec = get_record(rd, prev)?;
            prev = rec.id;
            if rec.id < FIRST_ID || rec.id >= next {
                return Err(bad("saved index: id"));
            }
            t.nodes[rec.id as usize] = Node {
                parent: rec.parent,
                name: rec.name.into_boxed_str(),
                meta: Meta {
                    attrs: rec.attrs,
                    size: rec.size,
                    modified: rec.modified,
                    created: rec.created,
                    dev: rd.u()?,
                    ino: rd.u()?,
                },
                children: Vec::new(),
                live: true,
            };
        }
        // Children lists, and nothing hanging off a missing folder.
        for id in FIRST_ID..next {
            let p = t.nodes[id as usize].parent;
            if t.nodes[id as usize].live {
                if (p as usize) < t.nodes.len() && t.nodes[p as usize].live {
                    t.nodes[p as usize].children.push(id);
                } else {
                    return Err(bad("saved index: parent"));
                }
            }
        }
        t.live = n;
        Ok(t)
    }

    /// Every entry below the root encoded for a full transfer (ascending
    /// ids, see `wire::put_record_fields`), and how many.
    pub fn encode(&self) -> (u64, Vec<u8>) {
        let mut out = Vec::with_capacity(self.live * 24);
        let (mut n, mut prev) = (0u64, 0u32);
        for id in FIRST_ID..self.next_id() {
            let node = &self.nodes[id as usize];
            if !node.live {
                continue;
            }
            let m = &node.meta;
            quickseek_remote::wire::put_record_fields(
                &mut out,
                id,
                node.parent,
                &node.name,
                m.attrs,
                m.size,
                m.modified,
                m.created,
                prev,
            );
            prev = id;
            n += 1;
        }
        (n, out)
    }

    /// Every entry below the root, ascending ids.
    pub fn records(&self) -> Vec<Record> {
        (FIRST_ID..self.next_id())
            .filter(|&i| self.nodes[i as usize].live)
            .map(|i| self.record(i))
            .collect()
    }

    pub fn create(&mut self, parent: u32, name: &str, meta: Meta, ops: &mut Vec<Op>) -> u32 {
        let id = self.next_id();
        self.nodes.push(Node {
            parent,
            name: name.into(),
            meta,
            children: Vec::new(),
            live: true,
        });
        self.nodes[parent as usize].children.push(id);
        self.live += 1;
        if !self.quiet {
            ops.push(Op::Create(self.record(id)));
        }
        id
    }

    /// Removes `id` and everything below it, the contents first.
    pub fn remove(&mut self, id: u32, ops: &mut Vec<Op>, removed_dirs: &mut Vec<u32>) {
        if id == ROOT_ID || self.get(id).is_none() {
            return;
        }
        let parent = self.nodes[id as usize].parent;
        let siblings = &mut self.nodes[parent as usize].children;
        if let Some(p) = siblings.iter().position(|&c| c == id) {
            siblings.swap_remove(p);
        }
        // Post-order without recursion.
        let mut stack = vec![(id, false)];
        while let Some((n, expanded)) = stack.pop() {
            if !expanded {
                stack.push((n, true));
                for &c in &self.nodes[n as usize].children {
                    stack.push((c, false));
                }
                continue;
            }
            let node = &mut self.nodes[n as usize];
            if node.meta.is_dir() {
                removed_dirs.push(n);
            }
            node.live = false;
            node.children = Vec::new();
            node.name = "".into();
            self.live -= 1;
            if !self.quiet {
                ops.push(Op::Remove { id: n });
            }
        }
    }

    pub fn move_to(&mut self, id: u32, parent: u32, name: &str, ops: &mut Vec<Op>) {
        let old = self.nodes[id as usize].parent;
        if old != parent {
            let siblings = &mut self.nodes[old as usize].children;
            if let Some(p) = siblings.iter().position(|&c| c == id) {
                siblings.swap_remove(p);
            }
            self.nodes[parent as usize].children.push(id);
            self.nodes[id as usize].parent = parent;
        }
        self.nodes[id as usize].name = name.into();
        ops.push(Op::Move {
            id,
            parent,
            name: name.to_string(),
        });
    }

    /// Records new metadata; an op only if something the PC shows changed.
    pub fn update(&mut self, id: u32, meta: Meta, ops: &mut Vec<Op>) {
        let n = &mut self.nodes[id as usize];
        if n.meta == meta {
            return;
        }
        let shown = n.meta.size != meta.size
            || n.meta.modified != meta.modified
            || n.meta.created != meta.created;
        n.meta = meta;
        if shown {
            ops.push(Op::Update {
                id,
                size: meta.size,
                modified: meta.modified,
                created: meta.created,
            });
        }
    }

    /// Makes the subtree at `id` match `fresh` (a listing of the same
    /// folder): unchanged entries keep their ids. New folders are appended
    /// to `new_dirs`, removed ones to `removed_dirs`.
    pub fn merge(
        &mut self,
        id: u32,
        fresh: &Fresh,
        ops: &mut Vec<Op>,
        new_dirs: &mut Vec<u32>,
        removed_dirs: &mut Vec<u32>,
    ) {
        let kids = fresh.children();
        let mut stack = vec![(id, 0u32)];
        while let Some((t, f)) = stack.pop() {
            let (pairs, gone): (Vec<(u32, Option<u32>)>, Vec<u32>) = {
                let mut have: HashMap<&str, u32> = HashMap::new();
                for &c in &self.nodes[t as usize].children {
                    have.insert(&self.nodes[c as usize].name, c);
                }
                let pairs = kids
                    .of(f)
                    .iter()
                    .map(|&fc| (fc, have.remove(&*fresh.items[fc as usize].1.name)))
                    .collect();
                (pairs, have.into_values().collect())
            };
            for (fc, existing) in pairs {
                let item = &fresh.items[fc as usize].1;
                match existing {
                    Some(c) if self.nodes[c as usize].meta.walks() == item.meta.walks() => {
                        self.update(c, item.meta, ops);
                        if item.meta.walks() {
                            stack.push((c, fc));
                        }
                    }
                    other => {
                        if let Some(c) = other {
                            self.remove(c, ops, removed_dirs);
                        }
                        self.create_from(t, fresh, &kids, fc, ops, new_dirs);
                    }
                }
            }
            for c in gone {
                self.remove(c, ops, removed_dirs);
            }
        }
    }

    /// Creates fresh item `f` and everything below it under `parent`.
    fn create_from(
        &mut self,
        parent: u32,
        fresh: &Fresh,
        kids: &Children,
        f: u32,
        ops: &mut Vec<Op>,
        new_dirs: &mut Vec<u32>,
    ) {
        let mut stack = vec![(parent, f)];
        while let Some((p, f)) = stack.pop() {
            let item = &fresh.items[f as usize].1;
            let id = self.create(p, &item.name, item.meta, ops);
            if item.meta.walks() {
                new_dirs.push(id);
                for &c in kids.of(f) {
                    stack.push((id, c));
                }
            }
        }
    }

    /// Total size and file count of every folder (index = id).
    pub fn folder_totals(&self) -> (Vec<u64>, Vec<u64>) {
        let n = self.nodes.len();
        let (mut size, mut files) = (vec![0u64; n], vec![0u64; n]);
        // Children can have smaller ids than their parent after a move, so
        // go by depth: deepest first.
        let mut order: Vec<(u32, u32)> = Vec::with_capacity(self.live + 1);
        let mut stack = vec![(ROOT_ID, 0u32)];
        while let Some((id, d)) = stack.pop() {
            order.push((id, d));
            for &c in &self.nodes[id as usize].children {
                stack.push((c, d + 1));
            }
        }
        order.sort_unstable_by_key(|&(_, d)| std::cmp::Reverse(d));
        for (id, _) in order {
            let node = &self.nodes[id as usize];
            if !node.meta.is_dir() {
                size[id as usize] = node.meta.size;
                files[id as usize] = 1;
            }
            if id != ROOT_ID {
                let p = node.parent as usize;
                size[p] += size[id as usize];
                files[p] += files[id as usize];
            }
        }
        (size, files)
    }
}

/// A listing of a folder tree made by [`walk`]: item 0 is the folder itself.
/// (Items keep the index the walk gave them, `items[i].0 == i`, so the
/// listing is sorted in place rather than copied.)
#[derive(Debug, Default)]
pub struct Fresh {
    pub items: Vec<(u32, FreshItem)>,
}

#[derive(Debug, Clone)]
pub struct FreshItem {
    pub parent: u32,
    pub name: Box<str>,
    pub meta: Meta,
}

/// Children of each fresh item, as one array of ranges.
pub struct Children {
    start: Vec<u32>,
    list: Vec<u32>,
}

impl Children {
    pub fn of(&self, i: u32) -> &[u32] {
        &self.list[self.start[i as usize] as usize..self.start[i as usize + 1] as usize]
    }
}

impl Fresh {
    pub fn children(&self) -> Children {
        let n = self.items.len();
        let mut start = vec![0u32; n + 1];
        for (_, it) in &self.items[1..] {
            start[it.parent as usize + 1] += 1;
        }
        for i in 0..n {
            start[i + 1] += start[i];
        }
        let mut fill = start.clone();
        let mut list = vec![0u32; n.saturating_sub(1)];
        for (i, (_, it)) in self.items.iter().enumerate().skip(1) {
            let p = it.parent as usize;
            list[fill[p] as usize] = i as u32;
            fill[p] += 1;
        }
        Children { start, list }
    }
}

/// Walks `dir` in parallel on `pool` (symlinks aren't followed, names that
/// aren't UTF-8 are skipped).
pub fn walk(dir: &Path, pool: &rayon::ThreadPool) -> Fresh {
    let next = AtomicU32::new(1);
    let out: Mutex<Vec<(u32, FreshItem)>> = Mutex::new(Vec::new());
    fn visit<'a>(
        dir: PathBuf,
        idx: u32,
        next: &'a AtomicU32,
        out: &'a Mutex<Vec<(u32, FreshItem)>>,
        s: &rayon::Scope<'a>,
    ) {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut local = Vec::new();
        for ent in rd.flatten() {
            let Ok(name) = ent.file_name().into_string() else {
                log::debug!("skipping a name that isn't UTF-8 in {}", dir.display());
                continue;
            };
            let Ok(m) = ent.metadata() else { continue };
            let meta = Meta::of(&name, &m);
            let i = next.fetch_add(1, Ordering::Relaxed);
            if meta.walks() {
                let p = ent.path();
                s.spawn(move |s| visit(p, i, next, out, s));
            }
            local.push((
                i,
                FreshItem {
                    parent: idx,
                    name: name.into_boxed_str(),
                    meta,
                },
            ));
        }
        out.lock().unwrap().extend(local);
    }
    let root_meta = std::fs::symlink_metadata(dir)
        .map(|m| Meta::of("", &m))
        .unwrap_or(Meta {
            attrs: FILE_ATTRIBUTE_DIRECTORY,
            ..Default::default()
        });
    out.lock().unwrap().push((
        0,
        FreshItem {
            parent: 0,
            name: "".into(),
            meta: root_meta,
        },
    ));
    pool.scope(|s| visit(dir.to_path_buf(), 0, &next, &out, s));
    let mut items = out.into_inner().unwrap();
    items.sort_unstable_by_key(|(i, _)| *i);
    items.shrink_to_fit();
    Fresh { items }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("qs-agent-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("a").join("b")).unwrap();
        std::fs::write(d.join("top.txt"), b"12345").unwrap();
        std::fs::write(d.join("a").join("one.bin"), vec![0u8; 1000]).unwrap();
        std::fs::write(d.join("a").join("b").join("deep.md"), b"#").unwrap();
        std::fs::write(d.join(".hidden"), b"").unwrap();
        d
    }

    pub fn pool() -> rayon::ThreadPool {
        rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build()
            .unwrap()
    }

    fn walked(d: &Path) -> (Tree, Vec<Op>) {
        let mut t = Tree::new(d);
        let mut ops = Vec::new();
        let f = walk(d, &pool());
        t.merge(ROOT_ID, &f, &mut ops, &mut Vec::new(), &mut Vec::new());
        (t, ops)
    }

    #[test]
    fn walk_and_merge_build_the_tree() {
        let d = tmp("walk");
        let (t, ops) = walked(&d);
        assert_eq!(t.len(), 6);
        assert_eq!(ops.len(), 6);
        let a = t.child(ROOT_ID, "a").unwrap();
        let b = t.child(a, "b").unwrap();
        let deep = t.child(b, "deep.md").unwrap();
        assert_eq!(t.path(deep), d.join("a").join("b").join("deep.md"));
        assert_eq!(
            t.get(t.child(a, "one.bin").unwrap()).unwrap().meta.size,
            1000
        );
        let h = t.child(ROOT_ID, ".hidden").unwrap();
        assert_ne!(t.get(h).unwrap().meta.attrs & FILE_ATTRIBUTE_HIDDEN, 0);
        // Parents come before children in a fresh walk's creates.
        let recs = t.records();
        assert_eq!(recs.len(), 6);
        let (size, files) = t.folder_totals();
        assert_eq!(size[ROOT_ID as usize], 1006);
        assert_eq!(files[a as usize], 2);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn re_merge_only_reports_differences() {
        let d = tmp("diff");
        let (mut t, _) = walked(&d);
        let a = t.child(ROOT_ID, "a").unwrap();
        let one = t.child(a, "one.bin").unwrap();
        std::fs::write(d.join("a").join("one.bin"), vec![0u8; 2000]).unwrap();
        std::fs::remove_dir_all(d.join("a").join("b")).unwrap();
        std::fs::create_dir(d.join("new")).unwrap();
        std::fs::write(d.join("new").join("x.txt"), b"x").unwrap();
        let mut ops = Vec::new();
        let (mut nd, mut rd) = (Vec::new(), Vec::new());
        t.merge(ROOT_ID, &walk(&d, &pool()), &mut ops, &mut nd, &mut rd);
        // one.bin keeps its id and gets an update; b and deep.md go; new and x.txt come.
        assert_eq!(t.child(a, "one.bin"), Some(one));
        assert_eq!(t.get(one).unwrap().meta.size, 2000);
        let kinds: Vec<&str> = ops
            .iter()
            .map(|o| match o {
                Op::Create(_) => "create",
                Op::Remove { .. } => "remove",
                Op::Move { .. } => "move",
                Op::Update { .. } => "update",
            })
            .collect();
        assert_eq!(kinds.iter().filter(|k| **k == "create").count(), 2);
        assert_eq!(kinds.iter().filter(|k| **k == "remove").count(), 2);
        assert!(kinds.contains(&"update"));
        assert_eq!((nd.len(), rd.len()), (1, 1));
        // Removal order: the file inside b before b itself.
        let removes: Vec<u32> = ops
            .iter()
            .filter_map(|o| match o {
                Op::Remove { id } => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(removes[1], rd[0]);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn moves_keep_ids_and_totals_follow() {
        let d = tmp("move");
        let (mut t, _) = walked(&d);
        let a = t.child(ROOT_ID, "a").unwrap();
        let top = t.child(ROOT_ID, "top.txt").unwrap();
        let mut ops = Vec::new();
        t.move_to(a, ROOT_ID, "renamed", &mut ops);
        t.move_to(top, t.child(a, "b").unwrap(), "top.txt", &mut ops);
        assert_eq!(t.child(ROOT_ID, "renamed"), Some(a));
        assert!(
            t.path(top).ends_with("renamed/b/top.txt")
                || t.path(top).ends_with("renamed\\b\\top.txt")
        );
        let (size, _) = t.folder_totals();
        assert_eq!(size[a as usize], 1006);
        std::fs::remove_dir_all(&d).ok();
    }
}
