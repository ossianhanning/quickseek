//! Folder indexes: for volumes without an MFT/USN journal (FAT32, exFAT,
//! network shares) or any folder the user adds. Built by walking the tree
//! in parallel, kept current with `ReadDirectoryChangesW`.

use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::ptr::null;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
    FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
    FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesW,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResultEx, OVERLAPPED};
use windows_sys::Win32::System::Threading::CreateEventW;

use crate::fold::FoldMode;
use crate::index::Index;
use crate::ntfs::usn::FILE_ATTRIBUTE_DIRECTORY;
use crate::ntfs::win::{Handle, Token, wide};
use crate::ntfs::{self, Meta, Scan, ScanName};

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// Root path normalized for display: `G:\` → `G:`, `\\srv\share\` → `\\srv\share`.
pub fn root_name(path: &str) -> String {
    path.trim_end_matches(['\\', '/']).replace('/', "\\")
}

fn meta_of(m: &std::fs::Metadata) -> Meta {
    let attrs = m.file_attributes();
    Meta {
        in_use: true,
        is_dir: m.is_dir(),
        seq: 0,
        attrs,
        size: if m.is_dir() { 0 } else { m.file_size() },
        created: m.creation_time() as i64,
        modified: m.last_write_time() as i64,
    }
}

/// Walks `root` (in parallel) into a [`Scan`] with synthetic record numbers.
pub fn walk(root: &Path) -> Scan {
    let next = AtomicU32::new(ntfs::FIRST_USER_RECORD);
    let out: Mutex<Vec<(u32, u32, String, Meta)>> = Mutex::new(Vec::new());
    fn visit<'a>(
        dir: PathBuf,
        rec: u32,
        next: &'a AtomicU32,
        out: &'a Mutex<Vec<(u32, u32, String, Meta)>>,
        s: &rayon::Scope<'a>,
    ) {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut local = Vec::new();
        for ent in rd.flatten() {
            let Ok(m) = ent.metadata() else { continue };
            let r = next.fetch_add(1, Ordering::Relaxed);
            let meta = meta_of(&m);
            if meta.is_dir && meta.attrs & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                let p = ent.path();
                s.spawn(move |s| visit(p, r, next, out, s));
            }
            local.push((r, rec, ent.file_name().to_string_lossy().into_owned(), meta));
        }
        out.lock().unwrap().extend(local);
    }
    rayon::scope(|s| visit(root.to_path_buf(), ntfs::ROOT_RECORD, &next, &out, s));
    let items = out.into_inner().unwrap();
    let mut scan = Scan {
        has_sizes: true,
        ..Default::default()
    };
    scan.meta = vec![Meta::default(); next.load(Ordering::Relaxed) as usize];
    scan.meta[ntfs::ROOT_RECORD as usize] = Meta {
        in_use: true,
        is_dir: true,
        attrs: FILE_ATTRIBUTE_DIRECTORY,
        ..Default::default()
    };
    scan.names.push(ScanName {
        record: ntfs::ROOT_RECORD,
        parent: ntfs::ROOT_RECORD,
        off: 0,
        len: 1,
    });
    scan.strings.push(b'.');
    for (r, p, name, m) in items {
        scan.meta[r as usize] = m;
        scan.names.push(ScanName {
            record: r,
            parent: p,
            off: scan.strings.len() as u32,
            len: name.len() as u32,
        });
        scan.strings.extend_from_slice(name.as_bytes());
    }
    scan
}

pub fn build(root: &str, fold: FoldMode) -> Index {
    build_as(root, fold, None, crate::index::IndexOptions::default())
}

/// Walks with `token`'s identity (network shares reached by the service).
/// Every walker thread impersonates, since impersonation is per thread.
pub fn build_as(
    root: &str,
    fold: FoldMode,
    token: Option<Arc<Token>>,
    opts: crate::index::IndexOptions,
) -> Index {
    let letter = root.chars().next().unwrap_or('?').to_ascii_uppercase();
    let dir = PathBuf::from(format!("{}\\", root_name(root)));
    let scan = match token {
        Some(t) => {
            let t2 = t.clone();
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(rayon::current_num_threads().clamp(4, 32))
                .start_handler(move |_| {
                    t2.impersonate();
                })
                .build();
            match pool {
                Ok(p) => p.install(|| walk(&dir)),
                Err(_) => walk(&dir),
            }
        }
        None => walk(&dir),
    };
    let mut ix = Index::from_scan_with(letter, &root_name(root), &scan, fold, opts);
    ix.id_map = None;
    ix
}

