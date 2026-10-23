//! Applying USN journal records to an [`Index`].
//!
//! Records carry the file reference, parent reference, name and attributes,
//! which together with `Index::links` is enough to update the index without
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
    /// (record, file id) of files whose hard links changed: NTFS reports
    /// adding and removing a link alike, so the caller asks the file system
    /// which links exist ([`link_names_by_id`]) and drops the rest.
    pub link_checks: Vec<(u32, u128)>,
}

impl Applier {
    /// Applies `recs`; returns (record, file id) of files whose size/times
    /// should be refreshed.
    pub fn apply(&mut self, ix: &mut Index, recs: &[UsnRecord]) -> Vec<(u32, u128)> {
        let mut refresh: Vec<(u32, u128)> = Vec::new();
        let retry = std::mem::take(&mut self.orphans);
        for r in recs {
            self.apply_one(ix, r, &mut refresh);
        }
        // Records that arrived before their parent get one retry after the next batch.
        for r in retry.iter().filter(|o| !deleted_since(o, recs)) {
            self.apply_one(ix, r, &mut refresh);
        }
        self.orphans
            .retain(|o| !retry.iter().any(|x| x.usn == o.usn));
        refresh.sort_unstable();
        refresh.dedup();
        refresh
    }

    fn apply_one(&mut self, ix: &mut Index, r: &UsnRecord, refresh: &mut Vec<(u32, u128)>) {
        let reason = r.reason;
        let deleting = reason & USN_REASON_FILE_DELETE != 0;
        // NTFS: the record number is in the id. ReFS (128-bit ids): map to a
        // dense record, allocating one for files we haven't seen.
        let (rec, prec) = if ix.id_map.is_some() {
            let Some(rec) = ix.map_id(r.frn128, !deleting) else {
                return;
            };
            (rec, ix.map_id(r.parent128, false).unwrap_or(u32::MAX))
        } else {
            (r.record(), r.parent_record())
        };
        if ntfs::is_hidden_metafile(rec, prec) || rec == ntfs::ROOT_RECORD {
            return;
        }
        self.applied += 1;

        if deleting {
            ix.remove_record(rec);
            self.pending_rename.remove(&rec);
            return;
        }

        let parent = ix.dir_entry(prec).unwrap_or(NONE);

        if reason & USN_REASON_RENAME_OLD_NAME != 0 {
            let link = ix
                .find_link(rec, parent, &r.name)
                .or_else(|| ix.links(rec).next());
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
            && ix.seq(first) != r.seq()
        {
            ix.remove_record(rec);
        }

        let is_dir = r.attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
        let before = ix.entries.len();
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
                    _ => ix.add_link(
                        rec,
                        r.seq(),
                        parent,
                        &r.name,
                        r.attrs,
                        0,
                        r.timestamp,
                        r.timestamp,
                    ),
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
                        _ => ix.add_link(
                            rec,
                            r.seq(),
                            parent,
                            &r.name,
                            r.attrs,
                            0,
                            r.timestamp,
                            r.timestamp,
                        ),
                    }
                }
            }
        };

        let e = &mut ix.entries[entry as usize];
        e.set_attrs(r.attrs);
        if reason & DATA_CHANGE != 0 {
            e.mtime = e.mtime.max(crate::index::mtime_of(r.timestamp));
        }
        if !is_dir && reason & USN_REASON_HARD_LINK_CHANGE != 0 {
            self.link_checks.push((rec, r.frn128));
        }
        // An entry added just now (a new hard link, a missed create) starts
        // with the record's time and no size: fetch the file's own, or a
        // hard link made today shows an old file as modified today.
        if !is_dir && (reason & DATA_CHANGE != 0 || entry as usize >= before) {
            refresh.push((rec, r.frn128));
        }
        ix.note_recent(entry, r.timestamp);
        ix.generation += 1;
    }
}

fn deleted_since(o: &UsnRecord, recs: &[UsnRecord]) -> bool {
    recs.iter()
        .any(|r| r.frn128 == o.frn128 && r.reason & USN_REASON_FILE_DELETE != 0)
}

