//! The setup window: options, Install/Update, progress, and launching Quickseek.

use std::cell::RefCell;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    COLOR_WINDOW, CreateFontIndirectW, GetSysColor, HBRUSH, HFONT, LOGFONTW, UpdateWindow,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{
    BST_CHECKED, BST_UNCHECKED, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx,
};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
    SystemParametersInfoForDpi,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use quickseek_pkg::install::{self, Options, VERSION};
use quickseek_pkg::sys::{self, wide};
use quickseek_pkg::{Args, log_line};

const ID_DIR: i32 = 10;
const ID_SERVICE: i32 = 11;
const ID_SHORTCUT: i32 = 12;
const ID_PATH: i32 = 13;
const ID_STARTUP: i32 = 14;
const ID_MCP: i32 = 15;
const ID_LOG: i32 = 20;
const ID_GO: i32 = 1;
const ID_CLOSE: i32 = 2;

struct State {
    hwnd: HWND,
    opts: Options,
    launch: bool,
    done: bool,
}

thread_local! {
    static ST: RefCell<Option<State>> = const { RefCell::new(None) };
}

pub fn message(text: &str, error: bool) {
    let (t, c) = (wide(text), wide("quickseek setup"));
    unsafe {
        MessageBoxW(
            null_mut(),
            t.as_ptr(),
            c.as_ptr(),
            MB_OK
                | if error {
                    MB_ICONERROR
                } else {
                    MB_ICONINFORMATION
                },
        )
    };
}

fn item(h: HWND, id: i32) -> HWND {
    unsafe { GetDlgItem(h, id) }
}

fn checked(h: HWND, id: i32) -> bool {
    unsafe { SendMessageW(item(h, id), BM_GETCHECK, 0, 0) == BST_CHECKED as isize }
}

fn set_text(h: HWND, s: &str) {
    let w = wide(s);
    unsafe { SetWindowTextW(h, w.as_ptr()) };
}

fn get_text(h: HWND) -> String {
    unsafe {
        let n = GetWindowTextLengthW(h);
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..got as usize])
    }
}

fn go(h: HWND) {
    let done = ST.with(|s| s.borrow().as_ref().is_some_and(|s| s.done));
    if done {
        let dir = ST
            .with(|s| s.borrow().as_ref().map(|s| s.opts.dir.clone()))
            .unwrap();
        sys::launch_unelevated(&dir.join("quickseek-gui.exe"));
        unsafe { DestroyWindow(h) };
        return;
    }
    let mut o = ST.with(|s| s.borrow().as_ref().unwrap().opts.clone());
    o.dir = get_text(item(h, ID_DIR)).trim().into();
    o.service = checked(h, ID_SERVICE);
    o.shortcut = checked(h, ID_SHORTCUT);
    o.path = checked(h, ID_PATH);
    o.startup = checked(h, ID_STARTUP);
    if !checked(h, ID_MCP) {
        o.mcp.clear();
    }
    for id in [
        ID_GO,
        ID_CLOSE,
        ID_DIR,
        ID_SERVICE,
        ID_SHORTCUT,
        ID_PATH,
        ID_STARTUP,
        ID_MCP,
    ] {
        unsafe { EnableWindow(item(h, id), 0) };
    }
    let log_ctl = item(h, ID_LOG);
    let mut lines: Vec<String> = Vec::new();
    let mut log = |s: &str| {
        log_line(s);
        lines.push(s.to_string());
        set_text(log_ctl, &lines.join("\r\n"));
        unsafe { UpdateWindow(h) };
    };
    let r = install::install(&o, &mut log);
    unsafe {
        EnableWindow(item(h, ID_CLOSE), 1);
        EnableWindow(item(h, ID_GO), 1);
    }
    match r {
        Ok(()) => {
            let launch = ST.with(|s| s.borrow().as_ref().is_some_and(|s| s.launch));
            log(if launch {
                "Open Quickseek with the button below, or from the Start menu."
            } else {
                "Done."
            });
            if !o.mcp.is_empty() {
                log("Restart the AI assistants you connected so they load Quickseek.");
            }
            set_text(item(h, ID_GO), "Open Quickseek");
            ST.with(|s| {
                let mut b = s.borrow_mut();
                let st = b.as_mut().unwrap();
                st.done = true;
                st.opts = o;
            });
            if !launch {
                unsafe { EnableWindow(item(h, ID_GO), 0) };
            }
        }
        Err(e) => {
            log(&format!("Failed: {e}"));
            for id in [ID_DIR, ID_SERVICE, ID_SHORTCUT, ID_PATH, ID_STARTUP, ID_MCP] {
                unsafe { EnableWindow(item(h, id), 1) };
            }
        }
    }
}

