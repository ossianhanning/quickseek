//! One served share: its tree, the log of recent changes, and applying
//! change events and re-walks to both under one lock (so a full transfer
//! and the ops after it always agree).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use quickseek_remote::ROOT_ID;
use quickseek_remote::wire::Op;

use crate::tree::{Fresh, Meta, Tree, walk};
use crate::watch::Event;

/// Ops kept for clients that reconnect (older ones get a full transfer).
pub const LOG_CAP: usize = 200_000;
/// After loading a saved index, sequence numbers continue this far above
/// the saved one: a PC that saw ops after the save (which the agent no
/// longer has) gets a full transfer, never another history's ops.
const RESUME_GAP: u64 = 1 << 40;
const SAVE_MAGIC: &[u8; 8] = b"QSAIDX01";

pub struct State {
    pub tree: Tree,
    /// Chosen at start; ids are only meaningful within it.
    pub epoch: u64,
    /// Sequence number of the last op.
    pub seq: u64,
    /// The last ops, `log[i]` having seq `seq - log.len() + 1 + i`.
    pub log: VecDeque<Op>,
    /// The first walk finished: the tree is worth sending.
    pub ready: bool,
    pub status: String,
    pub capacity: u64,
    pub free: u64,
    /// Loaded from a save made at seq `.0`; the log continues from `.1`.
    pub resumed: Option<(u64, u64)>,
}

impl State {
    /// Ops after `seq`, at most `max`; `None` if they're no longer in the log.
    pub fn ops_after(&self, seq: u64, max: usize) -> Option<Vec<Op>> {
        // A PC that was in step with the saved index continues from it.
        let seq = match self.resumed {
            Some((saved, base)) if seq == saved => base,
            _ => seq,
        };
        let first = self.seq + 1 - self.log.len() as u64;
        if seq + 1 < first || seq > self.seq {
            return None;
        }
        let skip = (seq + 1 - first) as usize;
        Some(self.log.iter().skip(skip).take(max).cloned().collect())
    }
}

pub struct Share {
    pub name: String,
    pub root: PathBuf,
    pub state: Mutex<State>,
    /// Signalled after every change and state change.
    pub changed: Condvar,
    /// btrfs: the subvolumes and the generation the index is current to
    /// (empty when the share isn't indexed through btrfs).
    #[cfg(target_os = "linux")]
    pub subvols: Mutex<Vec<crate::btrfs::Subvol>>,
    /// One save at a time (they share a temporary file).
    saving: Mutex<()>,
}

/// What a batch did to the folders a watcher has to follow.
#[derive(Default, Debug)]
pub struct DirChanges {
    pub added: Vec<(u32, PathBuf)>,
    pub removed: Vec<u32>,
}

impl Share {
    pub fn new(name: &str, root: &Path, epoch: u64) -> Share {
        Share {
            name: name.to_string(),
            root: root.to_path_buf(),
            state: Mutex::new(State {
                tree: Tree::new(root),
                epoch,
                seq: 0,
                log: VecDeque::new(),
                ready: false,
                status: "walking".into(),
                capacity: 0,
                free: 0,
                resumed: None,
            }),
            changed: Condvar::new(),
            #[cfg(target_os = "linux")]
            subvols: Mutex::new(Vec::new()),
            saving: Mutex::new(()),
        }
    }

    /// Where this share's index is saved in config folder `dir`.
    pub fn save_path(&self, dir: &Path) -> PathBuf {
        let safe: String = self
            .name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        dir.join(format!("index-{safe}.idx"))
    }

