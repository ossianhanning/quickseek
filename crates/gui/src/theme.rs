//! Light and dark themes: one set of colour tokens ([`Palette`]) that every
//! painted surface reads, and the Win32 work that makes the common controls,
//! menus, scroll bars and the title bar follow it.
//!
//! The mode is System (Windows' "app mode", `AppsUseLightTheme`), Light or
//! Dark. Light uses the system colours, as before; dark is a fixed palette.
//! High contrast always uses the system colours.
//!
//! Windows has no public dark mode for Win32 controls. What's used here is
//! what Explorer and Notepad++ use: the `DarkMode_Explorer` / `DarkMode_CFD`
//! / `DarkMode_ItemsView` visual styles, uxtheme's (unnamed, by ordinal)
//! preferred app mode for popup menus, `DWMWA_USE_IMMERSIVE_DARK_MODE` for
//! the title bar, and painting of our own for what has no dark style: the
//! menu bar (the undocumented UAH messages), the status bar, the tab control
//! and checkbox labels.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ptr::{null, null_mut};
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetFocus, IsWindowEnabled};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::w::{self, px, send, wide};

/// What the user picked (View → Theme), as saved in the settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    System,
    Light,
    Dark,
}

impl Mode {
    pub fn parse(s: &str) -> Mode {
        match s {
            "light" => Mode::Light,
            "dark" => Mode::Dark,
            _ => Mode::System,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::System => "system",
            Mode::Light => "light",
            Mode::Dark => "dark",
        }
    }
}

/// Colour tokens (COLORREF, 0x00BBGGRR). Charts keep their meanings in both:
/// growth red, shrinkage blue, "everything else" grey.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub dark: bool,
    /// Lists, the tree, chart panes, the preview.
    pub window: COLORREF,
    pub text: COLORREF,
    /// Secondary text: hints, labels, deleting rows.
    pub muted: COLORREF,
    /// Dialogs, the menu bar, the status bar, tab bodies.
    pub face: COLORREF,
    /// Edit boxes and list boxes in dialogs.
    pub input: COLORREF,
    /// Borders and separators.
    pub edge: COLORREF,
    /// Menu bar items under the mouse, the selected tab.
    pub hot: COLORREF,
    /// Quickseek's teal: progress, the compared-with marker, folder bars.
    pub accent: COLORREF,
    /// Chart text and lines.
    pub ink: COLORREF,
    pub grid: COLORREF,
    pub grow: COLORREF,
    pub shrink: COLORREF,
    /// The same as fills with white text on them (map boxes, calendar days).
    pub grow_fill: COLORREF,
    pub shrink_fill: COLORREF,
    /// "Everything else" in stacked charts.
    pub other: COLORREF,
    /// Categorical colours for chart layers.
    pub layers: [COLORREF; 5],
    /// A map box that didn't change; changes blend from it to grow/shrink.
    pub neutral: COLORREF,
    /// Text on a strongly coloured fill.
    pub on_strong: COLORREF,
    /// Text on a lightly coloured (or neutral) fill.
    pub on_weak: COLORREF,
    /// "% of parent" bars: the track, folders, files, drives.
    pub track: COLORREF,
    pub bar_dir: COLORREF,
    pub bar_file: COLORREF,
    pub bar_root: COLORREF,
    /// Where a bar ended before (comparing), and the tree's +/− boxes.
    pub ghost: COLORREF,
    /// Matches in the text preview.
    pub highlight: COLORREF,
}

fn sys(i: SYS_COLOR_INDEX) -> COLORREF {
    unsafe { GetSysColor(i) }
}

impl Palette {
    pub fn light() -> Palette {
        Palette {
            dark: false,
            window: sys(COLOR_WINDOW),
            text: sys(COLOR_WINDOWTEXT),
            muted: sys(COLOR_GRAYTEXT),
            face: sys(COLOR_BTNFACE),
            input: sys(COLOR_WINDOW),
            edge: sys(COLOR_3DSHADOW),
            hot: sys(COLOR_HIGHLIGHT),
            accent: 0x0056_6E0F,
            ink: 0x0035_3535,
            grid: 0x00D9_E0E1,
            grow: 0x004A_4BE2,
            shrink: 0x00DD_8A37,
            grow_fill: 0x004A_4BE2,
            shrink_fill: 0x00DD_8A37,
            other: 0x00B7_C2C3,
            layers: [
                0x00D6_782A,
                0x0034_68EB,
                0x007A_AF1B,
                0x0000_A1ED,
                0x00A4_7BE8,
            ],
            neutral: 0x00E6_E2DE,
            on_strong: 0x00FF_FFFF,
            on_weak: 0x0035_3535,
            track: 0x00EC_E8E4,
            bar_dir: 0x0056_6E0F,
            bar_file: 0x00AE_C07F,
            bar_root: 0x007A_6B5B,
            ghost: 0x0080_8788,
            highlight: 0x0080_E6FF,
        }
    }

