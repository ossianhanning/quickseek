//! NAS agent volumes (see `docs/design/nas-agent.md`): a share served by
//! `quickseek-agent` is a volume keyed by its UNC root whose index is a
//! replica of the agent's, kept current by the changes it streams. The
//! agent's entry ids are the index's record numbers, so changes apply like
//! USN journal records. The replica is saved like any index and serves
//! while the agent can't be reached.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use quickseek_remote::client::{self, Error};
use quickseek_remote::wire::{AgentSnap, Msg, Op, Record};
use quickseek_remote::{FIRST_ID, ROOT_ID, pair};
use serde::{Deserialize, Serialize};

use crate::config::{RemoteConfig, RemoteShare};
use crate::engine::Volume;
use crate::fold::FoldMode;
use crate::history::History;
use crate::index::{Index, IndexOptions};
use crate::ntfs::{self, Meta, Scan, ScanName};
use crate::persist;

/// States a remote volume shows (besides the agent's own, e.g. "agent: walking").
pub const CONNECTING: &str = "connecting";
pub const RECEIVING: &str = "receiving";
pub const CACHED: &str = "cached";
pub const OFFLINE: &str = "offline";
pub const PAIRING_NEEDED: &str = "pairing needed";
pub const CERT_CHANGED: &str = "certificate changed: pair again";

/// Connection and handshake timeout.
const TIMEOUT: Duration = Duration::from_secs(15);
/// The agent pings every 30 s; this much silence means the connection is gone.
const SILENCE: Duration = Duration::from_secs(90);

// ---- what's kept on disk ---------------------------------------------------------

/// Per agent, next to the indexes (not in the config, which clients get).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Secret {
    /// Token from pairing, hex.
    pub token: String,
    /// Newest agent snapshot imported, per share (Unix seconds).
    pub imported: HashMap<String, i64>,
}

fn safe(host: &str) -> String {
    host.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

pub fn secret_path(host: &str) -> PathBuf {
    persist::db_dir()
        .join("remote")
        .join(format!("{}.json", safe(host)))
}

pub fn load_secret(host: &str) -> Option<Secret> {
    serde_json::from_slice(&std::fs::read(secret_path(host)).ok()?).ok()
}

pub fn save_secret(host: &str, s: &Secret) -> std::io::Result<()> {
    let p = secret_path(host);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s).unwrap())?;
    std::fs::rename(tmp, p)
}

/// Saved replica of a remote share.
pub fn cache_path(key: &str) -> PathBuf {
    let f = persist::folder_db_path(key);
    persist::db_dir()
        .join("remote")
        .join(f.file_name().unwrap_or_default())
}

pub fn pc_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "PC".into())
}

/// The UNC root a share gets by default.
pub fn default_unc(host: &str, share: &str) -> String {
    format!(r"\\{host}\{share}")
}

/// Pairs with the agent on `host` using its code; saves the token and
/// returns the config entry to add.
pub fn pair_with(host: &str, port: u16, code: &str) -> Result<RemoteConfig, String> {
    let p = client::pair(host, port, code, &pc_name(), crate::ipc::BUILD, TIMEOUT)
        .map_err(|e| e.to_string())?;
    let mut secret = load_secret(host).unwrap_or_default();
    secret.token = pair::hex(&p.token);
    save_secret(host, &secret).map_err(|e| format!("saving the pairing: {e}"))?;
    Ok(RemoteConfig {
        host: host.to_string(),
        port,
        fingerprint: pair::hex(&p.fingerprint),
        agent: p.agent,
        shares: p
            .shares
            .into_iter()
            .map(|s| RemoteShare {
                unc: default_unc(host, &s.name),
                name: s.name,
                root: s.root,
                enabled: true,
            })
            .collect(),
    })
}

/// Forgets an agent's token (the config entry is removed by the caller).
pub fn forget(host: &str) {
    let _ = std::fs::remove_file(secret_path(host));
}

