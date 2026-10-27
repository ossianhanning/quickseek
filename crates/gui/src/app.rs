//! The main window.

use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use quickseek_core::content::{self, ContentOptions, ContentQuery, Scan, Stats};
use quickseek_core::fmt;
use quickseek_core::fold::FoldMode;
use quickseek_core::history::SnapInfo;
use quickseek_core::ipc::{self, Backend, Page, Row, StatusInfo};
use quickseek_core::query::{FILTERS, Query, SearchFlags};
use quickseek_core::search::SortKey;
use quickseek_core::tree::{TreeLevel, TreeSort};
use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, RegisterHotKey, ReleaseCapture, SetCapture, SetFocus, UnregisterHotKey,
    VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_NEXT, VK_RETURN, VK_SHIFT,
};
use windows_sys::Win32::UI::Shell::{
    DefSubclassProc, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
    SetWindowSubclass, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::highlight;
use crate::preview::Preview;
use crate::results::{BLOCK, Results};
use crate::settings::{Settings, parse_hotkey};
use crate::shell::{self, FileInfoCache, MenuChoice};
use crate::theme;
use crate::tree as tv;
use crate::w::{self, px, send, wide};

pub const CLASS: &str = "quickseek.main";
pub const ID_SEARCH: i32 = 100;
pub const ID_FILTER: i32 = 101;
pub const ID_LIST: i32 = 102;
pub const ID_STATUS: i32 = 103;
pub const ID_TREE: i32 = 104;
pub const ID_TREE_BTN: i32 = 105;
pub const ID_PREVIEW_BTN: i32 = 108;
pub const ID_PROGRESS: i32 = 109;

pub const WM_APP_RESULT: u32 = WM_APP + 1;
pub const WM_APP_STATUS: u32 = WM_APP + 2;
pub const WM_APP_TRAY: u32 = WM_APP + 3;
/// Posted by a second instance: show the existing window.
pub const WM_APP_SHOW: u32 = WM_APP + 4;
pub const WM_APP_TREE: u32 = WM_APP + 5;
pub const WM_APP_CONTENT: u32 = WM_APP + 6;
/// A background delete finished (lparam: `Box<shell::DeleteDone>`).
pub const WM_APP_DELETED: u32 = WM_APP + 7;
/// The service connection broke (wparam 0) or is back (1; lparam: a boxed
/// Option<String>, the service's build).
const WM_APP_CONN: u32 = WM_APP + 22;
/// A column was resized or moved by the user (wparam: its header).
const WM_APP_COLUMNS: u32 = WM_APP + 21;
const TIMER_POLL: usize = 1;
const TIMER_PREVIEW: usize = 2;
const TIMER_GROWTH: usize = 3;
const HOTKEY_ID: i32 = 1;

// Menu command ids (< 0x1000; shell menu items use the range above).
const CMD_OPEN: u32 = 200;
const CMD_OPEN_PATH: u32 = 201;
const CMD_COPY_FULL: u32 = 202;
const CMD_COPY_NAME: u32 = 203;
const CMD_COPY_PATH: u32 = 204;
const CMD_EXPORT: u32 = 205;
const CMD_PROPERTIES: u32 = 206;
const CMD_EXIT: u32 = 209;
const CMD_EXPAND: u32 = 210;
const CMD_COLLAPSE: u32 = 211;
const CMD_SEARCH_HERE: u32 = 212;
const CMD_SHOW_IN_TREE: u32 = 213;
const CMD_CASE: u32 = 300;
const CMD_WW: u32 = 301;
const CMD_PATH: u32 = 302;
const CMD_REGEX: u32 = 303;
const CMD_DIACRITICS: u32 = 304;
const CMD_STOP: u32 = 305;
const CMD_REFRESH: u32 = 400;
const CMD_SORT_RUNS: u32 = 401;
const CMD_SORT_CREATED: u32 = 402;
pub const CMD_TREE: u32 = 403;
pub const CMD_PREVIEW: u32 = 404;
const CMD_OPTIONS: u32 = 500;
const CMD_LOGIN: u32 = 501;
const CMD_BACKGROUND: u32 = 502;
pub const CMD_CONNECT_AI: u32 = 503;
pub const CMD_COMPARE_DAY: u32 = 410;
pub const CMD_COMPARE_WEEK: u32 = 411;
pub const CMD_COMPARE_MONTH: u32 = 412;
pub const CMD_SAVE_POINT: u32 = 413;
pub const CMD_STOP_COMPARE: u32 = 414;
pub const CMD_THEME_SYSTEM: u32 = 420;
pub const CMD_THEME_LIGHT: u32 = 421;
pub const CMD_THEME_DARK: u32 = 422;
const CMD_SYNTAX: u32 = 600;
const CMD_ABOUT: u32 = 601;
const CMD_SHOW: u32 = 700;

const COLUMNS: [(&str, i32, bool); 5] = [
    ("Name", 260, false),
    ("Path", 440, false),
    ("Size", 120, true),
    ("Date Modified", 140, false),
    ("Type", 170, false),
];

pub struct Conn {
    pub be: Box<dyn Backend>,
    /// The search whose result set the backend session currently holds.
    pub seq: u64,
    /// How many times content search has filtered that result set.
    pub ver: u64,
}

enum Job {
    Search {
        seq: u64,
        query: String,
        flags: SearchFlags,
        sort: SortKey,
        desc: bool,
        keep: bool,
    },
    Poll,
    Run(String),
    Tree {
        seq: u64,
        query: String,
        flags: SearchFlags,
        sort: TreeSort,
        desc: bool,
    },
    TreeClose,
}

struct TreeDone {
    seq: u64,
    res: Result<TreeLevel, String>,
    ms: f64,
}

struct SearchDone {
    seq: u64,
    keep: bool,
    res: Result<Page, String>,
}

/// Progress of a content search: new stats, and the filtered first page
/// when rows were dropped.
struct ContentUpdate {
    seq: u64,
    ver: u64,
    page: Option<Result<Page, String>>,
    stats: Stats,
}

/// Reads the files of search `seq`'s results (as this user) and drops the
/// rows that don't match from the service's result set, posting progress
/// about three times a second.
fn content_scan(
    conn: Arc<Mutex<Conn>>,
    hwnd: isize,
    seq: u64,
    q: Arc<ContentQuery>,
    opts: ContentOptions,
    cancel: Arc<AtomicBool>,
) {
    let post = |u: ContentUpdate| unsafe {
        PostMessageW(
            hwnd as HWND,
            WM_APP_CONTENT,
            0,
            Box::into_raw(Box::new(u)) as isize,
        );
    };
    let superseded = || std::io::Error::other("superseded");
    let mut scan = Scan::new(q, opts, cancel);
    let mut pending: Vec<u32> = Vec::new();
    let mut last = Instant::now();
    loop {
        let batch = scan.next_batch(&mut |req| {
            let mut c = conn.lock();
            if c.seq != seq {
                return Err(superseded());
            }
            c.be.call(req)
        });
        let batch = match batch {
            Ok(b) => b,
            Err(e) => {
                if conn.lock().seq == seq {
                    scan.stats.done = true;
                    post(ContentUpdate {
                        seq,
                        ver: 0,
                        page: Some(Err(e)),
                        stats: scan.stats.clone(),
                    });
                }
                return;
            }
        };
        let done = batch.is_none();
        if let Some(b) = &batch {
            pending.extend(b.rejected());
        }
        if done || last.elapsed() >= Duration::from_millis(300) {
            last = Instant::now();
            let (page, ver) = if pending.is_empty() {
                (None, 0)
            } else {
                let mut c = conn.lock();
                if c.seq != seq {
                    return;
                }
                let p =
                    c.be.drop_rows(std::mem::take(&mut pending), BLOCK as u32 * 2);
                c.ver += 1;
                (Some(p), c.ver)
            };
            post(ContentUpdate {
                seq,
                ver,
                page,
                stats: scan.stats.clone(),
            });
        }
        if done {
            return;
        }
    }
}

pub struct App {
    pub hwnd: Cell<HWND>,
    pub edit: Cell<HWND>,
    pub combo: Cell<HWND>,
    pub list: Cell<HWND>,
    pub status: Cell<HWND>,
    menu: Cell<HMENU>,
    font: Cell<HFONT>,
    bold: Cell<HFONT>,
    pub conn: Arc<Mutex<Conn>>,
    jobs: Sender<Job>,
    results: RefCell<Results>,
    latest_seq: Cell<u64>,
    searching: Cell<bool>,
    hl_terms: RefCell<Vec<Vec<u8>>>,
    files: RefCell<FileInfoCache>,
    pub settings: RefCell<Settings>,
    disp: RefCell<Vec<u16>>,
    last_status: RefCell<Option<StatusInfo>>,
    test_mode: bool,
    tray_icon: Cell<HICON>,
    taskbar_created: u32,
    /// [`upgrade::CLOSE_FOR_UPDATE`]: an updater is about to replace us.
    close_for_update: u32,
    pub tree_btn: Cell<HWND>,
    pub tree_list: Cell<HWND>,
    tree: RefCell<tv::Model>,
    tree_seq: Cell<u64>,
    /// After the next tree build: expand these paths, then select `reveal`.
    restore: RefCell<Vec<String>>,
    reveal: RefCell<Option<String>>,
    /// When the last click toggled a +/− box (so the double-click that may follow doesn't toggle back).
    glyph_click: Cell<u32>,
    /// Comparing with a snapshot, the timeline strip and the details pane.
    pub growth: RefCell<crate::growth::Growth>,
    /// Paths being deleted in the background (lowercase, no trailing `\`).
    deleting: RefCell<std::collections::HashSet<String>>,
    deletes_running: Cell<u32>,
    /// What the last delete did, shown before the status text (until it expires).
    delete_note: RefCell<Option<(String, Option<Instant>)>>,
    /// Why the last search failed (a bad query, the service away), shown
    /// in the status bar until a search succeeds.
    search_error: RefCell<Option<String>>,
    /// The index generation the tree was built from, and when.
    tree_gen: Cell<u64>,
    tree_at: Cell<Option<Instant>>,
    /// The tree build in flight refreshes in place (no "adding up" status).
    tree_quiet: Cell<bool>,
    /// Select these again after the tree build in flight.
    keep_sel: RefCell<Vec<String>>,
    strip: Cell<HWND>,
    gpane: Cell<HWND>,
    pub preview: Preview,
    pub preview_btn: Cell<HWND>,
    progress: Cell<HWND>,
    /// The `content:` terms of the current search.
    content: RefCell<Option<Arc<ContentQuery>>>,
    scan_cancel: RefCell<Option<Arc<AtomicBool>>>,
    scan_stats: RefCell<Option<Stats>>,
    /// The splitter left of the preview pane: its x, and the drag offset while dragging.
    split_x: Cell<i32>,
    split_drag: Cell<Option<i32>>,
    /// The last column (in display order) of the list [0] and the tree [1]
    /// stretches to the right edge, so the hover and selection highlight
    /// span the row: which column that is and its own width (96 DPI).
    filler: [Cell<(i32, i32)>; 2],
    /// Set while we resize a column ourselves (not the user dragging it).
    stretching: Cell<bool>,
}

thread_local! {
    static APP: RefCell<Option<&'static App>> = const { RefCell::new(None) };
}

pub fn app() -> Option<&'static App> {
    APP.with(|a| *a.borrow())
}

fn sort_name(s: SortKey) -> &'static str {
    match s {
        SortKey::Name => "name",
        SortKey::Path => "path",
        SortKey::Size => "size",
        SortKey::Modified => "modified",
        SortKey::Created => "created",
        SortKey::RunCount => "runs",
    }
}

fn sort_from(s: &str) -> SortKey {
    match s {
        "path" => SortKey::Path,
        "size" => SortKey::Size,
        "modified" => SortKey::Modified,
        "created" => SortKey::Created,
        "runs" => SortKey::RunCount,
        _ => SortKey::Name,
    }
}

fn worker(conn: Arc<Mutex<Conn>>, rx: Receiver<Job>, hwnd: isize) {
    let post = |msg: u32, w: usize, l: isize| unsafe {
        PostMessageW(hwnd as HWND, msg, w, l);
    };
    let mut lost = false;
    while let Ok(job) = rx.recv() {
        // Coalesce: typing fast queues searches; only the newest matters.
        let mut job = job;
        let mut poll = false;
        while let Ok(next) = rx.try_recv() {
            match (&job, next) {
                (_, Job::Poll) => poll = true,
                (_, n @ (Job::Search { .. } | Job::Tree { .. } | Job::TreeClose)) => job = n,
                (_, r @ Job::Run(_)) => {
                    if let Job::Run(p) = r {
                        conn.lock().be.record_run(&p);
                    }
                }
            }
        }
        match job {
            Job::Search {
                seq,
                query,
                flags,
                sort,
                desc,
                keep,
            } => {
                let mut c = conn.lock();
                let res = c.be.search(&query, flags, sort, desc, BLOCK as u32 * 2);
                c.seq = seq;
                c.ver = 0;
                drop(c);
                let b = Box::new(SearchDone { seq, keep, res });
                post(WM_APP_RESULT, 0, Box::into_raw(b) as isize);
            }
            Job::Poll => poll = true,
            Job::Run(p) => {
                conn.lock().be.record_run(&p);
            }
            Job::Tree {
                seq,
                query,
                flags,
                sort,
                desc,
            } => {
                let t = std::time::Instant::now();
                let res = conn.lock().be.tree(&query, flags, sort, desc);
                let b = Box::new(TreeDone {
                    seq,
                    res,
                    ms: t.elapsed().as_secs_f64() * 1000.0,
                });
                post(WM_APP_TREE, 0, Box::into_raw(b) as isize);
            }
            Job::TreeClose => conn.lock().be.tree_close(),
        }
        if poll && let Some(s) = conn.lock().be.status() {
            post(WM_APP_STATUS, 0, Box::into_raw(Box::new(s)) as isize);
        }
        // The service went away (restarted, updated) or came back.
        let (now_lost, back) = {
            let mut c = conn.lock();
            (c.be.connection_lost(), c.be.take_reconnected())
        };
        if let Some(build) = back {
            post(WM_APP_CONN, 1, Box::into_raw(Box::new(build)) as isize);
        } else if now_lost && !lost {
            post(WM_APP_CONN, 0, 0);
        }
        lost = now_lost;
    }
}

impl App {
    fn flags(&self) -> SearchFlags {
        let s = self.settings.borrow();
        SearchFlags {
            case: s.match_case,
            whole_word: s.whole_word,
            path: s.match_path,
            regex: s.regex,
        }
    }

    fn sort(&self) -> (SortKey, bool) {
        let s = self.settings.borrow();
        (sort_from(&s.sort), s.descending)
    }

    pub fn query_text(&self) -> String {
        let text = w::get_text(self.edit.get());
        let f = self.settings.borrow().filter;
        match FILTERS.get(f) {
            Some((_, m)) if !m.is_empty() => format!("{m} {text}"),
            _ => text,
        }
    }

    pub fn tree_mode(&self) -> bool {
        self.settings.borrow().tree_mode
    }

    fn tree_sort(&self) -> (TreeSort, bool) {
        let s = self.settings.borrow();
        (tv::sort_from(&s.tree_sort), s.tree_descending)
    }

    pub fn search(&self, keep: bool) {
        if self.tree_mode() {
            self.build_tree(keep);
            return;
        }
        self.stop_scan();
        let seq = self.latest_seq.get() + 1;
        self.latest_seq.set(seq);
        let (names, cq) = match content::split_query(&self.query_text(), self.flags()) {
            Ok(v) => v,
            Err(e) => {
                *self.content.borrow_mut() = None;
                *self.results.borrow_mut() = Results::default();
                send(self.list.get(), LVM_SETITEMCOUNT, 0, 0);
                self.set_status_left(&e);
                return;
            }
        };
        *self.content.borrow_mut() = cq.map(Arc::new);
        *self.scan_stats.borrow_mut() = None;
        self.searching.set(true);
        let (sort, desc) = self.sort();
        let _ = self.jobs.send(Job::Search {
            seq,
            query: names,
            flags: self.flags(),
            sort,
            desc,
            keep,
        });
        let text = w::get_text(self.edit.get());
        let title = if text.is_empty() {
            "quickseek".to_string()
        } else {
            format!("{text} - Quickseek")
        };
        w::set_text(self.hwnd.get(), &title);
    }

