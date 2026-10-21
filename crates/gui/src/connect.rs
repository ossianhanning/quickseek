//! Tools → Connect AI Assistants: register `rapidfind-mcp` with Claude Code,
//! Claude Desktop, Cursor, VS Code and Windsurf (per user, no admin).

use std::cell::RefCell;
use std::ptr::{null, null_mut};

use rapidfind_mcp::register::{self as reg, ALL};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{COLOR_BTNFACE, HBRUSH};
use windows_sys::Win32::UI::Controls::{BST_CHECKED, BST_UNCHECKED};
/// Static control styles (not in windows-sys' WindowsAndMessaging set we use).
const SS_ENDELLIPSIS: u32 = 0x4000;
const SS_PATHELLIPSIS: u32 = 0x8000;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::app::App;
use crate::w::{self, px, send, wide};

pub const CLASS: &str = "rapidfind.connect";
/// Checkbox of client `i` is `ID_CLIENT + i`, its status label `ID_STATE + i`.
pub const ID_CLIENT: i32 = 1100;
pub const ID_STATE: i32 = 1150;
pub const ID_APPLY: i32 = 1190;
pub const ID_PATH: i32 = 1191;
pub const ID_RESULT: i32 = 1192;
/// Top of what's under the client rows (30 px each, from 62).
const BELOW: i32 = 62 + ALL.len() as i32 * 30 + 8;

thread_local! {
    static HWND_OPEN: RefCell<HWND> = const { RefCell::new(null_mut()) };
}

fn item(h: HWND, id: i32) -> HWND {
    unsafe { GetDlgItem(h, id) }
}

fn state_text(s: &reg::Status, exe: Option<&str>) -> String {
    match (&s.registered, &s.problem) {
        (Some(cmd), _) if exe.is_some_and(|e| e.eq_ignore_ascii_case(cmd)) => {
            if s.skill {
                "connected, with the rapidfind skill".into()
            } else {
                "connected".into()
            }
        }
        (Some(cmd), _) => format!("connected to another copy: {cmd}"),
        (None, Some(p)) => p.clone(),
        (None, None) if s.detected => "not connected".into(),
        _ => "not installed".into(),
    }
}

/// The exe that will be registered (a stable copy is made on Apply).
fn target_exe() -> Option<std::path::PathBuf> {
    let src = reg::sibling_exe()?;
    Some(if src.parent().is_some_and(reg::is_stable_dir) {
        src
    } else {
        reg::user_dir().join(reg::EXE)
    })
}

fn refresh(h: HWND) {
    let exe = target_exe();
    let exe_s = exe.as_ref().map(|p| p.to_string_lossy().into_owned());
    for (i, s) in reg::status_all().into_iter().enumerate() {
        let cb = item(h, ID_CLIENT + i as i32);
        send(
            cb,
            BM_SETCHECK,
            if s.registered.is_some() {
                BST_CHECKED
            } else {
                BST_UNCHECKED
            } as usize,
            0,
        );
        unsafe { EnableWindow(cb, (s.detected || s.registered.is_some()) as i32) };
        w::set_text(
            item(h, ID_STATE + i as i32),
            &state_text(&s, exe_s.as_deref()),
        );
    }
    w::set_text(
        item(h, ID_PATH),
        &match &exe {
            Some(p) => format!("Server: {}", p.display()),
            None => format!(
                "{} isn't next to rapidfind-gui.exe, so there's nothing to register.",
                reg::EXE
            ),
        },
    );
    unsafe { EnableWindow(item(h, ID_APPLY), exe.is_some() as i32) };
}