    /// Windows 11's dark surfaces; chart colours lighter so they keep
    /// their contrast (WCAG AA for text: 4.5:1 and more on `window`).
    pub fn dark() -> Palette {
        Palette {
            dark: true,
            window: 0x0020_2020,
            text: 0x00EA_EAEA,
            muted: 0x00A6_A6A6,
            face: 0x002B_2B2B,
            input: 0x001F_1F1F,
            edge: 0x0050_5050,
            hot: 0x0045_4545,
            accent: 0x00A5_CA5D,
            ink: 0x00E3_E3E3,
            grid: 0x0042_3F3B,
            grow: 0x006E_6EFF,
            shrink: 0x00F7_A64F,
            grow_fill: 0x003A_3AD6,
            shrink_fill: 0x00C8_782E,
            other: 0x0088_8079,
            layers: [
                0x00F0_9B5A,
                0x005E_8AF5,
                0x0094_C93D,
                0x003A_B8F5,
                0x00B8_96F0,
            ],
            neutral: 0x0045_3F3A,
            on_strong: 0x00FF_FFFF,
            on_weak: 0x00EA_EAEA,
            track: 0x0042_3D3A,
            bar_dir: 0x00A5_CA5D,
            bar_file: 0x0066_7D2E,
            bar_root: 0x00AA_9A8A,
            ghost: 0x00A0_A0A0,
            highlight: 0x0000_5C80,
        }
    }
}

thread_local! {
    static MODE: Cell<Mode> = const { Cell::new(Mode::System) };
    static PAL: Cell<Option<Palette>> = const { Cell::new(None) };
    static BRUSHES: RefCell<HashMap<COLORREF, isize>> = RefCell::new(HashMap::new());
}

/// The palette in effect (UI thread).
pub fn p() -> Palette {
    PAL.with(|c| {
        c.get().unwrap_or_else(|| {
            let p = compute(MODE.with(Cell::get));
            c.set(Some(p));
            p
        })
    })
}

pub fn dark() -> bool {
    p().dark
}

pub fn mode() -> Mode {
    MODE.with(Cell::get)
}

/// A solid brush of colour `c`, kept for the life of the program (there
/// are only a few dozen colours).
pub fn brush(c: COLORREF) -> HBRUSH {
    BRUSHES.with(|b| {
        *b.borrow_mut()
            .entry(c)
            .or_insert_with(|| unsafe { CreateSolidBrush(c) as isize })
    }) as HBRUSH
}

fn compute(m: Mode) -> Palette {
    let dark = !high_contrast()
        && dark_supported()
        && match m {
            Mode::Dark => true,
            Mode::Light => false,
            Mode::System => system_dark(),
        };
    if dark {
        Palette::dark()
    } else {
        Palette::light()
    }
}

/// Windows' app mode: Settings → Personalization → Colors.
pub fn system_dark() -> bool {
    let (key, value) = (
        wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
        wide("AppsUseLightTheme"),
    );
    let mut v: u32 = 1;
    let mut n = 4u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            &mut v as *mut u32 as _,
            &mut n,
        )
    };
    r == 0 && v == 0
}

fn high_contrast() -> bool {
    let mut hc: HIGHCONTRASTW = unsafe { std::mem::zeroed() };
    hc.cbSize = size_of::<HIGHCONTRASTW>() as u32;
    unsafe {
        SystemParametersInfoW(SPI_GETHIGHCONTRAST, hc.cbSize, &mut hc as *mut _ as _, 0) != 0
            && hc.dwFlags & HCF_HIGHCONTRASTON != 0
    }
}

/// uxtheme's dark mode entry points (by ordinal; Windows 10 1903 and later).
struct Ux {
    set_preferred_app_mode: Option<unsafe extern "system" fn(i32) -> i32>,
    allow_dark_mode_for_window: Option<unsafe extern "system" fn(HWND, i32) -> i32>,
    refresh_policy: Option<unsafe extern "system" fn()>,
    flush_menu_themes: Option<unsafe extern "system" fn()>,
}

fn ux() -> &'static Ux {
    static UX: OnceLock<Ux> = OnceLock::new();
    UX.get_or_init(|| unsafe {
        let none = Ux {
            set_preferred_app_mode: None,
            allow_dark_mode_for_window: None,
            refresh_policy: None,
            flush_menu_themes: None,
        };
        if build() < 18362 {
            return none;
        }
        let h = LoadLibraryW(wide("uxtheme.dll").as_ptr());
        if h.is_null() {
            return none;
        }
        let f = |n: usize| GetProcAddress(h, n as *const u8);
        Ux {
            set_preferred_app_mode: f(135).map(|f| std::mem::transmute(f)),
            allow_dark_mode_for_window: f(133).map(|f| std::mem::transmute(f)),
            refresh_policy: f(104).map(|f| std::mem::transmute(f)),
            flush_menu_themes: f(136).map(|f| std::mem::transmute(f)),
        }
    })
}