    fn on_result(&self, done: SearchDone) {
        if done.seq != self.latest_seq.get() {
            return; // superseded
        }
        self.searching.set(false);
        let list = self.list.get();
        *self.search_error.borrow_mut() = done.res.as_ref().err().map(|e| {
            if e == quickseek_core::ipc::LOADING {
                "Index is still loading…".to_string()
            } else {
                e.clone()
            }
        });
        match done.res {
            Ok(page) => {
                let total = page.total.min(i32::MAX as u64) as usize;
                *self.results.borrow_mut() = Results::new(done.seq, page);
                let text = self.query_text();
                let terms = Query::parse_with(
                    &text,
                    self.flags(),
                    FoldMode::Lower,
                    &quickseek_core::date::DateEnv::system(),
                )
                .map(|q| {
                    if self.flags().case {
                        Vec::new()
                    } else {
                        q.highlight_terms()
                    }
                })
                .unwrap_or_default();
                *self.hl_terms.borrow_mut() = terms;
                if done.keep {
                    send(
                        list,
                        LVM_SETITEMCOUNT,
                        total,
                        (LVSICF_NOSCROLL | LVSICF_NOINVALIDATEALL) as isize,
                    );
                } else {
                    send(list, LVM_SETITEMCOUNT, total, 0);
                    let mut it: LVITEMW = unsafe { std::mem::zeroed() };
                    it.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
                    send(list, LVM_SETITEMSTATE, usize::MAX, &it as *const _ as isize);
                    if total > 0 {
                        send(list, LVM_ENSUREVISIBLE, 0, 0);
                    }
                }
                unsafe { InvalidateRect(list, null(), 0) };
                let cq = self.content.borrow().clone();
                if let Some(cq) = cq {
                    self.start_scan(done.seq, cq);
                }
            }
            Err(_) => {
                *self.results.borrow_mut() = Results::default();
                send(list, LVM_SETITEMCOUNT, 0, 0);
                self.update_status();
                return;
            }
        }
        self.update_status();
        self.preview_soon();
    }

    // ---- content search -----------------------------------------------------------

    fn content_options(&self) -> ContentOptions {
        let s = self.settings.borrow();
        ContentOptions {
            max_size: s.content_max_mb.max(1) << 20,
            binary: s.content_binary,
            ifilter: s.content_ifilter,
            byte_budget: None,
            time_limit: None,
        }
    }

    /// Starts reading the files of search `seq`'s results.
    fn start_scan(&self, seq: u64, cq: Arc<ContentQuery>) {
        let cancel = Arc::new(AtomicBool::new(false));
        *self.scan_cancel.borrow_mut() = Some(cancel.clone());
        *self.scan_stats.borrow_mut() = Some(Stats::default());
        let (conn, hwnd, opts) = (
            self.conn.clone(),
            self.hwnd.get() as isize,
            self.content_options(),
        );
        std::thread::Builder::new()
            .name("content".into())
            .spawn(move || content_scan(conn, hwnd, seq, cq, opts, cancel))
            .unwrap();
        let pb = self.progress.get();
        send(pb, PBM_SETPOS, 0, 0);
        unsafe { ShowWindow(pb, SW_SHOW) };
    }

    pub fn scanning(&self) -> bool {
        self.scan_stats.borrow().as_ref().is_some_and(|s| !s.done)
    }

    /// Stops a running content search; the rows not read yet stay listed.
    pub fn stop_scan(&self) {
        if let Some(c) = self.scan_cancel.borrow_mut().take() {
            c.store(true, Ordering::Relaxed);
        }
    }

    fn on_content(&self, u: ContentUpdate) {
        if u.seq != self.latest_seq.get() || self.tree_mode() {
            return;
        }
        let list = self.list.get();
        match u.page {
            Some(Ok(page)) => {
                let total = page.total.min(i32::MAX as u64) as usize;
                let mut r = Results::new(u.seq, page);
                r.ver = u.ver;
                *self.results.borrow_mut() = r;
                send(
                    list,
                    LVM_SETITEMCOUNT,
                    total,
                    (LVSICF_NOSCROLL | LVSICF_NOINVALIDATEALL) as isize,
                );
                unsafe { InvalidateRect(list, null(), 0) };
            }
            Some(Err(e)) if e == quickseek_core::ipc::STALE => {
                self.search(true); // the index was compacted: start over
                return;
            }
            Some(Err(e)) => {
                *self.scan_stats.borrow_mut() = Some(u.stats);
                unsafe { ShowWindow(self.progress.get(), SW_HIDE) };
                self.set_status_left(&format!("Content search failed: {e}"));
                return;
            }
            None => {}
        }
        let done = u.stats.done;
        let pos = (u.stats.checked * 1000)
            .checked_div(u.stats.total)
            .unwrap_or(0);
        send(
            self.progress.get(),
            PBM_SETPOS,
            if done { 1000 } else { pos as usize },
            0,
        );
        *self.scan_stats.borrow_mut() = Some(u.stats);
        if done {
            self.scan_cancel.borrow_mut().take();
            unsafe { ShowWindow(self.progress.get(), SW_HIDE) };
        }
        self.update_status();
        self.preview_soon();
    }

    // ---- preview pane ---------------------------------------------------------------

    pub fn preview_on(&self) -> bool {
        self.settings.borrow().preview
    }

    pub fn set_preview(&self, on: bool) {
        self.settings.borrow_mut().preview = on;
        send(self.preview_btn.get(), BM_SETCHECK, on as usize, 0);
        w::check_menu(self.menu.get(), CMD_PREVIEW, on);
        unsafe { ShowWindow(self.preview.host.get(), if on { SW_SHOW } else { SW_HIDE }) };
        self.layout();
        if on {
            self.preview.invalidate();
            self.update_preview();
        } else {
            self.preview.show(None, None, "");
        }
    }

    /// Updates the preview shortly (selection changes come in bursts).
    fn preview_soon(&self) {
        if self.preview_on() {
            unsafe { SetTimer(self.hwnd.get(), TIMER_PREVIEW, 80, None) };
        }
    }

    fn update_preview(&self) {
        if !self.preview_on() {
            return;
        }
        let row = if self.tree_mode() {
            self.selected_rows().into_iter().next()
        } else {
            let i = send(
                self.list.get(),
                LVM_GETNEXTITEM,
                usize::MAX,
                (LVNI_FOCUSED | LVNI_SELECTED) as isize,
            );
            let i = if i < 0 {
                send(
                    self.list.get(),
                    LVM_GETNEXTITEM,
                    usize::MAX,
                    LVNI_SELECTED as isize,
                )
            } else {
                i
            };
            if i < 0 { None } else { self.row(i as usize) }
        };
        let cq = if self.tree_mode() {
            None
        } else {
            self.content.borrow().clone()
        };
        self.preview.show(row.as_ref(), cq, &self.query_text());
    }

    // ---- folder-size tree -------------------------------------------------------

    /// Rebuilds the tree for the current query (keeping expanded folders when `keep`).
    fn build_tree(&self, keep: bool) {
        let seq = self.tree_seq.get() + 1;
        self.tree_seq.set(seq);
        if keep {
            let mut r = self.restore.borrow_mut();
            if r.is_empty() {
                *r = self.tree.borrow().expanded_paths();
            }
        } else {
            self.restore.borrow_mut().clear();
        }
        let (sort, desc) = self.tree_sort();
        self.stop_scan();
        // The tree counts names only: content terms are left out.
        let query = content::split_query(&self.query_text(), self.flags())
            .map(|(names, _)| names)
            .unwrap_or_else(|_| self.query_text());
        let _ = self.jobs.send(Job::Tree {
            seq,
            query,
            flags: self.flags(),
            sort,
            desc,
        });
        self.tree_gen.set(
            self.last_status
                .borrow()
                .as_ref()
                .map_or(0, |s| s.generation),
        );
        self.tree_at.set(Some(Instant::now()));
        if !self.tree_quiet.get() {
            self.set_status_left("Adding up folder sizes…");
        }
        let text = w::get_text(self.edit.get());
        w::set_text(
            self.hwnd.get(),
            &if text.is_empty() {
                "Folder sizes - Quickseek".to_string()
            } else {
                format!("{text} - folder sizes - Quickseek")
            },
        );
    }

    fn on_tree(&self, done: TreeDone) {
        if done.seq != self.tree_seq.get() || !self.tree_mode() {
            return;
        }
        let list = self.tree_list.get();
        match done.res {
            Ok(level) => {
                {
                    let mut m = self.tree.borrow_mut();
                    m.set_roots(level.size, level.files, level.rows);
                    m.cloud = level.cloud;
                    m.elapsed_ms = done.ms;
                    m.query = self.query_text();
                }
                let restore = std::mem::take(&mut *self.restore.borrow_mut());
                self.refresh_history();
                if restore.is_empty() {
                    // Open the volumes (when there are only a few) one level.
                    let n = self.tree.borrow().nodes.len();
                    if n <= 3 {
                        for i in (0..n).rev() {
                            self.expand(i);
                        }
                    }
                    if self.comparing() {
                        self.drill_growth();
                    }
                } else {
                    for p in &restore {
                        // Look up first: the borrow must end before expand() changes the tree.
                        let at = {
                            let m = self.tree.borrow();
                            m.find(p).filter(|&i| !m.nodes[i].expanded)
                        };
                        if let Some(i) = at {
                            self.expand(i);
                        }
                    }
                }
                let n = self.tree.borrow().nodes.len();
                send(
                    list,
                    LVM_SETITEMCOUNT,
                    n,
                    if restore.is_empty() {
                        0
                    } else {
                        (LVSICF_NOSCROLL | LVSICF_NOINVALIDATEALL) as isize
                    },
                );
                self.tree_quiet.set(false);
                let keep = std::mem::take(&mut *self.keep_sel.borrow_mut());
                let found: Vec<usize> = {
                    let m = self.tree.borrow();
                    keep.iter().filter_map(|p| m.find(p)).collect()
                };
                if !found.is_empty() {
                    self.select_tree_rows(&found);
                }
                if let Some(p) = self.reveal.borrow_mut().take() {
                    self.reveal_path(&p);
                }
                if self.comparing() {
                    self.load_tree_series();
                }
                unsafe { InvalidateRect(list, null(), 0) };
                self.update_status();
                self.growth_soon();
            }
            Err(e) => {
                self.tree.borrow_mut().nodes.clear();
                send(list, LVM_SETITEMCOUNT, 0, 0);
                self.set_status_left(&if e == quickseek_core::ipc::LOADING {
                    "Index is still loading…".to_string()
                } else {
                    e
                });
            }
        }
    }

    /// Fetches node `i`'s children and shows them below it.
    fn expand(&self, i: usize) {
        let Some(node) = self.tree.borrow().nodes.get(i).cloned() else {
            return;
        };
        if !node.expandable() || node.expanded {
            return;
        }
        let (sort, desc) = self.tree_sort();
        let Some(mut c) = self.conn.try_lock_for(Duration::from_secs(5)) else {
            return;
        };
        let res = c.be.children(&node.node_ref(), sort, desc, 0, 200_000);
        drop(c);
        match res {
            Ok(level) => {
                // A folder whose only content is one folder opens through to it.
                let drill = level.rows.len() == 1
                    && level.rows[0].is_dir
                    && level.rows[0].has_children
                    && node.depth < 32;
                self.tree.borrow_mut().expand(i, level.rows);
                if drill {
                    self.expand(i + 1);
                }
                if self.comparing() {
                    self.load_tree_series();
                }
            }
            Err(e) if e == "no tree open" => self.build_tree(true), // service restarted
            Err(e) => self.set_status_left(&e),
        }
    }

    fn toggle_node(&self, i: usize) {
        let Some(expanded) = self.tree.borrow().nodes.get(i).map(|n| n.expanded) else {
            return;
        };
        if expanded {
            self.tree.borrow_mut().collapse(i);
        } else {
            self.expand(i);
        }
        let n = self.tree.borrow().nodes.len();
        let list = self.tree_list.get();
        send(
            list,
            LVM_SETITEMCOUNT,
            n,
            (LVSICF_NOSCROLL | LVSICF_NOINVALIDATEALL) as isize,
        );
        unsafe { InvalidateRect(list, null(), 0) };
        self.update_status();
    }

    fn select_tree_row(&self, i: usize) {
        let list = self.tree_list.get();
        let mut it: LVITEMW = unsafe { std::mem::zeroed() };
        it.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
        send(list, LVM_SETITEMSTATE, usize::MAX, &it as *const _ as isize);
        it.state = LVIS_SELECTED | LVIS_FOCUSED;
        send(list, LVM_SETITEMSTATE, i, &it as *const _ as isize);
        send(list, LVM_ENSUREVISIBLE, i, 0);
    }

    /// Selects rows `rows` (the first focused) without scrolling, after a refresh.
    fn select_tree_rows(&self, rows: &[usize]) {
        let list = self.tree_list.get();
        let mut it: LVITEMW = unsafe { std::mem::zeroed() };
        it.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
        send(list, LVM_SETITEMSTATE, usize::MAX, &it as *const _ as isize);
        for (k, &i) in rows.iter().enumerate() {
            it.state = if k == 0 {
                LVIS_SELECTED | LVIS_FOCUSED
            } else {
                LVIS_SELECTED
            };
            send(list, LVM_SETITEMSTATE, i, &it as *const _ as isize);
        }
    }

    /// Expands the folders down to `path` and selects it.
    fn reveal_path(&self, path: &str) {
        let mut at: Option<usize> = None;
        loop {
            let next = {
                let m = self.tree.borrow();
                let range = match at {
                    None => 0..m.nodes.len(),
                    Some(i) => {
                        let d = m.nodes[i].depth;
                        let end = m.nodes[i + 1..]
                            .iter()
                            .position(|x| x.depth <= d)
                            .map_or(m.nodes.len(), |p| i + 1 + p);
                        i + 1..end
                    }
                };
                let want = at.map_or(0, |i| m.nodes[i].depth + 1);
                range
                    .into_iter()
                    .find(|&j| m.nodes[j].depth == want && tv::contains(&m.nodes[j].path(), path))
            };
            let Some(j) = next else { break };
            at = Some(j);
            if self.tree.borrow().find(path) == Some(j) {
                break;
            }
            self.expand(j);
        }
        let n = self.tree.borrow().nodes.len();
        send(
            self.tree_list.get(),
            LVM_SETITEMCOUNT,
            n,
            LVSICF_NOSCROLL as isize,
        );
        if let Some(j) = at {
            self.select_tree_row(j);
        }
    }

    pub fn set_tree_mode(&self, on: bool) {
        self.settings.borrow_mut().tree_mode = on;
        send(self.tree_btn.get(), BM_SETCHECK, on as usize, 0);
        w::check_menu(self.menu.get(), CMD_TREE, on);
        unsafe {
            ShowWindow(self.tree_list.get(), if on { SW_SHOW } else { SW_HIDE });
            ShowWindow(self.list.get(), if on { SW_HIDE } else { SW_SHOW });
        }
        if !on && self.comparing() {
            // Leaving the tree stops comparing (without rebuilding it).
            if let Some(mut c) = self.conn.try_lock_for(Duration::from_secs(2)) {
                let _ = c.be.compare("");
            }
            let prev = self.growth.borrow_mut().prev_sort.take();
            self.growth.borrow_mut().base = None;
            if let Some((k, d)) = prev {
                let mut s = self.settings.borrow_mut();
                (s.tree_sort, s.tree_descending) = (k, d);
            }
            self.show_growth_columns(false);
        }
        self.layout();
        if on {
            self.search(false);
        } else {
            self.tree.borrow_mut().nodes.clear();
            send(self.tree_list.get(), LVM_SETITEMCOUNT, 0, 0);
            let _ = self.jobs.send(Job::TreeClose);
            self.search(false);
        }
    }

    fn tree_selected(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut i: isize = -1;
        loop {
            i = send(
                self.tree_list.get(),
                LVM_GETNEXTITEM,
                i as usize,
                LVNI_SELECTED as isize,
            );
            if i < 0 || out.len() >= 10_000 {
                break;
            }
            out.push(i as usize);
        }
        out
    }

    fn tree_selected_paths(&self) -> Vec<String> {
        let m = self.tree.borrow();
        self.tree_selected()
            .into_iter()
            .filter_map(|i| m.nodes.get(i).map(tv::Node::path))
            .collect()
    }

    fn update_header_tree(&self) {
        let header = send(self.tree_list.get(), LVM_GETHEADER, 0, 0) as HWND;
        let (key, desc) = self.tree_sort();
        let col = match key {
            TreeSort::Name => 0,
            TreeSort::Size => 1,
            TreeSort::Files => 3,
            TreeSort::Modified => 5,
            TreeSort::Change => 6,
        };
        for c in 0..tv::COLUMNS.len() as i32 {
            let mut hd: HDITEMW = unsafe { std::mem::zeroed() };
            hd.mask = HDI_FORMAT;
            send(header, HDM_GETITEMW, c as usize, &mut hd as *mut _ as isize);
            hd.fmt &= !(HDF_SORTUP | HDF_SORTDOWN);
            if c == col {
                hd.fmt |= if desc { HDF_SORTDOWN } else { HDF_SORTUP };
            }
            send(header, HDM_SETITEMW, c as usize, &hd as *const _ as isize);
        }
    }

