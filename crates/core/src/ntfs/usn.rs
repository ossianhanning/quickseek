//! USN_RECORD parsing shared by the enumeration fallback and the journal, plus
//! the FSCTL_ENUM_USN_DATA scanner (works without parsing the MFT ourselves,
//! but gives no sizes or timestamps).

use std::io;

use windows_sys::Win32::System::Ioctl::FSCTL_ENUM_USN_DATA;

use std::collections::HashMap;

use super::record::{FRN_MASK, u16le, u32le, u64le};
use super::win::{self, Handle};
use super::{Meta, Scan, ScanName};

#[derive(Debug, Clone, Default)]
pub struct UsnRecord {
    pub frn: u64,
    pub parent: u64,
    /// Full ids (V3 records: 128-bit on ReFS; V2: zero-extended).
    pub frn128: u128,
    pub parent128: u128,
    pub usn: i64,
    pub timestamp: i64,
    pub reason: u32,
    pub attrs: u32,
    pub name: String,
}

impl UsnRecord {
    pub fn record(&self) -> u32 {
        (self.frn & FRN_MASK) as u32
    }
    pub fn parent_record(&self) -> u32 {
        (self.parent & FRN_MASK) as u32
    }
    pub fn seq(&self) -> u16 {
        (self.frn >> 48) as u16
    }
}

/// Parses a buffer of USN_RECORD_V2/V3 records (as returned after the leading
/// 8-byte USN / FRN cursor).
pub fn parse_records(mut b: &[u8], mut f: impl FnMut(UsnRecord)) {
    while b.len() >= 8 {
        let len = u32le(b, 0) as usize;
        if len < 60 || len > b.len() {
            break;
        }
        let r = &b[..len];
        let major = u16le(r, 4);
        let u128le = |o: usize| u128::from_le_bytes(r[o..o + 16].try_into().unwrap());
        let parsed = match major {
            2 => {
                let nlen = u16le(r, 56) as usize;
                let noff = u16le(r, 58) as usize;
                let (f, p) = (u64le(r, 8), u64le(r, 16));
                Some((
                    f as u128,
                    p as u128,
                    u64le(r, 24) as i64,
                    u64le(r, 32) as i64,
                    u32le(r, 40),
                    u32le(r, 52),
                    noff,
                    nlen,
                ))
            }
            // V3 uses 128-bit ids; on NTFS the low 64 bits are the classic FRN.
            3 if len >= 76 => {
                let nlen = u16le(r, 72) as usize;
                let noff = u16le(r, 74) as usize;
                Some((
                    u128le(8),
                    u128le(24),
                    u64le(r, 40) as i64,
                    u64le(r, 48) as i64,
                    u32le(r, 56),
                    u32le(r, 68),
                    noff,
                    nlen,
                ))
            }
            _ => None,
        };
        if let Some((frn128, parent128, usn, timestamp, reason, attrs, noff, nlen)) = parsed
            && noff + nlen <= len
        {
            let it = r[noff..noff + nlen]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]));
            let name: String = char::decode_utf16(it)
                .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect();
            f(UsnRecord {
                frn: frn128 as u64,
                parent: parent128 as u64,
                frn128,
                parent128,
                usn,
                timestamp,
                reason,
                attrs,
                name,
            });
        }
        b = &b[len..];
    }
}

#[repr(C)]
struct MftEnumDataV0 {
    start_frn: u64,
    low_usn: i64,
    high_usn: i64,
}

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

