//! On-disk index: raw dumps of the index arrays plus the journal cursor.
//! Loading is a few large reads (no parsing), then the journal is replayed
//! from the saved USN, which is how a restart avoids rescanning the MFT.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use bytemuck::Pod;

use crate::fold::FoldMode;
use crate::index::{Entry, Index, NONE};

const MAGIC: &[u8; 8] = b"RAPIDFD\x04";

static DB_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Overrides where indexes are stored (the service uses %ProgramData%).
pub fn set_db_dir(dir: PathBuf) {
    let _ = DB_DIR.set(dir);
}

pub fn db_dir() -> PathBuf {
    if let Some(d) = DB_DIR.get() {
        return d.clone();
    }
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
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

fn write_vec<T: Pod, W: Write>(w: &mut W, v: &[T]) -> io::Result<()> {
    w.write_all(&(v.len() as u64).to_le_bytes())?;
    w.write_all(bytemuck::cast_slice(v))
}

fn read_u64<R: Read>(r: &mut R) -> io::Result<u64> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn read_vec<T: Pod, R: Read>(r: &mut R, max: u64) -> io::Result<Vec<T>> {
    let n = read_u64(r)?;
    if n > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "corrupt index file"));
    }
    let mut v: Vec<T> = vec![T::zeroed(); n as usize];
    r.read_exact(bytemuck::cast_slice_mut(&mut v))?;
    Ok(v)
}

/// Saves `ix` atomically. Arrays are written as they are (deleted entries
/// included), so saving is just sequential writes; compaction only happens
/// once garbage passes 20%.
pub fn save(ix: &mut Index, path: &Path) -> io::Result<()> {
    if ix.garbage * 5 > ix.entries.len() {
        ix.compact();
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
        w.write_all(MAGIC)?;
        w.write_all(&(ix.letter as u32).to_le_bytes())?;
        w.write_all(&ix.volume_serial.to_le_bytes())?;
        w.write_all(&ix.journal_id.to_le_bytes())?;
        w.write_all(&ix.next_usn.to_le_bytes())?;
        w.write_all(&ix.root.to_le_bytes())?;
        w.write_all(&[ix.has_sizes as u8, (ix.fold_mode == FoldMode::StripDiacritics) as u8])?;
        // The folded arena and sort ranks are derived data: rebuilt in
        // parallel on load, which is faster than reading them from disk.
        write_vec(&mut w, &ix.entries)?;
        write_vec(&mut w, &ix.names)?;
        let (ri, rt): (Vec<u32>, Vec<i64>) = ix.recent.iter().map(|(&i, &t)| (i, t)).unzip();
        write_vec(&mut w, &ri)?;
        write_vec(&mut w, &rt)?;
        let (ids, recs): (Vec<u128>, Vec<u32>) = ix.id_map.iter().flatten().map(|(&k, &v)| (k, v)).unzip();
        write_vec(&mut w, &ids)?;
        write_vec(&mut w, &recs)?;
        w.write_all(&(ix.rec_map.len() as u64).to_le_bytes())?;
        w.flush()?;
    }
    std::fs::rename(&tmp, path)
}

pub fn load(path: &Path) -> io::Result<Index> {
    let mut r = BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a rapidfind index (or old version)"));
    }
    let mut b4 = [0u8; 4];
    r.read_exact(&mut b4)?;
    let letter = char::from_u32(u32::from_le_bytes(b4)).unwrap_or('?');
    r.read_exact(&mut b4)?;
    let volume_serial = u32::from_le_bytes(b4);
    let journal_id = read_u64(&mut r)?;
    let next_usn = read_u64(&mut r)? as i64;
    r.read_exact(&mut b4)?;
    let root = u32::from_le_bytes(b4);
    let mut b1 = [0u8; 2];
    r.read_exact(&mut b1)?;
    const MAX: u64 = 1 << 32;
    let mut entries: Vec<Entry> = read_vec(&mut r, MAX)?;
    let names: Vec<u8> = read_vec(&mut r, MAX)?;
    let ri: Vec<u32> = read_vec(&mut r, MAX)?;
    let rt: Vec<i64> = read_vec(&mut r, MAX)?;
    let ids: Vec<u128> = read_vec(&mut r, MAX)?;
    let recs: Vec<u32> = read_vec(&mut r, MAX)?;
    let rec_len = read_u64(&mut r)? as usize;
    let bad = ri.len() != rt.len()
        || ids.len() != recs.len()
        || ri.iter().any(|&i| i as usize >= entries.len())
        || root as usize >= entries.len().max(1)
        || entries.iter().any(|e| e.name_off as usize + e.name_len as usize > names.len());
    if bad {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "corrupt index file"));
    }

    // Rebuild the record map (hard-link chains) from live entries.
    let mut rec_map = vec![NONE; rec_len];
    let mut garbage = 0;
    for (i, e) in entries.iter_mut().enumerate() {
        e.next_link = NONE;
        if e.is_deleted() {
            garbage += 1;
            continue;
        }
        let r = e.record as usize;
        if r >= rec_map.len() {
            rec_map.resize(r + 1, NONE);
        }
        e.next_link = rec_map[r];
        rec_map[r] = i as u32;
    }
    let mut ix = Index {
        letter,
        entries,
        names,
        rec_map,
        recent: ri.into_iter().zip(rt).collect(),
        id_map: (!ids.is_empty()).then(|| ids.into_iter().zip(recs).collect()),
        fold_mode: if b1[1] != 0 { FoldMode::StripDiacritics } else { FoldMode::Lower },
        root,
        has_sizes: b1[0] != 0,
        garbage,
        generation: 0,
        journal_id,
        next_usn,
        volume_serial,
        ..Default::default()
    };
    // Sort ranks are computed in the background by the engine (sorting
    // falls back to exact comparisons until then), so loading stays fast.
    ix.rebuild_fold();
    ix.shrink();
    Ok(ix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::sample;

    #[test]
    fn roundtrip() {
        let mut ix = sample();
        ix.journal_id = 77;
        ix.next_usn = 1234;
        let p = std::env::temp_dir().join(format!("rapidfind-test-{}.db", std::process::id()));
        save(&mut ix, &p).unwrap();
        let back = load(&p).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!((back.journal_id, back.next_usn, back.letter), (77, 1234, 'C'));
        assert_eq!(back.entries.len(), ix.entries.len());
        for i in 0..ix.entries.len() as u32 {
            assert_eq!(back.path(i), ix.path(i));
            assert_eq!(back.fold_name(i), ix.fold_name(i));
        }
        assert_eq!(back.links(35).count(), 1);
        assert!(back.ranks_missing(), "ranks are rebuilt after load");
        let mut back = back;
        back.compute_ranks();
        assert_eq!(back.ranks.by, ix.ranks.by);
    }

    #[test]
    fn roundtrip_with_garbage_and_renames() {
        let mut ix = sample();
        let r = (0..ix.entries.len() as u32).find(|&i| ix.name(i) == "notes.txt").unwrap();
        let p = ix.entries[r as usize].parent;
        ix.rename_entry(r, p, "renamed.txt");
        ix.remove_record(35);
        let path = std::env::temp_dir().join(format!("rapidfind-test2-{}.db", std::process::id()));
        save(&mut ix, &path).unwrap();
        let back = load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(back.garbage, ix.garbage);
        assert_eq!(back.fold_name(r), b"renamed.txt");
        assert_eq!(back.links(35).count(), 0);
        let q = crate::query::Query::parse("note").unwrap();
        let h = crate::search::search(&[&back], &q, &Default::default());
        assert_eq!(h.total, 0);
    }
}
