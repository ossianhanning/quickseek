//! `quickseek-agent`: indexes folders on a Linux NAS where the files are and
//! serves the index to the Quickseek service on Windows, which keeps a
//! replica (see `docs/design/nas-agent.md`).
//!
//! * [`btrfs`]: the index read from btrfs's own trees, and catching up by generation.
//! * [`tree`]: entries by id, the walk, and merging a walk into the tree.
//! * [`share`]: one served folder: tree, change log, reconciling events.
//! * [`watch`]: fanotify / inotify.
//! * [`server`]: TLS sessions with PCs.
//! * [`state`]: certificate, paired PCs, pairing codes.
//! * [`snaps`]: daily folder-size snapshots.

#[cfg(target_os = "linux")]
pub mod btrfs;
pub mod server;
pub mod share;
pub mod snaps;
pub mod state;
pub mod tree;
pub mod watch;

use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;

use crate::server::Agent;
use crate::share::Share;
use crate::state::Store;
use crate::watch::{Event, Kind};

#[derive(Debug, Clone)]
pub struct Options {
    /// (name, path) of every served folder.
    pub shares: Vec<(String, PathBuf)>,
    pub config: PathBuf,
    pub listen: String,
    pub port: u16,
    pub watch: Kind,
    /// Mounts for the fanotify mark (default: the mount of each share).
    pub marks: Vec<PathBuf>,
    pub threads: usize,
    /// Re-walk everything this often (0 = only after lost events).
    pub rewalk_hours: u64,
    /// Shown on the PC (default: the host name).
    pub name: String,
    /// After the first walk, walker threads read at idle I/O priority: they
    /// only get the disks when nothing else wants them. The first walk (which
    /// the PC waits for) runs at normal priority, and the live changes' few
    /// reads never wait.
    pub idle_walks: bool,
    /// How shares are read in full.
    pub index: IndexKind,
}

/// How a share is read in full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    /// From btrfs's own trees where possible (catching up by generation),
    /// else by walking.
    Auto,
    /// Always by walking (readdir and lstat).
    Walk,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            shares: Vec::new(),
            config: PathBuf::from("/config"),
            listen: "0.0.0.0".into(),
            port: quickseek_remote::DEFAULT_PORT,
            watch: Kind::Auto,
            marks: Vec::new(),
            threads: std::thread::available_parallelism().map_or(2, |n| n.get().min(4)),
            rewalk_hours: 24,
            name: host_name(),
            idle_walks: true,
            index: IndexKind::Auto,
        }
    }
}

pub fn host_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "nas".into())
}