/// Windows' build number (0 if unknown).
fn build() -> u32 {
    static B: OnceLock<u32> = OnceLock::new();
    *B.get_or_init(|| {
        let (key, value) = (
            wide(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            wide("CurrentBuildNumber"),
        );
        let mut buf = [0u16; 16];
        let mut n = (buf.len() * 2) as u32;
        let r = unsafe {
            RegGetValueW(
                windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                value.as_ptr(),
                windows_sys::Win32::System::Registry::RRF_RT_REG_SZ,
                null_mut(),
                buf.as_mut_ptr() as _,
                &mut n,
            )
        };
        if r != 0 {
            return 0;
        }
        String::from_utf16_lossy(&buf[..(n as usize / 2).saturating_sub(1)])
            .trim()
            .parse()
            .unwrap_or(0)
    })
}

fn dark_supported() -> bool {
    ux().set_preferred_app_mode.is_some()
}

/// Sets the mode at startup (before windows are created).
pub fn init(m: Mode) {
    MODE.with(|c| c.set(m));
    PAL.with(|c| c.set(Some(compute(m))));
    app_mode();
}

/// Popup menus (ours, and the shell's context menus) follow this.
fn app_mode() {
    let u = ux();
    unsafe {
        if let Some(f) = u.set_preferred_app_mode {
            f(if dark() { 2 } else { 3 }); // ForceDark / ForceLight
        }
        if let Some(f) = u.refresh_policy {
            f();
        }
        if let Some(f) = u.flush_menu_themes {
            f();
        }
    }
}

/// Switches the mode (View → Theme) and repaints everything.
pub fn set_mode(m: Mode) {
    MODE.with(|c| c.set(m));
    refresh();
}

/// Windows' colours or app mode changed (WM_SETTINGCHANGE "ImmersiveColorSet",
/// WM_SYSCOLORCHANGE): re-read them.
pub fn system_changed() {
    let before = p();
    let now = compute(mode());
    if now != before {
        refresh();
    }
}

fn refresh() {
    PAL.with(|c| c.set(Some(compute(mode()))));
    app_mode();
    unsafe extern "system" fn each(h: HWND, _: LPARAM) -> i32 {
        if is_attached(h) {
            apply(h);
        }
        1
    }
    unsafe { EnumThreadWindows(GetCurrentThreadId(), Some(each), 0) };
}

// ---- windows ---------------------------------------------------------------------

/// What a top-level window's background is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// The main window: lists, panes.
    Window = 1,
    /// Dialogs.
    Face = 2,
}

const SUB_TOP: usize = 0x7468_656d; // "them"
const SUB_CHECK: usize = 0x7468_6d31;
const SUB_STATUS: usize = 0x7468_6d32;
const SUB_TABS: usize = 0x7468_6d33;
const SUB_LIST: usize = 0x7468_6d34;
const SUB_EDGE: usize = 0x7468_6d35;

fn prop() -> Vec<u16> {
    wide("quickseek.theme")
}

fn is_attached(h: HWND) -> bool {
    unsafe { !GetPropW(h, prop().as_ptr()).is_null() }
}

/// Themes a top-level window and its children now and on every change.
pub fn attach(h: HWND, surface: Surface) {
    unsafe {
        SetPropW(h, prop().as_ptr(), surface as isize as _);
        SetWindowSubclass(h, Some(top_proc), SUB_TOP, surface as usize);
    }
    apply(h);
}

fn surface_color(s: usize) -> COLORREF {
    if s == Surface::Window as usize {
        p().window
    } else {
        p().face
    }
}

/// Applies the current theme to `top` and every control in it.
pub fn apply(top: HWND) {
    let d = dark();
    unsafe {
        if let Some(f) = ux().allow_dark_mode_for_window {
            f(top, d as i32);
        }
        let v: i32 = d as i32;
        // 20 on Windows 10 20H1 and later, 19 before.
        let attr = if build() >= 19041 { 20 } else { 19 };
        DwmSetWindowAttribute(top, attr, &v as *const i32 as _, 4);
        unsafe extern "system" fn each(h: HWND, _: LPARAM) -> i32 {
            apply_control(h);
            1
        }
        EnumChildWindows(top, Some(each), 0);
        // New visual styles can change controls' sizes (a drop-down's
        // height): lay the window out again.
        let rc = w::client_rect(top);
        send(
            top,
            WM_SIZE,
            SIZE_RESTORED as usize,
            ((rc.bottom as isize) << 16) | (rc.right as isize & 0xFFFF),
        );
        // The title bar repaints on activation; nudge the frame now.
        SetWindowPos(
            top,
            null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        RedrawWindow(
            top,
            null(),
            null_mut(),
            RDW_ERASE | RDW_INVALIDATE | RDW_FRAME | RDW_ALLCHILDREN,
        );
    }
}

fn class_of(h: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(h, buf.as_mut_ptr(), 64) } as usize;
    String::from_utf16_lossy(&buf[..n])
}

fn set_theme(h: HWND, name: Option<&str>) {
    let w = name.map(wide);
    unsafe {
        SetWindowTheme(h, w.as_ref().map_or(null(), |v| v.as_ptr()), null());
    }
}

/// Removes the visual style (classic drawing in our colours).
fn no_theme(h: HWND) {
    let e = wide("");
    unsafe { SetWindowTheme(h, e.as_ptr(), e.as_ptr()) };
}