    /// Saves the index (tree, epoch, seq, btrfs generations) so a restart
    /// only catches up and PCs in step with it don't need it all again.
    /// Returns the seq saved.
    pub fn save(&self, dir: &Path) -> std::io::Result<u64> {
        use quickseek_remote::wire::{put, put_str};
        let _one = self.saving.lock().unwrap();
        let mut body = Vec::new();
        let seq = {
            let st = self.state.lock().unwrap();
            if !st.ready {
                return Ok(0);
            }
            put(&mut body, st.epoch);
            put(&mut body, st.seq);
            put_str(&mut body, &self.root.to_string_lossy());
            #[cfg(target_os = "linux")]
            let subvols: Vec<(u64, u64, u64)> = self
                .subvols
                .lock()
                .unwrap()
                .iter()
                .map(|s| (s.tree, s.dev, s.generation))
                .collect();
            #[cfg(not(target_os = "linux"))]
            let subvols: Vec<(u64, u64, u64)> = Vec::new();
            put(&mut body, subvols.len() as u64);
            for (t, d, g) in subvols {
                put(&mut body, t);
                put(&mut body, d);
                put(&mut body, g);
            }
            st.tree.save_to(&mut body);
            st.seq
        };
        let mut data = SAVE_MAGIC.to_vec();
        data.extend(quickseek_remote::wire::compress(&body));
        let path = self.save_path(dir);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, data)?;
        std::fs::rename(tmp, path)?;
        Ok(seq)
    }

    /// Loads a saved index (before anything else touched this share).
    pub fn load(&self, dir: &Path) -> std::io::Result<()> {
        use quickseek_remote::wire::{Rd, bad};
        let data = std::fs::read(self.save_path(dir))?;
        let body = data
            .strip_prefix(SAVE_MAGIC)
            .ok_or_else(|| bad("saved index: magic"))?;
        let body = quickseek_remote::wire::decompress(body)?;
        let mut rd = Rd(&body);
        let epoch = rd.u()?;
        let seq = rd.u()?;
        if rd.str()? != self.root.to_string_lossy() {
            return Err(bad("saved index: another folder"));
        }
        let n = rd.u()? as usize;
        let mut subvols = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            subvols.push((rd.u()?, rd.u()?, rd.u()?));
        }
        let tree = Tree::load_from(&self.root, &mut rd)?;
        let entries = tree.len();
        let mut st = self.state.lock().unwrap();
        st.tree = tree;
        st.epoch = epoch;
        st.resumed = Some((seq, seq + RESUME_GAP));
        st.seq = seq + RESUME_GAP;
        st.log.clear();
        st.ready = true;
        st.status = "ready".into();
        #[cfg(target_os = "linux")]
        {
            *self.subvols.lock().unwrap() = subvols
                .into_iter()
                .map(|(tree, dev, generation)| crate::btrfs::Subvol {
                    tree,
                    dev,
                    generation,
                })
                .collect();
        }
        #[cfg(not(target_os = "linux"))]
        let _ = subvols;
        log::info!("{}: loaded the saved index ({entries} entries)", self.name);
        self.changed.notify_all();
        Ok(())
    }

    fn commit(&self, st: &mut State, ops: Vec<Op>) {
        if ops.is_empty() {
            return;
        }
        st.seq += ops.len() as u64;
        st.log.extend(ops);
        while st.log.len() > LOG_CAP {
            st.log.pop_front();
        }
        self.changed.notify_all();
    }

    pub fn set_status(&self, s: &str) {
        let mut st = self.state.lock().unwrap();
        if st.status != s {
            log::info!("{}: {s}", self.name);
            st.status = s.to_string();
            self.changed.notify_all();
        }
    }

    /// Walks the whole share and merges it into the tree (the first walk,
    /// and the gap-filler after an overflow or on schedule).
    pub fn rewalk(&self, pool: &rayon::ThreadPool) -> DirChanges {
        let t0 = std::time::Instant::now();
        let fresh = walk(&self.root, pool);
        self.merge_walk(fresh, t0.elapsed())
    }

    /// Merges a walk of the whole share (made without the lock, while
    /// changes kept being applied: the caller re-applies those afterwards,
    /// since the listing may predate them).
    pub fn merge_walk(&self, fresh: Fresh, walked: Duration) -> DirChanges {
        let mut st = self.state.lock().unwrap();
        let mut ops = Vec::new();
        let mut added = Vec::new();
        let mut removed = Vec::new();
        let first = !st.ready;
        st.tree.quiet = first;
        if let Some((_, r)) = fresh.items.first() {
            // The root's own metadata (no op: the PC doesn't show the root's dates).
            st.tree.nodes[ROOT_ID as usize].meta = r.meta;
        }
        st.tree
            .merge(ROOT_ID, &fresh, &mut ops, &mut added, &mut removed);
        st.tree.quiet = false;
        let entries = fresh.items.len().saturating_sub(1);
        drop(fresh);
        if first {
            added.insert(0, ROOT_ID);
        }
        log::info!(
            "{}: indexed {} entries in {} ms ({} changes sent)",
            self.name,
            entries,
            walked.as_millis(),
            ops.len()
        );
        self.commit(&mut st, ops);
        st.ready = true;
        let (c, f) = disk_space(&self.root);
        (st.capacity, st.free) = (c, f);
        st.status = "ready".into();
        self.changed.notify_all();
        let added = added.into_iter().map(|d| (d, st.tree.path(d))).collect();
        DirChanges { added, removed }
    }

    /// Applies a batch of change events (already filtered to this share).
    pub fn apply(&self, events: &[Event], pool: &rayon::ThreadPool) -> DirChanges {
        let mut st = self.state.lock().unwrap();
        let mut ops = Vec::new();
        let mut out = DirChanges::default();
        let mut added = Vec::new();
        // Collapse repeats of the same name (a file written in many pieces).
        let mut seen = std::collections::HashSet::new();
        let mut dirs_touched = Vec::new();
        for ev in events {
            match ev {
                Event::Touch { dir, name, .. } => {
                    if !seen.insert((*dir, name.as_str())) {
                        continue;
                    }
                    dirs_touched.push(*dir);
                    reconcile(
                        &mut st.tree,
                        *dir,
                        name,
                        &mut ops,
                        &mut added,
                        &mut out.removed,
                        pool,
                    );
                }
                Event::Rename {
                    from_dir,
                    from_name,
                    to_dir,
                    to_name,
                    ..
                } => {
                    // Later touches of either name must look again.
                    seen.clear();
                    dirs_touched.extend([*from_dir, *to_dir]);
                    let moving = st
                        .tree
                        .get(*from_dir)
                        .and_then(|_| st.tree.child(*from_dir, from_name));
                    match moving {
                        Some(id) if st.tree.is_dir(*to_dir) => {
                            if let Some(old) = st.tree.child(*to_dir, to_name)
                                && old != id
                            {
                                st.tree.remove(old, &mut ops, &mut out.removed);
                            }
                            st.tree.move_to(id, *to_dir, to_name, &mut ops);
                            reconcile(
                                &mut st.tree,
                                *to_dir,
                                to_name,
                                &mut ops,
                                &mut added,
                                &mut out.removed,
                                pool,
                            );
                        }
                        _ => {
                            reconcile(
                                &mut st.tree,
                                *from_dir,
                                from_name,
                                &mut ops,
                                &mut added,
                                &mut out.removed,
                                pool,
                            );
                            reconcile(
                                &mut st.tree,
                                *to_dir,
                                to_name,
                                &mut ops,
                                &mut added,
                                &mut out.removed,
                                pool,
                            );
                        }
                    }
                }
                Event::DirGone { dir, .. } => {
                    if *dir != ROOT_ID {
                        st.tree.remove(*dir, &mut ops, &mut out.removed);
                    }
                }
                Event::Overflow => {}
            }
        }
        // A folder's own date modified changes with its contents.
        dirs_touched.sort_unstable();
        dirs_touched.dedup();
        for d in dirs_touched {
            if d != ROOT_ID
                && st.tree.is_dir(d)
                && let Ok(m) = std::fs::symlink_metadata(st.tree.path(d))
            {
                let name = st
                    .tree
                    .get(d)
                    .map(|n| n.name.to_string())
                    .unwrap_or_default();
                st.tree.update(d, Meta::of(&name, &m), &mut ops);
            }
        }
        self.commit(&mut st, ops);
        out.added = added
            .into_iter()
            .filter(|&d| st.tree.is_dir(d))
            .map(|d| (d, st.tree.path(d)))
            .collect();
        out
    }

    /// Applies a btrfs catch-up ([`crate::btrfs::changed`]): new metadata for
    /// changed inodes, and every changed folder compared with a listing of
    /// it (deletions, additions, and moves recognised by inode).
    #[cfg(target_os = "linux")]
    pub fn catch_up(
        &self,
        changed: &[crate::btrfs::Changed],
        pool: &rayon::ThreadPool,
    ) -> DirChanges {
        use std::collections::{HashMap, HashSet};
        let mut st = self.state.lock().unwrap();
        let tree = &mut st.tree;
        let mut ops = Vec::new();
        let mut out = DirChanges::default();
        let mut added = Vec::new();
        let mut by_ino: HashMap<(u64, u64), Vec<u32>> = HashMap::new();
        for id in std::iter::once(ROOT_ID).chain(quickseek_remote::FIRST_ID..tree.next_id()) {
            if let Some(n) = tree.get(id) {
                by_ino.entry((n.meta.dev, n.meta.ino)).or_default().push(id);
            }
        }
        // New metadata; the changed folders to look into.
        let mut dirs = Vec::new();
        for c in changed {
            for &id in by_ino
                .get(&(c.dev, c.ino))
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                let n = tree.get(id).unwrap();
                let name = n.name.to_string();
                let walks = n.meta.walks();
                let meta = c.inode.meta(&name, c.dev, c.ino);
                if id == ROOT_ID {
                    tree.nodes[ROOT_ID as usize].meta = meta;
                } else if meta.walks() == walks {
                    tree.update(id, meta, &mut ops);
                }
                if walks {
                    dirs.push(id);
                }
            }
        }
        dirs.sort_unstable();
        dirs.dedup();
        // Each changed folder against its listing.
        let mut gone: HashSet<u32> = HashSet::new();
        let mut new: Vec<(u32, String)> = Vec::new();
        for &d in &dirs {
            let Ok(listing) = crate::btrfs::list(&tree.path(d)) else {
                continue; // went away itself: its parent's listing removes it
            };
            let have: HashMap<&str, u32> = tree.nodes[d as usize]
                .children
                .iter()
                .map(|&c| (&*tree.nodes[c as usize].name, c))
                .collect();
            for (&name, &c) in &have {
                if listing.get(name) != Some(&tree.nodes[c as usize].meta.ino) {
                    gone.insert(c);
                }
            }
            for (name, ino) in &listing {
                if have
                    .get(name.as_str())
                    .is_none_or(|&c| tree.nodes[c as usize].meta.ino != *ino)
                {
                    new.push((d, name.clone()));
                }
            }
        }
        // A new name for an inode that left its old place: a move.
        let mut create = Vec::new();
        for (d, name) in new {
            let path = tree.path(d).join(&name);
            let Ok(m) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            let meta = Meta::of(&name, &m);
            let moved = by_ino
                .get(&(meta.dev, meta.ino))
                .and_then(|ids| ids.iter().copied().find(|id| gone.contains(id)));
            match moved {
                Some(id) if tree.get(id).is_some_and(|n| n.meta.walks() == meta.walks()) => {
                    gone.remove(&id);
                    tree.move_to(id, d, &name, &mut ops);
                    tree.update(id, meta, &mut ops);
                }
                _ => create.push((d, name)),
            }
        }
        for id in gone {
            tree.remove(id, &mut ops, &mut out.removed);
        }
        for (d, name) in create {
            reconcile(tree, d, &name, &mut ops, &mut added, &mut out.removed, pool);
        }
        let n_ops = ops.len();
        self.commit(&mut st, ops);
        out.added = added
            .into_iter()
            .filter(|&d| st.tree.is_dir(d))
            .map(|d| (d, st.tree.path(d)))
            .collect();
        let level = if n_ops > 0 {
            log::Level::Info
        } else {
            log::Level::Debug
        };
        log::log!(
            level,
            "{}: caught up {} changed inodes, {} folders looked at, {n_ops} changes sent",
            self.name,
            changed.len(),
            dirs.len()
        );
        out
    }

    /// Refreshes the disk numbers; true if they moved enough to tell clients.
    pub fn refresh_disk(&self) -> bool {
        let (c, f) = disk_space(&self.root);
        let mut st = self.state.lock().unwrap();
        let moved = st.capacity != c || st.free.abs_diff(f) > (256 << 20);
        if moved {
            (st.capacity, st.free) = (c, f);
            self.changed.notify_all();
        }
        moved
    }

    /// Waits until something changes or `timeout` passes.
    pub fn wait<'a>(
        &self,
        st: std::sync::MutexGuard<'a, State>,
        timeout: Duration,
    ) -> std::sync::MutexGuard<'a, State> {
        self.changed.wait_timeout(st, timeout).unwrap().0
    }
}

