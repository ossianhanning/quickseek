//! On-disk index, plus the journal cursor so a restart only replays the
//! changes since the last save.
//!
//! Format v6 is columnar and compressed: each field of every entry is its own
//! stream, transformed so neighbours look alike (record numbers and
//! timestamps as zigzag-varint deltas, parents relative to the entry, sizes
//! as varints), then cut into 4 MB blocks and zstd-compressed in parallel.
//! Names are stored once, in entry order. Anything derivable (folded names,
//! sort ranks, hard-link chains, exclusion flags, arena offsets) is rebuilt
//! on load. Loading reads a fraction of the bytes of a raw dump, which on a
//! slow disk makes it faster, not slower. v6 stores dates modified as the
//! seconds entries keep (v5 had FILETIMEs; it still loads).

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::fold::FoldMode;
use crate::index::{
    Entry, F_DELETED, F_EXCLUDED, F_INCLUDE_ANCESTOR, F_NAME_IN_FOLD, Index, IndexOptions, NONE,
};

const MAGIC: &[u8; 8] = b"RAPIDFD\x06";
/// Dates modified as FILETIMEs; loaded and converted.
const MAGIC_V5: &[u8; 8] = b"RAPIDFD\x05";
const BLOCK: usize = 4 << 20;
const LEVEL: i32 = 3;
/// Flag bits rebuilt on load rather than stored.
const DERIVED_FLAGS: u16 = F_EXCLUDED | F_INCLUDE_ANCESTOR | F_NAME_IN_FOLD;
const STREAMS: usize = 11;

static DB_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Overrides where indexes are stored (the service uses %ProgramData%).
pub fn set_db_dir(dir: PathBuf) {
    let _ = DB_DIR.set(dir);
}

pub fn db_dir() -> PathBuf {
    if let Some(d) = DB_DIR.get() {
        return d.clone();
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("rapidfind")
}

/// Cache file for a folder index, named by a hash of its (case-folded) path.
pub fn folder_db_path(key: &str) -> PathBuf {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.to_lowercase().bytes() {
        h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    db_dir().join("folders").join(format!("{h:016x}.db"))
}

pub fn db_path(letter: char) -> PathBuf {
    db_dir().join(format!("{letter}.db"))
}

fn bad() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "corrupt index file")
}

// ---- varints ---------------------------------------------------------------------

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    (v >> 1) as i64 ^ -((v & 1) as i64)
}

struct Varints<'a> {
    b: &'a [u8],
    p: usize,
}

impl Varints<'_> {
    fn next(&mut self) -> io::Result<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let byte = *self.b.get(self.p).ok_or_else(bad)?;
            self.p += 1;
            v |= ((byte & 0x7F) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
            if shift > 63 {
                return Err(bad());
            }
        }
    }
}

// ---- compressed streams -----------------------------------------------------------

/// Compresses `data` in independent blocks (in parallel).
fn write_stream(w: &mut impl Write, data: &[u8]) -> io::Result<()> {
    let blocks: Vec<io::Result<Vec<u8>>> = data
        .par_chunks(BLOCK)
        .map(|c| zstd::bulk::compress(c, LEVEL))
        .collect();
    w.write_all(&(data.len() as u64).to_le_bytes())?;
    w.write_all(&(blocks.len() as u64).to_le_bytes())?;
    for b in blocks {
        let b = b?;
        w.write_all(&(b.len() as u64).to_le_bytes())?;
        w.write_all(&b)?;
    }
    Ok(())
}

fn read_u64(r: &mut &[u8]) -> io::Result<u64> {
    if r.len() < 8 {
        return Err(bad());
    }
    let v = u64::from_le_bytes(r[..8].try_into().unwrap());
    *r = &r[8..];
    Ok(v)
}

fn read_u32(r: &mut &[u8]) -> io::Result<u32> {
    if r.len() < 4 {
        return Err(bad());
    }
    let v = u32::from_le_bytes(r[..4].try_into().unwrap());
    *r = &r[4..];
    Ok(v)
}

