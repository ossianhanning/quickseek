//! Parsing of raw NTFS FILE records. Pure functions over byte slices, so they
//! can be unit-tested without a volume.
//!
//! Layout references: the NTFS documentation from the Linux-NTFS project, and
//! the parsers in `ntfs-reader` (MIT/Apache-2.0) and Indexer++ (MIT).

pub const ATTR_STANDARD_INFORMATION: u32 = 0x10;
pub const ATTR_ATTRIBUTE_LIST: u32 = 0x20;
pub const ATTR_FILE_NAME: u32 = 0x30;
pub const ATTR_DATA: u32 = 0x80;
pub const ATTR_BITMAP: u32 = 0xB0;
pub const ATTR_END: u32 = 0xFFFF_FFFF;

pub const REC_IN_USE: u16 = 0x1;
pub const REC_DIRECTORY: u16 = 0x2;

/// NTFS applies the update-sequence fixup every 512 bytes regardless of the
/// physical sector size.
const FIXUP_STRIDE: usize = 512;

pub const NS_POSIX: u8 = 0;
pub const NS_WIN32: u8 = 1;
pub const NS_DOS: u8 = 2;
pub const NS_WIN32_AND_DOS: u8 = 3;

pub const FRN_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

#[inline]
pub fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
#[inline]
pub fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
#[inline]
pub fn u64le(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// Applies the update sequence array in place. Returns false if the record is
/// torn or not a FILE record.
pub fn apply_fixup(rec: &mut [u8]) -> bool {
    if rec.len() < 48 || &rec[0..4] != b"FILE" {
        return false;
    }
    let usa_off = u16le(rec, 4) as usize;
    let usa_count = u16le(rec, 6) as usize;
    if usa_count == 0 || usa_off + usa_count * 2 > rec.len() {
        return false;
    }
    let check = [rec[usa_off], rec[usa_off + 1]];
    for i in 1..usa_count {
        let end = i * FIXUP_STRIDE;
        if end > rec.len() {
            break;
        }
        if rec[end - 2..end] != check {
            return false;
        }
        let fix = [rec[usa_off + i * 2], rec[usa_off + i * 2 + 1]];
        rec[end - 2..end].copy_from_slice(&fix);
    }
    true
}

pub struct RecordHeader {
    pub flags: u16,
    pub attrs_offset: usize,
    pub used_size: usize,
    pub base_ref: u64,
}

pub fn header(rec: &[u8]) -> RecordHeader {
    RecordHeader {
        flags: u16le(rec, 22),
        attrs_offset: u16le(rec, 20) as usize,
        used_size: (u32le(rec, 24) as usize).min(rec.len()),
        base_ref: u64le(rec, 32),
    }
}

/// One attribute inside a FILE record.
pub struct Attr<'a> {
    pub ty: u32,
    pub raw: &'a [u8],
}

impl<'a> Attr<'a> {
    pub fn non_resident(&self) -> bool {
        self.raw[8] != 0
    }
    pub fn name_len(&self) -> usize {
        self.raw[9] as usize
    }
    /// Value of a resident attribute.
    pub fn value(&self) -> Option<&'a [u8]> {
        if self.non_resident() || self.raw.len() < 24 {
            return None;
        }
        let len = u32le(self.raw, 16) as usize;
        let off = u16le(self.raw, 20) as usize;
        self.raw.get(off..off.checked_add(len)?)
    }
    pub fn lowest_vcn(&self) -> u64 {
        if self.non_resident() {
            u64le(self.raw, 16)
        } else {
            0
        }
    }
    /// Logical size of the stream (non-resident: data_size; resident: value length).
    pub fn data_size(&self) -> u64 {
        if self.non_resident() {
            if self.raw.len() >= 56 {
                u64le(self.raw, 48)
            } else {
                0
            }
        } else {
            u32le(self.raw, 16) as u64
        }
    }
    /// Decoded run list of a non-resident attribute: (vcn, lcn or None for sparse, clusters).
    pub fn runs(&self) -> Vec<Run> {
        let mut out = Vec::new();
        if !self.non_resident() || self.raw.len() < 64 {
            return out;
        }
        let mut p = u16le(self.raw, 32) as usize;
        let mut vcn = self.lowest_vcn();
        let mut lcn: i64 = 0;
        while p < self.raw.len() {
            let hdr = self.raw[p];
            if hdr == 0 {
                break;
            }
            let len_sz = (hdr & 0x0F) as usize;
            let off_sz = (hdr >> 4) as usize;
            if len_sz == 0 || len_sz > 8 || off_sz > 8 || p + 1 + len_sz + off_sz > self.raw.len() {
                break;
            }
            let length = read_var(&self.raw[p + 1..p + 1 + len_sz], false) as u64;
            let sparse = off_sz == 0;
            if !sparse {
                // Run offsets are signed and relative to the previous run's LCN.
                lcn += read_var(&self.raw[p + 1 + len_sz..p + 1 + len_sz + off_sz], true);
            }
            out.push(Run {
                vcn,
                lcn: if sparse { None } else { Some(lcn as u64) },
                clusters: length,
            });
            vcn += length;
            p += 1 + len_sz + off_sz;
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub vcn: u64,
    pub lcn: Option<u64>,
    pub clusters: u64,
}

fn read_var(b: &[u8], signed: bool) -> i64 {
    let mut v: i64 = 0;
    for (i, &x) in b.iter().enumerate() {
        v |= (x as i64) << (8 * i);
    }
    if signed && !b.is_empty() && b.len() < 8 && b[b.len() - 1] & 0x80 != 0 {
        v |= -1i64 << (8 * b.len());
    }
    v
}

/// Iterates the attributes of a fixed-up record.
pub fn attrs(rec: &[u8]) -> impl Iterator<Item = Attr<'_>> {
    let h = header(rec);
    let mut p = h.attrs_offset;
    let end = h.used_size;
    std::iter::from_fn(move || {
        if p + 16 > end {
            return None;
        }
        let ty = u32le(rec, p);
        if ty == ATTR_END {
            return None;
        }
        let len = u32le(rec, p + 4) as usize;
        if len < 16 || p + len > end {
            return None;
        }
        let a = Attr {
            ty,
            raw: &rec[p..p + len],
        };
        p += len;
        Some(a)
    })
}

