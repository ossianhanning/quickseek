//! The preview pane (View → Preview Pane, `Alt+P`), right of the results:
//! text files with the `content:` matches highlighted, pictures and videos as
//! shell thumbnails, and anything else through the preview handler Explorer
//! would use (Office documents, PDFs, fonts, ...).
//!
//! Files are read on a background thread; preview handlers run on the UI
//! thread (they need its window), mostly in their own `prevhost.exe`.
//! Online-only cloud files are never opened, since that would download them.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::ptr::{null, null_mut};
use std::sync::Arc;

use rapidfind_core::content::{self, ContentQuery, Encoding};
use rapidfind_core::ipc::Row;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CLSCTX_LOCAL_SERVER, CLSIDFromString, COINIT_APARTMENTTHREADED,
    CoCreateInstance, CoInitializeEx, CoUninitialize, STGM_READ, STGM_SHARE_DENY_NONE,
};
use windows::Win32::UI::Controls::RichEdit::*;
use windows::Win32::UI::Shell::PropertiesSystem::{IInitializeWithFile, IInitializeWithStream};
use windows::Win32::UI::Shell::{
    ASSOCF_INIT_DEFAULTTOSTAR, ASSOCSTR_SHELLEXTENSION, AssocQueryStringW, IInitializeWithItem,
    IPreviewHandler, IShellItem, IShellItemImageFactory, SHCreateItemFromParsingName,
    SHCreateStreamOnFileEx, SIIGBF_RESIZETOFIT,
};
use windows::core::{HSTRING, Interface, PCWSTR, PWSTR, w};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Controls::EM_LINESCROLL;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::w::{self as w_, send, wide};

pub const CLASS: &str = "rapidfind.preview";
/// Automation ids: the pane and its text view.
pub const ID_PREVIEW: i32 = 106;
pub const ID_PREVIEW_TEXT: i32 = 107;
const WM_APP_PREVIEW: u32 = WM_APP + 20;

/// Text shown at most (the rest of a big file is cut).
const MAX_TEXT: usize = 512 << 10;
const MAX_HIGHLIGHTS: usize = 2000;
const OFFLINE_ATTRS: u32 = 0x1000 | 0x4_0000 | 0x40_0000;

enum Loaded {
    Text {
        text: Vec<u16>,
        hits: Vec<Range<usize>>,
        cut: bool,
        enc: Encoding,
    },
    Bitmap(HBITMAP),
    /// Not text or a picture: try a preview handler, then a thumbnail.
    Other,
    Message(String),
}

struct Done {
    seq: u64,
    loaded: Loaded,
}

#[derive(Default)]
pub struct Preview {
    pub host: Cell<HWND>,
    pub text: Cell<HWND>,
    seq: Cell<u64>,
    /// What is shown: path plus the query its highlights came from.
    key: RefCell<String>,
    path: RefCell<String>,
    bitmap: Cell<HBITMAP>,
    alpha: Cell<bool>,
    message: RefCell<String>,
    handler: RefCell<Option<IPreviewHandler>>,
    font: Cell<HFONT>,
    /// What the text view shows, e.g. "text · Utf16Le · 2 matches".
    text_desc: RefCell<String>,
}

impl Preview {
    /// Creates the (hidden) pane as a child of `parent`.
    pub fn create(&self, parent: HWND) {
        unsafe {
            windows_sys::Win32::System::LibraryLoader::LoadLibraryW(wide("Msftedit.dll").as_ptr());
            let class = wide(CLASS);
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(host_proc),
                hInstance: w_::hinstance(),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hbrBackground: (COLOR_WINDOW + 1) as usize as HBRUSH,
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassExW(&wc);
        }
        let host = w_::child(
            parent,
            CLASS,
            "Preview",
            WS_CLIPCHILDREN,
            WS_EX_CLIENTEDGE,
            ID_PREVIEW,
        );
        unsafe { ShowWindow(host, SW_HIDE) };
        let style = WS_VSCROLL | (ES_MULTILINE | ES_READONLY | ES_AUTOVSCROLL) as u32;
        let text = w_::child(host, "RICHEDIT50W", "", style, 0, ID_PREVIEW_TEXT);
        unsafe { ShowWindow(text, SW_HIDE) };
        send(text, EM_SETEVENTMASK, 0, 0);
        send(text, EM_EXLIMITTEXT, 0, (MAX_TEXT * 2) as isize);
        self.host.set(host);
        self.text.set(text);
        *self.message.borrow_mut() = "Select a file to preview it.".into();
    }

