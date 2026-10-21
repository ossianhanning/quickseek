//! Small Win32 helpers over windows-sys.

use std::ptr::null;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{CreateFontIndirectW, HFONT, LOGFONTW};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn hinstance() -> windows_sys::Win32::Foundation::HINSTANCE {
    unsafe { GetModuleHandleW(null()) }
}

pub fn send(h: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { SendMessageW(h, msg, w, l) }
}

/// Creates a child control. `id` doubles as its UI Automation id.
pub fn child(parent: HWND, class: &str, text: &str, style: u32, ex: u32, id: i32) -> HWND {
    let c = wide(class);
    let t = wide(text);
    unsafe {
        CreateWindowExW(
            ex,
            c.as_ptr(),
            t.as_ptr(),
            WS_CHILD | WS_VISIBLE | style,
            0,
            0,
            10,
            10,
            parent,
            id as isize as _,
            hinstance(),
            null(),
        )
    }
}

pub fn set_text(h: HWND, s: &str) {
    let w = wide(s);
    unsafe { SetWindowTextW(h, w.as_ptr()) };
}

pub fn get_text(h: HWND) -> String {
    unsafe {
        let n = GetWindowTextLengthW(h);
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
        String::from_utf16_lossy(&buf[..got as usize])
    }
}

pub fn move_to(h: HWND, x: i32, y: i32, w: i32, hh: i32) {
    unsafe { MoveWindow(h, x, y, w.max(0), hh.max(0), 1) };
}

pub fn client_rect(h: HWND) -> RECT {
    let mut r = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe { GetClientRect(h, &mut r) };
    r
}

pub fn dpi(h: HWND) -> i32 {
    let d = unsafe { GetDpiForWindow(h) };
    if d == 0 { 96 } else { d as i32 }
}

/// Scales a 96-DPI length for this window.
pub fn px(h: HWND, v: i32) -> i32 {
    v * dpi(h) / 96
}

/// The system message font at this window's DPI, optionally bold.
pub fn ui_font(h: HWND, bold: bool) -> HFONT {
    unsafe {
        let mut ncm: NONCLIENTMETRICSW = std::mem::zeroed();
        ncm.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
        let d = dpi(h) as u32;
        windows_sys::Win32::UI::HiDpi::SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS,
            ncm.cbSize,
            &mut ncm as *mut _ as *mut _,
            0,
            d,
        );
        let mut lf: LOGFONTW = ncm.lfMessageFont;
        if bold {
            lf.lfWeight = 700;
        }
        CreateFontIndirectW(&lf)
    }
}

pub fn set_font(h: HWND, f: HFONT) {
    send(h, WM_SETFONT, f as usize, 1);
}

pub fn message_box(owner: HWND, text: &str, title: &str, flags: u32) -> i32 {
    let (t, c) = (wide(text), wide(title));
    unsafe { MessageBoxW(owner, t.as_ptr(), c.as_ptr(), flags) }
}

pub fn loword(v: usize) -> u32 {
    (v & 0xFFFF) as u32
}

pub fn hiword(v: usize) -> u32 {
    ((v >> 16) & 0xFFFF) as u32
}

pub fn menu_item(menu: HMENU, id: u32, text: &str) {
    let t = wide(text);
    unsafe { AppendMenuW(menu, MF_STRING, id as usize, t.as_ptr()) };
}

pub fn menu_sep(menu: HMENU) {
    unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, null()) };
}

pub fn submenu(bar: HMENU, text: &str) -> HMENU {
    let m = unsafe { CreatePopupMenu() };
    let t = wide(text);
    unsafe { AppendMenuW(bar, MF_POPUP, m as usize, t.as_ptr()) };
    m
}

pub fn check_menu(menu: HMENU, id: u32, on: bool) {
    unsafe { CheckMenuItem(menu, id, if on { MF_CHECKED } else { MF_UNCHECKED }) };
}
