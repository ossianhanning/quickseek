//! NTFS access: raw $MFT scan (fast path), FSCTL_ENUM_USN_DATA (fallback),
//! and the USN change journal (live updates).

pub mod journal;
pub mod mft;
pub mod record;
pub mod usn;
pub mod win;

/// Per-record metadata gathered during a scan. Indexed by MFT record number.
#[derive(Clone, Copy, Default)]
pub struct Meta {
    pub in_use: bool,
    pub is_dir: bool,
    pub seq: u16,
    pub attrs: u32,
    pub size: u64,
    pub created: i64,
    pub modified: i64,
}

/// One (parent, name) link. Files with hard links have several.
#[derive(Clone, Copy)]
pub struct ScanName {
    pub record: u32,
    pub parent: u32,
    pub off: u32,
    pub len: u32,
}

/// Result of enumerating a volume, before it becomes an [`crate::index::Index`].
#[derive(Default)]
pub struct Scan {
    pub meta: Vec<Meta>,
    pub names: Vec<ScanName>,
    /// UTF-8 name bytes referenced by `names`.
    pub strings: Vec<u8>,
    /// Whether sizes and timestamps are known (raw MFT) or not (USN enum fallback).
    pub has_sizes: bool,
}

impl Scan {
    pub fn name(&self, n: &ScanName) -> &str {
        // Names are only ever written from `String`s.
        unsafe { std::str::from_utf8_unchecked(&self.strings[n.off as usize..(n.off + n.len) as usize]) }
    }
}

/// Record numbers below this are NTFS metafiles ($MFT, $LogFile, ...), except the root.
pub const FIRST_USER_RECORD: u32 = 24;
pub const ROOT_RECORD: u32 = 5;
/// $Extend directory; its children ($UsnJrnl, $Quota, ...) are metafiles too.
pub const EXTEND_RECORD: u32 = 11;

pub fn is_hidden_metafile(record: u32, parent: u32) -> bool {
    (record < FIRST_USER_RECORD && record != ROOT_RECORD) || parent == EXTEND_RECORD
}
