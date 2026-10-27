//! Messages and framing.
//!
//! A frame is a `u32` little-endian length and that many bytes: a tag byte
//! naming the message, then its fields as LEB128 varints and length-prefixed
//! byte strings. Tag bit 0x80 means the fields are zstd-compressed (big
//! change batches). A full transfer is `Full`, then `Chunk`s whose bytes
//! together are one zstd stream of [`Record`]s, then `FullEnd`.

use std::io::{self, Read, Write};

/// Largest frame either side accepts.
pub const MAX_FRAME: usize = 32 << 20;
/// Bytes of compressed records per `Chunk`.
pub const CHUNK: usize = 1 << 20;
/// Change batches with more encoded bytes than this are compressed.
const COMPRESS_OVER: usize = 4096;
const COMPRESSED: u8 = 0x80;

/// One share an agent serves.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ShareInfo {
    /// Short name, by default the folder's name (`data`); the PC's UNC root
    /// is `\\host\name` unless configured otherwise.
    pub name: String,
    /// Path on the NAS (`/volume1/data`), for display.
    pub root: String,
    /// Entries currently indexed.
    pub entries: u64,
}

/// An entry as sent in a full transfer, or created by a change.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Record {
    pub id: u32,
    pub parent: u32,
    pub name: String,
    /// Windows file attributes (directory, hidden, reparse point).
    pub attrs: u32,
    pub size: u64,
    /// FILETIMEs.
    pub modified: i64,
    pub created: i64,
}

impl Record {
    pub fn is_dir(&self) -> bool {
        self.attrs & crate::FILE_ATTRIBUTE_DIRECTORY != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Create(Record),
    /// Removes one entry. A removed folder's contents are removed first.
    Remove {
        id: u32,
    },
    Move {
        id: u32,
        parent: u32,
        name: String,
    },
    Update {
        id: u32,
        size: u64,
        modified: i64,
        created: i64,
    },
}

/// A folder in an agent snapshot (see `history.rs` on the PC).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnapFolder {
    /// Index of the parent in the same list (`u32::MAX` for the root, which is first).
    pub parent: u32,
    pub name: String,
    pub size: u64,
    pub files: u64,
}

/// A big file in an agent snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnapFile {
    /// Index of its folder.
    pub parent: u32,
    pub name: String,
    pub size: u64,
}

/// One share's folder sizes at a time, taken on the agent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentSnap {
    /// Unix seconds.
    pub time: i64,
    pub share: String,
    pub capacity: u64,
    pub free: u64,
    pub folders: Vec<SnapFolder>,
    pub files: Vec<SnapFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// Both sides open with this; the agent's answer carries the chosen
    /// version in `max` (and `min`).
    Hello {
        min: u8,
        max: u8,
        build: String,
    },
    /// Pairing: `proof` = [`crate::pair::pc_proof`].
    Pair {
        proof: [u8; 32],
        nonce: [u8; 16],
        name: String,
    },
    Paired {
        token: [u8; 32],
        proof: [u8; 32],
    },
    Auth {
        token: [u8; 32],
        name: String,
    },
    Welcome {
        agent: String,
        shares: Vec<ShareInfo>,
    },
    /// Stream one share; resume after `seq` of `epoch` when the agent still can.
    Subscribe {
        share: String,
        epoch: u64,
        seq: u64,
    },
    Full {
        epoch: u64,
        seq: u64,
        count: u64,
    },
    Chunk(Vec<u8>),
    FullEnd,
    /// `seq` is the sequence number after the last op.
    Changes {
        seq: u64,
        ops: Vec<Op>,
    },
    Disk {
        capacity: u64,
        free: u64,
    },
    State(String),
    /// Agent snapshots of the subscribed share newer than `since` (Unix seconds).
    History {
        since: i64,
    },
    Snapshots(Vec<AgentSnap>),
    Ping,
    Pong,
    Error(String),
}

mod tag {
    pub const HELLO: u8 = 1;
    pub const PAIR: u8 = 2;
    pub const PAIRED: u8 = 3;
    pub const AUTH: u8 = 4;
    pub const WELCOME: u8 = 5;
    pub const SUBSCRIBE: u8 = 6;
    pub const FULL: u8 = 7;
    pub const CHUNK: u8 = 8;
    pub const FULL_END: u8 = 9;
    pub const CHANGES: u8 = 10;
    pub const DISK: u8 = 11;
    pub const STATE: u8 = 12;
    pub const HISTORY: u8 = 13;
    pub const SNAPSHOTS: u8 = 14;
    pub const PING: u8 = 15;
    pub const PONG: u8 = 16;
    pub const ERROR: u8 = 17;
}

