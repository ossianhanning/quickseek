//! Shell integration: icons, type names, opening files, clipboard, the
//! Explorer context menu, deleting in the background, drag-out, folder
//! pickers and start-on-login.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::{null, null_mut};

use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, POINT, S_OK,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree, IDataObject,
};
use windows::Win32::System::Ole::{
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DoDragDrop, IDropSource,
    IDropSource_Impl,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CMF_NORMAL, CMIC_MASK_PTINVOKE, CMINVOKECOMMANDINFOEX, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
    FileOpenDialog, FileSaveDialog, IContextMenu, IContextMenu2, IContextMenu3, IFileOpenDialog,
    IFileSaveDialog, ILFindLastID, IShellFolder, SHBindToParent, SHParseDisplayName,
    SIGDN_FILESYSPATH,
};
use windows::core::{BOOL, HRESULT, Interface, PCSTR, PCWSTR, implement};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_INVOKEIDLIST, SHELLEXECUTEINFOW, SHFILEINFOW, SHGFI_SMALLICON, SHGFI_SYSICONINDEX,
    SHGFI_TYPENAME, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW, ShellExecuteExW, ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx,
};

use crate::w::wide;

const CMIC_MASK_UNICODE: u32 = 0x4000;

// ---- icons and type names ----------------------------------------------------

/// Small system icons and type names by extension, via SHGetFileInfo with
/// SHGFI_USEFILEATTRIBUTES (no disk access, so it's fast and works for
/// offline network files too).
#[derive(Default)]
pub struct FileInfoCache {
    map: HashMap<(bool, String), (i32, String)>,
    pub image_list: isize,
}

fn ext_of(name: &str) -> String {
    match name.rfind('.') {
        Some(p) if p > 0 => name[p..].to_lowercase(),
        _ => String::new(),
    }
}

impl FileInfoCache {
    pub fn get(&mut self, name: &str, is_dir: bool) -> (i32, String) {
        let key = (is_dir, if is_dir { String::new() } else { ext_of(name) });
        if let Some(v) = self.map.get(&key) {
            return v.clone();
        }
        let probe = wide(&format!("x{}", key.1));
        let mut sfi: SHFILEINFOW = unsafe { std::mem::zeroed() };
        let attrs = if is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
        let il = unsafe {
            SHGetFileInfoW(
                probe.as_ptr(),
                attrs,
                &mut sfi,
                size_of::<SHFILEINFOW>() as u32,
                SHGFI_SYSICONINDEX | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES | SHGFI_TYPENAME,
            )
        };
        if il != 0 {
            self.image_list = il as isize;
        }
        let n = sfi
            .szTypeName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(sfi.szTypeName.len());
        let ty = String::from_utf16_lossy(&sfi.szTypeName[..n]);
        let v = (sfi.iIcon, ty);
        self.map.insert(key, v.clone());
        v
    }
}

// ---- formatting --------------------------------------------------------------

fn thousands_sep() -> &'static str {
    static SEP: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SEP.get_or_init(|| {
        use windows_sys::Win32::Globalization::{GetLocaleInfoEx, LOCALE_STHOUSAND};
        let mut buf = [0u16; 8];
        let n = unsafe {
            GetLocaleInfoEx(null(), LOCALE_STHOUSAND, buf.as_mut_ptr(), buf.len() as i32)
        };
        if n > 1 {
            String::from_utf16_lossy(&buf[..n as usize - 1])
        } else {
            ",".into()
        }
    })
}

/// `1234567` → `1,234,567` with the user's thousands separator.
pub fn group(n: u64) -> String {
    group_with(n, thousands_sep())
}

pub fn group_with(n: u64, sep: &str) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3 * sep.len());
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push_str(sep);
        }
        out.push(c);
    }
    out
}

/// Everything-style size: whole KB, rounded up, grouped (`193,986,560 KB`).
pub fn size_kb(bytes: u64) -> String {
    format!("{} KB", group(bytes.div_ceil(1024)))
}

// ---- opening & clipboard -----------------------------------------------------

pub fn open(hwnd: HWND, path: &str) -> bool {
    let (verb, p) = (wide("open"), wide(path));
    let dir = std::path::Path::new(path)
        .parent()
        .map(|d| wide(&d.to_string_lossy()));
    let r = unsafe {
        ShellExecuteW(
            hwnd,
            verb.as_ptr(),
            p.as_ptr(),
            null(),
            dir.as_ref().map_or(null(), |d| d.as_ptr()),
            SW_SHOWNORMAL,
        )
    };
    r as isize > 32
}

