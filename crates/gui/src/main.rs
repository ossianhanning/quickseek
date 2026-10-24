//! Quickseek GUI: an Everything-style search window over the Quickseek
//! service (or an in-process index when run elevated without the service).
//!
//! Flags: `--background` (start in the tray), `--test` (no tray/hotkey/
//! single-instance; used by the UI automation tests), `--local` (don't use
//! the service).

#![cfg_attr(not(test), windows_subsystem = "windows")]

mod app;
mod connect;
mod growth;
mod highlight;
mod msgbox;
mod options;
mod preview;
mod results;
mod settings;
mod shell;
mod theme;
mod tree;
mod w;

use std::ptr::null_mut;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use quickseek_core::engine::{Engine, EngineOptions};
use quickseek_core::ipc::{self, Backend};
use quickseek_core::ntfs::win as ntwin;
use quickseek_core::persist;
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Controls::{_LI_METRIC, LIM_LARGE, LIM_SMALL, LoadIconMetric};
use windows_sys::Win32::UI::Controls::{
    ICC_BAR_CLASSES, ICC_LISTVIEW_CLASSES, ICC_STANDARD_CLASSES, ICC_TAB_CLASSES,
    INITCOMMONCONTROLSEX, InitCommonControlsEx,
};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// Visual styles are on (comctl32 v6 via the embedded manifest).
pub fn themed() -> bool {
    true
}

/// Quickseek's icon (resource 1, assets/quickseek.ico) at the system's
/// large-icon size: windows' title bars and Alt+Tab.
pub fn app_icon() -> HICON {
    static ICON: OnceLock<isize> = OnceLock::new();
    *ICON.get_or_init(|| load_icon(LIM_LARGE)) as HICON
}

/// The same at the small-icon size: title bars' small icon and the tray.
pub fn app_icon_small() -> HICON {
    static ICON: OnceLock<isize> = OnceLock::new();
    *ICON.get_or_init(|| load_icon(LIM_SMALL)) as HICON
}

fn load_icon(metric: _LI_METRIC) -> isize {
    let mut h: HICON = null_mut();
    unsafe {
        if LoadIconMetric(w::hinstance(), 1 as _, metric, &mut h) != 0 || h.is_null() {
            h = LoadIconW(null_mut(), IDI_APPLICATION);
        }
    }
    h as isize
}

fn connect(local: bool) -> Result<Box<dyn Backend>, String> {
    if !local {
        match ipc::Client::connect() {
            Ok(c) => return Ok(Box::new(ipc::Remote::new(c))),
            Err(e) if ipc::is_version_mismatch(&e) => {
                return Err(format!(
                    "{e}.\n\nUpdate or reinstall Quickseek (quickseek-setup.exe) so they match."
                ));
            }
            Err(e) => log::info!("service not reachable: {e}"),
        }
    }
    if !ntwin::is_elevated() {
        return Err(concat!(
            "The Quickseek service isn't running.\n\n",
            "Install it once from an elevated terminal:\n    quickseek service install\n\n",
            "or run quickseek-gui as administrator to index in this process."
        )
        .into());
    }
    // In-process: the engine loads volumes in the background; searches
    // cover what's loaded so far.
    let engine = Arc::new(Engine::new(EngineOptions {
        config_path: Some(persist::db_dir().join("config.json")),
        runs_path: Some(persist::db_dir().join("runs.json")),
        ..Default::default()
    }));
    engine.enable_history(persist::db_dir().join("history"));
    let e = engine.clone();
    std::thread::spawn(move || {
        e.sync_volumes();
        loop {
            std::thread::sleep(Duration::from_secs(3));
            e.sync_if_changed();
            e.maybe_snapshot();
        }
    });
    Ok(Box::new(ipc::Local::new(engine)))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let has = |f: &str| args.iter().any(|a| a == f);
    let (test, background, local) = (has("--test"), has("--background"), has("--local"));

    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = windows::Win32::System::Ole::OleInitialize(None);
        let icc = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_BAR_CLASSES | ICC_STANDARD_CLASSES | ICC_TAB_CLASSES,
        };
        InitCommonControlsEx(&icc);
    }

    // One instance per user: a second launch just shows the first.
    if !test {
        let name = w::wide("Local\\quickseek-gui");
        unsafe {
            CreateMutexW(null_mut(), 0, name.as_ptr());
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let class = w::wide(app::CLASS);
                let h = FindWindowW(class.as_ptr(), null_mut());
                if !h.is_null() {
                    PostMessageW(h, app::WM_APP_SHOW, 0, 0);
                }
                return;
            }
        }
    }

    let be = match connect(local) {
        Ok(b) => b,
        Err(e) => {
            w::message_box(null_mut(), &e, "quickseek", MB_OK | MB_ICONINFORMATION);
            return;
        }
    };
    let app = app::create(be, test, background);

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    unsafe {
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if app::global_key(&msg) || options::handle_key(&msg) {
                continue;
            }
            let opt = FindWindowW(w::wide(options::CLASS).as_ptr(), null_mut());
            if !opt.is_null() && IsDialogMessageW(opt, &msg) != 0 {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    let _ = app.conn.lock().be.finish();
}