    pub fn set_font(&self, dpi: i32) {
        let mut lf: LOGFONTW = unsafe { std::mem::zeroed() };
        lf.lfHeight = -(10 * dpi / 72);
        let face = wide("Consolas");
        lf.lfFaceName[..face.len()].copy_from_slice(&face);
        let f = unsafe { CreateFontIndirectW(&lf) };
        w_::set_font(self.text.get(), f);
        let old = self.font.replace(f);
        if !old.is_null() {
            unsafe { DeleteObject(old as _) };
        }
    }

    pub fn layout(&self) {
        let rc = w_::client_rect(self.host.get());
        w_::move_to(self.text.get(), 0, 0, rc.right, rc.bottom);
        if let Some(h) = &*self.handler.borrow() {
            let r = windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: rc.right,
                bottom: rc.bottom,
            };
            let _ = unsafe { h.SetRect(&r) };
        }
        unsafe { InvalidateRect(self.host.get(), null(), 1) };
    }

    /// Shows `row` (or a hint when `None`); `cq` and `query` give the
    /// highlights. Does nothing if that is already shown.
    pub fn show(&self, row: Option<&Row>, cq: Option<Arc<ContentQuery>>, query: &str) {
        let path = row.map(|r| r.full_path()).unwrap_or_default();
        let key = format!("{path}\0{query}");
        if *self.key.borrow() == key {
            return;
        }
        *self.key.borrow_mut() = key;
        let seq = self.seq.get() + 1;
        self.seq.set(seq);
        self.clear();
        let Some(row) = row else {
            self.set_message("Select a file to preview it.");
            return;
        };
        *self.path.borrow_mut() = path.clone();
        if row.attrs & OFFLINE_ATTRS != 0 && !row.is_dir {
            self.set_message("Online-only file: not downloaded for a preview.");
            return;
        }
        self.set_message("");
        let host = self.host.get() as isize;
        let (is_dir, name) = (row.is_dir, row.name.clone());
        std::thread::spawn(move || {
            let loaded = load(&path, &name, is_dir, cq.as_deref());
            post(host, seq, loaded);
        });
    }

    /// Forgets what is shown, so the next [`Preview::show`] reloads.
    pub fn invalidate(&self) {
        self.key.borrow_mut().clear();
    }

    fn clear(&self) {
        if let Some(h) = self.handler.borrow_mut().take() {
            let _ = unsafe { h.Unload() };
        }
        let old = self.bitmap.replace(null_mut());
        if !old.is_null() {
            unsafe { DeleteObject(old as _) };
        }
        unsafe { ShowWindow(self.text.get(), SW_HIDE) };
    }

    fn set_message(&self, m: &str) {
        *self.message.borrow_mut() = m.into();
        // The pane's window text says what it shows (for UI Automation).
        w_::set_text(self.host.get(), &self.state());
        unsafe { InvalidateRect(self.host.get(), null(), 1) };
    }

    fn on_loaded(&self, done: Done) {
        if done.seq != self.seq.get() {
            if let Loaded::Bitmap(b) = done.loaded {
                unsafe { DeleteObject(b as _) };
            }
            return;
        }
        match done.loaded {
            Loaded::Message(m) => self.set_message(&m),
            Loaded::Bitmap(b) => {
                self.alpha.set(has_alpha(b));
                self.bitmap.set(b);
                self.set_message("");
            }
            Loaded::Text {
                text,
                hits,
                cut,
                enc,
            } => self.show_text(text, &hits, cut, enc),
            Loaded::Other => {
                let path = self.path.borrow().clone();
                match self.open_handler(&path) {
                    Ok(h) => {
                        *self.handler.borrow_mut() = Some(h);
                        self.set_message("");
                    }
                    Err(_) => {
                        // No handler: the shell's thumbnail (an icon at worst).
                        let (host, seq) = (self.host.get() as isize, done.seq);
                        std::thread::spawn(move || {
                            let loaded = thumbnail(&path).map_or_else(
                                || Loaded::Message("No preview available.".into()),
                                Loaded::Bitmap,
                            );
                            post(host, seq, loaded);
                        });
                    }
                }
            }
        }
    }

    fn show_text(&self, mut text: Vec<u16>, hits: &[Range<usize>], cut: bool, enc: Encoding) {
        let edit = self.text.get();
        if cut {
            text.extend("\n\n[… preview cut here]".encode_utf16());
        }
        *self.text_desc.borrow_mut() = format!("text · {enc:?} · {} matches", hits.len());
        stream_in(edit, &text);
        // Default look, then the highlights.
        let mut cf: CHARFORMAT2W = unsafe { std::mem::zeroed() };
        cf.Base.cbSize = size_of::<CHARFORMAT2W>() as u32;
        cf.Base.dwMask = CFM_BACKCOLOR | CFM_BOLD;
        cf.Base.dwEffects = CFE_AUTOBACKCOLOR;
        send(
            edit,
            EM_SETCHARFORMAT,
            SCF_ALL as usize,
            &cf as *const _ as isize,
        );
        cf.Base.dwEffects = CFE_BOLD;
        cf.crBackColor = windows::Win32::Foundation::COLORREF(0x0080E6FF); // amber (BGR)
        for r in hits.iter().take(MAX_HIGHLIGHTS) {
            let cr = CHARRANGE {
                cpMin: r.start as i32,
                cpMax: r.end as i32,
            };
            send(edit, EM_EXSETSEL, 0, &cr as *const _ as isize);
            send(
                edit,
                EM_SETCHARFORMAT,
                SCF_SELECTION as usize,
                &cf as *const _ as isize,
            );
        }
        let at = hits.first().map_or(0, |r| r.start as i32);
        let cr = CHARRANGE {
            cpMin: at,
            cpMax: at,
        };
        send(edit, EM_EXSETSEL, 0, &cr as *const _ as isize);
        // Put the first match a few lines from the top.
        send(edit, WM_VSCROLL, SB_TOP as usize, 0);
        let line = send(edit, EM_EXLINEFROMCHAR, 0, at as isize);
        send(edit, EM_LINESCROLL, 0, (line - 3).max(0));
        unsafe { ShowWindow(edit, SW_SHOW) };
        self.set_message("");
    }

    fn open_handler(&self, path: &str) -> windows::core::Result<IPreviewHandler> {
        let ext = std::path::Path::new(path)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .ok_or(windows::core::Error::empty())?;
        let mut buf = [0u16; 64];
        let mut n = buf.len() as u32;
        unsafe {
            AssocQueryStringW(
                ASSOCF_INIT_DEFAULTTOSTAR,
                ASSOCSTR_SHELLEXTENSION,
                &HSTRING::from(ext),
                w!("{8895b1c6-b41f-4c1c-a562-0d564250836f}"),
                Some(PWSTR(buf.as_mut_ptr())),
                &mut n,
            )
            .ok()?;
            let clsid = CLSIDFromString(PCWSTR(buf.as_ptr()))?;
            // Out of process (prevhost.exe) when registered that way, like Explorer.
            let h: IPreviewHandler = CoCreateInstance(&clsid, None, CLSCTX_LOCAL_SERVER)
                .or_else(|_| CoCreateInstance(&clsid, None, CLSCTX_INPROC_SERVER))?;
            let hpath = HSTRING::from(path);
            if let Ok(i) = h.cast::<IInitializeWithStream>() {
                let s = SHCreateStreamOnFileEx(
                    &hpath,
                    (STGM_READ | STGM_SHARE_DENY_NONE).0,
                    0,
                    false,
                    None,
                )?;
                i.Initialize(&s, STGM_READ.0)?;
            } else if let Ok(i) = h.cast::<IInitializeWithFile>() {
                i.Initialize(&hpath, STGM_READ.0)?;
            } else if let Ok(i) = h.cast::<IInitializeWithItem>() {
                let item: IShellItem = SHCreateItemFromParsingName(&hpath, None)?;
                i.Initialize(&item, STGM_READ.0)?;
            } else {
                return Err(windows::core::Error::empty());
            }
            let rc = w_::client_rect(self.host.get());
            let r = windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: rc.right,
                bottom: rc.bottom,
            };
            h.SetWindow(windows::Win32::Foundation::HWND(self.host.get() as _), &r)?;
            h.DoPreview()?;
            Ok(h)
        }
    }

    fn paint(&self, hwnd: HWND) {
        unsafe {
            let mut ps: PAINTSTRUCT = std::mem::zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            let rc = w_::client_rect(hwnd);
            FillRect(hdc, &rc, GetSysColorBrush(COLOR_WINDOW));
            let b = self.bitmap.get();
            if !b.is_null() {
                let mut bm: BITMAP = std::mem::zeroed();
                GetObjectW(b as _, size_of::<BITMAP>() as i32, &mut bm as *mut _ as _);
                let (bw, bh) = (bm.bmWidth.max(1), bm.bmHeight.max(1));
                let pad = w_::px(hwnd, 8);
                let (aw, ah) = ((rc.right - 2 * pad).max(1), (rc.bottom - 2 * pad).max(1));
                // Fit, but don't blow small images up.
                let scale = (aw as f64 / bw as f64).min(ah as f64 / bh as f64).min(1.0);
                let (dw, dh) = ((bw as f64 * scale) as i32, (bh as f64 * scale) as i32);
                let (x, y) = ((rc.right - dw) / 2, (rc.bottom - dh) / 2);
                let mem = CreateCompatibleDC(hdc);
                let old = SelectObject(mem, b as _);
                if self.alpha.get() {
                    let bf = BLENDFUNCTION {
                        BlendOp: AC_SRC_OVER as u8,
                        BlendFlags: 0,
                        SourceConstantAlpha: 255,
                        AlphaFormat: AC_SRC_ALPHA as u8,
                    };
                    AlphaBlend(hdc, x, y, dw, dh, mem, 0, 0, bw, bh, bf);
                } else {
                    SetStretchBltMode(hdc, HALFTONE);
                    StretchBlt(hdc, x, y, dw, dh, mem, 0, 0, bw, bh, SRCCOPY);
                }
                SelectObject(mem, old);
                DeleteDC(mem);
            } else if self.handler.borrow().is_none() {
                let m = self.message.borrow();
                if !m.is_empty() {
                    let mut r = rc;
                    r.left += w_::px(hwnd, 12);
                    r.right -= w_::px(hwnd, 12);
                    r.top += w_::px(hwnd, 24);
                    let old =
                        SelectObject(hdc, SendMessageW(GetParent(hwnd), WM_GETFONT, 0, 0) as _);
                    SetBkMode(hdc, TRANSPARENT as i32);
                    SetTextColor(hdc, GetSysColor(COLOR_GRAYTEXT));
                    let t: Vec<u16> = m.encode_utf16().collect();
                    DrawTextW(
                        hdc,
                        t.as_ptr(),
                        t.len() as i32,
                        &mut r,
                        DT_CENTER | DT_WORDBREAK | DT_NOPREFIX,
                    );
                    SelectObject(hdc, old);
                }
            }
            EndPaint(hwnd, &ps);
        }
    }

    /// What the pane shows: the text view's description, "image", "handler" or the message.
    pub fn state(&self) -> String {
        if unsafe { IsWindowVisible(self.text.get()) } != 0 {
            self.text_desc.borrow().clone()
        } else if !self.bitmap.get().is_null() {
            "image".into()
        } else if self.handler.borrow().is_some() {
            "handler".into()
        } else {
            self.message.borrow().clone()
        }
    }
}