/// Enumerates every file via FSCTL_ENUM_USN_DATA. Slower than the raw MFT scan
/// and without sizes/timestamps, but needs no knowledge of the on-disk format.
pub fn scan_enum(h: &Handle) -> io::Result<Scan> {
    let mut scan = Scan::default();
    let mut q = MftEnumDataV0 {
        start_frn: 0,
        low_usn: 0,
        high_usn: i64::MAX,
    };
    let mut buf = vec![0u64; 1 << 17]; // 1 MiB, 8-byte aligned
    loop {
        let res = unsafe {
            win::ioctl(
                h,
                FSCTL_ENUM_USN_DATA,
                &q as *const _ as *const _,
                size_of::<MftEnumDataV0>(),
                buf.as_mut_ptr() as *mut _,
                buf.len() * 8,
            )
        };
        let n = match res {
            Ok(n) => n as usize,
            Err(e) if e.raw_os_error() == Some(win::ERROR_HANDLE_EOF) => break,
            Err(e) => return Err(e),
        };
        if n <= 8 {
            break;
        }
        let bytes: &[u8] = bytemuck::cast_slice(&buf);
        q.start_frn = u64le(bytes, 0);
        parse_records(&bytes[8..n], |r| {
            let rec = r.record() as usize;
            if scan.meta.len() <= rec {
                scan.meta.resize(rec + 1, Meta::default());
            }
            scan.meta[rec] = Meta {
                in_use: true,
                is_dir: r.attrs & FILE_ATTRIBUTE_DIRECTORY != 0,
                seq: r.seq(),
                attrs: r.attrs,
                ..Default::default()
            };
            scan.names.push(ScanName {
                record: r.record(),
                parent: r.parent_record(),
                off: scan.strings.len() as u32,
                len: r.name.len() as u32,
            });
            scan.strings.extend_from_slice(r.name.as_bytes());
        });
    }
    // The root directory is not returned by the enumeration.
    let root = super::ROOT_RECORD as usize;
    if scan.meta.len() <= root {
        scan.meta.resize(root + 1, Meta::default());
    }
    scan.meta[root] = Meta {
        in_use: true,
        is_dir: true,
        attrs: FILE_ATTRIBUTE_DIRECTORY,
        ..Default::default()
    };
    Ok(scan)
}

#[repr(C)]
struct MftEnumDataV1 {
    start_frn: u64,
    low_usn: i64,
    high_usn: i64,
    min_major_version: u16,
    max_major_version: u16,
}

/// Turns enumerated records with 128-bit ids into a [`Scan`] with dense
/// record numbers; `root_id` becomes [`super::ROOT_RECORD`]. Returns the id map.
pub fn scan_from_records(records: &[UsnRecord], root_id: u128) -> (Scan, HashMap<u128, u32>) {
    let mut map: HashMap<u128, u32> = HashMap::from([(root_id, super::ROOT_RECORD)]);
    let mut next = super::FIRST_USER_RECORD;
    let mut id = |x: u128, map: &mut HashMap<u128, u32>| {
        *map.entry(x).or_insert_with(|| {
            next += 1;
            next - 1
        })
    };
    let mut scan = Scan::default();
    for r in records {
        let rec = id(r.frn128, &mut map);
        let parent = id(r.parent128, &mut map);
        let m = rec.max(parent) as usize;
        if scan.meta.len() <= m {
            scan.meta.resize(m + 1, Meta::default());
        }
        scan.meta[rec as usize] = Meta {
            in_use: true,
            is_dir: r.attrs & FILE_ATTRIBUTE_DIRECTORY != 0,
            attrs: r.attrs,
            ..Default::default()
        };
        scan.names.push(ScanName {
            record: rec,
            parent,
            off: scan.strings.len() as u32,
            len: r.name.len() as u32,
        });
        scan.strings.extend_from_slice(r.name.as_bytes());
    }
    let root = super::ROOT_RECORD as usize;
    if scan.meta.len() <= root {
        scan.meta.resize(root + 1, Meta::default());
    }
    scan.meta[root] = Meta {
        in_use: true,
        is_dir: true,
        attrs: FILE_ATTRIBUTE_DIRECTORY,
        ..Default::default()
    };
    // The root's own name entry so Index::from_scan finds it.
    scan.names.push(ScanName {
        record: super::ROOT_RECORD,
        parent: super::ROOT_RECORD,
        off: scan.strings.len() as u32,
        len: 1,
    });
    scan.strings.push(b'.');
    (scan, map)
}