    fn tree_status(&self) -> String {
        let m = self.tree.borrow();
        let sel = self.tree_selected();
        let what = if m.query.trim().is_empty() {
            "indexed".to_string()
        } else {
            format!("matching \"{}\"", m.query.trim())
        };
        let cloud = if m.cloud > 0 {
            format!(" (+ {} online-only)", fmt::size(m.cloud))
        } else {
            String::new()
        };
        let mut s = format!(
            "{}{cloud} in {} files {what}  ·  {:.0} ms",
            fmt::size(m.size),
            shell::group(m.files),
            m.elapsed_ms
        );
        if let Some(b) = &self.growth.borrow().base {
            let roots: Vec<String> = m
                .nodes
                .iter()
                .filter(|n| n.depth == 0 && n.row.has_before)
                .map(|n| {
                    let free = if n.row.free_before > 0 {
                        format!(
                            ", free {} → {}",
                            fmt::size(n.row.free_before),
                            fmt::size(n.row.free)
                        )
                    } else {
                        String::new()
                    };
                    format!("{} {}{free}", n.row.name, tv::signed_size(n.row.change()))
                })
                .collect();
            s = format!(
                "Compared with {} ({}): {}  ·  Esc stops  ·  {s}",
                crate::growth::day(b.time, false),
                crate::growth::ago(b.time),
                roots.join("; ")
            );
        }
        if let [i] = sel[..]
            && let Some(n) = m.nodes.get(i)
        {
            s = format!(
                "{}: {} ({:.1} % of its folder), {} files  ·  {s}",
                n.path(),
                fmt::size(n.row.size),
                n.fraction() * 100.0,
                shell::group(n.row.files)
            );
        }
        s
    }

    fn tree_custom_draw(&self, cd: &mut NMLVCUSTOMDRAW) -> LRESULT {
        let stage = cd.nmcd.dwDrawStage;
        if stage == CDDS_PREPAINT {
            return CDRF_NOTIFYITEMDRAW as LRESULT;
        }
        if stage == CDDS_ITEMPREPAINT {
            return CDRF_NOTIFYSUBITEMDRAW as LRESULT;
        }
        if stage != (CDDS_ITEMPREPAINT | CDDS_SUBITEM) {
            return CDRF_DODEFAULT as LRESULT;
        }
        if !matches!(cd.iSubItem, 0 | 2 | tv::COL_CHANGE | tv::COL_TREND) {
            // Default-drawn columns: grey while being deleted.
            let dim = self
                .tree
                .borrow()
                .nodes
                .get(cd.nmcd.dwItemSpec)
                .is_some_and(|n| n.deleting);
            cd.clrText = if dim {
                theme::p().muted
            } else {
                theme::p().text
            };
            return CDRF_NEWFONT as LRESULT;
        }
        let i = cd.nmcd.dwItemSpec;
        let Some(node) = self.tree.borrow().nodes.get(i).cloned() else {
            return CDRF_DODEFAULT as LRESULT;
        };
        let list = self.tree_list.get();
        let rc = tv::cell_rect(list, i, cd.iSubItem);
        // A hidden (zero-width) or nearly hidden column: nothing to draw.
        if rc.right - rc.left < px(list, 8) {
            return CDRF_SKIPDEFAULT as LRESULT;
        }
        let selected =
            send(list, LVM_GETITEMSTATE, i, LVIS_SELECTED as isize) as u32 & LVIS_SELECTED != 0;
        let focused = unsafe { GetFocus() } == list;
        let p = tv::Paint {
            hdc: cd.nmcd.hdc,
            font: self.font.get(),
            bold: self.bold.get(),
            selected,
            hot: is_hot(list, i, cd),
            classic_sel: selected && focused && !crate::themed(),
            dim: node.deleting,
        };
        // Everything stays inside its cell (sparklines, bars, long names).
        let saved = unsafe { SaveDC(p.hdc) };
        unsafe { IntersectClipRect(p.hdc, rc.left, rc.top, rc.right, rc.bottom) };
        if cd.iSubItem == 0 {
            let icon = self
                .files
                .borrow_mut()
                .get(&node.row.name, node.row.is_dir)
                .0;
            let il = self.files.borrow().image_list;
            tv::draw_name(list, &p, &node, rc, icon, il);
        } else if cd.iSubItem == tv::COL_CHANGE {
            tv::draw_change(list, &p, &node, rc);
        } else if cd.iSubItem == tv::COL_TREND {
            let g = self.growth.borrow();
            tv::draw_trend(
                list,
                &p,
                &node,
                rc,
                if node.row.is_dir {
                    g.series
                        .get(&crate::growth::key(&node.path()))
                        .map(|v| v.as_slice())
                } else {
                    None
                },
            );
        } else {
            tv::draw_bar(list, &p, &node, rc);
        }
        unsafe { RestoreDC(p.hdc, saved) };
        CDRF_SKIPDEFAULT as LRESULT
    }

    // ---- deleting ----------------------------------------------------------------

    /// Deletes what's selected in the view showing (Recycle Bin unless `permanent`).
    fn delete_selection(&self, permanent: bool) {
        if self.tree_mode() {
            self.delete_paths(self.tree_selected_paths(), 0, permanent);
        } else {
            let rows = self.selected_rows();
            self.delete_paths(
                rows.iter().map(|r| r.full_path()).collect(),
                rows.iter().map(|r| r.size).sum(),
                permanent,
            );
        }
    }

    /// Deletes `paths` on a thread of its own: the window stays usable, the
    /// rows turn grey and the tree takes their size off at once, and both
    /// views refresh when it's done. `bytes` is the size when not in the tree.
    fn delete_paths(&self, paths: Vec<String>, bytes: u64, permanent: bool) {
        if paths.is_empty() {
            return;
        }
        if let Some(p) = paths.iter().find(|p| is_root(p)) {
            self.set_status_left(&format!(
                "{p} is a whole drive or share: Quickseek doesn't delete those."
            ));
            return;
        }
        {
            let mut d = self.deleting.borrow_mut();
            d.extend(paths.iter().map(|p| delete_key(p)));
        }
        let freed = if self.tree_mode() {
            self.tree.borrow_mut().mark_deleting(&paths)
        } else {
            bytes
        };
        unsafe {
            InvalidateRect(self.tree_list.get(), null(), 0);
            InvalidateRect(self.list.get(), null(), 0);
        }
        self.deletes_running.set(self.deletes_running.get() + 1);
        let what = items_label(&paths);
        let size = if freed > 0 {
            format!(" ({})", fmt::size(freed))
        } else {
            String::new()
        };
        let note = if permanent {
            format!("Deleting {what}{size}…")
        } else {
            format!("Moving {what}{size} to the Recycle Bin…")
        };
        *self.delete_note.borrow_mut() = Some((note, None));
        self.update_status();
        let hwnd = self.hwnd.get() as isize;
        shell::delete_async(paths, permanent, self.test_mode, move |d| unsafe {
            PostMessageW(
                hwnd as HWND,
                WM_APP_DELETED,
                0,
                Box::into_raw(Box::new(d)) as isize,
            );
        });
    }

    fn on_deleted(&self, d: shell::DeleteDone) {
        self.deletes_running
            .set(self.deletes_running.get().saturating_sub(1));
        {
            let mut set = self.deleting.borrow_mut();
            for p in &d.paths {
                set.remove(&delete_key(p));
            }
        }
        let what = items_label(&d.paths);
        let note = match (&d.error, d.aborted, d.permanent) {
            (Some(e), _, _) => format!("Couldn't delete {what}: {}", e.trim()),
            (None, true, _) => format!("Deleting {what} was cancelled or skipped some items"),
            (None, false, true) => format!("Deleted {what}"),
            (None, false, false) => {
                format!("Moved {what} to the Recycle Bin (the space comes back when it's emptied)")
            }
        };
        *self.delete_note.borrow_mut() =
            Some((note, Some(Instant::now() + Duration::from_secs(10))));
        if self.tree_mode() {
            self.refresh_tree_quietly();
        } else {
            self.search(true);
        }
        unsafe { InvalidateRect(self.list.get(), null(), 0) };
        self.update_status();
    }

    /// Whether row `path` of either view is being deleted.
    fn is_deleting(&self, path: &str) -> bool {
        let d = self.deleting.borrow();
        !d.is_empty() && d.contains(&delete_key(path))
    }

    /// Rebuilds the tree in place: open folders, the selection and the
    /// scroll position stay.
    fn refresh_tree_quietly(&self) {
        self.tree_quiet.set(true);
        *self.keep_sel.borrow_mut() = self.tree_selected_paths();
        self.build_tree(true);
    }

