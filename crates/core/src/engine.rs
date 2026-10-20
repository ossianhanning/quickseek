//! Ties it together: per-volume index lifecycle (load → replay journal, or
//! scan MFT → catch up), live watcher threads, and multi-volume search.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};

use crate::index::Index;
use crate::live::{Applier, file_info_by_id};
use crate::ntfs::journal::{self, Journal, JournalError};
use crate::ntfs::{mft, usn, win};
use crate::persist;
use crate::query::Query;
use crate::search::{Hits, SearchOptions, search};

#[derive(Debug, Clone)]
pub enum LoadReport {
    /// Loaded from disk and replayed `replayed` journal records.
    Loaded { replayed: usize, ms: u128 },
    Scanned { method: &'static str, stats: Option<mft::MftStats>, caught_up: usize, ms: u128 },
}

impl std::fmt::Display for LoadReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadReport::Loaded { replayed, ms } => write!(f, "loaded index in {ms} ms, replayed {replayed} journal records"),
            LoadReport::Scanned { method, stats, caught_up, ms } => {
                write!(f, "scanned via {method} in {ms} ms")?;
                if let Some(s) = stats {
                    write!(
                        f,
                        " ({} of {} records in use, {:.1} MiB read in {} ms, {} empty chunks skipped)",
                        s.used_records,
                        s.total_records,
                        s.bytes_read as f64 / (1 << 20) as f64,
                        s.read_ms,
                        s.chunks_skipped
                    )?;
                }
                write!(f, ", caught up {caught_up} journal records")
            }
        }
    }
}

/// Journal records worth saving the index for; fewer are cheaper to replay
/// on the next start than rewriting hundreds of MB.
pub const SAVE_THRESHOLD: u64 = 20_000;

pub struct Volume {
    pub letter: char,
    pub index: RwLock<Index>,
    pub applier: Mutex<Applier>,
    /// Journal records applied since the last save (`u64::MAX` = never saved).
    pub unsaved: AtomicU64,
}

fn refresh(letter: char, ix_lock: &RwLock<Index>, list: &[(u32, u64)]) {
    if list.is_empty() || !ix_lock.read().has_sizes {
        return;
    }
    let Ok(hint) = win::open_root_dir(letter) else { return };
    let infos: Vec<_> = list
        .iter()
        .filter_map(|&(rec, frn)| file_info_by_id(&hint, frn).map(|i| (rec, i)))
        .collect();
    let mut ix = ix_lock.write();
    for (rec, (size, m, c)) in infos {
        ix.set_file_info(rec, size, m, c);
    }
}

/// Replays the journal from the index's cursor up to now.
fn catch_up(ix: &mut Index, applier: &mut Applier, j: &mut Journal) -> Result<(usize, Vec<(u32, u64)>), JournalError> {
    let recs = j.drain()?;
    let refresh = applier.apply(ix, &recs);
    ix.journal_id = j.journal_id;
    ix.next_usn = j.next_usn;
    Ok((recs.len(), refresh))
}

pub fn scan_volume(letter: char, progress: &(dyn Fn(u64, u64) + Sync)) -> anyhow::Result<(Index, LoadReport, Journal, Applier)> {
    let t0 = Instant::now();
    let serial = win::list_volumes().into_iter().find(|v| v.letter == letter).map_or(0, |v| v.serial);
    // Note the journal position *before* scanning so changes made during the
    // scan are replayed afterwards.
    let h = win::open_volume(letter, 0)?;
    let jd = journal::query_or_create(&h)?;
    drop(h);

    let (scan, method, stats) = match mft::scan(letter, progress) {
        Ok((s, st)) => (s, "raw MFT", Some(st)),
        Err(e) => {
            log::warn!("{letter}: raw MFT scan failed ({e}); falling back to FSCTL_ENUM_USN_DATA");
            let h = win::open_volume(letter, 0)?;
            (usn::scan_enum(&h)?, "FSCTL_ENUM_USN_DATA", None)
        }
    };
    let mut ix = Index::from_scan(letter, &scan);
    drop(scan);
    ix.volume_serial = serial;

    let mut j = Journal::open(letter, Some(jd.journal_id), Some(jd.next_usn)).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut applier = Applier::default();
    let (n, list) = catch_up(&mut ix, &mut applier, &mut j).map_err(|e| anyhow::anyhow!("{e}"))?;
    let lock = RwLock::new(ix);
    refresh(letter, &lock, &list);
    let ix = lock.into_inner();
    let report = LoadReport::Scanned { method, stats, caught_up: n, ms: t0.elapsed().as_millis() };
    Ok((ix, report, j, applier))
}