/// A running agent (tests stop it by dropping the handle's `stop` flag).
pub struct Running {
    pub port: u16,
    pub fingerprint: [u8; 32],
    pub agent: Arc<Agent>,
    pub stop: Arc<AtomicBool>,
    /// Lets tests ask for a re-walk (watchers off).
    pub rewalk_now: Arc<AtomicBool>,
    listening: std::net::SocketAddr,
    server: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Running {
    /// Stops serving; the port is free again when this returns.
    fn drop(&mut self) {
        self.agent.shut_down(self.listening);
        if let Some(h) = self.server.take() {
            let _ = h.join();
        }
    }
}

/// Starts the agent: listener, first walk, watcher and snapshot loops in
/// background threads. Returns once it listens (the first walk continues).
pub fn start(opts: Options) -> anyhow::Result<Running> {
    if opts.shares.is_empty() {
        anyhow::bail!("no shares: give --share NAME=PATH (or QUICKSEEK_SHARES)");
    }
    let store = Store::new(&opts.config)?;
    let (cert, key) = store.identity()?;
    let fingerprint = quickseek_remote::tls::fingerprint(&cert);
    let tls = quickseek_remote::tls::server_config(cert, key)?;
    let epoch = u64::from_le_bytes(quickseek_remote::pair::random());
    let shares: Vec<Arc<Share>> = opts
        .shares
        .iter()
        .map(|(n, p)| Arc::new(Share::new(n, p, epoch)))
        .collect();
    // Saved indexes first, so a PC never finds these shares unready.
    for sh in &shares {
        if let Err(e) = sh.load(&store.dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!(
                "{}: the saved index can't be used ({e}); indexing anew",
                sh.name
            );
        }
    }
    if store.clients().is_empty() && store.code().is_none() {
        let c = store.new_code()?;
        log::info!(
            "no PC is paired yet. Pairing code: {} (valid for {} minutes; `quickseek-agent pair` makes a new one)",
            c.code,
            state::CODE_MINUTES
        );
    }
    let listener = TcpListener::bind((opts.listen.as_str(), opts.port))
        .with_context(|| format!("listening on {}:{}", opts.listen, opts.port))?;
    let listening = listener.local_addr()?;
    let port = listening.port();
    log::info!(
        "{} serving {} on port {port}; certificate {}",
        server::BUILD,
        opts.shares
            .iter()
            .map(|(n, p)| format!("{n} ({})", p.display()))
            .collect::<Vec<_>>()
            .join(", "),
        quickseek_remote::pair::show_fingerprint(&fingerprint)
    );
    let stop = Arc::new(AtomicBool::new(false));
    let agent = Arc::new(Agent::new(
        opts.name.clone(),
        shares.clone(),
        store,
        tls,
        fingerprint,
        stop.clone(),
    ));
    let a = agent.clone();
    let server = std::thread::Builder::new()
        .name("server".into())
        .spawn(move || a.serve(listener))?;
    let rewalk_now = Arc::new(AtomicBool::new(false));
    let (st, rw) = (stop.clone(), rewalk_now.clone());
    let a = agent.clone();
    std::thread::Builder::new()
        .name("index".into())
        .spawn(move || index_loop(&opts, &a, &st, &rw))?;
    Ok(Running {
        port,
        fingerprint,
        agent,
        stop,
        rewalk_now,
        listening,
        server: Some(server),
    })
}

/// Set by SIGTERM/SIGINT (`docker stop`): save the indexes and exit.
pub static TERM: AtomicBool = AtomicBool::new(false);

/// What a background indexing job produced for one share.
struct Indexed {
    share: usize,
    fresh: tree::Fresh,
    took: Duration,
    /// From btrfs's trees: the subvolumes and their generations.
    #[cfg(target_os = "linux")]
    subvols: Option<Vec<btrfs::Subvol>>,
}

/// Reads a share in full: from btrfs's trees when it can, else by walking.
fn index_share(
    i: usize,
    root: &std::path::Path,
    try_btrfs: bool,
    pool: &rayon::ThreadPool,
) -> Indexed {
    let t0 = Instant::now();
    #[cfg(target_os = "linux")]
    if try_btrfs {
        match btrfs::index(root, pool.current_num_threads()) {
            Ok((fresh, subvols)) => {
                return Indexed {
                    share: i,
                    fresh,
                    took: t0.elapsed(),
                    subvols: Some(subvols),
                };
            }
            Err(e) => log::warn!(
                "{}: reading btrfs's trees failed ({e}); walking instead",
                root.display()
            ),
        }
    }
    let _ = try_btrfs;
    Indexed {
        share: i,
        fresh: tree::walk(root, pool),
        took: t0.elapsed(),
        #[cfg(target_os = "linux")]
        subvols: None,
    }
}

/// btrfs: applies what changed since the share's generations. False if it
/// couldn't (the share then needs a full index).
#[cfg(target_os = "linux")]
fn catch_up(
    sh: &Share,
    i: usize,
    pool: &rayon::ThreadPool,
    w: &mut Box<dyn watch::Watcher>,
) -> bool {
    use std::os::unix::fs::MetadataExt;
    let subvols = sh.subvols.lock().unwrap().clone();
    if subvols.is_empty() {
        return false;
    }
    let t0 = Instant::now();
    let (changed, mut gens) = match btrfs::changed(&sh.root, &subvols) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("{}: btrfs catch-up failed ({e})", sh.name);
            return false;
        }
    };
    let ch = sh.catch_up(&changed, pool);
    w.untrack(i, &ch.removed);
    w.track(i, &ch.added);
    // Subvolumes created inside the share since: follow them too.
    for (_, path) in &ch.added {
        if let Ok(m) = std::fs::symlink_metadata(path)
            && m.ino() == 256
            && !gens.iter().any(|s| s.dev == m.dev())
            && let Ok(s) = btrfs::subvol_of(path)
        {
            gens.push(s);
        }
    }
    *sh.subvols.lock().unwrap() = gens;
    log::debug!(
        "{}: btrfs catch-up in {} ms",
        sh.name,
        t0.elapsed().as_millis()
    );
    true
}