    /// Live tree: rebuilds after the index changed (from any program), at
    /// most every few seconds, not while deleting, not while the user is in
    /// the middle of clicking or scrolling, and not while hidden.
    fn maybe_refresh_tree(&self, generation: u64) {
        if generation == self.tree_gen.get()
            || self.deletes_running.get() > 0
            || self.tree_quiet.get()
        {
            return;
        }
        if self
            .tree_at
            .get()
            .is_some_and(|t| t.elapsed() < Duration::from_secs(if self.test_mode { 1 } else { 5 }))
        {
            return;
        }
        let hwnd = self.hwnd.get();
        if unsafe { IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 } {
            return;
        }
        let mut li = windows_sys::Win32::UI::Input::KeyboardAndMouse::LASTINPUTINFO {
            cbSize: size_of::<windows_sys::Win32::UI::Input::KeyboardAndMouse::LASTINPUTINFO>()
                as u32,
            dwTime: 0,
        };
        let now = unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount() };
        if unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetLastInputInfo(&mut li) }
            != 0
            && now.wrapping_sub(li.dwTime) < 1200
            && !self.test_mode
        {
            return;
        }
        self.refresh_tree_quietly();
    }

    fn tree_context_menu(&self, x: i32, y: i32) {
        let paths = self.tree_selected_paths();
        if paths.is_empty() {
            return;
        }
        let single_dir = {
            let m = self.tree.borrow();
            let sel = self.tree_selected();
            (sel.len() == 1)
                .then(|| m.nodes[sel[0]].clone())
                .filter(|n| n.row.is_dir)
        };
        let mut own: Vec<(u32, &str)> = Vec::new();
        if let Some(n) = &single_dir {
            if n.expandable() {
                own.push(if n.expanded {
                    (CMD_COLLAPSE, "&Collapse")
                } else {
                    (CMD_EXPAND, "&Expand")
                });
            }
            own.push((CMD_SEARCH_HERE, "&Search in This Folder"));
            own.push((0, ""));
        }
        own.extend([
            (CMD_OPEN, "&Open"),
            (CMD_OPEN_PATH, "Open &Path"),
            (0, ""),
            (CMD_COPY_FULL, "Copy &Full Path"),
            (CMD_COPY_NAME, "Copy &Name"),
            (0, ""),
            (CMD_PROPERTIES, "P&roperties"),
        ]);
        match shell::context_menu(self.hwnd.get(), &paths, &own, x, y) {
            MenuChoice::Own(id) => self.command(id),
            MenuChoice::Shell => self.refresh_tree_quietly(), // e.g. renamed: show it
            MenuChoice::Delete { permanent } => self.delete_paths(paths, 0, permanent),
            MenuChoice::None => {}
        }
    }

    fn tree_notify(&self, nm: &NMHDR, lparam: LPARAM) -> Option<LRESULT> {
        match nm.code {
            LVN_GETDISPINFOW => {
                let di = unsafe { &mut *(lparam as *mut NMLVDISPINFOW) };
                let item = &mut di.item;
                let Some(node) = self.tree.borrow().nodes.get(item.iItem as usize).cloned() else {
                    return Some(0);
                };
                if item.mask & LVIF_TEXT != 0 {
                    let text = tv::cell_text(&node, item.iSubItem);
                    let mut buf = self.disp.borrow_mut();
                    buf.clear();
                    buf.extend(
                        text.encode_utf16()
                            .take(item.cchTextMax.max(1) as usize - 1),
                    );
                    buf.push(0);
                    item.pszText = buf.as_mut_ptr();
                }
                Some(0)
            }
            NM_CLICK => {
                let ia = unsafe { &*(lparam as *const NMITEMACTIVATE) };
                if ia.iItem >= 0 {
                    let i = ia.iItem as usize;
                    let depth = self.tree.borrow().nodes.get(i).map(|n| n.depth);
                    if let Some(d) = depth {
                        let rc = tv::cell_rect(self.tree_list.get(), i, 0);
                        let (a, b) = tv::glyph_span(self.tree_list.get(), &rc, d);
                        if ia.ptAction.x >= a && ia.ptAction.x < b {
                            self.glyph_click.set(unsafe {
                                windows_sys::Win32::System::SystemInformation::GetTickCount()
                            });
                            self.toggle_node(i);
                        }
                    }
                }
                Some(0)
            }
            LVN_ITEMACTIVATE => {
                let ia = unsafe { &*(lparam as *const NMITEMACTIVATE) };
                let now = unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount() };
                if now.wrapping_sub(self.glyph_click.get()) < 600 {
                    return Some(0); // second click of a +/− double-click
                }
                let i = if ia.iItem >= 0 {
                    ia.iItem as usize
                } else {
                    self.tree_selected().first().copied().unwrap_or(usize::MAX)
                };
                let node = self.tree.borrow().nodes.get(i).cloned();
                match node {
                    Some(n) if n.row.is_dir => self.toggle_node(i),
                    Some(n) if shell::open(self.hwnd.get(), &n.path()) => {
                        let _ = self.jobs.send(Job::Run(n.path()));
                    }
                    Some(_) => {}
                    None => {}
                }
                Some(0)
            }
            LVN_KEYDOWN => {
                let k = unsafe { &*(lparam as *const NMLVKEYDOWN) };
                let ctrl = unsafe { GetKeyState(VK_CONTROL as i32) } < 0;
                let sel = self.tree_selected();
                let cur = sel.first().copied();
                let vk = k.wVKey;
                match vk {
                    0x2E /* Delete */ => self.delete_selection(unsafe { GetKeyState(VK_SHIFT as i32) } < 0),
                    0x27 /* Right */ => {
                        if let Some(i) = cur {
                            let (expanded, expandable) = self.tree.borrow().nodes.get(i).map_or((false, false), |n| (n.expanded, n.expandable()));
                            if expandable && !expanded {
                                self.toggle_node(i);
                            } else if expanded {
                                self.select_tree_row(i + 1);
                            }
                        }
                    }
                    0x25 /* Left */ => {
                        if let Some(i) = cur {
                            let expanded = self.tree.borrow().nodes.get(i).is_some_and(|n| n.expanded);
                            if expanded {
                                self.toggle_node(i);
                            } else if let Some(p) = { self.tree.borrow().parent_of(i) } {
                                self.select_tree_row(p);
                            }
                        }
                    }
                    0x1B /* Esc */ if self.comparing() => self.set_compare(None),
                    k if ctrl && k == b'C' as u16 => {
                        let paths = self.tree_selected_paths();
                        if !paths.is_empty() {
                            shell::copy_text(self.hwnd.get(), &paths.join("\r\n"));
                        }
                    }
                    _ => {}
                }
                Some(0)
            }
            LVN_COLUMNCLICK => {
                let lv = unsafe { &*(lparam as *const NMLISTVIEW) };
                if let Some(key) = tv::column_sort(lv.iSubItem) {
                    {
                        let mut s = self.settings.borrow_mut();
                        if tv::sort_from(&s.tree_sort) == key {
                            s.tree_descending = !s.tree_descending;
                        } else {
                            s.tree_sort = tv::sort_name(key).into();
                            s.tree_descending = key != TreeSort::Name;
                        }
                    }
                    self.update_header_tree();
                    self.build_tree(true);
                }
                Some(0)
            }
            LVN_ITEMCHANGED => {
                self.update_status();
                self.preview_soon();
                self.growth_soon();
                Some(0)
            }
            LVN_BEGINDRAG => {
                shell::drag_out(self.hwnd.get(), &self.tree_selected_paths());
                Some(0)
            }
            NM_RCLICK => {
                let mut pt = POINT { x: 0, y: 0 };
                unsafe { GetCursorPos(&mut pt) };
                self.tree_context_menu(pt.x, pt.y);
                Some(0)
            }
            NM_CUSTOMDRAW => {
                Some(self.tree_custom_draw(unsafe { &mut *(lparam as *mut NMLVCUSTOMDRAW) }))
            }
            _ => None,
        }
    }

    // ---- folder growth -----------------------------------------------------------

    pub fn ui_font(&self) -> HFONT {
        self.font.get()
    }

    pub fn ui_bold(&self) -> HFONT {
        self.bold.get()
    }

    pub fn comparing(&self) -> bool {
        self.growth.borrow().base.is_some()
    }

    fn growth_soon(&self) {
        if self.tree_mode() {
            unsafe { SetTimer(self.hwnd.get(), TIMER_GROWTH, 120, None) };
        }
    }

    /// Re-reads the snapshot list (cheap); forgets cached series if it changed.
    fn refresh_history(&self) {
        let list = self
            .conn
            .try_lock_for(Duration::from_secs(5))
            .and_then(|mut c| c.be.history().ok());
        let mut g = self.growth.borrow_mut();
        if let Some(l) = list
            && l != g.snaps
        {
            g.snaps = l;
            g.series.clear();
            g.free.clear();
        }
    }

    /// Makes sure these folders' size series are cached.
    fn ensure_series(&self, paths: &[String]) {
        let missing: Vec<String> = {
            let g = self.growth.borrow();
            if g.snaps.is_empty() {
                return;
            }
            let mut seen = std::collections::HashSet::new();
            paths
                .iter()
                .filter(|p| {
                    !g.series.contains_key(&crate::growth::key(p))
                        && seen.insert(crate::growth::key(p))
                })
                .cloned()
                .collect()
        };
        if missing.is_empty() {
            return;
        }
        let Some(reply) = self
            .conn
            .try_lock_for(Duration::from_secs(5))
            .and_then(|mut c| c.be.series(&missing).ok())
        else {
            return;
        };
        let mut g = self.growth.borrow_mut();
        if reply.snaps != g.snaps {
            g.snaps = reply.snaps.clone();
            g.series.clear();
            g.free.clear();
        }
        for r in reply.rows {
            let k = crate::growth::key(&r.path);
            if r.free.iter().any(|&f| f != u64::MAX) {
                g.free.insert(k.clone(), r.free);
            }
            g.series.insert(k, r.sizes);
        }
    }

    /// Series for every folder in the tree (for the Trend column).
    fn load_tree_series(&self) {
        let paths: Vec<String> = self
            .tree
            .borrow()
            .nodes
            .iter()
            .filter(|n| n.row.is_dir)
            .map(tv::Node::path)
            .collect();
        self.ensure_series(&paths);
        unsafe { InvalidateRect(self.tree_list.get(), null(), 0) };
    }

    fn show_growth_columns(&self, on: bool) {
        self.show_growth_columns_raw(on);
        self.stretch_last(1);
    }

    fn show_growth_columns_raw(&self, on: bool) {
        let (tl, hwnd) = (self.tree_list.get(), self.hwnd.get());
        for c in [tv::COL_CHANGE, tv::COL_TREND] {
            let w = tv::COLUMNS[c as usize].1;
            send(
                tl,
                LVM_SETCOLUMNWIDTH,
                c as usize,
                if on { px(hwnd, w) } else { 0 } as isize,
            );
        }
    }

    /// Compares the tree with `snap` (`None`: stop). Sorts by change while comparing.
    pub fn set_compare(&self, snap: Option<SnapInfo>) {
        let id = snap.as_ref().map_or(String::new(), |s| s.id.clone());
        match self
            .conn
            .try_lock_for(Duration::from_secs(5))
            .map(|mut c| c.be.compare(&id))
        {
            Some(Err(e)) => {
                self.set_status_left(&e);
                return;
            }
            None => return,
            Some(Ok(())) => {}
        }
        let was = self.comparing();
        {
            let mut g = self.growth.borrow_mut();
            if !was && snap.is_some() {
                let s = self.settings.borrow();
                g.prev_sort = Some((s.tree_sort.clone(), s.tree_descending));
            }
            g.base = snap;
            g.pane = None;
        }
        let now = self.comparing();
        if now && !was {
            let mut s = self.settings.borrow_mut();
            s.tree_sort = "change".into();
            s.tree_descending = true;
        } else if !now
            && was
            && let Some((k, d)) = self.growth.borrow_mut().prev_sort.take()
        {
            let mut s = self.settings.borrow_mut();
            (s.tree_sort, s.tree_descending) = (k, d);
        }
        self.show_growth_columns(now);
        self.update_header_tree();
        self.layout();
        self.build_tree(false);
    }

    /// Opens the path that grew most, while one child holds most of the growth.
    fn drill_growth(&self) {
        let roots: Vec<usize> = {
            self.tree
                .borrow()
                .nodes
                .iter()
                .enumerate()
                .filter(|(_, n)| n.depth == 0)
                .map(|(i, _)| i)
                .collect()
        };
        // The root that grew most (after expanding, indices shift: find by path).
        let best_root = {
            let m = self.tree.borrow();
            roots
                .iter()
                .copied()
                .max_by_key(|&i| m.nodes[i].row.change())
                .map(|i| m.nodes[i].path())
        };
        let Some(mut path) = best_root else { return };
        for _ in 0..12 {
            let next = {
                let m = self.tree.borrow();
                let Some(i) = m.find(&path) else { break };
                let n = &m.nodes[i];
                if !n.expanded {
                    let expandable = n.expandable();
                    drop(m);
                    if !expandable {
                        break;
                    }
                    self.expand(i);
                    continue;
                }
                let d = n.depth;
                let kids: Vec<&tv::Node> = m.nodes[i + 1..]
                    .iter()
                    .take_while(|x| x.depth > d)
                    .filter(|x| x.depth == d + 1)
                    .collect();
                let total = n.row.change();
                kids.iter()
                    .max_by_key(|k| k.row.change())
                    .filter(|k| k.row.is_dir && k.row.change() > 0 && k.row.change() * 2 >= total)
                    .map(|k| k.path())
            };
            match next {
                Some(p) => path = p,
                None => break,
            }
        }
        if let Some(i) = self.tree.borrow().find(&path) {
            let n = self.tree.borrow().nodes.len();
            send(self.tree_list.get(), LVM_SETITEMCOUNT, n, 0);
            self.select_tree_row(i);
        }
    }

    /// A click on the timeline: compare with that day.
    pub fn strip_click(&self, x: i32) {
        let pick = {
            let g = self.growth.borrow();
            let Some(d) = g.strip.as_ref() else { return };
            let rc = w::client_rect(self.strip.get());
            let pr = crate::growth::plot_rect(self.strip.get(), &rc);
            crate::growth::nearest(&d.times, x, pr.left, pr.right)
                .filter(|&i| i + 1 < d.times.len())
                .and_then(|i| g.snaps.get(i).cloned())
        };
        if let Some(s) = pick {
            self.set_compare(Some(s));
        }
    }

    /// The folder the strip and pane are about: the selection, else the whole tree.
    fn growth_scope(&self) -> Option<tv::Node> {
        let sel = self.tree_selected();
        let m = self.tree.borrow();
        sel.first()
            .and_then(|&i| m.nodes.get(i))
            .filter(|n| n.row.is_dir)
            .cloned()
    }

    /// Children of a node: from the tree if it's open, else asked for.
    fn growth_children(&self, n: &tv::Node, sort: TreeSort) -> Vec<quickseek_core::tree::TreeRow> {
        {
            let m = self.tree.borrow();
            if let Some(i) = m.find(&n.path())
                && m.nodes[i].expanded
            {
                let d = m.nodes[i].depth;
                return m.nodes[i + 1..]
                    .iter()
                    .take_while(|x| x.depth > d)
                    .filter(|x| x.depth == d + 1)
                    .map(|x| x.row.clone())
                    .collect();
            }
        }
        self.conn
            .try_lock_for(Duration::from_secs(5))
            .and_then(|mut c| c.be.children(&n.node_ref(), sort, true, 0, 40).ok())
            .map(|l| l.rows)
            .unwrap_or_default()
    }

    /// Rebuilds what the strip and the pane show for the current selection.
    fn rebuild_growth(&self) {
        if !self.tree_mode() {
            return;
        }
        let scope = self.growth_scope();
        let comparing = self.comparing();
        // Layers: the scope's biggest children (by change when comparing), or the volumes.
        let (title, scope_path, kids, capacity, root_path): (
            String,
            Option<String>,
            Vec<quickseek_core::tree::TreeRow>,
            u64,
            Option<String>,
        ) = match &scope {
            Some(n) => {
                let mut kids: Vec<_> = self
                    .growth_children(
                        n,
                        if comparing {
                            TreeSort::Change
                        } else {
                            TreeSort::Size
                        },
                    )
                    .into_iter()
                    .filter(|r| r.is_dir)
                    .collect();
                if comparing {
                    kids.sort_by_key(|r| std::cmp::Reverse(r.change().unsigned_abs()));
                } else {
                    kids.sort_by_key(|r| std::cmp::Reverse(r.size));
                }
                kids.truncate(4);
                let root = (n.depth == 0 && n.row.capacity > 0).then(|| n.path());
                (
                    n.row.name.clone(),
                    Some(n.path()),
                    kids,
                    if n.depth == 0 { n.row.capacity } else { 0 },
                    root,
                )
            }
            None => {
                let m = self.tree.borrow();
                let mut roots: Vec<_> = m
                    .nodes
                    .iter()
                    .filter(|n| n.depth == 0)
                    .map(|n| n.row.clone())
                    .collect();
                roots.sort_by_key(|r| std::cmp::Reverse(r.size));
                (
                    "All drives".into(),
                    None,
                    roots.into_iter().take(4).collect(),
                    0,
                    None,
                )
            }
        };
        // The pane is about the selection, else the first drive.
        let pane_node = if comparing {
            scope
                .clone()
                .or_else(|| self.tree.borrow().nodes.first().cloned())
        } else {
            None
        };
        let mut paths: Vec<String> = kids.iter().map(|r| r.full_path()).collect();
        paths.extend(scope_path.iter().cloned());
        paths.extend(pane_node.iter().map(tv::Node::path));
        self.ensure_series(&paths);
        let strip = {
            let g = self.growth.borrow();
            let now = quickseek_core::history::now_unix();
            let mut times: Vec<i64> = g.snaps.iter().map(|s| s.time).collect();
            times.push(now);
            let at = |v: u64| if v == u64::MAX { 0 } else { v };
            let series_of = |p: &str, now_v: u64| -> Vec<u64> {
                let mut v: Vec<u64> = g
                    .series
                    .get(&crate::growth::key(p))
                    .map(|s| s.iter().map(|&x| at(x)).collect())
                    .unwrap_or_else(|| vec![0; g.snaps.len()]);
                v.resize(g.snaps.len(), 0);
                v.push(now_v);
                v
            };
            let mut layers: Vec<crate::growth::Layer> = kids
                .iter()
                .map(|r| crate::growth::Layer {
                    label: r.name.clone(),
                    values: series_of(&r.full_path(), r.size),
                })
                .collect();
            let scope_vals = match (&scope, &scope_path) {
                (Some(n), Some(p)) => series_of(p, n.row.size),
                _ => (0..times.len())
                    .map(|i| layers.iter().map(|l| l.values[i]).sum())
                    .collect(),
            };
            let summed: Vec<u64> = (0..times.len())
                .map(|i| layers.iter().map(|l| l.values[i]).sum())
                .collect();
            let other: Vec<u64> = scope_vals
                .iter()
                .zip(&summed)
                .map(|(a, b)| a.saturating_sub(*b))
                .collect();
            if other.iter().any(|&v| v > 0) || layers.is_empty() {
                layers.push(crate::growth::Layer {
                    label: "everything else".into(),
                    values: other,
                });
            } else {
                // Keep the "everything else" slot so colors stay put.
                layers.push(crate::growth::Layer {
                    label: "everything else".into(),
                    values: vec![0; times.len()],
                });
            }
            let free = match (&root_path, &scope) {
                (Some(p), Some(n)) => {
                    let mut f = g
                        .free
                        .get(&crate::growth::key(p))
                        .cloned()
                        .unwrap_or_default();
                    f.resize(g.snaps.len(), u64::MAX);
                    f.push(n.row.free);
                    f
                }
                _ => Vec::new(),
            };
            let base = g
                .base
                .as_ref()
                .and_then(|b| g.snaps.iter().position(|s| s.id == b.id));
            let save_points = g
                .snaps
                .iter()
                .enumerate()
                .filter(|(_, s)| s.is_save_point())
                .map(|(i, _)| i)
                .collect();
            let size_now = *scope_vals.last().unwrap_or(&0);
            let change = base.map(|b| size_now as i64 - scope_vals[b] as i64);
            let mut t = format!("{}  {}", title.trim_end_matches('\\'), fmt::size(size_now));
            if let Some(c) = change {
                t.push_str(&format!(" ({})", tv::signed_size(c)));
            }
            crate::growth::StripData {
                title: t,
                times,
                layers,
                capacity,
                free,
                base,
                save_points,
            }
        };
        let pane = pane_node.map(|n| {
            let mut d = self.pane_for(&n);
            d.days = {
                let g = self.growth.borrow();
                let mut times: Vec<i64> = g.snaps.iter().map(|s| s.time).collect();
                times.push(quickseek_core::history::now_unix());
                let mut vals = g
                    .series
                    .get(&crate::growth::key(&n.path()))
                    .cloned()
                    .unwrap_or_default();
                vals.resize(g.snaps.len(), u64::MAX);
                vals.push(n.row.size);
                crate::growth::daily(&times, &vals)
            };
            if self.growth.borrow().tab == crate::growth::TAB_MAP {
                d.map = self.map_for(&n);
            }
            d
        });
        {
            let mut g = self.growth.borrow_mut();
            g.strip = Some(strip);
            g.pane = pane;
        }
        unsafe {
            InvalidateRect(self.strip.get(), null(), 0);
            InvalidateRect(self.gpane.get(), null(), 0);
        }
    }

    /// One level of the treemap under `n`: its biggest entries, then one box
    /// for the rest (or for what was there then and is gone).
    fn map_level(
        &self,
        n: &tv::Node,
        max: usize,
    ) -> Vec<(crate::growth::MapItem, quickseek_core::tree::TreeRow)> {
        use crate::growth::MapItem;
        let rows = self.growth_children(n, TreeSort::Size);
        let mut out: Vec<(MapItem, quickseek_core::tree::TreeRow)> = rows
            .into_iter()
            .map(|r| {
                (
                    MapItem {
                        name: r.name.clone(),
                        path: r.full_path(),
                        size: r.size,
                        before: r.before,
                        kids: Vec::new(),
                    },
                    r,
                )
            })
            .filter(|(i, _)| i.area() > 0)
            .collect();
        out.sort_by_key(|(i, _)| std::cmp::Reverse(i.area()));
        out.truncate(max);
        let (now_sum, before_sum) = out
            .iter()
            .fold((0u64, 0u64), |(a, b), (i, _)| (a + i.size, b + i.before));
        let rest = MapItem {
            name: String::new(),
            path: String::new(),
            size: n.row.size.saturating_sub(now_sum),
            before: n.row.before.saturating_sub(before_sum),
            kids: Vec::new(),
        };
        // Below 1 % it would only be a sliver.
        if rest.area() * 100 > n.row.size.max(n.row.before) {
            let name = if rest.size == 0 {
                "deleted or moved".to_string()
            } else {
                "smaller items".to_string()
            };
            out.push((
                MapItem { name, ..rest },
                quickseek_core::tree::TreeRow::default(),
            ));
        }
        out
    }

    /// The treemap of `n`: its children, and the biggest folders' children.
    fn map_for(&self, n: &tv::Node) -> Vec<crate::growth::MapItem> {
        let mut top = self.map_level(n, 30);
        for (item, row) in top.iter_mut().take(10) {
            if row.is_dir && row.has_children {
                let child = tv::Node {
                    row: row.clone(),
                    depth: n.depth + 1,
                    expanded: false,
                    parent_size: 0,
                    deleting: false,
                };
                item.kids = self
                    .map_level(&child, 16)
                    .into_iter()
                    .map(|(i, _)| i)
                    .collect();
            }
        }
        top.into_iter().map(|(i, _)| i).collect()
    }

    pub fn set_growth_tab(&self, t: u8) {
        self.growth.borrow_mut().tab = t;
        self.rebuild_growth();
    }

    /// Map click: open that folder in the tree (the pane follows the selection).
    pub fn map_open(&self, path: &str) {
        self.reveal_path(path);
        self.rebuild_growth();
    }

    /// Map right-click: select the parent of what the pane shows.
    pub fn map_up(&self) {
        let parent = {
            let m = self.tree.borrow();
            let Some(&i) = self.tree_selected().first() else {
                return;
            };
            let Some(d) = m.nodes.get(i).map(|n| n.depth).filter(|&d| d > 0) else {
                return;
            };
            (0..i).rev().find(|&j| m.nodes[j].depth == d - 1)
        };
        if let Some(j) = parent {
            self.select_tree_row(j);
            self.rebuild_growth();
        }
    }

    /// Calendar click: compare with the snapshot nearest that day's start.
    pub fn compare_day(&self, day: i64) {
        let pick = self
            .growth
            .borrow()
            .snaps
            .iter()
            .min_by_key(|s| (s.time - day).abs())
            .cloned();
        if let Some(s) = pick {
            self.set_compare(Some(s));
        }
    }

    fn pane_for(&self, n: &tv::Node) -> crate::growth::PaneData {
        let base = self.growth.borrow().base.clone();
        let since = base.as_ref().map_or(String::new(), |b| {
            format!(
                "{} ({})",
                crate::growth::day(b.time, false),
                crate::growth::ago(b.time)
            )
        });
        let mut kids = self.growth_children(n, TreeSort::Change);
        kids.sort_by_key(|r| std::cmp::Reverse(r.change().unsigned_abs()));
        let mut parts: Vec<(String, i64)> = kids
            .iter()
            .filter(|r| r.change() != 0)
            .take(5)
            .map(|r| (r.name.clone(), r.change()))
            .collect();
        let rest: i64 = (n.row.change()) - parts.iter().map(|p| p.1).sum::<i64>();
        if rest != 0 {
            parts.push(("everything else".into(), rest));
        }
        let free = (n.depth == 0 && n.row.capacity > 0 && n.row.free_before > 0)
            .then_some((n.row.free_before, n.row.free));
        // The chain of biggest growers, a few levels down.
        let mut chain = Vec::new();
        let mut cur = n.clone();
        for _ in 0..7 {
            let kids = self.growth_children(&cur, TreeSort::Change);
            let Some(best) = kids
                .into_iter()
                .filter(|r| r.change() > 0)
                .max_by_key(|r| r.change())
            else {
                break;
            };
            if best.change() * 5 < cur.row.change().max(1) {
                break;
            }
            chain.push((best.name.clone(), best.change(), best.size));
            if !best.is_dir || !best.has_children {
                break;
            }
            cur = tv::Node {
                row: best,
                depth: cur.depth + 1,
                expanded: false,
                parent_size: 0,
                deleting: false,
            };
        }
        crate::growth::PaneData {
            title: n.path(),
            since,
            before: n.row.before,
            now: n.row.size,
            parts,
            free,
            chain,
            ..Default::default()
        }
    }

    fn set_status_left(&self, s: &str) {
        // "service connection: Reconnecting…" reads as just that.
        let s = if s.contains(ipc::RECONNECTING) {
            ipc::RECONNECTING
        } else {
            s
        };
        let w = wide(s);
        send(self.status.get(), SB_SETTEXTW, 0, w.as_ptr() as isize);
    }

    /// The delete note, while it's current.
    fn note(&self) -> Option<String> {
        let mut n = self.delete_note.borrow_mut();
        if n.as_ref()
            .is_some_and(|(_, until)| until.is_some_and(|t| Instant::now() > t))
        {
            *n = None;
        }
        n.as_ref().map(|(s, _)| s.clone())
    }

    fn update_status(&self) {
        let note = self.note().map(|n| format!("{n}  ·  ")).unwrap_or_default();
        if self.tree_mode() {
            let s = self.tree_status();
            self.set_status_left(&format!("{note}{s}"));
            return;
        }
        let r = self.results.borrow();
        let selected = send(self.list.get(), LVM_GETSELECTEDCOUNT, 0, 0) as usize;
        let mut s = format!("{} objects", shell::group(r.total as u64));
        if selected > 0 {
            s = format!("{} of {s} selected", shell::group(selected as u64));
        }
        match &*self.scan_stats.borrow() {
            Some(st) if !st.done => s.push_str(&format!(
                "  ·  searching contents: {} of {} files, {} found  ·  Esc stops",
                shell::group(st.checked),
                shell::group(st.total),
                shell::group(st.matched)
            )),
            Some(st) => {
                s.push_str(&format!(
                    "  ·  {} files read in {:.1} s",
                    shell::group(st.checked),
                    st.elapsed.as_secs_f64()
                ));
                if st.cancelled {
                    s.push_str(&format!(
                        " (stopped: {} not read and still listed)",
                        shell::group(st.total.saturating_sub(st.checked))
                    ));
                }
                let skipped = st.skipped();
                if !skipped.is_empty() {
                    s.push_str(&format!("; skipped {skipped}"));
                }
            }
            None if !w::get_text(self.edit.get()).is_empty()
                || self.settings.borrow().filter != 0 =>
            {
                s.push_str(&format!("  ·  {:.1} ms", r.elapsed_ms))
            }
            None => {}
        }
        drop(r);
        match &*self.search_error.borrow() {
            Some(e) => self.set_status_left(e),
            None => self.set_status_left(&s),
        }
        let right = {
            let conn_label = self.conn.try_lock().map(|c| c.be.label()).unwrap_or("…");
            let mut parts = vec![conn_label.to_string()];
            if let Some(st) = &*self.last_status.borrow() {
                for v in st.volumes.iter().filter(|v| v.state != "ready") {
                    parts.push(format!("{}: {}", v.key, v.state));
                }
            }
            parts.join("  ·  ")
        };
        let w = wide(&right);
        send(self.status.get(), SB_SETTEXTW, 1, w.as_ptr() as isize);
    }

    /// Row `i`, fetching its block from the backend when not cached.
    fn row(&self, i: usize) -> Option<Row> {
        if let Some(r) = self.results.borrow().get(i) {
            return Some(r.clone());
        }
        self.fetch_blocks(i, i);
        self.results.borrow().get(i).cloned()
    }

    fn fetch_blocks(&self, from: usize, to: usize) {
        let (seq, ver, missing) = {
            let r = self.results.borrow();
            (r.seq, r.ver, r.missing(from, to))
        };
        if missing.is_empty() {
            return;
        }
        // Don't stall painting behind a running search.
        let Some(mut c) = self.conn.try_lock_for(Duration::from_millis(50)) else {
            return;
        };
        if c.seq != seq || c.ver != ver {
            return; // the session already holds a newer search (or filtering step)
        }
        for start in missing {
            match c.be.fetch(start as u64, BLOCK as u32) {
                Ok(p) => self.results.borrow_mut().insert(start, p.rows),
                Err(e) if e == quickseek_core::ipc::STALE => {
                    drop(c);
                    self.search(true);
                    return;
                }
                Err(_) => return,
            }
        }
    }

    fn cell_text(&self, row: &Row, col: i32) -> String {
        match col {
            0 => row.name.clone(),
            1 => row.dir.clone(),
            2 => {
                if !row.has_size || (row.is_dir && row.size == 0 && row.dir.is_empty()) {
                    String::new()
                } else {
                    shell::size_kb(row.size)
                }
            }
            3 => fmt::time(row.modified),
            4 => self.files.borrow_mut().get(&row.name, row.is_dir).1,
            _ => String::new(),
        }
    }

    pub fn selected(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut i: isize = -1;
        loop {
            i = send(
                self.list.get(),
                LVM_GETNEXTITEM,
                i as usize,
                LVNI_SELECTED as isize,
            );
            if i < 0 || out.len() >= 100_000 {
                break;
            }
            out.push(i as usize);
        }
        out
    }

    pub fn selected_rows(&self) -> Vec<Row> {
        if self.tree_mode() {
            let m = self.tree.borrow();
            return self
                .tree_selected()
                .into_iter()
                .filter_map(|i| m.nodes.get(i))
                .map(|n| {
                    let r = &n.row;
                    // A root's name is "C:" with an empty dir, like a result row.
                    Row {
                        name: r.name.clone(),
                        dir: r.dir.clone(),
                        size: r.size,
                        modified: r.modified,
                        is_dir: r.is_dir,
                        has_size: r.has_size,
                        ..Default::default()
                    }
                })
                .collect();
        }
        self.selected()
            .into_iter()
            .filter_map(|i| self.row(i))
            .collect()
    }

    fn open_selected(&self, mut rows: Vec<Row>) {
        if rows.is_empty()
            && let Some(r) = self.row(0)
        {
            rows.push(r);
        }
        for r in rows.iter().take(32) {
            let p = r.full_path();
            if shell::open(self.hwnd.get(), &p) {
                let _ = self.jobs.send(Job::Run(p));
            }
        }
    }

    fn copy(&self, what: u32) {
        let rows = self.selected_rows();
        let text: Vec<String> = rows
            .iter()
            .map(|r| match what {
                CMD_COPY_NAME => r.name.clone(),
                CMD_COPY_PATH => r.dir.clone(),
                _ => r.full_path(),
            })
            .collect();
        if !text.is_empty() {
            shell::copy_text(self.hwnd.get(), &text.join("\r\n"));
        }
    }

    pub fn command(&self, id: u32) {
        let hwnd = self.hwnd.get();
        match id {
            CMD_OPEN => self.open_selected(self.selected_rows()),
            CMD_OPEN_PATH => {
                for r in self.selected_rows().iter().take(16) {
                    shell::open_path(&r.full_path());
                }
            }
            CMD_COPY_FULL | CMD_COPY_NAME | CMD_COPY_PATH => self.copy(id),
            CMD_PROPERTIES => {
                if let Some(r) = self.selected_rows().first() {
                    shell::properties(hwnd, &r.full_path());
                }
            }
            CMD_EXPORT => self.export(),
            CMD_EXIT => unsafe {
                DestroyWindow(hwnd);
            },
            CMD_CASE | CMD_WW | CMD_PATH | CMD_REGEX => {
                {
                    let mut s = self.settings.borrow_mut();
                    let f = match id {
                        CMD_CASE => &mut s.match_case,
                        CMD_WW => &mut s.whole_word,
                        CMD_PATH => &mut s.match_path,
                        _ => &mut s.regex,
                    };
                    *f = !*f;
                }
                self.sync_menu();
                self.search(false);
            }
            CMD_DIACRITICS => {
                let mut c = self.conn.lock();
                if let Ok(mut cfg) = c.be.get_config() {
                    cfg.match_diacritics = !cfg.match_diacritics;
                    let _ = c.be.set_config(&cfg);
                }
                drop(c);
                self.sync_menu();
                self.search(true);
            }
            CMD_REFRESH => self.search(true),
            CMD_THEME_SYSTEM | CMD_THEME_LIGHT | CMD_THEME_DARK => {
                let m = match id {
                    CMD_THEME_LIGHT => theme::Mode::Light,
                    CMD_THEME_DARK => theme::Mode::Dark,
                    _ => theme::Mode::System,
                };
                {
                    let mut s = self.settings.borrow_mut();
                    s.theme = m.as_str().into();
                    s.save();
                }
                theme::set_mode(m);
                self.sync_menu();
            }
            CMD_SORT_RUNS => self.set_sort(SortKey::RunCount),
            CMD_SORT_CREATED => self.set_sort(SortKey::Created),
            CMD_OPTIONS => crate::options::open(self),
            CMD_CONNECT_AI => crate::connect::open(self),
            CMD_LOGIN => {
                shell::set_start_on_login(!shell::start_on_login());
                self.sync_menu();
            }
            CMD_BACKGROUND => {
                let on = !self.settings.borrow().run_in_background;
                self.settings.borrow_mut().run_in_background = on;
                self.sync_menu();
            }
            CMD_SYNTAX => {
                w::message_box(
                    hwnd,
                    SYNTAX_HELP,
                    "quickseek search syntax",
                    MB_OK | MB_ICONINFORMATION,
                );
            }
            CMD_ABOUT => {
                w::message_box(
                    hwnd,
                    &format!(
                        "Quickseek {}\nInstant filename search for NTFS, ReFS, folders and network shares.",
                        env!("CARGO_PKG_VERSION")
                    ),
                    "About Quickseek",
                    MB_OK | MB_ICONINFORMATION,
                );
            }
            CMD_SHOW => self.show(),
            CMD_TREE => self.set_tree_mode(!self.tree_mode()),
            CMD_PREVIEW => self.set_preview(!self.preview_on()),
            CMD_STOP => self.stop_scan(),
            CMD_EXPAND | CMD_COLLAPSE => {
                if let Some(&i) = self.tree_selected().first() {
                    self.toggle_node(i);
                }
            }
            CMD_SEARCH_HERE => {
                if let Some(p) = self.tree_selected_paths().first() {
                    let p = format!("\"{}\\\" ", p.trim_end_matches('\\'));
                    self.set_tree_mode(false);
                    w::set_text(self.edit.get(), &p);
                    unsafe { SetFocus(self.edit.get()) };
                    send(self.edit.get(), EM_SETSEL, p.len(), p.len() as isize);
                }
            }
            CMD_COMPARE_DAY | CMD_COMPARE_WEEK | CMD_COMPARE_MONTH => {
                if !self.tree_mode() {
                    self.set_tree_mode(true);
                }
                self.refresh_history();
                let days = match id {
                    CMD_COMPARE_DAY => 1,
                    CMD_COMPARE_WEEK => 7,
                    _ => 30,
                };
                let snap = crate::growth::snap_days_ago(&self.growth.borrow().snaps, days);
                match snap {
                    Some(s) => self.set_compare(Some(s)),
                    None => self.set_status_left("No folder-size history yet: the service keeps one snapshot a day from now on."),
                }
            }
            CMD_SAVE_POINT => {
                let name = format!(
                    "Save point {}",
                    crate::growth::day(quickseek_core::history::now_unix(), true)
                );
                let res = self
                    .conn
                    .try_lock_for(Duration::from_secs(10))
                    .map(|mut c| c.be.save_point(&name));
                match res {
                    Some(Ok(s)) => {
                        self.refresh_history();
                        self.set_status_left(&format!(
                            "Saved \"{}\": compare with it later on the timeline (the dot).",
                            s.name
                        ));
                        self.growth_soon();
                    }
                    Some(Err(e)) => self.set_status_left(&e),
                    None => {}
                }
            }
            CMD_STOP_COMPARE => self.set_compare(None),
            CMD_SHOW_IN_TREE => {
                if let Some(r) = self.selected_rows().first() {
                    *self.reveal.borrow_mut() = Some(r.full_path());
                    self.set_tree_mode(true);
                }
            }
            _ => {}
        }
    }

    fn set_sort(&self, key: SortKey) {
        {
            let mut s = self.settings.borrow_mut();
            let cur = sort_from(&s.sort);
            if cur == key {
                s.descending = !s.descending;
            } else {
                s.sort = sort_name(key).into();
                // Like Everything: sizes, dates and counts sort descending first.
                s.descending = matches!(
                    key,
                    SortKey::Size | SortKey::Modified | SortKey::Created | SortKey::RunCount
                );
            }
        }
        self.update_header();
        self.search(false);
    }

    fn update_header(&self) {
        let header = send(self.list.get(), LVM_GETHEADER, 0, 0) as HWND;
        let (key, desc) = self.sort();
        let col = match key {
            SortKey::Name => 0,
            SortKey::Path => 1,
            SortKey::Size => 2,
            SortKey::Modified => 3,
            _ => -1,
        };
        for c in 0..COLUMNS.len() as i32 {
            let mut hd: HDITEMW = unsafe { std::mem::zeroed() };
            hd.mask = HDI_FORMAT;
            send(header, HDM_GETITEMW, c as usize, &mut hd as *mut _ as isize);
            hd.fmt &= !(HDF_SORTUP | HDF_SORTDOWN);
            if c == col {
                hd.fmt |= if desc { HDF_SORTDOWN } else { HDF_SORTUP };
            }
            send(header, HDM_SETITEMW, c as usize, &hd as *const _ as isize);
        }
    }

    pub fn sync_menu(&self) {
        let m = self.menu.get();
        let s = self.settings.borrow();
        w::check_menu(m, CMD_CASE, s.match_case);
        w::check_menu(m, CMD_WW, s.whole_word);
        w::check_menu(m, CMD_PATH, s.match_path);
        w::check_menu(m, CMD_REGEX, s.regex);
        w::check_menu(m, CMD_BACKGROUND, s.run_in_background);
        let pick = match theme::Mode::parse(&s.theme) {
            theme::Mode::System => CMD_THEME_SYSTEM,
            theme::Mode::Light => CMD_THEME_LIGHT,
            theme::Mode::Dark => CMD_THEME_DARK,
        };
        unsafe { CheckMenuRadioItem(m, CMD_THEME_SYSTEM, CMD_THEME_DARK, pick, MF_BYCOMMAND) };
        drop(s);
        w::check_menu(m, CMD_LOGIN, shell::start_on_login());
        let diacritics = self
            .conn
            .try_lock()
            .and_then(|mut c| c.be.get_config().ok())
            .map(|c| c.match_diacritics);
        if let Some(d) = diacritics {
            w::check_menu(m, CMD_DIACRITICS, d);
        }
    }

    pub fn register_hotkey(&self) {
        let hwnd = self.hwnd.get();
        unsafe { UnregisterHotKey(hwnd, HOTKEY_ID) };
        if self.test_mode {
            return;
        }
        let hk = self.settings.borrow().hotkey.clone();
        if let Some((mods, vk)) = parse_hotkey(&hk)
            && unsafe {
                RegisterHotKey(hwnd, HOTKEY_ID, mods | 0x4000 /* MOD_NOREPEAT */, vk)
            } == 0
        {
            self.set_status_left(&format!("Hotkey {hk} is used by another program"));
        }
    }

    pub fn show(&self) {
        let hwnd = self.hwnd.get();
        unsafe {
            ShowWindow(
                hwnd,
                if IsIconic(hwnd) != 0 {
                    SW_RESTORE
                } else {
                    SW_SHOW
                },
            );
            SetForegroundWindow(hwnd);
            SetFocus(self.edit.get());
        }
        send(self.edit.get(), EM_SETSEL, 0, -1);
    }

    fn toggle(&self) {
        let hwnd = self.hwnd.get();
        let visible = unsafe {
            IsWindowVisible(hwnd) != 0 && IsIconic(hwnd) == 0 && GetForegroundWindow() == hwnd
        };
        if visible {
            unsafe { ShowWindow(hwnd, SW_HIDE) };
        } else {
            self.show();
        }
    }

    fn export(&self) {
        let Some(path) = shell::pick_save_file(self.hwnd.get(), "Quickseek results.csv") else {
            return;
        };
        let total = self.results.borrow().total;
        let mut out = String::from("Name,Path,Size,Date Modified\r\n");
        let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        let mut c = self.conn.lock();
        let mut off = 0;
        while off < total {
            let Ok(p) = c.be.fetch(off as u64, 10_000) else {
                break;
            };
            if p.rows.is_empty() {
                break;
            }
            for r in &p.rows {
                out.push_str(&format!(
                    "{},{},{},{}\r\n",
                    q(&r.name),
                    q(&r.dir),
                    r.size,
                    fmt::time(r.modified)
                ));
            }
            off += p.rows.len();
        }
        drop(c);
        if let Err(e) = std::fs::write(&path, out) {
            w::message_box(
                self.hwnd.get(),
                &format!("Export failed: {e}"),
                "quickseek",
                MB_OK | MB_ICONERROR,
            );
        }
    }

    fn layout(&self) {
        let hwnd = self.hwnd.get();
        let rc = w::client_rect(hwnd);
        let (cw, ch) = (rc.right, rc.bottom);
        let pad = px(hwnd, 4);
        let eh = px(hwnd, 26);
        let fw = px(hwnd, 150);
        let bw = px(hwnd, 64);
        let pbw = px(hwnd, 72);
        w::move_to(self.tree_btn.get(), pad, pad, bw, eh);
        w::move_to(
            self.edit.get(),
            2 * pad + bw,
            pad,
            cw - fw - bw - pbw - 5 * pad,
            eh,
        );
        // A drop-down list's closed height comes from its font: set its
        // selection field so the whole box is `eh`, like the search box and
        // the buttons (at any DPI: the border is measured, not assumed).
        // After moving it: resizing it resets the field to the font's.
        let combo = self.combo.get();
        w::move_to(combo, cw - fw - pbw - 2 * pad, pad, fw, px(hwnd, 300));
        let field = send(combo, CB_GETITEMHEIGHT, usize::MAX, 0) as i32;
        // The closed box's height: the drop button sits inset by the same
        // border above and below (its window rect is the open list's).
        let mut info: COMBOBOXINFO = unsafe { std::mem::zeroed() };
        info.cbSize = size_of::<COMBOBOXINFO>() as u32;
        let closed = if unsafe { GetComboBoxInfo(combo, &mut info) } != 0 {
            info.rcButton.top + info.rcButton.bottom
        } else {
            0
        };
        let border = closed - field;
        if field > 0 && border > 0 && field + border != eh {
            send(combo, CB_SETITEMHEIGHT, usize::MAX, (eh - border) as isize);
        }

        w::move_to(self.preview_btn.get(), cw - pbw - pad, pad, pbw, eh);
        send(self.status.get(), WM_SIZE, 0, 0);
        let mut sr = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        unsafe { GetWindowRect(self.status.get(), &mut sr) };
        let sh = sr.bottom - sr.top;
        let parts = [cw - px(hwnd, 420), -1];
        send(self.status.get(), SB_SETPARTS, 2, parts.as_ptr() as isize);
        w::move_to(
            self.progress.get(),
            parts[0] - px(hwnd, 168),
            px(hwnd, 4),
            px(hwnd, 160),
            sh - px(hwnd, 7),
        );
        let top = pad * 2 + eh;
        let (mut lw, gap) = (cw, px(hwnd, 5));
        if self.preview_on() {
            let pw = px(hwnd, self.settings.borrow().preview_width)
                .clamp(px(hwnd, 120), (cw - px(hwnd, 200)).max(px(hwnd, 120)));
            lw = (cw - pw - gap).max(0);
            self.split_x.set(lw);
            w::move_to(
                self.preview.host.get(),
                lw + gap,
                top,
                cw - lw - gap,
                ch - top - sh,
            );
        }
        w::move_to(self.list.get(), 0, top, lw, ch - top - sh);
        // Tree mode: the history strip on top, the growth pane on the right when comparing.
        let tree = self.tree_mode();
        let strip_h = if tree { px(hwnd, 124) } else { 0 };
        let pane_w = if tree && self.comparing() {
            px(hwnd, 360).min(lw / 2)
        } else {
            0
        };
        let body = (ch - top - sh - strip_h).max(0);
        w::move_to(self.strip.get(), 0, top, lw, strip_h);
        w::move_to(self.tree_list.get(), 0, top + strip_h, lw - pane_w, body);
        w::move_to(self.gpane.get(), lw - pane_w, top + strip_h, pane_w, body);
        unsafe {
            ShowWindow(self.strip.get(), if tree { SW_SHOW } else { SW_HIDE });
            ShowWindow(self.gpane.get(), if pane_w > 0 { SW_SHOW } else { SW_HIDE });
        }
        self.stretch_last(0);
        self.stretch_last(1);
    }

    /// The service is back after the connection broke: refresh what's shown.
    /// A service of another build means Quickseek was updated under this
    /// window: offer to restart it with the new version.
    fn on_reconnected(&self, build: Option<String>) {
        if let Some(b) = build.filter(|b| b != ipc::BUILD)
            && !self.test_mode
        {
            log::info!("the service is now {b}, this window is {}", ipc::BUILD);
            let hwnd = self.hwnd.get();
            if crate::msgbox::ask(
                hwnd,
                "Quickseek was updated. Restart this window to use the new version.",
                "Quickseek",
                "Restart",
            ) {
                self.restart_window();
                return;
            }
        }
        self.search(true);
        self.update_status();
    }

    /// Starts this window's program anew (the updated file, not this
    /// process's old one) and closes this one.
    fn restart_window(&self) {
        if let Some(exe) = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join("quickseek-gui.exe")))
        {
            let _ = std::process::Command::new(exe).spawn();
        }
        self.quit();
    }

    /// An updater (process `pid`) is about to replace our program: start
    /// again once it's done (a hidden helper waits for it; started by us, so
    /// unelevated), as we are now (in the tray or not), and close.
    fn close_for_update(&self, pid: u32) {
        let visible = unsafe { IsWindowVisible(self.hwnd.get()) } != 0;
        if !self.test_mode
            && let Some(exe) = std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|d| d.join("quickseek-gui.exe")))
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            let path = exe.to_string_lossy().replace('\'', "''");
            let args = if visible {
                ""
            } else {
                " -ArgumentList '--background'"
            };
            let script = format!(
                "Wait-Process -Id {pid} -Timeout 900 -ErrorAction SilentlyContinue; \
                 Start-Sleep -Milliseconds 500; Start-Process -FilePath '{path}'{args}"
            );
            let _ = std::process::Command::new("powershell.exe")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-WindowStyle",
                    "Hidden",
                    "-Command",
                    &script,
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .spawn();
        }
        self.quit();
    }

    /// Closes the window and ends the program, tray or not.
    fn quit(&self) {
        unsafe { DestroyWindow(self.hwnd.get()) };
    }

    /// The list (0) or tree (1) view.
    fn view(&self, which: usize) -> HWND {
        if which == 0 {
            self.list.get()
        } else {
            self.tree_list.get()
        }
    }

    /// The last column in display order.
    fn last_column(list: HWND) -> i32 {
        let header = send(list, LVM_GETHEADER, 0, 0) as HWND;
        let n = send(header, HDM_GETITEMCOUNT, 0, 0);
        if n <= 0 {
            return -1;
        }
        send(header, HDM_ORDERTOINDEX, (n - 1) as usize, 0) as i32
    }

    /// Widens the last column to the view's right edge (never narrower than
    /// its own width), so a row's hover and selection highlight reach it.
    fn stretch_last(&self, which: usize) {
        let list = self.view(which);
        let hwnd = self.hwnd.get();
        let last = Self::last_column(list);
        if last < 0 {
            return;
        }
        let (mut col, mut base) = self.filler[which].get();
        if col != last {
            // Another column is last now (reordered): give the old one its
            // own width back, and take the new one's as it is.
            self.stretching.set(true);
            if col >= 0 {
                send(
                    list,
                    LVM_SETCOLUMNWIDTH,
                    col as usize,
                    px(hwnd, base) as isize,
                );
            }
            self.stretching.set(false);
            col = last;
            base = send(list, LVM_GETCOLUMNWIDTH, last as usize, 0) as i32 * 96 / w::dpi(hwnd);
            self.filler[which].set((col, base));
        }
        let header = send(list, LVM_GETHEADER, 0, 0) as HWND;
        let n = send(header, HDM_GETITEMCOUNT, 0, 0).max(0) as usize;
        let others: i32 = (0..n)
            .filter(|&c| c as i32 != last)
            .map(|c| send(list, LVM_GETCOLUMNWIDTH, c, 0) as i32)
            .sum();
        let want = px(hwnd, base).max(w::client_rect(list).right - others);
        if send(list, LVM_GETCOLUMNWIDTH, last as usize, 0) as i32 != want {
            self.stretching.set(true);
            send(list, LVM_SETCOLUMNWIDTH, last as usize, want as isize);
            self.stretching.set(false);
        }
    }

    /// A column's width changed (the user dragging a divider): the last
    /// column's own width follows the user, then it fills again.
    fn column_resized(&self, header: HWND) {
        if self.stretching.get() {
            return;
        }
        for which in 0..2 {
            let list = self.view(which);
            if send(list, LVM_GETHEADER, 0, 0) as HWND != header {
                continue;
            }
            let last = Self::last_column(list);
            let (col, _) = self.filler[which].get();
            if col == last && last >= 0 {
                let hwnd = self.hwnd.get();
                let now = send(list, LVM_GETCOLUMNWIDTH, last as usize, 0) as i32;
                let others: i32 = (0..send(header, HDM_GETITEMCOUNT, 0, 0).max(0) as usize)
                    .filter(|&c| c as i32 != last)
                    .map(|c| send(list, LVM_GETCOLUMNWIDTH, c, 0) as i32)
                    .sum();
                // Only a width the user picked, not the filled one.
                if now < w::client_rect(list).right - others {
                    self.filler[which].set((col, (now * 96 / w::dpi(hwnd)).max(40)));
                }
            }
            self.stretch_last(which);
        }
    }

    /// Over the gap between the list and the preview pane.
    fn on_splitter(&self, x: i32, y: i32) -> bool {
        let hwnd = self.hwnd.get();
        let top = px(hwnd, 4) * 2 + px(hwnd, 26);
        self.preview_on()
            && x >= self.split_x.get()
            && x < self.split_x.get() + px(hwnd, 5)
            && y >= top
    }

    fn apply_fonts(&self) {
        let hwnd = self.hwnd.get();
        let (f, b) = (w::ui_font(hwnd, false), w::ui_font(hwnd, true));
        let (of, ob) = (self.font.replace(f), self.bold.replace(b));
        for c in [
            self.edit.get(),
            self.combo.get(),
            self.list.get(),
            self.status.get(),
            self.tree_btn.get(),
            self.tree_list.get(),
            self.preview_btn.get(),
        ] {
            w::set_font(c, f);
        }
        self.preview.set_font(w::dpi(hwnd));
        unsafe {
            if !of.is_null() {
                DeleteObject(of as _);
            }
            if !ob.is_null() {
                DeleteObject(ob as _);
            }
        }
    }

    fn add_tray(&self) {
        if self.test_mode {
            return;
        }
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = self.hwnd.get();
        nid.uID = 1;
        nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        nid.uCallbackMessage = WM_APP_TRAY;
        nid.hIcon = self.tray_icon.get();
        let tip = wide("Quickseek");
        nid.szTip[..tip.len()].copy_from_slice(&tip);
        unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
    }

    fn remove_tray(&self) {
        let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = self.hwnd.get();
        nid.uID = 1;
        unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
    }

    fn tray_menu(&self) {
        let menu = unsafe { CreatePopupMenu() };
        w::menu_item(menu, CMD_SHOW, "Show Quickseek");
        w::menu_item(menu, CMD_OPTIONS, "Options…");
        w::menu_sep(menu);
        w::menu_item(menu, CMD_EXIT, "Exit");
        let mut pt = POINT { x: 0, y: 0 };
        unsafe {
            GetCursorPos(&mut pt);
            SetForegroundWindow(self.hwnd.get());
            let cmd = TrackPopupMenuEx(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                pt.x,
                pt.y,
                self.hwnd.get(),
                null(),
            );
            DestroyMenu(menu);
            if cmd != 0 {
                self.command(cmd as u32);
            }
        }
    }

    fn context_menu(&self, x: i32, y: i32) {
        let rows = self.selected_rows();
        if rows.is_empty() {
            return;
        }
        let paths: Vec<String> = rows.iter().map(|r| r.full_path()).collect();
        let own = [
            (CMD_OPEN, "&Open"),
            (CMD_OPEN_PATH, "Open &Path"),
            (0, ""),
            (CMD_COPY_FULL, "Copy &Full Path"),
            (CMD_COPY_NAME, "Copy &Name"),
            (CMD_COPY_PATH, "Copy Pa&th"),
            (0, ""),
            (CMD_SHOW_IN_TREE, "Show in Folder Size &Tree"),
            (CMD_PROPERTIES, "P&roperties"),
        ];
        match shell::context_menu(self.hwnd.get(), &paths, &own, x, y) {
            MenuChoice::Own(id) => self.command(id),
            MenuChoice::Shell => self.search(true), // e.g. rename: show the effect
            MenuChoice::Delete { permanent } => {
                self.delete_paths(paths, rows.iter().map(|r| r.size).sum(), permanent)
            }
            MenuChoice::None => {}
        }
    }

    /// Custom draw: bold matched text in the Name and Path columns.
    fn custom_draw(&self, cd: &mut NMLVCUSTOMDRAW) -> LRESULT {
        let stage = cd.nmcd.dwDrawStage;
        if stage == CDDS_PREPAINT {
            return CDRF_NOTIFYITEMDRAW as LRESULT;
        }
        if stage == CDDS_ITEMPREPAINT {
            return CDRF_NOTIFYSUBITEMDRAW as LRESULT;
        }
        if stage != (CDDS_ITEMPREPAINT | CDDS_SUBITEM) {
            return CDRF_DODEFAULT as LRESULT;
        }
        // Grey while being deleted.
        let dim = !self.deleting.borrow().is_empty()
            && self
                .row(cd.nmcd.dwItemSpec)
                .is_some_and(|r| self.is_deleting(&r.full_path()));
        cd.clrText = if dim {
            theme::p().muted
        } else {
            theme::p().text
        };
        if cd.iSubItem > 1 || self.hl_terms.borrow().is_empty() {
            return CDRF_NEWFONT as LRESULT;
        }
        let i = cd.nmcd.dwItemSpec;
        let Some(row) = self.row(i) else {
            return CDRF_DODEFAULT as LRESULT;
        };
        let text = if cd.iSubItem == 0 {
            row.name.clone()
        } else {
            row.dir.clone()
        };
        let ranges = highlight::ranges(&text, &self.hl_terms.borrow());
        if ranges.is_empty() {
            return CDRF_DODEFAULT as LRESULT;
        }
        // Let the list paint background/selection and the icon, then draw our text.
        let list = self.list.get();
        let mut rc = RECT {
            left: if cd.iSubItem == 0 {
                LVIR_LABEL as i32
            } else {
                LVIR_BOUNDS as i32
            },
            top: cd.iSubItem,
            right: 0,
            bottom: 0,
        };
        send(list, LVM_GETSUBITEMRECT, i, &mut rc as *mut _ as isize);
        let hdc = cd.nmcd.hdc;
        unsafe {
            let selected =
                send(list, LVM_GETITEMSTATE, i, LVIS_SELECTED as isize) as u32 & LVIS_SELECTED != 0;
            let focused = GetFocus() == list;
            let classic_sel = selected && focused && !crate::themed();
            let hot = is_hot(list, i, cd);
            let pal = theme::p();
            let bg: COLORREF = if classic_sel {
                GetSysColor(COLOR_HIGHLIGHT)
            } else {
                pal.window
            };
            let fg: COLORREF = if classic_sel {
                GetSysColor(COLOR_HIGHLIGHTTEXT)
            } else if dim {
                pal.muted
            } else {
                pal.text
            };
            // Not over the list view's selection or hover highlight.
            if classic_sel || !(selected || hot) {
                FillRect(hdc, &rc, theme::brush(bg));
            }
            SetBkMode(hdc, TRANSPARENT as i32);
            SetTextColor(hdc, fg);
            // Same text inset the list view uses for its own drawing.
            let edge = windows_sys::Win32::UI::HiDpi::GetSystemMetricsForDpi(
                SM_CXEDGE,
                w::dpi(list) as u32,
            );
            let mut x = if cd.iSubItem == 0 {
                rc.left + edge
            } else {
                rc.left + 2 * edge + edge / 2
            };
            if cd.iSubItem == 0 {
                // We skip the default paint, so draw the icon too.
                let mut ir = RECT {
                    left: LVIR_ICON as i32,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                send(list, LVM_GETSUBITEMRECT, i, &mut ir as *mut _ as isize);
                let icon = self.files.borrow_mut().get(&row.name, row.is_dir).0;
                let il = self.files.borrow().image_list;
                let sz = windows_sys::Win32::UI::HiDpi::GetSystemMetricsForDpi(
                    SM_CXSMICON,
                    w::dpi(list) as u32,
                );
                ImageList_Draw(
                    il as HIMAGELIST,
                    icon,
                    hdc,
                    ir.left + (ir.right - ir.left - sz) / 2,
                    ir.top + (ir.bottom - ir.top - sz) / 2,
                    ILD_TRANSPARENT,
                );
            }
            for (seg, hot) in highlight::segments(&text, &ranges) {
                let old = SelectObject(
                    hdc,
                    if hot {
                        self.bold.get()
                    } else {
                        self.font.get()
                    } as _,
                );
                let w16: Vec<u16> = seg.encode_utf16().collect();
                let mut sz = windows_sys::Win32::Foundation::SIZE { cx: 0, cy: 0 };
                GetTextExtentPoint32W(hdc, w16.as_ptr(), w16.len() as i32, &mut sz);
                let y = rc.top + (rc.bottom - rc.top - sz.cy) / 2;
                ExtTextOutW(
                    hdc,
                    x,
                    y,
                    ETO_CLIPPED,
                    &rc,
                    w16.as_ptr(),
                    w16.len() as u32,
                    null(),
                );
                SelectObject(hdc, old);
                x += sz.cx;
                if x > rc.right {
                    break;
                }
            }
        }
        CDRF_SKIPDEFAULT as LRESULT
    }

    fn notify(&self, nm: &NMHDR, lparam: LPARAM) -> Option<LRESULT> {
        if nm.idFrom == ID_TREE as usize {
            return self.tree_notify(nm, lparam);
        }
        if nm.idFrom != ID_LIST as usize {
            return None;
        }
        match nm.code {
            LVN_GETDISPINFOW => {
                let di = unsafe { &mut *(lparam as *mut NMLVDISPINFOW) };
                let item = &mut di.item;
                let Some(row) = self.row(item.iItem as usize) else {
                    return Some(0);
                };
                if item.mask & LVIF_TEXT != 0 {
                    let text = self.cell_text(&row, item.iSubItem);
                    let mut buf = self.disp.borrow_mut();
                    buf.clear();
                    buf.extend(
                        text.encode_utf16()
                            .take(item.cchTextMax.max(1) as usize - 1),
                    );
                    buf.push(0);
                    item.pszText = buf.as_mut_ptr();
                }
                if item.mask & LVIF_IMAGE != 0 && item.iSubItem == 0 {
                    item.iImage = self.files.borrow_mut().get(&row.name, row.is_dir).0;
                }
                Some(0)
            }
            HDN_ENDTRACKW | HDN_DIVIDERDBLCLICKW | HDN_ENDDRAG => {
                let header = nm.hwndFrom;
                let hwnd = self.hwnd.get();
                // After the header has applied the change.
                unsafe {
                    PostMessageW(hwnd, WM_APP_COLUMNS, header as usize, 0);
                }
                None
            }
            LVN_ODCACHEHINT => {
                let h = unsafe { &*(lparam as *const NMLVCACHEHINT) };
                self.fetch_blocks(h.iFrom.max(0) as usize, h.iTo.max(0) as usize);
                Some(0)
            }
            LVN_COLUMNCLICK => {
                let lv = unsafe { &*(lparam as *const NMLISTVIEW) };
                let key = match lv.iSubItem {
                    1 => SortKey::Path,
                    2 => SortKey::Size,
                    3 => SortKey::Modified,
                    _ => SortKey::Name,
                };
                self.set_sort(key);
                Some(0)
            }
            LVN_ITEMACTIVATE => {
                self.open_selected(self.selected_rows());
                Some(0)
            }
            LVN_ITEMCHANGED => {
                self.update_status();
                self.preview_soon();
                Some(0)
            }
            LVN_BEGINDRAG => {
                let paths: Vec<String> =
                    self.selected_rows().iter().map(|r| r.full_path()).collect();
                shell::drag_out(self.hwnd.get(), &paths);
                Some(0)
            }
            LVN_KEYDOWN => {
                let k = unsafe { &*(lparam as *const NMLVKEYDOWN) };
                let ctrl = unsafe { GetKeyState(VK_CONTROL as i32) } < 0;
                let shift = unsafe { GetKeyState(VK_SHIFT as i32) } < 0;
                if k.wVKey == 0x2E && !ctrl {
                    self.delete_selection(shift);
                } else if ctrl && k.wVKey == b'C' as u16 {
                    self.copy(if shift { CMD_COPY_NAME } else { CMD_COPY_FULL });
                } else if ctrl && k.wVKey == b'A' as u16 {
                    let mut it: LVITEMW = unsafe { std::mem::zeroed() };
                    it.stateMask = LVIS_SELECTED;
                    it.state = LVIS_SELECTED;
                    send(
                        self.list.get(),
                        LVM_SETITEMSTATE,
                        usize::MAX,
                        &it as *const _ as isize,
                    );
                }
                Some(0)
            }
            NM_RCLICK => {
                let mut pt = POINT { x: 0, y: 0 };
                unsafe { GetCursorPos(&mut pt) };
                self.context_menu(pt.x, pt.y);
                Some(0)
            }
            NM_CUSTOMDRAW => {
                Some(self.custom_draw(unsafe { &mut *(lparam as *mut NMLVCUSTOMDRAW) }))
            }
            _ => None,
        }
    }

    fn save_layout(&self) {
        let hwnd = self.hwnd.get();
        let mut s = self.settings.borrow_mut();
        let mut wp: WINDOWPLACEMENT = unsafe { std::mem::zeroed() };
        wp.length = size_of::<WINDOWPLACEMENT>() as u32;
        if unsafe { GetWindowPlacement(hwnd, &mut wp) } != 0 {
            let r = wp.rcNormalPosition;
            s.window = Some((r.left, r.top, r.right - r.left, r.bottom - r.top));
        }
        let widths = |which: usize, n: usize| -> Vec<i32> {
            let (col, base) = self.filler[which].get();
            (0..n)
                .map(|c| {
                    if c as i32 == col {
                        base
                    } else {
                        send(self.view(which), LVM_GETCOLUMNWIDTH, c, 0) as i32 * 96 / w::dpi(hwnd)
                    }
                })
                .collect()
        };
        s.column_widths = widths(0, COLUMNS.len());
        s.tree_column_widths = widths(1, tv::COLUMNS.len());
        if let Ok(i) = usize::try_from(send(self.combo.get(), CB_GETCURSEL, 0, 0)) {
            s.filter = i;
        }
        s.save();
    }
}

