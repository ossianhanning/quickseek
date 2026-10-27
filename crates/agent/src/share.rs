//! One served share: its tree, the log of recent changes, and applying
//! change events and re-walks to both under one lock (so a full transfer
//! and the ops after it always agree).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use quickseek_remote::ROOT_ID;
use quickseek_remote::wire::Op;

use crate::tree::{Meta, Tree, walk};
use crate::watch::Event;

/// Ops kept for clients that reconnect (older ones get a full transfer).
pub const LOG_CAP: usize = 200_000;

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
}

impl State {
    /// Ops after `seq`, at most `max`; `None` if they're no longer in the log.
    pub fn ops_after(&self, seq: u64, max: usize) -> Option<Vec<Op>> {
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
            }),
            changed: Condvar::new(),
        }
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
        let walked = t0.elapsed();
        let mut st = self.state.lock().unwrap();
        let mut ops = Vec::new();
        let mut added = Vec::new();
        let mut removed = Vec::new();
        let first = !st.ready;
        st.tree.quiet = first;
        st.tree
            .merge(ROOT_ID, &fresh, &mut ops, &mut added, &mut removed);
        st.tree.quiet = false;
        let entries = fresh.items.len().saturating_sub(1);
        drop(fresh);
        if first {
            added.insert(0, ROOT_ID);
        }
        log::info!(
            "{}: walked {} entries in {} ms ({} changes sent)",
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
