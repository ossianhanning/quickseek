//! Named-pipe IPC between the elevated service (owns the index) and
//! unelevated clients (CLI, terminal UI, GUI).
//!
//! Wire format: every message is a `u32` little-endian length followed by
//! that many bytes, starting with a protocol version byte. Strings are `u32`
//! length + UTF-8.
//!
//! Each connection is a [`Session`]: `Search` runs a query and keeps its
//! [`ResultSet`]; `Fetch` pages through it (the GUI's virtual list asks for
//! the rows it is about to draw). A fetch after the index was compacted
//! returns [`STALE`] and the client re-runs the search.
//!
//! Security: the pipe's DACL lets SYSTEM, Administrators and the pipe owner
//! do anything and interactive users only read/write data. In particular they
//! don't get FILE_CREATE_PIPE_INSTANCE, so they can't add server instances to
//! our pipe. The first instance is created with FILE_FLAG_FIRST_PIPE_INSTANCE,
//! so the service refuses to start if someone else already owns the name.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::FromRawHandle;
use std::ptr::null_mut;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{CreateDirectoryW, FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};

use crate::config::Config;
use crate::engine::Engine;
use crate::ntfs::win::wide;
use crate::query::SearchFlags;
use crate::search::{Hit, ResultSet, SearchOptions, SortKey};

pub const PIPE_NAME: &str = r"\\.\pipe\rapidfind";
const MAX_FRAME: u32 = 64 << 20;
const PROTOCOL: u8 = 2;
/// Error text for a fetch from a result set the index has outgrown.
pub const STALE: &str = "stale result set";
pub const LOADING: &str = "index is still loading";

/// Pipe DACL: full access for SYSTEM, Administrators and the pipe owner (the
/// service); interactive users get
/// FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE only.
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)(A;;0x100083;;;IU)";
/// Same access a client asks for when opening the pipe.
const CLIENT_ACCESS: u32 = 0x0010_0083;

// ---- messages ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Search { query: String, flags: SearchFlags, sort: SortKey, descending: bool, page: u32 },
    Fetch { offset: u64, count: u32 },
    Status,
    GetConfig,
    SetConfig(String),
    RecordRun(String),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Row {
    pub name: String,
    /// Containing folder; empty for a volume root.
    pub dir: String,
    pub size: u64,
    pub modified: i64,
    pub created: i64,
    pub attrs: u32,
    pub run_count: u32,
    pub is_dir: bool,
    /// False for volumes indexed without sizes (fallback scan, ReFS).
    pub has_size: bool,
}

impl Row {
    pub fn full_path(&self) -> String {
        match self.dir.as_str() {
            "" if self.name.ends_with('\\') => self.name.clone(),
            "" => format!("{}\\", self.name),
            d if d.ends_with('\\') => format!("{d}{}", self.name),
            d => format!("{d}\\{}", self.name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Page {
    pub total: u64,
    pub offset: u64,
    pub elapsed_us: u64,
    pub generation: u64,
    pub rows: Vec<Row>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct VolumeInfo {
    pub key: String,
    pub kind: String,
    pub entries: u64,
    pub ready: bool,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct StatusInfo {
    pub ready: bool,
    pub generation: u64,
    pub volumes: Vec<VolumeInfo>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    Page(Page),
    Status(StatusInfo),
    Config(String),
    Count(u32),
    Ok,
    Error(String),
}

// ---- encoding ---------------------------------------------------------------

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s.as_bytes());
    }
}

struct R<'a>(&'a [u8]);

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("malformed message: {what}"))
}

impl R<'_> {
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        if self.0.len() < n {
            return Err(bad("truncated"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn str(&mut self) -> io::Result<String> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| bad("utf-8"))
    }
}

pub fn sort_to_u8(s: SortKey) -> u8 {
    match s {
        SortKey::Name => 0,
        SortKey::Path => 1,
        SortKey::Size => 2,
        SortKey::Modified => 3,
        SortKey::Created => 4,
        SortKey::RunCount => 5,
    }
}