/// An index holding only the root (before the first walk finishes).
pub fn empty(root: &str, fold: FoldMode) -> Index {
    let letter = root.chars().next().unwrap_or('?').to_ascii_uppercase();
    let mut scan = Scan {
        has_sizes: true,
        ..Default::default()
    };
    scan.meta = vec![Meta::default(); ntfs::FIRST_USER_RECORD as usize];
    scan.meta[ntfs::ROOT_RECORD as usize] = Meta {
        in_use: true,
        is_dir: true,
        attrs: FILE_ATTRIBUTE_DIRECTORY,
        ..Default::default()
    };
    scan.names.push(ScanName {
        record: ntfs::ROOT_RECORD,
        parent: ntfs::ROOT_RECORD,
        off: 0,
        len: 1,
    });
    scan.strings.push(b'.');
    Index::from_scan_named(letter, &root_name(root), &scan, fold)
}

/// Entry for a path relative to the root (`a\b\c`), if indexed.
pub fn resolve(ix: &Index, rel: &str) -> Option<u32> {
    let mut cur = ix.root;
    for comp in rel.split('\\').filter(|c| !c.is_empty()) {
        cur = ix.find_child(cur, comp)?;
    }
    Some(cur)
}

fn split_parent(rel: &str) -> (&str, &str) {
    match rel.rfind('\\') {
        Some(p) => (&rel[..p], &rel[p + 1..]),
        None => ("", rel),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Added,
    Removed,
    Modified,
    RenamedOld,
    RenamedNew,
}

impl Action {
    fn from_raw(a: u32) -> Option<Action> {
        Some(match a {
            1 => Action::Added,
            2 => Action::Removed,
            3 => Action::Modified,
            4 => Action::RenamedOld,
            5 => Action::RenamedNew,
            _ => return None,
        })
    }
}

/// Applies one change notification. `root` is the folder's real path.
/// Returns false when the index can't follow (caller should rescan).
pub fn apply_change(
    ix: &mut Index,
    root: &Path,
    action: Action,
    rel: &str,
    pending_old: &mut Option<u32>,
    now: i64,
) -> bool {
    let (parent_rel, name) = split_parent(rel);
    match action {
        Action::Added => {
            let Some(parent) = resolve(ix, parent_rel) else {
                return false;
            };
            let full = root.join(rel);
            let Ok(m) = std::fs::symlink_metadata(&full) else {
                return true;
            }; // already gone
            let meta = meta_of(&m);
            if let Some(existing) = ix.find_child(parent, name) {
                let rec = ix.record(existing);
                ix.set_file_info(rec, meta.size, meta.modified, meta.created);
                return true;
            }
            let idx = add(ix, parent, name, &meta);
            ix.note_recent(idx, now);
            if meta.is_dir && meta.attrs & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                add_tree(ix, idx, &full);
            }
            true
        }
        Action::Removed => {
            if let Some(i) = resolve(ix, rel) {
                ix.remove_subtree(i);
            }
            true
        }
        Action::Modified => {
            if let Some(i) = resolve(ix, rel)
                && let Ok(m) = std::fs::symlink_metadata(root.join(rel))
            {
                let meta = meta_of(&m);
                let rec = ix.record(i);
                ix.set_file_info(rec, meta.size, meta.modified, meta.created);
                ix.note_recent(i, now);
            }
            true
        }
        Action::RenamedOld => {
            *pending_old = resolve(ix, rel);
            true
        }
        Action::RenamedNew => {
            let Some(parent) = resolve(ix, parent_rel) else {
                return false;
            };
            match pending_old.take() {
                Some(old) if ix.is_live(old) => {
                    ix.rename_entry(old, parent, name);
                    ix.note_recent(old, now);
                    true
                }
                _ => apply_change(ix, root, Action::Added, rel, pending_old, now),
            }
        }
    }
}

fn add(ix: &mut Index, parent: u32, name: &str, m: &Meta) -> u32 {
    let rec = ix.rec_count.max(ntfs::FIRST_USER_RECORD);
    ix.add_link(
        rec,
        0,
        parent,
        name,
        m.attrs
            | if m.is_dir {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                0
            },
        m.size,
        m.modified,
        m.created,
    )
}

