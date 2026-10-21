//! Raw $MFT reader — the trick that makes the initial index take seconds.
//!
//! Instead of walking directories, we open the volume device, find the $MFT
//! from the boot data, decode its run list, and stream the whole table off
//! disk in large sequential reads:
//!
//! * `FILE_FLAG_NO_BUFFERING` + page-aligned buffers: no copy through, or
//!   pollution of, the system file cache.
//! * $MFT::$BITMAP tells us which records are in use; chunks with no live
//!   records are never read, and dead records are never parsed.
//! * Chunks are read and parsed (fixups, attribute walk, UTF-16 → UTF-8) by
//!   all rayon workers at once, so many reads are in flight and disk and CPU
//!   overlap.
//! * Unlike FSCTL_ENUM_USN_DATA, raw records also give us sizes and timestamps
//!   for free, which is why Everything reads the MFT directly too.

use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use rayon::prelude::*;
use std::time::Instant;

use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_NO_BUFFERING;
use windows_sys::Win32::System::Ioctl::FSCTL_GET_NTFS_VOLUME_DATA;

use super::record::*;
use super::win::{self, AlignedBuf, Handle};
use super::{Meta, Scan, ScanName};

/// NTFS_VOLUME_DATA_BUFFER
#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
pub struct NtfsVolumeData {
    pub volume_serial: i64,
    pub number_sectors: i64,
    pub total_clusters: i64,
    pub free_clusters: i64,
    pub total_reserved: i64,
    pub bytes_per_sector: u32,
    pub bytes_per_cluster: u32,
    pub bytes_per_file_record_segment: u32,
    pub clusters_per_file_record_segment: u32,
    pub mft_valid_data_length: i64,
    pub mft_start_lcn: i64,
    pub mft2_start_lcn: i64,
    pub mft_zone_start: i64,
    pub mft_zone_end: i64,
}

pub fn volume_data(h: &Handle) -> io::Result<NtfsVolumeData> {
    let mut vd = NtfsVolumeData::default();
    unsafe {
        win::ioctl(
            h,
            FSCTL_GET_NTFS_VOLUME_DATA,
            std::ptr::null(),
            0,
            &mut vd as *mut _ as *mut _,
            size_of::<NtfsVolumeData>(),
        )?;
    }
    Ok(vd)
}

/// Bytes read per chunk. Large enough for sequential throughput, small enough
/// that several chunks are in flight for parallel parsing.
const CHUNK_BYTES: usize = 4 << 20;

#[derive(Default, Debug, Clone)]
pub struct MftStats {
    pub total_records: u64,
    pub used_records: u64,
    pub bytes_read: u64,
    pub chunks_skipped: u64,
    pub read_ms: u128,
    pub total_ms: u128,
}

/// Where raw volume bytes are read from: the volume device itself, or the
/// physical disk at the volume's offset (bypasses volume filter drivers that
/// reject raw reads).
pub struct Dev {
    h: Handle,
    base: u64,
}

impl Dev {
    fn read_exact_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        win::read_exact_at(&self.h, self.base + off, buf)
    }
}

#[derive(Clone, Copy, Debug)]
enum Source {
    Volume { unbuffered: bool },
    PhysicalDisk,
}

/// Virtual→physical mapping of a non-resident stream.
struct Stream {
    runs: Vec<Run>,
    cluster: u64,
    size: u64,
}

impl Stream {
    /// Reads `buf.len()` bytes of the stream starting at virtual byte `vstart`.
    /// `vstart` and `buf.len()` must be sector multiples (NO_BUFFERING).
    fn read(&self, h: &Dev, vstart: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let v = vstart + done as u64;
            let vcn = v / self.cluster;
            let in_cluster = v % self.cluster;
            let run = self
                .runs
                .iter()
                .find(|r| vcn >= r.vcn && vcn < r.vcn + r.clusters);
            let Some(run) = run else {
                // Past the last run: zero fill (only happens for rounding at the tail).
                buf[done..].fill(0);
                return Ok(());
            };
            let run_bytes_left = (run.vcn + run.clusters - vcn) * self.cluster - in_cluster;
            let n = (buf.len() - done).min(run_bytes_left as usize);
            match run.lcn {
                Some(lcn) => {
                    let disk = (lcn + (vcn - run.vcn)) * self.cluster + in_cluster;
                    let len = n;
                    h.read_exact_at(disk, &mut buf[done..done + n])
                        .map_err(|e| {
                            ctx(&format!("read {len} bytes at disk offset {disk:#x}"), e)
                        })?;
                }
                None => buf[done..done + n].fill(0),
            }
            done += n;
        }
        Ok(())
    }

    fn read_all(&self, h: &Dev, sector: usize) -> io::Result<Vec<u8>> {
        let len = (self.size as usize).next_multiple_of(sector);
        let mut buf = AlignedBuf::new(len);
        self.read(h, 0, &mut buf[..len])?;
        Ok(buf[..self.size as usize].to_vec())
    }
}

