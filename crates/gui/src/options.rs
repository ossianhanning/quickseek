//! Options window (Tools → Options): index configuration (sent to the
//! service) plus GUI preferences.

use std::cell::RefCell;
use std::ptr::{null, null_mut};

use quickseek_core::config::{Config, FolderConfig, VolumeConfig};
use quickseek_core::ntfs::win as ntwin;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{COLOR_BTNFACE, HBRUSH};
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::app::App;
use crate::shell;
use crate::w::{self, px, send, wide};

pub const CLASS: &str = "quickseek.options";

pub const ID_TABS: i32 = 900;
pub const ID_HOTKEY: i32 = 910;
pub const ID_BACKGROUND: i32 = 911;
pub const ID_LOGIN: i32 = 912;
pub const ID_DIACRITICS: i32 = 913;
pub const ID_AUTO_FIXED: i32 = 920;
pub const ID_AUTO_REMOVABLE: i32 = 921;
pub const ID_VOLUMES: i32 = 922;
pub const ID_VOL_INCLUDE: i32 = 923;
pub const ID_FOLDERS: i32 = 930;
pub const ID_FOLDER_ADD: i32 = 931;
pub const ID_FOLDER_REMOVE: i32 = 932;
pub const ID_RESCAN: i32 = 933;
pub const ID_WATCH: i32 = 934;
pub const ID_FOLDER_PATH: i32 = 935;
pub const ID_FOLDER_ADD_PATH: i32 = 936;
pub const ID_FOLDER_STATE: i32 = 937;
pub const ID_FOLDER_USER: i32 = 938;
pub const ID_FOLDER_PASSWORD: i32 = 939;
pub const ID_FOLDER_LOGIN: i32 = 951;
pub const ID_FOLDER_LOGOUT: i32 = 952;
pub const ID_EX_HIDDEN: i32 = 940;
pub const ID_EX_SYSTEM: i32 = 941;
pub const ID_EX_FOLDERS: i32 = 942;
pub const ID_EX_ADD_FOLDER: i32 = 943;
pub const ID_EX_PATTERN: i32 = 944;
pub const ID_EX_ADD_PATTERN: i32 = 945;
pub const ID_EX_REMOVE: i32 = 946;
pub const ID_EX_FILES: i32 = 947;
pub const ID_INCLUDE_FILES: i32 = 948;
pub const ID_APPLY: i32 = 950;
pub const ID_IX_CREATED: i32 = 960;
/// WM_COMMAND ids 980.. select an options page.
pub const ID_SELECT_TAB: i32 = 980;
pub const ID_IX_SORT_SIZE: i32 = 961;
pub const ID_IX_SORT_MODIFIED: i32 = 962;
pub const ID_IX_SORT_PATH: i32 = 963;
pub const ID_SH_CACHE: i32 = 964;
pub const ID_SH_REWALK: i32 = 965;
pub const ID_SH_CREATED: i32 = 966;
pub const ID_SH_RESCAN: i32 = 967;
pub const ID_NAS_LIST: i32 = 970;
pub const ID_NAS_HOST: i32 = 971;
pub const ID_NAS_CODE: i32 = 972;
pub const ID_NAS_PAIR: i32 = 973;
pub const ID_NAS_REMOVE: i32 = 974;
pub const ID_NAS_TOGGLE: i32 = 975;
pub const ID_NAS_STATE: i32 = 976;
pub const ID_NAS_ALIAS: i32 = 977;
pub const ID_NAS_SET_ALIAS: i32 = 978;

const TABS: [&str; 7] = [
    "General", "Volumes", "Folders", "Exclude", "Index", "Shares", "NAS",
];

struct Opt {
    hwnd: HWND,
    cfg: Config,
    /// (control, tab index) for showing/hiding per tab.
    controls: Vec<(HWND, usize)>,
    font: windows_sys::Win32::Graphics::Gdi::HFONT,
    /// Drive letters in the volume list, in row order.
    vol_rows: Vec<char>,
    /// NAS list rows: (host, share's UNC root for share rows).
    nas_rows: Vec<(String, Option<String>)>,
    filling: bool,
}

thread_local! {
    static OPT: RefCell<Option<Opt>> = const { RefCell::new(None) };
}

/// Control by id: page controls are children of the tab control (so themed
/// checkboxes paint the tab body behind them), the rest of the window.
fn item(hwnd: HWND, id: i32) -> HWND {
    unsafe {
        let h = GetDlgItem(hwnd, id);
        if !h.is_null() {
            return h;
        }
        GetDlgItem(GetDlgItem(hwnd, ID_TABS), id)
    }
}

/// Brush and color of the tab body, sampled from the painted tab control
/// (the theme's color hints don't match what Windows 11 actually draws).
fn pane_brush(tabs: HWND) -> (isize, u32) {
    static PANE: std::sync::OnceLock<(isize, u32)> = std::sync::OnceLock::new();
    if let Some(p) = PANE.get() {
        return *p;
    }
    let sampled = unsafe {
        let rc = w::client_rect(tabs);
        let hdc = windows_sys::Win32::Graphics::Gdi::GetDC(tabs);
        let c =
            windows_sys::Win32::Graphics::Gdi::GetPixel(hdc, rc.right / 2, rc.bottom - px(tabs, 8));
        windows_sys::Win32::Graphics::Gdi::ReleaseDC(tabs, hdc);
        c
    };
    // CLR_INVALID or not painted yet (black): use a light default for now.
    if sampled == 0xFFFF_FFFF || sampled == 0 {
        static FALLBACK: std::sync::OnceLock<isize> = std::sync::OnceLock::new();
        let b = *FALLBACK.get_or_init(|| unsafe {
            windows_sys::Win32::Graphics::Gdi::CreateSolidBrush(0x00F9F9F9) as isize
        });
        return (b, 0x00F9F9F9);
    }
    *PANE.get_or_init(|| {
        (
            unsafe { windows_sys::Win32::Graphics::Gdi::CreateSolidBrush(sampled) as isize },
            sampled,
        )
    })
}

/// Ctrl+Tab / Ctrl+Shift+Tab switch option pages (no dialog manager does it for us).
pub fn handle_key(msg: &MSG) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetKeyState, VK_CONTROL, VK_SHIFT, VK_TAB,
    };
    let Some(hwnd) = OPT.with(|o| o.try_borrow().ok().and_then(|o| o.as_ref().map(|o| o.hwnd)))
    else {
        return false;
    };
    if msg.message != WM_KEYDOWN
        || msg.wParam != VK_TAB as usize
        || unsafe { GetKeyState(VK_CONTROL as i32) } >= 0
    {
        return false;
    }
    if unsafe { GetAncestor(msg.hwnd, GA_ROOT) } != hwnd {
        return false;
    }
    let tabs = item(hwnd, ID_TABS);
    let n = TABS.len() as isize;
    let cur = send(tabs, TCM_GETCURSEL, 0, 0);
    let back = unsafe { GetKeyState(VK_SHIFT as i32) } < 0;
    let next = ((cur + if back { n - 1 } else { 1 }) % n) as usize;
    send(tabs, TCM_SETCURSEL, next, 0);
    show_tab(next);
    true
}

fn ctl(id: i32) -> HWND {
    OPT.with(|o| o.borrow().as_ref().map_or(null_mut(), |o| item(o.hwnd, id)))
}

