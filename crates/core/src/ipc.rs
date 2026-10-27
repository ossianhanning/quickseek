//! Named-pipe IPC between the elevated service (owns the index) and
//! unelevated clients (CLI, terminal UI, GUI).
//!
//! Wire format: every message is a `u32` little-endian length followed by
//! that many bytes, starting with a protocol version byte. Strings are `u32`
//! length + UTF-8.
//!
//! Versions: a client opens with a hello (first byte [`HELLO`], a layout that
//! never changes) naming the protocol range and build it speaks; the service
//! picks the newest version both know and answers every later request in the
//! version it came in. The service serves [`MIN_PROTOCOL`]..=[`PROTOCOL`], so
//! a client one release older (an MCP server an AI app started before an
//! update) keeps working. Services from before the hello answer it with an
//! error in their own version, which tells the client what they speak. When
//! the two have no version in common, calls fail with a [`VersionMismatch`]
//! error, which clients can tell apart from the service restarting.
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

use windows_sys::Win32::Foundation::{
    ERROR_PIPE_CONNECTED, GetLastError, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};

use crate::config::Config;
use crate::engine::Engine;
use crate::history::SnapInfo;
use crate::ntfs::win::wide;
use crate::query::SearchFlags;
use crate::search::{Hit, ResultSet, SearchOptions, SortKey};
use crate::tree::{NodeRef, Tree, TreeLevel, TreeRow, TreeSort};

pub const PIPE_NAME: &str = r"\\.\pipe\quickseek";
const MAX_FRAME: u32 = 64 << 20;
/// Newest protocol version this build speaks.
pub const PROTOCOL: u8 = 4;
/// Oldest version it still serves and speaks (3: before folder-size history).
pub const MIN_PROTOCOL: u8 = 3;
/// First byte of a hello and of its answer (no protocol version is 0).
const HELLO: u8 = 0;
/// This build: version + git commit (see build.rs).
pub const BUILD: &str = env!("QUICKSEEK_BUILD");
/// Error text for a fetch from a result set the index has outgrown.
pub const STALE: &str = "stale result set";
pub const LOADING: &str = "index is still loading";
pub const NO_HISTORY: &str = "folder-size history isn't kept here";
/// In a volume's status state when its index can't be saved (e.g. the disk is full).
pub const SAVE_FAILED: &str = "index can't be saved";

/// Pipe DACL: full access for SYSTEM, Administrators and the pipe owner (the
/// service); interactive users get
/// FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE only.
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)(A;;0x100083;;;IU)";
/// Same access a client asks for when opening the pipe.
const CLIENT_ACCESS: u32 = 0x0010_0083;

// ---- messages ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Search {
        query: String,
        flags: SearchFlags,
        sort: SortKey,
        descending: bool,
        page: u32,
    },
    Fetch {
        offset: u64,
        count: u32,
    },
    Status,
    GetConfig,
    SetConfig(String),
    RecordRun(String),
    /// Opens a folder-size tree (only over `query`'s matches if non-empty)
    /// and returns the volume roots.
    Tree {
        query: String,
        flags: SearchFlags,
        sort: TreeSort,
        descending: bool,
    },
    /// Children of a folder in the open tree.
    Children {
        node: NodeRef,
        sort: TreeSort,
        descending: bool,
        offset: u64,
        count: u32,
    },
    /// Frees the open tree.
    TreeClose,
    /// Rows of the current result set in a fixed order (its sort order when
    /// first asked); positions stay valid across [`Request::DropRows`]. For
    /// content search, which the client does.
    Snapshot {
        offset: u64,
        count: u32,
    },
    /// Removes snapshot positions from the result set; answers with its
    /// first `page` rows.
    DropRows {
        positions: Vec<u32>,
        page: u32,
    },
    /// Folder-size snapshots, oldest first.
    History,
    /// Takes a snapshot now, named (a save point).
    SavePoint {
        name: String,
    },
    /// Compare the open tree (and later ones) with snapshot `id`; empty = stop.
    Compare {
        id: String,
    },
    /// Sizes of these folders in every snapshot.
    Series {
        paths: Vec<String>,
    },
}

/// One folder's size in every snapshot (aligned with [`SeriesReply::snaps`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SeriesRow {
    pub path: String,
    /// `u64::MAX` where the snapshot doesn't have it (smaller than its threshold).
    pub sizes: Vec<u64>,
    /// Volume roots: free bytes then (`u64::MAX` = unknown).
    pub free: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SeriesReply {
    pub snaps: Vec<SnapInfo>,
    pub rows: Vec<SeriesRow>,
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
    Tree(TreeLevel),
    Snapshots(Vec<SnapInfo>),
    Series(SeriesReply),
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
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("malformed message: {what}"),
    )
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

/// Whether protocol `ver` (a supported one) has request kind `kind`.
fn request_known(ver: u8, kind: u8) -> bool {
    ver >= 4 || kind <= 11
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        self.encode_for(PROTOCOL)
            .expect("the newest protocol has every request")
    }

    /// Encodes in protocol `ver`; fails if that version doesn't have this request.
    pub fn encode_for(&self, ver: u8) -> io::Result<Vec<u8>> {
        let mut w = W(vec![ver]);
        match self {
            Request::Search {
                query,
                flags,
                sort,
                descending,
                page,
            } => {
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
            Request::Tree {
                query,
                flags,
                sort,
                descending,
            } => {
                w.u8(7);
                w.u8(flags.bits());
                w.u8(sort.to_u8());
                w.u8(*descending as u8);
                w.str(query);
            }
            Request::Children {
                node,
                sort,
                descending,
                offset,
                count,
            } => {
                w.u8(8);
                w.str(&node.vol);
                w.u32(node.idx);
                w.u64(node.epoch);
                w.str(&node.path);
                w.u8(sort.to_u8());
                w.u8(*descending as u8);
                w.u64(*offset);
                w.u32(*count);
            }
            Request::TreeClose => w.u8(9),
            Request::History => w.u8(12),
            Request::SavePoint { name } => {
                w.u8(13);
                w.str(name);
            }
            Request::Compare { id } => {
                w.u8(14);
                w.str(id);
            }
            Request::Series { paths } => {
                w.u8(15);
                w.u32(paths.len() as u32);
                for p in paths {
                    w.str(p);
                }
            }
            Request::Snapshot { offset, count } => {
                w.u8(10);
                w.u64(*offset);
                w.u32(*count);
            }
            Request::DropRows { positions, page } => {
                w.u8(11);
                w.u32(*page);
                w.u32(positions.len() as u32);
                for p in positions {
                    w.u32(*p);
                }
            }
        }
        if !request_known(ver, w.0[1]) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "the Quickseek service is too old for this (it speaks protocol v{ver}); update Quickseek"
                ),
            ));
        }
        Ok(w.0)
    }

    pub fn decode(b: &[u8]) -> io::Result<Request> {
        match Self::decode_any(b)? {
            (PROTOCOL, r) => Ok(r),
            _ => Err(bad("protocol version")),
        }
    }

    /// Decodes a request in any supported version; returns the version too.
    pub fn decode_any(b: &[u8]) -> io::Result<(u8, Request)> {
        let mut r = R(b);
        let ver = r.u8()?;
        if !(MIN_PROTOCOL..=PROTOCOL).contains(&ver) {
            return Err(bad("protocol version"));
        }
        let kind = r.u8()?;
        if !request_known(ver, kind) {
            return Err(bad("request kind"));
        }
        Ok((
            ver,
            match kind {
                1 => {
                    let flags = SearchFlags::from_bits(r.u8()?);
                    let sort = sort_from_u8(r.u8()?)?;
                    let descending = r.u8()? != 0;
                    let page = r.u32()?;
                    Request::Search {
                        query: r.str()?,
                        flags,
                        sort,
                        descending,
                        page,
                    }
                }
                2 => Request::Fetch {
                    offset: r.u64()?,
                    count: r.u32()?,
                },
                3 => Request::Status,
                4 => Request::GetConfig,
                5 => Request::SetConfig(r.str()?),
                6 => Request::RecordRun(r.str()?),
                7 => {
                    let flags = SearchFlags::from_bits(r.u8()?);
                    let sort = TreeSort::from_u8(r.u8()?).ok_or_else(|| bad("tree sort"))?;
                    let descending = r.u8()? != 0;
                    Request::Tree {
                        query: r.str()?,
                        flags,
                        sort,
                        descending,
                    }
                }
                8 => {
                    let node = NodeRef {
                        vol: r.str()?,
                        idx: r.u32()?,
                        epoch: r.u64()?,
                        path: r.str()?,
                    };
                    let sort = TreeSort::from_u8(r.u8()?).ok_or_else(|| bad("tree sort"))?;
                    Request::Children {
                        node,
                        sort,
                        descending: r.u8()? != 0,
                        offset: r.u64()?,
                        count: r.u32()?,
                    }
                }
                9 => Request::TreeClose,
                10 => Request::Snapshot {
                    offset: r.u64()?,
                    count: r.u32()?,
                },
                12 => Request::History,
                13 => Request::SavePoint { name: r.str()? },
                14 => Request::Compare { id: r.str()? },
                15 => {
                    let n = r.u32()? as usize;
                    if n > r.0.len() / 4 {
                        return Err(bad("truncated"));
                    }
                    Request::Series {
                        paths: (0..n).map(|_| r.str()).collect::<io::Result<_>>()?,
                    }
                }
                11 => {
                    let page = r.u32()?;
                    let n = r.u32()? as usize;
                    if n > r.0.len() / 4 {
                        return Err(bad("truncated"));
                    }
                    let positions = (0..n).map(|_| r.u32()).collect::<io::Result<_>>()?;
                    Request::DropRows { positions, page }
                }
                _ => return Err(bad("request kind")),
            },
        ))
    }
}