/// Parsed $FILE_NAME attribute.
pub struct FileName<'a> {
    pub parent_ref: u64,
    pub namespace: u8,
    /// UTF-16LE bytes of the name.
    pub name_utf16: &'a [u8],
}

pub fn parse_file_name(v: &[u8]) -> Option<FileName<'_>> {
    if v.len() < 66 {
        return None;
    }
    let n = v[64] as usize;
    let name = v.get(66..66 + n * 2)?;
    Some(FileName {
        parent_ref: u64le(v, 0),
        namespace: v[65],
        name_utf16: name,
    })
}

/// Parsed $STANDARD_INFORMATION (times are FILETIME, 100ns since 1601).
#[derive(Default, Clone, Copy)]
pub struct StdInfo {
    pub created: i64,
    pub modified: i64,
    pub attributes: u32,
}

pub fn parse_std_info(v: &[u8]) -> Option<StdInfo> {
    if v.len() < 36 {
        return None;
    }
    Some(StdInfo {
        created: u64le(v, 0) as i64,
        modified: u64le(v, 8) as i64,
        attributes: u32le(v, 32),
    })
}

pub fn utf16le_to_string(b: &[u8]) -> String {
    let it = b.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c));
    char::decode_utf16(it)
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const REC: usize = 1024;

    /// Builds a minimal valid FILE record (fixups already "applied" in reverse
    /// so that `apply_fixup` restores the original bytes).
    pub struct RecBuilder {
        pub buf: Vec<u8>,
        pub off: usize,
    }

    impl RecBuilder {
        pub fn new(flags: u16, base_ref: u64) -> Self {
            let mut buf = vec![0u8; REC];
            buf[0..4].copy_from_slice(b"FILE");
            buf[4..6].copy_from_slice(&48u16.to_le_bytes()); // usa offset
            buf[6..8].copy_from_slice(&3u16.to_le_bytes()); // usn + 2 sectors
            buf[20..22].copy_from_slice(&56u16.to_le_bytes());
            buf[22..24].copy_from_slice(&flags.to_le_bytes());
            buf[28..32].copy_from_slice(&(REC as u32).to_le_bytes());
            buf[32..40].copy_from_slice(&base_ref.to_le_bytes());
            RecBuilder { buf, off: 56 }
        }

        pub fn resident(mut self, ty: u32, name_len: u8, value: &[u8]) -> Self {
            let len = (24 + value.len()).next_multiple_of(8);
            let o = self.off;
            self.buf[o..o + 4].copy_from_slice(&ty.to_le_bytes());
            self.buf[o + 4..o + 8].copy_from_slice(&(len as u32).to_le_bytes());
            self.buf[o + 9] = name_len;
            self.buf[o + 16..o + 20].copy_from_slice(&(value.len() as u32).to_le_bytes());
            self.buf[o + 20..o + 22].copy_from_slice(&24u16.to_le_bytes());
            self.buf[o + 24..o + 24 + value.len()].copy_from_slice(value);
            self.off += len;
            self
        }

        pub fn nonresident(mut self, ty: u32, size: u64, runs: &[u8]) -> Self {
            let len = (64 + runs.len() + 1).next_multiple_of(8);
            let o = self.off;
            self.buf[o..o + 4].copy_from_slice(&ty.to_le_bytes());
            self.buf[o + 4..o + 8].copy_from_slice(&(len as u32).to_le_bytes());
            self.buf[o + 8] = 1;
            self.buf[o + 32..o + 34].copy_from_slice(&64u16.to_le_bytes());
            self.buf[o + 48..o + 56].copy_from_slice(&size.to_le_bytes());
            self.buf[o + 64..o + 64 + runs.len()].copy_from_slice(runs);
            self.off += len;
            self
        }

        pub fn file_name(self, parent: u64, ns: u8, name: &str) -> Self {
            let u: Vec<u16> = name.encode_utf16().collect();
            let mut v = vec![0u8; 66 + u.len() * 2];
            v[0..8].copy_from_slice(&parent.to_le_bytes());
            v[64] = u.len() as u8;
            v[65] = ns;
            for (i, c) in u.iter().enumerate() {
                v[66 + i * 2..68 + i * 2].copy_from_slice(&c.to_le_bytes());
            }
            self.resident(ATTR_FILE_NAME, 0, &v)
        }

        pub fn std_info(self, created: i64, modified: i64, attributes: u32) -> Self {
            let mut v = vec![0u8; 72];
            v[0..8].copy_from_slice(&created.to_le_bytes());
            v[8..16].copy_from_slice(&modified.to_le_bytes());
            v[32..36].copy_from_slice(&attributes.to_le_bytes());
            self.resident(ATTR_STANDARD_INFORMATION, 0, &v)
        }

        pub fn finish(mut self) -> Vec<u8> {
            let o = self.off;
            self.buf[o..o + 4].copy_from_slice(&ATTR_END.to_le_bytes());
            let used = (o + 8) as u32;
            self.buf[24..28].copy_from_slice(&used.to_le_bytes());
            // Move the real sector tails into the USA and stamp the USN.
            let usn = [0x34u8, 0x12];
            self.buf[48..50].copy_from_slice(&usn);
            for i in 1..3 {
                let end = i * 512;
                let tail = [self.buf[end - 2], self.buf[end - 1]];
                self.buf[48 + i * 2..50 + i * 2].copy_from_slice(&tail);
                self.buf[end - 2..end].copy_from_slice(&usn);
            }
            self.buf
        }
    }

    #[test]
    fn fixup_restores_tails_and_rejects_torn() {
        let mut r = RecBuilder::new(REC_IN_USE, 0)
            .file_name(5, NS_WIN32, "hello.txt")
            .finish();
        let mut torn = r.clone();
        assert!(apply_fixup(&mut r));
        torn[1023] ^= 0xFF;
        assert!(!apply_fixup(&mut torn));
    }

    #[test]
    fn parses_names_and_std_info() {
        let mut r = RecBuilder::new(REC_IN_USE, 0)
            .std_info(1, 2, 0x20)
            .file_name(5, NS_DOS, "HELLO~1.TXT")
            .file_name(5, NS_WIN32, "hello world.txt")
            .finish();
        assert!(apply_fixup(&mut r));
        let names: Vec<_> = attrs(&r)
            .filter(|a| a.ty == ATTR_FILE_NAME)
            .map(|a| parse_file_name(a.value().unwrap()).unwrap())
            .map(|f| (f.namespace, utf16le_to_string(f.name_utf16)))
            .collect();
        assert_eq!(
            names,
            vec![
                (NS_DOS, "HELLO~1.TXT".into()),
                (NS_WIN32, "hello world.txt".into())
            ]
        );
        let si = attrs(&r)
            .find(|a| a.ty == ATTR_STANDARD_INFORMATION)
            .and_then(|a| parse_std_info(a.value().unwrap()))
            .unwrap();
        assert_eq!((si.created, si.modified, si.attributes), (1, 2, 0x20));
    }

    #[test]
    fn decodes_signed_relative_runs() {
        // run 1: 0x10 clusters at LCN 0x1000; run 2: 8 clusters at -0x800 relative; sparse 4.
        let runs = [
            0x21, 0x10, 0x00, 0x10, 0x21, 0x08, 0x00, 0xF8, 0x01, 0x04, 0x00,
        ];
        let mut r = RecBuilder::new(REC_IN_USE, 0)
            .nonresident(ATTR_DATA, 12345, &runs)
            .finish();
        assert!(apply_fixup(&mut r));
        let a = attrs(&r).find(|a| a.ty == ATTR_DATA).unwrap();
        assert_eq!(a.data_size(), 12345);
        assert_eq!(
            a.runs(),
            vec![
                Run {
                    vcn: 0,
                    lcn: Some(0x1000),
                    clusters: 0x10
                },
                Run {
                    vcn: 0x10,
                    lcn: Some(0x800),
                    clusters: 8
                },
                Run {
                    vcn: 0x18,
                    lcn: None,
                    clusters: 4
                },
            ]
        );
    }
}