// ---- managing agents (the service's side of `quickseek remotes`) -----------------

/// NAS agent actions, JSON in and out (sent over the pipe):
/// - `{"op":"list"}` → every agent with its shares, their state and size;
/// - `{"op":"add","host":…,"port":…,"code":…}` pairs (or pairs again) and
///   indexes its shares → the agent's entry;
/// - `{"op":"remove","host":…}` forgets it, its token and its saved copies;
/// - `{"op":"enable","unc":…,"enabled":bool}` turns one share on or off.
pub fn action(eng: &crate::engine::Engine, json: &str) -> Result<String, String> {
    use serde_json::{Value, json};
    let a: Value = serde_json::from_str(json).map_err(|e| format!("bad request: {e}"))?;
    let s = |k: &str| a.get(k).and_then(Value::as_str).unwrap_or("").trim();
    let host = s("host").to_string();
    let mut cfg = eng.config();
    let out = match s("op") {
        "list" => {
            let status = eng.status();
            Value::Array(
                cfg.remotes
                    .iter()
                    .map(|r| {
                        let shares: Vec<Value> = r
                            .shares
                            .iter()
                            .map(|sh| {
                                let st = status
                                    .iter()
                                    .find(|v| v.key.eq_ignore_ascii_case(&sh.unc));
                                json!({
                                    "name": sh.name, "root": sh.root, "unc": sh.unc,
                                    "enabled": sh.enabled,
                                    "state": st.map_or(if sh.enabled { "starting" } else { "off" }, |v| v.state.as_str()),
                                    "entries": st.map_or(0, |v| v.entries),
                                })
                            })
                            .collect();
                        json!({
                            "host": r.host, "port": r.port, "agent": r.agent,
                            "fingerprint": pair::show_fingerprint(&pair::unhex::<32>(&r.fingerprint).unwrap_or_default()),
                            "paired": load_secret(&r.host).is_some_and(|s| !s.token.is_empty()),
                            "shares": shares,
                        })
                    })
                    .collect(),
            )
        }
        "add" => {
            if host.is_empty() {
                return Err("which host?".into());
            }
            let port = a
                .get("port")
                .and_then(Value::as_u64)
                .map_or(quickseek_remote::DEFAULT_PORT, |p| p as u16);
            let mut rc = pair_with(&host, port, s("code"))?;
            // Pairing again keeps the choices made for its shares.
            if let Some(old) = cfg
                .remotes
                .iter()
                .find(|r| r.host.eq_ignore_ascii_case(&host))
            {
                for sh in &mut rc.shares {
                    if let Some(o) = old.shares.iter().find(|o| o.name == sh.name) {
                        sh.unc = o.unc.clone();
                        sh.enabled = o.enabled;
                    }
                }
            }
            cfg.remotes.retain(|r| !r.host.eq_ignore_ascii_case(&host));
            cfg.remotes.push(rc.clone());
            eng.set_config(cfg).map_err(|e| format!("{e:#}"))?;
            serde_json::to_value(&rc).unwrap()
        }
        "remove" => {
            let Some(old) = cfg
                .remotes
                .iter()
                .find(|r| r.host.eq_ignore_ascii_case(&host))
                .cloned()
            else {
                return Err(format!("{host} isn't a NAS agent here"));
            };
            cfg.remotes.retain(|r| !r.host.eq_ignore_ascii_case(&host));
            eng.set_config(cfg).map_err(|e| format!("{e:#}"))?;
            forget(&host);
            for sh in &old.shares {
                let _ = std::fs::remove_file(cache_path(&sh.unc));
            }
            json!({ "removed": old.host })
        }
        "enable" => {
            let unc = s("unc");
            let on = a.get("enabled").and_then(Value::as_bool).unwrap_or(true);
            let sh = cfg
                .remotes
                .iter_mut()
                .flat_map(|r| r.shares.iter_mut())
                .find(|sh| sh.unc.eq_ignore_ascii_case(unc))
                .ok_or_else(|| format!("{unc} isn't a NAS share here"))?;
            sh.enabled = on;
            eng.set_config(cfg).map_err(|e| format!("{e:#}"))?;
            json!({ "unc": unc, "enabled": on })
        }
        op => return Err(format!("unknown NAS agent action {op:?}")),
    };
    Ok(out.to_string())
}

