//! Ties it together: per-volume index lifecycle (load → replay journal, or
//! scan → catch up), live watcher threads, volumes appearing/disappearing,
//! configuration, run history and multi-volume search.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};

use crate::config::{Config, FolderConfig};
use crate::folder::{self, WatchEvent, Watcher};
use crate::index::{Index, IndexOptions};
use crate::live::{Applier, file_info_by_id};
use crate::ntfs::journal::{self, Journal, JournalError};
use crate::ntfs::{mft, usn, win};
use crate::persist;
use crate::query::{Query, SearchFlags};
use crate::runs::RunHistory;
use crate::search::{Extras, Hits, ResultSet, SearchOptions, search};

/// Journal records worth saving the index for; fewer are cheaper to replay
/// on the next start than rewriting hundreds of MB.
pub const SAVE_THRESHOLD: u64 = 20_000;

#[derive(Debug, Clone)]
pub enum LoadReport {
    /// Loaded from disk and replayed `replayed` journal records.
    Loaded { replayed: usize, ms: u128 },
    Scanned { method: &'static str, stats: Option<mft::MftStats>, caught_up: usize, ms: u128 },
    Walked { entries: usize, ms: u128 },
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
            LoadReport::Walked { entries, ms } => write!(f, "walked {entries} entries in {ms} ms"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeKind {
    Ntfs,
    Refs,
    Folder,
}

impl VolumeKind {
    pub fn label(self) -> &'static str {
        match self {
            VolumeKind::Ntfs => "NTFS",
            VolumeKind::Refs => "ReFS",
            VolumeKind::Folder => "Folder",
        }
    }
}

pub struct Volume {
    /// `C` for drives, the folder path for folder indexes.
    pub key: String,
    pub letter: char,
    pub kind: VolumeKind,
    pub index: RwLock<Index>,
    pub applier: Mutex<Applier>,
    /// Journal records applied since the last save (`u64::MAX` = never saved).
    pub unsaved: AtomicU64,
    /// Cleared when the volume disappears or is excluded; its watcher exits.
    pub alive: AtomicBool,
    /// Human-readable state: ready, scanning, cached, offline, waiting for credentials.
    pub state: Mutex<String>,
}

impl Volume {
    fn new(key: &str, letter: char, kind: VolumeKind, ix: Index, applier: Applier, unsaved: u64, state: &str) -> Arc<Volume> {
        Arc::new(Volume {
            key: key.to_string(),
            letter,
            kind,
            index: RwLock::new(ix),
            applier: Mutex::new(applier),
            unsaved: AtomicU64::new(unsaved),
            alive: AtomicBool::new(true),
            state: Mutex::new(state.to_string()),
        })
    }

    /// Counts `n` more unsaved changes; saturates so a never-saved volume
    /// (`u64::MAX`) stays that way. Returns the new count.
    fn add_unsaved(&self, n: u64) -> u64 {
        let old = self.unsaved.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |u| Some(u.saturating_add(n))).unwrap();
        old.saturating_add(n)
    }

    fn set_state(&self, s: &str) {
        let mut st = self.state.lock();
        if *st != s {
            log::info!("{}: {s}", self.key);
            *st = s.to_string();
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct VolumeStatus {
    pub key: String,
    pub kind: VolumeKind,
    pub entries: u64,
    pub ready: bool,
    pub state: String,
}

/// The identity network folders are scanned with (set by the service from
/// a connecting client; `None` = this process's own identity).
pub type TokenSlot = Arc<RwLock<Option<Arc<crate::ntfs::win::Token>>>>;

type Reporter = Box<dyn Fn(&str, &anyhow::Result<LoadReport>) + Send + Sync>;

#[derive(Default)]
pub struct EngineOptions {
    pub config_path: Option<PathBuf>,
    pub runs_path: Option<PathBuf>,
    pub force_scan: bool,
    /// Only these drive letters (empty = per config).
    pub only: Vec<char>,
    pub report: Option<Reporter>,
}

pub struct Engine {
    volumes: RwLock<Vec<Arc<Volume>>>,
    loading: Mutex<Vec<String>>,
    config: RwLock<Config>,
    config_version: AtomicU64,
    runs: RwLock<RunHistory>,
    set_version: AtomicU64,
    signature: Mutex<Option<(u32, u64)>>,
    opts: EngineOptions,
    stop: Arc<AtomicBool>,
    net_token: TokenSlot,
    /// Index options changed in a way that needs fresh data (date created on).
    rescan_requested: AtomicBool,
}

// ---- loading single volumes ------------------------------------------------------

fn refresh(letter: char, extended: bool, ix_lock: &RwLock<Index>, list: &[(u32, u128)]) {
    if list.is_empty() || !ix_lock.read().has_sizes {
        return;
    }
    let Ok(hint) = win::open_root_dir(letter) else { return };
    let infos: Vec<_> = list.iter().filter_map(|&(rec, id)| file_info_by_id(&hint, id, extended).map(|i| (rec, i))).collect();
    let mut ix = ix_lock.write();
    for (rec, (size, m, c)) in infos {
        ix.set_file_info(rec, size, m, c);
    }
}

/// Replays the journal from the index's cursor up to now.
fn catch_up(ix: &mut Index, applier: &mut Applier, j: &mut Journal) -> Result<(usize, Vec<(u32, u128)>), JournalError> {
    let recs = j.drain()?;
    let refresh = applier.apply(ix, &recs);
    ix.journal_id = j.journal_id;
    ix.next_usn = j.next_usn;
    Ok((recs.len(), refresh))
}

fn serial_of(letter: char) -> u32 {
    win::list_volumes().into_iter().find(|v| v.letter == letter).map_or(0, |v| v.serial)
}

pub fn scan_volume(letter: char, progress: &(dyn Fn(u64, u64) + Sync)) -> anyhow::Result<(Index, LoadReport, Journal, Applier)> {
    scan_journaled(letter, VolumeKind::Ntfs, IndexOptions::default(), progress)
}

fn scan_journaled(letter: char, kind: VolumeKind, opts: IndexOptions, progress: &(dyn Fn(u64, u64) + Sync)) -> anyhow::Result<(Index, LoadReport, Journal, Applier)> {
    let root_name = format!("{letter}:");
    let fold = crate::fold::FoldMode::Lower;
    let t0 = Instant::now();
    let serial = serial_of(letter);
    // Note the journal position *before* scanning so changes made during the
    // scan are replayed afterwards.
    let h = win::open_volume(letter, 0)?;
    let jd = journal::query_or_create(&h)?;

    let (mut ix, method, stats) = match kind {
        VolumeKind::Refs => {
            let root = win::root_file_id(letter)?;
            let (scan, map) = usn::scan_enum_v3(&h, root)?;
            let mut ix = Index::from_scan_with(letter, &root_name, &scan, fold, opts);
            ix.id_map = Some(map);
            (ix, "FSCTL_ENUM_USN_DATA v3 (ReFS)", None)
        }
        _ => {
            drop(h);
            match mft::scan(letter, progress) {
                Ok((s, st)) => (Index::from_scan_with(letter, &root_name, &s, fold, opts), "raw MFT", Some(st)),
                Err(e) => {
                    log::warn!("{letter}: raw MFT scan failed ({e}); falling back to FSCTL_ENUM_USN_DATA");
                    let h = win::open_volume(letter, 0)?;
                    (Index::from_scan_with(letter, &root_name, &usn::scan_enum(&h)?, fold, opts), "FSCTL_ENUM_USN_DATA", None)
                }
            }
        }
    };
    ix.volume_serial = serial;

    let mut j = Journal::open(letter, Some(jd.journal_id), Some(jd.next_usn)).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut applier = Applier::default();
    let (n, list) = catch_up(&mut ix, &mut applier, &mut j).map_err(|e| anyhow::anyhow!("{e}"))?;
    let lock = RwLock::new(ix);
    refresh(letter, kind == VolumeKind::Refs, &lock, &list);
    let ix = lock.into_inner();
    let report = LoadReport::Scanned { method, stats, caught_up: n, ms: t0.elapsed().as_millis() };
    Ok((ix, report, j, applier))
}

/// Loads the saved index for `letter` and replays the journal, or scans.
pub fn open_volume(letter: char, force_scan: bool, progress: &(dyn Fn(u64, u64) + Sync)) -> anyhow::Result<(Index, LoadReport, Journal, Applier)> {
    open_journaled(letter, VolumeKind::Ntfs, force_scan, IndexOptions::default(), progress)
}

fn open_journaled(letter: char, kind: VolumeKind, force_scan: bool, opts: IndexOptions, progress: &(dyn Fn(u64, u64) + Sync)) -> anyhow::Result<(Index, LoadReport, Journal, Applier)> {
    let t0 = Instant::now();
    if !force_scan {
        let serial = serial_of(letter);
        match persist::load(&persist::db_path(letter)) {
            // A saved index without date created can't serve an index that wants it: rescan.
            Ok(ix) if opts.created && ix.created.is_empty() => {
                log::info!("{letter}: date created now indexed; rescanning");
                drop(ix);
            }
            Ok(mut ix) if ix.letter == letter && ix.volume_serial == serial && (kind == VolumeKind::Refs) == ix.id_map.is_some() => {
                match Journal::open(letter, Some(ix.journal_id), Some(ix.next_usn)) {
                    Ok(mut j) => {
                        let mut applier = Applier::default();
                        if let Ok((n, list)) = catch_up(&mut ix, &mut applier, &mut j) {
                            let lock = RwLock::new(ix);
                            refresh(letter, kind == VolumeKind::Refs, &lock, &list);
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
    scan_journaled(letter, kind, opts, progress)
}

// ---- the engine ------------------------------------------------------------------

/// What should be indexed right now: (key, letter, kind, folder config).
fn desired(cfg: &Config, only: &[char]) -> Vec<(String, char, VolumeKind, Option<FolderConfig>)> {
    let mut out = Vec::new();
    for v in win::list_volumes() {
        if !only.is_empty() && !only.contains(&v.letter) {
            continue;
        }
        let kind = match v.fs.as_str() {
            "NTFS" => VolumeKind::Ntfs,
            "ReFS" => VolumeKind::Refs,
            _ => continue,
        };
        let auto = match v.drive_type {
            win::DRIVE_FIXED => cfg.auto_include_fixed,
            win::DRIVE_REMOVABLE => cfg.auto_include_removable,
            _ => false,
        };
        let include = match cfg.volumes.get(&v.letter.to_string()) {
            Some(vc) => vc.include,
            None => auto,
        };
        if include || !only.is_empty() {
            out.push((v.letter.to_string(), v.letter, kind, None));
        }
    }
    if only.is_empty() {
        // Folders stay indexed while unreachable (offline shares keep their
        // cached index); their watcher retries.
        for f in &cfg.folders {
            let key = folder::root_name(&f.path);
            if !key.is_empty() {
                out.push((key.clone(), key.chars().next().unwrap_or('?').to_ascii_uppercase(), VolumeKind::Folder, Some(f.clone())));
            }
        }
    }
    out
}

impl Engine {
    pub fn new(opts: EngineOptions) -> Engine {
        let config = opts.config_path.as_deref().map(Config::load).unwrap_or_default();
        let runs = opts.runs_path.as_deref().map(RunHistory::load).unwrap_or_default();
        Engine {
            volumes: RwLock::new(Vec::new()),
            loading: Mutex::new(Vec::new()),
            config: RwLock::new(config),
            config_version: AtomicU64::new(0),
            runs: RwLock::new(runs),
            set_version: AtomicU64::new(0),
            signature: Mutex::new(None),
            opts,
            stop: Arc::new(AtomicBool::new(false)),
            net_token: Arc::new(RwLock::new(None)),
            rescan_requested: AtomicBool::new(false),
        }
    }

    /// Network folders will be scanned as this user from now on (the
    /// service calls this with the identity of a connecting client).
    pub fn offer_token(&self, t: crate::ntfs::win::Token) {
        let mut slot = self.net_token.write();
        let first = slot.is_none();
        *slot = Some(Arc::new(t));
        if first {
            log::info!("received client credentials for network folders");
        }
    }

    pub fn has_token(&self) -> bool {
        self.net_token.read().is_some()
    }

    /// Opens the given volumes (all included ones when empty), synchronously.
    pub fn open(letters: &[char], force_scan: bool, on_report: impl Fn(char, &anyhow::Result<LoadReport>) + Send + Sync + 'static) -> Engine {
        let opts = EngineOptions {
            force_scan,
            only: letters.to_vec(),
            report: Some(Box::new(move |k, r| on_report(k.chars().next().unwrap_or('?'), r))),
            config_path: Some(persist::db_dir().join("config.json")),
            runs_path: Some(persist::db_dir().join("runs.json")),
        };
        let e = Engine::new(opts);
        e.sync_volumes();
        e
    }

    /// An engine over prebuilt indexes, without watchers (tests, mock servers).
    pub fn from_indexes(ixs: Vec<Index>) -> Engine {
        let e = Engine::new(EngineOptions::default());
        {
            let mut vols = e.volumes.write();
            for ix in ixs {
                let (key, letter) = (ix.letter.to_string(), ix.letter);
                vols.push(Volume::new(&key, letter, VolumeKind::Ntfs, ix, Applier::default(), 0, "ready"));
            }
        }
        e
    }

    fn report(&self, key: &str, r: &anyhow::Result<LoadReport>) {
        if let Some(f) = &self.opts.report {
            f(key, r);
        }
        match r {
            Ok(rep) => log::info!("{key}: {rep}"),
            Err(e) => log::error!("{key}: {e:#}"),
        }
    }

    /// Re-syncs if drives or the config changed since the last call (cheap
    /// enough to poll every few seconds).
    pub fn sync_if_changed(&self) {
        let sig = (win::logical_drives(), self.config_version.load(Ordering::Relaxed));
        if *self.signature.lock() == Some(sig) {
            return;
        }
        self.sync_volumes();
        *self.signature.lock() = Some(sig);
    }

    /// Adds newly present/included volumes and drops removed/excluded ones.
    pub fn sync_volumes(&self) {
        let cfg = self.config.read().clone();
        let want = desired(&cfg, &self.opts.only);
        if self.rescan_requested.swap(false, Ordering::Relaxed) {
            // Rebuild each volume in the background of the old one: searches
            // keep using the old index until the new one replaces it.
            for old in self.volumes() {
                let Some(w) = want.iter().find(|w| w.0 == old.key) else { continue };
                match self.load(&w.0, w.1, w.2, w.3.clone(), &cfg, true) {
                    Ok((v, rep)) => {
                        self.report(&w.0, &Ok(rep));
                        let mut vols = self.volumes.write();
                        if let Some(slot) = vols.iter_mut().find(|x| x.key == old.key) {
                            *slot = v;
                        }
                        old.alive.store(false, Ordering::Relaxed);
                        self.set_version.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => self.report(&w.0, &Err(e)),
                }
            }
        }
        {
            let mut vols = self.volumes.write();
            let before = vols.len();
            vols.retain(|v| {
                let keep = want.iter().any(|w| w.0 == v.key && w.2 == v.kind);
                if !keep {
                    v.alive.store(false, Ordering::Relaxed);
                    log::info!("{}: removed from index", v.key);
                }
                keep
            });
            if vols.len() != before {
                self.set_version.fetch_add(1, Ordering::Relaxed);
            }
        }
        for (key, letter, kind, fcfg) in want {
            if self.volumes.read().iter().any(|v| v.key == key) {
                continue;
            }
            self.loading.lock().push(key.clone());
            let res = self.load(&key, letter, kind, fcfg, &cfg, self.opts.force_scan);
            self.loading.lock().retain(|k| *k != key);
            match res {
                Ok((v, rep)) => {
                    self.report(&key, &Ok(rep));
                    self.volumes.write().push(v);
                    self.set_version.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => self.report(&key, &Err(e)),
            }
        }
    }

    fn load(&self, key: &str, letter: char, kind: VolumeKind, fcfg: Option<FolderConfig>, cfg: &Config, force_scan: bool) -> anyhow::Result<(Arc<Volume>, LoadReport)> {
        let fold = cfg.fold_mode();
        let rules = cfg.rules(letter);
        let opts = cfg.index_options();
        match kind {
            VolumeKind::Ntfs | VolumeKind::Refs => {
                let (mut ix, rep, j, applier) = open_journaled(letter, kind, force_scan, opts, &|_, _| {})?;
                ix.set_options(opts);
                ix.set_fold_mode(fold);
                ix.apply_rules(rules);
                let unsaved = match rep {
                    LoadReport::Loaded { replayed, .. } => replayed as u64,
                    _ => u64::MAX,
                };
                let missing = ix.ranks_missing();
                let v = Volume::new(key, letter, kind, ix, applier, unsaved, "ready");
                if missing {
                    rank_in_background(v.clone());
                }
                let (vv, stop) = (v.clone(), self.stop.clone());
                std::thread::Builder::new().name(format!("usn-{letter}")).spawn(move || watch_journal(vv, j, stop))?;
                Ok((v, rep))
            }
            VolumeKind::Folder => {
                // Serve the cached copy (or just the root) right away; the
                // watcher thread walks the folder in the background.
                let fcfg = fcfg.unwrap_or_default();
                let t0 = Instant::now();
                let cached = persist::load(&persist::folder_db_path(key)).ok();
                let had_cache = cached.is_some();
                let mut ix = cached.unwrap_or_else(|| folder::empty(key, fold));
                ix.set_options(opts);
                ix.set_fold_mode(fold);
                ix.apply_rules(rules);
                let rep = LoadReport::Walked { entries: ix.len_live(), ms: t0.elapsed().as_millis() };
                let missing = ix.ranks_missing();
                let v = Volume::new(key, letter, kind, ix, Applier::default(), 0, if had_cache { "cached" } else { "scanning" });
                if missing {
                    rank_in_background(v.clone());
                }
                let (vv, stop, tok) = (v.clone(), self.stop.clone(), self.net_token.clone());
                std::thread::Builder::new().name(format!("dir-{letter}")).spawn(move || watch_folder(vv, fcfg, stop, tok))?;
                Ok((v, rep))
            }
        }
    }

    pub fn volume_count(&self) -> usize {
        self.volumes.read().len()
    }

    pub fn volumes(&self) -> Vec<Arc<Volume>> {
        self.volumes.read().clone()
    }

    pub fn status(&self) -> Vec<VolumeStatus> {
        let mut v: Vec<VolumeStatus> = self
            .volumes
            .read()
            .iter()
            .map(|v| VolumeStatus {
                key: v.key.clone(),
                kind: v.kind,
                entries: v.index.read().len_visible() as u64,
                ready: true,
                state: v.state.lock().clone(),
            })
            .collect();
        for k in self.loading.lock().iter() {
            v.push(VolumeStatus { key: k.clone(), kind: VolumeKind::Ntfs, entries: 0, ready: false, state: "loading".into() });
        }
        v
    }

    pub fn is_loading(&self) -> bool {
        !self.loading.lock().is_empty()
    }

    /// Runs `f` with read access to every volume's index.
    pub fn with_indexes<R>(&self, f: impl FnOnce(&[&Index]) -> R) -> R {
        let vols = self.volumes.read();
        let guards: Vec<_> = vols.iter().map(|v| v.index.read()).collect();
        let refs: Vec<&Index> = guards.iter().map(|g| &**g).collect();
        f(&refs)
    }

    pub fn search(&self, q: &Query, opts: &SearchOptions) -> Hits {
        self.with_indexes(|ixs| search(ixs, q, opts))
    }

    /// Parses and runs a query into a pageable result set.
    pub fn query(&self, text: &str, flags: SearchFlags, opts: &SearchOptions) -> Result<ResultSet, String> {
        let (fold, created) = {
            let c = self.config.read();
            (c.fold_mode(), c.index_date_created)
        };
        let q = Query::parse_with(text, flags, fold, &crate::date::DateEnv::system())?;
        let have_created = created || self.with_indexes(|ixs| ixs.iter().any(|i| !i.created.is_empty()));
        if (q.uses_created || opts.sort == crate::search::SortKey::Created) && !have_created {
            return Err("Date created isn't indexed. Turn on Options → Index → \"Index date created\" (rescans once).".into());
        }
        let runs = self.runs.read();
        Ok(self.with_indexes(|ixs| ResultSet::new(ixs, &q, opts, Extras { runs: Some(&runs) })))
    }

    /// Changes whenever any index (or the set of volumes) changes.
    pub fn generation(&self) -> u64 {
        let g: u64 = self.volumes.read().iter().map(|v| v.index.read().generation).sum();
        g.wrapping_add(self.set_version.load(Ordering::Relaxed) << 40)
    }

    pub fn config(&self) -> Config {
        self.config.read().clone()
    }

    /// Applies and saves a new configuration. Rules and diacritic mode take
    /// effect immediately; volume/folder changes on the next sync.
    pub fn set_config(&self, c: Config) -> anyhow::Result<()> {
        if let Some(p) = &self.opts.config_path {
            c.save(p)?;
        }
        let fold = c.fold_mode();
        let opts = c.index_options();
        if opts.created && !self.config.read().index_date_created {
            self.rescan_requested.store(true, Ordering::Relaxed);
        }
        for v in self.volumes.read().iter() {
            let rules = c.rules(v.letter);
            let mut ix = v.index.write();
            let gained_ranks = (1..4).any(|k| opts.fast_sort[k] && !ix.opts.fast_sort[k]);
            ix.set_options(opts);
            ix.set_fold_mode(fold);
            if ix.rules != rules {
                ix.apply_rules(rules);
            }
            drop(ix);
            if gained_ranks {
                rank_in_background(v.clone());
            }
        }
        *self.config.write() = c;
        self.config_version.fetch_add(1, Ordering::Relaxed);
        self.set_version.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Finds the entry for a full path.
    pub fn lookup_path(&self, path: &str) -> Option<(usize, u32)> {
        let path = path.trim_end_matches('\\').replace('/', "\\");
        self.with_indexes(|ixs| {
            // Longest matching root wins (folder indexes inside drives).
            let mut best: Option<(usize, usize)> = None;
            for (v, ix) in ixs.iter().enumerate() {
                let root = ix.name(ix.root);
                let matches = path.len() >= root.len()
                    && path[..root.len()].eq_ignore_ascii_case(root)
                    && (path.len() == root.len() || path.as_bytes()[root.len()] == b'\\');
                if matches && best.is_none_or(|(_, l)| root.len() > l) {
                    best = Some((v, root.len()));
                }
            }
            let (v, rl) = best?;
            let ix = ixs[v];
            folder::resolve(ix, &path[rl..]).map(|i| (v, i))
        })
    }

    /// Counts a run of `path`; returns the new count.
    pub fn record_run(&self, path: &str) -> Option<u32> {
        let (v, i) = self.lookup_path(path)?;
        let (serial, record) = self.with_indexes(|ixs| (ixs[v].volume_serial, ixs[v].entries[i as usize].record));
        let now = crate::date::DateEnv::system().now_local_min;
        let info = self.runs.write().record(serial, record, now);
        if let Some(p) = &self.opts.runs_path
            && let Err(e) = self.runs.read().save(p)
        {
            log::warn!("saving run history: {e}");
        }
        self.set_version.fetch_add(1, Ordering::Relaxed);
        Some(info.count)
    }

    pub fn runs(&self) -> parking_lot::RwLockReadGuard<'_, RunHistory> {
        self.runs.read()
    }

    /// Saves volumes with at least `min_changes` unsaved changes (folder
    /// indexes go to their cache file).
    pub fn save_all(&self, min_changes: u64) -> anyhow::Result<()> {
        for v in self.volumes.read().iter() {
            let n = v.unsaved.load(Ordering::Relaxed);
            if n > 0 && n >= min_changes {
                let path = if v.kind == VolumeKind::Folder { persist::folder_db_path(&v.key) } else { persist::db_path(v.letter) };
                let mut ix = v.index.write();
                persist::save(&mut ix, &path)?;
                v.unsaved.store(0, Ordering::Relaxed);
            }
        }
        Ok(())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for v in self.volumes.read().iter() {
            v.alive.store(false, Ordering::Relaxed);
        }
    }
}

/// Computes sort ranks under a read lock and installs them if the index
/// didn't change shape meanwhile (retries a few times otherwise).
fn rank_in_background(v: Arc<Volume>) {
    let _ = std::thread::Builder::new().name(format!("ranks-{}", v.letter)).spawn(move || {
        for _ in 0..5 {
            let t0 = Instant::now();
            let (snapshot, len, epoch, mode) = {
                let ix = v.index.read();
                (ix.ranks_snapshot(), ix.entries.len(), ix.epoch, ix.fold_mode)
            };
            let mut ix = v.index.write();
            if ix.entries.len() == len && ix.epoch == epoch && ix.fold_mode == mode {
                ix.ranks = snapshot;
                log::info!("{}: sort ranks ready in {} ms", v.key, t0.elapsed().as_millis());
                return;
            }
        }
    });
}

/// Housekeeping after a batch of changes: re-apply exclude rules after
/// folder moves, and refresh stale sort ranks (computed under a read lock).
fn maintain(v: &Volume, last_ranks: &mut Instant) {
    if v.index.read().rules_stale {
        let mut ix = v.index.write();
        let rules = ix.rules.clone();
        ix.apply_rules(rules);
    }
    if last_ranks.elapsed() > Duration::from_secs(10) && v.index.read().ranks_need_refresh() {
        let (snapshot, len, epoch, mode) = {
            let ix = v.index.read();
            (ix.ranks_snapshot(), ix.entries.len(), ix.epoch, ix.fold_mode)
        };
        let mut ix = v.index.write();
        // Only this watcher thread adds entries, but config changes could
        // have refolded or compacted meanwhile.
        if ix.entries.len() == len && ix.epoch == epoch && ix.fold_mode == mode {
            ix.ranks = snapshot;
        }
        *last_ranks = Instant::now();
    }
}

fn watch_journal(v: Arc<Volume>, mut j: Journal, stop: Arc<AtomicBool>) {
    let mut last_save = Instant::now();
    let mut last_ranks = Instant::now();
    let refs = v.kind == VolumeKind::Refs;
    let mut errors = 0u32;
    while !stop.load(Ordering::Relaxed) && v.alive.load(Ordering::Relaxed) {
        // Blocks until the volume changes.
        let first = match j.read(true) {
            Ok(b) => {
                errors = 0;
                b
            }
            Err(JournalError::Gap) => {
                log::warn!("{}: journal gap; rescanning", v.key);
                let opts = v.index.read().opts;
                match scan_journaled(v.letter, v.kind, opts, &|_, _| {}) {
                    Ok((mut ix, _, nj, applier)) => {
                        let (fold, rules) = {
                            let old = v.index.read();
                            (old.fold_mode, old.rules.clone())
                        };
                        ix.set_fold_mode(fold);
                        ix.apply_rules(rules);
                        ix.epoch = v.index.read().epoch + 1;
                        *v.index.write() = ix;
                        *v.applier.lock() = applier;
                        j = nj;
                        v.unsaved.store(u64::MAX, Ordering::Relaxed);
                        continue;
                    }
                    Err(e) => {
                        log::error!("{}: rescan failed: {e}", v.key);
                        std::thread::sleep(Duration::from_secs(30));
                        continue;
                    }
                }
            }
            Err(JournalError::Io(e)) => {
                errors += 1;
                if errors > 5 && !win::list_volumes().iter().any(|x| x.letter == v.letter) {
                    log::info!("{}: volume gone; watcher exits", v.key);
                    return;
                }
                log::error!("{}: journal read failed: {e}", v.key);
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
        refresh(v.letter, refs, &v.index, &list);
        maintain(&v, &mut last_ranks);
        let unsaved = v.add_unsaved(batch.len() as u64);
        if unsaved >= SAVE_THRESHOLD && last_save.elapsed() > Duration::from_secs(300) {
            let mut ix = v.index.write();
            if let Err(e) = persist::save(&mut ix, &persist::db_path(v.letter)) {
                log::error!("{}: save failed: {e}", v.key);
            }
            v.unsaved.store(0, Ordering::Relaxed);
            last_save = Instant::now();
        }
    }
}

/// Walks the folder again and swaps the result in (then caches it).
fn rescan_folder(v: &Volume, token: Option<Arc<crate::ntfs::win::Token>>) {
    let (fold, rules, serial, generation, epoch) = {
        let ix = v.index.read();
        (ix.fold_mode, ix.rules.clone(), ix.volume_serial, ix.generation, ix.epoch)
    };
    let t0 = Instant::now();
    let opts = v.index.read().opts;
    let mut ix = folder::build_as(&v.key, fold, token, opts);
    ix.volume_serial = serial;
    ix.apply_rules(rules);
    ix.generation = generation + 1;
    ix.epoch = epoch + 1;
    log::info!("{}: walked {} entries in {} ms", v.key, ix.len_live(), t0.elapsed().as_millis());
    if let Err(e) = persist::save(&mut ix, &persist::folder_db_path(&v.key)) {
        log::warn!("{}: caching folder index failed: {e}", v.key);
    }
    *v.index.write() = ix;
    v.unsaved.store(0, Ordering::Relaxed);
}

/// Sleeps up to `d`, waking early when the volume is dropped.
fn nap(v: &Volume, stop: &AtomicBool, d: Duration) {
    let t = Instant::now();
    while t.elapsed() < d && v.alive.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Folder index lifecycle: initial walk (in the background), change
/// watching, periodic rescans, and riding out unreachable network shares.
fn watch_folder(v: Arc<Volume>, cfg: FolderConfig, stop: Arc<AtomicBool>, token: TokenSlot) {
    let root = PathBuf::from(format!("{}\\", v.key));
    let network = win::is_network_path(&v.key);
    let mut needs_walk = true;
    let mut last_scan = Instant::now();
    let mut last_ranks = Instant::now();
    let mut watcher: Option<Watcher> = None;
    let mut pending_old = None;
    while !stop.load(Ordering::Relaxed) && v.alive.load(Ordering::Relaxed) {
        // The service reaches shares as the user who last connected.
        let tok = if network { token.read().clone() } else { None };
        match &tok {
            Some(t) => {
                t.impersonate();
            }
            None => crate::ntfs::win::Token::revert(),
        }
        if needs_walk || (cfg.rescan_minutes > 0 && last_scan.elapsed() > Duration::from_secs(cfg.rescan_minutes as u64 * 60)) {
            if !root.is_dir() {
                let running_as_service = std::env::var_os("RAPIDFIND_SERVICE").is_some();
                v.set_state(if network && tok.is_none() && running_as_service { "waiting for credentials" } else { "offline" });
                watcher = None;
                needs_walk = true;
                nap(&v, &stop, Duration::from_secs(30));
                continue;
            }
            if v.state.lock().as_str() != "cached" {
                v.set_state("scanning");
            }
            rescan_folder(&v, tok.clone());
            v.set_state("ready");
            needs_walk = false;
            last_scan = Instant::now();
            if cfg.watch {
                watcher = Watcher::new(&root).ok();
            }
        }
        let Some(w) = watcher.as_mut() else {
            nap(&v, &stop, Duration::from_secs(1));
            continue;
        };
        match w.next(1000) {
            Ok(WatchEvent::Timeout) => {}
            Ok(WatchEvent::Overflow) => {
                log::info!("{}: change buffer overflowed; rescanning", v.key);
                needs_walk = true;
            }
            Ok(WatchEvent::Changes(changes)) => {
                let env = crate::date::DateEnv::system();
                let now_ft = (env.to_filetime)(env.now_local_min);
                let mut ok = true;
                {
                    let mut ix = v.index.write();
                    for (a, rel) in &changes {
                        ok &= folder::apply_change(&mut ix, &root, *a, rel, &mut pending_old, now_ft);
                    }
                }
                v.add_unsaved(changes.len() as u64);
                if !ok {
                    needs_walk = true;
                }
                maintain(&v, &mut last_ranks);
            }
            Err(e) => {
                // Typically a network share dropping: keep the index, retry.
                log::warn!("{}: watch failed ({e}); will rescan when reachable", v.key);
                watcher = None;
                needs_walk = true;
                nap(&v, &stop, Duration::from_secs(5));
            }
        }
    }
    crate::ntfs::win::Token::revert();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::sample;

    #[test]
    fn from_indexes_queries_config_and_runs() {
        let mut ix = sample();
        ix.volume_serial = 9;
        let e = Engine::from_indexes(vec![ix]);
        assert_eq!(e.volume_count(), 1);
        let rs = e.query("note", SearchFlags::default(), &SearchOptions::default()).unwrap();
        assert_eq!(rs.total, 2);
        assert_eq!(e.lookup_path(r"C:\Users\Me\notes.txt").map(|x| x.0), Some(0));
        assert_eq!(e.lookup_path(r"C:\Users\Me\nope.txt"), None);
        assert_eq!(e.record_run(r"C:\Users\Me\notes.txt"), Some(1));
        assert_eq!(e.record_run(r"C:\Users\Me\notes.txt"), Some(2));
        let rs = e.query("runcount:2", SearchFlags::default(), &SearchOptions::default()).unwrap();
        assert_eq!(rs.total, 1);
        let g = e.generation();
        let c = Config { exclude_files: vec!["*.txt".into()], ..Default::default() };
        e.set_config(c).unwrap();
        assert_ne!(e.generation(), g);
        assert_eq!(e.query("note", SearchFlags::default(), &SearchOptions::default()).unwrap().total, 1);
        let c = Config { match_diacritics: false, ..Default::default() };
        e.set_config(c).unwrap();
        assert_eq!(e.query("note", SearchFlags::default(), &SearchOptions::default()).unwrap().total, 2);
    }

    #[test]
    fn date_created_option() {
        let mut ix = sample();
        ix.set_options(IndexOptions { created: false, ..Default::default() });
        assert!(ix.created.is_empty());
        let e = Engine::from_indexes(vec![ix]);
        let err = e.query("dc:2026", SearchFlags::default(), &SearchOptions::default()).err().unwrap();
        assert!(err.contains("isn't indexed"), "{err}");
        let sorted = SearchOptions { sort: crate::search::SortKey::Created, ..Default::default() };
        assert!(e.query("", SearchFlags::default(), &sorted).is_err());
        // Fast sorts off: same order, computed without ranks.
        let c = Config { fast_sort_size: false, fast_sort_modified: false, ..Default::default() };
        e.set_config(c).unwrap();
        let by_size = SearchOptions { sort: crate::search::SortKey::Size, descending: true, limit: 2 };
        let rs = e.query("file:", SearchFlags::default(), &by_size).unwrap();
        let names: Vec<String> = e.with_indexes(|ixs| rs.first_page().iter().map(|h| ixs[0].name(h.idx).to_string()).collect());
        assert_eq!(names, ["Report.PDF", "notepad.exe"]);
        assert!(e.with_indexes(|ixs| ixs[0].ranks.by[1].is_empty()));
    }

    #[test]
    fn folder_volumes_load_and_follow_changes() {
        let d = std::env::temp_dir().join(format!("rapidfind-engine-folder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("one.txt"), b"1").unwrap();
        let cfg_path = d.join("cfg").join("config.json");
        let key = folder::root_name(&d.to_string_lossy());
        let cfg = Config {
            auto_include_fixed: false,
            folders: vec![FolderConfig { path: key.clone(), watch: true, rescan_minutes: 0 }],
            ..Default::default()
        };
        cfg.save(&cfg_path).unwrap();
        let e = Engine::new(EngineOptions { config_path: Some(cfg_path), ..Default::default() });
        e.sync_volumes();
        assert_eq!(e.status().iter().filter(|s| s.kind == VolumeKind::Folder).count(), 1);
        let t = Instant::now();
        while e.query("one.txt", SearchFlags::default(), &SearchOptions::default()).unwrap().total == 0 {
            assert!(t.elapsed() < Duration::from_secs(5), "initial background walk didn't finish");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(e.status()[0].state, "ready");
        assert!(persist::folder_db_path(&key).exists(), "walked folder index is cached");
        std::fs::write(d.join("two.txt"), b"22").unwrap();
        let t = Instant::now();
        while e.query("two.txt", SearchFlags::default(), &SearchOptions::default()).unwrap().total == 0 {
            assert!(t.elapsed() < Duration::from_secs(5), "folder watcher didn't pick up the new file");
            std::thread::sleep(Duration::from_millis(20));
        }
        // Removing the folder from the config drops the volume.
        let mut c2 = e.config();
        c2.folders.clear();
        e.set_config(c2).unwrap();
        e.sync_if_changed();
        assert_eq!(e.volume_count(), 0);
        drop(e);
        std::thread::sleep(Duration::from_millis(1200));
        std::fs::remove_file(persist::folder_db_path(&key)).ok();
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn unreachable_folder_stays_listed_as_offline() {
        let d = std::env::temp_dir().join(format!("rapidfind-engine-offline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let cfg_path = d.join("config.json");
        let missing = d.join("not-there").to_string_lossy().into_owned();
        Config { auto_include_fixed: false, folders: vec![FolderConfig { path: missing.clone(), ..Default::default() }], ..Default::default() }
            .save(&cfg_path)
            .unwrap();
        let e = Engine::new(EngineOptions { config_path: Some(cfg_path), ..Default::default() });
        e.sync_volumes();
        let t = Instant::now();
        while e.status().first().map(|s| s.state.clone()) != Some("offline".into()) {
            assert!(t.elapsed() < Duration::from_secs(5), "{:?}", e.status());
            std::thread::sleep(Duration::from_millis(20));
        }
        // It comes back once the folder appears.
        std::fs::create_dir_all(d.join("not-there")).unwrap();
        std::fs::write(d.join("not-there").join("back.txt"), b"1").unwrap();
        drop(e);
        std::fs::remove_dir_all(&d).ok();
    }
}

/// Real SMB: indexes a folder through the loopback share and watches it.
/// Machine-specific, so ignored by default:
/// `RAPIDFIND_TEST_UNC=\\localhost\S\some\dir cargo test unc_share -- --ignored`
#[cfg(test)]
mod unc_tests {
    use super::*;

    #[test]
    #[ignore]
    fn unc_share_walk_and_watch() {
        let Ok(unc) = std::env::var("RAPIDFIND_TEST_UNC") else { return };
        let tmp = std::env::temp_dir().join(format!("rapidfind-unc-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let cfg_path = tmp.join("config.json");
        Config { auto_include_fixed: false, folders: vec![FolderConfig { path: unc.clone(), watch: true, rescan_minutes: 0 }], ..Default::default() }
            .save(&cfg_path)
            .unwrap();
        let e = Engine::new(EngineOptions { config_path: Some(cfg_path), ..Default::default() });
        let t = Instant::now();
        e.sync_volumes();
        while e.status().first().map(|s| s.state.clone()) != Some("ready".into()) {
            assert!(t.elapsed() < Duration::from_secs(60), "{:?}", e.status());
            std::thread::sleep(Duration::from_millis(50));
        }
        println!("walked {:?} in {} ms", e.status(), t.elapsed().as_millis());
        let probe = format!("rapidfind-unc-probe-{}.txt", std::process::id());
        let path = std::path::PathBuf::from(format!("{}\\{probe}", folder::root_name(&unc)));
        std::fs::write(&path, b"x").unwrap();
        let t = Instant::now();
        loop {
            let rs = e.query(&probe, SearchFlags::default(), &SearchOptions::default()).unwrap();
            if rs.total == 1 {
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(10), "change over SMB not seen");
            std::thread::sleep(Duration::from_millis(50));
        }
        println!("SMB change visible after {} ms", t.elapsed().as_millis());
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(persist::folder_db_path(&folder::root_name(&unc))).ok();
        std::fs::remove_dir_all(&tmp).ok();
    }
}