pub fn sort_from_u8(v: u8) -> io::Result<SortKey> {
    Ok(match v {
        0 => SortKey::Name,
        1 => SortKey::Path,
        2 => SortKey::Size,
        3 => SortKey::Modified,
        4 => SortKey::Created,
        5 => SortKey::RunCount,
        _ => return Err(bad("sort key")),
    })
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(vec![PROTOCOL]);
        match self {
            Request::Search { query, flags, sort, descending, page } => {
                w.u8(1);
                w.u8(flags.bits());
                w.u8(sort_to_u8(*sort));
                w.u8(*descending as u8);
                w.u32(*page);
                w.str(query);
            }
            Request::Fetch { offset, count } => {
                w.u8(2);
                w.u64(*offset);
                w.u32(*count);
            }
            Request::Status => w.u8(3),
            Request::GetConfig => w.u8(4),
            Request::SetConfig(s) => {
                w.u8(5);
                w.str(s);
            }
            Request::RecordRun(p) => {
                w.u8(6);
                w.str(p);
            }
        }
        w.0
    }

    pub fn decode(b: &[u8]) -> io::Result<Request> {
        let mut r = R(b);
        if r.u8()? != PROTOCOL {
            return Err(bad("protocol version"));
        }
        Ok(match r.u8()? {
            1 => {
                let flags = SearchFlags::from_bits(r.u8()?);
                let sort = sort_from_u8(r.u8()?)?;
                let descending = r.u8()? != 0;
                let page = r.u32()?;
                Request::Search { query: r.str()?, flags, sort, descending, page }
            }
            2 => Request::Fetch { offset: r.u64()?, count: r.u32()? },
            3 => Request::Status,
            4 => Request::GetConfig,
            5 => Request::SetConfig(r.str()?),
            6 => Request::RecordRun(r.str()?),
            _ => return Err(bad("request kind")),
        })
    }
}

impl Response {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = W(vec![PROTOCOL]);
        match self {
            Response::Page(p) => {
                w.u8(1);
                w.u64(p.total);
                w.u64(p.offset);
                w.u64(p.elapsed_us);
                w.u64(p.generation);
                w.u32(p.rows.len() as u32);
                for row in &p.rows {
                    w.str(&row.name);
                    w.str(&row.dir);
                    w.u64(row.size);
                    w.u64(row.modified as u64);
                    w.u64(row.created as u64);
                    w.u32(row.attrs);
                    w.u32(row.run_count);
                    w.u8(row.is_dir as u8 | (row.has_size as u8) << 1);
                }
            }
            Response::Status(s) => {
                w.u8(2);
                w.u8(s.ready as u8);
                w.u64(s.generation);
                w.u32(s.volumes.len() as u32);
                for v in &s.volumes {
                    w.str(&v.key);
                    w.str(&v.kind);
                    w.u64(v.entries);
                    w.u8(v.ready as u8);
                    w.str(&v.state);
                }
            }
            Response::Error(e) => {
                w.u8(3);
                w.str(e);
            }
            Response::Config(c) => {
                w.u8(4);
                w.str(c);
            }
            Response::Count(n) => {
                w.u8(5);
                w.u32(*n);
            }
            Response::Ok => w.u8(6),
        }
        w.0
    }

    pub fn decode(b: &[u8]) -> io::Result<Response> {
        let mut r = R(b);
        if r.u8()? != PROTOCOL {
            return Err(bad("protocol version (client and service versions differ?)"));
        }
        Ok(match r.u8()? {
            1 => {
                let (total, offset, elapsed_us, generation) = (r.u64()?, r.u64()?, r.u64()?, r.u64()?);
                let n = r.u32()? as usize;
                let mut rows = Vec::with_capacity(n.min(1 << 16));
                for _ in 0..n {
                    let (name, dir) = (r.str()?, r.str()?);
                    let (size, modified, created) = (r.u64()?, r.u64()? as i64, r.u64()? as i64);
                    let (attrs, run_count, flags) = (r.u32()?, r.u32()?, r.u8()?);
                    rows.push(Row { name, dir, size, modified, created, attrs, run_count, is_dir: flags & 1 != 0, has_size: flags & 2 != 0 });
                }
                Response::Page(Page { total, offset, elapsed_us, generation, rows })
            }
            2 => {
                let ready = r.u8()? != 0;
                let generation = r.u64()?;
                let n = r.u32()? as usize;
                let mut volumes = Vec::with_capacity(n.min(64));
                for _ in 0..n {
                    volumes.push(VolumeInfo { key: r.str()?, kind: r.str()?, entries: r.u64()?, ready: r.u8()? != 0, state: r.str()? });
                }
                Response::Status(StatusInfo { ready, generation, volumes })
            }
            3 => Response::Error(r.str()?),
            4 => Response::Config(r.str()?),
            5 => Response::Count(r.u32()?),
            6 => Response::Ok,
            _ => return Err(bad("response kind")),
        })
    }
}