fn add_tree(ix: &mut Index, dir_idx: u32, dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for ent in rd.flatten() {
        let Ok(m) = ent.metadata() else { continue };
        let meta = meta_of(&m);
        let name = ent.file_name().to_string_lossy().into_owned();
        let i = add(ix, dir_idx, &name, &meta);
        if meta.is_dir && meta.attrs & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
            add_tree(ix, i, &ent.path());
        }
    }
}

/// Parses a FILE_NOTIFY_INFORMATION buffer into (action, relative path).
pub fn parse_notifications(buf: &[u8]) -> Vec<(Action, String)> {
    let mut out = Vec::new();
    let mut off = 0usize;
    loop {
        if off + 12 > buf.len() {
            break;
        }
        let next = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        let action = u32::from_le_bytes(buf[off + 4..off + 8].try_into().unwrap());
        let len = u32::from_le_bytes(buf[off + 8..off + 12].try_into().unwrap()) as usize;
        let name_start = off + 12;
        if name_start + len > buf.len() {
            break;
        }
        let units = buf[name_start..name_start + len]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]));
        let name: String = char::decode_utf16(units)
            .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect();
        if let Some(a) = Action::from_raw(action) {
            out.push((a, name));
        }
        if next == 0 {
            break;
        }
        off += next;
    }
    out
}

pub enum WatchEvent {
    Changes(Vec<(Action, String)>),
    /// Buffer overflowed: too many changes at once; rescan.
    Overflow,
    Timeout,
}

/// Recursive directory watcher (overlapped, so it can time out and stop).
pub struct Watcher {
    dir: Handle,
    event: Handle,
    buf: Vec<u64>,
    ov: Box<OVERLAPPED>,
    armed: bool,
}

unsafe impl Send for Watcher {}