/// Locates the blocks of one stream (no decompression yet).
fn take_stream<'a>(r: &mut &'a [u8]) -> io::Result<(usize, Vec<&'a [u8]>)> {
    let raw = read_u64(r)? as usize;
    let n = read_u64(r)? as usize;
    if raw > (1usize << 40) || n != raw.div_ceil(BLOCK) {
        return Err(bad());
    }
    let mut blocks = Vec::with_capacity(n);
    for _ in 0..n {
        let len = read_u64(r)? as usize;
        if len > r.len() {
            return Err(bad());
        }
        blocks.push(&r[..len]);
        *r = &r[len..];
    }
    Ok((raw, blocks))
}

fn decompress(raw: usize, blocks: &[&[u8]]) -> io::Result<Vec<u8>> {
    let parts: Vec<io::Result<Vec<u8>>> = blocks
        .par_iter()
        .enumerate()
        .map(|(k, b)| zstd::bulk::decompress(b, BLOCK.min(raw - k * BLOCK)))
        .collect();
    let mut out = Vec::with_capacity(raw);
    for p in parts {
        out.extend_from_slice(&p?);
    }
    if out.len() != raw {
        return Err(bad());
    }
    Ok(out)
}

// ---- save / load ------------------------------------------------------------------

/// Builds one column stream from every entry (chunks in parallel).
fn column(n: usize, f: impl Fn(usize, &mut Vec<u8>) + Sync) -> Vec<u8> {
    const CHUNK: usize = 1 << 16;
    let parts: Vec<Vec<u8>> = (0..n.div_ceil(CHUNK))
        .into_par_iter()
        .map(|c| {
            let mut out = Vec::with_capacity(CHUNK * 3);
            for i in c * CHUNK..((c + 1) * CHUNK).min(n) {
                f(i, &mut out);
            }
            out
        })
        .collect();
    parts.concat()
}

/// Saves `ix` atomically (compacting first once garbage passes 20%, or once
/// enough entries were added out of record order to slow record lookups).
pub fn save(ix: &mut Index, path: &Path) -> io::Result<()> {
    prepare_save(ix);
    write(ix, path)
}

/// The part of [`save`] that changes the index (compacting, if worthwhile).
pub fn prepare_save(ix: &mut Index) {
    if ix.garbage * 5 > ix.entries.len() || ix.late_count() * 16 > ix.entries.len() {
        ix.compact();
    }
}

/// The part of [`save`] that only reads the index (so it can run under a
/// read lock, letting searches go on). On failure (a full disk) the partial
/// temporary file is removed and the previous save stays.
pub fn write(ix: &Index, path: &Path) -> io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp");
    let r = write_to(ix, &tmp).and_then(|()| std::fs::rename(&tmp, path));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