/// Loads the saved index for `letter` and replays the journal, or scans.
pub fn open_volume(letter: char, force_scan: bool, progress: &(dyn Fn(u64, u64) + Sync)) -> anyhow::Result<(Index, LoadReport, Journal, Applier)> {
    let t0 = Instant::now();
    if !force_scan {
        let serial = win::list_volumes().into_iter().find(|v| v.letter == letter).map_or(0, |v| v.serial);
        match persist::load(&persist::db_path(letter)) {
            Ok(mut ix) if ix.letter == letter && ix.volume_serial == serial => {
                match Journal::open(letter, Some(ix.journal_id), Some(ix.next_usn)) {
                    Ok(mut j) => {
                        let mut applier = Applier::default();
                        if let Ok((n, list)) = catch_up(&mut ix, &mut applier, &mut j) {
                            let lock = RwLock::new(ix);
                            refresh(letter, &lock, &list);
                            let ix = lock.into_inner();
                            return Ok((ix, LoadReport::Loaded { replayed: n, ms: t0.elapsed().as_millis() }, j, applier));
                        }
                        log::warn!("{letter}: journal replay failed; rescanning");
                    }
                    Err(e) => log::warn!("{letter}: saved index is stale ({e}); rescanning"),
                }
            }
            Ok(_) => log::warn!("{letter}: saved index belongs to another volume; rescanning"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("{letter}: can't load saved index ({e}); rescanning"),
        }
    }
    scan_volume(letter, progress)
}

pub struct Engine {
    pub volumes: Vec<Arc<Volume>>,
    stop: Arc<AtomicBool>,
    watchers: Vec<JoinHandle<()>>,
}

impl Engine {
    /// Opens the given volumes (all NTFS volumes when empty).
    pub fn open(letters: &[char], force_scan: bool, mut on_report: impl FnMut(char, &anyhow::Result<LoadReport>)) -> Engine {
        let letters: Vec<char> = if letters.is_empty() {
            win::list_volumes().into_iter().filter(|v| v.fs == "NTFS").map(|v| v.letter).collect()
        } else {
            letters.to_vec()
        };
        let mut volumes = Vec::new();
        let mut journals = Vec::new();
        for l in letters {
            let res = open_volume(l, force_scan, &|_, _| {});
            match res {
                Ok((ix, report, j, applier)) => {
                    let unsaved = match report {
                        LoadReport::Loaded { replayed, .. } => replayed as u64,
                        LoadReport::Scanned { .. } => u64::MAX,
                    };
                    on_report(l, &Ok(report));
                    volumes.push(Arc::new(Volume {
                        letter: l,
                        index: RwLock::new(ix),
                        applier: Mutex::new(applier),
                        unsaved: AtomicU64::new(unsaved),
                    }));
                    journals.push(j);
                }
                Err(e) => on_report(l, &Err(e)),
            }
        }
        Engine { volumes, stop: Arc::new(AtomicBool::new(false)), watchers: Vec::new() }.with_journals(journals)
    }

    fn with_journals(mut self, journals: Vec<Journal>) -> Self {
        for (v, j) in self.volumes.iter().cloned().zip(journals) {
            let stop = self.stop.clone();
            self.watchers.push(std::thread::Builder::new()
                .name(format!("usn-{}", v.letter))
                .spawn(move || watch(v, j, stop))
                .unwrap());
        }
        self
    }

    /// Runs `f` with read access to every volume's index.
    pub fn with_indexes<R>(&self, f: impl FnOnce(&[&Index]) -> R) -> R {
        let guards: Vec<_> = self.volumes.iter().map(|v| v.index.read()).collect();
        let refs: Vec<&Index> = guards.iter().map(|g| &**g).collect();
        f(&refs)
    }

    pub fn search(&self, q: &Query, opts: &SearchOptions) -> Hits {
        self.with_indexes(|ixs| search(ixs, q, opts))
    }

    pub fn generation(&self) -> u64 {
        self.volumes.iter().map(|v| v.index.read().generation).sum()
    }

    /// Saves volumes with at least `min_changes` unsaved journal records.
    /// Anything below that is cheaper to replay on the next start.
    pub fn save_all(&self, min_changes: u64) -> anyhow::Result<()> {
        for v in &self.volumes {
            let n = v.unsaved.load(Ordering::Relaxed);
            if n > 0 && n >= min_changes {
                let mut ix = v.index.write();
                persist::save(&mut ix, &persist::db_path(v.letter))?;
                v.unsaved.store(0, Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Watchers are blocked in the kernel; they're daemon-like and die with the process.
    }
}

fn watch(v: Arc<Volume>, mut j: Journal, stop: Arc<AtomicBool>) {
    let mut last_save = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        // Blocks until the volume changes.
        let first = match j.read(true) {
            Ok(b) => b,
            Err(JournalError::Gap) => {
                log::warn!("{}: journal gap; rescanning", v.letter);
                match scan_volume(v.letter, &|_, _| {}) {
                    Ok((ix, _, nj, applier)) => {
                        *v.index.write() = ix;
                        *v.applier.lock() = applier;
                        j = nj;
                        v.unsaved.store(u64::MAX, Ordering::Relaxed);
                        continue;
                    }
                    Err(e) => {
                        log::error!("{}: rescan failed: {e}", v.letter);
                        std::thread::sleep(Duration::from_secs(30));
                        continue;
                    }
                }
            }
            Err(JournalError::Io(e)) => {
                log::error!("{}: journal read failed: {e}", v.letter);
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        // Coalesce bursts (e.g. unzipping) into one write-lock.
        let mut batch = first;
        if let Ok(more) = j.drain() {
            batch.extend(more);
        }
        if batch.is_empty() {
            continue;
        }
        let list = {
            let mut ix = v.index.write();
            let list = v.applier.lock().apply(&mut ix, &batch);
            ix.journal_id = j.journal_id;
            ix.next_usn = j.next_usn;
            list
        };
        refresh(v.letter, &v.index, &list);
        let unsaved = v.unsaved.fetch_add(batch.len() as u64, Ordering::Relaxed).saturating_add(batch.len() as u64);
        if unsaved >= SAVE_THRESHOLD && last_save.elapsed() > Duration::from_secs(300) {
            let mut ix = v.index.write();
            if let Err(e) = persist::save(&mut ix, &persist::db_path(v.letter)) {
                log::error!("{}: save failed: {e}", v.letter);
            }
            v.unsaved.store(0, Ordering::Relaxed);
            last_save = Instant::now();
        }
    }
}