/// Opens the containing folder with the item selected.
pub fn open_path(path: &str) {
    let _ = std::process::Command::new("explorer.exe")
        .arg(format!("/select,{path}"))
        .spawn();
}

pub fn properties(hwnd: HWND, path: &str) {
    let (verb, p) = (wide("properties"), wide(path));
    let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_INVOKEIDLIST;
    sei.hwnd = hwnd;
    sei.lpVerb = verb.as_ptr();
    sei.lpFile = p.as_ptr();
    sei.nShow = SW_SHOWNORMAL;
    unsafe { ShellExecuteExW(&mut sei) };
}

pub fn copy_text(hwnd: HWND, text: &str) -> bool {
    let w = wide(text);
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return false;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
        if h.is_null() {
            CloseClipboard();
            return false;
        }
        let p = GlobalLock(h) as *mut u16;
        std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
        GlobalUnlock(h);
        SetClipboardData(13 /* CF_UNICODETEXT */, h);
        CloseClipboard();
    }
    true
}

// ---- shell items ---------------------------------------------------------------

/// PIDLs for paths that share one parent folder.
struct Items {
    pidls: Vec<*mut ITEMIDLIST>,
    folder: IShellFolder,
}

impl Drop for Items {
    fn drop(&mut self) {
        for &p in &self.pidls {
            unsafe { CoTaskMemFree(Some(p as *const c_void)) };
        }
    }
}

impl Items {
    fn new(paths: &[String]) -> Option<Items> {
        if paths.is_empty() {
            return None;
        }
        let parent = |p: &str| {
            std::path::Path::new(p)
                .parent()
                .map(|x| x.to_string_lossy().to_lowercase())
        };
        let first = parent(&paths[0]);
        if paths.iter().any(|p| parent(p) != first) {
            return None; // GetUIObjectOf needs one parent folder
        }
        let mut pidls = Vec::new();
        for p in paths {
            let w = wide(p);
            let mut pidl: *mut ITEMIDLIST = null_mut();
            if unsafe { SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None) }.is_err()
            {
                for &q in &pidls {
                    unsafe { CoTaskMemFree(Some(q as *const c_void)) };
                }
                return None;
            }
            pidls.push(pidl);
        }
        let folder: IShellFolder = unsafe { SHBindToParent(pidls[0], None) }.ok()?;
        Some(Items { pidls, folder })
    }

    fn children(&self) -> Vec<*const ITEMIDLIST> {
        self.pidls
            .iter()
            .map(|&p| unsafe { ILFindLastID(p) as *const ITEMIDLIST })
            .collect()
    }

    fn ui_object<T: Interface>(&self, hwnd: HWND) -> Option<T> {
        unsafe {
            self.folder.GetUIObjectOf(
                windows::Win32::Foundation::HWND(hwnd),
                &self.children(),
                None,
            )
        }
        .ok()
    }
}

thread_local! {
    /// The shell menu being shown, so owner-drawn submenus (Send to, Open
    /// with) get their messages.
    static ACTIVE_MENU: RefCell<Option<IContextMenu>> = const { RefCell::new(None) };
}

/// Forwards WM_INITMENUPOPUP / WM_DRAWITEM / WM_MEASUREITEM / WM_MENUCHAR to
/// the active shell menu. Returns `Some(result)` when handled.
pub fn forward_menu_message(msg: u32, w: WPARAM, l: LPARAM) -> Option<LRESULT> {
    ACTIVE_MENU.with(|m| {
        let m = m.borrow();
        let cm = m.as_ref()?;
        let (w, l) = (
            windows::Win32::Foundation::WPARAM(w),
            windows::Win32::Foundation::LPARAM(l),
        );
        if let Ok(c3) = cm.cast::<IContextMenu3>() {
            let mut r = windows::Win32::Foundation::LRESULT(0);
            if unsafe { c3.HandleMenuMsg2(msg, w, l, Some(&mut r)) }.is_ok() {
                return Some(r.0);
            }
        } else if let Ok(c2) = cm.cast::<IContextMenu2>()
            && unsafe { c2.HandleMenuMsg(msg, w, l) }.is_ok()
        {
            return Some(0);
        }
        None
    })
}

pub enum MenuChoice {
    Own(u32),
    Shell,
    /// The shell's Delete (with Shift held: permanently), left to the caller
    /// to run in the background instead of blocking in `InvokeCommand`.
    Delete {
        permanent: bool,
    },
    None,
}

