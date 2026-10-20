//! The main window.

use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use parking_lot::Mutex;
use rapidfind_core::fmt;
use rapidfind_core::fold::FoldMode;
use rapidfind_core::ipc::{Backend, Page, Row, StatusInfo};
use rapidfind_core::query::{FILTERS, Query, SearchFlags};
use rapidfind_core::search::SortKey;
use rapidfind_core::tree::{TreeLevel, TreeSort};
use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetFocus, GetKeyState, RegisterHotKey, SetFocus, UnregisterHotKey, VK_CONTROL, VK_DOWN, VK_ESCAPE, VK_NEXT, VK_RETURN, VK_SHIFT};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, SetWindowSubclass, Shell_NotifyIconW};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::highlight;
use crate::results::{BLOCK, Results};
use crate::settings::{Settings, parse_hotkey};
use crate::shell::{self, FileInfoCache, MenuChoice};
use crate::tree as tv;
use crate::w::{self, px, send, wide};

pub const CLASS: &str = "rapidfind.main";
pub const ID_SEARCH: i32 = 100;
pub const ID_FILTER: i32 = 101;
pub const ID_LIST: i32 = 102;
pub const ID_STATUS: i32 = 103;
pub const ID_TREE: i32 = 104;
pub const ID_TREE_BTN: i32 = 105;

pub const WM_APP_RESULT: u32 = WM_APP + 1;
pub const WM_APP_STATUS: u32 = WM_APP + 2;
pub const WM_APP_TRAY: u32 = WM_APP + 3;
/// Posted by a second instance: show the existing window.
pub const WM_APP_SHOW: u32 = WM_APP + 4;
pub const WM_APP_TREE: u32 = WM_APP + 5;
const TIMER_POLL: usize = 1;
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
const CMD_REFRESH: u32 = 400;
const CMD_SORT_RUNS: u32 = 401;
const CMD_SORT_CREATED: u32 = 402;
pub const CMD_TREE: u32 = 403;
const CMD_OPTIONS: u32 = 500;
const CMD_LOGIN: u32 = 501;
const CMD_BACKGROUND: u32 = 502;
const CMD_SYNTAX: u32 = 600;
const CMD_ABOUT: u32 = 601;
const CMD_SHOW: u32 = 700;

const COLUMNS: [(&str, i32, bool); 5] = [("Name", 260, false), ("Path", 440, false), ("Size", 120, true), ("Date Modified", 140, false), ("Type", 170, false)];

pub struct Conn {
    pub be: Box<dyn Backend>,
    /// The search whose result set the backend session currently holds.
    pub seq: u64,
}

enum Job {
    Search { seq: u64, query: String, flags: SearchFlags, sort: SortKey, desc: bool, keep: bool },
    Poll,
    Run(String),
    Tree { seq: u64, query: String, flags: SearchFlags, sort: TreeSort, desc: bool },
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
    pub tree_btn: Cell<HWND>,
    pub tree_list: Cell<HWND>,
    tree: RefCell<tv::Model>,
    tree_seq: Cell<u64>,
    /// After the next tree build: expand these paths, then select `reveal`.
    restore: RefCell<Vec<String>>,
    reveal: RefCell<Option<String>>,
    /// When the last click toggled a +/− box (so the double-click that may follow doesn't toggle back).
    glyph_click: Cell<u32>,
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
            Job::Search { seq, query, flags, sort, desc, keep } => {
                let mut c = conn.lock();
                let res = c.be.search(&query, flags, sort, desc, BLOCK as u32 * 2);
                c.seq = seq;
                drop(c);
                let b = Box::new(SearchDone { seq, keep, res });
                post(WM_APP_RESULT, 0, Box::into_raw(b) as isize);
            }
            Job::Poll => poll = true,
            Job::Run(p) => {
                conn.lock().be.record_run(&p);
            }
            Job::Tree { seq, query, flags, sort, desc } => {
                let t = std::time::Instant::now();
                let res = conn.lock().be.tree(&query, flags, sort, desc);
                let b = Box::new(TreeDone { seq, res, ms: t.elapsed().as_secs_f64() * 1000.0 });
                post(WM_APP_TREE, 0, Box::into_raw(b) as isize);
            }
            Job::TreeClose => conn.lock().be.tree_close(),
        }
        if poll && let Some(s) = conn.lock().be.status() {
            post(WM_APP_STATUS, 0, Box::into_raw(Box::new(s)) as isize);
        }
    }
}

