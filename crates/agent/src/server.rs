//! The TLS server: one thread per connected PC. After the hello and pairing
//! or authentication, a PC asks for history and subscribes to one share;
//! from then on only the agent talks (full transfer, then changes, with
//! pings while idle).

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quickseek_remote::tls::ServerStream;
use quickseek_remote::wire::{self, Msg, ShareInfo};
use quickseek_remote::{MIN_PROTOCOL, PROTOCOL, pair};

use crate::share::Share;
use crate::state::Store;

/// Most PCs served at once (a connection per share per PC).
const MAX_SESSIONS: usize = 32;
/// Idle streams send a ping this often; the PC drops a silent one after 90 s.
const PING_EVERY: Duration = Duration::from_secs(30);
/// Ops per `Changes` message.
const BATCH: usize = 20_000;

pub struct Agent {
    pub name: String,
    pub shares: Vec<Arc<Share>>,
    pub store: Store,
    pub tls: Arc<quickseek_remote::rustls::ServerConfig>,
    pub fingerprint: [u8; 32],
    pub sessions: AtomicUsize,
    /// Which change notification is in use (for logs and tests).
    pub watcher: std::sync::Mutex<&'static str>,
    /// Set when the agent stops (tests stop and start agents in one process).
    pub stop: Arc<AtomicBool>,
    /// Open connections, to cut them when stopping.
    socks: Mutex<Vec<(u64, TcpStream)>>,
    next_sock: AtomicU64,
}

pub const BUILD: &str = concat!("quickseek-agent ", env!("CARGO_PKG_VERSION"));

impl Agent {
    pub fn new(
        name: String,
        shares: Vec<Arc<Share>>,
        store: Store,
        tls: Arc<quickseek_remote::rustls::ServerConfig>,
        fingerprint: [u8; 32],
        stop: Arc<AtomicBool>,
    ) -> Agent {
        Agent {
            name,
            shares,
            store,
            tls,
            fingerprint,
            sessions: AtomicUsize::new(0),
            watcher: std::sync::Mutex::new("starting"),
            stop,
            socks: Mutex::new(Vec::new()),
            next_sock: AtomicU64::new(0),
        }
    }

    /// Stops accepting and cuts every open connection.
    pub fn shut_down(&self, port: u16) {
        self.stop.store(true, Ordering::Relaxed);
        for (_, s) in self.socks.lock().unwrap().drain(..) {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_secs(1),
        );
    }

    pub fn share_infos(&self) -> Vec<ShareInfo> {
        self.shares
            .iter()
            .map(|s| ShareInfo {
                name: s.name.clone(),
                root: s.root.display().to_string(),
                entries: s.state.lock().unwrap().tree.len() as u64,
            })
            .collect()
    }

    /// Accepts connections until stopped.
    pub fn serve(self: Arc<Self>, listener: TcpListener) {
        for sock in listener.incoming() {
            if self.stop.load(Ordering::Relaxed) {
                return;
            }
            let Ok(sock) = sock else { continue };
            if self.sessions.load(Ordering::Relaxed) >= MAX_SESSIONS {
                log::warn!("too many connections; refusing one");
                continue;
            }
            let peer = sock.peer_addr().map(|a| a.to_string()).unwrap_or_default();
            let me = self.clone();
            me.sessions.fetch_add(1, Ordering::Relaxed);
            let id = me.next_sock.fetch_add(1, Ordering::Relaxed);
            if let Ok(c) = sock.try_clone() {
                me.socks.lock().unwrap().push((id, c));
            }
            let _ = std::thread::Builder::new()
                .name(format!("client {peer}"))
                .spawn(move || {
                    let r = quickseek_remote::tls::accept(
                        me.tls.clone(),
                        sock,
                        Duration::from_secs(30),
                    )
                    .and_then(|s| me.session(s, &peer));
                    if let Err(e) = r {
                        log::debug!("{peer}: {e}");
                    }
                    me.socks.lock().unwrap().retain(|(i, _)| *i != id);
                    me.sessions.fetch_sub(1, Ordering::Relaxed);
                });
        }
    }