/// Opens a file by id (64-bit NTFS or 128-bit ReFS id) to read attributes.
fn open_by_id(hint: &Handle, id: u128, extended: bool) -> Option<Handle> {
    use windows_sys::Win32::Storage::FileSystem::{
        ExtendedFileIdType, FILE_ID_128, FILE_ID_DESCRIPTOR_0,
    };
    let desc = FILE_ID_DESCRIPTOR {
        dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
        Type: if extended {
            ExtendedFileIdType
        } else {
            FileIdType
        },
        Anonymous: if extended {
            FILE_ID_DESCRIPTOR_0 {
                ExtendedFileId: FILE_ID_128 {
                    Identifier: id.to_le_bytes(),
                },
            }
        } else {
            FILE_ID_DESCRIPTOR_0 { FileId: id as i64 }
        },
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
    (h != INVALID_HANDLE_VALUE).then_some(Handle(h))
}

/// Every name (hard link) a file has now, as paths below the volume root
/// (`\dir\name`, lower case), or None if it can't be opened.
pub fn link_names_by_id(
    hint: &Handle,
    id: u128,
    extended: bool,
    root: &str,
) -> Option<Vec<String>> {
    use windows_sys::Win32::Storage::FileSystem::{
        FindClose, FindFirstFileNameW, FindNextFileNameW, GetFinalPathNameByHandleW,
        VOLUME_NAME_NONE,
    };
    let h = open_by_id(hint, id, extended)?;
    let mut buf = vec![0u16; 1024];
    let n = unsafe {
        GetFinalPathNameByHandleW(h.0, buf.as_mut_ptr(), buf.len() as u32, VOLUME_NAME_NONE)
    } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    drop(h);
    let first: Vec<u16> = format!("{}\\", root.trim_end_matches('\\'))
        .encode_utf16()
        .chain(buf[..n].iter().copied().skip_while(|&c| c == '\\' as u16))
        .chain([0])
        .collect();
    let mut names = Vec::new();
    let mut len = buf.len() as u32;
    let find = unsafe { FindFirstFileNameW(first.as_ptr(), 0, &mut len, buf.as_mut_ptr()) };
    if find == INVALID_HANDLE_VALUE {
        return None;
    }
    loop {
        let s = String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)]);
        names.push(s.to_lowercase());
        len = buf.len() as u32;
        if unsafe { FindNextFileNameW(find, &mut len, buf.as_mut_ptr()) } == 0 {
            break;
        }
    }
    unsafe { FindClose(find) };
    Some(names)
}