/// The tab control forwards its children's notifications to the window.
unsafe extern "system" fn tab_proc(
    h: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if matches!(msg, WM_COMMAND | WM_CTLCOLORSTATIC | WM_CTLCOLORBTN)
        || (msg == WM_NOTIFY && unsafe { (*(lp as *const NMHDR)).hwndFrom } != h)
    {
        return unsafe { SendMessageW(GetParent(h), msg, wp, lp) };
    }
    unsafe { windows_sys::Win32::UI::Shell::DefSubclassProc(h, msg, wp, lp) }
}

fn checked(id: i32) -> bool {
    send(ctl(id), BM_GETCHECK, 0, 0) == 1
}

fn set_checked(id: i32, on: bool) {
    send(ctl(id), BM_SETCHECK, on as usize, 0);
}

fn folder_label(f: &FolderConfig) -> String {
    let mut s = f.path.clone();
    let mut extra = Vec::new();
    if f.watch {
        extra.push("watch".to_string());
    }
    if f.rescan_minutes > 0 {
        extra.push(format!("rescan every {} min", f.rescan_minutes));
    }
    if let Some(u) = &f.user {
        extra.push(format!("signs in as {u}"));
    }
    if !extra.is_empty() {
        s.push_str(&format!("   ({})", extra.join(", ")));
    }
    s
}

fn lb_clear(h: HWND) {
    send(h, LB_RESETCONTENT, 0, 0);
}

fn lb_add(h: HWND, s: &str) {
    send(h, LB_ADDSTRING, 0, wide(s).as_ptr() as isize);
}

fn lb_sel(h: HWND) -> Option<usize> {
    usize::try_from(send(h, LB_GETCURSEL, 0, 0)).ok()
}

fn fill_lists(o: &Opt) {
    let folders = item(o.hwnd, ID_FOLDERS);
    let sel = lb_sel(folders);
    lb_clear(folders);
    for f in &o.cfg.folders {
        lb_add(folders, &folder_label(f));
    }
    if let Some(s) = sel.filter(|&s| s < o.cfg.folders.len()) {
        send(folders, LB_SETCURSEL, s, 0);
    }
    let ex = item(o.hwnd, ID_EX_FOLDERS);
    lb_clear(ex);
    for f in &o.cfg.exclude_folders {
        lb_add(ex, f);
    }
}