impl Response {
    pub fn encode(&self) -> Vec<u8> {
        self.encode_for(PROTOCOL)
    }

    /// Encodes in protocol `ver` (a supported one). Answers that version
    /// doesn't have become errors (its clients never ask for them).
    pub fn encode_for(&self, ver: u8) -> Vec<u8> {
        if ver < 4 && matches!(self, Response::Snapshots(_) | Response::Series(_)) {
            return Response::Error("not available in this client's protocol version".into())
                .encode_for(ver);
        }
        let mut w = W(vec![ver]);
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
            Response::Tree(l) => {
                w.u8(7);
                w.u64(l.size);
                w.u64(l.cloud);
                w.u64(l.files);
                w.u64(l.total);
                let history = ver >= 4;
                if history {
                    w.u64(l.before);
                    w.u8(l.has_before as u8);
                }
                w.u32(l.rows.len() as u32);
                for r in &l.rows {
                    w.str(&r.name);
                    w.str(&r.dir);
                    w.u64(r.size);
                    w.u64(r.cloud);
                    w.u64(r.files);
                    w.u64(r.modified as u64);
                    let f = r.is_dir as u8
                        | (r.has_children as u8) << 1
                        | (r.has_size as u8) << 2
                        | (r.matched as u8) << 3
                        | (r.hardlink as u8) << 4;
                    if history {
                        w.u8(f | (r.has_before as u8) << 5 | (r.changed as u8) << 6);
                        w.u64(r.before);
                        w.u64(r.free_before);
                    } else {
                        w.u8(f);
                    }
                    w.str(&r.vol);
                    w.u32(r.idx);
                    w.u64(r.epoch);
                    w.u64(r.capacity);
                    w.u64(r.free);
                }
            }
            Response::Snapshots(list) => {
                w.u8(8);
                snaps_out(&mut w, list);
            }
            Response::Series(s) => {
                w.u8(9);
                snaps_out(&mut w, &s.snaps);
                w.u32(s.rows.len() as u32);
                for row in &s.rows {
                    w.str(&row.path);
                    w.u32(row.sizes.len() as u32);
                    for &v in &row.sizes {
                        w.u64(v);
                    }
                    w.u32(row.free.len() as u32);
                    for &v in &row.free {
                        w.u64(v);
                    }
                }
            }
        }
        w.0
    }

    pub fn decode(b: &[u8]) -> io::Result<Response> {
        Self::decode_for(b, PROTOCOL)
    }

    /// Decodes an answer in protocol `ver` (the one the request went in).
    pub fn decode_for(b: &[u8], ver: u8) -> io::Result<Response> {
        let mut r = R(b);
        if r.u8()? != ver {
            return Err(bad(
                "protocol version (client and service versions differ?)",
            ));
        }
        let history = ver >= 4;
        Ok(match r.u8()? {
            1 => {
                let (total, offset, elapsed_us, generation) =
                    (r.u64()?, r.u64()?, r.u64()?, r.u64()?);
                let n = r.u32()? as usize;
                let mut rows = Vec::with_capacity(n.min(1 << 16));
                for _ in 0..n {
                    let (name, dir) = (r.str()?, r.str()?);
                    let (size, modified, created) = (r.u64()?, r.u64()? as i64, r.u64()? as i64);
                    let (attrs, run_count, flags) = (r.u32()?, r.u32()?, r.u8()?);
                    rows.push(Row {
                        name,
                        dir,
                        size,
                        modified,
                        created,
                        attrs,
                        run_count,
                        is_dir: flags & 1 != 0,
                        has_size: flags & 2 != 0,
                    });
                }
                Response::Page(Page {
                    total,
                    offset,
                    elapsed_us,
                    generation,
                    rows,
                })
            }
            2 => {
                let ready = r.u8()? != 0;
                let generation = r.u64()?;
                let n = r.u32()? as usize;
                let mut volumes = Vec::with_capacity(n.min(64));
                for _ in 0..n {
                    volumes.push(VolumeInfo {
                        key: r.str()?,
                        kind: r.str()?,
                        entries: r.u64()?,
                        ready: r.u8()? != 0,
                        state: r.str()?,
                    });
                }
                Response::Status(StatusInfo {
                    ready,
                    generation,
                    volumes,
                })
            }
            3 => Response::Error(r.str()?),
            4 => Response::Config(r.str()?),
            5 => Response::Count(r.u32()?),
            6 => Response::Ok,
            7 => {
                let mut l = TreeLevel {
                    size: r.u64()?,
                    cloud: r.u64()?,
                    files: r.u64()?,
                    total: r.u64()?,
                    before: 0,
                    has_before: false,
                    rows: Vec::new(),
                };
                if history {
                    (l.before, l.has_before) = (r.u64()?, r.u8()? != 0);
                }
                let n = r.u32()? as usize;
                l.rows.reserve(n.min(1 << 16));
                for _ in 0..n {
                    let (name, dir) = (r.str()?, r.str()?);
                    let (size, cloud, files, modified, f) =
                        (r.u64()?, r.u64()?, r.u64()?, r.u64()? as i64, r.u8()?);
                    let (before, free_before) = if history {
                        (r.u64()?, r.u64()?)
                    } else {
                        (0, 0)
                    };
                    l.rows.push(TreeRow {
                        before,
                        free_before,
                        has_before: f & 32 != 0,
                        changed: f & 64 != 0,
                        name,
                        dir,
                        size,
                        cloud,
                        hardlink: f & 16 != 0,
                        files,
                        modified,
                        is_dir: f & 1 != 0,
                        has_children: f & 2 != 0,
                        has_size: f & 4 != 0,
                        matched: f & 8 != 0,
                        vol: r.str()?,
                        idx: r.u32()?,
                        epoch: r.u64()?,
                        capacity: r.u64()?,
                        free: r.u64()?,
                    });
                }
                Response::Tree(l)
            }
            8 if history => Response::Snapshots(snaps_in(&mut r)?),
            9 if history => {
                let snaps = snaps_in(&mut r)?;
                let n = r.u32()? as usize;
                let mut rows = Vec::with_capacity(n.min(4096));
                let vec_u64 = |r: &mut R| -> io::Result<Vec<u64>> {
                    let n = r.u32()? as usize;
                    if n > r.0.len() / 8 {
                        return Err(bad("truncated"));
                    }
                    (0..n).map(|_| r.u64()).collect()
                };
                for _ in 0..n {
                    let path = r.str()?;
                    let sizes = vec_u64(&mut r)?;
                    let free = vec_u64(&mut r)?;
                    rows.push(SeriesRow { path, sizes, free });
                }
                Response::Series(SeriesReply { snaps, rows })
            }
            _ => return Err(bad("response kind")),
        })
    }
}