fn apply_control(h: HWND) {
    let d = dark();
    let pal = p();
    unsafe {
        if let Some(f) = ux().allow_dark_mode_for_window {
            f(h, d as i32);
        }
    }
    let class = class_of(h);
    match class.as_str() {
        "SysListView32" => {
            set_theme(h, Some(if d { "DarkMode_Explorer" } else { "Explorer" }));
            send(h, LVM_SETBKCOLOR, 0, pal.window as isize);
            send(h, LVM_SETTEXTBKCOLOR, 0, pal.window as isize);
            send(h, LVM_SETTEXTCOLOR, 0, pal.text as isize);
            let header = send(h, LVM_GETHEADER, 0, 0) as HWND;
            if !header.is_null() {
                set_theme(header, d.then_some("DarkMode_ItemsView"));
            }
            let tips = send(h, LVM_GETTOOLTIPS, 0, 0) as HWND;
            if !tips.is_null() {
                set_theme(tips, d.then_some("DarkMode_Explorer"));
            }
            unsafe { SetWindowSubclass(h, Some(list_proc), SUB_LIST, 0) };
        }
        "Edit" => set_theme(h, d.then_some("DarkMode_CFD")),
        "RICHEDIT50W" => {
            set_theme(h, d.then_some("DarkMode_Explorer"));
            use windows::Win32::UI::Controls::RichEdit as re;
            send(h, re::EM_SETBKGNDCOLOR, 0, pal.window as isize);
            // The text's colour, leaving the highlights' backgrounds.
            let mut cf: re::CHARFORMAT2W = unsafe { std::mem::zeroed() };
            cf.Base.cbSize = size_of::<re::CHARFORMAT2W>() as u32;
            cf.Base.dwMask = re::CFM_COLOR;
            cf.Base.crTextColor = windows::Win32::Foundation::COLORREF(pal.text);
            send(
                h,
                re::EM_SETCHARFORMAT,
                re::SCF_ALL as usize,
                &cf as *const _ as isize,
            );
        }
        "ComboBox" => {
            set_theme(h, d.then_some("DarkMode_CFD"));
            let mut info: COMBOBOXINFO = unsafe { std::mem::zeroed() };
            info.cbSize = size_of::<COMBOBOXINFO>() as u32;
            if unsafe { GetComboBoxInfo(h, &mut info) } != 0 && !info.hwndList.is_null() {
                set_theme(info.hwndList, d.then_some("DarkMode_Explorer"));
            }
        }
        "ListBox" | "ScrollBar" => set_theme(h, d.then_some("DarkMode_Explorer")),
        "Button" => {
            set_theme(h, d.then_some("DarkMode_Explorer"));
            if is_check(h) {
                unsafe { SetWindowSubclass(h, Some(check_proc), SUB_CHECK, 0) };
            }
        }
        "msctls_statusbar32" => unsafe {
            SetWindowSubclass(h, Some(status_proc), SUB_STATUS, 0);
        },
        "SysTabControl32" => unsafe {
            SetWindowSubclass(h, Some(tabs_proc), SUB_TABS, 0);
        },
        // Our own panes (the preview): their sunken border.
        c if c.starts_with("quickseek")
            && unsafe { GetWindowLongW(h, GWL_EXSTYLE) } as u32 & WS_EX_CLIENTEDGE != 0 =>
        unsafe {
            SetWindowSubclass(h, Some(edge_proc), SUB_EDGE, 0);
            SetWindowPos(
                h,
                null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        },
        "msctls_progress32" => {
            if d {
                no_theme(h);
                send(h, PBM_SETBKCOLOR, 0, pal.input as isize);
                send(h, PBM_SETBARCOLOR, 0, pal.accent as isize);
            } else {
                send(h, PBM_SETBKCOLOR, 0, CLR_DEFAULT as isize);
                send(h, PBM_SETBARCOLOR, 0, CLR_DEFAULT as isize);
                set_theme(h, None);
            }
        }
        _ => {}
    }
    unsafe { InvalidateRect(h, null(), 1) };
}

fn is_check(h: HWND) -> bool {
    let style = unsafe { GetWindowLongW(h, GWL_STYLE) } as u32;
    let kind = style & BS_TYPEMASK as u32;
    style & BS_PUSHLIKE as u32 == 0
        && matches!(
            kind as i32,
            BS_CHECKBOX
                | BS_AUTOCHECKBOX
                | BS_3STATE
                | BS_AUTO3STATE
                | BS_RADIOBUTTON
                | BS_AUTORADIOBUTTON
        )
}

/// WM_CTLCOLOR*: our colours in dark mode (None in light: the default).
pub fn ctl_color(msg: u32, hdc: HDC, surface: COLORREF) -> Option<LRESULT> {
    let pal = p();
    if !pal.dark {
        return None;
    }
    let bg = match msg {
        WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => pal.input,
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN | WM_CTLCOLORDLG => surface,
        _ => return None,
    };
    unsafe {
        SetTextColor(hdc, pal.text);
        SetBkColor(hdc, bg);
    }
    Some(brush(bg) as LRESULT)
}

// ---- the menu bar (undocumented UAH messages) ------------------------------------

const WM_UAHDRAWMENU: u32 = 0x0091;
const WM_UAHDRAWMENUITEM: u32 = 0x0092;

#[repr(C)]
struct UahMenu {
    hmenu: HMENU,
    hdc: HDC,
    flags: u32,
}

#[repr(C)]
struct UahMenuItem {
    position: i32,
    metrics: [u32; 8],
    popup: [u32; 5],
}

#[repr(C)]
struct UahDrawMenuItem {
    dis: DRAWITEMSTRUCT,
    um: UahMenu,
    umi: UahMenuItem,
}

/// The menu bar's rect in window coordinates.
fn menu_bar_rect(h: HWND) -> Option<RECT> {
    let mut mbi: MENUBARINFO = unsafe { std::mem::zeroed() };
    mbi.cbSize = size_of::<MENUBARINFO>() as u32;
    if unsafe { GetMenuBarInfo(h, OBJID_MENU, 0, &mut mbi) } == 0 {
        return None;
    }
    let mut wr: RECT = unsafe { std::mem::zeroed() };
    unsafe { GetWindowRect(h, &mut wr) };
    let mut r = mbi.rcBar;
    unsafe { OffsetRect(&mut r, -wr.left, -wr.top) };
    Some(r)
}

fn draw_menu_bar(h: HWND, um: &UahMenu) {
    if let Some(r) = menu_bar_rect(h) {
        unsafe { FillRect(um.hdc, &r, brush(p().face)) };
    }
}

fn draw_menu_item(d: &UahDrawMenuItem) {
    let pal = p();
    let mut buf = [0u16; 128];
    let mut mii: MENUITEMINFOW = unsafe { std::mem::zeroed() };
    mii.cbSize = size_of::<MENUITEMINFOW>() as u32;
    mii.fMask = MIIM_STRING;
    mii.dwTypeData = buf.as_mut_ptr();
    mii.cch = buf.len() as u32 - 1;
    unsafe { GetMenuItemInfoW(d.um.hmenu, d.umi.position as u32, 1, &mut mii) };
    let state = d.dis.itemState;
    let hot = state & (ODS_HOTLIGHT | ODS_SELECTED) != 0;
    let grey = state & (ODS_INACTIVE | ODS_GRAYED | ODS_DISABLED) != 0;
    let mut flags = DT_CENTER | DT_SINGLELINE | DT_VCENTER;
    if state & ODS_NOACCEL != 0 {
        flags |= DT_HIDEPREFIX;
    }
    let mut rc = d.dis.rcItem;
    unsafe {
        FillRect(d.um.hdc, &rc, brush(if hot { pal.hot } else { pal.face }));
        SetBkMode(d.um.hdc, TRANSPARENT as i32);
        SetTextColor(d.um.hdc, if grey { pal.muted } else { pal.text });
        DrawTextW(d.um.hdc, buf.as_ptr(), mii.cch as i32, &mut rc, flags);
    }
}

/// The light line Windows draws between the menu bar and the client area.
fn cover_menu_line(h: HWND) {
    let Some(bar) = menu_bar_rect(h) else { return };
    let r = RECT {
        left: bar.left,
        top: bar.bottom,
        right: bar.right,
        bottom: bar.bottom + 1,
    };
    unsafe {
        let dc = GetWindowDC(h);
        FillRect(dc, &r, brush(p().face));
        ReleaseDC(h, dc);
    }
}

unsafe extern "system" fn top_proc(
    h: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    surface: usize,
) -> LRESULT {
    match msg {
        WM_SETTINGCHANGE => {
            if lp != 0 {
                let s = unsafe { wide_at(lp as *const u16) };
                if s == "ImmersiveColorSet" {
                    system_changed();
                }
            }
        }
        WM_SYSCOLORCHANGE => system_changed(),
        _ => {}
    }
    if dark() {
        match msg {
            WM_ERASEBKGND => {
                let rc = w::client_rect(h);
                unsafe { FillRect(wp as HDC, &rc, brush(surface_color(surface))) };
                return 1;
            }
            WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORSTATIC | WM_CTLCOLORBTN
            | WM_CTLCOLORDLG => {
                if let Some(r) = ctl_color(msg, wp as HDC, surface_color(surface)) {
                    return r;
                }
            }
            WM_UAHDRAWMENU => {
                draw_menu_bar(h, unsafe { &*(lp as *const UahMenu) });
                return 1;
            }
            WM_UAHDRAWMENUITEM => {
                draw_menu_item(unsafe { &*(lp as *const UahDrawMenuItem) });
                return 1;
            }
            WM_NCPAINT | WM_NCACTIVATE => {
                let r = unsafe { DefSubclassProc(h, msg, wp, lp) };
                cover_menu_line(h);
                return r;
            }
            _ => {}
        }
    }
    unsafe { DefSubclassProc(h, msg, wp, lp) }
}

unsafe fn wide_at(p: *const u16) -> String {
    let mut n = 0;
    while unsafe { *p.add(n) } != 0 && n < 256 {
        n += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, n) })
}

