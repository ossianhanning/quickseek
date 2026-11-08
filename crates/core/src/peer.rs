//! This PC as an agent for other PCs (two-way search between Windows PCs).
//!
//! The service serves its own NTFS/ReFS drives with the NAS agent's server
//! ([`quickseek_agent::server`]): same protocol, pairing, pinned certificate
//! and change log, so another PC adds this one exactly like a NAS
//! (`quickseek remotes add <this PC> --pair <code>`) and keeps a live
//! replica of each drive, keyed `\\<this PC>\C$`. Only local drives are
//! served, never the replicas of NAS shares or other PCs, so two PCs that
//! serve each other don't loop.
//!
//! Each served drive is an agent [`Share`] whose tree is kept in step with
//! the drive's index: the index notes the records its live updates touch
//! ([`Index::touched`]), and every second those are compared with the tree,
//! producing the ops the other PCs apply. A rebuilt index or new exclude
//! rules compare everything once. Entries excluded here aren't served; a
//! hard-linked file is served once (at one of its names).

use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLockWriteGuard};
use quickseek_agent::server::Agent;
use quickseek_agent::share::Share;
use quickseek_agent::state::Store;
use quickseek_agent::tree::{Meta, Tree};
use quickseek_remote::wire::Op;
use quickseek_remote::{FIRST_ID, ROOT_ID};

use crate::config::PeerConfig;
use crate::engine::{Engine, Volume, VolumeKind};
use crate::index::{Index, NONE};

/// Firewall rule the service adds while serving (removed when turned off).
pub const FIREWALL_RULE: &str = "Quickseek PC sharing";

/// Where the certificate, paired PCs and saved trees live.
pub fn dir() -> PathBuf {
    crate::persist::db_dir().join("peer")
}

/// The share name of drive `key` (`C` → `C$`, like the admin share, so
/// the other PC's paths are `\\THISPC\C$\...` and open over SMB). A folder
/// index (tests only) is named after its last folder.
pub fn share_name(key: &str) -> String {
    let k = key.trim_end_matches([':', '\\']);
    format!("{}$", k.rsplit('\\').next().unwrap_or(k))
}

/// The store's folder: SYSTEM and administrators only in the service (it
/// holds the private key and the paired PCs' tokens).
fn make_dir(d: &Path) -> std::io::Result<()> {
    if std::env::var_os("QUICKSEEK_SERVICE").is_some() {
        crate::ipc::create_private_dir(d)
    } else {
        std::fs::create_dir_all(d)
    }
}

/// The folder a served volume's tree is rooted at.
fn root_of(key: &str) -> String {
    if key.len() == 1 {
        format!("{key}:\\")
    } else {
        format!("{}\\", key.trim_end_matches('\\'))
    }
}

/// Tests: serve folder indexes too (they can't build an NTFS index without
/// administrator rights) and listen on loopback only.
#[doc(hidden)]
pub static TEST_MODE: AtomicBool = AtomicBool::new(false);

struct Served {
    key: String,
    share: Arc<Share>,
    /// Record number → tree id.
    ids: HashMap<u32, u32>,
    /// The index this tree is in step with (a rebuilt one is compared in full).
    volume: Option<std::sync::Weak<Volume>>,
    saved_seq: u64,
}

struct Running {
    cfg: PeerConfig,
    keys: Vec<String>,
    agent: Arc<Agent>,
    listening: SocketAddr,
    sync: Option<std::thread::JoinHandle<()>>,
    error: Option<String>,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);
/// Why the last start failed (shown by `quickseek peer`).
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

/// The drives that would be served now.
fn wanted(eng: &Engine, cfg: &PeerConfig) -> Vec<String> {
    let mut v: Vec<String> = eng
        .volumes()
        .iter()
        .filter(|v| {
            (matches!(v.kind, VolumeKind::Ntfs | VolumeKind::Refs)
                || (v.kind == VolumeKind::Folder && TEST_MODE.load(Ordering::Relaxed)))
                && cfg.serves(&v.key)
        })
        .map(|v| v.key.clone())
        .collect();
    v.sort();
    v
}