// ---- disk space ------------------------------------------------------------------

fn disks() -> &'static RwLock<HashMap<String, (u64, u64)>> {
    static D: OnceLock<RwLock<HashMap<String, (u64, u64)>>> = OnceLock::new();
    D.get_or_init(Default::default)
}

/// Capacity and free bytes the agent reported for the share at `root`.
pub fn disk_space(root: &str) -> Option<(u64, u64)> {
    disks()
        .read()
        .get(&root.trim_end_matches('\\').to_lowercase())
        .copied()
}

fn set_disk(key: &str, d: (u64, u64)) {
    disks()
        .write()
        .insert(key.trim_end_matches('\\').to_lowercase(), d);
}

// ---- building and updating the replica -------------------------------------------

/// An index of a full transfer, rooted at `key`.
pub fn build(key: &str, records: &[Record], fold: FoldMode, opts: IndexOptions) -> Index {
    let max = records
        .iter()
        .map(|r| r.id)
        .max()
        .unwrap_or(0)
        .max(FIRST_ID - 1);
    let mut scan = Scan {
        has_sizes: true,
        meta: vec![Meta::default(); max as usize + 1],
        ..Default::default()
    };
    scan.meta[ROOT_ID as usize] = Meta {
        in_use: true,
        is_dir: true,
        attrs: ntfs::usn::FILE_ATTRIBUTE_DIRECTORY,
        ..Default::default()
    };
    scan.names.push(ScanName {
        record: ROOT_ID,
        parent: ROOT_ID,
        off: 0,
        len: 1,
    });
    scan.strings.push(b'.');
    for r in records {
        scan.meta[r.id as usize] = Meta {
            in_use: true,
            is_dir: r.is_dir(),
            seq: 0,
            attrs: r.attrs,
            size: if r.is_dir() { 0 } else { r.size },
            created: r.created,
            modified: r.modified,
        };
        scan.names.push(ScanName {
            record: r.id,
            parent: r.parent,
            off: scan.strings.len() as u32,
            len: r.name.len() as u32,
        });
        scan.strings.extend_from_slice(r.name.as_bytes());
    }
    let letter = key.chars().next().unwrap_or('\\');
    let mut ix = Index::from_scan_with(letter, &crate::folder::root_name(key), &scan, fold, opts);
    ix.id_map = None;
    ix
}

/// Applies a batch of changes. False if they don't fit the replica (an
/// unknown parent): it has to be sent again in full.
pub fn apply(ix: &mut Index, ops: &[Op], now: i64) -> bool {
    for op in ops {
        match op {
            Op::Create(r) => {
                let Some(p) = ix.dir_entry(r.parent) else {
                    return false;
                };
                if ix.links(r.id).next().is_some() {
                    ix.remove_record(r.id);
                }
                let i = ix.add_link(r.id, 0, p, &r.name, r.attrs, r.size, r.modified, r.created);
                ix.note_recent(i, now);
            }
            Op::Remove { id } => ix.remove_record(*id),
            Op::Move { id, parent, name } => {
                let (Some(i), Some(p)) = (ix.dir_entry(*id), ix.dir_entry(*parent)) else {
                    return false;
                };
                ix.rename_entry(i, p, name);
                ix.note_recent(i, now);
            }
            Op::Update {
                id,
                size,
                modified,
                created,
            } => {
                ix.set_file_info(*id, *size, *modified, *created);
                if let Some(i) = ix.dir_entry(*id) {
                    ix.note_recent(i, now);
                }
            }
        }
    }
    true
}

