//! `quickseek-agent`: indexes folders on a Linux NAS where the files are and
//! serves the index to the Quickseek service on Windows, which keeps a
//! replica (see `docs/design/nas-agent.md`).
//!
//! * [`tree`]: entries by id, the walk, and merging a walk into the tree.
//! * [`share`]: one served folder: tree, change log, reconciling events.
//! * [`watch`]: fanotify / inotify.
//! * [`server`]: TLS sessions with PCs.
//! * [`state`]: certificate, paired PCs, pairing codes.
//! * [`snaps`]: daily folder-size snapshots.

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
    /// only get the disks when nothing else wants them. The first walk runs
    /// at normal priority (on a busy NAS an idle one took ~4x as long), and
    /// the live changes' few reads never wait.
    pub idle_walks: bool,
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
}

impl Drop for Running {
    fn drop(&mut self) {
        self.agent.shut_down(self.port);
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
    let port = listener.local_addr()?.port();
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
    std::thread::Builder::new()
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
    })
}

fn index_loop(opts: &Options, agent: &Agent, stop: &AtomicBool, rewalk_now: &AtomicBool) {
    let shares = &agent.shares;
    let hist = agent.store.history_dir();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(opts.threads.max(1))
        .thread_name(|i| format!("walk-{i}"))
        .build()
        .expect("thread pool");
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
    let rewalk_all = |w: &mut Box<dyn watch::Watcher>| {
        for (i, sh) in shares.iter().enumerate() {
            let ch = sh.rewalk(&pool);
            w.untrack(i, &ch.removed);
            w.track(i, &ch.added);
        }
    };
    // Twice: the second pass catches what changed in folders the watcher
    // didn't follow yet during the first (it's fast: the metadata is cached).
    rewalk_all(&mut w);
    rewalk_all(&mut w);
    if opts.idle_walks {
        // I/O priority is per thread: every walker sets its own.
        pool.broadcast(|_| idle_io());
    }
    let mut last_walk = Instant::now();
    let mut last_disk = Instant::now();
    let mut last_snap = Instant::now() - Duration::from_secs(3600);
    while !stop.load(Ordering::Relaxed) {
        let events = w.wait(Duration::from_secs(1));
        if events.contains(&Event::Overflow) {
            log::info!("change events were lost; re-walking");
            rewalk_all(&mut w);
            last_walk = Instant::now();
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
                }
            }
        }
        if rewalk_now.swap(false, Ordering::Relaxed)
            || (opts.rewalk_hours > 0
                && last_walk.elapsed() > Duration::from_secs(opts.rewalk_hours * 3600))
        {
            rewalk_all(&mut w);
            last_walk = Instant::now();
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
fn idle_io() {
    // ioprio_set(IOPRIO_WHO_PROCESS, this thread, IOPRIO_CLASS_IDLE << 13)
    unsafe { libc::syscall(libc::SYS_ioprio_set, 1, 0, 3 << 13) };
}

#[cfg(not(target_os = "linux"))]
fn idle_io() {}

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