/// Reads record 0 ($MFT itself) and resolves the $DATA and $BITMAP streams,
/// following $ATTRIBUTE_LIST into extension records if the MFT is very fragmented.
fn locate_mft(h: &Dev, vd: &NtfsVolumeData) -> io::Result<(Stream, Vec<u8>)> {
    let rs = vd.bytes_per_file_record_segment as usize;
    let cluster = vd.bytes_per_cluster as u64;
    let sector = vd.bytes_per_sector.max(512) as usize;

    let read_len = rs.next_multiple_of(sector);
    let mut buf = AlignedBuf::new(read_len);
    let off = vd.mft_start_lcn as u64 * cluster;
    h.read_exact_at(off, &mut buf[..read_len]).map_err(|e| {
        ctx(&format!("read MFT record 0 ({read_len} bytes at {off:#x}; sector {sector}, cluster {cluster}, record {rs})"), e)
    })?;
    let mut rec0 = buf[..rs].to_vec();
    if !apply_fixup(&mut rec0) {
        return Err(io::Error::other("MFT record 0 is corrupt"));
    }

    let mut data = Stream {
        runs: Vec::new(),
        cluster,
        size: 0,
    };
    let mut bitmap: Option<Stream> = None;
    let mut bitmap_resident: Option<Vec<u8>> = None;
    let mut attr_list: Option<Vec<u8>> = None;

    let mut take = |rec: &[u8],
                    data: &mut Stream,
                    bitmap: &mut Option<Stream>,
                    attr_list: &mut Option<Vec<u8>>|
     -> io::Result<()> {
        for a in attrs(rec) {
            match a.ty {
                ATTR_DATA if a.name_len() == 0 && a.non_resident() => {
                    if a.lowest_vcn() == 0 {
                        data.size = a.data_size();
                    }
                    data.runs.extend(a.runs());
                }
                ATTR_BITMAP if a.name_len() == 0 => {
                    if a.non_resident() {
                        let s = bitmap.get_or_insert(Stream {
                            runs: Vec::new(),
                            cluster,
                            size: 0,
                        });
                        if a.lowest_vcn() == 0 {
                            s.size = a.data_size();
                        }
                        s.runs.extend(a.runs());
                    } else {
                        bitmap_resident = a.value().map(|v| v.to_vec());
                    }
                }
                ATTR_ATTRIBUTE_LIST => {
                    *attr_list = Some(if a.non_resident() {
                        let s = Stream {
                            runs: a.runs(),
                            cluster,
                            size: a.data_size(),
                        };
                        s.read_all(h, sector)
                            .map_err(|e| ctx("read $MFT attribute list", e))?
                    } else {
                        a.value().unwrap_or_default().to_vec()
                    });
                }
                _ => {}
            }
        }
        Ok(())
    };
    take(&rec0, &mut data, &mut bitmap, &mut attr_list)?;

    // Fragmented $MFT: further $DATA / $BITMAP extents live in extension records.
    if let Some(list) = attr_list {
        let mut ext_records: Vec<u64> = Vec::new();
        let mut p = 0usize;
        while p + 26 <= list.len() {
            let ty = u32le(&list, p);
            let len = u16le(&list, p + 4) as usize;
            let reference = u64le(&list, p + 16) & FRN_MASK;
            if len == 0 {
                break;
            }
            if (ty == ATTR_DATA || ty == ATTR_BITMAP)
                && reference != 0
                && !ext_records.contains(&reference)
            {
                ext_records.push(reference);
            }
            p += len;
        }
        for r in ext_records {
            data.runs.sort_by_key(|r| r.vcn);
            let mut b = AlignedBuf::new(read_len);
            let vstart = (r * rs as u64) / sector as u64 * sector as u64;
            data.read(h, vstart, &mut b[..read_len])?;
            let skew = (r * rs as u64 - vstart) as usize;
            let mut rec = b[skew..skew + rs].to_vec();
            if apply_fixup(&mut rec) {
                take(&rec, &mut data, &mut bitmap, &mut None)?;
            }
        }
    }
    data.runs.sort_by_key(|r| r.vcn);
    data.runs.dedup();

    let bitmap_bytes = match (bitmap, bitmap_resident) {
        (Some(mut s), _) => {
            s.runs.sort_by_key(|r| r.vcn);
            s.read_all(h, sector)
                .map_err(|e| ctx("read $MFT bitmap", e))?
        }
        (None, Some(b)) => b,
        // No bitmap: treat every record as potentially used.
        (None, None) => vec![0xFF; (data.size / rs as u64).div_ceil(8) as usize],
    };
    Ok((data, bitmap_bytes))
}

