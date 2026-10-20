//! End-to-end GUI tests: the real rapidfind-gui.exe driven through UI
//! Automation, talking to a mock service (synthetic index) on a private pipe.
//!
//! These open windows and briefly take focus (some steps click or press
//! keys), so don't type while they run. Run with:
//! `cargo test -p rapidfind-gui --test ui -- --test-threads=1`

mod common;

use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rapidfind_core::engine::Engine;
use rapidfind_core::query::SearchFlags;
use rapidfind_core::search::{SearchOptions, SortKey};
use uiautomation::controls::ControlType;
use uiautomation::patterns::{UIExpandCollapsePattern, UIInvokePattern, UISelectionItemPattern, UIValuePattern};
use uiautomation::types::TreeScope;
use uiautomation::{UIAutomation, UIElement};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetClassNameW, GetWindowThreadProcessId, PostMessageW, WM_COMMAND};

// Menu command ids (crates/gui/src/app.rs).
const CMD_CASE: usize = 300;
const CMD_DIACRITICS: usize = 304;
const CMD_OPTIONS: usize = 500;

const ID_SEARCH: &str = "100";
const ID_FILTER: &str = "101";
const ID_LIST: &str = "102";

fn eventually<T>(what: &str, timeout: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let t = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(t.elapsed() < timeout, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Top-level window of `pid` with this class.
fn find_window(pid: u32, class: &str) -> Option<HWND> {
    struct Q {
        pid: u32,
        class: Vec<u16>,
        found: HWND,
    }
    unsafe extern "system" fn cb(h: HWND, l: isize) -> i32 {
        let q = unsafe { &mut *(l as *mut Q) };
        let mut p = 0;
        unsafe { GetWindowThreadProcessId(h, &mut p) };
        if p == q.pid {
            let mut buf = [0u16; 64];
            let n = unsafe { GetClassNameW(h, buf.as_mut_ptr(), 64) } as usize;
            if buf[..n] == q.class[..] {
                q.found = h;
                return 0;
            }
        }
        1
    }
    let mut q = Q { pid, class: class.encode_utf16().collect(), found: std::ptr::null_mut() };
    unsafe { EnumWindows(Some(cb), &mut q as *mut _ as isize) };
    (!q.found.is_null()).then_some(q.found)
}

struct Gui {
    child: Child,
    hwnd: HWND,
    ua: UIAutomation,
    win: UIElement,
    engine: Arc<Engine>,
}

impl Drop for Gui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Gui {
    fn launch(engine: Arc<Engine>) -> Gui {
        let pipe = format!(r"\\.\pipe\rapidfind-uitest-{}", std::process::id());
        common::serve(&pipe, engine.clone());
        let settings = std::env::temp_dir().join(format!("rapidfind-uitest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&settings);
        let child = Command::new(env!("CARGO_BIN_EXE_rapidfind-gui"))
            .arg("--test")
            .env("RAPIDFIND_PIPE", &pipe)
            .env("RAPIDFIND_GUI_DIR", &settings)
            .spawn()
            .expect("launching rapidfind-gui");
        let pid = child.id();
        let hwnd = eventually("main window", Duration::from_secs(15), || find_window(pid, "rapidfind.main"));
        let ua = UIAutomation::new().unwrap();
        let win = ua.element_from_handle(windows::Win32::Foundation::HWND(hwnd).into()).unwrap();
        Gui { child, hwnd, ua, win, engine }
    }

    fn by_id(&self, id: &str) -> UIElement {
        let id = id.to_string();
        self.ua
            .create_matcher()
            .from_ref(&self.win)
            .timeout(5000)
            .filter_fn(Box::new(move |e: &UIElement| Ok(e.get_automation_id().map(|a| a == id).unwrap_or(false))))
            .find_first()
            .unwrap_or_else(|e| panic!("element with automation id: {e}"))
    }

    fn set_query(&self, q: &str) {
        self.by_id(ID_SEARCH).get_pattern::<UIValuePattern>().unwrap().set_value(q).unwrap();
    }

    fn list_hwnd(&self) -> HWND {
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetDlgItem(self.hwnd, 102) }
    }

    fn rows(&self) -> i32 {
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW(self.list_hwnd(), 0x1004 /* LVM_GETITEMCOUNT */, 0, 0) as i32 }
    }

    /// Cell text through MSAA: the owner-data list view answers for any row
    /// (which makes the GUI fetch that block from the service). Column 0 is
    /// the item name; other columns come from the "Header: value, ..."
    /// description.
    fn cell(&self, row: i32, col: i32) -> String {
        use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_I4};
        use windows::Win32::UI::Accessibility::{AccessibleObjectFromWindow, IAccessible};
        use windows::core::Interface;
        let mut p = std::ptr::null_mut();
        let acc: IAccessible = unsafe {
            if AccessibleObjectFromWindow(windows::Win32::Foundation::HWND(self.list_hwnd()), 0xFFFF_FFFC /* OBJID_CLIENT */, &IAccessible::IID, &mut p).is_err() || p.is_null() {
                return String::new();
            }
            IAccessible::from_raw(p)
        };
        let child = VARIANT {
            Anonymous: VARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 { vt: VT_I4, wReserved1: 0, wReserved2: 0, wReserved3: 0, Anonymous: VARIANT_0_0_0 { lVal: row + 1 } }),
            },
        };
        if col == 0 {
            return unsafe { acc.get_accName(&child) }.map(|b| b.to_string()).unwrap_or_default();
        }
        let desc = unsafe { acc.get_accDescription(&child) }.map(|b| b.to_string()).unwrap_or_default();
        const HEADERS: [&str; 5] = ["Name", "Path", "Size", "Date Modified", "Type"];
        let key = format!("{}: ", HEADERS[col as usize]);
        let Some(start) = desc.find(&key).map(|i| i + key.len()) else { return String::new() };
        let rest = &desc[start..];
        let end = HEADERS[col as usize + 1..].iter().filter_map(|h| rest.find(&format!(", {h}: "))).min().unwrap_or(rest.len());
        rest[..end].to_string()
    }

    /// Clicks (a real mouse click) until the list shows the expected first
    /// row; a click can be swallowed while the window is being activated.
    fn click_until(&self, el: &UIElement, what: &str, n: usize, first: &str) {
        let pause = Duration::from_millis(u64::from(unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime() }) + 150);
        for _ in 0..4 {
            let _ = self.win.set_focus();
            el.click().unwrap();
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(2) {
                if self.rows() == n as i32 && self.cell(0, 0) == first {
                    std::thread::sleep(pause); // so the next click isn't a double-click
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            std::thread::sleep(pause);
        }
        self.wait_for(what, n, Some(first));
    }

    fn command(&self, id: usize) {
        unsafe { PostMessageW(self.hwnd, WM_COMMAND, id, 0) };
    }

    /// Expected total and first names for a query, straight from the engine.
    fn expect(&self, q: &str, flags: SearchFlags, sort: SortKey, desc: bool) -> (usize, Vec<String>) {
        let rs = self.engine.query(q, flags, &SearchOptions { sort, descending: desc, limit: 0 }).unwrap();
        let names = self.engine.with_indexes(|ixs| rs.first_page().iter().map(|h| ixs[h.vol as usize].name(h.idx).to_string()).collect());
        (rs.total, names)
    }

    /// Waits until the list shows `n` rows whose first cell is `first`.
    fn wait_for(&self, what: &str, n: usize, first: Option<&str>) {
        let t = Instant::now();
        loop {
            let (rows, c0) = (self.rows(), self.cell(0, 0));
            if rows == n as i32 && first.is_none_or(|f| c0 == f) {
                return;
            }
            if t.elapsed() > Duration::from_secs(8) {
                panic!("timed out waiting for {what}: want {n} rows starting {first:?}, have {rows} rows starting {c0:?} (title {:?})", self.win.get_name());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn gui_end_to_end() {
    let engine = Arc::new(Engine::from_indexes(vec![common::sample_index('C', 5_000)]));
    let g = Gui::launch(engine.clone());
    let none = SearchFlags::default();

    // 1. Empty query lists everything, sorted by name.
    let (total, names) = g.expect("", none, SortKey::Name, false);
    g.wait_for("all entries", total, Some(&names[0]));
    assert_eq!(g.cell(1, 0), names[1]);

    // 2. Typing filters; Name/Path/Size/Type columns are filled.
    g.set_query("report-2026");
    g.wait_for("report-2026 row", 1, Some("report-2026.pdf"));
    assert_eq!(g.cell(0, 1), r"C:\Users\me\Documents");
    assert!(g.cell(0, 2).ends_with("KB"), "size column: {}", g.cell(0, 2));
    assert!(!g.cell(0, 4).is_empty(), "type column from the shell");

    // 3. Header click sorts by size (descending first), again flips.
    g.set_query("file0000*");
    let (n, _) = g.expect("file0000*", none, SortKey::Name, false);
    g.wait_for("wildcard rows", n, None);
    let header = g
        .ua
        .create_matcher()
        .from_ref(&g.win)
        .control_type(ControlType::HeaderItem)
        .name("Size")
        .timeout(5000)
        .find_first()
        .expect("Size header");
    let (_, by_size) = g.expect("file0000*", none, SortKey::Size, true);
    g.click_until(&header, "sorted by size, descending", n, &by_size[0]);
    assert_eq!(g.cell(1, 0), by_size[1]);
    let (_, asc) = g.expect("file0000*", none, SortKey::Size, false);
    g.click_until(&header, "sorted by size, ascending", n, &asc[0]);

    // 4. Deep paging: the last row of a large result set (fetched on demand).
    g.set_query("");
    let (total, _) = g.expect("", none, SortKey::Size, false);
    g.wait_for("all entries again", total, None);
    let full = engine.query("", none, &SearchOptions { sort: SortKey::Size, descending: false, limit: 0 }).unwrap();
    let last = engine.with_indexes(|ixs| {
        let h = full.first_page()[total - 1];
        ixs[h.vol as usize].name(h.idx).to_string()
    });
    eventually("last row text", Duration::from_secs(8), || (g.cell(total as i32 - 1, 0) == last).then_some(()));

    // 5. Filter dropdown: Picture.
    let combo = g.by_id(ID_FILTER);
    combo.get_pattern::<UIExpandCollapsePattern>().unwrap().expand().unwrap();
    let item = g
        .ua
        .create_matcher()
        .control_type(ControlType::ListItem)
        .name("Picture")
        .timeout(5000)
        .find_first()
        .expect("Picture in the filter list");
    let _ = item.get_pattern::<UISelectionItemPattern>().map(|p| p.select());
    item.click().unwrap();
    let (pics, _) = g.expect("pic:", none, SortKey::Size, false);
    g.wait_for("picture filter", pics, None);
    combo.get_pattern::<UIExpandCollapsePattern>().unwrap().expand().unwrap();
    let everything = g.ua.create_matcher().control_type(ControlType::ListItem).name("Everything").timeout(5000).find_first().unwrap();
    everything.click().unwrap();

    // 6. Search menu toggles: Match Case.
    g.set_query("readme.md");
    g.wait_for("case-insensitive by default", 1, Some("README.md"));
    g.command(CMD_CASE);
    g.wait_for("case-sensitive: no match", 0, None);
    g.set_query("README.md");
    g.wait_for("case-sensitive: exact", 1, Some("README.md"));
    g.command(CMD_CASE);

    // 7. Match Diacritics off: "Angstrom" finds "Ångström.png".
    g.set_query("Angstrom");
    g.wait_for("diacritics matter by default", 0, None);
    g.command(CMD_DIACRITICS);
    g.wait_for("diacritics ignored", 1, Some("Ångström.png"));
    assert!(!engine.config().match_diacritics);
    g.command(CMD_DIACRITICS);

    // 8. Live updates: a file added to the index appears without re-typing.
    g.set_query("zz-live-test");
    g.wait_for("nothing yet", 0, None);
    {
        let v = &engine.volumes()[0];
        let mut ix = v.index.write();
        let (root, rec) = (ix.root, ix.rec_map.len() as u32);
        ix.add_link(rec, 0, root, "zz-live-test.txt", 0x20, 1, common::BASE_TIME, common::BASE_TIME);
    }
    g.wait_for("live file shows up", 1, Some("zz-live-test.txt"));

    // 9. Context menu → Copy Full Path.
    g.set_query("notes.txt");
    g.wait_for("notes row", 1, Some("notes.txt"));
    let list = g.by_id(ID_LIST);
    let row = g.ua.create_matcher().from_ref(&list).control_type(ControlType::ListItem).timeout(5000).find_first().expect("list item");
    row.get_pattern::<UISelectionItemPattern>().unwrap().select().unwrap();
    row.right_click().unwrap();
    let copy = g
        .ua
        .create_matcher()
        .control_type(ControlType::MenuItem)
        .name("Copy Full Path")
        .timeout(5000)
        .find_first()
        .expect("context menu item");
    copy.click().unwrap();
    let clip = eventually("clipboard", Duration::from_secs(5), || {
        uiautomation::clipboards::Clipboard::open().ok().and_then(|c| c.get_text().ok()).filter(|t| t.contains("notes.txt"))
    });
    assert_eq!(clip, r"C:\Users\me\Documents\notes.txt");

    // 10. Options: add an exclude pattern; the service applies it.
    g.command(CMD_OPTIONS);
    let pid = g.child.id();
    let opt_hwnd = eventually("options window", Duration::from_secs(5), || find_window(pid, "rapidfind.options"));
    let opt = g.ua.element_from_handle(windows::Win32::Foundation::HWND(opt_hwnd).into()).unwrap();
    let tab = g.ua.create_matcher().from_ref(&opt).control_type(ControlType::TabItem).name("Exclude").timeout(5000).find_first().unwrap();
    tab.click().unwrap();
    let find = |id: &str| {
        let id = id.to_string();
        g.ua.create_matcher()
            .from_ref(&opt)
            .timeout(5000)
            .filter_fn(Box::new(move |e: &UIElement| Ok(e.get_automation_id().map(|a| a == id).unwrap_or(false))))
            .find_first()
            .unwrap()
    };
    find("944").get_pattern::<UIValuePattern>().unwrap().set_value("Downloads").unwrap();
    find("945").get_pattern::<UIInvokePattern>().unwrap().invoke().unwrap(); // Add Pattern
    find("1").get_pattern::<UIInvokePattern>().unwrap().invoke().unwrap(); // OK
    eventually("config applied", Duration::from_secs(5), || engine.config().exclude_folders.contains(&"Downloads".to_string()).then_some(()));
    g.set_query("setup.exe");
    g.wait_for("excluded folder hidden", 0, None);

    // 11. Esc clears the search box.
    g.set_query("something");
    let edit = g.by_id(ID_SEARCH);
    edit.set_focus().unwrap();
    edit.send_keys("{Esc}", 10).unwrap();
    eventually("search cleared", Duration::from_secs(5), || {
        edit.get_pattern::<UIValuePattern>().unwrap().get_value().unwrap().is_empty().then_some(())
    });

    let _ = TreeScope::Descendants;
}