fn write_to(ix: &Index, tmp: &Path) -> io::Result<()> {
    let n = ix.entries.len();
    let e = &ix.entries;
    let prev = |i: usize| if i == 0 { None } else { Some(&e[i - 1]) };
    let with_created = ix.created.len() == n;

    let names = column(n, |i, o| o.extend_from_slice(ix.name(i as u32).as_bytes()));
    let recs = ix.records();
    let name_lens = column(n, |i, o| put_varint(o, e[i].name_len() as u64));
    let records = column(n, |i, o| {
        put_varint(
            o,
            zigzag(recs[i] as i64 - if i == 0 { 0 } else { recs[i - 1] as i64 }),
        )
    });
    drop(recs);
    let parents = column(n, |i, o| {
        put_varint(
            o,
            if e[i].parent == NONE {
                0
            } else {
                zigzag(i as i64 - e[i].parent as i64) + 1
            },
        )
    });
    let sizes = column(n, |i, o| put_varint(o, e[i].size));
    let modified = column(n, |i, o| {
        put_varint(
            o,
            zigzag(e[i].mtime as i64 - prev(i).map_or(0, |p| p.mtime as i64)),
        )
    });
    let created = if with_created {
        column(n, |i, o| {
            put_varint(
                o,
                zigzag(ix.created[i].wrapping_sub(if i == 0 { 0 } else { ix.created[i - 1] })),
            )
        })
    } else {
        Vec::new()
    };
    let flags = column(n, |i, o| {
        o.extend_from_slice(&(e[i].flags & !DERIVED_FLAGS).to_le_bytes())
    });
    let seqs = column(n, |i, o| put_varint(o, ix.seq(i as u32) as u64));
    let mut recent = Vec::new();
    for (&i, &t) in &ix.recent {
        put_varint(&mut recent, i as u64);
        put_varint(&mut recent, zigzag(t));
    }
    let mut ids = Vec::new();
    for (&k, &v) in ix.id_map.iter().flatten() {
        ids.extend_from_slice(&k.to_le_bytes());
        ids.extend_from_slice(&v.to_le_bytes());
    }

    {
        let mut w = io::BufWriter::with_capacity(1 << 20, std::fs::File::create(tmp)?);
        w.write_all(MAGIC)?;
        w.write_all(&(ix.letter as u32).to_le_bytes())?;
        w.write_all(&ix.volume_serial.to_le_bytes())?;
        w.write_all(&ix.journal_id.to_le_bytes())?;
        w.write_all(&ix.next_usn.to_le_bytes())?;
        w.write_all(&ix.root.to_le_bytes())?;
        w.write_all(&[
            ix.has_sizes as u8,
            (ix.fold_mode == FoldMode::StripDiacritics) as u8,
            with_created as u8,
            0,
        ])?;
        w.write_all(&(n as u64).to_le_bytes())?;
        w.write_all(&(ix.rec_count as u64).to_le_bytes())?;
        let all: [&Vec<u8>; STREAMS] = [
            &names, &name_lens, &records, &parents, &sizes, &modified, &created, &flags, &seqs,
            &recent, &ids,
        ];
        for s in all {
            write_stream(&mut w, s)?;
        }
        w.flush()?;
    }
    Ok(())
}