fn snaps_out(w: &mut W, list: &[SnapInfo]) {
    w.u32(list.len() as u32);
    for s in list {
        w.str(&s.id);
        w.u64(s.time as u64);
        w.str(&s.name);
        w.u64(s.folders);
        w.u64(s.bytes);
    }
}

fn snaps_in(r: &mut R) -> io::Result<Vec<SnapInfo>> {
    let n = r.u32()? as usize;
    if n > r.0.len() / 12 {
        return Err(bad("truncated"));
    }
    (0..n)
        .map(|_| {
            Ok(SnapInfo {
                id: r.str()?,
                time: r.u64()? as i64,
                name: r.str()?,
                folders: r.u64()?,
                bytes: r.u64()?,
            })
        })
        .collect()
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
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe
            ) =>
        {
            return Ok(None);
        }
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

// ---- versions -------------------------------------------------------------------

/// The client and the service share no protocol version: one of them is
/// from another Quickseek build and has to be updated (or restarted).
#[derive(Debug, Clone, PartialEq)]
pub struct VersionMismatch {
    /// Versions the service speaks, if it said.
    pub service: Option<(u8, u8)>,
    pub service_build: Option<String>,
}

impl std::fmt::Display for VersionMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "this program (quickseek {BUILD}, protocol v{MIN_PROTOCOL}-v{PROTOCOL}) and the Quickseek service"
        )?;
        match (&self.service_build, self.service) {
            (Some(b), Some((lo, hi))) => write!(f, " ({b}, protocol v{lo}-v{hi})")?,
            (None, Some((lo, hi))) if lo == hi => write!(f, " (protocol v{lo})")?,
            (None, Some((lo, hi))) => write!(f, " (protocol v{lo}-v{hi})")?,
            _ => {}
        }
        write!(f, " are different versions")
    }
}

impl std::error::Error for VersionMismatch {}

/// Whether `e` says the service is another, incompatible build (as opposed to
/// not running, restarting or busy).
pub fn is_version_mismatch(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|i| i.is::<VersionMismatch>())
}

fn mismatch(service: Option<(u8, u8)>, service_build: Option<String>) -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        VersionMismatch {
            service,
            service_build,
        },
    )
}

/// What a client sends first: `HELLO, min, max, build, program`.
fn hello(program: &str) -> Vec<u8> {
    let mut w = W(vec![HELLO, MIN_PROTOCOL, PROTOCOL]);
    w.str(BUILD);
    w.str(program);
    w.0
}

/// Answers a hello: `HELLO, chosen (0 = none in common), min, max, build`.
/// Returns the answer and the chosen version.
fn answer_hello(frame: &[u8]) -> (Vec<u8>, u8) {
    let mut r = R(&frame[1..]);
    let parsed = (|| -> io::Result<(u8, u8, String, String)> {
        Ok((r.u8()?, r.u8()?, r.str()?, r.str()?))
    })();
    let (lo, hi, build, program) = parsed.unwrap_or((0, 0, "?".into(), "?".into()));
    let chosen = hi.min(PROTOCOL);
    let chosen = if chosen >= lo.max(MIN_PROTOCOL) {
        chosen
    } else {
        0
    };
    if chosen == 0 {
        log::warn!(
            "{program} {build} (protocol v{lo}-v{hi}) can't talk to this service ({BUILD}, v{MIN_PROTOCOL}-v{PROTOCOL}); update it"
        );
    } else if build != BUILD {
        log::info!("{program} {build} connected (protocol v{chosen}; this service is {BUILD})");
    } else {
        log::debug!("{program} connected");
    }
    let mut w = W(vec![HELLO, chosen, MIN_PROTOCOL, PROTOCOL]);
    w.str(BUILD);
    (w.0, chosen)
}

/// Reads the service's answer to [`hello`]: the version to use and its build.
fn read_hello_answer(frame: &[u8]) -> io::Result<(u8, Option<String>)> {
    match frame.first() {
        Some(&HELLO) => {
            let mut r = R(&frame[1..]);
            let (chosen, lo, hi) = (r.u8()?, r.u8()?, r.u8()?);
            let build = r.str()?;
            if chosen == 0 || !(MIN_PROTOCOL..=PROTOCOL).contains(&chosen) {
                return Err(mismatch(Some((lo, hi)), Some(build)));
            }
            Ok((chosen, Some(build)))
        }
        // A service from before hellos: it rejected ours in its own version.
        Some(&v) if (MIN_PROTOCOL..=PROTOCOL).contains(&v) => Ok((v, None)),
        Some(&v) => Err(mismatch(Some((v, v)), None)),
        None => Err(bad("empty answer")),
    }
}

// ---- answering requests -------------------------------------------------------

/// Per-connection state: the current result set.
#[derive(Default)]
pub struct Session {
    rs: Option<ResultSet>,
    generation: u64,
    tree: Option<Tree>,
    /// Snapshot the tree is compared with.
    compare: Option<Arc<crate::history::PathMap>>,
}

