//! Change notification: fanotify (one filesystem mark) or inotify (a watch
//! per folder) on Linux, nothing elsewhere. Both report events as
//! (share, folder id, name) for [`crate::share::Share::apply`] to reconcile.

use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Something happened to `name` in folder `dir`: look at it again.
    Touch {
        share: usize,
        dir: u32,
        name: String,
    },
    Rename {
        share: usize,
        from_dir: u32,
        from_name: String,
        to_dir: u32,
        to_name: String,
    },
    /// A watched folder itself went away.
    DirGone { share: usize, dir: u32 },
    /// Events were lost: re-walk everything.
    Overflow,
}

impl Event {
    pub fn share(&self) -> Option<usize> {
        match self {
            Event::Touch { share, .. }
            | Event::Rename { share, .. }
            | Event::DirGone { share, .. } => Some(*share),
            Event::Overflow => None,
        }
    }
}

pub trait Watcher: Send {
    /// Follow these folders of `share` from now on.
    fn track(&mut self, share: usize, dirs: &[(u32, PathBuf)]);
    /// Stop following these (removed) folders.
    fn untrack(&mut self, share: usize, dirs: &[u32]);
    /// Waits up to `timeout` for events; returns as soon as some arrived and
    /// a short batching window passed.
    fn wait(&mut self, timeout: Duration) -> Vec<Event>;
    fn name(&self) -> &'static str;
}

/// Which mechanism to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Auto,
    Fanotify,
    Inotify,
    None,
}

impl std::str::FromStr for Kind {
    type Err = String;
    fn from_str(s: &str) -> Result<Kind, String> {
        Ok(match s {
            "auto" => Kind::Auto,
            "fanotify" => Kind::Fanotify,
            "inotify" => Kind::Inotify,
            "none" => Kind::None,
            _ => return Err(format!("--watch: auto, fanotify, inotify or none, not {s}")),
        })
    }
}

/// No notifications: only the scheduled re-walk keeps the index current.
pub struct NoWatch;

impl Watcher for NoWatch {
    fn track(&mut self, _: usize, _: &[(u32, PathBuf)]) {}
    fn untrack(&mut self, _: usize, _: &[u32]) {}
    fn wait(&mut self, timeout: Duration) -> Vec<Event> {
        std::thread::sleep(timeout);
        Vec::new()
    }
    fn name(&self) -> &'static str {
        "none (re-walks only)"
    }
}

/// Opens the best available watcher. `mark` is the mount to put a fanotify
/// filesystem mark on (it must be the filesystem's root for btrfs).
pub fn open(kind: Kind, mark: &[PathBuf]) -> Box<dyn Watcher> {
    #[cfg(target_os = "linux")]
    {
        if matches!(kind, Kind::Auto | Kind::Fanotify) {
            match linux::Fanotify::new(mark) {
                Ok(w) => return Box::new(w),
                Err(e) => log::warn!(
                    "fanotify isn't available ({e}); {}",
                    if kind == Kind::Fanotify {
                        "falling back to inotify"
                    } else {
                        "using inotify (give the container CAP_SYS_ADMIN and the volume's root mount for fanotify)"
                    }
                ),
            }
        }
        if kind != Kind::None {
            match linux::Inotify::new() {
                Ok(w) => return Box::new(w),
                Err(e) => {
                    log::warn!("inotify isn't available ({e}); only re-walks will find changes")
                }
            }
        }
    }
    let _ = (kind, mark);
    Box::new(NoWatch)
}

/// The mount point holding `path` (from /proc/self/mountinfo): where a
/// fanotify filesystem mark has to go.
pub fn mount_of(path: &Path) -> Option<PathBuf> {
    let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let path = path.canonicalize().ok()?;
    let mut best: Option<PathBuf> = None;
    for line in info.lines() {
        let Some(mp) = line.split(' ').nth(4) else {
            continue;
        };
        let mp = PathBuf::from(unescape(mp));
        if path.starts_with(&mp)
            && best
                .as_ref()
                .is_none_or(|b| mp.components().count() > b.components().count())
        {
            best = Some(mp);
        }
    }
    best
}