pub fn load(path: &Path) -> io::Result<Index> {
    let data = std::fs::read(path)?;
    let mut r: &[u8] = &data;
    let v5 = r.len() >= 8 && &r[..8] == MAGIC_V5;
    if r.len() < 8 || (&r[..8] != MAGIC && !v5) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a rapidfind index (or old version)",
        ));
    }
    r = &r[8..];
    let letter = char::from_u32(read_u32(&mut r)?).unwrap_or('?');
    let volume_serial = read_u32(&mut r)?;
    let journal_id = read_u64(&mut r)?;
    let next_usn = read_u64(&mut r)? as i64;
    let root = read_u32(&mut r)?;
    let hdr = read_u32(&mut r)?.to_le_bytes();
    let n = read_u64(&mut r)? as usize;
    let rec_len = read_u64(&mut r)? as usize;
    if n > u32::MAX as usize || root as usize >= n.max(1) || rec_len > u32::MAX as usize {
        return Err(bad());
    }
    let mut located = Vec::with_capacity(STREAMS);
    for _ in 0..STREAMS {
        located.push(take_stream(&mut r)?);
    }
    let streams: Vec<Vec<u8>> = located
        .par_iter()
        .map(|(raw, b)| decompress(*raw, b))
        .collect::<io::Result<_>>()?;
    let [
        names,
        name_lens,
        records,
        parents,
        sizes,
        modified,
        created,
        flags,
        seqs,
        recent,
        ids,
    ]: [Vec<u8>; STREAMS] = streams.try_into().map_err(|_| bad())?;
    if flags.len() != n * 2 {
        return Err(bad());
    }

    // Decode the varint columns in parallel (each one is sequential).
    let col = |b: &[u8], delta: bool| -> io::Result<Vec<i64>> {
        let mut out = Vec::with_capacity(n);
        let mut acc = 0i64;
        let mut v = Varints { b, p: 0 };
        for _ in 0..n {
            let x = v.next()?;
            if delta {
                acc = acc.wrapping_add(unzigzag(x));
                out.push(acc);
            } else {
                out.push(x as i64);
            }
        }
        Ok(out)
    };
    let has_created = hdr[2] != 0;
    let ((lens, recs), ((pars, szs), ((mods, cre), sq))) = rayon::join(
        || rayon::join(|| col(&name_lens, false), || col(&records, true)),
        || {
            rayon::join(
                || rayon::join(|| col(&parents, false), || col(&sizes, false)),
                || {
                    rayon::join(
                        || {
                            rayon::join(
                                || col(&modified, true),
                                || {
                                    if has_created {
                                        col(&created, true)
                                    } else {
                                        Ok(Vec::new())
                                    }
                                },
                            )
                        },
                        || col(&seqs, false),
                    )
                },
            )
        },
    );
    let (lens, recs, pars, szs, mods, cre, sq) = (lens?, recs?, pars?, szs?, mods?, cre?, sq?);

    let mut entries = Vec::with_capacity(n);
    let mut off = 0usize;
    for i in 0..n {
        let len = lens[i] as usize;
        if len > crate::index::MAX_NAME || off + len > names.len() {
            return Err(bad());
        }
        let parent = match pars[i] as u64 {
            0 => NONE,
            z => {
                let p = i as i64 - unzigzag(z - 1);
                if p < 0 || p as usize >= n {
                    return Err(bad());
                }
                p as u32
            }
        };
        let fl = u16::from_le_bytes([flags[2 * i], flags[2 * i + 1]]);
        entries.push(Entry {
            size: szs[i] as u64,
            mtime: if v5 {
                crate::index::mtime_of(mods[i])
            } else {
                mods[i].clamp(0, u32::MAX as i64) as u32
            },
            parent,
            // Offset into the loaded `names` until rebuild_fold below.
            name_ref: off as u32,
            len_rec: len as u16,
            flags: fl & !DERIVED_FLAGS,
        });
        off += len;
    }
    if off != names.len() || std::str::from_utf8(&names).is_err() {
        return Err(bad());
    }

    let garbage = entries.iter().filter(|e| e.flags & F_DELETED != 0).count();
    let seqs: Vec<u16> = sq.into_iter().map(|s| s as u16).collect();
    let mut recent_map = std::collections::HashMap::new();
    let mut v = Varints { b: &recent, p: 0 };
    while v.p < recent.len() {
        let i = v.next()? as u32;
        let t = unzigzag(v.next()?);
        if (i as usize) < n {
            recent_map.insert(i, t);
        }
    }
    if ids.len() % 20 != 0 {
        return Err(bad());
    }
    let id_map = (!ids.is_empty()).then(|| {
        ids.as_chunks::<20>()
            .0
            .iter()
            .map(|c| {
                (
                    u128::from_le_bytes(c[..16].try_into().unwrap()),
                    u32::from_le_bytes(c[16..].try_into().unwrap()),
                )
            })
            .collect()
    });
    let mut ix = Index {
        letter,
        entries,
        seqs,
        rec_count: rec_len as u32,
        recent: recent_map,
        id_map,
        created: cre,
        opts: IndexOptions {
            created: has_created,
            ..IndexOptions::default()
        },
        fold_mode: if hdr[1] != 0 {
            FoldMode::StripDiacritics
        } else {
            FoldMode::Lower
        },
        root,
        has_sizes: hdr[0] != 0,
        garbage,
        journal_id,
        next_usn,
        volume_serial,
        ..Default::default()
    };
    // The folded arena and single-copy name packing are derived; sort ranks
    // follow in the background (engine::rank_in_background).
    let recs: Vec<u32> = recs.into_iter().map(|r| r as u32).collect();
    ix.rebuild_rec_index(&recs);
    drop(recs);
    ix.fold_raw_names(&names);
    drop(names);
    ix.shrink();
    if ix.late_count() * 16 > ix.entries.len() {
        // Saved out of record order (older folder indexes): sort it once.
        ix.compact();
    }
    Ok(ix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::{by_name, sample};

    #[test]
    fn varints_roundtrip() {
        let mut b = Vec::new();
        let vals = [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX];
        for v in vals {
            put_varint(&mut b, v);
        }
        let mut r = Varints { b: &b, p: 0 };
        for v in vals {
            assert_eq!(r.next().unwrap(), v);
        }
        for v in [0i64, -1, 1, i64::MIN, i64::MAX] {
            assert_eq!(unzigzag(zigzag(v)), v);
        }
    }

    #[test]
    fn roundtrip() {
        let mut ix = sample();
        ix.journal_id = 77;
        ix.next_usn = 1234;
        ix.note_recent(by_name(&ix, "notes.txt"), -5);
        let p = std::env::temp_dir().join(format!("rapidfind-test-{}.db", std::process::id()));
        save(&mut ix, &p).unwrap();
        let mut back = load(&p).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(
            (back.journal_id, back.next_usn, back.letter),
            (77, 1234, 'C')
        );
        assert_eq!(back.entries.len(), ix.entries.len());
        for i in 0..ix.entries.len() as u32 {
            assert_eq!(back.path(i), ix.path(i));
            assert_eq!(back.fold_name(i), ix.fold_name(i));
            assert_eq!({ back.entries[i as usize].size }, {
                ix.entries[i as usize].size
            });
            assert_eq!({ back.entries[i as usize].mtime }, {
                ix.entries[i as usize].mtime
            });
            assert_eq!(back.created(i), ix.created(i));
            assert_eq!(
                back.entries[i as usize].attrs(),
                ix.entries[i as usize].attrs()
            );
        }
        assert_eq!(back.links(35).count(), 1);
        assert_eq!(back.recent.get(&by_name(&back, "notes.txt")), Some(&-5));
        assert!(back.ranks_missing(), "ranks are rebuilt after load");
        back.compute_ranks();
        assert_eq!(back.ranks.by, ix.ranks.by);
    }

    #[test]
    fn roundtrip_with_garbage_renames_and_no_created() {
        let mut ix = sample();
        ix.set_options(IndexOptions {
            created: false,
            ..Default::default()
        });
        let r = by_name(&ix, "notes.txt");
        let p = ix.entries[r as usize].parent;
        ix.rename_entry(r, p, "Renamed.TXT");
        ix.remove_record(35);
        let path = std::env::temp_dir().join(format!("rapidfind-test2-{}.db", std::process::id()));
        save(&mut ix, &path).unwrap();
        let back = load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(back.garbage, ix.garbage);
        assert_eq!(back.name(r), "Renamed.TXT");
        assert_eq!(back.fold_name(r), b"renamed.txt");
        assert_eq!(back.links(35).count(), 0);
        assert!(back.created.is_empty() && !back.opts.created);
        let q = crate::query::Query::parse("note").unwrap();
        assert_eq!(
            crate::search::search(&[&back], &q, &Default::default()).total,
            0
        );
    }

    #[test]
    fn rejects_truncated_files() {
        let mut ix = sample();
        let p = std::env::temp_dir().join(format!("rapidfind-test3-{}.db", std::process::id()));
        save(&mut ix, &p).unwrap();
        let data = std::fs::read(&p).unwrap();
        for cut in [9, data.len() / 2, data.len() - 1] {
            std::fs::write(&p, &data[..cut]).unwrap();
            assert!(load(&p).is_err(), "cut at {cut}");
        }
        std::fs::remove_file(&p).ok();
    }
}