fn rows(eng: &Engine, hits: &[Hit]) -> Vec<Row> {
    let runs = eng.runs();
    eng.with_indexes(|ixs| {
        hits.iter()
            .filter(|h| {
                (h.vol as usize) < ixs.len() && (h.idx as usize) < ixs[h.vol as usize].entries.len()
            })
            .map(|h| {
                let ix = ixs[h.vol as usize];
                let e = &ix.entries[h.idx as usize];
                Row {
                    name: ix.name(h.idx).to_string(),
                    dir: ix.parent_path(h.idx),
                    size: e.size,
                    modified: e.modified(),
                    created: ix.created(h.idx).unwrap_or(0),
                    attrs: e.attrs(),
                    run_count: runs
                        .get(ix.volume_serial, ix.record(h.idx))
                        .map_or(0, |r| r.count),
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
                    .map(|v| VolumeInfo {
                        key: v.key,
                        kind: v.kind.label().into(),
                        entries: v.entries,
                        ready: v.ready,
                        state: v.state,
                    })
                    .collect(),
            }),
            Request::Search {
                query,
                flags,
                sort,
                descending,
                page,
            } => {
                let t = Instant::now();
                let generation = eng.generation();
                let opts = SearchOptions {
                    sort: *sort,
                    descending: *descending,
                    limit: (*page).max(1) as usize,
                };
                match eng.query(query, *flags, &opts) {
                    Ok(rs) => {
                        let first = rows(eng, rs.first_page());
                        let total = rs.total as u64;
                        self.rs = Some(rs);
                        self.generation = generation;
                        Response::Page(Page {
                            total,
                            offset: 0,
                            elapsed_us: t.elapsed().as_micros() as u64,
                            generation,
                            rows: first,
                        })
                    }
                    Err(e) => {
                        self.rs = None;
                        Response::Error(e)
                    }
                }
            }
            Request::Fetch { offset, count } => {
                let Some(rs) = self.rs.as_mut() else {
                    return Response::Error("no search yet".into());
                };
                let t = Instant::now();
                let page = eng.with_indexes(|ixs| {
                    rs.page(ixs, *offset as usize, (*count).min(100_000) as usize)
                });
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
            Request::Snapshot { offset, count } => {
                let Some(rs) = self.rs.as_mut() else {
                    return Response::Error("no search yet".into());
                };
                let t = Instant::now();
                match eng.with_indexes(|ixs| {
                    rs.snapshot_page(ixs, *offset as usize, (*count).min(100_000) as usize)
                }) {
                    Ok((total, hits)) => Response::Page(Page {
                        total: total as u64,
                        offset: *offset,
                        elapsed_us: t.elapsed().as_micros() as u64,
                        generation: self.generation,
                        rows: rows(eng, &hits),
                    }),
                    Err(_) => Response::Error(STALE.into()),
                }
            }
            Request::DropRows { positions, page } => {
                let Some(rs) = self.rs.as_mut() else {
                    return Response::Error("no search yet".into());
                };
                let t = Instant::now();
                let res = eng.with_indexes(|ixs| {
                    rs.drop_positions(ixs, positions)?;
                    rs.page(ixs, 0, (*page).min(100_000) as usize)
                });
                match res {
                    Ok(hits) => Response::Page(Page {
                        total: rs.total as u64,
                        offset: 0,
                        elapsed_us: t.elapsed().as_micros() as u64,
                        generation: self.generation,
                        rows: rows(eng, &hits),
                    }),
                    Err(_) => Response::Error(STALE.into()),
                }
            }
            Request::GetConfig => {
                Response::Config(serde_json::to_string_pretty(&eng.config()).unwrap())
            }
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
            Request::Tree {
                query,
                flags,
                sort,
                descending,
            } => {
                self.tree = None; // free the old one before building
                let tree = Tree::build(eng, query, *flags).and_then(|mut t| {
                    let roots =
                        t.children(eng, &NodeRef::default(), *sort, *descending, 0, usize::MAX)?;
                    Ok((t, roots))
                });
                match tree {
                    Ok((mut t, mut roots)) => {
                        if self.compare.is_some() {
                            t.base = self.compare.clone();
                            match t.children(
                                eng,
                                &NodeRef::default(),
                                *sort,
                                *descending,
                                0,
                                usize::MAX,
                            ) {
                                Ok(r) => roots = r,
                                Err(e) => return Response::Error(e),
                            }
                        }
                        self.tree = Some(t);
                        Response::Tree(roots)
                    }
                    Err(e) => Response::Error(e),
                }
            }
            Request::Children {
                node,
                sort,
                descending,
                offset,
                count,
            } => {
                let Some(t) = self.tree.as_mut() else {
                    return Response::Error("no tree open".into());
                };
                match t.children(
                    eng,
                    node,
                    *sort,
                    *descending,
                    *offset as usize,
                    *count as usize,
                ) {
                    Ok(l) => Response::Tree(l),
                    Err(e) => Response::Error(e),
                }
            }
            Request::TreeClose => {
                self.tree = None;
                Response::Ok
            }
            Request::History => match eng.history() {
                Some(h) => Response::Snapshots(h.list()),
                None => Response::Error(NO_HISTORY.into()),
            },
            Request::SavePoint { name } => match eng.take_snapshot(if name.trim().is_empty() {
                "save point"
            } else {
                name.trim()
            }) {
                Ok(info) => Response::Snapshots(vec![info]),
                Err(e) => Response::Error(e),
            },
            Request::Compare { id } => {
                if id.is_empty() {
                    self.compare = None;
                } else {
                    let Some(h) = eng.history() else {
                        return Response::Error(NO_HISTORY.into());
                    };
                    match h.paths(id) {
                        Ok(m) => self.compare = Some(m),
                        Err(e) => return Response::Error(format!("snapshot {id}: {e}")),
                    }
                }
                if let Some(t) = self.tree.as_mut() {
                    t.base = self.compare.clone();
                }
                Response::Ok
            }
            Request::Series { paths } => match eng.series(paths) {
                Ok(s) => Response::Series(s),
                Err(e) => Response::Error(e),
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
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                s.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                null_mut(),
            )
        };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(Sd(sd))
        }
    }
    fn attrs(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0,
            bInheritHandle: 0,
        }
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
            PIPE_ACCESS_DUPLEX
                | if first {
                    FILE_FLAG_FIRST_PIPE_INSTANCE
                } else {
                    0
                },
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
pub type HandlerFactory = Arc<
    dyn Fn(Option<crate::ntfs::win::Token>) -> Box<dyn FnMut(&Request) -> Response + Send>
        + Send
        + Sync,
>;

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
        let factory = factory.clone();
        std::thread::spawn(move || {
            let mut pipe = pipe;
            // Made on the first frame: Windows lets a server act as its pipe
            // client only once it has read from the pipe (error 1368).
            let mut handler = None;
            while let Ok(Some(frame)) = read_frame(&mut pipe) {
                let out = if frame.first() == Some(&HELLO) {
                    answer_hello(&frame).0
                } else {
                    // Answer in the request's version (clients older than the
                    // hello just send theirs); unknown ones get ours, which
                    // tells the client we differ.
                    match Request::decode_any(&frame) {
                        Ok((ver, req)) => {
                            handler.get_or_insert_with(|| factory(client_token(&pipe)))(&req)
                                .encode_for(ver)
                        }
                        Err(e) => {
                            let ver = frame
                                .first()
                                .copied()
                                .filter(|v| (MIN_PROTOCOL..=PROTOCOL).contains(v))
                                .unwrap_or(PROTOCOL);
                            Response::Error(e.to_string()).encode_for(ver)
                        }
                    }
                };
                if write_frame(&mut pipe, &out).is_err() {
                    break;
                }
            }
        });
    }
}