fn post(host: isize, seq: u64, loaded: Loaded) {
    let b = Box::new(Done { seq, loaded });
    let raw = Box::into_raw(b);
    if unsafe { PostMessageW(host as HWND, WM_APP_PREVIEW, 0, raw as isize) } == 0 {
        drop(unsafe { Box::from_raw(raw) });
    }
}

fn is_picture_or_video(name: &str) -> bool {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    let has = |list: &str| list.split(';').any(|e| e == ext);
    !ext.is_empty() && (has(rapidfind_core::query::PICTURE) || has(rapidfind_core::query::VIDEO))
}

/// Runs on a background thread.
fn load(path: &str, name: &str, is_dir: bool, cq: Option<&ContentQuery>) -> Loaded {
    if is_dir || is_picture_or_video(name) {
        return thumbnail(path).map_or_else(
            || Loaded::Message("No preview available.".into()),
            Loaded::Bitmap,
        );
    }
    if content::uses_ifilter(name) {
        return Loaded::Other; // documents look best in their own previewer
    }
    let mut head = Vec::new();
    match std::fs::File::open(path) {
        Ok(f) => {
            use std::io::Read;
            if let Err(e) = f.take(MAX_TEXT as u64 + 1).read_to_end(&mut head) {
                return Loaded::Message(format!("Can't read this file: {e}"));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Loaded::Message(
            "This file isn't there any more (it was moved or deleted, or its drive is offline)."
                .into(),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Loaded::Message(
                "Can't open this file: access is denied, or another program has it locked.".into(),
            );
        }
        Err(e) => return Loaded::Message(format!("Can't open this file: {e}")),
    }
    let cut = head.len() > MAX_TEXT;
    head.truncate(MAX_TEXT);
    let (enc, _) = content::detect(&head);
    if enc == Encoding::Binary {
        return Loaded::Other;
    }
    let (enc, text) = content::decode_all(&head);
    let (text, hits) = text_and_hits(&text, cq);
    Loaded::Text {
        text,
        hits,
        cut,
        enc,
    }
}

/// The text as the rich edit stores it (`\r\n` → one character) in UTF-16,
/// and the content matches as ranges of it.
fn text_and_hits(text: &str, cq: Option<&ContentQuery>) -> (Vec<u16>, Vec<Range<usize>>) {
    let text = text.replace("\r\n", "\n");
    let bytes = cq
        .map(|q| q.find_ranges(text.as_bytes(), MAX_HIGHLIGHTS))
        .unwrap_or_default();
    // UTF-8 byte offsets → UTF-16 offsets.
    let mut map = Vec::with_capacity(bytes.len() * 2);
    let mut want: Vec<usize> = bytes.iter().flat_map(|r| [r.start, r.end]).collect();
    want.sort_unstable();
    let (mut u16_at, mut wi) = (0usize, 0usize);
    let mut offsets = std::collections::HashMap::new();
    for (b, c) in text
        .char_indices()
        .chain(std::iter::once((text.len(), '\0')))
    {
        while wi < want.len() && want[wi] <= b {
            offsets.insert(want[wi], u16_at);
            wi += 1;
        }
        u16_at += c.len_utf16();
    }
    for r in &bytes {
        map.push(offsets[&r.start]..offsets[&r.end]);
    }
    (text.encode_utf16().collect(), map)
}

/// The shell's thumbnail (or icon) for a path, about 1024 px.
fn thumbnail(path: &str) -> Option<HBITMAP> {
    unsafe {
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let r = (|| -> windows::core::Result<HBITMAP> {
            let f: IShellItemImageFactory =
                SHCreateItemFromParsingName(&HSTRING::from(path), None)?;
            let b = f.GetImage(
                windows::Win32::Foundation::SIZE { cx: 1024, cy: 1024 },
                SIIGBF_RESIZETOFIT,
            )?;
            Ok(b.0 as HBITMAP)
        })();
        if init {
            CoUninitialize();
        }
        r.ok()
    }
}

/// Whether a 32-bit bitmap uses its alpha channel (else it's drawn opaque).
fn has_alpha(b: HBITMAP) -> bool {
    unsafe {
        let mut bm: BITMAP = std::mem::zeroed();
        GetObjectW(b as _, size_of::<BITMAP>() as i32, &mut bm as *mut _ as _);
        if bm.bmBitsPixel != 32 || bm.bmBits.is_null() {
            return false;
        }
        let px = std::slice::from_raw_parts(
            bm.bmBits as *const u8,
            (bm.bmWidthBytes * bm.bmHeight) as usize,
        );
        px.as_chunks::<4>().0.iter().any(|p| p[3] != 0)
    }
}

/// Sets a rich edit's text as plain text (EM_STREAMIN, so nothing is taken for RTF).
fn stream_in(edit: HWND, text: &[u16]) {
    struct Src<'a> {
        data: &'a [u8],
        at: usize,
    }
    unsafe extern "system" fn cb(cookie: usize, buf: *mut u8, cb: i32, got: *mut i32) -> u32 {
        let s = unsafe { &mut *(cookie as *mut Src) };
        let n = (s.data.len() - s.at).min(cb.max(0) as usize);
        unsafe {
            std::ptr::copy_nonoverlapping(s.data.as_ptr().add(s.at), buf, n);
            *got = n as i32;
        }
        s.at += n;
        0
    }
    let bytes: Vec<u8> = text.iter().flat_map(|u| u.to_le_bytes()).collect();
    let mut src = Src {
        data: &bytes,
        at: 0,
    };
    let mut es = EDITSTREAM {
        dwCookie: &mut src as *mut _ as usize,
        dwError: 0,
        pfnCallback: Some(cb),
    };
    send(
        edit,
        EM_STREAMIN,
        (SF_TEXT | SF_UNICODE) as usize,
        &mut es as *mut _ as isize,
    );
}

unsafe extern "system" fn host_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let Some(app) = crate::app::app() else {
        return unsafe { DefWindowProcW(hwnd, msg, w, l) };
    };
    let p = &app.preview;
    match msg {
        WM_APP_PREVIEW => {
            let done = unsafe { Box::from_raw(l as *mut Done) };
            p.on_loaded(*done);
            0
        }
        WM_PAINT => {
            p.paint(hwnd);
            0
        }
        WM_SIZE => {
            p.layout();
            0
        }
        WM_ERASEBKGND => 1,
        _ => unsafe { DefWindowProcW(hwnd, msg, w, l) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rapidfind_core::query::SearchFlags;

    #[test]
    fn hit_offsets_are_utf16_after_crlf() {
        let cq = content::split_query("content:needle", SearchFlags::default())
            .unwrap()
            .1
            .unwrap();
        let (text, hits) = text_and_hits("å😀\r\nx NEEDLE needle", Some(&cq));
        let s = String::from_utf16(&text).unwrap();
        assert_eq!(s, "å😀\nx NEEDLE needle");
        // å = 1 unit, 😀 = 2, \n = 1, "x " = 2 → first hit at 6.
        assert_eq!(hits, vec![6..12, 13..19]);
        assert!(text_and_hits("abc", None).1.is_empty());
        assert!(
            is_picture_or_video("a.JPG")
                && is_picture_or_video("b.mkv")
                && !is_picture_or_video("c.txt")
                && !is_picture_or_video("jpg")
        );
    }
}