/// The canonical verb of shell menu item `off` ("delete", "rename", ...).
fn verb(cm: &IContextMenu, off: usize) -> String {
    let mut buf = [0u16; 64];
    let r = unsafe {
        cm.GetCommandString(
            off,
            4, /* GCS_VERBW */
            None,
            windows::core::PSTR(buf.as_mut_ptr() as *mut u8),
            buf.len() as u32,
        )
    };
    if r.is_err() {
        return String::new();
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..n]).to_ascii_lowercase()
}

const SHELL_FIRST: u32 = 0x1000;

/// Shows our `own` items followed by the real Explorer context menu for
/// `paths` (when they share a folder). Own item ids must be < 0x1000.
pub fn context_menu(
    hwnd: HWND,
    paths: &[String],
    own: &[(u32, &str)],
    x: i32,
    y: i32,
) -> MenuChoice {
    let menu = unsafe { CreatePopupMenu() };
    for &(id, text) in own {
        if id == 0 {
            crate::w::menu_sep(menu);
        } else {
            crate::w::menu_item(menu, id, text);
        }
    }
    let items = Items::new(paths);
    let cm: Option<IContextMenu> = items.as_ref().and_then(|i| i.ui_object(hwnd));
    if let Some(cm) = &cm {
        crate::w::menu_sep(menu);
        let count =
            unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetMenuItemCount(menu) } as u32;
        let _ = unsafe {
            cm.QueryContextMenu(
                windows::Win32::UI::WindowsAndMessaging::HMENU(menu),
                count,
                SHELL_FIRST,
                0x7FFF,
                CMF_NORMAL,
            )
        };
        ACTIVE_MENU.with(|m| *m.borrow_mut() = Some(cm.clone()));
    }
    let cmd = unsafe { TrackPopupMenuEx(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, x, y, hwnd, null()) }
        as u32;
    ACTIVE_MENU.with(|m| *m.borrow_mut() = None);
    let choice = if cmd == 0 {
        MenuChoice::None
    } else if cmd < SHELL_FIRST {
        MenuChoice::Own(cmd)
    } else if let Some(cm) = &cm
        && verb(cm, (cmd - SHELL_FIRST) as usize) == "delete"
    {
        let shift = unsafe {
            windows_sys::Win32::UI::Input::KeyboardAndMouse::GetKeyState(0x10 /* VK_SHIFT */)
        } < 0;
        MenuChoice::Delete { permanent: shift }
    } else if let Some(cm) = &cm {
        let off = (cmd - SHELL_FIRST) as usize;
        let mut ici: CMINVOKECOMMANDINFOEX = unsafe { std::mem::zeroed() };
        ici.cbSize = size_of::<CMINVOKECOMMANDINFOEX>() as u32;
        ici.fMask = CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE;
        ici.hwnd = windows::Win32::Foundation::HWND(hwnd);
        ici.lpVerb = PCSTR(off as *const u8);
        ici.lpVerbW = PCWSTR(off as *const u16);
        ici.nShow = SW_SHOWNORMAL;
        ici.ptInvoke = POINT { x, y };
        let _ = unsafe { cm.InvokeCommand(&ici as *const _ as *const _) };
        MenuChoice::Shell
    } else {
        MenuChoice::None
    };
    unsafe { DestroyMenu(menu) };
    choice
}

/// How a background delete ended.
#[derive(Debug)]
pub struct DeleteDone {
    pub paths: Vec<String>,
    pub permanent: bool,
    /// The user cancelled (in the confirmation or progress dialog), or some
    /// items were skipped.
    pub aborted: bool,
    pub error: Option<String>,
}

/// Deletes `paths` on a thread of its own with the shell's file operation
/// (its confirmation and progress dialogs, the Recycle Bin unless
/// `permanent`), then calls `done`. The calling window never waits.
/// `quiet` (the UI tests) skips confirmations and the Recycle Bin.
pub fn delete_async(
    paths: Vec<String>,
    permanent: bool,
    quiet: bool,
    done: impl FnOnce(DeleteDone) + Send + 'static,
) {
    std::thread::Builder::new()
        .name("delete".into())
        .spawn(move || {
            let (aborted, error) = match delete_now(&paths, permanent, quiet) {
                Ok(aborted) => (aborted, None),
                // Cancelled from the dialog.
                Err(e) if e.code().0 as u32 == 0x8007_04C7 || e.code().0 as u32 == 0x8027_0000 => {
                    (true, None)
                }
                Err(e) => (false, Some(e.message().to_string())),
            };
            done(DeleteDone {
                paths,
                permanent: permanent || quiet,
                aborted,
                error,
            });
        })
        .expect("delete thread");
}