/// ReFS (or any volume with 128-bit file ids): enumerates via
/// FSCTL_ENUM_USN_DATA asking for V3 records.
pub fn scan_enum_v3(h: &Handle, root_id: u128) -> io::Result<(Scan, HashMap<u128, u32>)> {
    let mut q = MftEnumDataV1 {
        start_frn: 0,
        low_usn: 0,
        high_usn: i64::MAX,
        min_major_version: 2,
        max_major_version: 3,
    };
    let mut buf = vec![0u64; 1 << 17];
    let mut records = Vec::new();
    loop {
        let res = unsafe {
            win::ioctl(
                h,
                FSCTL_ENUM_USN_DATA,
                &q as *const _ as *const _,
                size_of::<MftEnumDataV1>(),
                buf.as_mut_ptr() as *mut _,
                buf.len() * 8,
            )
        };
        let n = match res {
            Ok(n) => n as usize,
            Err(e) if e.raw_os_error() == Some(win::ERROR_HANDLE_EOF) => break,
            Err(e) => return Err(e),
        };
        if n <= 8 {
            break;
        }
        let bytes: &[u8] = bytemuck::cast_slice(&buf);
        q.start_frn = u64le(bytes, 0);
        parse_records(&bytes[8..n], |r| records.push(r));
    }
    Ok(scan_from_records(&records, root_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2(frn: u64, parent: u64, reason: u32, name: &str) -> Vec<u8> {
        let u: Vec<u16> = name.encode_utf16().collect();
        let len = (60 + u.len() * 2).next_multiple_of(8);
        let mut r = vec![0u8; len];
        r[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        r[4..6].copy_from_slice(&2u16.to_le_bytes());
        r[8..16].copy_from_slice(&frn.to_le_bytes());
        r[16..24].copy_from_slice(&parent.to_le_bytes());
        r[40..44].copy_from_slice(&reason.to_le_bytes());
        r[56..58].copy_from_slice(&((u.len() * 2) as u16).to_le_bytes());
        r[58..60].copy_from_slice(&60u16.to_le_bytes());
        for (i, c) in u.iter().enumerate() {
            r[60 + i * 2..62 + i * 2].copy_from_slice(&c.to_le_bytes());
        }
        r
    }

    #[test]
    fn dense_scan_from_128_bit_ids() {
        let root: u128 = 1 << 100;
        let mk = |f: u128, p: u128, attrs: u32, name: &str| UsnRecord {
            frn128: f,
            parent128: p,
            attrs,
            name: name.into(),
            ..Default::default()
        };
        // Child listed before its parent: ids are still resolved.
        let recs = [
            mk(30 << 64, 20 << 64, 0x20, "file.txt"),
            mk(20 << 64, root, 0x10, "dir"),
        ];
        let (scan, map) = scan_from_records(&recs, root);
        let ix = crate::index::Index::from_scan('R', &scan);
        let f = (0..ix.entries.len() as u32)
            .find(|&i| ix.name(i) == "file.txt")
            .unwrap();
        assert_eq!(ix.path(f), r"R:\dir\file.txt");
        assert_eq!(map[&root], crate::ntfs::ROOT_RECORD);
    }

    #[test]
    fn parses_v2_records() {
        let buf = [
            v2((2 << 48) | 100, 5, 0x100, "a.txt"),
            v2(101, 100, 0x200, "ünï"),
        ]
        .concat();
        let mut got = Vec::new();
        parse_records(&buf, |r| {
            got.push((r.record(), r.seq(), r.parent_record(), r.reason, r.name))
        });
        assert_eq!(
            got,
            vec![
                (100, 2, 5, 0x100, "a.txt".into()),
                (101, 0, 100, 0x200, "ünï".into())
            ]
        );
    }
}