// ---- encoding ----------------------------------------------------------------------

pub fn put(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Signed values zigzag-encoded.
pub fn put_i(out: &mut Vec<u8>, v: i64) {
    put(out, ((v << 1) ^ (v >> 63)) as u64);
}

pub fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put(out, b.len() as u64);
    out.extend_from_slice(b);
}

pub fn put_str(out: &mut Vec<u8>, s: &str) {
    put_bytes(out, s.as_bytes());
}

pub fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("bad message: {what}"))
}

/// Reads fields back.
pub struct Rd<'a>(pub &'a [u8]);

impl<'a> Rd<'a> {
    pub fn u(&mut self) -> io::Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let (&b, rest) = self.0.split_first().ok_or_else(|| bad("truncated"))?;
            self.0 = rest;
            v |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(bad("varint"))
    }

    pub fn u32(&mut self) -> io::Result<u32> {
        u32::try_from(self.u()?).map_err(|_| bad("u32"))
    }

    pub fn i(&mut self) -> io::Result<i64> {
        let v = self.u()?;
        Ok((v >> 1) as i64 ^ -((v & 1) as i64))
    }

    pub fn bytes(&mut self) -> io::Result<&'a [u8]> {
        let n = self.u()? as usize;
        if n > self.0.len() {
            return Err(bad("truncated bytes"));
        }
        let (b, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(b)
    }

    pub fn str(&mut self) -> io::Result<String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| bad("utf-8"))
    }

    pub fn fixed<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        self.bytes()?.try_into().map_err(|_| bad("fixed length"))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Appends a record. Ids are delta-encoded against `prev_id` (full transfers
/// send them ascending, so the deltas are small).
pub fn put_record(out: &mut Vec<u8>, r: &Record, prev_id: u32) {
    put_i(out, r.id as i64 - prev_id as i64);
    put_i(out, r.parent as i64 - r.id as i64);
    put_str(out, &r.name);
    put(out, r.attrs as u64);
    put(out, r.size);
    put_i(out, r.modified);
    put_i(out, r.created - r.modified);
}

pub fn get_record(rd: &mut Rd, prev_id: u32) -> io::Result<Record> {
    let id = (prev_id as i64 + rd.i()?) as u32;
    let parent = (id as i64 + rd.i()?) as u32;
    let name = rd.str()?;
    let attrs = rd.u32()?;
    let size = rd.u()?;
    let modified = rd.i()?;
    let created = modified + rd.i()?;
    Ok(Record {
        id,
        parent,
        name,
        attrs,
        size,
        modified,
        created,
    })
}

fn put_op(out: &mut Vec<u8>, op: &Op) {
    match op {
        Op::Create(r) => {
            out.push(0);
            put_record(out, r, 0);
        }
        Op::Remove { id } => {
            out.push(1);
            put(out, *id as u64);
        }
        Op::Move { id, parent, name } => {
            out.push(2);
            put(out, *id as u64);
            put(out, *parent as u64);
            put_str(out, name);
        }
        Op::Update {
            id,
            size,
            modified,
            created,
        } => {
            out.push(3);
            put(out, *id as u64);
            put(out, *size);
            put_i(out, *modified);
            put_i(out, *created);
        }
    }
}

fn get_op(rd: &mut Rd) -> io::Result<Op> {
    let (&k, rest) = rd.0.split_first().ok_or_else(|| bad("op"))?;
    rd.0 = rest;
    Ok(match k {
        0 => Op::Create(get_record(rd, 0)?),
        1 => Op::Remove { id: rd.u32()? },
        2 => Op::Move {
            id: rd.u32()?,
            parent: rd.u32()?,
            name: rd.str()?,
        },
        3 => Op::Update {
            id: rd.u32()?,
            size: rd.u()?,
            modified: rd.i()?,
            created: rd.i()?,
        },
        _ => return Err(bad("op kind")),
    })
}

fn put_snap(out: &mut Vec<u8>, s: &AgentSnap) {
    put_i(out, s.time);
    put_str(out, &s.share);
    put(out, s.capacity);
    put(out, s.free);
    put(out, s.folders.len() as u64);
    for f in &s.folders {
        put(out, f.parent.wrapping_add(1) as u64);
        put_str(out, &f.name);
        put(out, f.size);
        put(out, f.files);
    }
    put(out, s.files.len() as u64);
    for f in &s.files {
        put(out, f.parent as u64);
        put_str(out, &f.name);
        put(out, f.size);
    }
}

fn get_snap(rd: &mut Rd) -> io::Result<AgentSnap> {
    let mut s = AgentSnap {
        time: rd.i()?,
        share: rd.str()?,
        capacity: rd.u()?,
        free: rd.u()?,
        ..Default::default()
    };
    let n = rd.u()? as usize;
    s.folders.reserve(n.min(1 << 20));
    for _ in 0..n {
        s.folders.push(SnapFolder {
            parent: rd.u32()?.wrapping_sub(1),
            name: rd.str()?,
            size: rd.u()?,
            files: rd.u()?,
        });
    }
    let n = rd.u()? as usize;
    s.files.reserve(n.min(1 << 20));
    for _ in 0..n {
        s.files.push(SnapFile {
            parent: rd.u32()?,
            name: rd.str()?,
            size: rd.u()?,
        });
    }
    Ok(s)
}

impl AgentSnap {
    /// Compressed, for keeping on disk.
    pub fn to_file_bytes(&self) -> Vec<u8> {
        let mut b = b"QSASNAP1".to_vec();
        let mut body = Vec::new();
        put_snap(&mut body, self);
        b.extend(zstd::bulk::compress(&body, 9).unwrap_or_default());
        b
    }

    pub fn from_file_bytes(data: &[u8]) -> io::Result<AgentSnap> {
        let body = data
            .strip_prefix(b"QSASNAP1")
            .ok_or_else(|| bad("snapshot magic"))?;
        let raw = zstd::stream::decode_all(body)?;
        get_snap(&mut Rd(&raw))
    }
}

impl Msg {
    /// The frame body (tag + fields).
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        match self {
            Msg::Hello { min, max, build } => {
                o.push(tag::HELLO);
                o.push(*min);
                o.push(*max);
                put_str(&mut o, build);
            }
            Msg::Pair { proof, nonce, name } => {
                o.push(tag::PAIR);
                put_bytes(&mut o, proof);
                put_bytes(&mut o, nonce);
                put_str(&mut o, name);
            }
            Msg::Paired { token, proof } => {
                o.push(tag::PAIRED);
                put_bytes(&mut o, token);
                put_bytes(&mut o, proof);
            }
            Msg::Auth { token, name } => {
                o.push(tag::AUTH);
                put_bytes(&mut o, token);
                put_str(&mut o, name);
            }
            Msg::Welcome { agent, shares } => {
                o.push(tag::WELCOME);
                put_str(&mut o, agent);
                put(&mut o, shares.len() as u64);
                for s in shares {
                    put_str(&mut o, &s.name);
                    put_str(&mut o, &s.root);
                    put(&mut o, s.entries);
                }
            }
            Msg::Subscribe { share, epoch, seq } => {
                o.push(tag::SUBSCRIBE);
                put_str(&mut o, share);
                put(&mut o, *epoch);
                put(&mut o, *seq);
            }
            Msg::Full { epoch, seq, count } => {
                o.push(tag::FULL);
                put(&mut o, *epoch);
                put(&mut o, *seq);
                put(&mut o, *count);
            }
            Msg::Chunk(b) => {
                o.push(tag::CHUNK);
                o.extend_from_slice(b);
            }
            Msg::FullEnd => o.push(tag::FULL_END),
            Msg::Changes { seq, ops } => {
                let mut body = Vec::new();
                put(&mut body, *seq);
                put(&mut body, ops.len() as u64);
                for op in ops {
                    put_op(&mut body, op);
                }
                if body.len() > COMPRESS_OVER
                    && let Ok(z) = zstd::bulk::compress(&body, 1)
                {
                    o.push(tag::CHANGES | COMPRESSED);
                    o.extend(z);
                } else {
                    o.push(tag::CHANGES);
                    o.extend(body);
                }
            }
            Msg::Disk { capacity, free } => {
                o.push(tag::DISK);
                put(&mut o, *capacity);
                put(&mut o, *free);
            }
            Msg::State(s) => {
                o.push(tag::STATE);
                put_str(&mut o, s);
            }
            Msg::History { since } => {
                o.push(tag::HISTORY);
                put_i(&mut o, *since);
            }
            Msg::Snapshots(list) => {
                let mut body = Vec::new();
                put(&mut body, list.len() as u64);
                for s in list {
                    put_snap(&mut body, s);
                }
                o.push(tag::SNAPSHOTS | COMPRESSED);
                o.extend(zstd::bulk::compress(&body, 3).unwrap_or_default());
            }
            Msg::Ping => o.push(tag::PING),
            Msg::Pong => o.push(tag::PONG),
            Msg::Error(s) => {
                o.push(tag::ERROR);
                put_str(&mut o, s);
            }
        }
        o
    }

    pub fn decode(body: &[u8]) -> io::Result<Msg> {
        let (&t, rest) = body.split_first().ok_or_else(|| bad("empty"))?;
        let inflated;
        let rest = if t & COMPRESSED != 0 {
            inflated = zstd::bulk::decompress(rest, MAX_FRAME * 4)?;
            &inflated[..]
        } else {
            rest
        };
        let mut rd = Rd(rest);
        let m = match t & !COMPRESSED {
            tag::HELLO => {
                let [min, max] = *rest.first_chunk::<2>().ok_or_else(|| bad("hello"))?;
                rd.0 = &rest[2..];
                Msg::Hello {
                    min,
                    max,
                    build: rd.str()?,
                }
            }
            tag::PAIR => Msg::Pair {
                proof: rd.fixed()?,
                nonce: rd.fixed()?,
                name: rd.str()?,
            },
            tag::PAIRED => Msg::Paired {
                token: rd.fixed()?,
                proof: rd.fixed()?,
            },
            tag::AUTH => Msg::Auth {
                token: rd.fixed()?,
                name: rd.str()?,
            },
            tag::WELCOME => {
                let agent = rd.str()?;
                let n = rd.u()? as usize;
                let mut shares = Vec::with_capacity(n.min(256));
                for _ in 0..n {
                    shares.push(ShareInfo {
                        name: rd.str()?,
                        root: rd.str()?,
                        entries: rd.u()?,
                    });
                }
                Msg::Welcome { agent, shares }
            }
            tag::SUBSCRIBE => Msg::Subscribe {
                share: rd.str()?,
                epoch: rd.u()?,
                seq: rd.u()?,
            },
            tag::FULL => Msg::Full {
                epoch: rd.u()?,
                seq: rd.u()?,
                count: rd.u()?,
            },
            tag::CHUNK => return Ok(Msg::Chunk(rest.to_vec())),
            tag::FULL_END => Msg::FullEnd,
            tag::CHANGES => {
                let seq = rd.u()?;
                let n = rd.u()? as usize;
                let mut ops = Vec::with_capacity(n.min(1 << 20));
                for _ in 0..n {
                    ops.push(get_op(&mut rd)?);
                }
                Msg::Changes { seq, ops }
            }
            tag::DISK => Msg::Disk {
                capacity: rd.u()?,
                free: rd.u()?,
            },
            tag::STATE => Msg::State(rd.str()?),
            tag::HISTORY => Msg::History { since: rd.i()? },
            tag::SNAPSHOTS => {
                let n = rd.u()? as usize;
                let mut list = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    list.push(get_snap(&mut rd)?);
                }
                Msg::Snapshots(list)
            }
            tag::PING => Msg::Ping,
            tag::PONG => Msg::Pong,
            tag::ERROR => Msg::Error(rd.str()?),
            _ => return Err(bad(&format!("unknown message {t}"))),
        };
        Ok(m)
    }
}