/// An agent snapshot as a history volume keyed by `key`.
pub fn snap_volume(key: &str, s: &AgentSnap) -> crate::history::SnapVolume {
    crate::history::SnapVolume {
        key: key.to_string(),
        capacity: s.capacity,
        free: s.free,
        folders: s
            .folders
            .iter()
            .enumerate()
            .map(|(i, f)| crate::history::SnapFolder {
                parent: f.parent,
                name: if i == 0 {
                    crate::folder::root_name(key)
                } else {
                    f.name.clone()
                },
                size: f.size,
                cloud: 0,
                files: f.files,
            })
            .collect(),
        files: s
            .files
            .iter()
            .map(|f| crate::history::SnapFile {
                parent: f.parent,
                name: f.name.clone(),
                size: f.size,
            })
            .collect(),
    }
}

// ---- the connection --------------------------------------------------------------

fn nap(v: &Volume, stop: &AtomicBool, d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end && !stop.load(Ordering::Relaxed) && v.alive.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Keeps `v` a replica of `share` on the agent until the volume goes away:
/// connects, resumes or receives it in full, applies changes, reconnects
/// with backoff.
pub(crate) fn watch(
    v: Arc<Volume>,
    rc: RemoteConfig,
    share: RemoteShare,
    stop: Arc<AtomicBool>,
    history: Arc<OnceLock<History>>,
) {
    let mut backoff = Duration::from_secs(1);
    let mut connected_once = false;
    let pin = pair::unhex::<32>(&rc.fingerprint);
    while !stop.load(Ordering::Relaxed) && v.alive.load(Ordering::Relaxed) {
        let away = if connected_once {
            OFFLINE
        } else if v.index.read().len_live() > 0 {
            CACHED
        } else {
            CONNECTING
        };
        let secret = load_secret(&rc.host).unwrap_or_default();
        let (Some(pin), Some(token)) = (pin, pair::unhex::<32>(&secret.token)) else {
            v.set_state(PAIRING_NEEDED);
            nap(&v, &stop, Duration::from_secs(30));
            continue;
        };
        let sess = client::connect(
            &rc.host,
            rc.port,
            pin,
            token,
            &pc_name(),
            crate::ipc::BUILD,
            TIMEOUT,
        );
        let mut sess = match sess {
            Ok(s) => s,
            Err(Error::PinMismatch) => {
                log::warn!("{}: {}", v.key, quickseek_remote::tls::PIN_MISMATCH);
                v.set_state(CERT_CHANGED);
                nap(&v, &stop, Duration::from_secs(60));
                continue;
            }
            Err(Error::Refused(e)) => {
                log::warn!("{}: the agent refused: {e}", v.key);
                v.set_state(if e.contains("not paired") {
                    PAIRING_NEEDED
                } else {
                    away
                });
                nap(&v, &stop, Duration::from_secs(60));
                continue;
            }
            Err(e) => {
                log::debug!("{}: {e}", v.key);
                v.set_state(away);
                nap(&v, &stop, backoff);
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        };
        backoff = Duration::from_secs(1);
        connected_once = true;
        if let Err(e) = session(&v, &rc, &share, &mut sess, &secret, &stop, &history)
            && v.alive.load(Ordering::Relaxed)
            && !stop.load(Ordering::Relaxed)
        {
            log::info!("{}: connection to {} lost ({e})", v.key, rc.host);
            v.set_state(OFFLINE);
            nap(&v, &stop, backoff);
        }
    }
}

fn session(
    v: &Arc<Volume>,
    rc: &RemoteConfig,
    share: &RemoteShare,
    sess: &mut client::Session,
    secret: &Secret,
    stop: &AtomicBool,
    history: &OnceLock<History>,
) -> Result<(), String> {
    let e = |e: Error| e.to_string();
    sess.set_read_timeout(SILENCE).map_err(|e| e.to_string())?;
    // Days of history the PC missed.
    if let Some(h) = history.get() {
        let since = secret.imported.get(&share.name).copied().unwrap_or(0);
        sess.send(&Msg::History { since }).map_err(e)?;
        if let Msg::Snapshots(list) = sess.recv().map_err(e)? {
            import_history(h, &v.key, &rc.host, &share.name, &list);
        }
    }
    let (epoch, seq) = {
        let ix = v.index.read();
        (ix.journal_id, ix.next_usn as u64)
    };
    sess.send(&Msg::Subscribe {
        share: share.name.clone(),
        epoch,
        seq,
    })
    .map_err(e)?;
    let mut last_ranks = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) || !v.alive.load(Ordering::Relaxed) {
            return Ok(());
        }
        match sess.recv().map_err(e)? {
            Msg::Full { epoch, seq, count } => {
                v.set_state(RECEIVING);
                let t0 = Instant::now();
                let mut z = Vec::new();
                loop {
                    match sess.recv().map_err(e)? {
                        Msg::Chunk(c) => z.extend(c),
                        Msg::FullEnd => break,
                        Msg::Ping => {}
                        m => return Err(format!("unexpected {m:?} in a full transfer")),
                    }
                }
                let records =
                    quickseek_remote::wire::decode_records(&z, count).map_err(|e| e.to_string())?;
                let (fold, opts, rules, generation, ix_epoch) = {
                    let ix = v.index.read();
                    (
                        ix.fold_mode,
                        ix.opts,
                        ix.rules.clone(),
                        ix.generation,
                        ix.epoch,
                    )
                };
                let mut ix = build(&v.key, &records, fold, opts);
                ix.apply_rules(rules);
                ix.journal_id = epoch;
                ix.next_usn = seq as i64;
                ix.generation = generation + 1;
                ix.epoch = ix_epoch + 1;
                *v.index.write() = ix;
                v.unsaved.store(u64::MAX, Ordering::Relaxed);
                crate::engine::rank_in_background(v.clone());
                log::info!(
                    "{}: received {count} entries ({:.1} MB) in {} ms",
                    v.key,
                    z.len() as f64 / 1e6,
                    t0.elapsed().as_millis()
                );
                v.set_state("ready");
            }
            Msg::Changes { seq, ops } => {
                let env = crate::date::DateEnv::system();
                let now = (env.to_filetime)(env.now_local_min);
                let ok = {
                    let mut ix = v.index.write();
                    let ok = apply(&mut ix, &ops, now);
                    ix.next_usn = seq as i64;
                    if !ok {
                        // Ask for it in full next time.
                        ix.journal_id = 0;
                    }
                    ok
                };
                v.add_unsaved(ops.len() as u64);
                if !ok {
                    return Err("changes didn't fit the copy; fetching it again".into());
                }
                crate::engine::maintain(v, &mut last_ranks);
                if v.state.lock().as_str() != "ready" {
                    v.set_state("ready");
                }
            }
            Msg::Disk { capacity, free } => set_disk(&v.key, (capacity, free)),
            Msg::State(s) => {
                if s != "ready" {
                    v.set_state(&format!("agent: {s}"));
                }
            }
            Msg::Ping | Msg::Pong => {}
            m => return Err(format!("unexpected {m:?}")),
        }
    }
}