pub fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let mut buf = Vec::with_capacity(payload.len() + 4);
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    w.write_all(&buf)?;
    w.flush()
}

/// Reads one frame; `Ok(None)` on clean EOF.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if matches!(e.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe) => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len);
    if len > MAX_FRAME {
        return Err(bad("frame too large"));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

// ---- answering requests -------------------------------------------------------

/// Per-connection state: the current result set.
#[derive(Default)]
pub struct Session {
    rs: Option<ResultSet>,
    generation: u64,
}

fn rows(eng: &Engine, hits: &[Hit]) -> Vec<Row> {
    let runs = eng.runs();
    eng.with_indexes(|ixs| {
        hits.iter()
            .filter(|h| (h.vol as usize) < ixs.len() && (h.idx as usize) < ixs[h.vol as usize].entries.len())
            .map(|h| {
                let ix = ixs[h.vol as usize];
                let e = &ix.entries[h.idx as usize];
                Row {
                    name: ix.name(h.idx).to_string(),
                    dir: ix.parent_path(h.idx),
                    size: e.size,
                    modified: e.modified,
                    created: ix.created(h.idx).unwrap_or(0),
                    attrs: e.attrs(),
                    run_count: runs.get(ix.volume_serial, e.record).map_or(0, |r| r.count),
                    is_dir: e.is_dir(),
                    has_size: ix.has_sizes,
                }
            })
            .collect()
    })
}

impl Session {
    /// Answers a request against an engine (`None` while it is loading).
    pub fn handle(&mut self, eng: Option<&Engine>, req: &Request) -> Response {
        let Some(eng) = eng else {
            return match req {
                Request::Status => Response::Status(StatusInfo::default()),
                _ => Response::Error(LOADING.into()),
            };
        };
        match req {
            Request::Status => Response::Status(StatusInfo {
                ready: !eng.is_loading() || eng.volume_count() > 0,
                generation: eng.generation(),
                volumes: eng
                    .status()
                    .into_iter()
                    .map(|v| VolumeInfo { key: v.key, kind: v.kind.label().into(), entries: v.entries, ready: v.ready, state: v.state })
                    .collect(),
            }),
            Request::Search { query, flags, sort, descending, page } => {
                let t = Instant::now();
                let generation = eng.generation();
                let opts = SearchOptions { sort: *sort, descending: *descending, limit: (*page).max(1) as usize };
                match eng.query(query, *flags, &opts) {
                    Ok(rs) => {
                        let first = rows(eng, rs.first_page());
                        let total = rs.total as u64;
                        self.rs = Some(rs);
                        self.generation = generation;
                        Response::Page(Page { total, offset: 0, elapsed_us: t.elapsed().as_micros() as u64, generation, rows: first })
                    }
                    Err(e) => {
                        self.rs = None;
                        Response::Error(e)
                    }
                }
            }
            Request::Fetch { offset, count } => {
                let Some(rs) = self.rs.as_mut() else { return Response::Error("no search yet".into()) };
                let t = Instant::now();
                let page = eng.with_indexes(|ixs| rs.page(ixs, *offset as usize, (*count).min(100_000) as usize));
                match page {
                    Ok(hits) => Response::Page(Page {
                        total: rs.total as u64,
                        offset: *offset,
                        elapsed_us: t.elapsed().as_micros() as u64,
                        generation: self.generation,
                        rows: rows(eng, &hits),
                    }),
                    Err(_) => Response::Error(STALE.into()),
                }
            }
            Request::GetConfig => Response::Config(serde_json::to_string_pretty(&eng.config()).unwrap()),
            Request::SetConfig(json) => match serde_json::from_str::<Config>(json) {
                Ok(c) => match eng.set_config(c) {
                    Ok(()) => Response::Ok,
                    Err(e) => Response::Error(format!("{e:#}")),
                },
                Err(e) => Response::Error(format!("bad config: {e}")),
            },
            Request::RecordRun(path) => match eng.record_run(path) {
                Some(n) => Response::Count(n),
                None => Response::Error(format!("not indexed: {path}")),
            },
        }
    }
}

// ---- server -------------------------------------------------------------------

/// A security descriptor parsed from SDDL, freed on drop.
struct Sd(PSECURITY_DESCRIPTOR);

impl Sd {
    fn new(sddl: &str) -> io::Result<Sd> {
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        let s = wide(sddl);
        let ok = unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(s.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut()) };
        if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(Sd(sd)) }
    }
    fn attrs(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES { nLength: size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: self.0, bInheritHandle: 0 }
    }
}