fn index_loop(opts: &Options, agent: &Agent, stop: &AtomicBool, rewalk_now: &AtomicBool) {
    let shares = &agent.shares;
    let n = shares.len();
    let hist = agent.store.history_dir();
    let cfg_dir = agent.store.dir.clone();
    let pool = Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(opts.threads.max(1))
            .thread_name(|i| format!("walk-{i}"))
            .build()
            .expect("thread pool"),
    );
    let marks: Vec<std::path::PathBuf> = if opts.marks.is_empty() {
        let mut m: Vec<_> = shares
            .iter()
            .filter_map(|s| watch::mount_of(&s.root))
            .collect();
        m.sort();
        m.dedup();
        m
    } else {
        opts.marks.clone()
    };
    let mut w = watch::open(opts.watch, &marks);
    *agent.watcher.lock().unwrap() = w.name();
    // Which shares are read from btrfs's trees.
    #[cfg(target_os = "linux")]
    let use_btrfs: Vec<bool> = shares
        .iter()
        .map(|s| {
            let ok = opts.index == IndexKind::Auto && btrfs::probe(&s.root).is_ok();
            log::info!(
                "{}: {}",
                s.name,
                if ok {
                    "indexed from btrfs's trees, caught up by generation"
                } else {
                    "indexed by walking it"
                }
            );
            ok
        })
        .collect();
    #[cfg(not(target_os = "linux"))]
    let use_btrfs = vec![false; n];
    // Saved indexes: serve them at once, then catch up.
    let loaded: Vec<bool> = shares
        .iter()
        .map(|s| s.state.lock().unwrap().resumed.is_some())
        .collect();
    for (i, sh) in shares.iter().enumerate() {
        if loaded[i] {
            let dirs = sh.state.lock().unwrap().tree.all_dirs();
            w.track(i, &dirs);
        }
    }
    // Shares due for a full index (walk or tree search), done on a thread
    // while this loop keeps applying change events; each finished one is
    // merged and the events seen during it are applied again (the listing
    // may predate them).
    let mut due: Vec<u8> = (0..n).map(|_| 0).collect();
    for i in 0..n {
        #[cfg(target_os = "linux")]
        let caught = loaded[i] && use_btrfs[i] && catch_up(&shares[i], i, &pool, &mut w);
        #[cfg(not(target_os = "linux"))]
        let caught = false;
        // Walked shares go twice the first time: the second catches what
        // changed in folders the watcher didn't follow yet during the first.
        // A btrfs share catches up right after its first index instead.
        due[i] = if caught {
            0
        } else if use_btrfs[i] || loaded[i] {
            1
        } else {
            2
        };
    }
    let (tx, rx) = std::sync::mpsc::channel::<Indexed>();
    // While a job runs: the events applied since it started, per share, and
    // how many shares it has left.
    let mut running: Option<(Vec<Vec<Event>>, usize)> = None;
    let mut first_done = vec![false; n];
    let mut last_full = Instant::now();
    let mut last_disk = Instant::now();
    let mut last_snap = Instant::now() - Duration::from_secs(3600);
    let mut last_save = Instant::now();
    // btrfs writes new entries and sizes into its trees at its next commit
    // (up to 30 s; a file's size even at writeback), so what a tree search
    // reads can lag a little: a cheap catch-up every minute (a search for
    // newer leaves skips everything unchanged) picks those up, and anything
    // a lost event hid.
    #[cfg(target_os = "linux")]
    let mut last_catch = Instant::now();
    let mut saved_seq: Vec<u64> = vec![u64::MAX; n];
    let save = |saved_seq: &mut Vec<u64>, force: bool| {
        for (i, sh) in shares.iter().enumerate() {
            let seq = sh.state.lock().unwrap().seq;
            if force || seq != saved_seq[i] {
                match sh.save(&cfg_dir) {
                    Ok(s) => saved_seq[i] = s,
                    Err(e) => log::warn!("{}: saving the index: {e}", sh.name),
                }
            }
        }
    };
    loop {
        if TERM.load(Ordering::Relaxed) {
            save(&mut saved_seq, true);
            log::info!("stopping (indexes saved)");
            std::process::exit(0);
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if running.is_none() && due.iter().any(|&d| d > 0) {
            let which: Vec<usize> = (0..n).filter(|&i| due[i] > 0).collect();
            for &i in &which {
                due[i] -= 1;
            }
            let jobs: Vec<(usize, std::path::PathBuf, bool)> = which
                .iter()
                .map(|&i| (i, shares[i].root.clone(), use_btrfs[i]))
                .collect();
            let (pool2, tx2) = (pool.clone(), tx.clone());
            std::thread::Builder::new()
                .name("index".into())
                .spawn(move || {
                    for (i, root, b) in jobs {
                        if tx2.send(index_share(i, &root, b, &pool2)).is_err() {
                            return;
                        }
                    }
                })
                .expect("index thread");
            running = Some((vec![Vec::new(); n], which.len()));
            last_full = Instant::now();
        }
        let events = w.wait(Duration::from_millis(if running.is_some() {
            200
        } else {
            1000
        }));
        if events.contains(&Event::Overflow) {
            log::info!("change events were lost; catching up");
            for (i, sh) in shares.iter().enumerate() {
                #[cfg(target_os = "linux")]
                if use_btrfs[i] && running.is_none() && catch_up(sh, i, &pool, &mut w) {
                    continue;
                }
                let _ = sh;
                due[i] = due[i].max(1);
            }
        } else if !events.is_empty() {
            for (i, sh) in shares.iter().enumerate() {
                let mine: Vec<Event> = events
                    .iter()
                    .filter(|e| e.share() == Some(i))
                    .cloned()
                    .collect();
                if !mine.is_empty() {
                    let ch = sh.apply(&mine, &pool);
                    w.untrack(i, &ch.removed);
                    w.track(i, &ch.added);
                    if let Some((seen, _)) = running.as_mut() {
                        seen[i].extend(mine);
                    }
                }
            }
        }
        while let Ok(done) = rx.try_recv() {
            let i = done.share;
            let sh = &shares[i];
            let ch = sh.merge_walk(done.fresh, done.took);
            w.untrack(i, &ch.removed);
            w.track(i, &ch.added);
            if let Some((seen, left)) = running.as_mut() {
                let again = std::mem::take(&mut seen[i]);
                if !again.is_empty() {
                    let ch = sh.apply(&again, &pool);
                    w.untrack(i, &ch.removed);
                    w.track(i, &ch.added);
                }
                *left -= 1;
                if *left == 0 {
                    running = None;
                }
            }
            #[cfg(target_os = "linux")]
            if let Some(subvols) = done.subvols {
                *sh.subvols.lock().unwrap() = subvols;
                // What changed while the trees were read, in folders not
                // followed yet: the generations were read first.
                catch_up(sh, i, &pool, &mut w);
            }
            // Only now: its folders are followed, so from here on every
            // change reaches the PCs.
            sh.set_ready();
            if !first_done[i] {
                first_done[i] = true;
                save(&mut saved_seq, false);
            }
            if first_done.iter().all(|&d| d) && opts.idle_walks {
                // After the first indexes, walk at idle I/O priority (it's
                // per thread: every walker sets its own).
                pool.broadcast(|_| idle_io());
            }
        }
        if rewalk_now.swap(false, Ordering::Relaxed)
            || (opts.rewalk_hours > 0
                && running.is_none()
                && last_full.elapsed() > Duration::from_secs(opts.rewalk_hours * 3600))
        {
            for d in due.iter_mut() {
                *d = (*d).max(1);
            }
        }
        #[cfg(target_os = "linux")]
        if running.is_none() && last_catch.elapsed() > Duration::from_secs(60) {
            for (i, sh) in shares.iter().enumerate() {
                if use_btrfs[i] && first_done[i] && !catch_up(sh, i, &pool, &mut w) {
                    due[i] = due[i].max(1);
                }
            }
            last_catch = Instant::now();
        }
        if last_save.elapsed() > Duration::from_secs(600) {
            save(&mut saved_seq, false);
            last_save = Instant::now();
        }
        if last_disk.elapsed() > Duration::from_secs(60) {
            for sh in shares {
                sh.refresh_disk();
            }
            last_disk = Instant::now();
        }
        if last_snap.elapsed() > Duration::from_secs(600) {
            snaps::maybe_take(&hist, shares, state::now());
            last_snap = Instant::now();
        }
    }
}