/// Starts, restarts or stops serving to match the config (the service calls
/// this every few seconds).
pub fn keep(eng: &Arc<Engine>) {
    let cfg = eng.config().peer;
    let keys = if cfg.enabled {
        wanted(eng, &cfg)
    } else {
        Vec::new()
    };
    let mut running = RUNNING.lock();
    if let Some(r) = running.as_ref()
        && r.cfg == cfg
        && r.keys == keys
    {
        return;
    }
    let was_on = running.is_some();
    if let Some(r) = running.take() {
        stop(r);
    }
    if !cfg.enabled {
        if was_on {
            let _ = firewall(None);
        }
        *LAST_ERROR.lock() = None;
        return;
    }
    if keys.is_empty() {
        return; // drives still loading
    }
    match start(eng.clone(), &cfg, keys) {
        Ok(r) => {
            *LAST_ERROR.lock() = None;
            *running = Some(r);
        }
        Err(e) => {
            let msg = format!("{e:#}");
            // Once per distinct error, not every few seconds.
            if LAST_ERROR.lock().as_deref() != Some(msg.as_str()) {
                log::error!("PC sharing: {msg}");
            }
            *LAST_ERROR.lock() = Some(msg);
        }
    }
}

/// Stops serving (service shutdown): saves the trees.
pub fn shut_down() {
    if let Some(r) = RUNNING.lock().take() {
        stop(r);
    }
}

fn start(eng: Arc<Engine>, cfg: &PeerConfig, keys: Vec<String>) -> anyhow::Result<Running> {
    use anyhow::Context;
    let dir = dir();
    make_dir(&dir)?;
    let store = Store::new(&dir)?;
    let (cert, key) = store.identity()?;
    let fingerprint = quickseek_remote::tls::fingerprint(&cert);
    let tls = quickseek_remote::tls::server_config(cert, key)?;
    let epoch = u64::from_le_bytes(quickseek_remote::pair::random());
    let mut served = Vec::new();
    for k in &keys {
        let share = Arc::new(Share::new(&share_name(k), Path::new(&root_of(k)), epoch));
        if let Err(e) = share.load(&dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!("PC sharing: {k}: the saved tree can't be used ({e}); comparing anew");
        }
        let ids = share.edit(|st, _| ids_of(&st.tree));
        let saved_seq = share.state.lock().unwrap().seq;
        served.push(Served {
            key: k.clone(),
            share,
            ids,
            volume: None,
            saved_seq,
        });
    }
    let host = if TEST_MODE.load(Ordering::Relaxed) {
        "127.0.0.1"
    } else {
        "0.0.0.0"
    };
    let listener = TcpListener::bind((host, cfg.port))
        .with_context(|| format!("listening on port {}", cfg.port))?;
    let listening = listener.local_addr()?;
    let stop = Arc::new(AtomicBool::new(false));
    let agent = Arc::new(Agent::new(
        crate::remote::pc_name(),
        served.iter().map(|s| s.share.clone()).collect(),
        store,
        tls,
        fingerprint,
        stop.clone(),
    ));
    let a = agent.clone();
    std::thread::Builder::new()
        .name("peer server".into())
        .spawn(move || a.serve(listener))?;
    let st = stop.clone();
    let sync = std::thread::Builder::new()
        .name("peer sync".into())
        .spawn(move || sync_loop(&eng, served, &st, &dir))?;
    let error = firewall(Some(cfg.port)).err();
    if let Some(e) = &error {
        log::warn!("PC sharing: the firewall rule: {e}");
    }
    log::info!(
        "PC sharing: serving {} on port {}; certificate {}",
        keys.iter()
            .map(|k| share_name(k))
            .collect::<Vec<_>>()
            .join(", "),
        listening.port(),
        quickseek_remote::pair::show_fingerprint(&fingerprint)
    );
    Ok(Running {
        cfg: cfg.clone(),
        keys,
        agent,
        listening,
        sync: Some(sync),
        error,
    })
}

fn stop(mut r: Running) {
    r.agent.shut_down(r.listening);
    if let Some(t) = r.sync.take() {
        let _ = t.join(); // saves on the way out
    }
}

/// Record → id for every entry of a (loaded) tree.
fn ids_of(t: &Tree) -> HashMap<u32, u32> {
    let mut ids = HashMap::with_capacity(t.len() + 1);
    for id in FIRST_ID..t.next_id() {
        if let Some(n) = t.get(id) {
            ids.insert(n.meta.ino as u32, id);
        }
    }
    ids
}