// ---- controls without a dark style -------------------------------------------------

fn window_text(h: HWND) -> Vec<u16> {
    let n = unsafe { GetWindowTextLengthW(h) } as usize;
    let mut buf = vec![0u16; n + 1];
    let got = unsafe { GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32) } as usize;
    buf.truncate(got);
    buf
}

/// The colour behind a control: its parent's surface.
fn behind(h: HWND) -> COLORREF {
    let mut cur = unsafe { GetParent(h) };
    while !cur.is_null() {
        let v = unsafe { GetPropW(cur, prop().as_ptr()) } as usize;
        if v != 0 {
            return surface_color(v);
        }
        cur = unsafe { GetParent(cur) };
    }
    p().face
}

/// Checkboxes and radio buttons: the dark style draws the box, but the
/// label stays black, so the label is ours.
unsafe extern "system" fn check_proc(
    h: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _d: usize,
) -> LRESULT {
    if !dark() {
        return unsafe { DefSubclassProc(h, msg, wp, lp) };
    }
    match msg {
        WM_ERASEBKGND => 1,
        WM_PAINT | WM_PRINTCLIENT => {
            let mut ps: PAINTSTRUCT = unsafe { std::mem::zeroed() };
            let hdc = if msg == WM_PAINT {
                unsafe { BeginPaint(h, &mut ps) }
            } else {
                wp as HDC
            };
            paint_check(h, hdc);
            if msg == WM_PAINT {
                unsafe { EndPaint(h, &ps) };
            }
            0
        }
        // These repaint the default way, outside WM_PAINT.
        BM_SETCHECK | BM_SETSTATE | WM_ENABLE | WM_SETTEXT | WM_UPDATEUISTATE | WM_SETFOCUS
        | WM_KILLFOCUS => {
            let r = unsafe { DefSubclassProc(h, msg, wp, lp) };
            unsafe { InvalidateRect(h, null(), 0) };
            r
        }
        _ => unsafe { DefSubclassProc(h, msg, wp, lp) },
    }
}