/// mountinfo escapes spaces and such as `\040`.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(target_os = "linux")]
pub mod linux {
    use std::collections::HashMap;
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::{Event, Watcher};

    /// Events arriving within this window after the first one form a batch.
    const BATCH: Duration = Duration::from_millis(150);
    /// Our own backlog cap: past this many events in one batch, re-walk instead.
    const MAX_BATCH: usize = 500_000;

    fn cpath(p: &Path) -> io::Result<CString> {
        CString::new(p.as_os_str().as_bytes()).map_err(io::Error::other)
    }

    fn poll_in(fd: i32, timeout: Duration) -> bool {
        let mut p = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
        unsafe { libc::poll(&mut p, 1, ms) > 0 }
    }

    /// Reads events for up to `timeout`, then keeps reading for the batch window.
    fn batch(fd: i32, timeout: Duration, mut parse: impl FnMut(&[u8]) -> bool) {
        if !poll_in(fd, timeout) {
            return;
        }
        let end = Instant::now() + BATCH;
        let mut buf = vec![0u8; 256 << 10];
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n > 0 {
                if !parse(&buf[..n as usize]) {
                    return;
                }
                continue;
            }
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() || !poll_in(fd, left) {
                return;
            }
        }
    }

    // ---- fanotify ------------------------------------------------------------------

    const FAN_CLASS_NOTIF: u32 = 0;
    const FAN_CLOEXEC: u32 = 0x1;
    const FAN_NONBLOCK: u32 = 0x2;
    const FAN_UNLIMITED_QUEUE: u32 = 0x10;
    const FAN_REPORT_DIR_FID: u32 = 0x400;
    const FAN_REPORT_NAME: u32 = 0x800;
    const FAN_MARK_ADD: u32 = 0x1;
    const FAN_MARK_FILESYSTEM: u32 = 0x100;
    const FAN_MODIFY: u64 = 0x2;
    const FAN_ATTRIB: u64 = 0x4;
    const FAN_CLOSE_WRITE: u64 = 0x8;
    const FAN_MOVED_FROM: u64 = 0x40;
    const FAN_MOVED_TO: u64 = 0x80;
    const FAN_CREATE: u64 = 0x100;
    const FAN_DELETE: u64 = 0x200;
    const FAN_Q_OVERFLOW: u64 = 0x4000;
    const FAN_RENAME: u64 = 0x1000_0000;
    const FAN_ONDIR: u64 = 0x4000_0000;
    const INFO_DFID_NAME: u8 = 2;
    const INFO_OLD_DFID_NAME: u8 = 10;
    const INFO_NEW_DFID_NAME: u8 = 12;
    const MAX_HANDLE_SZ: usize = 128;

    /// One filesystem mark per mount; folders are recognised by file handle.
    pub struct Fanotify {
        fd: i32,
        /// Handle (type + bytes) → (share, folder id).
        dirs: HashMap<Box<[u8]>, (usize, u32)>,
        keys: HashMap<(usize, u32), Box<[u8]>>,
        renames: bool,
    }

    /// A folder's file handle as events report it: type, then bytes.
    fn handle_of(p: &Path) -> io::Result<Box<[u8]>> {
        #[repr(C)]
        struct Fh {
            bytes: u32,
            kind: i32,
            data: [u8; MAX_HANDLE_SZ],
        }
        let c = cpath(p)?;
        let mut fh = Fh {
            bytes: MAX_HANDLE_SZ as u32,
            kind: 0,
            data: [0; MAX_HANDLE_SZ],
        };
        let mut mount_id: i32 = 0;
        let r = unsafe {
            libc::syscall(
                libc::SYS_name_to_handle_at,
                libc::AT_FDCWD,
                c.as_ptr(),
                &mut fh as *mut Fh,
                &mut mount_id as *mut i32,
                0,
            )
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut k = fh.kind.to_ne_bytes().to_vec();
        k.extend_from_slice(&fh.data[..fh.bytes as usize]);
        Ok(k.into_boxed_slice())
    }

    impl Fanotify {
        pub fn new(marks: &[PathBuf]) -> io::Result<Fanotify> {
            if marks.is_empty() {
                return Err(io::Error::other("nothing to mark"));
            }
            let flags = FAN_CLASS_NOTIF
                | FAN_CLOEXEC
                | FAN_NONBLOCK
                | FAN_UNLIMITED_QUEUE
                | FAN_REPORT_DIR_FID
                | FAN_REPORT_NAME;
            let fd =
                unsafe { libc::fanotify_init(flags, (libc::O_RDONLY | libc::O_LARGEFILE) as u32) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let base = FAN_CREATE
                | FAN_DELETE
                | FAN_MOVED_FROM
                | FAN_MOVED_TO
                | FAN_CLOSE_WRITE
                | FAN_MODIFY
                | FAN_ATTRIB
                | FAN_ONDIR;
            let mut renames = true;
            for m in marks {
                let c = cpath(m)?;
                let mark = |mask: u64| unsafe {
                    libc::fanotify_mark(
                        fd,
                        FAN_MARK_ADD | FAN_MARK_FILESYSTEM,
                        mask,
                        libc::AT_FDCWD,
                        c.as_ptr(),
                    )
                };
                if renames && mark(base | FAN_RENAME) == 0 {
                    continue;
                }
                // Before 5.17 there's no FAN_RENAME: moves become delete + create.
                renames = false;
                if mark(base) != 0 {
                    let e = io::Error::last_os_error();
                    unsafe { libc::close(fd) };
                    return Err(io::Error::new(
                        e.kind(),
                        format!(
                            "marking {}: {e}{}",
                            m.display(),
                            if e.raw_os_error() == Some(libc::EXDEV) {
                                " (a btrfs subvolume: mount the volume's root instead)"
                            } else {
                                ""
                            }
                        ),
                    ));
                }
            }
            log::info!(
                "watching with fanotify on {}{}",
                marks
                    .iter()
                    .map(|m| m.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                if renames {
                    ""
                } else {
                    " (no FAN_RENAME: moves are delete + create)"
                }
            );
            Ok(Fanotify {
                fd,
                dirs: HashMap::new(),
                keys: HashMap::new(),
                renames,
            })
        }
    }

    impl Drop for Fanotify {
        fn drop(&mut self) {
            unsafe { libc::close(self.fd) };
        }
    }

    /// One DFID_NAME info record: (handle key, name).
    fn dfid_name(rec: &[u8]) -> Option<(&[u8], &str)> {
        // header(4) fsid(8) handle_bytes(4) handle_type(4) handle name\0
        let hb = u32::from_ne_bytes(rec.get(12..16)?.try_into().ok()?) as usize;
        let key = rec.get(16..20 + hb)?;
        let rest = &rec[20 + hb..];
        let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        Some((key, std::str::from_utf8(&rest[..end]).ok()?))
    }

    impl Watcher for Fanotify {
        fn track(&mut self, share: usize, dirs: &[(u32, PathBuf)]) {
            for (id, p) in dirs {
                match handle_of(p) {
                    Ok(h) => {
                        self.keys.insert((share, *id), h.clone());
                        self.dirs.insert(h, (share, *id));
                    }
                    Err(e) => log::debug!("{}: no file handle ({e})", p.display()),
                }
            }
        }

        fn untrack(&mut self, share: usize, dirs: &[u32]) {
            for id in dirs {
                if let Some(h) = self.keys.remove(&(share, *id)) {
                    self.dirs.remove(&h);
                }
            }
        }

        fn wait(&mut self, timeout: Duration) -> Vec<Event> {
            let mut out = Vec::new();
            let dirs = &self.dirs;
            let mut overflow = false;
            batch(self.fd, timeout, |buf| {
                let mut off = 0;
                while off + 24 <= buf.len() {
                    let len = u32::from_ne_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
                    let meta_len =
                        u16::from_ne_bytes(buf[off + 6..off + 8].try_into().unwrap()) as usize;
                    let mask = u64::from_ne_bytes(buf[off + 8..off + 16].try_into().unwrap());
                    if len < 24 || off + len > buf.len() {
                        break;
                    }
                    let ev = &buf[off..off + len];
                    off += len;
                    if mask & FAN_Q_OVERFLOW != 0 {
                        overflow = true;
                        continue;
                    }
                    let mut old = None;
                    let mut new = None;
                    let mut plain = None;
                    let mut i = meta_len;
                    while i + 4 <= ev.len() {
                        let t = ev[i];
                        let l = u16::from_ne_bytes(ev[i + 2..i + 4].try_into().unwrap()) as usize;
                        if l < 4 || i + l > ev.len() {
                            break;
                        }
                        let rec = &ev[i..i + l];
                        match t {
                            INFO_DFID_NAME => plain = dfid_name(rec),
                            INFO_OLD_DFID_NAME => old = dfid_name(rec),
                            INFO_NEW_DFID_NAME => new = dfid_name(rec),
                            _ => {}
                        }
                        i += l;
                    }
                    let known = |r: Option<(&[u8], &str)>| {
                        r.and_then(|(k, n)| dirs.get(k).map(|&(s, d)| (s, d, n.to_string())))
                    };
                    if mask & FAN_RENAME != 0 {
                        match (known(old), known(new)) {
                            (Some((s1, d1, n1)), Some((s2, d2, n2))) if s1 == s2 => {
                                out.push(Event::Rename {
                                    share: s1,
                                    from_dir: d1,
                                    from_name: n1,
                                    to_dir: d2,
                                    to_name: n2,
                                })
                            }
                            (a, b) => {
                                for (s, d, n) in [a, b].into_iter().flatten() {
                                    out.push(Event::Touch {
                                        share: s,
                                        dir: d,
                                        name: n,
                                    });
                                }
                            }
                        }
                    } else if let Some((s, d, n)) = known(plain) {
                        out.push(Event::Touch {
                            share: s,
                            dir: d,
                            name: n,
                        });
                    }
                }
                if out.len() > MAX_BATCH {
                    overflow = true;
                }
                !overflow
            });
            if overflow {
                return vec![Event::Overflow];
            }
            let _ = self.renames;
            out
        }

        fn name(&self) -> &'static str {
            "fanotify"
        }
    }

    // ---- inotify -------------------------------------------------------------------

    const IN_MODIFY: u32 = 0x2;
    const IN_ATTRIB: u32 = 0x4;
    const IN_CLOSE_WRITE: u32 = 0x8;
    const IN_MOVED_FROM: u32 = 0x40;
    const IN_MOVED_TO: u32 = 0x80;
    const IN_CREATE: u32 = 0x100;
    const IN_DELETE: u32 = 0x200;
    const IN_DELETE_SELF: u32 = 0x400;
    const IN_Q_OVERFLOW: u32 = 0x4000;
    const IN_IGNORED: u32 = 0x8000;
    const IN_ONLYDIR: u32 = 0x0100_0000;
    const IN_DONT_FOLLOW: u32 = 0x0200_0000;
    const IN_EXCL_UNLINK: u32 = 0x0400_0000;

    /// A watch per folder.
    pub struct Inotify {
        fd: i32,
        wds: HashMap<i32, (usize, u32)>,
        ids: HashMap<(usize, u32), i32>,
        warned_full: bool,
    }

    impl Inotify {
        pub fn new() -> io::Result<Inotify> {
            let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            log::info!("watching with inotify (a watch per folder)");
            Ok(Inotify {
                fd,
                wds: HashMap::new(),
                ids: HashMap::new(),
                warned_full: false,
            })
        }

        pub fn watches(&self) -> usize {
            self.wds.len()
        }
    }

    impl Drop for Inotify {
        fn drop(&mut self) {
            unsafe { libc::close(self.fd) };
        }
    }

    impl Watcher for Inotify {
        fn track(&mut self, share: usize, dirs: &[(u32, PathBuf)]) {
            let mask = IN_CREATE
                | IN_DELETE
                | IN_MOVED_FROM
                | IN_MOVED_TO
                | IN_CLOSE_WRITE
                | IN_MODIFY
                | IN_ATTRIB
                | IN_DELETE_SELF
                | IN_ONLYDIR
                | IN_DONT_FOLLOW
                | IN_EXCL_UNLINK;
            for (id, p) in dirs {
                let Ok(c) = cpath(p) else { continue };
                let wd = unsafe { libc::inotify_add_watch(self.fd, c.as_ptr(), mask) };
                if wd < 0 {
                    let e = io::Error::last_os_error();
                    if e.raw_os_error() == Some(libc::ENOSPC) && !self.warned_full {
                        self.warned_full = true;
                        log::warn!(
                            "out of inotify watches at {} folders: raise fs.inotify.max_user_watches",
                            self.wds.len()
                        );
                    }
                    continue;
                }
                // A moved folder keeps its watch; re-adding returns the same wd.
                self.wds.insert(wd, (share, *id));
                self.ids.insert((share, *id), wd);
            }
        }

        fn untrack(&mut self, share: usize, dirs: &[u32]) {
            for id in dirs {
                if let Some(wd) = self.ids.remove(&(share, *id)) {
                    self.wds.remove(&wd);
                    unsafe { libc::inotify_rm_watch(self.fd, wd) };
                }
            }
        }

        fn wait(&mut self, timeout: Duration) -> Vec<Event> {
            let mut out = Vec::new();
            let mut overflow = false;
            let mut moved_from: HashMap<u32, (usize, u32, String)> = HashMap::new();
            let wds = &mut self.wds;
            batch(self.fd, timeout, |buf| {
                let mut off = 0;
                while off + 16 <= buf.len() {
                    let wd = i32::from_ne_bytes(buf[off..off + 4].try_into().unwrap());
                    let mask = u32::from_ne_bytes(buf[off + 4..off + 8].try_into().unwrap());
                    let cookie = u32::from_ne_bytes(buf[off + 8..off + 12].try_into().unwrap());
                    let len =
                        u32::from_ne_bytes(buf[off + 12..off + 16].try_into().unwrap()) as usize;
                    let raw = &buf[off + 16..(off + 16 + len).min(buf.len())];
                    off += 16 + len;
                    if mask & IN_Q_OVERFLOW != 0 {
                        overflow = true;
                        continue;
                    }
                    let Some(&(share, dir)) = wds.get(&wd) else {
                        continue;
                    };
                    if mask & IN_IGNORED != 0 {
                        wds.remove(&wd);
                        continue;
                    }
                    if mask & IN_DELETE_SELF != 0 {
                        out.push(Event::DirGone { share, dir });
                        continue;
                    }
                    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
                    let Ok(name) = std::str::from_utf8(&raw[..end]) else {
                        continue;
                    };
                    if mask & IN_MOVED_FROM != 0 {
                        moved_from.insert(cookie, (share, dir, name.to_string()));
                    } else if mask & IN_MOVED_TO != 0 {
                        match moved_from.remove(&cookie) {
                            Some((s, d, n)) if s == share => out.push(Event::Rename {
                                share,
                                from_dir: d,
                                from_name: n,
                                to_dir: dir,
                                to_name: name.to_string(),
                            }),
                            other => {
                                if let Some((s, d, n)) = other {
                                    out.push(Event::Touch {
                                        share: s,
                                        dir: d,
                                        name: n,
                                    });
                                }
                                out.push(Event::Touch {
                                    share,
                                    dir,
                                    name: name.to_string(),
                                });
                            }
                        }
                    } else {
                        out.push(Event::Touch {
                            share,
                            dir,
                            name: name.to_string(),
                        });
                    }
                }
                if out.len() > MAX_BATCH {
                    overflow = true;
                }
                !overflow
            });
            if overflow {
                return vec![Event::Overflow];
            }
            // A move out of every watched folder: the source just went away.
            for (_, (share, dir, name)) in moved_from {
                out.push(Event::Touch { share, dir, name });
            }
            out
        }

        fn name(&self) -> &'static str {
            "inotify"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mountinfo_escapes() {
        assert_eq!(unescape(r"/mnt/my\040disk"), "/mnt/my disk");
        assert_eq!(unescape("/volume1"), "/volume1");
    }

    #[test]
    fn kinds_parse() {
        assert_eq!("auto".parse::<Kind>(), Ok(Kind::Auto));
        assert!("x".parse::<Kind>().is_err());
    }
}
