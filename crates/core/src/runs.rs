//! Run history: how often a file was opened from the UI (Everything's "Run
//! Count"). Keyed by (volume serial, MFT record) so it survives renames and
//! moves on NTFS.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::index::{Index, NONE};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunInfo {
    pub count: u32,
    /// FILETIME of the last run.
    pub last: i64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RunHistory {
    entries: Vec<(u32, u32, RunInfo)>,
    #[serde(skip)]
    map: HashMap<(u32, u32), usize>,
}

impl RunHistory {
    pub fn load(path: &Path) -> RunHistory {
        let mut h: RunHistory = std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        h.reindex();
        h
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(path, serde_json::to_string(self).unwrap())
    }

    fn reindex(&mut self) {
        self.map = self.entries.iter().enumerate().map(|(k, &(s, r, _))| ((s, r), k)).collect();
    }

    pub fn record(&mut self, serial: u32, record: u32, now: i64) -> RunInfo {
        match self.map.get(&(serial, record)) {
            Some(&k) => {
                let e = &mut self.entries[k].2;
                e.count += 1;
                e.last = now;
                *e
            }
            None => {
                let info = RunInfo { count: 1, last: now };
                self.map.insert((serial, record), self.entries.len());
                self.entries.push((serial, record, info));
                info
            }
        }
    }

    pub fn get(&self, serial: u32, record: u32) -> Option<RunInfo> {
        self.map.get(&(serial, record)).map(|&k| self.entries[k].2)
    }

    /// Entry → run count for the runs that belong to this index.
    pub fn for_index(&self, ix: &Index) -> HashMap<u32, u32> {
        self.entries
            .iter()
            .filter(|(s, _, _)| *s == ix.volume_serial)
            .filter_map(|&(_, r, info)| {
                let e = ix.links(r).find(|&i| ix.is_visible(i)).unwrap_or(NONE);
                (e != NONE).then_some((e, info.count))
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::{by_name, sample};

    #[test]
    fn counts_and_maps_to_entries() {
        let mut ix = sample();
        ix.volume_serial = 7;
        let mut h = RunHistory::default();
        h.record(7, 35, 1);
        h.record(7, 35, 2);
        h.record(8, 35, 3); // other volume
        assert_eq!(h.get(7, 35), Some(RunInfo { count: 2, last: 2 }));
        let m = h.for_index(&ix);
        assert_eq!(m.len(), 1);
        assert_eq!(m[&by_name(&ix, "notepad.exe")], 2);
        let p = std::env::temp_dir().join(format!("rapidfind-runs-{}.json", std::process::id()));
        h.save(&p).unwrap();
        let back = RunHistory::load(&p);
        std::fs::remove_file(&p).ok();
        assert_eq!(back.get(7, 35), Some(RunInfo { count: 2, last: 2 }));
    }
}