impl Drop for Sd {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0) };
    }
}

/// Creates `dir` readable/writable only by SYSTEM and Administrators (if it
/// doesn't exist yet). Used for the service's index and binary.
pub fn create_private_dir(dir: &std::path::Path) -> io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    if let Some(p) = dir.parent() {
        std::fs::create_dir_all(p)?;
    }
    let sd = Sd::new("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)")?;
    let sa = sd.attrs();
    let path = wide(&dir.to_string_lossy());
    if unsafe { CreateDirectoryW(path.as_ptr(), &sa) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn create_instance(pipe_name: &str, sa: &SECURITY_ATTRIBUTES, first: bool) -> io::Result<File> {
    let name = wide(pipe_name);
    let h = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 },
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            1 << 16,
            1 << 16,
            0,
            sa,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(h) })
}

/// Makes a request handler for one new connection, given the client's
/// identity (when it could be captured).
pub type HandlerFactory = Arc<dyn Fn(Option<crate::ntfs::win::Token>) -> Box<dyn FnMut(&Request) -> Response + Send> + Send + Sync>;

/// A factory serving `engine` (once it is set) with a [`Session`] per client.
/// The client's identity is handed to the engine for network folders.
pub fn engine_handler(engine: Arc<std::sync::OnceLock<Arc<Engine>>>) -> HandlerFactory {
    Arc::new(move |token| {
        let engine = engine.clone();
        let mut token = token;
        let mut session = Session::default();
        Box::new(move |req| {
            let e = engine.get();
            if let (Some(e), Some(t)) = (e, token.take()) {
                e.offer_token(t);
            }
            session.handle(e.map(|e| &**e), req)
        })
    })
}

/// Serves requests forever: one thread per connected client. Returns only if
/// the pipe can't be created.
pub fn serve(factory: HandlerFactory) -> io::Result<()> {
    serve_on(PIPE_NAME, factory)
}

pub fn serve_on(pipe_name: &str, factory: HandlerFactory) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    let sd = Sd::new(PIPE_SDDL)?;
    let sa = sd.attrs();
    let mut first = true;
    loop {
        let pipe = create_instance(pipe_name, &sa, first)?;
        first = false;
        let ok = unsafe { ConnectNamedPipe(pipe.as_raw_handle(), null_mut()) };
        if ok == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
            continue; // client vanished between create and connect
        }
        let token = crate::ntfs::win::Token::from_pipe_client(&pipe);
        let mut handler = factory(token);
        std::thread::spawn(move || {
            let mut pipe = pipe;
            while let Ok(Some(frame)) = read_frame(&mut pipe) {
                let resp = match Request::decode(&frame) {
                    Ok(req) => handler(&req),
                    Err(e) => Response::Error(e.to_string()),
                };
                if write_frame(&mut pipe, &resp.encode()).is_err() {
                    break;
                }
            }
        });
    }
}

// ---- client -------------------------------------------------------------------

pub struct Client {
    pipe: File,
}

impl Client {
    /// Connects to the service (or `RAPIDFIND_PIPE` if set); fails fast if it isn't running.
    pub fn connect() -> io::Result<Client> {
        match std::env::var("RAPIDFIND_PIPE") {
            Ok(p) if !p.is_empty() => Self::connect_to(&p),
            _ => Self::connect_to(PIPE_NAME),
        }
    }