const SYNTAX_HELP: &str = "\
foo bar\tname contains foo AND bar
foo|bar\tOR (binds tighter than AND)
!foo\tNOT          <a b>|c\tgrouping
\"a b\"\tliteral with spaces
*.rs  a?c\twildcards match the whole name
users\\me\ta \\ or / matches the full path
users\\*\\*.pdf\tin a path, * spans any number of folders
wfn:node_modules\tthe whole name exactly

case:  ww:  path:  regex:\tper-term options (also in the Search menu)
ext:jpg;png\textensions
size:>10mb  size:1mb..2mb
dm:today  dm:lastweek  dm:2026-01..2026-06  dm:last3days
dc: (created)  rc: (recently changed)
parent:C:\\dir\tdirect children    depth:2
empty:\tempty folders     attrib:HS\tattributes
dupe:  sizedupe:\tduplicate names / sizes
runcount:>0\topened from quickseek
file:  folder:  audio:  video:  pic:  doc:  exe:  zip:

content:text	the file contains text (read as you; Esc stops)
case:content:  ww:content:  regex:content:  !content:
	combine with AND only, e.g.  *.rs content:\"fn main\"";

// ---- window procedures ---------------------------------------------------------

unsafe extern "system" fn edit_proc(
    h: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_KEYDOWN
        && let Some(app) = app()
    {
        match w as u16 {
            k if k == VK_DOWN || k == VK_NEXT => {
                let list = if app.tree_mode() {
                    app.tree_list.get()
                } else {
                    app.list.get()
                };
                if send(list, LVM_GETITEMCOUNT, 0, 0) > 0 {
                    unsafe { SetFocus(list) };
                    if send(list, LVM_GETSELECTEDCOUNT, 0, 0) == 0 {
                        let mut it: LVITEMW = unsafe { std::mem::zeroed() };
                        it.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
                        it.state = LVIS_SELECTED | LVIS_FOCUSED;
                        send(list, LVM_SETITEMSTATE, 0, &it as *const _ as isize);
                    }
                }
                return 0;
            }
            VK_RETURN if !app.tree_mode() => {
                app.open_selected(app.selected_rows());
                return 0;
            }
            VK_ESCAPE => {
                if app.scanning() {
                    app.stop_scan(); // the first Esc stops a content search
                } else if w::get_text(h).is_empty() {
                    if app.settings.borrow().run_in_background && !app.test_mode {
                        unsafe { ShowWindow(app.hwnd.get(), SW_HIDE) };
                    }
                } else {
                    w::set_text(h, "");
                }
                return 0;
            }
            _ => {}
        }
    }
    if msg == WM_CHAR && (w == 0x0D || w == 0x1B) {
        return 0; // no beep
    }
    unsafe { DefSubclassProc(h, msg, w, l) }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let Some(app) = app() else {
        return unsafe { DefWindowProcW(hwnd, msg, w, l) };
    };
    if matches!(
        msg,
        WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR
    ) && let Some(r) = shell::forward_menu_message(msg, w, l)
    {
        return r;
    }
    match msg {
        WM_SIZE => {
            app.layout();
            0
        }
        WM_GETMINMAXINFO => {
            let mmi = unsafe { &mut *(l as *mut MINMAXINFO) };
            mmi.ptMinTrackSize = POINT {
                x: px(hwnd, 420),
                y: px(hwnd, 240),
            };
            0
        }
        WM_DPICHANGED => {
            let r = unsafe { &*(l as *const RECT) };
            unsafe {
                SetWindowPos(
                    hwnd,
                    null_mut(),
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                )
            };
            app.apply_fonts();
            app.layout();
            0
        }
        WM_COMMAND => {
            let (id, code) = (w::loword(w), w::hiword(w));
            if id == ID_SEARCH as u32 && code == EN_CHANGE {
                app.search(false);
            } else if id == ID_TREE_BTN as u32 && code == BN_CLICKED {
                app.set_tree_mode(!app.tree_mode());
            } else if id == ID_PREVIEW_BTN as u32 && code == BN_CLICKED {
                app.set_preview(!app.preview_on());
            } else if id == ID_FILTER as u32 && code == CBN_SELCHANGE {
                if let Ok(i) = usize::try_from(send(app.combo.get(), CB_GETCURSEL, 0, 0)) {
                    app.settings.borrow_mut().filter = i;
                }
                app.search(false);
            } else if l == 0 {
                app.command(id);
            }
            0
        }
        WM_NOTIFY => {
            let nm = unsafe { &*(l as *const NMHDR) };
            app.notify(nm, l)
                .unwrap_or_else(|| unsafe { DefWindowProcW(hwnd, msg, w, l) })
        }
        WM_APP_TREE => {
            let done = unsafe { Box::from_raw(l as *mut TreeDone) };
            app.on_tree(*done);
            0
        }
        WM_APP_RESULT => {
            let done = unsafe { Box::from_raw(l as *mut SearchDone) };
            app.on_result(*done);
            0
        }
        WM_APP_DELETED => {
            let d = unsafe { Box::from_raw(l as *mut shell::DeleteDone) };
            app.on_deleted(*d);
            0
        }
        WM_APP_CONTENT => {
            let u = unsafe { Box::from_raw(l as *mut ContentUpdate) };
            app.on_content(*u);
            0
        }
        WM_SETCURSOR if w as HWND == hwnd => {
            let mut pt = POINT { x: 0, y: 0 };
            unsafe {
                GetCursorPos(&mut pt);
                windows_sys::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt);
            }
            if app.on_splitter(pt.x, pt.y) {
                unsafe { SetCursor(LoadCursorW(null_mut(), IDC_SIZEWE)) };
                return 1;
            }
            unsafe { DefWindowProcW(hwnd, msg, w, l) }
        }
        WM_LBUTTONDOWN => {
            let (x, y) = (
                w::loword(l as usize) as i16 as i32,
                w::hiword(l as usize) as i16 as i32,
            );
            if app.on_splitter(x, y) {
                app.split_drag.set(Some(x - app.split_x.get()));
                unsafe { SetCapture(hwnd) };
            }
            0
        }
        WM_MOUSEMOVE => {
            if let Some(off) = app.split_drag.get() {
                let x = w::loword(l as usize) as i16 as i32 - off;
                let cw = w::client_rect(hwnd).right;
                let gap = px(hwnd, 5);
                app.settings.borrow_mut().preview_width = (cw - x - gap).max(0) * 96 / w::dpi(hwnd);
                app.layout();
            }
            0
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED => {
            if app.split_drag.take().is_some() && msg == WM_LBUTTONUP {
                unsafe { ReleaseCapture() };
            }
            0
        }
        WM_APP_STATUS => {
            let st = unsafe { Box::from_raw(l as *mut StatusInfo) };
            let changed = { app.results.borrow().generation != st.generation };
            let was_loading = app.last_status.borrow().as_ref().is_some_and(|s| !s.ready);
            let generation = st.generation;
            *app.last_status.borrow_mut() = Some(*st);
            if app.tree_mode() {
                if was_loading && st_ready(&app.last_status.borrow()) {
                    app.search(true);
                } else {
                    app.maybe_refresh_tree(generation);
                    app.update_status();
                }
            } else if ((changed && app.content.borrow().is_none()) || was_loading)
                && !app.searching.get()
            {
                // A content search isn't redone for every change (F5 does).
                app.search(true);
            } else {
                app.update_status();
            }
            0
        }
        WM_TIMER if w == TIMER_POLL => {
            let _ = app.jobs.send(Job::Poll);
            0
        }
        WM_TIMER if w == TIMER_GROWTH => {
            unsafe { KillTimer(hwnd, TIMER_GROWTH) };
            app.rebuild_growth();
            0
        }
        WM_TIMER if w == TIMER_PREVIEW => {
            unsafe { KillTimer(hwnd, TIMER_PREVIEW) };
            app.update_preview();
            0
        }
        WM_HOTKEY => {
            app.toggle();
            0
        }
        WM_APP_CONN => {
            if w == 0 {
                app.set_status_left(ipc::RECONNECTING);
            } else {
                let build = unsafe { Box::from_raw(l as *mut Option<String>) };
                app.on_reconnected(*build);
            }
            0
        }
        WM_APP_COLUMNS => {
            app.column_resized(w as HWND);
            0
        }
        WM_APP_SHOW => {
            app.show();
            0
        }
        WM_APP_TRAY => {
            match w::loword(l as usize) {
                WM_LBUTTONUP => app.toggle(),
                WM_RBUTTONUP => app.tray_menu(),
                _ => {}
            }
            0
        }
        WM_ACTIVATE if w::loword(w) != 0 => {
            unsafe { SetFocus(app.edit.get()) };
            0
        }
        WM_CLOSE => {
            if app.settings.borrow().run_in_background && !app.test_mode {
                unsafe { ShowWindow(hwnd, SW_HIDE) };
            } else {
                unsafe { DestroyWindow(hwnd) };
            }
            0
        }
        WM_DESTROY => {
            app.save_layout();
            app.remove_tray();
            unsafe {
                KillTimer(hwnd, TIMER_POLL);
                UnregisterHotKey(hwnd, HOTKEY_ID);
                PostQuitMessage(0);
            }
            0
        }
        m if m == app.close_for_update && m != 0 => {
            app.close_for_update(w as u32);
            1
        }
        m if m == app.taskbar_created && m != 0 => {
            app.add_tray(); // Explorer restarted
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

/// Creates the main window and its controls.
pub fn create(be: Box<dyn Backend>, test_mode: bool, start_hidden: bool) -> &'static App {
    unsafe {
        let icon = crate::app_icon();
        let class = wide(CLASS);
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: w::hinstance(),
            hIcon: icon,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as usize as HBRUSH,
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
            hIconSm: crate::app_icon_small(),
        };
        RegisterClassExW(&wc);

        let settings = Settings::load();
        theme::init(theme::Mode::parse(&settings.theme));
        let (tx, rx) = channel();
        let conn = Arc::new(Mutex::new(Conn { be, seq: 0, ver: 0 }));
        let app: &'static App = Box::leak(Box::new(App {
            hwnd: Cell::new(null_mut()),
            edit: Cell::new(null_mut()),
            combo: Cell::new(null_mut()),
            list: Cell::new(null_mut()),
            status: Cell::new(null_mut()),
            menu: Cell::new(null_mut()),
            font: Cell::new(null_mut()),
            bold: Cell::new(null_mut()),
            conn: conn.clone(),
            jobs: tx,
            results: RefCell::new(Results::default()),
            latest_seq: Cell::new(0),
            searching: Cell::new(false),
            hl_terms: RefCell::new(Vec::new()),
            files: RefCell::new(FileInfoCache::default()),
            settings: RefCell::new(settings.clone()),
            disp: RefCell::new(Vec::with_capacity(512)),
            last_status: RefCell::new(None),
            test_mode,
            tray_icon: Cell::new(crate::app_icon_small()),
            taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            close_for_update: RegisterWindowMessageW(
                wide(quickseek_core::upgrade::CLOSE_FOR_UPDATE).as_ptr(),
            ),
            tree_btn: Cell::new(null_mut()),
            tree_list: Cell::new(null_mut()),
            tree: RefCell::new(tv::Model::default()),
            tree_seq: Cell::new(0),
            restore: RefCell::new(Vec::new()),
            reveal: RefCell::new(None),
            glyph_click: Cell::new(0),
            growth: RefCell::new(crate::growth::Growth::default()),
            deleting: RefCell::new(Default::default()),
            deletes_running: Cell::new(0),
            delete_note: RefCell::new(None),
            search_error: RefCell::new(None),
            tree_gen: Cell::new(0),
            tree_at: Cell::new(None),
            tree_quiet: Cell::new(false),
            keep_sel: RefCell::new(Vec::new()),
            strip: Cell::new(null_mut()),
            gpane: Cell::new(null_mut()),
            preview: Preview::default(),
            preview_btn: Cell::new(null_mut()),
            progress: Cell::new(null_mut()),
            content: RefCell::new(None),
            scan_cancel: RefCell::new(None),
            scan_stats: RefCell::new(None),
            split_x: Cell::new(0),
            split_drag: Cell::new(None),
            filler: [Cell::new((-1, 0)), Cell::new((-1, 0))],
            stretching: Cell::new(false),
        }));
        APP.with(|a| *a.borrow_mut() = Some(app));

        let (x, y, cw, ch) = settings
            .window
            .unwrap_or((CW_USEDEFAULT, CW_USEDEFAULT, 1180, 720));
        let title = wide("Quickseek");
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
            x,
            y,
            cw,
            ch,
            null_mut(),
            null_mut(),
            w::hinstance(),
            null(),
        );
        app.hwnd.set(hwnd);
        if settings.window.is_none() {
            // Default size is in 96-DPI units.
            let (sw, sh) = (px(hwnd, 1180), px(hwnd, 720));
            SetWindowPos(hwnd, null_mut(), 0, 0, sw, sh, SWP_NOMOVE | SWP_NOZORDER);
        }

        // Menu bar.
        let bar = CreateMenu();
        let file = w::submenu(bar, "&File");
        w::menu_item(file, CMD_OPEN, "&Open\tEnter");
        w::menu_item(file, CMD_OPEN_PATH, "Open &Path");
        w::menu_sep(file);
        w::menu_item(file, CMD_EXPORT, "&Export Results…");
        w::menu_sep(file);
        w::menu_item(file, CMD_EXIT, "E&xit\tCtrl+Q");
        let edit = w::submenu(bar, "&Edit");
        w::menu_item(edit, CMD_COPY_FULL, "Copy &Full Path\tCtrl+C");
        w::menu_item(edit, CMD_COPY_NAME, "Copy &Name\tCtrl+Shift+C");
        w::menu_item(edit, CMD_COPY_PATH, "Copy &Path");
        let search = w::submenu(bar, "&Search");
        w::menu_item(search, CMD_CASE, "Match &Case");
        w::menu_item(search, CMD_WW, "Match &Whole Word");
        w::menu_item(search, CMD_PATH, "Match &Path");
        w::menu_item(search, CMD_DIACRITICS, "Match &Diacritics");
        w::menu_item(search, CMD_REGEX, "Enable &Regex");
        w::menu_sep(search);
        w::menu_item(search, CMD_STOP, "&Stop Content Search\tEsc");
        let view = w::submenu(bar, "&View");
        w::menu_item(view, CMD_SORT_RUNS, "Sort by &Run Count");
        w::menu_item(view, CMD_SORT_CREATED, "Sort by Date &Created");
        w::menu_sep(view);
        w::menu_item(view, CMD_TREE, "Folder Size &Tree\tCtrl+T");
        w::menu_item(view, CMD_COMPARE_DAY, "Compare with &Yesterday");
        w::menu_item(view, CMD_COMPARE_WEEK, "Compare with a &Week Ago");
        w::menu_item(view, CMD_COMPARE_MONTH, "Compare with a &Month Ago");
        w::menu_item(view, CMD_SAVE_POINT, "Take a &Save Point");
        w::menu_item(view, CMD_STOP_COMPARE, "Stop Compari&ng");
        w::menu_item(view, CMD_PREVIEW, "&Preview Pane\tAlt+P");
        let themes = w::submenu(view, "T&heme");
        w::menu_item(themes, CMD_THEME_SYSTEM, "&System (as Windows)");
        w::menu_item(themes, CMD_THEME_LIGHT, "&Light");
        w::menu_item(themes, CMD_THEME_DARK, "&Dark");
        w::menu_sep(view);
        w::menu_item(view, CMD_REFRESH, "&Refresh\tF5");
        let tools = w::submenu(bar, "&Tools");
        w::menu_item(tools, CMD_OPTIONS, "&Options…\tCtrl+P");
        w::menu_item(tools, CMD_CONNECT_AI, "Connect &AI Assistants…");
        w::menu_sep(tools);
        w::menu_item(tools, CMD_BACKGROUND, "Run in &Background (tray)");
        w::menu_item(tools, CMD_LOGIN, "Start with &Windows");
        let help = w::submenu(bar, "&Help");
        w::menu_item(help, CMD_SYNTAX, "Search &Syntax");
        w::menu_item(help, CMD_ABOUT, "&About");
        SetMenu(hwnd, bar);
        app.menu.set(bar);

        // Controls.
        let btn = w::child(
            hwnd,
            "Button",
            "Tree",
            WS_TABSTOP | (BS_AUTOCHECKBOX | BS_PUSHLIKE) as u32,
            0,
            ID_TREE_BTN,
        );
        app.tree_btn.set(btn);
        let edit = w::child(
            hwnd,
            "Edit",
            "",
            WS_TABSTOP | ES_AUTOHSCROLL as u32,
            WS_EX_CLIENTEDGE,
            ID_SEARCH,
        );
        send(edit, EM_SETCUEBANNER, 1, wide("Search").as_ptr() as isize);
        SetWindowSubclass(edit, Some(edit_proc), 1, 0);
        app.edit.set(edit);

        let combo = w::child(
            hwnd,
            "ComboBox",
            "",
            WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32,
            0,
            ID_FILTER,
        );
        for (name, _) in FILTERS {
            send(combo, CB_ADDSTRING, 0, wide(name).as_ptr() as isize);
        }
        send(
            combo,
            CB_SETCURSEL,
            settings.filter.min(FILTERS.len() - 1),
            0,
        );
        app.combo.set(combo);
        let pbtn = w::child(
            hwnd,
            "Button",
            "Preview",
            WS_TABSTOP | (BS_AUTOCHECKBOX | BS_PUSHLIKE) as u32,
            0,
            ID_PREVIEW_BTN,
        );
        app.preview_btn.set(pbtn);

        let list = w::child(
            hwnd,
            "SysListView32",
            "",
            WS_TABSTOP | (LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS),
            0,
            ID_LIST,
        );
        send(
            list,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            0,
            (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_HEADERDRAGDROP | LVS_EX_LABELTIP)
                as isize,
        );
        if crate::themed() {
            SetWindowTheme(list, wide("Explorer").as_ptr(), null());
        }
        for (c, (name, width, right)) in COLUMNS.iter().enumerate() {
            let wname = wide(name);
            let mut col: LVCOLUMNW = std::mem::zeroed();
            col.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_FMT;
            col.fmt = if *right { LVCFMT_RIGHT } else { LVCFMT_LEFT };
            let saved = crate::settings::column_width(
                settings.column_widths.get(c).copied(),
                *width,
                crate::settings::min_column_width(name),
            );
            col.cx = px(hwnd, saved);
            col.pszText = wname.as_ptr() as *mut u16;
            send(list, LVM_INSERTCOLUMNW, c, &col as *const _ as isize);
        }
        // System image list for icons (shared; the list must not destroy it).
        let _ = app.files.borrow_mut().get("x.txt", false);
        let il = app.files.borrow().image_list;
        let style = GetWindowLongPtrW(list, GWL_STYLE);
        SetWindowLongPtrW(list, GWL_STYLE, style | LVS_SHAREIMAGELISTS as isize);
        send(list, LVM_SETIMAGELIST, LVSIL_SMALL as usize, il);
        app.list.set(list);

        // Folder-size tree (shown instead of the list in tree mode).
        let tl = w::child(
            hwnd,
            "SysListView32",
            "",
            WS_TABSTOP | (LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS),
            0,
            ID_TREE,
        );
        ShowWindow(tl, SW_HIDE);
        send(
            tl,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            0,
            (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP) as isize,
        );
        if crate::themed() {
            SetWindowTheme(tl, wide("Explorer").as_ptr(), null());
        }
        for (c, (name, width, right)) in tv::COLUMNS.iter().enumerate() {
            let wname = wide(name);
            let mut col: LVCOLUMNW = std::mem::zeroed();
            col.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_FMT;
            col.fmt = if *right { LVCFMT_RIGHT } else { LVCFMT_LEFT };
            let saved = crate::settings::column_width(
                settings.tree_column_widths.get(c).copied(),
                *width,
                crate::settings::min_column_width(name),
            );
            col.cx = px(hwnd, saved);
            col.pszText = wname.as_ptr() as *mut u16;
            send(tl, LVM_INSERTCOLUMNW, c, &col as *const _ as isize);
        }
        let order = tv::COLUMN_ORDER;
        send(
            tl,
            LVM_SETCOLUMNORDERARRAY,
            order.len(),
            order.as_ptr() as isize,
        );
        for c in [tv::COL_CHANGE, tv::COL_TREND] {
            send(tl, LVM_SETCOLUMNWIDTH, c as usize, 0);
        }
        app.tree_list.set(tl);
        let (strip, gpane) = crate::growth::create(hwnd);
        app.strip.set(strip);
        app.gpane.set(gpane);

        let status = w::child(hwnd, "msctls_statusbar32", "", SBARS_SIZEGRIP, 0, ID_STATUS);
        app.status.set(status);
        let pb = w::child(status, "msctls_progress32", "", PBS_SMOOTH, 0, ID_PROGRESS);
        send(pb, PBM_SETRANGE32, 0, 1000);
        ShowWindow(pb, SW_HIDE);
        app.progress.set(pb);
        app.preview.create(hwnd);

        app.apply_fonts();
        theme::attach(hwnd, theme::Surface::Window);
        app.layout();
        app.update_header();
        app.update_header_tree();
        app.sync_menu();
        app.add_tray();
        app.register_hotkey();

        let hw = hwnd as isize;
        std::thread::Builder::new()
            .name("search".into())
            .spawn(move || worker(conn, rx, hw))
            .unwrap();
        SetTimer(hwnd, TIMER_POLL, 700, None);

        if !start_hidden {
            ShowWindow(hwnd, SW_SHOW);
            SetFocus(edit);
        }
        if settings.preview {
            app.set_preview(true);
        }
        if settings.tree_mode {
            app.set_tree_mode(true);
        } else {
            app.search(false);
        }
        app
    }
}