fn import_history(h: &History, key: &str, host: &str, share: &str, list: &[AgentSnap]) {
    // Only finished days: today's comes from this PC's own snapshot.
    let today = crate::history::local_midnight(crate::history::now_unix());
    let mut newest = None;
    let mut added = 0;
    for s in list.iter().filter(|s| s.share == share && s.time < today) {
        match h.merge_volume(s.time, snap_volume(key, s)) {
            Ok(true) => added += 1,
            Ok(false) => {}
            Err(e) => {
                log::warn!("{key}: importing the agent's snapshot: {e}");
                return;
            }
        }
        newest = newest.max(Some(s.time));
    }
    if let Some(t) = newest {
        let mut sec = load_secret(host).unwrap_or_default();
        sec.imported.insert(share.to_string(), t);
        let _ = save_secret(host, &sec);
    }
    if added > 0 {
        log::info!("{key}: {added} days of history from the agent");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Query;
    use crate::search::{SearchOptions, search};
    use quickseek_remote::FILE_ATTRIBUTE_DIRECTORY;

    fn rec(id: u32, parent: u32, name: &str, dir: bool, size: u64) -> Record {
        Record {
            id,
            parent,
            name: name.into(),
            attrs: if dir { FILE_ATTRIBUTE_DIRECTORY } else { 0 },
            size,
            modified: 133_000_000_000_000_000,
            created: 0,
        }
    }

    fn paths(ix: &Index, q: &str) -> Vec<String> {
        let h = search(
            &[ix],
            &Query::parse(q).unwrap(),
            &SearchOptions {
                limit: 0,
                ..Default::default()
            },
        );
        let mut v: Vec<String> = h.items.iter().map(|h| ix.path(h.idx)).collect();
        v.sort();
        v
    }

    #[test]
    fn full_transfer_then_changes() {
        let key = r"\\nas\data";
        let recs = vec![
            rec(24, ROOT_ID, "Photos", true, 0),
            rec(25, 24, "a.jpg", false, 100),
            rec(26, ROOT_ID, "notes.txt", false, 5),
            // A folder moved under a newer one: parent id above its own.
            rec(27, 30, "old", true, 0),
            rec(30, ROOT_ID, "Archive", true, 0),
        ];
        let mut ix = build(key, &recs, FoldMode::Lower, IndexOptions::default());
        assert_eq!(paths(&ix, "a.jpg"), [r"\\nas\data\Photos\a.jpg"]);
        assert_eq!(paths(&ix, "old"), [r"\\nas\data\Archive\old"]);
        let root_size = |ix: &Index| ix.entries[ix.root as usize].size;
        assert_eq!(root_size(&ix), 105);
        let ops = vec![
            Op::Create(rec(31, 24, "b.jpg", false, 1000)),
            Op::Update {
                id: 25,
                size: 200,
                modified: 1,
                created: 0,
            },
            Op::Move {
                id: 24,
                parent: 30,
                name: "Pictures".into(),
            },
            Op::Remove { id: 26 },
        ];
        assert!(apply(&mut ix, &ops, 0));
        assert_eq!(
            paths(&ix, "jpg"),
            [
                r"\\nas\data\Archive\Pictures\a.jpg",
                r"\\nas\data\Archive\Pictures\b.jpg"
            ]
        );
        assert!(paths(&ix, "notes").is_empty());
        assert_eq!(root_size(&ix), 1200);
        // A create under an unknown folder: the replica is out of step.
        assert!(!apply(
            &mut ix,
            &[Op::Create(rec(40, 999, "x", false, 1))],
            0
        ));
    }

    #[test]
    fn replica_survives_save_and_load() {
        let key = r"\\nas\media";
        let recs = vec![
            rec(24, ROOT_ID, "TV", true, 0),
            rec(25, 24, "show.mkv", false, 7),
        ];
        let mut ix = build(key, &recs, FoldMode::Lower, IndexOptions::default());
        ix.journal_id = 99;
        ix.next_usn = 12;
        let d = std::env::temp_dir().join(format!("qs-remote-ix-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("r.db");
        persist::save(&mut ix, &p).unwrap();
        let mut back = persist::load(&p).unwrap();
        assert_eq!((back.journal_id, back.next_usn), (99, 12));
        assert!(apply(
            &mut back,
            &[Op::Create(rec(26, 24, "new.mkv", false, 3))],
            0
        ));
        assert_eq!(paths(&back, "mkv").len(), 2);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn disk_space_by_root() {
        set_disk(r"\\NAS\Data", (10, 4));
        assert_eq!(disk_space(r"\\nas\data\"), Some((10, 4)));
        assert_eq!(disk_space(r"\\nas\other"), None);
    }
}