static TOKEN_ERROR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Why the last client's identity couldn't be captured, if it couldn't.
pub fn token_error() -> Option<String> {
    TOKEN_ERROR
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

/// The identity of the client on `pipe` (after a frame was read from it),
/// for reaching network folders as that user.
fn client_token(pipe: &File) -> Option<crate::ntfs::win::Token> {
    let r = crate::ntfs::win::Token::from_pipe_client(pipe);
    *TOKEN_ERROR.lock().unwrap_or_else(|p| p.into_inner()) = r.as_ref().err().cloned();
    match r {
        Ok(t) => Some(t),
        Err(e) => {
            // Once per kind of failure: clients connect often.
            static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
            let mut seen = SEEN.lock().unwrap_or_else(|p| p.into_inner());
            if !seen.contains(&e) {
                log::warn!("can't act as the connecting client (for network folders): {e}");
                seen.push(e);
            }
            None
        }
    }
}

/// For tests: serves `name` on a background thread, answering each frame
/// with `answer(frame)`, the way a service of another build would.
#[doc(hidden)]
pub fn serve_frames(name: &str, answer: fn(&[u8]) -> Vec<u8>) {
    use std::os::windows::io::AsRawHandle;
    let name = name.to_string();
    std::thread::spawn(move || {
        let sd = Sd::new(PIPE_SDDL).unwrap();
        let sa = sd.attrs();
        let mut first = true;
        while let Ok(mut pipe) = create_instance(&name, &sa, first) {
            first = false;
            unsafe { ConnectNamedPipe(pipe.as_raw_handle(), null_mut()) };
            std::thread::spawn(move || {
                while let Ok(Some(f)) = read_frame(&mut pipe) {
                    if write_frame(&mut pipe, &answer(&f)).is_err() {
                        break;
                    }
                }
            });
        }
    });
}

// ---- client -------------------------------------------------------------------

pub struct Client {
    pipe: File,
    /// The pipe it connected to, for reconnecting.
    name: String,
    /// Protocol version agreed on in the hello.
    ver: u8,
    service_build: Option<String>,
}

impl Client {
    /// Connects to the service (or `QUICKSEEK_PIPE` if set); fails fast if it isn't running.
    pub fn connect() -> io::Result<Client> {
        match std::env::var("QUICKSEEK_PIPE") {
            Ok(p) if !p.is_empty() => Self::connect_to(&p),
            _ => Self::connect_to(PIPE_NAME),
        }
    }

    /// Connects and agrees on a protocol version; fails with a
    /// [`VersionMismatch`] (see [`is_version_mismatch`]) if there is none.
    pub fn connect_to(pipe_name: &str) -> io::Result<Client> {
        let mut c = Self::open(pipe_name)?;
        let program = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default();
        write_frame(&mut c.pipe, &hello(&program))?;
        let frame = read_frame(&mut c.pipe)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "service closed the connection",
            )
        })?;
        (c.ver, c.service_build) = read_hello_answer(&frame)?;
        Ok(c)
    }

    fn open(pipe_name: &str) -> io::Result<Client> {
        let open = || {
            std::fs::OpenOptions::new()
                .access_mode(CLIENT_ACCESS)
                .security_qos_flags(crate::ntfs::win::CLIENT_QOS)
                .open(pipe_name)
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match open() {
                Ok(pipe) => {
                    return Ok(Client {
                        pipe,
                        name: pipe_name.to_string(),
                        ver: PROTOCOL,
                        service_build: None,
                    });
                }
                // All instances busy: wait for one.
                Err(e) if e.raw_os_error() == Some(231) && Instant::now() < deadline => {
                    let name = wide(pipe_name);
                    unsafe { WaitNamedPipeW(name.as_ptr(), 500) };
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// The pipe this client talks to.
    pub fn pipe_name(&self) -> &str {
        &self.name
    }

    /// The protocol version in use.
    pub fn protocol(&self) -> u8 {
        self.ver
    }

    /// The service's build ([`BUILD`]), if it is new enough to say.
    pub fn service_build(&self) -> Option<&str> {
        self.service_build.as_deref()
    }

    pub fn call(&mut self, req: &Request) -> io::Result<Response> {
        write_frame(&mut self.pipe, &req.encode_for(self.ver)?)?;
        let frame = read_frame(&mut self.pipe)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "service closed the connection",
            )
        })?;
        match frame.first() {
            Some(&v) if v != self.ver => Err(mismatch(Some((v, v)), self.service_build.clone())),
            _ => Response::decode_for(&frame, self.ver),
        }
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
    /// The connection to the service is down and being retried.
    fn connection_lost(&self) -> bool {
        false
    }
    /// After a reconnect (once): the service's build, if it says.
    fn take_reconnected(&mut self) -> Option<Option<String>> {
        None
    }

    fn search(
        &mut self,
        query: &str,
        flags: SearchFlags,
        sort: SortKey,
        descending: bool,
        page: u32,
    ) -> Result<Page, String> {
        match self.call(&Request::Search {
            query: query.into(),
            flags,
            sort,
            descending,
            page,
        }) {
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

    /// Rows of the result set's snapshot (see [`Request::Snapshot`]).
    fn snapshot(&mut self, offset: u64, count: u32) -> Result<Page, String> {
        page_reply(self.call(&Request::Snapshot { offset, count }))
    }

    /// Drops snapshot positions; returns the new first page.
    fn drop_rows(&mut self, positions: Vec<u32>, page: u32) -> Result<Page, String> {
        page_reply(self.call(&Request::DropRows { positions, page }))
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

    /// Opens a folder-size tree; returns the volume roots.
    fn tree(
        &mut self,
        query: &str,
        flags: SearchFlags,
        sort: TreeSort,
        descending: bool,
    ) -> Result<TreeLevel, String> {
        tree_reply(self.call(&Request::Tree {
            query: query.into(),
            flags,
            sort,
            descending,
        }))
    }

    fn children(
        &mut self,
        node: &NodeRef,
        sort: TreeSort,
        descending: bool,
        offset: u64,
        count: u32,
    ) -> Result<TreeLevel, String> {
        tree_reply(self.call(&Request::Children {
            node: node.clone(),
            sort,
            descending,
            offset,
            count,
        }))
    }

    fn tree_close(&mut self) {
        let _ = self.call(&Request::TreeClose);
    }

    fn history(&mut self) -> Result<Vec<SnapInfo>, String> {
        snaps_reply(self.call(&Request::History))
    }

    fn save_point(&mut self, name: &str) -> Result<SnapInfo, String> {
        snaps_reply(self.call(&Request::SavePoint { name: name.into() }))?
            .pop()
            .ok_or_else(|| "no snapshot".into())
    }

    /// Compare the tree with snapshot `id` (empty: stop comparing).
    fn compare(&mut self, id: &str) -> Result<(), String> {
        match self.call(&Request::Compare { id: id.into() }) {
            Ok(Response::Ok) => Ok(()),
            Ok(Response::Error(e)) => Err(e),
            Ok(_) => Err("unexpected response".into()),
            Err(e) => Err(format!("service connection: {e}")),
        }
    }

    fn series(&mut self, paths: &[String]) -> Result<SeriesReply, String> {
        match self.call(&Request::Series {
            paths: paths.to_vec(),
        }) {
            Ok(Response::Series(s)) => Ok(s),
            Ok(Response::Error(e)) => Err(e),
            Ok(_) => Err("unexpected response".into()),
            Err(e) => Err(format!("service connection: {e}")),
        }
    }
}

fn page_reply(r: io::Result<Response>) -> Result<Page, String> {
    match r {
        Ok(Response::Page(p)) => Ok(p),
        Ok(Response::Error(e)) => Err(e),
        Ok(_) => Err("unexpected response".into()),
        Err(e) => Err(format!("service connection: {e}")),
    }
}

fn snaps_reply(r: io::Result<Response>) -> Result<Vec<SnapInfo>, String> {
    match r {
        Ok(Response::Snapshots(l)) => Ok(l),
        Ok(Response::Error(e)) => Err(e),
        Ok(_) => Err("unexpected response".into()),
        Err(e) => Err(format!("service connection: {e}")),
    }
}

fn tree_reply(r: io::Result<Response>) -> Result<TreeLevel, String> {
    match r {
        Ok(Response::Tree(l)) => Ok(l),
        Ok(Response::Error(e)) => Err(e),
        Ok(_) => Err("unexpected response".into()),
        Err(e) => Err(format!("service connection: {e}")),
    }
}

pub struct Local {
    pub engine: Arc<Engine>,
    session: Session,
}

impl Local {
    pub fn new(engine: Arc<Engine>) -> Local {
        Local {
            engine,
            session: Session::default(),
        }
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

/// What a call answers while the connection is being re-established.
pub const RECONNECTING: &str = "Reconnecting to the service…";

/// The service over its pipe. When the connection breaks (the service
/// restarted, was updated, or the pipe closed), the next call reconnects
/// and is retried once; while the service is away calls fail fast with
/// [`RECONNECTING`], trying again after 0.5 s, then up to every 5 s.
pub struct Remote {
    client: Option<Client>,
    pipe: String,
    next_try: Instant,
    backoff: Duration,
    reconnected: Option<Option<String>>,
}

const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Errors that mean the connection is gone (not a refused request).
pub fn is_disconnect(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(2 | 109 | 231 | 232 | 233 | 535 | 536)
    ) || matches!(
        e.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
    )
}

impl Remote {
    pub fn new(c: Client) -> Remote {
        Remote {
            pipe: c.pipe_name().to_string(),
            client: Some(c),
            next_try: Instant::now(),
            backoff: BACKOFF_MIN,
            reconnected: None,
        }
    }

    pub fn client(&self) -> Option<&Client> {
        self.client.as_ref()
    }

    fn reconnect(&mut self) -> io::Result<()> {
        let now = Instant::now();
        if now < self.next_try {
            return Err(io::Error::new(io::ErrorKind::NotConnected, RECONNECTING));
        }
        match Client::connect_to(&self.pipe) {
            Ok(c) => {
                self.reconnected = Some(c.service_build().map(String::from));
                self.client = Some(c);
                self.backoff = BACKOFF_MIN;
                Ok(())
            }
            Err(e) if is_version_mismatch(&e) => Err(e),
            Err(_) => {
                self.next_try = now + self.backoff;
                self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
                Err(io::Error::new(io::ErrorKind::NotConnected, RECONNECTING))
            }
        }
    }
}

impl Backend for Remote {
    fn call(&mut self, req: &Request) -> io::Result<Response> {
        if self.client.is_none() {
            self.reconnect()?;
        }
        match self.client.as_mut().unwrap().call(req) {
            Err(e) if is_disconnect(&e) => {
                // Try at once (a restarted service is usually back), then back off.
                self.client = None;
                self.next_try = Instant::now();
                self.backoff = BACKOFF_MIN;
                self.reconnect()?;
                self.client.as_mut().unwrap().call(req)
            }
            r => r,
        }
    }
    fn label(&self) -> &'static str {
        "service"
    }
    fn connection_lost(&self) -> bool {
        self.client.is_none()
    }
    fn take_reconnected(&mut self) -> Option<Option<String>> {
        self.reconnected.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::sample;

    /// The service acts as the connecting user to reach network shares:
    /// the client must open the pipe allowing impersonation, or the server
    /// only gets an identification token, which SMB refuses.
    #[test]
    fn client_allows_impersonation() {
        let name = format!(r"\\.\pipe\quickseek-test-qos-{}", std::process::id());
        let sd = Sd::new(PIPE_SDDL).unwrap();
        let server = create_instance(&name, &sd.attrs(), true).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::os::windows::io::AsRawHandle;
            let mut server = server;
            unsafe { ConnectNamedPipe(server.as_raw_handle(), null_mut()) };
            // As the service does: after the hello was read.
            let hello = read_frame(&mut server).unwrap().unwrap();
            let t = crate::ntfs::win::Token::from_pipe_client(&server);
            let _ = tx.send(t.map(|t| (t.level(), t.user())));
            let _ = write_frame(&mut server, &answer_hello(&hello).0);
        });
        let _client = Client::connect_to(&name).unwrap();
        let (level, user) = rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap_or_else(|e| panic!("capturing the client's token: {e}"));
        assert_eq!(
            level,
            Some(2),
            "impersonation level (2 = SecurityImpersonation)"
        );
        assert!(user.is_some_and(|u| !u.is_empty()));
    }

    /// What the service does with each client: its handler gets the
    /// client's identity (captured once the first request was read; asking
    /// right after the connect fails with error 1368).
    #[test]
    fn serve_hands_the_client_token_to_the_handler() {
        let name = format!(r"\\.\pipe\quickseek-test-token-{}", std::process::id());
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let factory: HandlerFactory = Arc::new(move |token: Option<crate::ntfs::win::Token>| {
            let _ = tx.lock().unwrap().send(token.and_then(|t| t.level()));
            Box::new(|_req: &Request| Response::Error("test".into()))
        });
        let server_name = name.clone();
        std::thread::spawn(move || serve_on(&server_name, factory));
        let mut c = Client::connect_to(&name)
            .or_else(|_| {
                std::thread::sleep(Duration::from_millis(200));
                Client::connect_to(&name)
            })
            .unwrap();
        let _ = c.call(&Request::Status);
        let level = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(level, Some(2), "the handler got no impersonation token");
    }

    #[test]
    fn request_roundtrip() {
        for req in [
            Request::Search {
                query: "ext:rs ünï \"a b\"".into(),
                flags: SearchFlags {
                    case: true,
                    regex: true,
                    ..Default::default()
                },
                sort: SortKey::RunCount,
                descending: true,
                page: 123,
            },
            Request::Search {
                query: String::new(),
                flags: SearchFlags::default(),
                sort: SortKey::Name,
                descending: false,
                page: 0,
            },
            Request::Fetch {
                offset: 1 << 40,
                count: 77,
            },
            Request::Status,
            Request::GetConfig,
            Request::SetConfig("{}".into()),
            Request::RecordRun(r"C:\x".into()),
            Request::Tree {
                query: "*.mp4".into(),
                flags: SearchFlags {
                    path: true,
                    ..Default::default()
                },
                sort: TreeSort::Files,
                descending: true,
            },
            Request::Children {
                node: NodeRef {
                    vol: r"\\nas\share".into(),
                    idx: 7,
                    epoch: 3,
                    path: r"C:\Users".into(),
                },
                sort: TreeSort::Modified,
                descending: false,
                offset: 10,
                count: 500,
            },
            Request::TreeClose,
            Request::Snapshot {
                offset: 5,
                count: 4096,
            },
            Request::DropRows {
                positions: vec![0, 7, u32::MAX],
                page: 512,
            },
            Request::DropRows {
                positions: Vec::new(),
                page: 0,
            },
            Request::History,
            Request::SavePoint {
                name: "before cleanup".into(),
            },
            Request::Compare {
                id: "1760000000-a".into(),
            },
            Request::Series {
                paths: vec![r"C:\Users".into(), "D:".into()],
            },
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
                Row {
                    name: "a.txt".into(),
                    dir: r"C:\Users\me".into(),
                    size: 10,
                    modified: -1,
                    created: 5,
                    attrs: 0x20,
                    run_count: 3,
                    is_dir: false,
                    has_size: true,
                },
                Row {
                    name: "C:".into(),
                    is_dir: true,
                    ..Default::default()
                },
            ],
        });
        let status = Response::Status(StatusInfo {
            ready: true,
            generation: 7,
            volumes: vec![VolumeInfo {
                key: "C".into(),
                kind: "NTFS".into(),
                entries: 3,
                ready: true,
                state: "ready".into(),
            }],
        });
        let tree = Response::Tree(TreeLevel {
            size: 1 << 40,
            cloud: 77,
            files: 12,
            total: 2,
            before: 3,
            has_before: true,
            rows: vec![
                TreeRow {
                    name: "C:".into(),
                    size: 5,
                    files: 2,
                    is_dir: true,
                    has_children: true,
                    has_size: true,
                    vol: "C".into(),
                    idx: 9,
                    epoch: 1,
                    capacity: 100,
                    free: 40,
                    before: 4,
                    has_before: true,
                    free_before: 50,
                    ..Default::default()
                },
                TreeRow {
                    name: "a.mp4".into(),
                    dir: r"C:\v".into(),
                    size: 3,
                    files: 1,
                    modified: -5,
                    matched: true,
                    cloud: 9,
                    hardlink: true,
                    ..Default::default()
                },
            ],
        });
        let snaps = Response::Snapshots(vec![SnapInfo {
            id: "1760000000-s".into(),
            time: 1_760_000_000,
            name: "before cleanup".into(),
            folders: 12,
            bytes: 3456,
        }]);
        let series = Response::Series(SeriesReply {
            snaps: vec![SnapInfo {
                id: "1-a".into(),
                time: 1,
                ..Default::default()
            }],
            rows: vec![SeriesRow {
                path: r"C:\x".into(),
                sizes: vec![5, u64::MAX],
                free: vec![],
            }],
        });
        let all = [
            page,
            status,
            Response::Error("bad size: x".into()),
            Response::Config("{}".into()),
            Response::Count(4),
            Response::Ok,
            tree,
            snaps,
            series,
        ];
        let mut wire = Vec::new();
        for r in &all {
            write_frame(&mut wire, &r.encode()).unwrap();
        }
        let mut rd = &wire[..];
        for r in &all {
            assert_eq!(
                &Response::decode(&read_frame(&mut rd).unwrap().unwrap()).unwrap(),
                r
            );
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
        let r = |name: &str, dir: &str| Row {
            name: name.into(),
            dir: dir.into(),
            ..Default::default()
        };
        assert_eq!(r("C:", "").full_path(), r"C:\");
        assert_eq!(r("x", r"C:\").full_path(), r"C:\x");
        assert_eq!(r("x", r"C:\a").full_path(), r"C:\a\x");
    }

    #[test]
    fn session_search_fetch_config_runs() {
        let mut ix = sample();
        ix.volume_serial = 3;
        let mut be = Local::new(Arc::new(Engine::from_indexes(vec![ix])));
        let p = be
            .search("", SearchFlags::default(), SortKey::Name, false, 2)
            .unwrap();
        assert_eq!((p.total, p.rows.len()), (7, 2));
        let p2 = be.fetch(2, 10).unwrap();
        let names: Vec<_> = p2.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["notepad.exe", "notes.txt", "Report.PDF", "Users", "Windows"]
        );
        assert_eq!(p2.rows[0].full_path(), r"C:\Windows\notepad.exe");
        assert_eq!(be.record_run(r"C:\Windows\notepad.exe"), Some(1));
        let p = be
            .search("notepad", SearchFlags::default(), SortKey::Name, false, 10)
            .unwrap();
        assert_eq!(p.rows[0].run_count, 1);
        let mut c = be.get_config().unwrap();
        c.exclude_files = vec!["*.exe".into()];
        be.set_config(&c).unwrap();
        assert_eq!(
            be.search("notepad", SearchFlags::default(), SortKey::Name, false, 10)
                .unwrap()
                .total,
            0
        );
        assert!(
            be.search("size:x", SearchFlags::default(), SortKey::Name, false, 10)
                .is_err()
        );
        assert!(be.generation().is_some());
    }

    /// Snapshots, comparing the tree with one, and size series.
    #[test]
    fn session_history_compare_series() {
        use crate::index::tests::by_name;
        let dir =
            std::env::temp_dir().join(format!("quickseek-ipc-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let eng = Arc::new(Engine::from_indexes(vec![sample()]));
        let mut be = Local::new(eng.clone());
        assert_eq!(be.history().unwrap_err(), NO_HISTORY);
        eng.enable_history(dir.clone());
        assert!(be.history().unwrap().is_empty());
        // sample() folders are tiny: the threshold would drop them, so snapshot with none.
        let mut t = Tree::build(&eng, "", SearchFlags::default()).unwrap();
        let s = t.snapshot(&eng, "before", 0, 100).unwrap();
        let paths: Vec<String> = s
            .paths()
            .get(r"C:\Users\Me")
            .map(|x| x.to_string())
            .into_iter()
            .collect();
        assert_eq!(paths, ["1024"]);
        let saved = eng.history().unwrap().save(&s).unwrap();
        assert_eq!(be.history().unwrap(), std::slice::from_ref(&saved));
        // The folder grows by 5000 bytes after the snapshot.
        {
            let v = &eng.volumes()[0];
            let mut ix = v.index.write();
            let me = by_name(&ix, "Me");
            ix.add_link(90, 0, me, "big.bin", 0x20, 5000, i64::MAX / 2, 0);
        }
        be.compare(&saved.id).unwrap();
        let roots = be
            .tree("", SearchFlags::default(), TreeSort::Change, true)
            .unwrap();
        let c = &roots.rows[0];
        assert!(c.has_before && c.change() == 5000, "{c:?}");
        let l = be
            .children(
                &NodeRef {
                    path: r"C:\Users\Me".into(),
                    ..Default::default()
                },
                TreeSort::Change,
                true,
                0,
                10,
            )
            .unwrap();
        assert_eq!(l.rows[0].name, "big.bin");
        assert!(l.rows[0].changed && l.rows[0].change() == 5000);
        assert_eq!((l.before, l.has_before), (1024, true));
        let notes = l.rows.iter().find(|r| r.name == "notes.txt").unwrap();
        assert_eq!(notes.change(), 0);
        let series = be
            .series(&[r"C:\Users\Me".into(), r"C:\Nope".into()])
            .unwrap();
        assert_eq!(series.snaps.len(), 1);
        assert_eq!(
            (series.rows[0].sizes[0], series.rows[1].sizes[0]),
            (1024, u64::MAX)
        );
        be.compare("").unwrap();
        let plain = be
            .tree("", SearchFlags::default(), TreeSort::Size, true)
            .unwrap();
        assert!(!plain.rows[0].has_before);
        assert!(be.compare("123-a").is_err());
        let sp = be.save_point("after").unwrap();
        assert!(sp.is_save_point() && sp.name == "after");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn session_tree() {
        let mut be = Local::new(Arc::new(Engine::from_indexes(vec![sample()])));
        let none = NodeRef {
            path: "C:".into(),
            ..Default::default()
        };
        assert!(be.children(&none, TreeSort::Size, true, 0, 10).is_err()); // no tree yet
        let roots = be
            .tree("", SearchFlags::default(), TreeSort::Size, true)
            .unwrap();
        assert_eq!(roots.rows[0].full_path(), r"C:\");
        let r = &roots.rows[0];
        let l = be
            .children(
                &NodeRef {
                    vol: r.vol.clone(),
                    idx: r.idx,
                    epoch: r.epoch,
                    path: r.full_path(),
                },
                TreeSort::Size,
                true,
                0,
                10,
            )
            .unwrap();
        assert_eq!(
            l.rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["Users", "Windows"]
        );
        assert_eq!(l.rows[0].full_path(), r"C:\Users");
        let f = be
            .tree("ext:exe", SearchFlags::default(), TreeSort::Size, true)
            .unwrap();
        assert_eq!(f.rows[0].size, 200);
        assert!(
            be.tree("size:bad", SearchFlags::default(), TreeSort::Size, true)
                .is_err()
        );
        be.tree_close();
        assert!(be.children(&none, TreeSort::Size, true, 0, 10).is_err());
    }

    /// Real named-pipe round trip with the production DACL and client access
    /// mask (works unelevated: the test user is an interactive user).
    #[test]
    fn pipe_roundtrip() {
        let name = format!(r"\\.\pipe\quickseek-test-{}", std::process::id());
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
        assert_eq!(
            c.call(&Request::Status).unwrap(),
            Response::Status(StatusInfo::default())
        );
        let search = Request::Search {
            query: "note".into(),
            flags: SearchFlags::default(),
            sort: SortKey::Name,
            descending: false,
            page: 1,
        };
        assert_eq!(c.call(&search).unwrap(), Response::Error(LOADING.into()));
        let _ = engine.set(Arc::new(Engine::from_indexes(vec![sample()])));
        let Response::Page(p) = c.call(&search).unwrap() else {
            panic!()
        };
        assert_eq!((p.total, p.rows.len()), (2, 1));
        // A second client is served concurrently by another instance, with its own session.
        let mut c2 = Client::connect_to(&name).unwrap();
        assert!(matches!(
            c2.call(&Request::Fetch {
                offset: 0,
                count: 1
            })
            .unwrap(),
            Response::Error(_)
        ));
        let Response::Page(p) = c
            .call(&Request::Fetch {
                offset: 1,
                count: 5,
            })
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(p.rows[0].name, "notes.txt");
        assert_eq!((c.protocol(), c.service_build()), (PROTOCOL, Some(BUILD)));
    }

    /// v3 (the release before folder-size history) still encodes and decodes.
    #[test]
    fn previous_protocol() {
        let tree = Response::Tree(TreeLevel {
            size: 9,
            total: 1,
            before: 3,
            has_before: true,
            rows: vec![TreeRow {
                name: "C:".into(),
                size: 9,
                is_dir: true,
                has_children: true,
                before: 4,
                has_before: true,
                changed: true,
                free_before: 5,
                ..Default::default()
            }],
            ..Default::default()
        });
        let Response::Tree(back) = Response::decode_for(&tree.encode_for(3), 3).unwrap() else {
            panic!()
        };
        assert_eq!(
            (
                back.size,
                back.has_before,
                back.rows[0].name.as_str(),
                back.rows[0].has_children,
                back.rows[0].has_before
            ),
            (9, false, "C:", true, false)
        );
        assert_eq!(
            Response::decode_for(&Response::Snapshots(vec![]).encode_for(3), 3).unwrap(),
            Response::Error("not available in this client's protocol version".into())
        );
        assert!(Request::History.encode_for(3).is_err());
        let s = Request::Search {
            query: "x".into(),
            flags: SearchFlags::default(),
            sort: SortKey::Size,
            descending: true,
            page: 9,
        };
        assert_eq!(
            Request::decode_any(&s.encode_for(3).unwrap()).unwrap(),
            (3, s)
        );
        // A v3 frame with a v4-only kind, or versions nobody speaks, are rejected.
        assert!(Request::decode_any(&[3, 12]).is_err());
        assert!(Request::decode_any(&[MIN_PROTOCOL - 1, 3]).is_err());
        assert!(Request::decode_any(&[PROTOCOL + 1, 3]).is_err());
    }

    fn fake_service(name: &str, answer: fn(&[u8]) -> Vec<u8>) {
        serve_frames(name, answer);
    }

    fn connect_retry(name: &str) -> io::Result<Client> {
        for _ in 0..100 {
            match Client::connect_to(name) {
                Err(e) if e.raw_os_error() == Some(2) => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                r => return r,
            }
        }
        Client::connect_to(name)
    }

    #[test]
    fn talks_to_a_service_from_before_hellos() {
        // A v3 service rejects anything else (the hello too) with a v3 error.
        let name = format!(r"\\.\pipe\quickseek-test-v3-{}", std::process::id());
        fake_service(&name, |f| match Request::decode_any(f) {
            Ok((3, Request::Status)) => Response::Status(StatusInfo {
                ready: true,
                ..Default::default()
            })
            .encode_for(3),
            _ => Response::Error("malformed message: protocol version".into()).encode_for(3),
        });
        let mut c = connect_retry(&name).unwrap();
        assert_eq!((c.protocol(), c.service_build()), (3, None));
        assert!(matches!(c.call(&Request::Status).unwrap(), Response::Status(s) if s.ready));
        // What v3 doesn't have fails cleanly, without a round trip.
        assert!(
            c.call(&Request::History)
                .is_err_and(|e| !is_version_mismatch(&e))
        );
    }

    #[test]
    fn incompatible_service_is_a_version_mismatch() {
        let name = format!(r"\\.\pipe\quickseek-test-v9-{}", std::process::id());
        fake_service(&name, |_| {
            Response::Error("malformed message: protocol version".into()).encode_for(9)
        });
        let e = connect_retry(&name).err().unwrap();
        assert!(is_version_mismatch(&e), "{e}");
        assert!(
            e.to_string().contains("different versions") && e.to_string().contains("v9"),
            "{e}"
        );
        // A newer service that does hellos but dropped our versions says so (and its build).
        let name = format!(r"\\.\pipe\quickseek-test-v9h-{}", std::process::id());
        fake_service(&name, |_| {
            let mut w = W(vec![HELLO, 0, 8, 9]);
            w.str("9.0.0+abc");
            w.0
        });
        let e = connect_retry(&name).err().unwrap();
        assert!(
            is_version_mismatch(&e) && e.to_string().contains("9.0.0+abc"),
            "{e}"
        );
        // Not running is not a mismatch.
        let e = Client::connect_to(r"\\.\pipe\quickseek-test-nobody-here")
            .err()
            .unwrap();
        assert!(!is_version_mismatch(&e));
    }

    /// The real server answers an old client (no hello) in its version, and
    /// picks a version both know from a hello.
    #[test]
    fn serves_older_clients() {
        let name = format!(r"\\.\pipe\quickseek-test-old-{}", std::process::id());
        let server_name = name.clone();
        let engine = Arc::new(std::sync::OnceLock::new());
        let _ = engine.set(Arc::new(Engine::from_indexes(vec![sample()])));
        std::thread::spawn(move || serve_on(&server_name, engine_handler(engine)));
        connect_retry(&name).unwrap();
        let mut raw = Client::open(&name).unwrap();
        let tree = Request::Tree {
            query: String::new(),
            flags: SearchFlags::default(),
            sort: TreeSort::Size,
            descending: true,
        };
        write_frame(&mut raw.pipe, &tree.encode_for(3).unwrap()).unwrap();
        let f = read_frame(&mut raw.pipe).unwrap().unwrap();
        assert!(
            matches!(Response::decode_for(&f, 3).unwrap(), Response::Tree(l) if l.rows[0].name == "C:")
        );
        // A hello from a client speaking v2-v3 gets v3.
        let mut w = W(vec![HELLO, 2, 3]);
        w.str("0.0.9");
        w.str("old-gui");
        write_frame(&mut raw.pipe, &w.0).unwrap();
        assert_eq!(
            read_hello_answer(&read_frame(&mut raw.pipe).unwrap().unwrap()).unwrap(),
            (3, Some(BUILD.to_string()))
        );
        // And a v9 request gets an error in our version: the client sees the mismatch.
        write_frame(&mut raw.pipe, &[9, 3]).unwrap();
        assert_eq!(read_frame(&mut raw.pipe).unwrap().unwrap()[0], PROTOCOL);
    }
}
