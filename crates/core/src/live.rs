//! Applying USN journal records to an [`Index`].
//!
//! Records carry the file reference, parent reference, name and attributes,
//! which together with `Index::rec_map` is enough to update the index without
//! touching the disk. The only thing a journal record lacks is the new file
//! size; files whose data changed are collected and refreshed in a batch with
//! `OpenFileById` + `GetFileInformationByHandleEx` (outside the index lock).

use std::collections::HashMap;
use std::ptr::null;

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_DESCRIPTOR, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileBasicInfo,
    FileIdType, FileStandardInfo, GetFileInformationByHandleEx, OpenFileById,
};
use windows_sys::Win32::System::Ioctl::*;

use crate::index::{Index, NONE};
use crate::ntfs::usn::{FILE_ATTRIBUTE_DIRECTORY, UsnRecord};
use crate::ntfs::win::Handle;
use crate::ntfs::{self};

const DATA_CHANGE: u32 = USN_REASON_DATA_OVERWRITE
    | USN_REASON_DATA_EXTEND
    | USN_REASON_DATA_TRUNCATION
    | USN_REASON_BASIC_INFO_CHANGE
    | USN_REASON_FILE_CREATE;

/// State carried across batches (a rename's OLD_NAME and NEW_NAME records can
/// land in different reads).
#[derive(Default)]
pub struct Applier {
    pending_rename: HashMap<u32, u32>,
    /// Records whose parent directory wasn't known yet.
    orphans: Vec<UsnRecord>,
    pub applied: u64,
}

impl Applier {
    /// Applies `recs`; returns the record numbers whose size/times should be refreshed.
    pub fn apply(&mut self, ix: &mut Index, recs: &[UsnRecord]) -> Vec<(u32, u64)> {
        let mut refresh: Vec<(u32, u64)> = Vec::new();
        let retry = std::mem::take(&mut self.orphans);
        for r in recs {
            self.apply_one(ix, r, &mut refresh);
        }
        // Records that arrived before their parent get one retry after the next batch.
        for r in retry.iter().filter(|o| !deleted_since(o, recs)) {
            self.apply_one(ix, r, &mut refresh);
        }
        self.orphans.retain(|o| !retry.iter().any(|x| x.usn == o.usn));
        refresh.sort_unstable();
        refresh.dedup();
        refresh
    }

    fn apply_one(&mut self, ix: &mut Index, r: &UsnRecord, refresh: &mut Vec<(u32, u64)>) {
        let rec = r.record();
        let prec = r.parent_record();
        if ntfs::is_hidden_metafile(rec, prec) || rec == ntfs::ROOT_RECORD {
            return;
        }
        self.applied += 1;
        let reason = r.reason;

        if reason & USN_REASON_FILE_DELETE != 0 {
            ix.remove_record(rec);
            self.pending_rename.remove(&rec);
            return;
        }

        let parent = ix.dir_entry(prec).unwrap_or(NONE);

        if reason & USN_REASON_RENAME_OLD_NAME != 0 {
            let link = ix.find_link(rec, parent, &r.name).or_else(|| ix.links(rec).next());
            if let Some(l) = link {
                self.pending_rename.insert(rec, l);
            }
            return;
        }

        if parent == NONE {
            // Parent created by a record we haven't seen yet (or outside our view);
            // retry with the next batch.
            if !self.orphans.iter().any(|o| o.usn == r.usn) {
                self.orphans.push(r.clone());
            }
            return;
        }

        // A recycled MFT record (new sequence number) is a different file.
        let links: Vec<u32> = ix.links(rec).collect();
        if reason & USN_REASON_FILE_CREATE != 0
            && let Some(&first) = links.first()
            && ix.entries[first as usize].seq != r.seq()
        {
            ix.remove_record(rec);
        }

        let is_dir = r.attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
        let entry = if reason & USN_REASON_RENAME_NEW_NAME != 0 {
            match self.pending_rename.remove(&rec).filter(|&l| ix.is_live(l)) {
                Some(l) => {
                    ix.rename_entry(l, parent, &r.name);
                    l
                }
                None => match ix.links(rec).collect::<Vec<_>>()[..] {
                    [l] => {
                        ix.rename_entry(l, parent, &r.name);
                        l
                    }
                    _ => ix.add_link(rec, r.seq(), parent, &r.name, r.attrs, 0, r.timestamp, r.timestamp),
                },
            }
        } else {
            match ix.find_link(rec, parent, &r.name) {
                Some(l) => l,
                None => {
                    match ix.links(rec).collect::<Vec<_>>()[..] {
                        // Missed rename: fix up the only link instead of duplicating.
                        [l] if reason & USN_REASON_HARD_LINK_CHANGE == 0 => {
                            ix.rename_entry(l, parent, &r.name);
                            l
                        }
                        _ => ix.add_link(rec, r.seq(), parent, &r.name, r.attrs, 0, r.timestamp, r.timestamp),
                    }
                }
            }
        };

        let e = &mut ix.entries[entry as usize];
        e.attrs = r.attrs;
        if reason & DATA_CHANGE != 0 {
            e.modified = r.timestamp.max(e.modified);
            if !is_dir {
                refresh.push((rec, r.frn));
            }
        }
        ix.generation += 1;
    }
}