/// Writes one message as a frame.
pub fn send(w: &mut impl Write, m: &Msg) -> io::Result<()> {
    let body = m.encode();
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend(body);
    w.write_all(&frame)?;
    w.flush()
}

/// Reads one frame and decodes it.
pub fn recv(r: &mut impl Read) -> io::Result<Msg> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(bad("frame length"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Msg::decode(&body)
}

/// Encodes records for a full transfer: ascending ids, zstd-compressed,
/// split into `Chunk` payloads.
pub fn encode_records(records: impl IntoIterator<Item = Record>) -> (u64, Vec<Vec<u8>>) {
    let mut raw = Vec::new();
    let mut prev = 0;
    let mut n = 0u64;
    for r in records {
        put_record(&mut raw, &r, prev);
        prev = r.id;
        n += 1;
    }
    let z = zstd::bulk::compress(&raw, 3).unwrap_or_default();
    (n, z.chunks(CHUNK).map(<[u8]>::to_vec).collect())
}

/// Decodes the concatenated `Chunk` payloads of a full transfer.
pub fn decode_records(z: &[u8], count: u64) -> io::Result<Vec<Record>> {
    let raw = zstd::stream::decode_all(z)?;
    let mut rd = Rd(&raw);
    let mut out = Vec::with_capacity(count.min(50_000_000) as usize);
    let mut prev = 0;
    while !rd.is_empty() {
        let r = get_record(&mut rd, prev)?;
        prev = r.id;
        out.push(r);
    }
    if out.len() as u64 != count {
        return Err(bad("record count"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: u32, parent: u32, name: &str, dir: bool) -> Record {
        Record {
            id,
            parent,
            name: name.into(),
            attrs: if dir {
                crate::FILE_ATTRIBUTE_DIRECTORY
            } else {
                0
            },
            size: if dir { 0 } else { id as u64 * 1000 },
            modified: 133_000_000_000_000_000 + id as i64,
            created: 132_000_000_000_000_000,
        }
    }

    fn round(m: Msg) {
        let mut buf = Vec::new();
        send(&mut buf, &m).unwrap();
        assert_eq!(recv(&mut &buf[..]).unwrap(), m);
    }

    #[test]
    fn messages_round_trip() {
        round(Msg::Hello {
            min: 1,
            max: 3,
            build: "19.9.0 abc".into(),
        });
        round(Msg::Pair {
            proof: [7; 32],
            nonce: [9; 16],
            name: "PC".into(),
        });
        round(Msg::Paired {
            token: [1; 32],
            proof: [2; 32],
        });
        round(Msg::Auth {
            token: [3; 32],
            name: "PC".into(),
        });
        round(Msg::Welcome {
            agent: "hearth".into(),
            shares: vec![ShareInfo {
                name: "data".into(),
                root: "/volume1/data".into(),
                entries: 590_000,
            }],
        });
        round(Msg::Subscribe {
            share: "media".into(),
            epoch: u64::MAX,
            seq: 0,
        });
        round(Msg::Full {
            epoch: 5,
            seq: 6,
            count: 7,
        });
        round(Msg::Chunk(vec![1, 2, 3]));
        round(Msg::FullEnd);
        round(Msg::Disk {
            capacity: 16 << 40,
            free: 15 << 40,
        });
        round(Msg::State("walking".into()));
        round(Msg::History { since: -5 });
        round(Msg::Ping);
        round(Msg::Pong);
        round(Msg::Error("nope".into()));
        round(Msg::Snapshots(vec![AgentSnap {
            time: 1_760_000_000,
            share: "data".into(),
            capacity: 10,
            free: 5,
            folders: vec![
                SnapFolder {
                    parent: u32::MAX,
                    name: String::new(),
                    size: 100 << 20,
                    files: 10,
                },
                SnapFolder {
                    parent: 0,
                    name: "Photos".into(),
                    size: 60 << 20,
                    files: 4,
                },
            ],
            files: vec![SnapFile {
                parent: 1,
                name: "big.mkv".into(),
                size: 50 << 20,
            }],
        }]));
    }

    #[test]
    fn change_batches_compress_and_round_trip() {
        let ops: Vec<Op> = (0..5000u32)
            .map(|i| match i % 4 {
                0 => Op::Create(rec(100 + i, 24, &format!("file {i}.txt"), i % 8 == 0)),
                1 => Op::Remove { id: i },
                2 => Op::Move {
                    id: i,
                    parent: 24,
                    name: format!("moved {i}"),
                },
                _ => Op::Update {
                    id: i,
                    size: i as u64,
                    modified: -1,
                    created: 0,
                },
            })
            .collect();
        let m = Msg::Changes { seq: 42, ops };
        let body = m.encode();
        assert_eq!(body[0], tag::CHANGES | COMPRESSED);
        round(m);
    }

    #[test]
    fn records_round_trip_in_chunks() {
        let recs: Vec<Record> = (crate::FIRST_ID..crate::FIRST_ID + 50_000)
            .map(|i| {
                rec(
                    i,
                    crate::ROOT_ID + (i % 7),
                    &format!("näme {i}"),
                    i % 9 == 0,
                )
            })
            .collect();
        let (n, chunks) = encode_records(recs.clone());
        assert_eq!(n, 50_000);
        let all: Vec<u8> = chunks.concat();
        assert_eq!(decode_records(&all, n).unwrap(), recs);
        assert!(decode_records(&all, n + 1).is_err());
    }

    #[test]
    fn rejects_garbage() {
        assert!(Msg::decode(&[]).is_err());
        assert!(Msg::decode(&[200]).is_err());
        assert!(Msg::decode(&[tag::SUBSCRIBE, 0xFF]).is_err());
        let mut huge = Vec::new();
        huge.extend_from_slice(&(u32::MAX).to_le_bytes());
        assert!(recv(&mut &huge[..]).is_err());
    }

    #[test]
    fn agent_snapshot_file_round_trip() {
        let s = AgentSnap {
            time: 1,
            share: "media".into(),
            ..Default::default()
        };
        assert_eq!(AgentSnap::from_file_bytes(&s.to_file_bytes()).unwrap(), s);
        assert!(AgentSnap::from_file_bytes(b"nope").is_err());
    }
}