fn sync_loop(eng: &Engine, mut served: Vec<Served>, stop: &AtomicBool, dir: &Path) {
    let mut last_save = Instant::now();
    let mut last_disk = Instant::now() - Duration::from_secs(3600);
    while !stop.load(Ordering::Relaxed) {
        let vols = eng.volumes();
        for s in &mut served {
            match vols.iter().find(|v| v.key == s.key) {
                Some(v) => sync_share(s, v),
                None => s.share.set_status("not available on this PC right now"),
            }
        }
        if last_disk.elapsed() > Duration::from_secs(60) {
            for s in &served {
                let (c, f) = crate::tree::disk_space(&root_of(&s.key));
                s.share.edit(|st, _| {
                    if st.capacity != c || st.free.abs_diff(f) > (256 << 20) {
                        (st.capacity, st.free) = (c, f);
                    }
                });
            }
            last_disk = Instant::now();
        }
        if last_save.elapsed() > Duration::from_secs(600) {
            save(&mut served, dir);
            last_save = Instant::now();
        }
        for _ in 0..10 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    save(&mut served, dir);
}

fn save(served: &mut [Served], dir: &Path) {
    for s in served {
        let seq = s.share.state.lock().unwrap().seq;
        if seq != s.saved_seq {
            match s.share.save(dir) {
                Ok(_) => s.saved_seq = seq,
                Err(e) => log::warn!("PC sharing: saving {}: {e}", s.share.name),
            }
        }
    }
}

/// Brings one share in step with its drive's index.
fn sync_share(s: &mut Served, v: &Arc<Volume>) {
    let same_volume = s
        .volume
        .as_ref()
        .and_then(|w| w.upgrade())
        .is_some_and(|old| Arc::ptr_eq(&old, v));
    let mut w = v.index.write();
    if w.entries.is_empty() {
        return; // still loading
    }
    let touched = if same_volume { w.touched.take() } else { None };
    w.touched = Some(HashSet::new());
    let ix = RwLockWriteGuard::downgrade(w);
    if touched.as_ref().is_some_and(HashSet::is_empty) {
        return;
    }
    let t0 = Instant::now();
    let full = touched.is_none();
    let ids = &mut s.ids;
    let n = s.share.edit(|st, ops| {
        let first = !st.ready;
        st.tree.quiet = first;
        let mut sy = Sync {
            ix: &ix,
            tree: &mut st.tree,
            ids,
            ops,
            done: HashSet::new(),
            root_rec: ix.record(ix.root),
        };
        sy.tree.nodes[ROOT_ID as usize].meta.ino = sy.root_rec as u64;
        match &touched {
            Some(t) => {
                for &r in t {
                    sy.id_of(r, 0);
                }
            }
            None => sy.everything(),
        }
        let n = sy.ops.len();
        st.tree.quiet = false;
        if first || st.status != "ready" {
            st.ready = true;
            st.status = "ready".into();
        }
        n
    });
    s.volume = Some(Arc::downgrade(v));
    if full {
        log::info!(
            "PC sharing: {} compared in full ({} entries, {n} changes) in {} ms",
            s.share.name,
            s.share.state.lock().unwrap().tree.len(),
            t0.elapsed().as_millis()
        );
    }
}

/// One pass of comparing records with the tree.
struct Sync<'a> {
    ix: &'a Index,
    tree: &'a mut Tree,
    ids: &'a mut HashMap<u32, u32>,
    ops: &'a mut Vec<Op>,
    /// Records already brought in step in this pass.
    done: HashSet<u32>,
    root_rec: u32,
}