fn apply(h: HWND) {
    let Some(src) = reg::sibling_exe() else {
        return;
    };
    let mut lines = Vec::new();
    let exe = match reg::stable_exe(&src) {
        Ok(e) => e,
        Err(e) => {
            w::set_text(item(h, ID_RESULT), &e);
            return;
        }
    };
    for (i, c) in ALL.into_iter().enumerate() {
        let want = send(item(h, ID_CLIENT + i as i32), BM_GETCHECK, 0, 0) == BST_CHECKED as isize;
        let st = reg::status(c);
        let current = st
            .registered
            .as_deref()
            .is_some_and(|r| r.eq_ignore_ascii_case(&exe.to_string_lossy()));
        let r = match (want, st.registered.is_some()) {
            (true, _) if current => continue,
            (true, _) => reg::register(c, &exe)
                .map(|()| format!("{}: connected; {}", c.label(), reg::restart_hint(c))),
            (false, true) => reg::unregister(c).map(|_| format!("{}: disconnected", c.label())),
            (false, false) => continue,
        };
        lines.push(r.unwrap_or_else(|e| format!("{}: {e}", c.label())));
    }
    refresh(h);
    w::set_text(
        item(h, ID_RESULT),
        &if lines.is_empty() {
            "Nothing to change.".to_string()
        } else {
            lines.join("\n")
        },
    );
}

unsafe extern "system" fn proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            match w::loword(wp) as i32 {
                ID_APPLY => apply(h),
                2 => unsafe {
                    DestroyWindow(h);
                },
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            unsafe { DestroyWindow(h) };
            0
        }
        WM_DESTROY => {
            HWND_OPEN.with(|o| *o.borrow_mut() = null_mut());
            if let Some(app) = crate::app::app() {
                unsafe {
                    EnableWindow(app.hwnd.get(), 1);
                    SetForegroundWindow(app.hwnd.get());
                }
            }
            0
        }
        _ => unsafe { DefWindowProcW(h, msg, wp, lp) },
    }
}

pub fn open(app: &App) {
    let existing = HWND_OPEN.with(|o| *o.borrow());
    if !existing.is_null() {
        unsafe { SetForegroundWindow(existing) };
        return;
    }
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
            hIconSm: crate::app_icon(),
        };
        RegisterClassExW(&wc);
        let owner = app.hwnd.get();
        let s = |v| px(owner, v);
        let title = wide("Connect AI Assistants");
        let h = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class.as_ptr(),
            title.as_ptr(),
            WS_CAPTION | WS_SYSMENU | WS_POPUP,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            s(620),
            s(BELOW + 220),
            owner,
            null_mut(),
            w::hinstance(),
            null(),
        );
        HWND_OPEN.with(|o| *o.borrow_mut() = h);
        let font = w::ui_font(h, false);
        let add =
            |class: &str, text: &str, style: u32, id: i32, x: i32, y: i32, wd: i32, ht: i32| {
                let c = w::child(h, class, text, style, 0, id);
                w::move_to(c, s(x), s(y), s(wd), s(ht));
                w::set_font(c, font);
                c
            };
        add(
            "Static",
            "Let AI assistants search your files and find what takes disk space, through rapidfind's MCP server. It runs as you (no admin) and only when an assistant uses it.",
            0,
            -1,
            16,
            12,
            580,
            40,
        );
        for (i, c) in ALL.into_iter().enumerate() {
            let y = 62 + i as i32 * 30;
            add(
                "Button",
                c.label(),
                (BS_AUTOCHECKBOX as u32) | WS_TABSTOP,
                ID_CLIENT + i as i32,
                24,
                y,
                170,
                24,
            );
            add(
                "Static",
                "",
                SS_ENDELLIPSIS,
                ID_STATE + i as i32,
                200,
                y + 3,
                390,
                22,
            );
        }
        add("Static", "", SS_PATHELLIPSIS, ID_PATH, 16, BELOW, 580, 20);
        add("Static", "", 0, ID_RESULT, 16, BELOW + 26, 580, 110);
        add(
            "Button",
            "Apply",
            (BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
            ID_APPLY,
            404,
            BELOW + 146,
            96,
            28,
        );
        add(
            "Button",
            "Close",
            (BS_PUSHBUTTON as u32) | WS_TABSTOP,
            2,
            508,
            BELOW + 146,
            96,
            28,
        );
        refresh(h);
        let mut or = std::mem::zeroed();
        GetWindowRect(owner, &mut or);
        let mut me = std::mem::zeroed();
        GetWindowRect(h, &mut me);
        let (mw, mh) = (me.right - me.left, me.bottom - me.top);
        SetWindowPos(
            h,
            null_mut(),
            or.left + ((or.right - or.left) - mw) / 2,
            or.top + ((or.bottom - or.top) - mh) / 2,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER,
        );
        ShowWindow(h, SW_SHOW);
    }
}