impl App {
    fn flags(&self) -> SearchFlags {
        let s = self.settings.borrow();
        SearchFlags { case: s.match_case, whole_word: s.whole_word, path: s.match_path, regex: s.regex }
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
        let seq = self.latest_seq.get() + 1;
        self.latest_seq.set(seq);
        self.searching.set(true);
        let (sort, desc) = self.sort();
        let _ = self.jobs.send(Job::Search { seq, query: self.query_text(), flags: self.flags(), sort, desc, keep });
        let text = w::get_text(self.edit.get());
        let title = if text.is_empty() { "rapidfind".to_string() } else { format!("{text} - rapidfind") };
        w::set_text(self.hwnd.get(), &title);
    }

    fn on_result(&self, done: SearchDone) {
        if done.seq != self.latest_seq.get() {
            return; // superseded
        }
        self.searching.set(false);
        let list = self.list.get();
        match done.res {
            Ok(page) => {
                let total = page.total.min(i32::MAX as u64) as usize;
                *self.results.borrow_mut() = Results::new(done.seq, page);
                let text = self.query_text();
                let terms = Query::parse_with(&text, self.flags(), FoldMode::Lower, &rapidfind_core::date::DateEnv::system())
                    .map(|q| if self.flags().case { Vec::new() } else { q.highlight_terms() })
                    .unwrap_or_default();
                *self.hl_terms.borrow_mut() = terms;
                if done.keep {
                    send(list, LVM_SETITEMCOUNT, total, (LVSICF_NOSCROLL | LVSICF_NOINVALIDATEALL) as isize);
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
            }
            Err(e) => {
                *self.results.borrow_mut() = Results::default();
                send(list, LVM_SETITEMCOUNT, 0, 0);
                self.set_status_left(&if e == rapidfind_core::ipc::LOADING { "Index is still loading…".to_string() } else { e });
                return;
            }
        }
        self.update_status();
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
        let _ = self.jobs.send(Job::Tree { seq, query: self.query_text(), flags: self.flags(), sort, desc });
        self.set_status_left("Adding up folder sizes…");
        let text = w::get_text(self.edit.get());
        w::set_text(self.hwnd.get(), &if text.is_empty() { "Folder sizes - rapidfind".to_string() } else { format!("{text} - folder sizes - rapidfind") });
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
                if restore.is_empty() {
                    // Open the volumes (when there are only a few) one level.
                    let n = self.tree.borrow().nodes.len();
                    if n <= 3 {
                        for i in (0..n).rev() {
                            self.expand(i);
                        }
                    }
                } else {
                    for p in &restore {
                        if let Some(i) = self.tree.borrow().find(p).filter(|&i| !self.tree.borrow().nodes[i].expanded) {
                            self.expand(i);
                        }
                    }
                }
                let n = self.tree.borrow().nodes.len();
                send(list, LVM_SETITEMCOUNT, n, if restore.is_empty() { 0 } else { (LVSICF_NOSCROLL | LVSICF_NOINVALIDATEALL) as isize });
                if let Some(p) = self.reveal.borrow_mut().take() {
                    self.reveal_path(&p);
                }
                unsafe { InvalidateRect(list, null(), 0) };
                self.update_status();
            }
            Err(e) => {
                self.tree.borrow_mut().nodes.clear();
                send(list, LVM_SETITEMCOUNT, 0, 0);
                self.set_status_left(&if e == rapidfind_core::ipc::LOADING { "Index is still loading…".to_string() } else { e });
            }
        }
    }

    /// Fetches node `i`'s children and shows them below it.
    fn expand(&self, i: usize) {
        let Some(node) = self.tree.borrow().nodes.get(i).cloned() else { return };
        if !node.expandable() || node.expanded {
            return;
        }
        let (sort, desc) = self.tree_sort();
        let Some(mut c) = self.conn.try_lock_for(Duration::from_secs(5)) else { return };
        let res = c.be.children(&node.node_ref(), sort, desc, 0, 200_000);
        drop(c);
        match res {
            Ok(level) => {
                // A folder whose only content is one folder opens through to it.
                let drill = level.rows.len() == 1 && level.rows[0].is_dir && level.rows[0].has_children && node.depth < 32;
                self.tree.borrow_mut().expand(i, level.rows);
                if drill {
                    self.expand(i + 1);
                }
            }
            Err(e) if e == "no tree open" => self.build_tree(true), // service restarted
            Err(e) => self.set_status_left(&e),
        }
    }

    fn toggle_node(&self, i: usize) {
        let Some(expanded) = self.tree.borrow().nodes.get(i).map(|n| n.expanded) else { return };
        if expanded {
            self.tree.borrow_mut().collapse(i);
        } else {
            self.expand(i);
        }
        let n = self.tree.borrow().nodes.len();
        let list = self.tree_list.get();
        send(list, LVM_SETITEMCOUNT, n, (LVSICF_NOSCROLL | LVSICF_NOINVALIDATEALL) as isize);
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
                        let end = m.nodes[i + 1..].iter().position(|x| x.depth <= d).map_or(m.nodes.len(), |p| i + 1 + p);
                        i + 1..end
                    }
                };
                let want = at.map_or(0, |i| m.nodes[i].depth + 1);
                range.into_iter().find(|&j| m.nodes[j].depth == want && tv::contains(&m.nodes[j].path(), path))
            };
            let Some(j) = next else { break };
            at = Some(j);
            if self.tree.borrow().find(path) == Some(j) {
                break;
            }
            self.expand(j);
        }
        let n = self.tree.borrow().nodes.len();
        send(self.tree_list.get(), LVM_SETITEMCOUNT, n, LVSICF_NOSCROLL as isize);
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
            i = send(self.tree_list.get(), LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize);
            if i < 0 || out.len() >= 10_000 {
                break;
            }
            out.push(i as usize);
        }
        out
    }

    fn tree_selected_paths(&self) -> Vec<String> {
        let m = self.tree.borrow();
        self.tree_selected().into_iter().filter_map(|i| m.nodes.get(i).map(tv::Node::path)).collect()
    }

    fn update_header_tree(&self) {
        let header = send(self.tree_list.get(), LVM_GETHEADER, 0, 0) as HWND;
        let (key, desc) = self.tree_sort();
        let col = match key {
            TreeSort::Name => 0,
            TreeSort::Size => 1,
            TreeSort::Files => 3,
            TreeSort::Modified => 5,
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
        let what = if m.query.trim().is_empty() { "indexed".to_string() } else { format!("matching \"{}\"", m.query.trim()) };
        let cloud = if m.cloud > 0 { format!(" (+ {} online-only)", fmt::size(m.cloud)) } else { String::new() };
        let mut s = format!("{}{cloud} in {} files {what}  ·  {:.0} ms", fmt::size(m.size), shell::group(m.files), m.elapsed_ms);
        if let [i] = sel[..]
            && let Some(n) = m.nodes.get(i)
        {
            s = format!("{}: {} ({:.1} % of its folder), {} files  ·  {s}", n.path(), fmt::size(n.row.size), n.fraction() * 100.0, shell::group(n.row.files));
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
        if stage != (CDDS_ITEMPREPAINT | CDDS_SUBITEM) || !(cd.iSubItem == 0 || cd.iSubItem == 2) {
            return CDRF_DODEFAULT as LRESULT;
        }
        let i = cd.nmcd.dwItemSpec;
        let Some(node) = self.tree.borrow().nodes.get(i).cloned() else { return CDRF_DODEFAULT as LRESULT };
        let list = self.tree_list.get();
        let rc = tv::cell_rect(list, i, cd.iSubItem);
        let selected = send(list, LVM_GETITEMSTATE, i, LVIS_SELECTED as isize) as u32 & LVIS_SELECTED != 0;
        let focused = unsafe { GetFocus() } == list;
        let p = tv::Paint { hdc: cd.nmcd.hdc, font: self.font.get(), bold: self.bold.get(), selected, classic_sel: selected && focused && !crate::themed() };
        if cd.iSubItem == 0 {
            let icon = self.files.borrow_mut().get(&node.row.name, node.row.is_dir).0;
            let il = self.files.borrow().image_list;
            tv::draw_name(list, &p, &node, rc, icon, il);
        } else {
            tv::draw_bar(list, &p, &node, rc);
        }
        CDRF_SKIPDEFAULT as LRESULT
    }

    fn tree_context_menu(&self, x: i32, y: i32) {
        let paths = self.tree_selected_paths();
        if paths.is_empty() {
            return;
        }
        let single_dir = {
            let m = self.tree.borrow();
            let sel = self.tree_selected();
            (sel.len() == 1).then(|| m.nodes[sel[0]].clone()).filter(|n| n.row.is_dir)
        };
        let mut own: Vec<(u32, &str)> = Vec::new();
        if let Some(n) = &single_dir {
            if n.expandable() {
                own.push(if n.expanded { (CMD_COLLAPSE, "&Collapse") } else { (CMD_EXPAND, "&Expand") });
            }
            own.push((CMD_SEARCH_HERE, "&Search in This Folder"));
            own.push((0, ""));
        }
        own.extend([(CMD_OPEN, "&Open"), (CMD_OPEN_PATH, "Open &Path"), (0, ""), (CMD_COPY_FULL, "Copy &Full Path"), (CMD_COPY_NAME, "Copy &Name"), (0, ""), (CMD_PROPERTIES, "P&roperties")]);
        match shell::context_menu(self.hwnd.get(), &paths, &own, x, y) {
            MenuChoice::Own(id) => self.command(id),
            MenuChoice::Shell => self.build_tree(true), // e.g. deleted: sizes changed
            MenuChoice::None => {}
        }
    }

    fn tree_notify(&self, nm: &NMHDR, lparam: LPARAM) -> Option<LRESULT> {
        match nm.code {
            LVN_GETDISPINFOW => {
                let di = unsafe { &mut *(lparam as *mut NMLVDISPINFOW) };
                let item = &mut di.item;
                let Some(node) = self.tree.borrow().nodes.get(item.iItem as usize).cloned() else { return Some(0) };
                if item.mask & LVIF_TEXT != 0 {
                    let text = tv::cell_text(&node, item.iSubItem);
                    let mut buf = self.disp.borrow_mut();
                    buf.clear();
                    buf.extend(text.encode_utf16().take(item.cchTextMax.max(1) as usize - 1));
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
                            self.glyph_click.set(unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount() });
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
                let i = if ia.iItem >= 0 { ia.iItem as usize } else { self.tree_selected().first().copied().unwrap_or(usize::MAX) };
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
                            } else if let Some(p) = self.tree.borrow().parent_of(i) {
                                self.select_tree_row(p);
                            }
                        }
                    }
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
            NM_CUSTOMDRAW => Some(self.tree_custom_draw(unsafe { &mut *(lparam as *mut NMLVCUSTOMDRAW) })),
            _ => None,
        }
    }

    fn set_status_left(&self, s: &str) {
        let w = wide(s);
        send(self.status.get(), SB_SETTEXTW, 0, w.as_ptr() as isize);
    }

    fn update_status(&self) {
        if self.tree_mode() {
            let s = self.tree_status();
            self.set_status_left(&s);
            return;
        }
        let r = self.results.borrow();
        let selected = send(self.list.get(), LVM_GETSELECTEDCOUNT, 0, 0) as usize;
        let mut s = format!("{} objects", shell::group(r.total as u64));
        if selected > 0 {
            s = format!("{} of {s} selected", shell::group(selected as u64));
        }
        if !w::get_text(self.edit.get()).is_empty() || self.settings.borrow().filter != 0 {
            s.push_str(&format!("  ·  {:.1} ms", r.elapsed_ms));
        }
        drop(r);
        self.set_status_left(&s);
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
        let (seq, missing) = {
            let r = self.results.borrow();
            (r.seq, r.missing(from, to))
        };
        if missing.is_empty() {
            return;
        }
        // Don't stall painting behind a running search.
        let Some(mut c) = self.conn.try_lock_for(Duration::from_millis(50)) else { return };
        if c.seq != seq {
            return; // the session already holds a newer search
        }
        for start in missing {
            match c.be.fetch(start as u64, BLOCK as u32) {
                Ok(p) => self.results.borrow_mut().insert(start, p.rows),
                Err(e) if e == rapidfind_core::ipc::STALE => {
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
            i = send(self.list.get(), LVM_GETNEXTITEM, i as usize, LVNI_SELECTED as isize);
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
                    Row { name: r.name.clone(), dir: r.dir.clone(), size: r.size, modified: r.modified, is_dir: r.is_dir, has_size: r.has_size, ..Default::default() }
                })
                .collect();
        }
        self.selected().into_iter().filter_map(|i| self.row(i)).collect()
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
            CMD_SORT_RUNS => self.set_sort(SortKey::RunCount),
            CMD_SORT_CREATED => self.set_sort(SortKey::Created),
            CMD_OPTIONS => crate::options::open(self),
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
                w::message_box(hwnd, SYNTAX_HELP, "rapidfind search syntax", MB_OK | MB_ICONINFORMATION);
            }
            CMD_ABOUT => {
                w::message_box(
                    hwnd,
                    &format!("rapidfind {}\nInstant filename search for NTFS, ReFS, folders and network shares.", env!("CARGO_PKG_VERSION")),
                    "About rapidfind",
                    MB_OK | MB_ICONINFORMATION,
                );
            }
            CMD_SHOW => self.show(),
            CMD_TREE => self.set_tree_mode(!self.tree_mode()),
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
                s.descending = matches!(key, SortKey::Size | SortKey::Modified | SortKey::Created | SortKey::RunCount);
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
        drop(s);
        w::check_menu(m, CMD_LOGIN, shell::start_on_login());
        let diacritics = self.conn.try_lock().and_then(|mut c| c.be.get_config().ok()).map(|c| c.match_diacritics);
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
            && unsafe { RegisterHotKey(hwnd, HOTKEY_ID, mods | 0x4000 /* MOD_NOREPEAT */, vk) } == 0
        {
            self.set_status_left(&format!("Hotkey {hk} is used by another program"));
        }
    }

    pub fn show(&self) {
        let hwnd = self.hwnd.get();
        unsafe {
            ShowWindow(hwnd, if IsIconic(hwnd) != 0 { SW_RESTORE } else { SW_SHOW });
            SetForegroundWindow(hwnd);
            SetFocus(self.edit.get());
        }
        send(self.edit.get(), EM_SETSEL, 0, -1);
    }

    fn toggle(&self) {
        let hwnd = self.hwnd.get();
        let visible = unsafe { IsWindowVisible(hwnd) != 0 && IsIconic(hwnd) == 0 && GetForegroundWindow() == hwnd };
        if visible {
            unsafe { ShowWindow(hwnd, SW_HIDE) };
        } else {
            self.show();
        }
    }

    fn export(&self) {
        let Some(path) = shell::pick_save_file(self.hwnd.get(), "rapidfind results.csv") else { return };
        let total = self.results.borrow().total;
        let mut out = String::from("Name,Path,Size,Date Modified\r\n");
        let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        let mut c = self.conn.lock();
        let mut off = 0;
        while off < total {
            let Ok(p) = c.be.fetch(off as u64, 10_000) else { break };
            if p.rows.is_empty() {
                break;
            }
            for r in &p.rows {
                out.push_str(&format!("{},{},{},{}\r\n", q(&r.name), q(&r.dir), r.size, fmt::time(r.modified)));
            }
            off += p.rows.len();
        }
        drop(c);
        if let Err(e) = std::fs::write(&path, out) {
            w::message_box(self.hwnd.get(), &format!("Export failed: {e}"), "rapidfind", MB_OK | MB_ICONERROR);
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
        w::move_to(self.tree_btn.get(), pad, pad, bw, eh);
        w::move_to(self.edit.get(), 2 * pad + bw, pad, cw - fw - bw - 4 * pad, eh);
        w::move_to(self.combo.get(), cw - fw - pad, pad, fw, px(hwnd, 300));
        send(self.status.get(), WM_SIZE, 0, 0);
        let mut sr = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        unsafe { GetWindowRect(self.status.get(), &mut sr) };
        let sh = sr.bottom - sr.top;
        let parts = [cw - px(hwnd, 420), -1];
        send(self.status.get(), SB_SETPARTS, 2, parts.as_ptr() as isize);
        let top = pad * 2 + eh;
        w::move_to(self.list.get(), 0, top, cw, ch - top - sh);
        w::move_to(self.tree_list.get(), 0, top, cw, ch - top - sh);
    }

    fn apply_fonts(&self) {
        let hwnd = self.hwnd.get();
        let (f, b) = (w::ui_font(hwnd, false), w::ui_font(hwnd, true));
        let (of, ob) = (self.font.replace(f), self.bold.replace(b));
        for c in [self.edit.get(), self.combo.get(), self.list.get(), self.status.get(), self.tree_btn.get(), self.tree_list.get()] {
            w::set_font(c, f);
        }
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
        let tip = wide("rapidfind");
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
        w::menu_item(menu, CMD_SHOW, "Show rapidfind");
        w::menu_item(menu, CMD_OPTIONS, "Options…");
        w::menu_sep(menu);
        w::menu_item(menu, CMD_EXIT, "Exit");
        let mut pt = POINT { x: 0, y: 0 };
        unsafe {
            GetCursorPos(&mut pt);
            SetForegroundWindow(self.hwnd.get());
            let cmd = TrackPopupMenuEx(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, self.hwnd.get(), null());
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
            MenuChoice::Shell => self.search(true), // e.g. delete/rename: show the effect
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
        if stage != (CDDS_ITEMPREPAINT | CDDS_SUBITEM) || cd.iSubItem > 1 || self.hl_terms.borrow().is_empty() {
            return CDRF_DODEFAULT as LRESULT;
        }
        let i = cd.nmcd.dwItemSpec;
        let Some(row) = self.row(i) else { return CDRF_DODEFAULT as LRESULT };
        let text = if cd.iSubItem == 0 { row.name.clone() } else { row.dir.clone() };
        let ranges = highlight::ranges(&text, &self.hl_terms.borrow());
        if ranges.is_empty() {
            return CDRF_DODEFAULT as LRESULT;
        }
        // Let the list paint background/selection and the icon, then draw our text.
        let list = self.list.get();
        let mut rc = RECT { left: if cd.iSubItem == 0 { LVIR_LABEL as i32 } else { LVIR_BOUNDS as i32 }, top: cd.iSubItem, right: 0, bottom: 0 };
        send(list, LVM_GETSUBITEMRECT, i, &mut rc as *mut _ as isize);
        let hdc = cd.nmcd.hdc;
        unsafe {
            let selected = send(list, LVM_GETITEMSTATE, i, LVIS_SELECTED as isize) as u32 & LVIS_SELECTED != 0;
            let focused = GetFocus() == list;
            let classic_sel = selected && focused && !crate::themed();
            let bg: COLORREF = if classic_sel { GetSysColor(COLOR_HIGHLIGHT) } else { GetSysColor(COLOR_WINDOW) };
            let fg: COLORREF = if classic_sel { GetSysColor(COLOR_HIGHLIGHTTEXT) } else { GetSysColor(COLOR_WINDOWTEXT) };
            if !selected {
                let br = CreateSolidBrush(bg);
                FillRect(hdc, &rc, br);
                DeleteObject(br as _);
            }
            SetBkMode(hdc, TRANSPARENT as i32);
            SetTextColor(hdc, fg);
            // Same text inset the list view uses for its own drawing.
            let edge = windows_sys::Win32::UI::HiDpi::GetSystemMetricsForDpi(SM_CXEDGE, w::dpi(list) as u32);
            let mut x = if cd.iSubItem == 0 { rc.left + edge } else { rc.left + 2 * edge + edge / 2 };
            if cd.iSubItem == 0 {
                // We skip the default paint, so draw the icon too.
                let mut ir = RECT { left: LVIR_ICON as i32, top: 0, right: 0, bottom: 0 };
                send(list, LVM_GETSUBITEMRECT, i, &mut ir as *mut _ as isize);
                let icon = self.files.borrow_mut().get(&row.name, row.is_dir).0;
                let il = self.files.borrow().image_list;
                let sz = windows_sys::Win32::UI::HiDpi::GetSystemMetricsForDpi(SM_CXSMICON, w::dpi(list) as u32);
                ImageList_Draw(il as HIMAGELIST, icon, hdc, ir.left + (ir.right - ir.left - sz) / 2, ir.top + (ir.bottom - ir.top - sz) / 2, ILD_TRANSPARENT);
            }
            for (seg, hot) in highlight::segments(&text, &ranges) {
                let old = SelectObject(hdc, if hot { self.bold.get() } else { self.font.get() } as _);
                let w16: Vec<u16> = seg.encode_utf16().collect();
                let mut sz = windows_sys::Win32::Foundation::SIZE { cx: 0, cy: 0 };
                GetTextExtentPoint32W(hdc, w16.as_ptr(), w16.len() as i32, &mut sz);
                let y = rc.top + (rc.bottom - rc.top - sz.cy) / 2;
                ExtTextOutW(hdc, x, y, ETO_CLIPPED, &rc, w16.as_ptr(), w16.len() as u32, null());
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
                let Some(row) = self.row(item.iItem as usize) else { return Some(0) };
                if item.mask & LVIF_TEXT != 0 {
                    let text = self.cell_text(&row, item.iSubItem);
                    let mut buf = self.disp.borrow_mut();
                    buf.clear();
                    buf.extend(text.encode_utf16().take(item.cchTextMax.max(1) as usize - 1));
                    buf.push(0);
                    item.pszText = buf.as_mut_ptr();
                }
                if item.mask & LVIF_IMAGE != 0 && item.iSubItem == 0 {
                    item.iImage = self.files.borrow_mut().get(&row.name, row.is_dir).0;
                }
                Some(0)
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
                Some(0)
            }
            LVN_BEGINDRAG => {
                let paths: Vec<String> = self.selected_rows().iter().map(|r| r.full_path()).collect();
                shell::drag_out(self.hwnd.get(), &paths);
                Some(0)
            }
            LVN_KEYDOWN => {
                let k = unsafe { &*(lparam as *const NMLVKEYDOWN) };
                let ctrl = unsafe { GetKeyState(VK_CONTROL as i32) } < 0;
                let shift = unsafe { GetKeyState(VK_SHIFT as i32) } < 0;
                if ctrl && k.wVKey == b'C' as u16 {
                    self.copy(if shift { CMD_COPY_NAME } else { CMD_COPY_FULL });
                } else if ctrl && k.wVKey == b'A' as u16 {
                    let mut it: LVITEMW = unsafe { std::mem::zeroed() };
                    it.stateMask = LVIS_SELECTED;
                    it.state = LVIS_SELECTED;
                    send(self.list.get(), LVM_SETITEMSTATE, usize::MAX, &it as *const _ as isize);
                }
                Some(0)
            }
            NM_RCLICK => {
                let mut pt = POINT { x: 0, y: 0 };
                unsafe { GetCursorPos(&mut pt) };
                self.context_menu(pt.x, pt.y);
                Some(0)
            }
            NM_CUSTOMDRAW => Some(self.custom_draw(unsafe { &mut *(lparam as *mut NMLVCUSTOMDRAW) })),
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
        s.column_widths = (0..COLUMNS.len()).map(|c| send(self.list.get(), LVM_GETCOLUMNWIDTH, c, 0) as i32 * 96 / w::dpi(hwnd)).collect();
        s.tree_column_widths = (0..tv::COLUMNS.len()).map(|c| send(self.tree_list.get(), LVM_GETCOLUMNWIDTH, c, 0) as i32 * 96 / w::dpi(hwnd)).collect();
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
runcount:>0\topened from rapidfind
file:  folder:  audio:  video:  pic:  doc:  exe:  zip:";

// ---- window procedures ---------------------------------------------------------

unsafe extern "system" fn edit_proc(h: HWND, msg: u32, w: WPARAM, l: LPARAM, _id: usize, _data: usize) -> LRESULT {
    if msg == WM_KEYDOWN
        && let Some(app) = app()
    {
        match w as u16 {
            k if k == VK_DOWN || k == VK_NEXT => {
                let list = if app.tree_mode() { app.tree_list.get() } else { app.list.get() };
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
                if w::get_text(h).is_empty() {
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
    let Some(app) = app() else { return unsafe { DefWindowProcW(hwnd, msg, w, l) } };
    if matches!(msg, WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR)
        && let Some(r) = shell::forward_menu_message(msg, w, l)
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
            mmi.ptMinTrackSize = POINT { x: px(hwnd, 420), y: px(hwnd, 240) };
            0
        }
        WM_DPICHANGED => {
            let r = unsafe { &*(l as *const RECT) };
            unsafe { SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE) };
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
            app.notify(nm, l).unwrap_or_else(|| unsafe { DefWindowProcW(hwnd, msg, w, l) })
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
        WM_APP_STATUS => {
            let st = unsafe { Box::from_raw(l as *mut StatusInfo) };
            let changed = { app.results.borrow().generation != st.generation };
            let was_loading = app.last_status.borrow().as_ref().is_some_and(|s| !s.ready);
            *app.last_status.borrow_mut() = Some(*st);
            if app.tree_mode() {
                if was_loading && st_ready(&app.last_status.borrow()) {
                    app.search(true);
                }
            } else if (changed || was_loading) && !app.searching.get() {
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
        WM_HOTKEY => {
            app.toggle();
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
            hIconSm: icon,
        };
        RegisterClassExW(&wc);

        let settings = Settings::load();
        let (tx, rx) = channel();
        let conn = Arc::new(Mutex::new(Conn { be, seq: 0 }));
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
            tray_icon: Cell::new(icon),
            taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            tree_btn: Cell::new(null_mut()),
            tree_list: Cell::new(null_mut()),
            tree: RefCell::new(tv::Model::default()),
            tree_seq: Cell::new(0),
            restore: RefCell::new(Vec::new()),
            reveal: RefCell::new(None),
            glyph_click: Cell::new(0),
        }));
        APP.with(|a| *a.borrow_mut() = Some(app));

        let (x, y, cw, ch) = settings.window.unwrap_or((CW_USEDEFAULT, CW_USEDEFAULT, 1180, 720));
        let title = wide("rapidfind");
        let hwnd = CreateWindowExW(0, class.as_ptr(), title.as_ptr(), WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN, x, y, cw, ch, null_mut(), null_mut(), w::hinstance(), null());
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
        let view = w::submenu(bar, "&View");
        w::menu_item(view, CMD_SORT_RUNS, "Sort by &Run Count");
        w::menu_item(view, CMD_SORT_CREATED, "Sort by Date &Created");
        w::menu_sep(view);
        w::menu_item(view, CMD_TREE, "Folder Size &Tree\tCtrl+T");
        w::menu_sep(view);
        w::menu_item(view, CMD_REFRESH, "&Refresh\tF5");
        let tools = w::submenu(bar, "&Tools");
        w::menu_item(tools, CMD_OPTIONS, "&Options…\tCtrl+P");
        w::menu_sep(tools);
        w::menu_item(tools, CMD_BACKGROUND, "Run in &Background (tray)");
        w::menu_item(tools, CMD_LOGIN, "Start with &Windows");
        let help = w::submenu(bar, "&Help");
        w::menu_item(help, CMD_SYNTAX, "Search &Syntax");
        w::menu_item(help, CMD_ABOUT, "&About");
        SetMenu(hwnd, bar);
        app.menu.set(bar);

        // Controls.
        let btn = w::child(hwnd, "Button", "Tree", WS_TABSTOP | (BS_AUTOCHECKBOX | BS_PUSHLIKE) as u32, 0, ID_TREE_BTN);
        app.tree_btn.set(btn);
        let edit = w::child(hwnd, "Edit", "", WS_TABSTOP | ES_AUTOHSCROLL as u32, WS_EX_CLIENTEDGE, ID_SEARCH);
        send(edit, EM_SETCUEBANNER, 1, wide("Search").as_ptr() as isize);
        SetWindowSubclass(edit, Some(edit_proc), 1, 0);
        app.edit.set(edit);

        let combo = w::child(hwnd, "ComboBox", "", WS_TABSTOP | WS_VSCROLL | CBS_DROPDOWNLIST as u32, 0, ID_FILTER);
        for (name, _) in FILTERS {
            send(combo, CB_ADDSTRING, 0, wide(name).as_ptr() as isize);
        }
        send(combo, CB_SETCURSEL, settings.filter.min(FILTERS.len() - 1), 0);
        app.combo.set(combo);

        let list = w::child(hwnd, "SysListView32", "", WS_TABSTOP | (LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS), 0, ID_LIST);
        send(list, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_HEADERDRAGDROP | LVS_EX_LABELTIP) as isize);
        if crate::themed() {
            SetWindowTheme(list, wide("Explorer").as_ptr(), null());
        }
        for (c, (name, width, right)) in COLUMNS.iter().enumerate() {
            let wname = wide(name);
            let mut col: LVCOLUMNW = std::mem::zeroed();
            col.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_FMT;
            col.fmt = if *right { LVCFMT_RIGHT } else { LVCFMT_LEFT };
            let saved = settings.column_widths.get(c).copied().filter(|&w| w > 10).unwrap_or(*width);
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
        let tl = w::child(hwnd, "SysListView32", "", WS_TABSTOP | (LVS_REPORT | LVS_OWNERDATA | LVS_SHOWSELALWAYS), 0, ID_TREE);
        ShowWindow(tl, SW_HIDE);
        send(tl, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, (LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER | LVS_EX_LABELTIP) as isize);
        if crate::themed() {
            SetWindowTheme(tl, wide("Explorer").as_ptr(), null());
        }
        for (c, (name, width, right)) in tv::COLUMNS.iter().enumerate() {
            let wname = wide(name);
            let mut col: LVCOLUMNW = std::mem::zeroed();
            col.mask = LVCF_TEXT | LVCF_WIDTH | LVCF_FMT;
            col.fmt = if *right { LVCFMT_RIGHT } else { LVCFMT_LEFT };
            let saved = settings.tree_column_widths.get(c).copied().filter(|&w| w > 10).unwrap_or(*width);
            col.cx = px(hwnd, saved);
            col.pszText = wname.as_ptr() as *mut u16;
            send(tl, LVM_INSERTCOLUMNW, c, &col as *const _ as isize);
        }
        app.tree_list.set(tl);

        let status = w::child(hwnd, "msctls_statusbar32", "", SBARS_SIZEGRIP, 0, ID_STATUS);
        app.status.set(status);

        app.apply_fonts();
        app.layout();
        app.update_header();
        app.update_header_tree();
        app.sync_menu();
        app.add_tray();
        app.register_hotkey();

        let hw = hwnd as isize;
        std::thread::Builder::new().name("search".into()).spawn(move || worker(conn, rx, hw)).unwrap();
        SetTimer(hwnd, TIMER_POLL, 700, None);

        if !start_hidden {
            ShowWindow(hwnd, SW_SHOW);
            SetFocus(edit);
        }
        if settings.tree_mode {
            app.set_tree_mode(true);
        } else {
            app.search(false);
        }
        app
    }
}

/// Keyboard shortcuts that work anywhere in the main window.
pub fn global_key(msg: &MSG) -> bool {
    let Some(app) = app() else { return false };
    if msg.message != WM_KEYDOWN || unsafe { GetAncestor(msg.hwnd, GA_ROOT) } != app.hwnd.get() {
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
