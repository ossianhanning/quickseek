//! Daily folder-size snapshots taken on the agent, so the PC's growth
//! history covers days it wasn't running. Same thresholds as the PC's
//! `history.rs`: folders and files of at least 10 MB.

use std::path::{Path, PathBuf};

use quickseek_remote::ROOT_ID;
use quickseek_remote::wire::{AgentSnap, SnapFile, SnapFolder};

use crate::share::Share;

pub const MIN_SIZE: u64 = 10 << 20;
pub const MAX_FOLDERS: usize = 60_000;
pub const MAX_FILES: usize = 20_000;
/// Snapshots older than this are deleted.
pub const KEEP_DAYS: i64 = 400;
/// A share gets a new snapshot when its last one is older than this.
pub const EVERY_SECS: i64 = 23 * 3600;

/// The share's folder sizes now.
pub fn take(sh: &Share, time: i64) -> AgentSnap {
    let st = sh.state.lock().unwrap();
    let t = &st.tree;
    let (size, files) = t.folder_totals();
    // Folders of at least MIN_SIZE, parents first (sizes include contents,
    // so the set is closed under ancestors), the biggest if there are too many.
    let mut keep: Vec<u32> = Vec::new();
    let mut stack = vec![ROOT_ID];
    while let Some(id) = stack.pop() {
        keep.push(id);
        for &c in &t.nodes[id as usize].children {
            if t.nodes[c as usize].meta.walks() && size[c as usize] >= MIN_SIZE {
                stack.push(c);
            }
        }
    }
    if keep.len() > MAX_FOLDERS {
        let cut = {
            let mut s: Vec<u64> = keep.iter().map(|&i| size[i as usize]).collect();
            s.sort_unstable_by(|a, b| b.cmp(a));
            s[MAX_FOLDERS - 1]
        };
        keep.retain(|&i| size[i as usize] > cut || i == ROOT_ID);
    }
    let mut pos = std::collections::HashMap::with_capacity(keep.len());
    let mut folders = Vec::with_capacity(keep.len());
    for &id in &keep {
        let n = &t.nodes[id as usize];
        let parent = if id == ROOT_ID {
            u32::MAX
        } else {
            match pos.get(&n.parent) {
                Some(&p) => p,
                None => continue,
            }
        };
        pos.insert(id, folders.len() as u32);
        folders.push(SnapFolder {
            parent,
            name: if id == ROOT_ID {
                String::new()
            } else {
                n.name.to_string()
            },
            size: size[id as usize],
            files: files[id as usize],
        });
    }
    let mut big: Vec<SnapFile> = Vec::new();
    for (id, n) in t.nodes.iter().enumerate() {
        if n.live
            && !n.meta.is_dir()
            && n.meta.size >= MIN_SIZE
            && id as u32 != ROOT_ID
            && let Some(&p) = pos.get(&n.parent)
        {
            big.push(SnapFile {
                parent: p,
                name: n.name.to_string(),
                size: n.meta.size,
            });
        }
    }
    if big.len() > MAX_FILES {
        big.sort_unstable_by_key(|f| std::cmp::Reverse(f.size));
        big.truncate(MAX_FILES);
    }
    AgentSnap {
        time,
        share: sh.name.clone(),
        capacity: st.capacity,
        free: st.free,
        folders,
        files: big,
    }
}

/// `-<share>.snap`, with only safe characters.
fn suffix(share: &str) -> String {
    let safe: String = share
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("-{safe}.snap")
}

fn file_name(s: &AgentSnap) -> String {
    format!("{}{}", s.time, suffix(&s.share))
}

pub fn save(dir: &Path, s: &AgentSnap) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let p = dir.join(file_name(s));
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, s.to_file_bytes())?;
    std::fs::rename(&tmp, &p)?;
    Ok(p)
}

/// (time, path) of every snapshot file, oldest first.
fn files(dir: &Path) -> Vec<(i64, PathBuf)> {
    let mut v: Vec<(i64, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().into_string().ok()?;
            let t = n.strip_suffix(".snap")?.split('-').next()?.parse().ok()?;
            Some((t, e.path()))
        })
        .collect();
    v.sort();
    v
}

/// Snapshots newer than `since` (all shares).
pub fn list(dir: &Path, since: i64) -> Vec<AgentSnap> {
    files(dir)
        .into_iter()
        .filter(|(t, _)| *t > since)
        .filter_map(|(_, p)| AgentSnap::from_file_bytes(&std::fs::read(p).ok()?).ok())
        .collect()
}

/// Time of the newest snapshot of `share`.
pub fn newest(dir: &Path, share: &str) -> Option<i64> {
    let suffix = suffix(share);
    files(dir)
        .into_iter()
        .filter(|(_, p)| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(&suffix))
        })
        .map(|(t, _)| t)
        .max()
}

pub fn prune(dir: &Path, now: i64) {
    for (t, p) in files(dir) {
        if t < now - KEEP_DAYS * 86_400 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Takes the snapshots that are due.
pub fn maybe_take(dir: &Path, shares: &[std::sync::Arc<Share>], now: i64) {
    for sh in shares {
        if !sh.state.lock().unwrap().ready {
            continue;
        }
        if newest(dir, &sh.name).is_some_and(|t| t > now - EVERY_SECS) {
            continue;
        }
        let s = take(sh, now);
        match save(dir, &s) {
            Ok(_) => log::info!("{}: snapshot of {} folders", sh.name, s.folders.len()),
            Err(e) => log::warn!("{}: saving a snapshot: {e}", sh.name),
        }
    }
    prune(dir, now);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::tests::pool;

    #[test]
    fn snapshots_keep_big_folders_and_files() {
        let d = std::env::temp_dir().join(format!("qs-agent-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("big").join("inner")).unwrap();
        std::fs::create_dir_all(d.join("small")).unwrap();
        std::fs::write(
            d.join("big").join("inner").join("movie.mkv"),
            vec![0u8; 11 << 20],
        )
        .unwrap();
        std::fs::write(d.join("small").join("x.txt"), b"x").unwrap();
        let sh = std::sync::Arc::new(Share::new("my share", &d, 1));
        sh.rewalk(&pool());
        let s = take(&sh, 1000);
        let names: Vec<&str> = s.folders.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["", "big", "inner"]);
        assert_eq!(s.folders[2].parent, 1);
        assert_eq!(s.files.len(), 1);
        assert_eq!(s.files[0].parent, 2);
        assert_eq!(s.folders[0].files, 2);
        let hist = d.join("hist");
        maybe_take(&hist, std::slice::from_ref(&sh), 5000);
        maybe_take(&hist, std::slice::from_ref(&sh), 5000 + 3600);
        assert_eq!(list(&hist, 0).len(), 1, "one a day");
        assert_eq!(newest(&hist, "my share"), Some(5000));
        maybe_take(&hist, std::slice::from_ref(&sh), 5000 + EVERY_SECS + 1);
        assert_eq!(list(&hist, 5000).len(), 1);
        prune(&hist, 5000 + KEEP_DAYS * 86_400 + 2);
        assert_eq!(list(&hist, 0).len(), 1);
        std::fs::remove_dir_all(&d).ok();
    }
}