fn paint_check(h: HWND, hdc: HDC) {
    let pal = p();
    let rc = w::client_rect(h);
    let style = unsafe { GetWindowLongW(h, GWL_STYLE) } as u32;
    let radio = matches!(
        (style & BS_TYPEMASK as u32) as i32,
        BS_RADIOBUTTON | BS_AUTORADIOBUTTON
    );
    let checked = send(h, BM_GETCHECK, 0, 0) as u32;
    let st = send(h, BM_GETSTATE, 0, 0) as u32;
    let enabled = unsafe { IsWindowEnabled(h) } != 0;
    // CBS_/RBS_: normal 1, hot 2, pressed 3, disabled 4; +4 checked, +8 mixed.
    let base = if !enabled {
        4
    } else if st & BST_PUSHED != 0 {
        3
    } else if st & BST_HOT != 0 {
        2
    } else {
        1
    };
    let state = base
        + match checked {
            BST_CHECKED => 4,
            BST_INDETERMINATE if !radio => 8,
            _ => 0,
        };
    unsafe {
        FillRect(hdc, &rc, brush(behind(h)));
        let font = send(h, WM_GETFONT, 0, 0) as HFONT;
        let old = SelectObject(hdc, font as _);
        let class = wide("Button");
        let theme = OpenThemeData(h, class.as_ptr());
        let part = if radio { BP_RADIOBUTTON } else { BP_CHECKBOX };
        let mut sz = windows_sys::Win32::Foundation::SIZE {
            cx: px(h, 13),
            cy: px(h, 13),
        };
        if theme != 0 {
            GetThemePartSize(theme, hdc, part, state as i32, null(), TS_DRAW, &mut sz);
        }
        let top = (rc.bottom - sz.cy) / 2;
        let glyph = RECT {
            left: 0,
            top,
            right: sz.cx,
            bottom: top + sz.cy,
        };
        if theme != 0 {
            DrawThemeBackground(theme, hdc, part, state as i32, &glyph, null());
            CloseThemeData(theme);
        }
        let text = window_text(h);
        let mut tr = RECT {
            left: sz.cx + px(h, 4),
            ..rc
        };
        let ui = send(h, WM_QUERYUISTATE, 0, 0) as u32;
        let mut flags = DT_LEFT | DT_VCENTER | DT_SINGLELINE;
        if style & BS_MULTILINE as u32 != 0 {
            flags = DT_LEFT | DT_WORDBREAK;
            let mut calc = tr;
            DrawTextW(
                hdc,
                text.as_ptr(),
                text.len() as i32,
                &mut calc,
                flags | DT_CALCRECT,
            );
            let hgt = calc.bottom - calc.top;
            tr.top = (rc.bottom - hgt) / 2;
        }
        if ui & UISF_HIDEACCEL != 0 {
            flags |= DT_HIDEPREFIX;
        }
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, if enabled { pal.text } else { pal.muted });
        let mut drawn = tr;
        DrawTextW(hdc, text.as_ptr(), text.len() as i32, &mut drawn, flags);
        if GetFocus() == h && ui & UISF_HIDEFOCUS == 0 && !text.is_empty() {
            let mut calc = tr;
            DrawTextW(
                hdc,
                text.as_ptr(),
                text.len() as i32,
                &mut calc,
                flags | DT_CALCRECT,
            );
            let fr = RECT {
                left: calc.left - 1,
                top: if flags & DT_VCENTER != 0 {
                    (rc.bottom - (calc.bottom - calc.top)) / 2 - 1
                } else {
                    calc.top - 1
                },
                right: calc.right + 1,
                bottom: 0,
            };
            let fr = RECT {
                bottom: fr.top + (calc.bottom - calc.top) + 2,
                ..fr
            };
            SetTextColor(hdc, pal.text);
            DrawFocusRect(hdc, &fr);
        }
        SelectObject(hdc, old);
    }
}

