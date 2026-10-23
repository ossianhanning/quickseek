//! A message box in the app's theme. Windows' own (`MessageBoxW`,
//! `TaskDialog`) stays light in dark mode, so this draws its own: an icon,
//! the text, and an OK button. Only the OK kind is needed.

use std::cell::Cell;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus};
use windows_sys::Win32::UI::Shell::{
    SHGSI_ICON, SHGSI_LARGEICON, SHGetStockIconInfo, SHSTOCKICONINFO, SIID_ERROR, SIID_INFO,
    SIID_WARNING,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::theme;
use crate::w::{self, px, wide};

const CLASS: &str = "rapidfind_msgbox";

thread_local! {
    static ICON: Cell<HICON> = const { Cell::new(null_mut()) };
}

unsafe extern "system" fn proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND if matches!(w::loword(wp) as i32, IDOK | IDCANCEL) => {
            unsafe { DestroyWindow(h) };
            0
        }
        WM_CLOSE => {
            unsafe { DestroyWindow(h) };
            0
        }
        WM_PAINT => {
            unsafe {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let hdc = BeginPaint(h, &mut ps);
                let size = icon_size(h);
                let icon = ICON.with(Cell::get);
                if !icon.is_null() {
                    DrawIconEx(
                        hdc,
                        px(h, 20),
                        px(h, 20),
                        icon,
                        size,
                        size,
                        0,
                        null_mut(),
                        DI_NORMAL,
                    );
                }
                EndPaint(h, &ps);
            }
            0
        }
        _ => unsafe { DefWindowProcW(h, msg, wp, lp) },
    }
}

fn icon_size(h: HWND) -> i32 {
    px(h, 32)
}

/// Shows `text` with an OK button; returns IDOK. `flags` picks the icon
/// (MB_ICONINFORMATION, MB_ICONWARNING, MB_ICONERROR), as MessageBoxW's.
pub fn show(owner: HWND, text: &str, title: &str, flags: u32) -> i32 {
    let icon_kind = flags & 0xF0;
    unsafe {
        let class = wide(CLASS);
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc),
            hInstance: w::hinstance(),
            hIcon: crate::app_icon(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_BTNFACE + 1) as usize as HBRUSH,
            lpszClassName: class.as_ptr(),
            hIconSm: crate::app_icon(),
            ..std::mem::zeroed()
        };
        RegisterClassExW(&wc);
        let t = wide(title);
        let h = CreateWindowExW(
            WS_EX_DLGMODALFRAME,
            class.as_ptr(),
            t.as_ptr(),
            WS_CAPTION | WS_SYSMENU | WS_POPUP,
            0,
            0,
            100,
            100,
            owner,
            null_mut(),
            w::hinstance(),
            null(),
        );
        if h.is_null() {
            let b = wide(text);
            return MessageBoxW(owner, b.as_ptr(), t.as_ptr(), flags);
        }
        let font = w::ui_font(h, false);
        let stock = match icon_kind {
            MB_ICONERROR => SIID_ERROR,
            MB_ICONWARNING => SIID_WARNING,
            _ => SIID_INFO,
        };
        let mut sii: SHSTOCKICONINFO = std::mem::zeroed();
        sii.cbSize = size_of::<SHSTOCKICONINFO>() as u32;
        if SHGetStockIconInfo(stock, SHGSI_ICON | SHGSI_LARGEICON, &mut sii) == 0 {
            ICON.with(|c| c.set(sii.hIcon));
        }

        // Measure the text: up to 560 px wide, tabs expanded.
        let body: Vec<u16> = text.replace('\n', "\r\n").encode_utf16().collect();
        let hdc = GetDC(h);
        let old = SelectObject(hdc, font as _);
        let mut r = RECT {
            left: 0,
            top: 0,
            right: px(h, 560),
            bottom: 0,
        };
        DrawTextW(
            hdc,
            body.as_ptr(),
            body.len() as i32,
            &mut r,
            DT_CALCRECT | DT_WORDBREAK | DT_EXPANDTABS | DT_NOPREFIX,
        );
        SelectObject(hdc, old);
        ReleaseDC(h, hdc);
        let (tw, th) = (r.right.max(px(h, 200)), r.bottom);
        let margin = px(h, 20);
        let text_x = margin + icon_size(h) + px(h, 14);
        let (bw, bh) = (px(h, 88), px(h, 26));
        let cw = text_x + tw + margin;
        let ch = margin + th.max(icon_size(h)) + px(h, 20) + bh + px(h, 14);

        let label = w::child(
            h, "Static", "", 0x80, /* SS_LEFT | SS_NOPREFIX */
            0, 100,
        );
        let label_text = wide(&text.replace('\n', "\r\n"));
        SetWindowTextW(label, label_text.as_ptr());
        w::move_to(
            label,
            text_x,
            margin + (icon_size(h) - th).max(0) / 2,
            tw + px(h, 2),
            th,
        );
        w::set_font(label, font);
        let ok = w::child(
            h,
            "Button",
            "OK",
            WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
            0,
            IDOK,
        );
        w::move_to(ok, cw - margin - bw, ch - px(h, 14) - bh, bw, bh);
        w::set_font(ok, font);

        // Size the frame for that client area, centred on the owner.
        let mut frame = RECT {
            left: 0,
            top: 0,
            right: cw,
            bottom: ch,
        };
        windows_sys::Win32::UI::HiDpi::AdjustWindowRectExForDpi(
            &mut frame,
            WS_CAPTION | WS_SYSMENU | WS_POPUP,
            0,
            WS_EX_DLGMODALFRAME,
            w::dpi(h) as u32,
        );
        let (fw, fh) = (frame.right - frame.left, frame.bottom - frame.top);
        let mut area = RECT {
            left: 0,
            top: 0,
            right: GetSystemMetrics(SM_CXSCREEN),
            bottom: GetSystemMetrics(SM_CYSCREEN),
        };
        if !owner.is_null() && IsWindowVisible(owner) != 0 && IsIconic(owner) == 0 {
            GetWindowRect(owner, &mut area);
        }
        let x = area.left + ((area.right - area.left) - fw) / 2;
        let y = area.top + ((area.bottom - area.top) - fh) / 2;
        SetWindowPos(h, null_mut(), x, y, fw, fh, SWP_NOZORDER | SWP_NOACTIVATE);

        theme::attach(h, theme::Surface::Face);
        if !owner.is_null() {
            EnableWindow(owner, 0);
        }
        ShowWindow(h, SW_SHOW);
        SetFocus(ok);

        // Modal loop until it's closed.
        let mut msg: MSG = std::mem::zeroed();
        while IsWindow(h) != 0 {
            let r = GetMessageW(&mut msg, null_mut(), 0, 0);
            if r <= 0 {
                PostQuitMessage(msg.wParam as i32);
                break;
            }
            if IsDialogMessageW(h, &msg) != 0 {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if !owner.is_null() {
            EnableWindow(owner, 1);
            SetForegroundWindow(owner);
        }
        DeleteObject(font as _);
        ICON.with(|c| {
            let i = c.replace(null_mut());
            if !i.is_null() {
                DestroyIcon(i);
            }
        });
        IDOK
    }
}