#[derive(Clone, Copy)]
struct BaseMeta {
    record: u32,
    meta: Meta,
    size_known: bool,
}

struct Chunk {
    first: u64,
    count: usize,
    /// Record ranges `[a, b)` to actually read.
    spans: Vec<(u64, u64)>,
}

#[derive(Default)]
struct ChunkOut {
    base: Vec<BaseMeta>,
    /// (base record, size) from an unnamed $DATA living in an extension record.
    ext_sizes: Vec<(u32, u64)>,
    names: Vec<ScanName>,
    strings: Vec<u8>,
}

fn parse_chunk(buf: &mut [u8], first: u64, count: usize, rs: usize, bitmap: &[u8]) -> ChunkOut {
    let mut out = ChunkOut::default();
    let mut name = String::new();
    for i in 0..count {
        let rec_no = first + i as u64;
        if bitmap
            .get((rec_no / 8) as usize)
            .is_none_or(|b| b & (1 << (rec_no % 8)) == 0)
        {
            continue;
        }
        let rec = &mut buf[i * rs..(i + 1) * rs];
        if !apply_fixup(rec) {
            continue;
        }
        let rec = &*rec;
        let h = header(rec);
        if h.flags & REC_IN_USE == 0 {
            continue;
        }
        let base = (h.base_ref & FRN_MASK) as u32;
        let owner = if base != 0 { base } else { rec_no as u32 };

        let mut meta = Meta {
            in_use: true,
            is_dir: h.flags & REC_DIRECTORY != 0,
            seq: u16le(rec, 16),
            ..Default::default()
        };
        let mut size_known = false;

        for a in attrs(rec) {
            match a.ty {
                ATTR_STANDARD_INFORMATION => {
                    if let Some(si) = a.value().and_then(parse_std_info) {
                        meta.attrs = si.attributes;
                        meta.created = si.created;
                        meta.modified = si.modified;
                    }
                }
                ATTR_FILE_NAME => {
                    let Some(f) = a.value().and_then(parse_file_name) else {
                        continue;
                    };
                    if f.namespace == NS_DOS {
                        continue; // 8.3 alias; the long name is in another $FILE_NAME
                    }
                    name.clear();
                    let it = f
                        .name_utf16
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]));
                    name.extend(
                        char::decode_utf16(it).map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER)),
                    );
                    out.names.push(ScanName {
                        record: owner,
                        parent: (f.parent_ref & FRN_MASK) as u32,
                        off: out.strings.len() as u32,
                        len: name.len() as u32,
                    });
                    out.strings.extend_from_slice(name.as_bytes());
                }
                ATTR_DATA if a.name_len() == 0 && a.lowest_vcn() == 0 => {
                    if base != 0 {
                        out.ext_sizes.push((owner, a.data_size()));
                    } else {
                        meta.size = a.data_size();
                        size_known = true;
                    }
                }
                _ => {}
            }
        }
        if base == 0 {
            out.base.push(BaseMeta {
                record: owner,
                meta,
                size_known,
            });
        }
    }
    out
}

fn ctx(stage: &str, e: io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("{stage}: {e}"))
}