unsafe extern "system" fn proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND => {
            match (wp & 0xFFFF) as i32 {
                ID_GO => go(h),
                ID_CLOSE => unsafe {
                    DestroyWindow(h);
                },
                _ => {}
            }
            0
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => unsafe {
            // Labels and checkboxes on the white window background.
            windows_sys::Win32::Graphics::Gdi::SetBkColor(wp as _, GetSysColor(COLOR_WINDOW));
            windows_sys::Win32::Graphics::Gdi::GetSysColorBrush(COLOR_WINDOW) as LRESULT
        },
        WM_CLOSE => {
            unsafe { DestroyWindow(h) };
            0
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(h, msg, wp, lp) },
    }
}

/// The magnifier stock icon (the GUI uses it too).
fn app_icon() -> HICON {
    use windows_sys::Win32::UI::Shell::{
        SHGSI_ICON, SHGetStockIconInfo, SHSTOCKICONINFO, SIID_FIND,
    };
    unsafe {
        let mut sii: SHSTOCKICONINFO = std::mem::zeroed();
        sii.cbSize = size_of::<SHSTOCKICONINFO>() as u32;
        if SHGetStockIconInfo(SIID_FIND, SHGSI_ICON, &mut sii) == 0 {
            sii.hIcon
        } else {
            LoadIconW(null_mut(), IDI_APPLICATION)
        }
    }
}

fn font(h: HWND) -> HFONT {
    unsafe {
        let mut ncm: NONCLIENTMETRICSW = std::mem::zeroed();
        ncm.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS,
            ncm.cbSize,
            &mut ncm as *mut _ as *mut _,
            0,
            GetDpiForWindow(h),
        );
        let lf: LOGFONTW = ncm.lfMessageFont;
        CreateFontIndirectW(&lf)
    }
}