fn fill(app: &App) {
    let status = app.conn.lock().be.status();
    OPT.with(|o| {
        let mut b = o.borrow_mut();
        let Some(o) = b.as_mut() else { return };
        o.filling = true;
        let s = app.settings.borrow();
        w::set_text(item(o.hwnd, ID_HOTKEY), &s.hotkey);
        drop(s);
        let c = o.cfg.clone();
        let set = |id: i32, on: bool| send(item(o.hwnd, id), BM_SETCHECK, on as usize, 0);
        set(ID_BACKGROUND, app.settings.borrow().run_in_background);
        set(ID_LOGIN, shell::start_on_login());
        set(ID_DIACRITICS, c.match_diacritics);
        set(ID_AUTO_FIXED, c.auto_include_fixed);
        set(ID_AUTO_REMOVABLE, c.auto_include_removable);
        set(ID_EX_HIDDEN, c.exclude_hidden);
        set(ID_EX_SYSTEM, c.exclude_system);
        set(ID_IX_CREATED, c.index_date_created);
        set(ID_IX_SORT_SIZE, c.fast_sort_size);
        set(ID_IX_SORT_MODIFIED, c.fast_sort_modified);
        set(ID_IX_SORT_PATH, c.fast_sort_path);
        set(ID_SH_CACHE, c.share_cache);
        set(ID_SH_REWALK, c.share_rewalk_on_start);
        set(ID_SH_CREATED, c.share_index_date_created);
        w::set_text(
            item(o.hwnd, ID_SH_RESCAN),
            &c.share_rescan_minutes.to_string(),
        );
        w::set_text(item(o.hwnd, ID_EX_FILES), &c.exclude_files.join(";"));
        w::set_text(
            item(o.hwnd, ID_INCLUDE_FILES),
            &c.include_only_files.join(";"),
        );
        fill_lists(o);

        // Volumes: every NTFS/ReFS drive, checked per config (or auto rule).
        let lv = item(o.hwnd, ID_VOLUMES);
        send(lv, LVM_DELETEALLITEMS, 0, 0);
        o.vol_rows.clear();
        for (row, v) in ntwin::list_volumes()
            .into_iter()
            .filter(|v| v.fs == "NTFS" || v.fs == "ReFS")
            .enumerate()
        {
            let entries = status
                .as_ref()
                .and_then(|s| s.volumes.iter().find(|x| x.key == v.letter.to_string()))
                .map(|x| (shell::group(x.entries), x.state.clone()))
                .unwrap_or_default();
            let kind = match v.drive_type {
                ntwin::DRIVE_REMOVABLE => "removable",
                ntwin::DRIVE_FIXED => "fixed",
                _ => "other",
            };
            let cells = [
                format!("{}:", v.letter),
                format!("{} ({kind})", v.fs),
                entries.0,
                entries.1,
            ];
            let t0 = wide(&cells[0]);
            let mut it: LVITEMW = unsafe { std::mem::zeroed() };
            it.mask = LVIF_TEXT;
            it.iItem = row as i32;
            it.pszText = t0.as_ptr() as *mut u16;
            send(lv, LVM_INSERTITEMW, 0, &it as *const _ as isize);
            for (sub, text) in cells.iter().enumerate().skip(1) {
                let t = wide(text);
                let mut s: LVITEMW = unsafe { std::mem::zeroed() };
                s.iSubItem = sub as i32;
                s.pszText = t.as_ptr() as *mut u16;
                send(lv, LVM_SETITEMTEXTW, row, &s as *const _ as isize);
            }
            let auto = match v.drive_type {
                ntwin::DRIVE_FIXED => c.auto_include_fixed,
                ntwin::DRIVE_REMOVABLE => c.auto_include_removable,
                _ => false,
            };
            let include = c
                .volumes
                .get(&v.letter.to_string())
                .map_or(auto, |vc| vc.include);
            let mut st: LVITEMW = unsafe { std::mem::zeroed() };
            st.stateMask = LVIS_STATEIMAGEMASK;
            st.state = (if include { 2 } else { 1 }) << 12;
            send(lv, LVM_SETITEMSTATE, row, &st as *const _ as isize);
            o.vol_rows.push(v.letter);
        }
        // Folder states from the service.
        let mut states: Vec<String> = status
            .map(|s| {
                s.volumes
                    .into_iter()
                    .filter(|v| v.kind == "Folder")
                    .map(|v| {
                        format!(
                            "{}: {} ({} entries)",
                            v.key,
                            v.state,
                            shell::group(v.entries)
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Shares a NAS agent indexes instead (Options → NAS).
        states.extend(c.folders.iter().filter_map(|f| {
            c.remote_share(&f.path)
                .map(|(r, _)| format!("{}: indexed by the NAS agent on {}", f.path, r.host))
        }));
        w::set_text(item(o.hwnd, ID_FOLDER_STATE), &states.join("\n"));
        o.filling = false;
    });
    on_folder_select();
    fill_nas(app);
}

/// The NAS page's list, from the service (agents, their shares and states).
fn fill_nas(app: &App) {
    let list = app
        .conn
        .lock()
        .be
        .remote(&serde_json::json!({ "op": "list" }));
    OPT.with(|o| {
        let mut b = o.borrow_mut();
        let Some(o) = b.as_mut() else { return };
        let lb = item(o.hwnd, ID_NAS_LIST);
        lb_clear(lb);
        o.nas_rows.clear();
        let list = match list {
            Ok(v) => v.as_array().cloned().unwrap_or_default(),
            Err(e) => {
                w::set_text(item(o.hwnd, ID_NAS_STATE), &e);
                return;
            }
        };
        let st = |v: &serde_json::Value, k: &str| v[k].as_str().unwrap_or("").to_string();
        for r in &list {
            let host = st(r, "host");
            lb_add(
                lb,
                &format!(
                    "{host}  (agent {}{})",
                    st(r, "agent"),
                    if r["paired"].as_bool() == Some(true) {
                        ""
                    } else {
                        ", not paired"
                    }
                ),
            );
            o.nas_rows.push((host.clone(), None));
            for sh in r["shares"].as_array().into_iter().flatten() {
                let path = match st(sh, "alias").as_str() {
                    "" => st(sh, "unc"),
                    a => format!("{a}  ({})", st(sh, "unc")),
                };
                lb_add(
                    lb,
                    &format!(
                        "      {}   {}{}   {}",
                        path,
                        st(sh, "state"),
                        match sh["entries"].as_u64() {
                            Some(n) if n > 0 => format!(", {} entries", shell::group(n)),
                            _ => String::new(),
                        },
                        st(sh, "root")
                    ),
                );
                o.nas_rows.push((host.clone(), Some(st(sh, "unc"))));
            }
        }
        if list.is_empty() {
            lb_add(lb, "No NAS agents yet.");
        }
    });
}

/// Pairs, removes or toggles on the NAS page; the service changes the
/// config, so the page's copy of the agent list is refreshed from it.
fn nas_action(app: &App, a: serde_json::Value) {
    let hwnd = OPT
        .with(|o| o.borrow().as_ref().map(|o| o.hwnd))
        .unwrap_or(null_mut());
    let op = a["op"].as_str().unwrap_or("").to_string();
    let wait = unsafe { SetCursor(LoadCursorW(null_mut(), IDC_WAIT)) };
    let res = app.conn.lock().be.remote(&a);
    unsafe { SetCursor(wait) };
    let msg = match &res {
        Ok(v) if op == "add" => format!(
            "Paired with {}. Its shares are being indexed: {}.",
            v["agent"].as_str().unwrap_or("the agent"),
            v["shares"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s["unc"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Ok(_) => "Done.".into(),
        Err(e) => e.clone(),
    };
    let remotes = app.conn.lock().be.get_config().map(|c| c.remotes);
    OPT.with(|o| {
        if let Some(o) = o.borrow_mut().as_mut() {
            if let Ok(r) = remotes {
                o.cfg.remotes = r;
            }
            w::set_text(item(o.hwnd, ID_NAS_STATE), &msg);
            if res.is_ok() && op == "add" {
                w::set_text(item(o.hwnd, ID_NAS_CODE), "");
            }
        }
    });
    if res.is_err() {
        w::message_box(hwnd, &msg, "quickseek", MB_OK | MB_ICONWARNING);
    }
    fill_nas(app);
}

/// Copies control values into the working config.
fn harvest(app: &App) -> Config {
    OPT.with(|o| {
        let mut b = o.borrow_mut();
        let o = b.as_mut().unwrap();
        let get = |id| item(o.hwnd, id);
        let chk = |id| send(get(id), BM_GETCHECK, 0, 0) == 1;
        let list = |id| -> Vec<String> {
            w::get_text(get(id))
                .split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        };
        let mut c = o.cfg.clone();
        c.match_diacritics = chk(ID_DIACRITICS);
        c.auto_include_fixed = chk(ID_AUTO_FIXED);
        c.auto_include_removable = chk(ID_AUTO_REMOVABLE);
        c.exclude_hidden = chk(ID_EX_HIDDEN);
        c.exclude_system = chk(ID_EX_SYSTEM);
        c.index_date_created = chk(ID_IX_CREATED);
        c.fast_sort_size = chk(ID_IX_SORT_SIZE);
        c.fast_sort_modified = chk(ID_IX_SORT_MODIFIED);
        c.fast_sort_path = chk(ID_IX_SORT_PATH);
        c.share_cache = chk(ID_SH_CACHE);
        c.share_rewalk_on_start = chk(ID_SH_REWALK);
        c.share_index_date_created = chk(ID_SH_CREATED);
        c.share_rescan_minutes = w::get_text(get(ID_SH_RESCAN))
            .trim()
            .parse()
            .unwrap_or(c.share_rescan_minutes);
        c.exclude_files = list(ID_EX_FILES);
        c.include_only_files = list(ID_INCLUDE_FILES);
        let lv = get(ID_VOLUMES);
        for (row, &letter) in o.vol_rows.iter().enumerate() {
            let st = send(lv, LVM_GETITEMSTATE, row, LVIS_STATEIMAGEMASK as isize) as u32;
            let on = (st >> 12) == 2;
            let e = c
                .volumes
                .entry(letter.to_string())
                .or_insert_with(VolumeConfig::default);
            e.include = on;
        }
        let mut s = app.settings.borrow_mut();
        s.hotkey = w::get_text(get(ID_HOTKEY)).trim().to_string();
        s.run_in_background = chk(ID_BACKGROUND);
        o.cfg = c.clone();
        c
    })
}

fn apply(app: &App) -> bool {
    let c = harvest(app);
    let hk = app.settings.borrow().hotkey.clone();
    if !hk.is_empty() && crate::settings::parse_hotkey(&hk).is_none() {
        let h = OPT
            .with(|o| o.borrow().as_ref().map(|o| o.hwnd))
            .unwrap_or(null_mut());
        w::message_box(
            h,
            &format!("\"{hk}\" isn't a valid hotkey (e.g. Ctrl+Alt+Space, Win+Shift+F, F12)."),
            "quickseek",
            MB_OK | MB_ICONWARNING,
        );
        return false;
    }
    let res = app.conn.lock().be.set_config(&c);
    if let Err(e) = res {
        let h = OPT
            .with(|o| o.borrow().as_ref().map(|o| o.hwnd))
            .unwrap_or(null_mut());
        w::message_box(
            h,
            &format!("Saving the index options failed:\n{e}"),
            "quickseek",
            MB_OK | MB_ICONERROR,
        );
        return false;
    }
    if checked(ID_LOGIN) != shell::start_on_login() {
        shell::set_start_on_login(checked(ID_LOGIN));
    }
    app.settings.borrow().save();
    app.register_hotkey();
    app.sync_menu();
    app.search(true);
    true
}

fn selected_volume(o: &Opt) -> Option<char> {
    let lv = item(o.hwnd, ID_VOLUMES);
    let i = send(lv, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize);
    usize::try_from(i)
        .ok()
        .and_then(|i| o.vol_rows.get(i))
        .copied()
}

fn on_volume_select() {
    OPT.with(|o| {
        let mut b = o.borrow_mut();
        let Some(o) = b.as_mut() else { return };
        let edit = item(o.hwnd, ID_VOL_INCLUDE);
        let sel = selected_volume(o);
        unsafe { EnableWindow(edit, sel.is_some() as i32) };
        o.filling = true;
        let text = sel
            .map(|l| o.cfg.volume(l).include_only.join(";"))
            .unwrap_or_default();
        w::set_text(edit, &text);
        o.filling = false;
    });
}

fn on_folder_select() {
    OPT.with(|o| {
        let mut b = o.borrow_mut();
        let Some(o) = b.as_mut() else { return };
        o.filling = true;
        let lb = item(o.hwnd, ID_FOLDERS);
        let sel = lb_sel(lb).and_then(|i| o.cfg.folders.get(i)).cloned();
        let (re, wa) = (item(o.hwnd, ID_RESCAN), item(o.hwnd, ID_WATCH));
        unsafe {
            EnableWindow(re, sel.is_some() as i32);
            EnableWindow(wa, sel.is_some() as i32);
        }
        if let Some(f) = &sel {
            w::set_text(re, &f.rescan_minutes.to_string());
            send(wa, BM_SETCHECK, f.watch as usize, 0);
        }
        // Logins are for shares only.
        let share = sel
            .as_ref()
            .is_some_and(|f| ntwin::is_network_path(&f.path));
        for id in [ID_FOLDER_USER, ID_FOLDER_PASSWORD, ID_FOLDER_LOGIN] {
            unsafe { EnableWindow(item(o.hwnd, id), share as i32) };
        }
        let has_login = sel.as_ref().is_some_and(|f| f.user.is_some());
        unsafe { EnableWindow(item(o.hwnd, ID_FOLDER_LOGOUT), (share && has_login) as i32) };
        let user = sel.and_then(|f| f.user).unwrap_or_default();
        w::set_text(item(o.hwnd, ID_FOLDER_USER), &user);
        w::set_text(item(o.hwnd, ID_FOLDER_PASSWORD), "");
        o.filling = false;
    });
}

/// Gives the selected share its own login (`set`) or forgets it, through
/// the service. Pending changes are applied first: the service only keeps
/// logins for folders it indexes.
fn set_folder_login(app: &App, set: bool) {
    let (hwnd, path) = OPT.with(|o| {
        let b = o.borrow();
        let o = b.as_ref().unwrap();
        let path = lb_sel(item(o.hwnd, ID_FOLDERS))
            .and_then(|i| o.cfg.folders.get(i))
            .map(|f| f.path.clone());
        (o.hwnd, path)
    });
    let Some(path) = path else { return };
    let user = if set {
        w::get_text(item(hwnd, ID_FOLDER_USER)).trim().to_string()
    } else {
        String::new()
    };
    let password =
        quickseek_core::credstore::Secret::new(w::get_text(item(hwnd, ID_FOLDER_PASSWORD)));
    w::set_text(item(hwnd, ID_FOLDER_PASSWORD), "");
    if set && (user.is_empty() || password.is_empty()) {
        crate::msgbox::show(
            hwnd,
            "Type the user name and password the share expects.",
            "Quickseek",
            MB_ICONWARNING,
        );
        return;
    }
    if !apply(app) {
        return;
    }
    let res = app.conn.lock().be.folder_login(&path, &user, password);
    match res {
        Ok(()) => {
            OPT.with(|o| {
                let mut b = o.borrow_mut();
                let o = b.as_mut().unwrap();
                if let Some(f) = o.cfg.folders.iter_mut().find(|f| f.path == path) {
                    f.user = set.then(|| user.clone());
                }
            });
            OPT.with(|o| {
                let mut b = o.borrow_mut();
                let o = b.as_mut().unwrap();
                let lb = item(o.hwnd, ID_FOLDERS);
                let sel = lb_sel(lb);
                fill_lists(o);
                if let Some(i) = sel {
                    send(lb, LB_SETCURSEL, i, 0);
                }
            });
            on_folder_select();
            let text = if set {
                format!("Quickseek now signs in to {path} as {user}.")
            } else {
                format!("{path} is reached as you again.")
            };
            crate::msgbox::show(hwnd, &text, "Quickseek", MB_ICONINFORMATION);
        }
        Err(e) => {
            crate::msgbox::show(hwnd, &e, "Quickseek", MB_ICONERROR);
        }
    }
}

fn add_folder(path: String) {
    let unc = ntwin::universal_path(path.trim());
    let path = quickseek_core::folder::root_name(&unc);
    if path.is_empty() {
        return;
    }
    if let Err(e) = quickseek_core::folder::check_root(&path) {
        let owner = OPT.with(|o| o.borrow().as_ref().map_or(null_mut(), |o| o.hwnd));
        crate::msgbox::show(owner, &e, "Quickseek", MB_ICONWARNING);
        return;
    }
    OPT.with(|o| {
        let mut b = o.borrow_mut();
        let o = b.as_mut().unwrap();
        if !o
            .cfg
            .folders
            .iter()
            .any(|f| f.path.eq_ignore_ascii_case(&path))
        {
            let network = ntwin::is_network_path(&path);
            // Remote shares: also rescan hourly in case change notifications
            // are lost (servers that don't support them, dropped links).
            o.cfg.folders.push(FolderConfig {
                path,
                watch: true,
                rescan_minutes: if network {
                    o.cfg.share_rescan_minutes
                } else {
                    0
                },
                user: None,
            });
        }
        fill_lists(o);
        let lb = item(o.hwnd, ID_FOLDERS);
        send(lb, LB_SETCURSEL, o.cfg.folders.len() - 1, 0);
    });
    on_folder_select();
}

fn show_tab(i: usize) {
    OPT.with(|o| {
        if let Some(o) = o.borrow().as_ref() {
            for &(h, t) in &o.controls {
                unsafe { ShowWindow(h, if t == i { SW_SHOW } else { SW_HIDE }) };
            }
        }
    });
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(app) = crate::app::app() else {
        return unsafe { DefWindowProcW(hwnd, msg, wp, lp) };
    };
    match msg {
        WM_NOTIFY => {
            let nm = unsafe { &*(lp as *const NMHDR) };
            if OPT.with(|o| o.try_borrow_mut().is_err()) {
                return 0;
            }
            if nm.idFrom == ID_TABS as usize && nm.code == TCN_SELCHANGE {
                show_tab(send(ctl(ID_TABS), TCM_GETCURSEL, 0, 0) as usize);
            }
            if nm.idFrom == ID_VOLUMES as usize && nm.code == LVN_ITEMCHANGED {
                on_volume_select();
            }
            0
        }
        WM_COMMAND => {
            let (id, code) = (w::loword(wp) as i32, w::hiword(wp));
            // Notifications sent back while we're updating our own controls
            // (state borrowed) are not user actions.
            let busy = OPT.with(|o| o.try_borrow_mut().is_err());
            if busy {
                return 0;
            }
            let filling = OPT.with(|o| o.borrow().as_ref().is_some_and(|o| o.filling));
            match id {
                1 => {
                    // OK
                    if apply(app) {
                        unsafe { DestroyWindow(hwnd) };
                    }
                }
                2 => unsafe {
                    DestroyWindow(hwnd);
                },
                ID_APPLY => {
                    apply(app);
                }
                // Select page N (scripting/screenshot hook; no keystrokes needed).
                id if (ID_SELECT_TAB..ID_SELECT_TAB + TABS.len() as i32).contains(&id) => {
                    let t = (id - ID_SELECT_TAB) as usize;
                    send(ctl(ID_TABS), TCM_SETCURSEL, t, 0);
                    show_tab(t);
                }
                ID_FOLDERS if code == LBN_SELCHANGE => on_folder_select(),
                ID_NAS_LIST if code == LBN_SELCHANGE => OPT.with(|o| {
                    let b = o.borrow();
                    let o = b.as_ref().unwrap();
                    let row = lb_sel(item(o.hwnd, ID_NAS_LIST)).and_then(|i| o.nas_rows.get(i));
                    let alias = match row {
                        Some((_, Some(unc))) => o
                            .cfg
                            .remotes
                            .iter()
                            .flat_map(|r| &r.shares)
                            .find(|s| s.is_root(unc))
                            .map(|s| s.alias.clone())
                            .unwrap_or_default(),
                        _ => String::new(),
                    };
                    w::set_text(item(o.hwnd, ID_NAS_ALIAS), &alias);
                }),
                ID_NAS_SET_ALIAS => {
                    let row = OPT.with(|o| {
                        let b = o.borrow();
                        let o = b.as_ref().unwrap();
                        lb_sel(item(o.hwnd, ID_NAS_LIST)).and_then(|i| o.nas_rows.get(i).cloned())
                    });
                    match row {
                        Some((_, Some(unc))) => {
                            // Checked here, in the user's session: the service
                            // can't see mapped drives.
                            let typed = w::get_text(ctl(ID_NAS_ALIAS));
                            match quickseek_core::remote::check_alias(&unc, &typed) {
                                Ok(alias) => nas_action(
                                    app,
                                    serde_json::json!({ "op": "alias", "unc": unc, "alias": alias }),
                                ),
                                Err(e) => {
                                    w::message_box(hwnd, &e, "quickseek", MB_OK | MB_ICONWARNING);
                                }
                            }
                        }
                        _ => {
                            w::message_box(
                                hwnd,
                                "Select one of the agent's shares first.",
                                "quickseek",
                                MB_OK | MB_ICONINFORMATION,
                            );
                        }
                    }
                }
                ID_NAS_PAIR => {
                    let host = w::get_text(ctl(ID_NAS_HOST)).trim().to_string();
                    let code = w::get_text(ctl(ID_NAS_CODE)).trim().to_string();
                    if host.is_empty() || code.is_empty() {
                        w::message_box(
                            hwnd,
                            "Enter the NAS's name (e.g. mynas.local) and the pairing code its agent shows.",
                            "quickseek",
                            MB_OK | MB_ICONINFORMATION,
                        );
                    } else {
                        nas_action(
                            app,
                            serde_json::json!({ "op": "add", "host": host, "code": code }),
                        );
                    }
                }
                ID_NAS_REMOVE | ID_NAS_TOGGLE => {
                    let row = OPT.with(|o| {
                        let b = o.borrow();
                        let o = b.as_ref().unwrap();
                        lb_sel(item(o.hwnd, ID_NAS_LIST)).and_then(|i| o.nas_rows.get(i).cloned())
                    });
                    match (id, row) {
                        (ID_NAS_REMOVE, Some((host, _))) => {
                            let ok = w::message_box(
                                hwnd,
                                &format!("Stop indexing the shares of {host} through its agent?"),
                                "quickseek",
                                MB_OKCANCEL | MB_ICONQUESTION,
                            );
                            if ok == IDOK {
                                nas_action(
                                    app,
                                    serde_json::json!({ "op": "remove", "host": host }),
                                );
                            }
                        }
                        (ID_NAS_TOGGLE, Some((_, Some(unc)))) => {
                            let on = OPT.with(|o| {
                                o.borrow().as_ref().is_some_and(|o| {
                                    o.cfg
                                        .remotes
                                        .iter()
                                        .flat_map(|r| &r.shares)
                                        .any(|s| s.unc == unc && !s.enabled)
                                })
                            });
                            nas_action(
                                app,
                                serde_json::json!({ "op": "enable", "unc": unc, "enabled": on }),
                            );
                        }
                        _ => {}
                    }
                }
                ID_VOL_INCLUDE if code == EN_CHANGE && !filling => OPT.with(|o| {
                    let mut b = o.borrow_mut();
                    let o = b.as_mut().unwrap();
                    if let Some(letter) = selected_volume(o) {
                        let list: Vec<String> = w::get_text(item(o.hwnd, ID_VOL_INCLUDE))
                            .split(';')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                            .collect();
                        o.cfg
                            .volumes
                            .entry(letter.to_string())
                            .or_insert_with(VolumeConfig::default)
                            .include_only = list;
                    }
                }),
                ID_FOLDER_LOGIN => set_folder_login(app, true),
                ID_FOLDER_LOGOUT => set_folder_login(app, false),
                ID_FOLDER_ADD => {
                    if let Some(p) = shell::pick_folder(hwnd) {
                        add_folder(p);
                    }
                }
                ID_FOLDER_ADD_PATH => {
                    let p = w::get_text(ctl(ID_FOLDER_PATH));
                    if !p.trim().is_empty() {
                        add_folder(p);
                        w::set_text(ctl(ID_FOLDER_PATH), "");
                    }
                }
                ID_FOLDER_REMOVE => OPT.with(|o| {
                    let mut b = o.borrow_mut();
                    let o = b.as_mut().unwrap();
                    if let Some(i) =
                        lb_sel(item(o.hwnd, ID_FOLDERS)).filter(|&i| i < o.cfg.folders.len())
                    {
                        o.cfg.folders.remove(i);
                        fill_lists(o);
                    }
                }),
                ID_RESCAN | ID_WATCH if !filling && (code == EN_CHANGE || code == BN_CLICKED) => {
                    OPT.with(|o| {
                        let mut b = o.borrow_mut();
                        let o = b.as_mut().unwrap();
                        let lb = item(o.hwnd, ID_FOLDERS);
                        if let Some(i) = lb_sel(lb).filter(|&i| i < o.cfg.folders.len()) {
                            let f = &mut o.cfg.folders[i];
                            f.rescan_minutes = w::get_text(item(o.hwnd, ID_RESCAN))
                                .trim()
                                .parse()
                                .unwrap_or(0);
                            f.watch = send(item(o.hwnd, ID_WATCH), BM_GETCHECK, 0, 0) == 1;
                            let label = wide(&folder_label(f));
                            o.filling = true;
                            send(lb, LB_DELETESTRING, i, 0);
                            send(lb, LB_INSERTSTRING, i, label.as_ptr() as isize);
                            send(lb, LB_SETCURSEL, i, 0);
                            o.filling = false;
                        }
                    })
                }
                ID_EX_ADD_FOLDER => {
                    if let Some(p) = shell::pick_folder(hwnd) {
                        OPT.with(|o| {
                            let mut b = o.borrow_mut();
                            let o = b.as_mut().unwrap();
                            o.cfg.exclude_folders.push(ntwin::universal_path(&p));
                            fill_lists(o);
                        });
                    }
                }
                ID_EX_ADD_PATTERN => {
                    let p = w::get_text(ctl(ID_EX_PATTERN)).trim().to_string();
                    if !p.is_empty() {
                        OPT.with(|o| {
                            let mut b = o.borrow_mut();
                            let o = b.as_mut().unwrap();
                            o.cfg.exclude_folders.push(p);
                            fill_lists(o);
                        });
                        w::set_text(ctl(ID_EX_PATTERN), "");
                    }
                }
                ID_EX_REMOVE => OPT.with(|o| {
                    let mut b = o.borrow_mut();
                    let o = b.as_mut().unwrap();
                    if let Some(i) = lb_sel(item(o.hwnd, ID_EX_FOLDERS))
                        .filter(|&i| i < o.cfg.exclude_folders.len())
                    {
                        o.cfg.exclude_folders.remove(i);
                        fill_lists(o);
                    }
                }),
                _ => {}
            }
            0
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            // Labels and checkboxes sit on the tab body: paint them in its color.
            let (brush, color) = pane_brush(item(hwnd, ID_TABS));
            unsafe {
                windows_sys::Win32::Graphics::Gdi::SetBkMode(wp as _, 1);
                windows_sys::Win32::Graphics::Gdi::SetBkColor(wp as _, color);
            }
            brush as LRESULT
        }
        WM_CLOSE => {
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_DESTROY => {
            OPT.with(|o| {
                if let Some(o) = o.borrow_mut().take() {
                    unsafe { windows_sys::Win32::Graphics::Gdi::DeleteObject(o.font as _) };
                }
            });
            unsafe { EnableWindow(app.hwnd.get(), 1) };
            unsafe { SetForegroundWindow(app.hwnd.get()) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Opens the options window (or focuses it if already open).
pub fn open(app: &App) {
    if let Some(h) = OPT.with(|o| o.borrow().as_ref().map(|o| o.hwnd)) {
        unsafe { SetForegroundWindow(h) };
        return;
    }
    let cfg = match app.conn.lock().be.get_config() {
        Ok(c) => c,
        Err(e) => {
            w::message_box(
                app.hwnd.get(),
                &format!("Can't read the index options:\n{e}"),
                "quickseek",
                MB_OK | MB_ICONERROR,
            );
            return;
        }
    };
    unsafe {
        let class = wide(CLASS);
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: w::hinstance(),
            hIcon: crate::app_icon(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_BTNFACE + 1) as usize as HBRUSH,
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
            hIconSm: crate::app_icon_small(),
        };
        RegisterClassExW(&wc);
        let owner = app.hwnd.get();
        let s = |v| px(owner, v);
        let title = wide("Quickseek Options");
        let hwnd = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class.as_ptr(),
            title.as_ptr(),
            WS_CAPTION | WS_SYSMENU | WS_POPUP | WS_CLIPCHILDREN,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            s(640),
            s(560),
            owner,
            null_mut(),
            w::hinstance(),
            null(),
        );
        let font = w::ui_font(hwnd, false);
        OPT.with(|o| {
            *o.borrow_mut() = Some(Opt {
                hwnd,
                cfg,
                controls: Vec::new(),
                font,
                vol_rows: Vec::new(),
                nas_rows: Vec::new(),
                filling: false,
            })
        });

        let rc = w::client_rect(hwnd);
        let (cw, ch) = (rc.right, rc.bottom);
        let tab_parent: std::cell::Cell<HWND> = std::cell::Cell::new(null_mut());
        let add = |class: &str,
                   text: &str,
                   style: u32,
                   ex: u32,
                   id: i32,
                   tab: Option<usize>,
                   x: i32,
                   y: i32,
                   wd: i32,
                   ht: i32|
         -> HWND {
            // Page controls live inside the tab control (coordinates relative to it).
            let (parent, ox, oy) = match (tab, tab_parent.get()) {
                (Some(_), p) if !p.is_null() => (p, 8, 8),
                _ => (hwnd, 0, 0),
            };
            let h = w::child(parent, class, text, style, ex, id);
            w::move_to(h, s(x - ox), s(y - oy), s(wd), s(ht));
            w::set_font(h, font);
            if let Some(t) = tab {
                OPT.with(|o| o.borrow_mut().as_mut().unwrap().controls.push((h, t)));
            }
            h
        };
        let chk = (BS_AUTOCHECKBOX as u32) | WS_TABSTOP;
        let btn = (BS_PUSHBUTTON as u32) | WS_TABSTOP;
        let edit_style = WS_TABSTOP | ES_AUTOHSCROLL as u32;
        let lb_style = WS_TABSTOP | WS_VSCROLL | (LBS_NOTIFY | LBS_NOINTEGRALHEIGHT) as u32;
        let wdt = cw * 96 / w::dpi(hwnd);
        let full = wdt - 48;

        let tabs = add(
            "SysTabControl32",
            "",
            WS_TABSTOP | WS_CLIPSIBLINGS | WS_CLIPCHILDREN,
            WS_EX_CONTROLPARENT,
            ID_TABS,
            None,
            8,
            8,
            wdt - 16,
            470,
        );
        windows_sys::Win32::UI::Shell::SetWindowSubclass(tabs, Some(tab_proc), 1, 0);
        tab_parent.set(tabs);
        for (i, t) in TABS.iter().enumerate() {
            let tw = wide(t);
            let mut item: TCITEMW = std::mem::zeroed();
            item.mask = TCIF_TEXT;
            item.pszText = tw.as_ptr() as *mut u16;
            send(tabs, TCM_INSERTITEMW, i, &item as *const _ as isize);
        }
        let (x0, y0) = (24, 44);

        // General
        add(
            "Static",
            "Global hotkey to show/hide Quickseek (e.g. Ctrl+Alt+Space; empty = none):",
            0,
            0,
            -1,
            Some(0),
            x0,
            y0,
            full,
            20,
        );
        add(
            "Edit",
            "",
            edit_style,
            WS_EX_CLIENTEDGE,
            ID_HOTKEY,
            Some(0),
            x0,
            y0 + 22,
            220,
            24,
        );
        add(
            "Button",
            "Run in background (keep running in the notification area when closed)",
            chk,
            0,
            ID_BACKGROUND,
            Some(0),
            x0,
            y0 + 60,
            full,
            22,
        );
        add(
            "Button",
            "Start Quickseek when Windows starts",
            chk,
            0,
            ID_LOGIN,
            Some(0),
            x0,
            y0 + 86,
            full,
            22,
        );
        add(
            "Button",
            "Match diacritics (å, é, ü are different from a, e, u)",
            chk,
            0,
            ID_DIACRITICS,
            Some(0),
            x0,
            y0 + 112,
            full,
            22,
        );

        // Volumes
        add(
            "Button",
            "Automatically include new fixed volumes",
            chk,
            0,
            ID_AUTO_FIXED,
            Some(1),
            x0,
            y0,
            full,
            22,
        );
        add(
            "Button",
            "Automatically include new removable volumes",
            chk,
            0,
            ID_AUTO_REMOVABLE,
            Some(1),
            x0,
            y0 + 26,
            full,
            22,
        );
        add(
            "Static",
            "NTFS / ReFS volumes (checked = indexed):",
            0,
            0,
            -1,
            Some(1),
            x0,
            y0 + 60,
            full,
            20,
        );
        let lv = add(
            "SysListView32",
            "",
            WS_TABSTOP | (LVS_REPORT | LVS_SINGLESEL | LVS_NOSORTHEADER | LVS_SHOWSELALWAYS),
            WS_EX_CLIENTEDGE,
            ID_VOLUMES,
            Some(1),
            x0,
            y0 + 82,
            full,
            250,
        );
        add(
            "Static",
            "Include only these folders on the selected volume (; separated, empty = whole volume):",
            0,
            0,
            -1,
            Some(1),
            x0,
            y0 + 344,
            full,
            20,
        );
        add(
            "Edit",
            "",
            edit_style,
            WS_EX_CLIENTEDGE,
            ID_VOL_INCLUDE,
            Some(1),
            x0,
            y0 + 366,
            full,
            24,
        );
        send(
            lv,
            LVM_SETEXTENDEDLISTVIEWSTYLE,
            0,
            (LVS_EX_CHECKBOXES | LVS_EX_FULLROWSELECT) as isize,
        );
        for (i, (name, width)) in [
            ("Volume", 80),
            ("File system", 160),
            ("Entries", 120),
            ("State", 160),
        ]
        .iter()
        .enumerate()
        {
            let wn = wide(name);
            let mut col: LVCOLUMNW = std::mem::zeroed();
            col.mask = LVCF_TEXT | LVCF_WIDTH;
            col.cx = s(*width);
            col.pszText = wn.as_ptr() as *mut u16;
            send(lv, LVM_INSERTCOLUMNW, i, &col as *const _ as isize);
        }

        // Folders
        add(
            "Static",
            "Folders and network shares, indexed by walking them (mapped drives are stored as UNC paths):",
            0,
            0,
            -1,
            Some(2),
            x0,
            y0,
            full,
            20,
        );
        add(
            "ListBox",
            "",
            lb_style,
            WS_EX_CLIENTEDGE,
            ID_FOLDERS,
            Some(2),
            x0,
            y0 + 22,
            full - 130,
            170,
        );
        add(
            "Button",
            "Add Folder…",
            btn,
            0,
            ID_FOLDER_ADD,
            Some(2),
            x0 + full - 120,
            y0 + 22,
            120,
            28,
        );
        add(
            "Button",
            "Remove",
            btn,
            0,
            ID_FOLDER_REMOVE,
            Some(2),
            x0 + full - 120,
            y0 + 56,
            120,
            28,
        );
        add(
            "Edit",
            "",
            edit_style,
            WS_EX_CLIENTEDGE,
            ID_FOLDER_PATH,
            Some(2),
            x0,
            y0 + 198,
            full - 130,
            28,
        );
        add(
            "Button",
            "Add Path",
            btn,
            0,
            ID_FOLDER_ADD_PATH,
            Some(2),
            x0 + full - 120,
            y0 + 198,
            120,
            28,
        );
        add(
            "Static",
            "Rescan every (minutes, 0 = only when needed):",
            0x200, /* SS_CENTERIMAGE */
            0,
            -1,
            Some(2),
            x0,
            y0 + 236,
            300,
            24,
        );
        add(
            "Edit",
            "0",
            edit_style | ES_NUMBER as u32,
            WS_EX_CLIENTEDGE,
            ID_RESCAN,
            Some(2),
            x0 + 310,
            y0 + 236,
            80,
            24,
        );
        add(
            "Button",
            "Watch for changes",
            chk,
            0,
            ID_WATCH,
            Some(2),
            x0,
            y0 + 268,
            full,
            22,
        );
        // A share's own login (the service keeps the password).
        add(
            "Static",
            "Sign in as:",
            0x200, /* SS_CENTERIMAGE */
            0,
            -1,
            Some(2),
            x0,
            y0 + 300,
            72,
            24,
        );
        add(
            "Edit",
            "",
            edit_style,
            WS_EX_CLIENTEDGE,
            ID_FOLDER_USER,
            Some(2),
            x0 + 76,
            y0 + 300,
            130,
            24,
        );
        add(
            "Static",
            "Password:",
            0x200, /* SS_CENTERIMAGE */
            0,
            -1,
            Some(2),
            x0 + 214,
            y0 + 300,
            62,
            24,
        );
        add(
            "Edit",
            "",
            edit_style | ES_PASSWORD as u32,
            WS_EX_CLIENTEDGE,
            ID_FOLDER_PASSWORD,
            Some(2),
            x0 + 280,
            y0 + 300,
            120,
            24,
        );
        add(
            "Button",
            "Set Login",
            btn,
            0,
            ID_FOLDER_LOGIN,
            Some(2),
            x0 + 408,
            y0 + 298,
            80,
            28,
        );
        add(
            "Button",
            "Clear",
            btn,
            0,
            ID_FOLDER_LOGOUT,
            Some(2),
            x0 + 492,
            y0 + 298,
            70,
            28,
        );
        add(
            "Static",
            "",
            0,
            0,
            ID_FOLDER_STATE,
            Some(2),
            x0,
            y0 + 336,
            full,
            84,
        );
        send(
            item(hwnd, ID_FOLDER_PATH),
            EM_SETCUEBANNER,
            1,
            wide(r"\\server\share\folder").as_ptr() as isize,
        );
        send(
            item(hwnd, ID_FOLDER_USER),
            EM_SETCUEBANNER,
            1,
            wide("user on the share").as_ptr() as isize,
        );

        // Exclude
        add(
            "Button",
            "Exclude hidden files and folders",
            chk,
            0,
            ID_EX_HIDDEN,
            Some(3),
            x0,
            y0,
            full,
            22,
        );
        add(
            "Button",
            "Exclude system files and folders",
            chk,
            0,
            ID_EX_SYSTEM,
            Some(3),
            x0,
            y0 + 26,
            full,
            22,
        );
        add(
            "Static",
            "Exclude folders (full paths, or folder names / patterns like node_modules, .git, *cache*):",
            0,
            0,
            -1,
            Some(3),
            x0,
            y0 + 58,
            full,
            20,
        );
        add(
            "ListBox",
            "",
            lb_style,
            WS_EX_CLIENTEDGE,
            ID_EX_FOLDERS,
            Some(3),
            x0,
            y0 + 80,
            full - 130,
            150,
        );
        add(
            "Button",
            "Add Folder…",
            btn,
            0,
            ID_EX_ADD_FOLDER,
            Some(3),
            x0 + full - 120,
            y0 + 80,
            120,
            28,
        );
        add(
            "Button",
            "Remove",
            btn,
            0,
            ID_EX_REMOVE,
            Some(3),
            x0 + full - 120,
            y0 + 114,
            120,
            28,
        );
        add(
            "Edit",
            "",
            edit_style,
            WS_EX_CLIENTEDGE,
            ID_EX_PATTERN,
            Some(3),
            x0,
            y0 + 236,
            full - 130,
            28,
        );
        add(
            "Button",
            "Add Pattern",
            btn,
            0,
            ID_EX_ADD_PATTERN,
            Some(3),
            x0 + full - 120,
            y0 + 236,
            120,
            28,
        );
        add(
            "Static",
            "Exclude files (wildcards, ; separated, e.g. *.tmp;~$*):",
            0,
            0,
            -1,
            Some(3),
            x0,
            y0 + 276,
            full,
            20,
        );
        add(
            "Edit",
            "",
            edit_style,
            WS_EX_CLIENTEDGE,
            ID_EX_FILES,
            Some(3),
            x0,
            y0 + 298,
            full,
            24,
        );
        add(
            "Static",
            "Include only files (empty = all):",
            0,
            0,
            -1,
            Some(3),
            x0,
            y0 + 332,
            full,
            20,
        );
        add(
            "Edit",
            "",
            edit_style,
            WS_EX_CLIENTEDGE,
            ID_INCLUDE_FILES,
            Some(3),
            x0,
            y0 + 354,
            full,
            24,
        );

        // Index
        let note = |text: &str, y: i32| {
            add("Static", text, 0, 0, -1, Some(4), x0 + 24, y, full - 24, 40);
        };
        add(
            "Static",
            "What the index keeps besides names, sizes and dates modified. Each option trades memory and disk space for features or sort speed.",
            0,
            0,
            -1,
            Some(4),
            x0,
            y0,
            full,
            40,
        );
        add(
            "Button",
            "Index date created",
            chk,
            0,
            ID_IX_CREATED,
            Some(4),
            x0,
            y0 + 50,
            full,
            22,
        );
        note(
            "8 bytes per file (about 55 MB for 7 million files). Needed for dc: and sorting by Date Created. Turning it on rescans each volume once (C: ~30 s); searching keeps working meanwhile.",
            y0 + 74,
        );
        add(
            "Button",
            "Fast size sort",
            chk,
            0,
            ID_IX_SORT_SIZE,
            Some(4),
            x0,
            y0 + 124,
            full,
            22,
        );
        note(
            "4 bytes per file. When off, sorting millions of results by size takes about twice as long (~150 ms instead of ~70 ms); normal result sets are unaffected.",
            y0 + 148,
        );
        add(
            "Button",
            "Fast date modified sort",
            chk,
            0,
            ID_IX_SORT_MODIFIED,
            Some(4),
            x0,
            y0 + 198,
            full,
            22,
        );
        note(
            "4 bytes per file. Same trade-off as size, for the Date Modified column.",
            y0 + 222,
        );
        add(
            "Button",
            "Fast path sort",
            chk,
            0,
            ID_IX_SORT_PATH,
            Some(4),
            x0,
            y0 + 256,
            full,
            22,
        );
        note(
            "4 bytes per file. When off, sorting by Path gets much slower (seconds for millions of results, ~1 s for 250,000).",
            y0 + 280,
        );

        // Shares
        let snote = |text: &str, y: i32| {
            add("Static", text, 0, 0, -1, Some(5), x0 + 24, y, full - 24, 40);
        };
        add(
            "Static",
            "Network shares (\\\\server\\share) are slow to walk, so their index has its own settings. Local folders and drives aren't affected.",
            0,
            0,
            -1,
            Some(5),
            x0,
            y0,
            full,
            40,
        );
        add(
            "Button",
            "Keep a copy of share indexes on disk",
            chk,
            0,
            ID_SH_CACHE,
            Some(5),
            x0,
            y0 + 50,
            full,
            22,
        );
        snote(
            "About 12 bytes per file on disk (120 MB for 10 million files). When off, shares are empty after a restart until they've been walked again, which takes minutes to hours over a slow link.",
            y0 + 74,
        );
        add(
            "Button",
            "Walk shares again when the service starts",
            chk,
            0,
            ID_SH_REWALK,
            Some(5),
            x0,
            y0 + 124,
            full,
            22,
        );
        snote(
            "Off: trust the cached copy and only watch for changes, until the rescan interval below. Saves a full walk per restart; changes made while the service was stopped show up at the next rescan.",
            y0 + 148,
        );
        add(
            "Button",
            "Index date created on shares",
            chk,
            0,
            ID_SH_CREATED,
            Some(5),
            x0,
            y0 + 198,
            full,
            22,
        );
        snote(
            "8 bytes per file in memory, no extra network traffic (it comes with the listing). Turning it on later re-walks every share, so it's on by default here.",
            y0 + 222,
        );
        add(
            "Static",
            "Rescan newly added shares every (minutes, 0 = never):",
            0x200, /* SS_CENTERIMAGE */
            0,
            -1,
            Some(5),
            x0,
            y0 + 272,
            340,
            24,
        );
        add(
            "Edit",
            "60",
            edit_style | ES_NUMBER as u32,
            WS_EX_CLIENTEDGE,
            ID_SH_RESCAN,
            Some(5),
            x0 + 350,
            y0 + 272,
            80,
            24,
        );
        snote(
            "Change notifications over SMB can be lost (dropped links, servers without support); a periodic walk catches what they missed.",
            y0 + 300,
        );

        // NAS agents
        add(
            "Static",
            "Shares on a NAS running quickseek-agent are indexed on the NAS itself and kept current over the network: a fast first index, changes within a second, and growth history for days this PC was off. Start the agent (see the README), then pair with the code in its log.",
            0,
            0,
            -1,
            Some(6),
            x0,
            y0,
            full,
            56,
        );
        add(
            "ListBox",
            "",
            lb_style,
            WS_EX_CLIENTEDGE,
            ID_NAS_LIST,
            Some(6),
            x0,
            y0 + 60,
            full - 130,
            190,
        );
        add(
            "Button",
            "Remove Agent",
            btn,
            0,
            ID_NAS_REMOVE,
            Some(6),
            x0 + full - 120,
            y0 + 60,
            120,
            28,
        );
        add(
            "Button",
            "Share On/Off",
            btn,
            0,
            ID_NAS_TOGGLE,
            Some(6),
            x0 + full - 120,
            y0 + 94,
            120,
            28,
        );
        add(
            "Static",
            "Local path for the selected share (e.g. a mapped drive):",
            0,
            0,
            -1,
            Some(6),
            x0 + full - 120,
            y0 + 132,
            120,
            48,
        );
        let alias = add(
            "Edit",
            "",
            edit_style,
            WS_EX_CLIENTEDGE,
            ID_NAS_ALIAS,
            Some(6),
            x0 + full - 120,
            y0 + 184,
            120,
            24,
        );
        send(alias, EM_SETCUEBANNER, 1, wide("H:").as_ptr() as isize);
        add(
            "Button",
            "Set Path",
            btn,
            0,
            ID_NAS_SET_ALIAS,
            Some(6),
            x0 + full - 120,
            y0 + 214,
            120,
            28,
        );
        add(
            "Static",
            "NAS:",
            0x200, /* SS_CENTERIMAGE */
            0,
            -1,
            Some(6),
            x0,
            y0 + 262,
            40,
            24,
        );
        let host = add(
            "Edit",
            "",
            edit_style,
            WS_EX_CLIENTEDGE,
            ID_NAS_HOST,
            Some(6),
            x0 + 44,
            y0 + 262,
            180,
            24,
        );
        send(
            host,
            EM_SETCUEBANNER,
            1,
            wide("mynas.local").as_ptr() as isize,
        );
        add(
            "Static",
            "Pairing code:",
            0x200, /* SS_CENTERIMAGE */
            0,
            -1,
            Some(6),
            x0 + 236,
            y0 + 262,
            90,
            24,
        );
        let code = add(
            "Edit",
            "",
            edit_style,
            WS_EX_CLIENTEDGE,
            ID_NAS_CODE,
            Some(6),
            x0 + 328,
            y0 + 262,
            full - 130 - 328,
            24,
        );
        send(
            code,
            EM_SETCUEBANNER,
            1,
            wide("K7QF-3MZD").as_ptr() as isize,
        );
        add(
            "Button",
            "Pair",
            btn,
            0,
            ID_NAS_PAIR,
            Some(6),
            x0 + full - 120,
            y0 + 260,
            120,
            28,
        );
        add(
            "Static",
            "",
            0,
            0,
            ID_NAS_STATE,
            Some(6),
            x0,
            y0 + 296,
            full,
            60,
        );

        // Buttons
        let bh = ch * 96 / w::dpi(hwnd);
        add(
            "Button",
            "OK",
            (BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
            0,
            1,
            None,
            wdt - 8 - 3 * 96 - 16,
            bh - 40,
            96,
            28,
        );
        add(
            "Button",
            "Cancel",
            btn,
            0,
            2,
            None,
            wdt - 8 - 2 * 96 - 8,
            bh - 40,
            96,
            28,
        );
        add(
            "Button",
            "Apply",
            btn,
            0,
            ID_APPLY,
            None,
            wdt - 8 - 96,
            bh - 40,
            96,
            28,
        );

        fill(app);
        show_tab(0);
        // Center over the main window.
        let mut or = std::mem::zeroed();
        GetWindowRect(owner, &mut or);
        let mut me = std::mem::zeroed();
        GetWindowRect(hwnd, &mut me);
        let (mw, mh) = (me.right - me.left, me.bottom - me.top);
        SetWindowPos(
            hwnd,
            null_mut(),
            or.left + ((or.right - or.left) - mw) / 2,
            or.top + ((or.bottom - or.top) - mh) / 2,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER,
        );
        crate::theme::attach(hwnd, crate::theme::Surface::Face);
        ShowWindow(hwnd, SW_SHOW);
        SetFocus(item(hwnd, ID_HOTKEY));
    }
    let _ = set_checked;
}