    fn session(&self, mut s: ServerStream, peer: &str) -> std::io::Result<()> {
        // Hello.
        let Msg::Hello { min, max, .. } = wire::recv(&mut s)? else {
            return refuse(&mut s, "expected a hello");
        };
        let v = PROTOCOL.min(max);
        if v < MIN_PROTOCOL.max(min) {
            return refuse(
                &mut s,
                &format!(
                    "this agent speaks protocol {MIN_PROTOCOL}-{PROTOCOL}, the PC {min}-{max}: update the older one"
                ),
            );
        }
        wire::send(
            &mut s,
            &Msg::Hello {
                min: v,
                max: v,
                build: BUILD.into(),
            },
        )?;
        // Pairing or authentication.
        let client = match wire::recv(&mut s)? {
            Msg::Pair { proof, nonce, name } => {
                match self.store.check_pairing(&proof, &self.fingerprint, &nonce) {
                    Ok(code) => {
                        let token = self
                            .store
                            .add_client(&name)
                            .map_err(std::io::Error::other)?;
                        log::info!("paired with {name} ({peer})");
                        let proof = pair::agent_proof(&code, &self.fingerprint, &nonce);
                        wire::send(&mut s, &Msg::Paired { token, proof })?;
                        name
                    }
                    Err(e) => {
                        log::warn!("{peer}: pairing failed: {e}");
                        return refuse(&mut s, &e);
                    }
                }
            }
            Msg::Auth { token, name } => match self.store.client_for(&token) {
                Some(c) => {
                    log::debug!("{} ({name}, {peer}) connected", c.name);
                    c.name
                }
                None => {
                    return refuse(
                        &mut s,
                        "not paired (or the pairing was revoked): pair again",
                    );
                }
            },
            _ => return refuse(&mut s, "expected pairing or authentication"),
        };
        wire::send(
            &mut s,
            &Msg::Welcome {
                agent: self.name.clone(),
                shares: self.share_infos(),
            },
        )?;
        loop {
            match wire::recv(&mut s)? {
                Msg::History { since } => {
                    let list = crate::snaps::list(&self.store.history_dir(), since);
                    wire::send(&mut s, &Msg::Snapshots(list))?;
                }
                Msg::Subscribe { share, epoch, seq } => {
                    let Some(sh) = self.shares.iter().find(|x| x.name == share) else {
                        return refuse(&mut s, &format!("no share called {share}"));
                    };
                    log::info!("{client} ({peer}) follows {share}");
                    return stream(&mut s, sh, epoch, seq);
                }
                Msg::Ping => wire::send(&mut s, &Msg::Pong)?,
                _ => return refuse(&mut s, "unexpected message"),
            }
        }
    }
}

fn refuse(s: &mut ServerStream, why: &str) -> std::io::Result<()> {
    let _ = wire::send(s, &Msg::Error(why.to_string()));
    s.conn.send_close_notify();
    let _ = s.conn.complete_io(&mut s.sock);
    Err(std::io::Error::other(why.to_string()))
}

/// Sends `sh` to the PC and keeps it current until the connection breaks.
fn stream(s: &mut ServerStream, sh: &Share, epoch: u64, seq: u64) -> std::io::Result<()> {
    let mut status = String::new();
    let mut disk = (u64::MAX, u64::MAX);
    let mut last_send = Instant::now();
    // Wait for the first walk.
    loop {
        let st = sh.state.lock().unwrap();
        if st.ready {
            break;
        }
        if st.status != status {
            status = st.status.clone();
            drop(st);
            wire::send(s, &Msg::State(status.clone()))?;
            continue;
        }
        drop(sh.wait(st, PING_EVERY));
        wire::send(s, &Msg::Ping)?;
    }
    let (mut epoch, mut sent) = (epoch, seq);
    loop {
        // Resume if the log still reaches back far enough, else send it all.
        let full = {
            let st = sh.state.lock().unwrap();
            if st.epoch == epoch && st.ops_after(sent, 0).is_some() {
                // In step with a saved index the agent loaded: continue from
                // where its log starts, and tell the PC (so its next resume
                // matches even if nothing changes until then).
                if let Some((saved, base)) = st.resumed
                    && sent == saved
                {
                    sent = base;
                    drop(st);
                    wire::send(
                        s,
                        &Msg::Changes {
                            seq: sent,
                            ops: Vec::new(),
                        },
                    )?;
                }
                None
            } else {
                Some((st.epoch, st.seq, st.tree.encode()))
            }
        };
        if let Some((e, q, (count, raw))) = full {
            let t0 = Instant::now();
            let chunks = wire::chunks(&raw);
            drop(raw);
            let bytes: usize = chunks.iter().map(Vec::len).sum();
            wire::send(
                s,
                &Msg::Full {
                    epoch: e,
                    seq: q,
                    count,
                },
            )?;
            for c in chunks {
                wire::send(s, &Msg::Chunk(c))?;
            }
            wire::send(s, &Msg::FullEnd)?;
            log::info!(
                "{}: sent {count} entries ({:.1} MB) in {} ms",
                sh.name,
                bytes as f64 / 1e6,
                t0.elapsed().as_millis()
            );
            (epoch, sent) = (e, q);
        }
        loop {
            let (ops, new_status, new_disk) = {
                let mut st = sh.state.lock().unwrap();
                let deadline = Instant::now() + PING_EVERY;
                while st.seq == sent
                    && st.epoch == epoch
                    && st.status == status
                    && (st.capacity, st.free) == disk
                {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    st = sh.wait(st, left);
                }
                if st.epoch != epoch {
                    break;
                }
                let Some(ops) = st.ops_after(sent, BATCH) else {
                    break; // fell out of the log: full transfer
                };
                (ops, st.status.clone(), (st.capacity, st.free))
            };
            if new_status != status {
                status = new_status;
                wire::send(s, &Msg::State(status.clone()))?;
                last_send = Instant::now();
            }
            if new_disk != disk {
                disk = new_disk;
                wire::send(
                    s,
                    &Msg::Disk {
                        capacity: disk.0,
                        free: disk.1,
                    },
                )?;
                last_send = Instant::now();
            }
            if !ops.is_empty() {
                sent += ops.len() as u64;
                wire::send(s, &Msg::Changes { seq: sent, ops })?;
                last_send = Instant::now();
            } else if last_send.elapsed() >= PING_EVERY {
                wire::send(s, &Msg::Ping)?;
                last_send = Instant::now();
            }
        }
    }
}