/// Shows the window and runs until it closes. Returns the exit code.
pub fn run(a: Args) -> i32 {
    if !install::has_payload() {
        message(
            "This quickseek-setup.exe was built without the program files. Build it with tools\\build-release.ps1.",
            true,
        );
        return 1;
    }
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let icc = INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES,
        };
        InitCommonControlsEx(&icc);
        let hinst = GetModuleHandleW(null());
        let class = wide("quickseek.setup");
        let icon = app_icon();
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: icon,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as usize as HBRUSH,
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
            hIconSm: icon,
        };
        RegisterClassExW(&wc);
        let installed = sys::installed_version();
        let title = wide(&format!("quickseek {VERSION} setup"));
        let h = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            10,
            10,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        let dpi = GetDpiForWindow(h) as i32;
        let s = |v: i32| v * dpi / 96;
        SetWindowPos(
            h,
            null_mut(),
            0,
            0,
            s(600),
            s(560),
            SWP_NOMOVE | SWP_NOZORDER,
        );
        let f = font(h);
        let add = |class: &str,
                   text: &str,
                   style: u32,
                   ex: u32,
                   id: i32,
                   x: i32,
                   y: i32,
                   w: i32,
                   ht: i32| {
            let (c, t) = (wide(class), wide(text));
            let c = CreateWindowExW(
                ex,
                c.as_ptr(),
                t.as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                s(x),
                s(y),
                s(w),
                s(ht),
                h,
                id as isize as _,
                hinst,
                null(),
            );
            SendMessageW(c, WM_SETFONT, f as usize, 1);
            c
        };
        let intro = match &installed {
            Some(v) if v == VERSION => format!(
                "quickseek {VERSION} is installed. Running setup again repairs it with the options below."
            ),
            Some(v) => format!(
                "quickseek {v} is installed; this updates it to {VERSION}. Your index and settings are kept."
            ),
            None => format!(
                "quickseek {VERSION}: instant file search and folder sizes for all your drives."
            ),
        };
        add("Static", &intro, 0, 0, -1, 20, 16, 550, 40);
        add("Static", "Install to:", 0, 0, -1, 20, 62, 90, 22);
        add(
            "Edit",
            &a.opts.dir.to_string_lossy(),
            WS_TABSTOP | ES_AUTOHSCROLL as u32,
            WS_EX_CLIENTEDGE,
            ID_DIR,
            110,
            58,
            460,
            26,
        );
        let chk = (BS_AUTOCHECKBOX as u32) | WS_TABSTOP;
        let rows: [(i32, &str, bool); 4] = [
            (
                ID_SERVICE,
                "Background service that keeps the index of your drives live (recommended)",
                a.opts.service,
            ),
            (ID_SHORTCUT, "Start menu shortcut", a.opts.shortcut),
            (ID_PATH, "Add the Quickseek command to PATH", a.opts.path),
            (
                ID_STARTUP,
                "Start Quickseek in the notification area when I sign in",
                a.opts.startup,
            ),
        ];
        for (i, (id, text, on)) in rows.iter().enumerate() {
            let c = add(
                "Button",
                text,
                chk,
                0,
                *id,
                20,
                100 + i as i32 * 30,
                550,
                24,
            );
            SendMessageW(
                c,
                BM_SETCHECK,
                if *on { BST_CHECKED } else { BST_UNCHECKED } as usize,
                0,
            );
        }
        let detected: Vec<&str> = a.opts.mcp.iter().map(|c| c.label()).collect();
        let mcp_text = if detected.is_empty() {
            "Connect AI assistants (none found: Claude Code, Claude Desktop, Cursor, VS Code, Windsurf, Codex, Antigravity)".to_string()
        } else {
            format!(
                "Connect AI assistants to Quickseek's MCP server: {}",
                detected.join(", ")
            )
        };
        let m = add(
            "Button",
            &mcp_text,
            chk | BS_MULTILINE as u32,
            0,
            ID_MCP,
            20,
            220,
            550,
            40,
        );
        let mcp_on = !detected.is_empty() && a.mcp_choice != Some(false);
        SendMessageW(
            m,
            BM_SETCHECK,
            if mcp_on { BST_CHECKED } else { BST_UNCHECKED } as usize,
            0,
        );
        EnableWindow(m, (!detected.is_empty()) as i32);
        add("Static", "", 0, 0, ID_LOG, 20, 272, 550, 170);
        add(
            "Button",
            if installed.is_some() {
                "Update"
            } else {
                "Install"
            },
            (BS_DEFPUSHBUTTON as u32) | WS_TABSTOP,
            0,
            ID_GO,
            344,
            462,
            112,
            30,
        );
        add(
            "Button",
            "Close",
            (BS_PUSHBUTTON as u32) | WS_TABSTOP,
            0,
            ID_CLOSE,
            464,
            462,
            106,
            30,
        );
        ST.with(|st| {
            *st.borrow_mut() = Some(State {
                hwnd: h,
                opts: a.opts.clone(),
                launch: a.launch,
                done: false,
            })
        });
        // Center on the screen.
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        SetWindowPos(
            h,
            null_mut(),
            (sw - s(600)) / 2,
            (sh - s(560)) / 2,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER,
        );
        ShowWindow(h, SW_SHOW);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if IsDialogMessageW(h, &msg) != 0 {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = ST.with(|st| st.borrow().as_ref().map(|s| s.hwnd));
    }
    0
}

/// Interactive uninstall: confirm, remove, report.
pub fn uninstall(a: &Args) -> i32 {
    let dir = &a.opts.dir;
    let q = wide(&format!(
        "Uninstall Quickseek from {}?\n\nThe service, shortcut and AI assistant connections are removed. Your index and settings in {} are kept{}.",
        dir.display(),
        sys::program_data().join("quickseek").display(),
        if a.purge {
            " (no: --purge deletes them too)"
        } else {
            ""
        }
    ));
    let c = wide("quickseek setup");
    if unsafe {
        MessageBoxW(
            null_mut(),
            q.as_ptr(),
            c.as_ptr(),
            MB_YESNO | MB_ICONQUESTION,
        )
    } != IDYES
    {
        return 1;
    }
    let mut lines = Vec::new();
    let r = install::uninstall(
        &install::Removal {
            dir: dir.clone(),
            service: true,
            system: true,
            purge: a.purge,
        },
        &mut |s| {
            log_line(s);
            lines.push(s.to_string());
        },
    );
    match r {
        Ok(()) => {
            message(&lines.join("\n"), false);
            0
        }
        Err(e) => {
            message(&format!("{}\n\nFailed: {e}", lines.join("\n")), true);
            1
        }
    }
}