    pub fn connect_to(pipe_name: &str) -> io::Result<Client> {
        let open = || std::fs::OpenOptions::new().access_mode(CLIENT_ACCESS).open(pipe_name);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match open() {
                Ok(pipe) => return Ok(Client { pipe }),
                // All instances busy: wait for one.
                Err(e) if e.raw_os_error() == Some(231) && Instant::now() < deadline => {
                    let name = wide(pipe_name);
                    unsafe { WaitNamedPipeW(name.as_ptr(), 500) };
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub fn call(&mut self, req: &Request) -> io::Result<Response> {
        write_frame(&mut self.pipe, &req.encode())?;
        let frame = read_frame(&mut self.pipe)?.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "service closed the connection"))?;
        Response::decode(&frame)
    }
}

// ---- backends (service or in-process) -------------------------------------------

/// Where requests go: the service over the pipe, or an in-process engine.
pub trait Backend: Send {
    fn call(&mut self, req: &Request) -> io::Result<Response>;
    fn label(&self) -> &'static str;
    /// Called before exit (the local engine saves if worthwhile).
    fn finish(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn search(&mut self, query: &str, flags: SearchFlags, sort: SortKey, descending: bool, page: u32) -> Result<Page, String> {
        match self.call(&Request::Search { query: query.into(), flags, sort, descending, page }) {
            Ok(Response::Page(p)) => Ok(p),
            Ok(Response::Error(e)) => Err(e),
            Ok(_) => Err("unexpected response".into()),
            Err(e) => Err(format!("service connection: {e}")),
        }
    }

    fn fetch(&mut self, offset: u64, count: u32) -> Result<Page, String> {
        match self.call(&Request::Fetch { offset, count }) {
            Ok(Response::Page(p)) => Ok(p),
            Ok(Response::Error(e)) => Err(e),
            Ok(_) => Err("unexpected response".into()),
            Err(e) => Err(format!("service connection: {e}")),
        }
    }

    fn status(&mut self) -> Option<StatusInfo> {
        match self.call(&Request::Status) {
            Ok(Response::Status(s)) => Some(s),
            _ => None,
        }
    }

    /// `None` while the index is still loading.
    fn generation(&mut self) -> Option<u64> {
        self.status().filter(|s| s.ready).map(|s| s.generation)
    }

    fn get_config(&mut self) -> Result<Config, String> {
        match self.call(&Request::GetConfig) {
            Ok(Response::Config(s)) => serde_json::from_str(&s).map_err(|e| e.to_string()),
            Ok(Response::Error(e)) => Err(e),
            Ok(_) => Err("unexpected response".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn set_config(&mut self, c: &Config) -> Result<(), String> {
        match self.call(&Request::SetConfig(serde_json::to_string(c).unwrap())) {
            Ok(Response::Ok) => Ok(()),
            Ok(Response::Error(e)) => Err(e),
            Ok(_) => Err("unexpected response".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn record_run(&mut self, path: &str) -> Option<u32> {
        match self.call(&Request::RecordRun(path.into())) {
            Ok(Response::Count(n)) => Some(n),
            _ => None,
        }
    }
}

pub struct Local {
    pub engine: Arc<Engine>,
    session: Session,
}

impl Local {
    pub fn new(engine: Arc<Engine>) -> Local {
        Local { engine, session: Session::default() }
    }
}

impl Backend for Local {
    fn call(&mut self, req: &Request) -> io::Result<Response> {
        Ok(self.session.handle(Some(&self.engine), req))
    }
    fn label(&self) -> &'static str {
        "local index"
    }
    fn finish(&mut self) -> anyhow::Result<()> {
        self.engine.save_all(crate::engine::SAVE_THRESHOLD)
    }
}

pub struct Remote(pub Client);

impl Backend for Remote {
    fn call(&mut self, req: &Request) -> io::Result<Response> {
        self.0.call(req)
    }
    fn label(&self) -> &'static str {
        "service"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::sample;

    #[test]
    fn request_roundtrip() {
        for req in [
            Request::Search {
                query: "ext:rs ünï \"a b\"".into(),
                flags: SearchFlags { case: true, regex: true, ..Default::default() },
                sort: SortKey::RunCount,
                descending: true,
                page: 123,
            },
            Request::Search { query: String::new(), flags: SearchFlags::default(), sort: SortKey::Name, descending: false, page: 0 },
            Request::Fetch { offset: 1 << 40, count: 77 },
            Request::Status,
            Request::GetConfig,
            Request::SetConfig("{}".into()),
            Request::RecordRun(r"C:\x".into()),
        ] {
            assert_eq!(Request::decode(&req.encode()).unwrap(), req);
        }
    }

    #[test]
    fn response_roundtrip_and_framing() {
        let page = Response::Page(Page {
            total: 4_000_000,
            offset: 100,
            elapsed_us: 1234,
            generation: 99,
            rows: vec![
                Row { name: "a.txt".into(), dir: r"C:\Users\me".into(), size: 10, modified: -1, created: 5, attrs: 0x20, run_count: 3, is_dir: false, has_size: true },
                Row { name: "C:".into(), is_dir: true, ..Default::default() },
            ],
        });
        let status = Response::Status(StatusInfo {
            ready: true,
            generation: 7,
            volumes: vec![VolumeInfo { key: "C".into(), kind: "NTFS".into(), entries: 3, ready: true, state: "ready".into() }],
        });
        let all = [page, status, Response::Error("bad size: x".into()), Response::Config("{}".into()), Response::Count(4), Response::Ok];
        let mut wire = Vec::new();
        for r in &all {
            write_frame(&mut wire, &r.encode()).unwrap();
        }
        let mut rd = &wire[..];
        for r in &all {
            assert_eq!(&Response::decode(&read_frame(&mut rd).unwrap().unwrap()).unwrap(), r);
        }
        assert_eq!(read_frame(&mut rd).unwrap(), None);
    }

    #[test]
    fn rejects_garbage() {
        assert!(Request::decode(&[]).is_err());
        assert!(Request::decode(&[PROTOCOL, 1, 0, 9]).is_err());
        assert!(Response::decode(&[PROTOCOL + 1, 1]).is_err());
        let mut huge = &(MAX_FRAME + 1).to_le_bytes()[..];
        assert!(read_frame(&mut huge).is_err());
    }

    #[test]
    fn full_paths() {
        let r = |name: &str, dir: &str| Row { name: name.into(), dir: dir.into(), ..Default::default() };
        assert_eq!(r("C:", "").full_path(), r"C:\");
        assert_eq!(r("x", r"C:\").full_path(), r"C:\x");
        assert_eq!(r("x", r"C:\a").full_path(), r"C:\a\x");
    }

    #[test]
    fn session_search_fetch_config_runs() {
        let mut ix = sample();
        ix.volume_serial = 3;
        let mut be = Local::new(Arc::new(Engine::from_indexes(vec![ix])));
        let p = be.search("", SearchFlags::default(), SortKey::Name, false, 2).unwrap();
        assert_eq!((p.total, p.rows.len()), (7, 2));
        let p2 = be.fetch(2, 10).unwrap();
        let names: Vec<_> = p2.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["notepad.exe", "notes.txt", "Report.PDF", "Users", "Windows"]);
        assert_eq!(p2.rows[0].full_path(), r"C:\Windows\notepad.exe");
        assert_eq!(be.record_run(r"C:\Windows\notepad.exe"), Some(1));
        let p = be.search("notepad", SearchFlags::default(), SortKey::Name, false, 10).unwrap();
        assert_eq!(p.rows[0].run_count, 1);
        let mut c = be.get_config().unwrap();
        c.exclude_files = vec!["*.exe".into()];
        be.set_config(&c).unwrap();
        assert_eq!(be.search("notepad", SearchFlags::default(), SortKey::Name, false, 10).unwrap().total, 0);
        assert!(be.search("size:x", SearchFlags::default(), SortKey::Name, false, 10).is_err());
        assert!(be.generation().is_some());
    }

    /// Real named-pipe round trip with the production DACL and client access
    /// mask (works unelevated: the test user is an interactive user).
    #[test]
    fn pipe_roundtrip() {
        let name = format!(r"\\.\pipe\rapidfind-test-{}", std::process::id());
        let server_name = name.clone();
        let engine = Arc::new(std::sync::OnceLock::new());
        let e2 = engine.clone();
        std::thread::spawn(move || serve_on(&server_name, engine_handler(e2)));
        let mut c = loop {
            if let Ok(c) = Client::connect_to(&name) {
                break c;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // Before the engine is set: loading.
        assert_eq!(c.call(&Request::Status).unwrap(), Response::Status(StatusInfo::default()));
        let search = Request::Search { query: "note".into(), flags: SearchFlags::default(), sort: SortKey::Name, descending: false, page: 1 };
        assert_eq!(c.call(&search).unwrap(), Response::Error(LOADING.into()));
        let _ = engine.set(Arc::new(Engine::from_indexes(vec![sample()])));
        let Response::Page(p) = c.call(&search).unwrap() else { panic!() };
        assert_eq!((p.total, p.rows.len()), (2, 1));
        // A second client is served concurrently by another instance, with its own session.
        let mut c2 = Client::connect_to(&name).unwrap();
        assert!(matches!(c2.call(&Request::Fetch { offset: 0, count: 1 }).unwrap(), Response::Error(_)));
        let Response::Page(p) = c.call(&Request::Fetch { offset: 1, count: 5 }).unwrap() else { panic!() };
        assert_eq!(p.rows[0].name, "notes.txt");
    }
}