fn delete_now(paths: &[String], permanent: bool, quiet: bool) -> windows::core::Result<bool> {
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
    use windows::Win32::UI::Shell::{
        FILEOPERATION_FLAGS, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_WANTNUKEWARNING,
        FOFX_RECYCLEONDELETE, FileOperation, IFileOperation, IShellItem,
        SHCreateItemFromParsingName,
    };
    unsafe {
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED).is_ok();
        let r = (|| {
            let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER)?;
            // No owner window: an owner on another thread would tie its
            // input to ours while the copy engine works.
            let mut flags = if permanent || quiet {
                0
            } else {
                FOF_ALLOWUNDO.0 | FOFX_RECYCLEONDELETE.0 | FOF_WANTNUKEWARNING.0
            };
            if quiet {
                flags |= FOF_NOCONFIRMATION.0;
            }
            op.SetOperationFlags(FILEOPERATION_FLAGS(flags))?;
            for p in paths {
                let w = wide(p);
                let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None)?;
                op.DeleteItem(&item, None)?;
            }
            op.PerformOperations()?;
            Ok(op.GetAnyOperationsAborted()?.as_bool())
        })();
        if init {
            CoUninitialize();
        }
        r
    }
}

#[implement(IDropSource)]
struct DropSource;

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            DRAGDROP_S_CANCEL
        } else if keys.0 & MK_LBUTTON.0 == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }
    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Drags files out of the list (into Explorer, editors, mail, ...).
pub fn drag_out(hwnd: HWND, paths: &[String]) -> bool {
    let Some(items) = Items::new(paths) else {
        return false;
    };
    let Some(data) = items.ui_object::<IDataObject>(hwnd) else {
        return false;
    };
    let source: IDropSource = DropSource.into();
    let mut effect = DROPEFFECT(0);
    unsafe {
        DoDragDrop(
            &data,
            &source,
            DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
            &mut effect,
        )
    }
    .is_ok()
}

// ---- dialogs -------------------------------------------------------------------

pub fn pick_folder(hwnd: HWND) -> Option<String> {
    unsafe {
        let d: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = d.GetOptions().ok()?;
        d.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)
            .ok()?;
        d.Show(Some(windows::Win32::Foundation::HWND(hwnd))).ok()?;
        let item = d.GetResult().ok()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

pub fn pick_save_file(hwnd: HWND, default_name: &str) -> Option<String> {
    unsafe {
        let d: IFileSaveDialog =
            CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let name = wide(default_name);
        d.SetFileName(PCWSTR(name.as_ptr())).ok()?;
        let ext = wide("csv");
        d.SetDefaultExtension(PCWSTR(ext.as_ptr())).ok()?;
        d.Show(Some(windows::Win32::Foundation::HWND(hwnd))).ok()?;
        let item = d.GetResult().ok()?;
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

// ---- start on login ---------------------------------------------------------------

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "quickseek";

pub fn start_on_login() -> bool {
    let (k, v) = (wide(RUN_KEY), wide(RUN_VALUE));
    let mut size = 0u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_SZ,
            null_mut(),
            null_mut(),
            &mut size,
        ) == 0
    }
}

pub fn set_start_on_login(on: bool) {
    let (k, v) = (wide(RUN_KEY), wide(RUN_VALUE));
    unsafe {
        if on {
            let exe = std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let cmd = wide(&format!("\"{exe}\" --background"));
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                k.as_ptr(),
                v.as_ptr(),
                REG_SZ,
                cmd.as_ptr() as *const _,
                (cmd.len() * 2) as u32,
            );
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouping_and_sizes() {
        assert_eq!(group_with(0, ","), "0");
        assert_eq!(group_with(999, ","), "999");
        assert_eq!(group_with(1000, ","), "1,000");
        assert_eq!(group_with(193_986_560, "\u{a0}"), "193\u{a0}986\u{a0}560");
        assert_eq!(size_kb(0), "0 KB");
        assert_eq!(size_kb(1), "1 KB");
        assert_eq!(size_kb(1024), "1 KB");
        assert_eq!(size_kb(1025), "2 KB");
    }

    #[test]
    fn type_names_and_icons_by_extension() {
        let mut c = FileInfoCache::default();
        let (_, folder) = c.get("Windows", true);
        let (_, txt) = c.get("notes.TXT", false);
        assert!(!folder.is_empty() && !txt.is_empty());
        assert_eq!(
            c.get("other.txt", false).1,
            txt,
            "cached per extension, case-insensitive"
        );
        assert_eq!(ext_of(".gitignore"), "");
        assert_eq!(ext_of("a.tar.GZ"), ".gz");
    }
}