/// The status bar: its parts' text on our surface.
unsafe extern "system" fn status_proc(
    h: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _d: usize,
) -> LRESULT {
    if !dark() {
        return unsafe { DefSubclassProc(h, msg, wp, lp) };
    }
    match msg {
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let pal = p();
            unsafe {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let hdc = BeginPaint(h, &mut ps);
                let rc = w::client_rect(h);
                FillRect(hdc, &rc, brush(pal.face));
                let top = RECT {
                    bottom: rc.top + 1,
                    ..rc
                };
                FillRect(hdc, &top, brush(pal.edge));
                let font = send(h, WM_GETFONT, 0, 0) as HFONT;
                let old = SelectObject(hdc, font as _);
                SetBkMode(hdc, TRANSPARENT as i32);
                SetTextColor(hdc, pal.text);
                let parts = send(h, SB_GETPARTS, 0, 0).max(1) as usize;
                for i in 0..parts {
                    let mut r: RECT = std::mem::zeroed();
                    send(h, SB_GETRECT, i, &mut r as *mut _ as isize);
                    let len = (send(h, SB_GETTEXTLENGTHW, i, 0) & 0xFFFF) as usize;
                    let mut buf = vec![0u16; len + 1];
                    send(h, SB_GETTEXTW, i, buf.as_mut_ptr() as isize);
                    let mut tr = RECT {
                        left: r.left + px(h, 6),
                        right: r.right - px(h, 4),
                        ..r
                    };
                    DrawTextW(
                        hdc,
                        buf.as_ptr(),
                        len as i32,
                        &mut tr,
                        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
                    );
                    if i + 1 < parts {
                        let sep = RECT {
                            left: r.right - 1,
                            top: r.top + px(h, 3),
                            right: r.right,
                            bottom: r.bottom - px(h, 3),
                        };
                        FillRect(hdc, &sep, brush(pal.edge));
                    }
                }
                // The size grip: three rows of dots, as Windows 11 draws it.
                let style = GetWindowLongW(h, GWL_STYLE) as u32;
                if style & SBARS_SIZEGRIP != 0 && IsZoomed(GetParent(h)) == 0 {
                    let d = px(h, 2).max(1);
                    let step = px(h, 4);
                    for row in 0..3 {
                        for col in 0..=row {
                            let x = rc.right - px(h, 3) - (col + 1) * step + (step - d);
                            let y = rc.bottom - px(h, 3) - (3 - row) * step + (step - d);
                            let dot = RECT {
                                left: x,
                                top: y,
                                right: x + d,
                                bottom: y + d,
                            };
                            FillRect(hdc, &dot, brush(pal.muted));
                        }
                    }
                }
                SelectObject(hdc, old);
                EndPaint(h, &ps);
            }
            0
        }
        _ => unsafe { DefSubclassProc(h, msg, wp, lp) },
    }
}

/// The options window's tab control: tabs and body on our surface; its
/// pages' controls ask it for their colours.
unsafe extern "system" fn tabs_proc(
    h: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _d: usize,
) -> LRESULT {
    if !dark() {
        return unsafe { DefSubclassProc(h, msg, wp, lp) };
    }
    match msg {
        WM_ERASEBKGND => 1,
        WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX | WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            ctl_color(msg, wp as HDC, p().face).unwrap_or(0)
        }
        WM_PAINT => {
            let pal = p();
            unsafe {
                let mut ps: PAINTSTRUCT = std::mem::zeroed();
                let hdc = BeginPaint(h, &mut ps);
                let rc = w::client_rect(h);
                FillRect(hdc, &rc, brush(pal.face));
                let mut body = rc;
                send(h, TCM_ADJUSTRECT, 0, &mut body as *mut _ as isize);
                let frame = RECT {
                    left: body.left - px(h, 4),
                    top: body.top - px(h, 3),
                    right: body.right + px(h, 4),
                    bottom: body.bottom + px(h, 4),
                };
                FrameRect(hdc, &frame, brush(pal.edge));
                let font = send(h, WM_GETFONT, 0, 0) as HFONT;
                let old = SelectObject(hdc, font as _);
                SetBkMode(hdc, TRANSPARENT as i32);
                let n = send(h, TCM_GETITEMCOUNT, 0, 0).max(0) as usize;
                let cur = send(h, TCM_GETCURSEL, 0, 0);
                for i in 0..n {
                    let mut r: RECT = std::mem::zeroed();
                    send(h, TCM_GETITEMRECT, i, &mut r as *mut _ as isize);
                    let selected = i as isize == cur;
                    if selected {
                        r.top -= px(h, 2);
                        r.bottom = frame.top + 1;
                    }
                    FillRect(hdc, &r, brush(if selected { pal.hot } else { pal.face }));
                    FrameRect(hdc, &r, brush(pal.edge));
                    if selected {
                        let join = RECT {
                            left: r.left + 1,
                            top: r.bottom - 1,
                            right: r.right - 1,
                            bottom: r.bottom,
                        };
                        FillRect(hdc, &join, brush(pal.hot));
                    }
                    let mut buf = [0u16; 64];
                    let mut item: TCITEMW = std::mem::zeroed();
                    item.mask = TCIF_TEXT;
                    item.pszText = buf.as_mut_ptr();
                    item.cchTextMax = buf.len() as i32;
                    send(h, TCM_GETITEMW, i, &mut item as *mut _ as isize);
                    let len = buf.iter().position(|&c| c == 0).unwrap_or(0);
                    SetTextColor(hdc, if selected { pal.text } else { pal.muted });
                    DrawTextW(
                        hdc,
                        buf.as_ptr(),
                        len as i32,
                        &mut r,
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                    );
                }
                SelectObject(hdc, old);
                EndPaint(h, &ps);
            }
            0
        }
        _ => unsafe { DefSubclassProc(h, msg, wp, lp) },
    }
}