/// Whether row `i` is under the mouse (the list view's hover highlight).
fn is_hot(list: HWND, i: usize, cd: &NMLVCUSTOMDRAW) -> bool {
    cd.nmcd.uItemState & CDIS_HOT != 0 || send(list, LVM_GETHOTITEM, 0, 0) == i as isize
}

/// Keyboard shortcuts that work anywhere in the main window.
pub fn global_key(msg: &MSG) -> bool {
    let Some(app) = app() else { return false };
    if unsafe { GetAncestor(msg.hwnd, GA_ROOT) } != app.hwnd.get() {
        return false;
    }
    if msg.message == WM_SYSKEYDOWN && msg.wParam == b'P' as usize {
        app.command(CMD_PREVIEW); // Alt+P, like Explorer
        return true;
    }
    if msg.message != WM_KEYDOWN {
        return false;
    }
    let ctrl = unsafe { GetKeyState(VK_CONTROL as i32) } < 0;
    match (ctrl, msg.wParam as u8) {
        (true, b'F') | (true, b'L') => {
            unsafe { SetFocus(app.edit.get()) };
            send(app.edit.get(), EM_SETSEL, 0, -1);
            true
        }
        (true, b'Q') => {
            app.command(CMD_EXIT);
            true
        }
        (true, b'P') => {
            app.command(CMD_OPTIONS);
            true
        }
        (true, b'T') => {
            app.command(CMD_TREE);
            true
        }
        (false, 0x74) => {
            app.command(CMD_REFRESH); // F5
            true
        }
        _ => false,
    }
}

fn st_ready(s: &Option<StatusInfo>) -> bool {
    s.as_ref().is_some_and(|s| s.ready)
}

/// How deleting tracks paths.
fn delete_key(p: &str) -> String {
    p.trim_end_matches('\\').to_lowercase()
}

/// A drive (`C:\`) or share root (`\\server\share`): never deleted from here.
fn is_root(p: &str) -> bool {
    let t = p.trim_end_matches('\\');
    if t.len() <= 2 {
        return true;
    }
    t.strip_prefix("\\\\")
        .is_some_and(|rest| rest.split('\\').filter(|s| !s.is_empty()).count() <= 2)
}

/// "notes.txt" or "3 items".
fn items_label(paths: &[String]) -> String {
    match paths {
        [one] => format!(
            "\"{}\"",
            one.trim_end_matches('\\')
                .rsplit('\\')
                .next()
                .unwrap_or(one)
        ),
        _ => format!("{} items", shell::group(paths.len() as u64)),
    }
}