impl Sync<'_> {
    /// Every record, then removing what's gone.
    fn everything(&mut self) {
        for i in 0..self.ix.entries.len() as u32 {
            if self.ix.is_visible(i) {
                self.id_of(self.ix.record(i), 0);
            }
        }
        let gone: Vec<u32> = self
            .ids
            .keys()
            .copied()
            .filter(|r| !self.done.contains(r))
            .collect();
        for r in gone {
            self.remove(r);
        }
    }

    /// The entry served for record `r` (one name of a hard-linked file).
    fn entry(&self, r: u32) -> Option<u32> {
        let i = self.ix.dir_entry(r)?;
        self.ix.is_visible(i).then_some(i)
    }

    fn meta(&self, i: u32, r: u32) -> Meta {
        let e = &self.ix.entries[i as usize];
        Meta {
            attrs: e.attrs(),
            size: if e.is_dir() { 0 } else { e.size },
            modified: e.modified(),
            created: self.ix.created(i).unwrap_or(0),
            dev: 0,
            ino: r as u64,
        }
    }

    /// Brings record `r` (and the folders above it) in step; its tree id,
    /// or `None` if it isn't served (gone, excluded, below something that is).
    fn id_of(&mut self, r: u32, depth: u32) -> Option<u32> {
        if r == self.root_rec {
            return Some(ROOT_ID);
        }
        if self.done.contains(&r) {
            return self.ids.get(&r).copied();
        }
        self.done.insert(r);
        let Some(i) = self.entry(r) else {
            self.remove(r);
            return None;
        };
        let parent = self.ix.entries[i as usize].parent;
        let pid = if parent == NONE || depth > 4096 {
            None
        } else {
            self.id_of(self.ix.record(parent), depth + 1)
        };
        let Some(pid) = pid else {
            self.remove(r);
            return None;
        };
        let name = self.ix.name(i);
        let meta = self.meta(i, r);
        if let Some(&id) = self.ids.get(&r) {
            let n = self.tree.get(id).expect("mapped ids are live");
            if n.meta.is_dir() == meta.is_dir() {
                if n.parent != pid || *n.name != *name {
                    self.tree.move_to(id, pid, &name, self.ops);
                }
                self.tree.update(id, meta, self.ops);
                return Some(id);
            }
            // A file became a folder or back (a reused record): anew.
            self.remove(r);
        }
        let id = self.tree.create(pid, &name, meta, self.ops);
        self.ids.insert(r, id);
        Some(id)
    }

    /// Removes record `r` and everything below it from the tree.
    fn remove(&mut self, r: u32) {
        let Some(id) = self.ids.remove(&r) else {
            return;
        };
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            if let Some(node) = self.tree.get(n) {
                if n != id {
                    self.ids.remove(&(node.meta.ino as u32));
                }
                stack.extend(node.children.iter().copied());
            }
        }
        self.tree.remove(id, self.ops, &mut Vec::new());
    }
}