/// A client edge in the edge colour (Windows draws it in light 3D colours).
unsafe extern "system" fn edge_proc(
    h: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _d: usize,
) -> LRESULT {
    if msg != WM_NCPAINT || !dark() {
        return unsafe { DefSubclassProc(h, msg, wp, lp) };
    }
    unsafe {
        let mut wr: RECT = std::mem::zeroed();
        GetWindowRect(h, &mut wr);
        let r = RECT {
            left: 0,
            top: 0,
            right: wr.right - wr.left,
            bottom: wr.bottom - wr.top,
        };
        let dc = GetWindowDC(h);
        let pal = p();
        FrameRect(dc, &r, brush(pal.edge));
        let inner = RECT {
            left: 1,
            top: 1,
            right: r.right - 1,
            bottom: r.bottom - 1,
        };
        FrameRect(dc, &inner, brush(pal.window));
        ReleaseDC(h, dc);
    }
    0
}

/// List views: the dark header style keeps black text, so set it.
unsafe extern "system" fn list_proc(
    h: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
    _id: usize,
    _d: usize,
) -> LRESULT {
    if msg == WM_NOTIFY && dark() {
        let nm = unsafe { &*(lp as *const windows_sys::Win32::UI::Controls::NMHDR) };
        let header = send(h, LVM_GETHEADER, 0, 0) as HWND;
        if nm.hwndFrom == header && nm.code == NM_CUSTOMDRAW {
            let cd = unsafe { &*(lp as *const NMCUSTOMDRAW) };
            if cd.dwDrawStage == CDDS_PREPAINT {
                return CDRF_NOTIFYITEMDRAW as LRESULT;
            }
            if cd.dwDrawStage == CDDS_ITEMPREPAINT {
                unsafe { SetTextColor(cd.hdc, p().text) };
                return CDRF_DODEFAULT as LRESULT;
            }
        }
    }
    unsafe { DefSubclassProc(h, msg, wp, lp) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG relative luminance of a COLORREF.
    fn lum(c: COLORREF) -> f64 {
        let ch = |s: u32| {
            let v = ((c >> s) & 0xFF) as f64 / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * ch(0) + 0.7152 * ch(8) + 0.0722 * ch(16)
    }

    fn contrast(a: COLORREF, b: COLORREF) -> f64 {
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn dark_text_meets_wcag_aa() {
        let d = Palette::dark();
        for (name, fg, bg) in [
            ("text on window", d.text, d.window),
            ("text on face", d.text, d.face),
            ("text on input", d.text, d.input),
            ("text on hot", d.text, d.hot),
            ("muted on window", d.muted, d.window),
            ("muted on face", d.muted, d.face),
            ("ink on window", d.ink, d.window),
            ("grow on window", d.grow, d.window),
            ("shrink on window", d.shrink, d.window),
            ("accent on window", d.accent, d.window),
            ("text on highlight", d.text, d.highlight),
            ("on_weak on neutral", d.on_weak, d.neutral),
            ("white on grow_fill", d.on_strong, d.grow_fill),
            ("white on shrink_fill", d.on_strong, d.shrink_fill),
        ] {
            let c = contrast(fg, bg);
            assert!(c >= 4.5, "{name}: {c:.2}");
        }
        // Chart marks: 3:1 against the background (non-text contrast).
        for (i, l) in d.layers.iter().enumerate() {
            assert!(contrast(*l, d.window) >= 3.0, "layer {i}");
        }
        assert!(contrast(d.other, d.window) >= 3.0, "other");
        assert!(contrast(d.grow_fill, d.window) >= 3.0, "growth fill");
        assert!(contrast(d.shrink_fill, d.window) >= 3.0, "shrink fill");
        assert!(contrast(d.bar_dir, d.track) >= 3.0, "folder bars");
    }

    #[test]
    fn modes_round_trip() {
        for m in [Mode::System, Mode::Light, Mode::Dark] {
            assert_eq!(Mode::parse(m.as_str()), m);
        }
        assert_eq!(Mode::parse("anything"), Mode::System);
    }
}