/// Scans the whole MFT of `letter`. `progress(done_records, total_records)`.
///
/// Tries unbuffered volume reads first, then buffered (some storage stacks
/// reject unbuffered I/O), then reading the same bytes through the physical
/// disk (some volume filter drivers reject raw volume reads entirely).
pub fn scan(letter: char, progress: &(dyn Fn(u64, u64) + Sync)) -> io::Result<(Scan, MftStats)> {
    let mut last = None;
    for src in [
        Source::Volume { unbuffered: true },
        Source::Volume { unbuffered: false },
        Source::PhysicalDisk,
    ] {
        match scan_with(letter, src, progress) {
            Ok(r) => return Ok(r),
            Err(e) => {
                log::info!("{letter}: MFT read via {src:?} failed: {e}");
                last = Some(e);
            }
        }
    }
    Err(last.unwrap())
}

fn scan_with(
    letter: char,
    src: Source,
    progress: &(dyn Fn(u64, u64) + Sync),
) -> io::Result<(Scan, MftStats)> {
    let t0 = Instant::now();
    let vol = win::open_volume(letter, 0).map_err(|e| ctx("open volume", e))?;
    let h = match src {
        Source::Volume { unbuffered } => {
            let flags = if unbuffered {
                FILE_FLAG_NO_BUFFERING
            } else {
                0
            };
            Dev {
                h: win::open_volume(letter, flags).map_err(|e| ctx("open volume", e))?,
                base: 0,
            }
        }
        Source::PhysicalDisk => {
            let (disk, base) =
                win::volume_disk_extent(&vol).map_err(|e| ctx("get volume disk extent", e))?;
            let h = win::open_physical_drive(disk, FILE_FLAG_NO_BUFFERING)
                .map_err(|e| ctx(&format!("open PhysicalDrive{disk}"), e))?;
            Dev { h, base }
        }
    };
    let vd = volume_data(&vol).map_err(|e| ctx("FSCTL_GET_NTFS_VOLUME_DATA", e))?;
    let rs = vd.bytes_per_file_record_segment as usize;
    if rs == 0 || !CHUNK_BYTES.is_multiple_of(rs) {
        return Err(io::Error::other(format!(
            "unsupported file record size {rs}"
        )));
    }
    let sector = vd.bytes_per_sector.max(512) as usize;
    let (stream, bitmap) = locate_mft(&h, &vd).map_err(|e| ctx("locate $MFT", e))?;

    let total = (stream.size / rs as u64).min(bitmap.len() as u64 * 8);
    let per_chunk = (CHUNK_BYTES / rs) as u64;

    // Plan chunks of up to CHUNK_BYTES. Within a chunk only spans containing
    // in-use records are read (gaps of GAP_RECORDS+ free records are skipped);
    // fully free chunks are never touched.
    const GAP_RECORDS: u64 = 256;
    let align = (sector / rs).max(1) as u64; // records per sector, for aligned I/O
    let in_use = |r: u64| bitmap[(r / 8) as usize] & (1 << (r % 8)) != 0;
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut skipped = 0u64;
    let mut used = 0u64;
    let mut first = 0u64;
    while first < total {
        let end = (first + per_chunk).min(total);
        let mut spans: Vec<(u64, u64)> = Vec::new();
        for r in first..end {
            if !in_use(r) {
                continue;
            }
            used += 1;
            match spans.last_mut() {
                Some(sp) if r - sp.1 <= GAP_RECORDS => sp.1 = r + 1,
                _ => spans.push((r, r + 1)),
            }
        }
        if spans.is_empty() {
            skipped += 1;
        } else {
            for sp in &mut spans {
                sp.0 = first + (sp.0 - first) / align * align;
                sp.1 = (first + (sp.1 - first).div_ceil(align) * align).min(end.max(sp.1));
            }
            let count = (spans.last().unwrap().1 - first) as usize;
            chunks.push(Chunk {
                first,
                count,
                spans,
            });
        }
        first = end;
    }

    // Every rayon worker reads its own chunk with a positional read and parses
    // it in place: many reads in flight at once (queue depth = pool size),
    // which is what NVMe/SSD need to reach full bandwidth.
    let done = AtomicU64::new(0);
    let io_us = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);
    let failure: Mutex<Option<io::Error>> = Mutex::new(None);
    thread_local! {
        static BUF: std::cell::RefCell<Option<AlignedBuf>> = const { std::cell::RefCell::new(None) };
    }
    let outs: Vec<ChunkOut> = chunks
        .par_iter()
        .with_max_len(1)
        .filter_map(|c| {
            let (first, count) = (c.first, c.count);
            if failure.lock().unwrap().is_some() {
                return None;
            }
            BUF.with(|cell| {
                let mut slot = cell.borrow_mut();
                let buf = slot.get_or_insert_with(|| AlignedBuf::new(CHUNK_BYTES));
                let t = Instant::now();
                for &(a, b) in &c.spans {
                    let off = ((a - first) as usize) * rs;
                    let len = (((b - a) as usize) * rs).next_multiple_of(sector);
                    let len = len.min(buf.len() - off);
                    if let Err(e) = stream.read(&h, a * rs as u64, &mut buf[off..off + len]) {
                        *failure.lock().unwrap() = Some(ctx(&format!("read MFT at record {a}"), e));
                        return None;
                    }
                    bytes.fetch_add(len as u64, Ordering::Relaxed);
                }
                io_us.fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
                let out = parse_chunk(buf, first, count, rs, &bitmap);
                let d = done.fetch_add(count as u64, Ordering::Relaxed) + count as u64;
                progress(d, total);
                Some(out)
            })
        })
        .collect();
    if let Some(e) = failure.into_inner().unwrap() {
        return Err(e);
    }
    let bytes_read = bytes.into_inner();
    let read_ms = (io_us.into_inner() / 1000 / rayon::current_num_threads().max(1) as u64) as u128;
    let meta = vec![Meta::default(); total as usize];

    // Merge chunk outputs.
    let mut meta = meta;
    let mut size_known = vec![false; total as usize];
    let mut scan = Scan {
        has_sizes: true,
        ..Default::default()
    };
    let n_names: usize = outs.iter().map(|o| o.names.len()).sum();
    let n_bytes: usize = outs.iter().map(|o| o.strings.len()).sum();
    scan.names.reserve(n_names);
    scan.strings.reserve(n_bytes);
    for o in &outs {
        for b in &o.base {
            if let Some(m) = meta.get_mut(b.record as usize) {
                *m = b.meta;
                size_known[b.record as usize] = b.size_known;
            }
        }
    }
    for o in outs {
        for &(rec, size) in &o.ext_sizes {
            if let Some(k) = size_known.get_mut(rec as usize)
                && !*k
            {
                meta[rec as usize].size = size;
                *k = true;
            }
        }
        let base = scan.strings.len() as u32;
        scan.names.extend(o.names.iter().map(|n| ScanName {
            off: n.off + base,
            ..*n
        }));
        scan.strings.extend_from_slice(&o.strings);
    }
    scan.meta = meta;

    let stats = MftStats {
        total_records: total,
        used_records: used,
        bytes_read,
        chunks_skipped: skipped,
        read_ms,
        total_ms: t0.elapsed().as_millis(),
    };
    Ok((scan, stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ntfs::record::tests::{REC, RecBuilder};

    #[test]
    fn parse_chunk_merges_names_sizes_and_skips_dos_and_free() {
        let r0 = RecBuilder::new(REC_IN_USE, 0)
            .std_info(10, 20, 0x20)
            .file_name(5, NS_DOS, "LONGNA~1.TXT")
            .file_name(5, NS_WIN32, "long name.txt")
            .resident(ATTR_DATA, 0, b"hello")
            .finish();
        // Extension record of record 0 carrying a hard link name.
        let r1 = RecBuilder::new(REC_IN_USE, 0)
            .file_name(7, NS_POSIX, "link.txt")
            .finish();
        let mut r1 = r1;
        r1[32..40].copy_from_slice(&((3u64 << 48) | 40).to_le_bytes()); // base = record 40
        // Not marked in bitmap: must be ignored.
        let r2 = RecBuilder::new(REC_IN_USE, 0)
            .file_name(5, NS_WIN32, "ghost")
            .finish();

        let mut buf = [r0, r1, r2].concat();
        let out = parse_chunk(&mut buf, 40, 3, REC, &[0, 0, 0, 0, 0, 0b011]);
        assert_eq!(out.base.len(), 1);
        assert_eq!(out.base[0].meta.size, 5);
        assert_eq!(out.base[0].meta.attrs, 0x20);
        let names: Vec<_> = out
            .names
            .iter()
            .map(|n| {
                (
                    n.record,
                    n.parent,
                    std::str::from_utf8(&out.strings[n.off as usize..(n.off + n.len) as usize])
                        .unwrap(),
                )
            })
            .collect();
        assert_eq!(names, vec![(40, 5, "long name.txt"), (40, 7, "link.txt")]);
    }
}