/// Makes `dir/name` in the tree match the filesystem.
fn reconcile(
    tree: &mut Tree,
    dir: u32,
    name: &str,
    ops: &mut Vec<Op>,
    added: &mut Vec<u32>,
    removed: &mut Vec<u32>,
    pool: &rayon::ThreadPool,
) {
    if name.is_empty() || name == "." || name == ".." || !tree.is_dir(dir) {
        return;
    }
    let path = tree.path(dir).join(name);
    let existing = tree.child(dir, name);
    match std::fs::symlink_metadata(&path) {
        Ok(m) => {
            let meta = Meta::of(name, &m);
            match existing {
                Some(id) if tree.get(id).unwrap().meta.walks() == meta.walks() => {
                    tree.update(id, meta, ops);
                }
                _ => {
                    if let Some(id) = existing {
                        tree.remove(id, ops, removed);
                    }
                    let id = tree.create(dir, name, meta, ops);
                    if meta.walks() {
                        // A new folder: what's in it already (created before
                        // a watch could see it, or moved in from outside).
                        added.push(id);
                        let fresh = walk(&path, pool);
                        tree.merge(id, &fresh, ops, added, removed);
                    }
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(id) = existing {
                tree.remove(id, ops, removed);
            }
        }
        Err(e) => log::debug!("{}: {e}", path.display()),
    }
}

/// Capacity and free bytes of the filesystem holding `path`.
#[cfg(target_os = "linux")]
pub fn disk_space(path: &Path) -> (u64, u64) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return (0, 0);
    };
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return (0, 0);
    }
    let unit = s.f_frsize as u64;
    (s.f_blocks as u64 * unit, s.f_bavail as u64 * unit)
}