/// Size and timestamps of a file by id (64-bit NTFS or 128-bit ReFS id).
pub fn file_info_by_id(hint: &Handle, id: u128, extended: bool) -> Option<(u64, i64, i64)> {
    let h = open_by_id(hint, id, extended)?;
    let mut std_info: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
    let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
    unsafe {
        if GetFileInformationByHandleEx(
            h.0,
            FileStandardInfo,
            &mut std_info as *mut _ as *mut _,
            size_of::<FILE_STANDARD_INFO>() as u32,
        ) == 0
        {
            return None;
        }
        if GetFileInformationByHandleEx(
            h.0,
            FileBasicInfo,
            &mut basic as *mut _ as *mut _,
            size_of::<FILE_BASIC_INFO>() as u32,
        ) == 0
        {
            return None;
        }
    }
    Some((
        std_info.EndOfFile as u64,
        basic.LastWriteTime,
        basic.CreationTime,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::sample;
    use crate::query::Query;
    use crate::search::{SearchOptions, search};

    fn rec(frn: u64, parent: u64, reason: u32, attrs: u32, name: &str, usn: i64) -> UsnRecord {
        UsnRecord {
            frn,
            parent,
            frn128: frn as u128,
            parent128: parent as u128,
            usn,
            timestamp: 1,
            reason,
            attrs,
            name: name.into(),
        }
    }

    fn find(ix: &Index, q: &str) -> Vec<String> {
        let h = search(
            &[ix],
            &Query::parse(q).unwrap(),
            &SearchOptions {
                limit: 0,
                ..Default::default()
            },
        );
        h.items.iter().map(|h| ix.path(h.idx)).collect()
    }

    #[test]
    fn create_rename_move_delete() {
        let mut ix = sample();
        let mut a = Applier::default();
        // Create dir 40 under Users(30), then file 41 inside it.
        a.apply(
            &mut ix,
            &[
                rec(40, 30, USN_REASON_FILE_CREATE, 0x10, "Proj", 1),
                rec(41, 40, USN_REASON_FILE_CREATE, 0x20, "main.rs", 2),
                rec(
                    41,
                    40,
                    USN_REASON_FILE_CREATE | USN_REASON_DATA_EXTEND | USN_REASON_CLOSE,
                    0x20,
                    "main.rs",
                    3,
                ),
            ],
        );
        assert_eq!(find(&ix, "main.rs"), vec![r"C:\Users\Proj\main.rs"]);
        assert_eq!(ix.links(41).count(), 1, "repeat records must not duplicate");

        // Rename the directory across batches: children follow.
        a.apply(
            &mut ix,
            &[rec(40, 30, USN_REASON_RENAME_OLD_NAME, 0x10, "Proj", 4)],
        );
        a.apply(
            &mut ix,
            &[rec(40, 34, USN_REASON_RENAME_NEW_NAME, 0x10, "Code", 5)],
        );
        assert_eq!(find(&ix, "main.rs"), vec![r"C:\Windows\Code\main.rs"]);
        assert!(find(&ix, "proj").is_empty());

        a.apply(
            &mut ix,
            &[rec(
                41,
                40,
                USN_REASON_FILE_DELETE | USN_REASON_CLOSE,
                0x20,
                "main.rs",
                6,
            )],
        );
        assert!(find(&ix, "main.rs").is_empty());
    }

    /// Journal changes (create, rename, delete) on an index big enough for
    /// the fast paths (name order walk, `ext:` arena scan, packed dates):
    /// `ext:`, `dm:` and name-sorted results match a brute-force answer.
    #[test]
    fn journal_changes_keep_fast_paths_right() {
        use crate::index::tests::scan_of;
        use crate::search::SortKey;
        let mut items: Vec<(u32, u32, String, bool, u64)> = vec![(30, 5, "Dir".into(), true, 0)];
        for k in 0..12_000u32 {
            let ext = ["dll", "txt", "DLL", "log"][(k % 4) as usize];
            items.push((
                40 + k,
                30,
                format!("{}File{k}.{ext}", ["a", "B", "c"][(k % 3) as usize]),
                false,
                10,
            ));
        }
        let refs: Vec<(u32, u32, &str, bool, u64)> = items
            .iter()
            .map(|i| (i.0, i.1, i.2.as_str(), i.3, i.4))
            .collect();
        let mut ix = Index::from_scan('C', &scan_of(&refs));
        let mut a = Applier::default();
        // 2026-10-20 12:00 UTC as a FILETIME.
        let day = 134_369_712_000_000_000i64;
        let mut recs = Vec::new();
        let mut usn = 10;
        let mut r = |frn: u64, parent: u64, reason: u32, name: &str, ts: i64| {
            usn += 1;
            UsnRecord {
                timestamp: ts,
                ..rec(frn, parent, reason, 0x20, name, usn)
            }
        };
        for k in 0..300u64 {
            // New files, half of them .dll, all dated that day.
            let name = format!("new{k}.{}", if k % 2 == 0 { "dll" } else { "dat" });
            recs.push(r(
                50_000 + k,
                30,
                USN_REASON_FILE_CREATE | USN_REASON_CLOSE,
                &name,
                day,
            ));
        }
        for k in (1..12_000u64).step_by(37) {
            // Renames: .txt files become .dll and the other way round.
            let old = items[k as usize + 1].2.clone();
            let new = if old.ends_with(".txt") {
                old.replace(".txt", ".dll")
            } else {
                old.replace(".dll", ".txt")
            };
            recs.push(r(40 + k, 30, USN_REASON_RENAME_OLD_NAME, &old, day));
            recs.push(r(
                40 + k,
                30,
                USN_REASON_RENAME_NEW_NAME | USN_REASON_CLOSE,
                &new,
                day,
            ));
        }
        for k in (5..12_000u64).step_by(53) {
            recs.push(r(
                40 + k,
                30,
                USN_REASON_FILE_DELETE | USN_REASON_CLOSE,
                "x",
                day,
            ));
        }
        a.apply(&mut ix, &recs);
        assert!(
            !ix.ranks.unranked_names.is_empty(),
            "changes since the ranks were computed"
        );

        let reference = |keep: &dyn Fn(u32) -> bool, desc: bool| {
            let mut v: Vec<u32> = (0..ix.entries.len() as u32)
                .filter(|&i| ix.is_visible(i) && keep(i))
                .collect();
            v.sort_by(|&x, &y| {
                ix.fold_name(x)
                    .cmp(ix.fold_name(y))
                    .then_with(|| ix.name(x).cmp(&ix.name(y)))
                    .then(x.cmp(&y))
            });
            if desc {
                v.reverse();
            }
            v
        };
        let is_dll =
            |i: u32| !ix.entries[i as usize].is_dir() && ix.fold_name(i).ends_with(b".dll");
        let on_day =
            |i: u32| ix.entries[i as usize].modified() / 864_000_000_000 == day / 864_000_000_000;
        let all = |_: u32| true;
        for (q, keep) in [
            ("ext:dll", &is_dll as &dyn Fn(u32) -> bool),
            ("dm:2026-10-20", &on_day),
            ("", &all),
        ] {
            for desc in [false, true] {
                for limit in [100, 0] {
                    let o = SearchOptions {
                        sort: SortKey::Name,
                        descending: desc,
                        limit,
                    };
                    let h = search(&[&ix], &Query::parse(q).unwrap(), &o);
                    let want = reference(keep, desc);
                    assert_eq!(h.total, want.len(), "{q}");
                    let got: Vec<u32> = h.items.iter().map(|h| h.idx).collect();
                    let n = if limit == 0 {
                        want.len()
                    } else {
                        limit.min(want.len())
                    };
                    assert_eq!(got[..n], want[..n], "{q} desc={desc} limit={limit}");
                }
            }
        }
        // The new files are there.
        assert!(find(&ix, "new0.dll").len() == 1 && find(&ix, "new1.dat").len() == 1);
        // Renames keep the date modified, as on NTFS: just the new files.
        assert_eq!(reference(&on_day, false).len(), 300);
    }

    #[test]
    fn child_before_parent_is_retried() {
        let mut ix = sample();
        let mut a = Applier::default();
        a.apply(
            &mut ix,
            &[rec(61, 60, USN_REASON_FILE_CREATE, 0x20, "late.txt", 1)],
        );
        assert!(find(&ix, "late").is_empty());
        a.apply(
            &mut ix,
            &[rec(60, 5, USN_REASON_FILE_CREATE, 0x10, "dir", 2)],
        );
        assert_eq!(find(&ix, "late"), vec![r"C:\dir\late.txt"]);
    }

    #[test]
    fn records_recent_changes() {
        let mut ix = sample();
        let mut a = Applier::default();
        a.apply(
            &mut ix,
            &[rec(80, 5, USN_REASON_FILE_CREATE, 0x20, "fresh.log", 1)],
        );
        let i = crate::index::tests::by_name(&ix, "fresh.log");
        assert_eq!(ix.recent.get(&i), Some(&1));
    }

    /// ReFS: 128-bit ids are mapped to dense records; the root id is
    /// registered when the volume is scanned.
    #[test]
    fn refs_ids_are_mapped() {
        let mut ix = sample();
        let root_id: u128 = 0x1111_0000_0000_0000_0000_0000_0000_0000;
        ix.id_map = Some(HashMap::from([(root_id, ntfs::ROOT_RECORD)]));
        let mut a = Applier::default();
        let dir: u128 = 0x2222_0000_0000_0000_0000_0000_0000_0007;
        let file: u128 = 0x3333_0000_0000_0000_0000_0000_0000_0009;
        let mk = |f: u128, p: u128, reason: u32, attrs: u32, name: &str, usn: i64| UsnRecord {
            frn128: f,
            parent128: p,
            frn: f as u64,
            parent: p as u64,
            usn,
            timestamp: 2,
            reason,
            attrs,
            name: name.into(),
        };
        let refresh = a.apply(
            &mut ix,
            &[
                mk(dir, root_id, USN_REASON_FILE_CREATE, 0x10, "devdrive", 1),
                mk(
                    file,
                    dir,
                    USN_REASON_FILE_CREATE | USN_REASON_DATA_EXTEND,
                    0x20,
                    "main.rs",
                    2,
                ),
            ],
        );
        assert_eq!(
            find(&ix, "main.rs"),
            vec![r"C:\devdrive\main.rs".to_string()]
        );
        assert_eq!(refresh.len(), 1);
        assert_eq!(refresh[0].1, file);
        a.apply(
            &mut ix,
            &[mk(file, dir, USN_REASON_FILE_DELETE, 0x20, "main.rs", 3)],
        );
        assert!(find(&ix, "main.rs").is_empty());
    }

    /// Removed hard links: the index keeps only the links the file system
    /// reports, and the lookup really lists a file's links.
    #[test]
    fn removed_hard_links_are_dropped() {
        let mut ix = sample();
        let notes = crate::index::tests::by_name(&ix, "notes.txt");
        let rec_no = ix.record(notes);
        let win = crate::index::tests::by_name(&ix, "Windows");
        ix.add_link(rec_no, 0, win, "second.txt", 0x20, 24, 0, 0);
        assert_eq!(ix.links(rec_no).count(), 2);
        assert_eq!(
            ix.drop_missing_links(rec_no, &[]),
            0,
            "no answer: keep everything"
        );
        let dropped = ix.drop_missing_links(rec_no, &[r"\windows\second.txt".to_string()]);
        assert_eq!(dropped, 1);
        assert_eq!(find(&ix, "second.txt"), vec![r"C:\Windows\second.txt"]);
        assert!(find(&ix, "notes.txt").is_empty());

        // The real lookup, on a temp folder of this machine.
        let dir = std::env::temp_dir().join(format!("quickseek-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The file system reports long names; TEMP can hold an 8.3 short name
        // (C:\Users\RUNNER~1 on GitHub's runners), so compare the real path.
        let dir = std::fs::canonicalize(&dir).unwrap();
        let dir = std::path::PathBuf::from(
            dir.to_string_lossy()
                .trim_start_matches(r"\\?\")
                .to_string(),
        );
        let a = dir.join("a.o");
        std::fs::write(&a, b"x").unwrap();
        std::fs::hard_link(&a, dir.join("b.o")).unwrap();
        let letter = dir.to_string_lossy().chars().next().unwrap();
        let id = {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Storage::FileSystem::{
                BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
            };
            let f = std::fs::File::open(&a).unwrap();
            let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
            assert!(unsafe { GetFileInformationByHandle(f.as_raw_handle(), &mut info) } != 0);
            (info.nFileIndexHigh as u128) << 32 | info.nFileIndexLow as u128
        };
        let hint = crate::ntfs::win::open_root_dir(letter).unwrap();
        let root = format!("{letter}:");
        let names = link_names_by_id(&hint, id, false, &root).unwrap();
        let rel = |p: &std::path::Path| p.to_string_lossy()[2..].to_lowercase();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(
            names.contains(&rel(&a)) && names.contains(&rel(&dir.join("b.o"))),
            "{names:?}"
        );
        std::fs::remove_file(dir.join("b.o")).unwrap();
        assert_eq!(
            link_names_by_id(&hint, id, false, &root).unwrap(),
            vec![rel(&a)]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A new hard link to an old file is a new entry: its size and dates
    /// are fetched from the file, not taken from the record's time.
    #[test]
    fn new_hard_link_is_refreshed() {
        let mut ix = sample();
        let mut a = Applier::default();
        let notes = crate::index::tests::by_name(&ix, "notes.txt");
        let rec_no = ix.record(notes) as u64;
        let refresh = a.apply(
            &mut ix,
            &[rec(
                rec_no,
                34,
                USN_REASON_HARD_LINK_CHANGE | USN_REASON_CLOSE,
                0x20,
                "notes-link.txt",
                9,
            )],
        );
        assert_eq!(
            find(&ix, "notes-link.txt"),
            vec![r"C:\Windows\notes-link.txt"]
        );
        assert_eq!(
            refresh.iter().map(|r| r.0 as u64).collect::<Vec<_>>(),
            vec![rec_no]
        );
        // A plain attribute change on a known file fetches nothing.
        let refresh = a.apply(
            &mut ix,
            &[rec(
                rec_no,
                34,
                USN_REASON_SECURITY_CHANGE | USN_REASON_CLOSE,
                0x20,
                "notes-link.txt",
                10,
            )],
        );
        assert!(refresh.is_empty());
    }

    #[test]
    fn reused_record_with_new_sequence_replaces_old_file() {
        let mut ix = sample();
        let mut a = Applier::default();
        a.apply(
            &mut ix,
            &[rec(
                (7 << 48) | 33,
                31,
                USN_REASON_FILE_CREATE,
                0x20,
                "fresh.txt",
                1,
            )],
        );
        assert!(find(&ix, "notes").is_empty());
        assert_eq!(find(&ix, "fresh"), vec![r"C:\Users\Me\fresh.txt"]);
    }
}