fn deleted_since(o: &UsnRecord, recs: &[UsnRecord]) -> bool {
    recs.iter().any(|r| r.record() == o.record() && r.reason & USN_REASON_FILE_DELETE != 0)
}

/// Size and timestamps of a file by id.
pub fn file_info_by_id(hint: &Handle, frn: u64) -> Option<(u64, i64, i64)> {
    let desc = FILE_ID_DESCRIPTOR {
        dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
        Type: FileIdType,
        Anonymous: windows_sys::Win32::Storage::FileSystem::FILE_ID_DESCRIPTOR_0 { FileId: frn as i64 },
    };
    let h: HANDLE = unsafe {
        OpenFileById(
            hint.0,
            &desc,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            FILE_FLAG_BACKUP_SEMANTICS,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    let h = Handle(h);
    let mut std_info: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
    let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
    unsafe {
        if GetFileInformationByHandleEx(h.0, FileStandardInfo, &mut std_info as *mut _ as *mut _, size_of::<FILE_STANDARD_INFO>() as u32) == 0 {
            return None;
        }
        if GetFileInformationByHandleEx(h.0, FileBasicInfo, &mut basic as *mut _ as *mut _, size_of::<FILE_BASIC_INFO>() as u32) == 0 {
            return None;
        }
    }
    Some((std_info.EndOfFile as u64, basic.LastWriteTime, basic.CreationTime))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::sample;
    use crate::query::Query;
    use crate::search::{SearchOptions, search};

    fn rec(frn: u64, parent: u64, reason: u32, attrs: u32, name: &str, usn: i64) -> UsnRecord {
        UsnRecord { frn, parent, usn, timestamp: 1, reason, attrs, name: name.into() }
    }

    fn find(ix: &Index, q: &str) -> Vec<String> {
        let h = search(&[ix], &Query::parse(q).unwrap(), &SearchOptions { limit: 0, ..Default::default() });
        h.items.iter().map(|h| ix.path(h.idx)).collect()
    }

    #[test]
    fn create_rename_move_delete() {
        let mut ix = sample();
        let mut a = Applier::default();
        // Create dir 40 under Users(30), then file 41 inside it.
        a.apply(&mut ix, &[
            rec(40, 30, USN_REASON_FILE_CREATE, 0x10, "Proj", 1),
            rec(41, 40, USN_REASON_FILE_CREATE, 0x20, "main.rs", 2),
            rec(41, 40, USN_REASON_FILE_CREATE | USN_REASON_DATA_EXTEND | USN_REASON_CLOSE, 0x20, "main.rs", 3),
        ]);
        assert_eq!(find(&ix, "main.rs"), vec![r"C:\Users\Proj\main.rs"]);
        assert_eq!(ix.links(41).count(), 1, "repeat records must not duplicate");

        // Rename the directory across batches: children follow.
        a.apply(&mut ix, &[rec(40, 30, USN_REASON_RENAME_OLD_NAME, 0x10, "Proj", 4)]);
        a.apply(&mut ix, &[rec(40, 34, USN_REASON_RENAME_NEW_NAME, 0x10, "Code", 5)]);
        assert_eq!(find(&ix, "main.rs"), vec![r"C:\Windows\Code\main.rs"]);
        assert!(find(&ix, "proj").is_empty());

        a.apply(&mut ix, &[rec(41, 40, USN_REASON_FILE_DELETE | USN_REASON_CLOSE, 0x20, "main.rs", 6)]);
        assert!(find(&ix, "main.rs").is_empty());
    }

    #[test]
    fn child_before_parent_is_retried() {
        let mut ix = sample();
        let mut a = Applier::default();
        a.apply(&mut ix, &[rec(61, 60, USN_REASON_FILE_CREATE, 0x20, "late.txt", 1)]);
        assert!(find(&ix, "late").is_empty());
        a.apply(&mut ix, &[rec(60, 5, USN_REASON_FILE_CREATE, 0x10, "dir", 2)]);
        assert_eq!(find(&ix, "late"), vec![r"C:\dir\late.txt"]);
    }

    #[test]
    fn reused_record_with_new_sequence_replaces_old_file() {
        let mut ix = sample();
        let mut a = Applier::default();
        a.apply(&mut ix, &[rec((7 << 48) | 33, 31, USN_REASON_FILE_CREATE, 0x20, "fresh.txt", 1)]);
        assert!(find(&ix, "notes").is_empty());
        assert_eq!(find(&ix, "fresh"), vec![r"C:\Users\Me\fresh.txt"]);
    }
}