#[cfg(not(target_os = "linux"))]
pub fn disk_space(_path: &Path) -> (u64, u64) {
    (0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::tests::{pool, tmp};

    fn names(sh: &Share) -> Vec<String> {
        let st = sh.state.lock().unwrap();
        let mut v: Vec<String> = st
            .tree
            .records()
            .iter()
            .map(|r| {
                st.tree
                    .path(r.id)
                    .strip_prefix(&sh.root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        v.sort();
        v
    }

    /// A walk runs while changes keep being applied; its listing can predate
    /// them, and applying them again after the merge restores them.
    #[test]
    fn changes_during_a_walk_survive_its_merge() {
        let d = tmp("walkrace");
        let p = pool();
        let sh = Share::new("t", &d, 1);
        sh.rewalk(&p);
        let t0 = std::time::Instant::now();
        let listing = walk(&d, &p); // the walk, "still running"
        std::fs::write(d.join("during.txt"), b"x").unwrap();
        std::fs::remove_file(d.join("top.txt")).unwrap();
        let seen = vec![
            Event::Touch {
                share: 0,
                dir: ROOT_ID,
                name: "during.txt".into(),
            },
            Event::Touch {
                share: 0,
                dir: ROOT_ID,
                name: "top.txt".into(),
            },
        ];
        sh.apply(&seen, &p);
        assert!(names(&sh).contains(&"during.txt".to_string()));
        sh.merge_walk(listing, t0.elapsed());
        // The older listing undid both changes...
        assert!(!names(&sh).contains(&"during.txt".to_string()));
        assert!(names(&sh).contains(&"top.txt".to_string()));
        // ...and applying the events seen meanwhile again restores them.
        sh.apply(&seen, &p);
        let n = names(&sh);
        assert!(
            n.contains(&"during.txt".to_string()) && !n.contains(&"top.txt".to_string()),
            "{n:?}"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn touches_and_renames_reconcile() {
        let d = tmp("share");
        let p = pool();
        let sh = Share::new("t", &d, 1);
        sh.rewalk(&p);
        let a = sh.state.lock().unwrap().tree.child(ROOT_ID, "a").unwrap();
        // New folder with content created "before the watch".
        std::fs::create_dir_all(d.join("a").join("new").join("inner")).unwrap();
        std::fs::write(d.join("a").join("new").join("inner").join("f.txt"), b"abc").unwrap();
        std::fs::remove_file(d.join("top.txt")).unwrap();
        let ch = sh.apply(
            &[
                Event::Touch {
                    share: 0,
                    dir: a,
                    name: "new".into(),
                },
                Event::Touch {
                    share: 0,
                    dir: a,
                    name: "new".into(),
                },
                Event::Touch {
                    share: 0,
                    dir: ROOT_ID,
                    name: "top.txt".into(),
                },
            ],
            &p,
        );
        assert_eq!(ch.added.len(), 2, "{ch:?}");
        let n = names(&sh);
        assert!(n.contains(&"a/new/inner/f.txt".to_string()), "{n:?}");
        assert!(!n.contains(&"top.txt".to_string()));
        // A folder rename keeps the id; its contents follow.
        let new = sh.state.lock().unwrap().tree.child(a, "new").unwrap();
        std::fs::rename(d.join("a").join("new"), d.join("moved")).unwrap();
        sh.apply(
            &[Event::Rename {
                share: 0,
                from_dir: a,
                from_name: "new".into(),
                to_dir: ROOT_ID,
                to_name: "moved".into(),
            }],
            &p,
        );
        assert_eq!(
            sh.state.lock().unwrap().tree.child(ROOT_ID, "moved"),
            Some(new)
        );
        assert!(names(&sh).contains(&"moved/inner/f.txt".to_string()));
        // Ops replay: the log holds everything since the start.
        let st = sh.state.lock().unwrap();
        assert_eq!(st.ops_after(0, usize::MAX).unwrap().len() as u64, st.seq);
        assert!(st.ops_after(st.seq + 1, 10).is_none());
        assert_eq!(st.ops_after(st.seq, 10).unwrap().len(), 0);
        drop(st);
        std::fs::remove_dir_all(&d).ok();
    }
}