/// Idle I/O priority for the calling thread (I/O priority is per thread on Linux).
#[cfg(target_os = "linux")]
pub fn idle_io() {
    // ioprio_set(IOPRIO_WHO_PROCESS, this thread, IOPRIO_CLASS_IDLE << 13)
    unsafe { libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13) };
}

#[cfg(not(target_os = "linux"))]
pub fn idle_io() {}

#[cfg(target_os = "linux")]
/// (dev, ino, size, mtime, name) of every entry, sorted: to tell whether
/// two readings of a share agree.
fn entry_keys(f: &tree::Fresh) -> Vec<(u64, u64, u64, i64, &str)> {
    let mut v: Vec<(u64, u64, u64, i64, &str)> = f.items[1..]
        .iter()
        .map(|(_, it)| {
            (
                it.meta.dev,
                it.meta.ino,
                it.meta.size,
                it.meta.modified,
                &*it.name,
            )
        })
        .collect();
    v.sort_unstable();
    v
}

/// `quickseek-agent bench`: times reading each share from btrfs's trees and
/// by walking (first and again), and a catch-up, and checks both readings
/// give the same entries. Read-only.
pub fn bench(opts: &Options, idle: bool) -> anyhow::Result<()> {
    if idle {
        idle_io();
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(opts.threads.max(1))
        .start_handler(move |_| {
            if idle {
                idle_io();
            }
        })
        .build()?;
    let pressure = || {
        std::fs::read_to_string("/proc/pressure/io")
            .ok()
            .and_then(|s| s.lines().next().map(str::to_string))
            .unwrap_or_else(|| "n/a".into())
    };
    println!(
        "I/O priority: {}; threads: {}; I/O pressure now: {}",
        if idle { "idle" } else { "normal" },
        opts.threads,
        pressure()
    );
    for (name, root) in &opts.shares {
        println!("== {name} ({})", root.display());
        let walk = |label: &str| {
            let t = Instant::now();
            let f = tree::walk(root, &pool);
            println!(
                "  walk ({label}): {} entries in {:.1} s",
                f.items.len() - 1,
                t.elapsed().as_secs_f64()
            );
            f
        };
        #[cfg(target_os = "linux")]
        {
            if let Err(e) = btrfs::probe(root) {
                println!("  btrfs tree search not available here: {e}");
                walk("first");
                continue;
            }
            let search = |label: &str| -> anyhow::Result<(tree::Fresh, Vec<btrfs::Subvol>)> {
                let t = Instant::now();
                let (f, s) = btrfs::index(root, opts.threads.max(1))?;
                println!(
                    "  btrfs tree search ({label}): {} entries in {:.1} s ({} subvolumes)",
                    f.items.len() - 1,
                    t.elapsed().as_secs_f64(),
                    s.len()
                );
                Ok((f, s))
            };
            // Which goes first gets the colder cache (QS_BENCH_FIRST=walk to swap).
            let (a, w) = if std::env::var("QS_BENCH_FIRST").as_deref() == Ok("walk") {
                let w = walk("first");
                (search("first")?.0, w)
            } else {
                let a = search("first")?.0;
                (a, walk("first"))
            };
            let same = entry_keys(&a) == entry_keys(&w);
            println!(
                "  the same entries (inode, size, date, name): {}",
                if same { "yes" } else { "NO" }
            );
            drop((a, w));
            let (_, subvols) = search("again")?;
            walk("again");
            let t = Instant::now();
            let (changed, _) = btrfs::changed(root, &subvols)?;
            println!(
                "  catch-up from the generations of the last search: {} changed inodes in {} ms",
                changed.len(),
                t.elapsed().as_millis()
            );
        }
        #[cfg(not(target_os = "linux"))]
        {
            walk("first");
        }
        println!("  I/O pressure now: {}", pressure());
    }
    if let Ok(s) = std::fs::read_to_string("/proc/self/status")
        && let Some(l) = s.lines().find(|l| l.starts_with("VmHWM"))
    {
        println!("peak memory: {}", l.trim_start_matches("VmHWM:").trim());
    }
    Ok(())
}

/// `NAME=PATH` or just `PATH` (named after its last folder).
pub fn parse_share(s: &str) -> anyhow::Result<(String, PathBuf)> {
    let (name, path) = match s.split_once('=') {
        Some((n, p)) => (n.trim().to_string(), PathBuf::from(p.trim())),
        None => {
            let p = PathBuf::from(s.trim());
            let n = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            (n, p)
        }
    };
    if name.is_empty() || path.as_os_str().is_empty() {
        anyhow::bail!("share {s:?}: expected NAME=PATH or PATH");
    }
    Ok((name, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_parse() {
        assert_eq!(
            parse_share("/volume1/data").unwrap(),
            ("data".into(), "/volume1/data".into())
        );
        assert_eq!(
            parse_share("m = /x/y").unwrap(),
            ("m".into(), "/x/y".into())
        );
        assert!(parse_share("=").is_err());
    }
}