/// Adds (or with `None` removes) the inbound firewall rule for the port:
/// private and domain networks, local subnet only.
fn firewall(port: Option<u16>) -> Result<(), String> {
    if std::env::var_os("QUICKSEEK_SERVICE").is_none() {
        return Ok(()); // tests and `--local` runs leave the firewall alone
    }
    let netsh = |args: &[String]| -> Result<(), String> {
        use std::os::windows::process::CommandExt;
        let out = std::process::Command::new("netsh")
            .args(args)
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output()
            .map_err(|e| format!("netsh: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
    };
    let s = |x: &str| x.to_string();
    let _ = netsh(&[
        s("advfirewall"),
        s("firewall"),
        s("delete"),
        s("rule"),
        format!("name={FIREWALL_RULE}"),
    ]);
    let Some(port) = port else { return Ok(()) };
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    netsh(&[
        s("advfirewall"),
        s("firewall"),
        s("add"),
        s("rule"),
        format!("name={FIREWALL_RULE}"),
        s("dir=in"),
        s("action=allow"),
        s("protocol=TCP"),
        format!("localport={port}"),
        format!("program={}", exe.display()),
        s("profile=private,domain"),
        s("remoteip=localsubnet"),
    ])
}

/// IPC actions (through [`crate::remote::action`]):
/// - `{"op":"peer"}` → state: on/off, port, what's served, paired PCs;
/// - `{"op":"peer_pair"}` → a new pairing code;
/// - `{"op":"peer_revoke","name":…}` forgets a paired PC.
pub fn action(eng: &Engine, op: &str, a: &serde_json::Value) -> Result<serde_json::Value, String> {
    use serde_json::json;
    let cfg = eng.config().peer;
    let store = || {
        make_dir(&dir()).map_err(|e| e.to_string())?;
        Store::new(&dir()).map_err(|e| format!("{e:#}"))
    };
    match op {
        "peer" => {
            let running = RUNNING.lock();
            let shares: Vec<serde_json::Value> = running
                .as_ref()
                .map(|r| {
                    r.agent
                        .shares
                        .iter()
                        .map(|s| {
                            let st = s.state.lock().unwrap();
                            json!({"name": s.name, "entries": st.tree.len(), "state": st.status})
                        })
                        .collect()
                })
                .unwrap_or_default();
            let st = store()?;
            let fingerprint = st
                .fingerprint()
                .map(|f| quickseek_remote::pair::show_fingerprint(&f))
                .unwrap_or_default();
            Ok(json!({
                "enabled": cfg.enabled,
                "running": running.is_some(),
                "name": crate::remote::pc_name(),
                "port": cfg.port,
                "volumes": cfg.volumes,
                "shares": shares,
                "connections": running.as_ref().map_or(0, |r| r.agent.sessions.load(Ordering::Relaxed)),
                "fingerprint": fingerprint,
                "clients": st.clients().iter().map(|c| json!({"name": c.name, "paired": c.paired})).collect::<Vec<_>>(),
                "error": LAST_ERROR.lock().clone().or_else(|| running.as_ref().and_then(|r| r.error.clone().map(|e| format!("firewall rule: {e}")))),
            }))
        }
        "peer_pair" => {
            let st = store()?;
            let c = st.new_code().map_err(|e| format!("{e:#}"))?;
            Ok(json!({
                "code": c.code,
                "minutes": quickseek_agent::state::CODE_MINUTES,
                "name": crate::remote::pc_name(),
                "fingerprint": quickseek_remote::pair::show_fingerprint(&st.fingerprint().map_err(|e| format!("{e:#}"))?),
                "enabled": cfg.enabled,
            }))
        }
        "peer_revoke" => {
            let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
            if store()?.revoke(name).map_err(|e| format!("{e:#}"))? {
                Ok(json!({"revoked": name}))
            } else {
                Err(format!("no paired PC called {name}"))
            }
        }
        _ => Err(format!("unknown action {op:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold::FoldMode;
    use crate::index::IndexOptions;
    use quickseek_remote::FILE_ATTRIBUTE_DIRECTORY as DIR;

    /// An index like an NTFS volume's: root record 5.
    fn index() -> Index {
        let mut ix = crate::remote::build("Z", &[], FoldMode::default(), IndexOptions::default());
        ix.touched = Some(HashSet::new());
        ix
    }

    fn add(ix: &mut Index, rec: u32, parent_rec: u32, name: &str, dir: bool, size: u64) {
        let p = ix.dir_entry(parent_rec).unwrap();
        ix.add_link(rec, 0, p, name, if dir { DIR } else { 0 }, size, 1_000, 0);
    }

    /// What a PC following the share shows: paths with sizes.
    fn shown(ix: &Index) -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = (0..ix.entries.len() as u32)
            .filter(|&i| ix.is_visible(i) && i != ix.root)
            .map(|i| {
                (
                    ix.path(i),
                    if ix.entries[i as usize].is_dir() {
                        0
                    } else {
                        ix.entries[i as usize].size
                    },
                )
            })
            .collect();
        v.sort();
        v
    }

    /// Paths of the served index relative to its root.
    fn rel(v: Vec<(String, u64)>, root: &str) -> Vec<(String, u64)> {
        v.into_iter()
            .map(|(p, s)| {
                (
                    p.trim_start_matches(root)
                        .trim_start_matches('\\')
                        .to_string(),
                    s,
                )
            })
            .collect()
    }

    struct Fixture {
        share: Share,
        ids: HashMap<u32, u32>,
        replica: Option<Index>,
        sent: u64,
    }

    impl Fixture {
        fn new() -> Fixture {
            Fixture {
                share: Share::new("Z$", Path::new("Z:\\"), 7),
                ids: HashMap::new(),
                replica: None,
                sent: 0,
            }
        }

        /// One sync, then the replica a PC keeps: a full copy the first
        /// time, the ops after that.
        fn sync(&mut self, ix: &mut Index, full: bool) {
            let touched = if full { None } else { ix.touched.take() };
            ix.touched = Some(HashSet::new());
            let ids = &mut self.ids;
            self.share.edit(|st, ops| {
                let first = !st.ready;
                st.tree.quiet = first;
                let mut sy = Sync {
                    ix,
                    tree: &mut st.tree,
                    ids,
                    ops,
                    done: HashSet::new(),
                    root_rec: ix.record(ix.root),
                };
                match &touched {
                    Some(t) => {
                        for &r in t {
                            sy.id_of(r, 0);
                        }
                    }
                    None => sy.everything(),
                }
                st.tree.quiet = false;
                st.ready = true;
            });
            let st = self.share.state.lock().unwrap();
            match &mut self.replica {
                None => {
                    self.replica = Some(crate::remote::build(
                        "\\\\pc\\Z$",
                        &st.tree.records(),
                        FoldMode::default(),
                        IndexOptions::default(),
                    ));
                }
                Some(r) => {
                    let ops = st.ops_after(self.sent, usize::MAX).expect("in the log");
                    assert!(
                        crate::remote::apply(r, &ops, 0),
                        "ops fit the replica: {ops:?}"
                    );
                }
            }
            self.sent = st.seq;
        }

        fn check(&self, ix: &Index) {
            let want = rel(shown(ix), "Z");
            let got = rel(shown(self.replica.as_ref().unwrap()), "\\\\pc\\Z$");
            assert_eq!(got, want);
        }
    }

    #[test]
    fn a_replica_follows_the_index() {
        let mut ix = index();
        add(&mut ix, 30, 5, "docs", true, 0);
        add(&mut ix, 31, 30, "a.txt", false, 10);
        add(&mut ix, 32, 30, "sub", true, 0);
        add(&mut ix, 33, 32, "b.txt", false, 20);
        add(&mut ix, 34, 5, "top.bin", false, 5);
        let mut f = Fixture::new();
        f.sync(&mut ix, true);
        f.check(&ix);
        assert_eq!(
            f.share.state.lock().unwrap().log.len(),
            0,
            "the first fill sends no ops"
        );

        // Create, rename, move a folder, change a size, delete.
        add(&mut ix, 35, 32, "c.txt", false, 7);
        let a = ix.dir_entry(31).unwrap();
        let root = ix.root;
        ix.rename_entry(a, root, "a-moved.txt");
        let sub = ix.dir_entry(32).unwrap();
        let top = ix.dir_entry(5).unwrap();
        ix.rename_entry(sub, top, "sub2");
        ix.set_file_info(33, 99, 2_000, 0);
        ix.remove_record(34);
        f.sync(&mut ix, false);
        f.check(&ix);

        // A folder deleted with its contents, a record reused as a folder.
        ix.remove_record(33);
        ix.remove_record(35);
        ix.remove_record(32);
        add(&mut ix, 34, 30, "now-a-folder", true, 0);
        add(&mut ix, 36, 34, "inside.txt", false, 1);
        f.sync(&mut ix, false);
        f.check(&ix);

        // Nothing changed: no ops.
        let seq = f.share.state.lock().unwrap().seq;
        f.sync(&mut ix, false);
        assert_eq!(f.share.state.lock().unwrap().seq, seq);

        // A full comparison of an index in step finds nothing to send.
        f.sync(&mut ix, true);
        assert_eq!(f.share.state.lock().unwrap().seq, seq);
        f.check(&ix);
    }

    #[test]
    fn excluded_entries_are_not_served() {
        let mut ix = index();
        add(&mut ix, 30, 5, "keep", true, 0);
        add(&mut ix, 31, 30, "x.txt", false, 1);
        add(&mut ix, 32, 5, "node_modules", true, 0);
        add(&mut ix, 33, 32, "dep.js", false, 1);
        let mut f = Fixture::new();
        f.sync(&mut ix, true);
        assert_eq!(f.share.state.lock().unwrap().tree.len(), 4);
        let rules = crate::config::Config {
            exclude_folders: vec!["node_modules".into()],
            ..Default::default()
        }
        .rules('Z');
        ix.apply_rules(rules);
        assert!(ix.touched.is_none(), "new rules compare everything");
        f.sync(&mut ix, true);
        f.check(&ix);
        assert_eq!(f.share.state.lock().unwrap().tree.len(), 2);
    }

    #[test]
    fn a_saved_tree_maps_back_to_records() {
        let mut ix = index();
        add(&mut ix, 30, 5, "d", true, 0);
        add(&mut ix, 31, 30, "f", false, 3);
        let mut f = Fixture::new();
        f.sync(&mut ix, true);
        let ids = f.share.edit(|st, _| ids_of(&st.tree));
        assert_eq!(ids, f.ids);
    }

    #[test]
    fn share_names_are_admin_shares() {
        assert_eq!(share_name("C"), "C$");
        assert_eq!(share_name("D:"), "D$");
        let cfg = PeerConfig {
            volumes: vec!["c:".into()],
            ..Default::default()
        };
        assert!(cfg.serves("C") && !cfg.serves("D"));
        assert!(PeerConfig::default().serves("E"));
    }
}