impl Watcher {
    pub fn new(root: &Path) -> std::io::Result<Watcher> {
        let p = wide(&root.to_string_lossy());
        let h: HANDLE = unsafe {
            windows_sys::Win32::Storage::FileSystem::CreateFileW(
                p.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if h == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let ev = unsafe { CreateEventW(null(), 1, 0, null()) };
        if ev.is_null() {
            unsafe { CloseHandle(h) };
            return Err(std::io::Error::last_os_error());
        }
        // 64 KB is the limit for network shares.
        Ok(Watcher {
            dir: Handle(h),
            event: Handle(ev),
            buf: vec![0u64; 8192],
            ov: Box::new(unsafe { std::mem::zeroed() }),
            armed: false,
        })
    }

    pub fn next(&mut self, timeout_ms: u32) -> std::io::Result<WatchEvent> {
        if !self.armed {
            *self.ov = unsafe { std::mem::zeroed() };
            self.ov.hEvent = self.event.0;
            let filter = FILE_NOTIFY_CHANGE_FILE_NAME
                | FILE_NOTIFY_CHANGE_DIR_NAME
                | FILE_NOTIFY_CHANGE_SIZE
                | FILE_NOTIFY_CHANGE_LAST_WRITE
                | FILE_NOTIFY_CHANGE_CREATION;
            let ok = unsafe {
                ReadDirectoryChangesW(
                    self.dir.0,
                    self.buf.as_mut_ptr() as *mut _,
                    (self.buf.len() * 8) as u32,
                    1,
                    filter,
                    std::ptr::null_mut(),
                    &mut *self.ov,
                    None,
                )
            };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            self.armed = true;
        }
        let mut n = 0u32;
        let ok = unsafe { GetOverlappedResultEx(self.dir.0, &*self.ov, &mut n, timeout_ms, 0) };
        if ok == 0 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(WAIT_TIMEOUT as i32) {
                return Ok(WatchEvent::Timeout);
            }
            self.armed = false;
            return Err(e);
        }
        self.armed = false;
        if n == 0 {
            return Ok(WatchEvent::Overflow);
        }
        let bytes: &[u8] = bytemuck::cast_slice(&self.buf);
        Ok(WatchEvent::Changes(parse_notifications(
            &bytes[..n as usize],
        )))
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        if self.armed {
            unsafe {
                CancelIoEx(self.dir.0, &*self.ov);
                let mut n = 0;
                GetOverlappedResultEx(self.dir.0, &*self.ov, &mut n, 1000, 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::Query;
    use crate::search::{SearchOptions, search};

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rapidfind-folder-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub").join("deep")).unwrap();
        std::fs::write(d.join("a.txt"), b"hello").unwrap();
        std::fs::write(d.join("sub").join("b.rs"), b"fn main(){}").unwrap();
        std::fs::write(d.join("sub").join("deep").join("c.md"), b"#").unwrap();
        d
    }

    fn names(ix: &Index, q: &str) -> Vec<String> {
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
    fn walk_builds_tree_with_sizes() {
        let d = tmpdir("walk");
        let ix = build(&d.to_string_lossy(), FoldMode::Lower);
        let root = root_name(&d.to_string_lossy());
        assert_eq!(names(&ix, "c.md"), vec![format!("{root}\\sub\\deep\\c.md")]);
        let a = resolve(&ix, "a.txt").unwrap();
        assert_eq!({ ix.entries[a as usize].size }, 5);
        assert!({ ix.entries[resolve(&ix, "sub").unwrap() as usize].size } >= 12);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn applies_add_rename_modify_remove() {
        let d = tmpdir("apply");
        let mut ix = build(&d.to_string_lossy(), FoldMode::Lower);
        let mut old = None;
        std::fs::create_dir_all(d.join("new").join("x")).unwrap();
        std::fs::write(d.join("new").join("x").join("y.txt"), b"1234").unwrap();
        assert!(apply_change(&mut ix, &d, Action::Added, "new", &mut old, 1));
        assert_eq!(
            names(&ix, "y.txt").len(),
            1,
            "added folder brings its subtree"
        );
        std::fs::rename(d.join("a.txt"), d.join("sub").join("moved.txt")).unwrap();
        apply_change(&mut ix, &d, Action::RenamedOld, "a.txt", &mut old, 2);
        apply_change(
            &mut ix,
            &d,
            Action::RenamedNew,
            "sub\\moved.txt",
            &mut old,
            2,
        );
        assert!(names(&ix, "a.txt").is_empty());
        assert!(names(&ix, "moved.txt")[0].ends_with("sub\\moved.txt"));
        std::fs::write(d.join("sub").join("moved.txt"), b"longer content").unwrap();
        apply_change(&mut ix, &d, Action::Modified, "sub\\moved.txt", &mut old, 3);
        assert_eq!(
            { ix.entries[resolve(&ix, "sub\\moved.txt").unwrap() as usize].size },
            14
        );
        apply_change(&mut ix, &d, Action::Removed, "sub", &mut old, 4);
        assert!(names(&ix, "b.rs").is_empty() && names(&ix, "moved.txt").is_empty());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn watcher_reports_changes() {
        let d = tmpdir("watch");
        let mut w = Watcher::new(&d).unwrap();
        assert!(matches!(w.next(50).unwrap(), WatchEvent::Timeout));
        std::fs::write(d.join("sub").join("fresh.txt"), b"x").unwrap();
        let mut seen = Vec::new();
        for _ in 0..20 {
            if let WatchEvent::Changes(c) = w.next(200).unwrap() {
                seen.extend(c);
            }
            if seen
                .iter()
                .any(|(a, p)| *a == Action::Added && p == "sub\\fresh.txt")
            {
                break;
            }
        }
        assert!(
            seen.iter()
                .any(|(a, p)| *a == Action::Added && p == "sub\\fresh.txt"),
            "{seen:?}"
        );
        drop(w);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn parses_notify_buffer() {
        let mut b = Vec::new();
        for (i, (a, n)) in [(1u32, "x.txt"), (4, "old"), (5, "dir\\new")]
            .iter()
            .enumerate()
        {
            let u: Vec<u16> = n.encode_utf16().collect();
            let len = (12 + u.len() * 2).next_multiple_of(4);
            let next = if i == 2 { 0 } else { len as u32 };
            let start = b.len();
            b.extend_from_slice(&next.to_le_bytes());
            b.extend_from_slice(&a.to_le_bytes());
            b.extend_from_slice(&((u.len() * 2) as u32).to_le_bytes());
            for c in u {
                b.extend_from_slice(&c.to_le_bytes());
            }
            b.resize(start + len, 0);
        }
        let v = parse_notifications(&b);
        assert_eq!(
            v,
            vec![
                (Action::Added, "x.txt".into()),
                (Action::RenamedOld, "old".into()),
                (Action::RenamedNew, "dir\\new".into())
            ]
        );
    }
}
