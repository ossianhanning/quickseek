//! End-to-end GUI tests: the real quickseek-gui.exe driven through UI
//! Automation, talking to a mock service (synthetic index) on a private pipe.
//!
//! These open windows and briefly take focus (some steps click or press
//! keys), so don't type while they run. Run with:
//! `cargo test -p quickseek-gui --test ui -- --test-threads=1`

mod common;

use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use quickseek_core::engine::Engine;
use quickseek_core::query::SearchFlags;
use quickseek_core::search::{SearchOptions, SortKey};
use uiautomation::controls::ControlType;
use uiautomation::patterns::{
    UIExpandCollapsePattern, UIInvokePattern, UISelectionItemPattern, UIValuePattern,
};
use uiautomation::types::TreeScope;
use uiautomation::{UIAutomation, UIElement};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowThreadProcessId, PostMessageW, WM_COMMAND,
};

// Menu command ids (crates/gui/src/app.rs).
const CMD_CASE: usize = 300;
const CMD_DIACRITICS: usize = 304;
const CMD_OPTIONS: usize = 500;
const CMD_TREE: usize = 403;
const CMD_EXPAND: usize = 210;
const CMD_COLLAPSE: usize = 211;
const CMD_STOP: usize = 305;
const CMD_PREVIEW: usize = 404;
const CMD_COMPARE_DAY: usize = 410;
const CMD_COMPARE_MONTH: usize = 412;
const CMD_STOP_COMPARE: usize = 414;

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
    let mut q = Q {
        pid,
        class: class.encode_utf16().collect(),
        found: std::ptr::null_mut(),
    };
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
        // One pipe per launch: an earlier test's mock service keeps running.
        static LAUNCHES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = LAUNCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let pipe = format!(r"\\.\pipe\quickseek-uitest-{}-{n}", std::process::id());
        common::serve(&pipe, engine.clone());
        Self::launch_on(&pipe, engine)
    }

    /// The GUI talking to whatever serves `pipe`.
    fn launch_on(pipe: &str, engine: Arc<Engine>) -> Gui {
        let settings =
            std::env::temp_dir().join(format!("quickseek-uitest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&settings);
        let child = Command::new(env!("CARGO_BIN_EXE_quickseek-gui"))
            .arg("--test")
            .env("QUICKSEEK_PIPE", pipe)
            .env("QUICKSEEK_GUI_DIR", &settings)
            .spawn()
            .expect("launching quickseek-gui");
        let pid = child.id();
        let hwnd = eventually("main window", Duration::from_secs(15), || {
            find_window(pid, "quickseek.main")
        });
        let ua = UIAutomation::new().unwrap();
        let win = ua
            .element_from_handle(windows::Win32::Foundation::HWND(hwnd).into())
            .unwrap();
        Gui {
            child,
            hwnd,
            ua,
            win,
            engine,
        }
    }

    fn by_id(&self, id: &str) -> UIElement {
        let id = id.to_string();
        self.ua
            .create_matcher()
            .from_ref(&self.win)
            .timeout(5000)
            .filter_fn(Box::new(move |e: &UIElement| {
                Ok(e.get_automation_id().map(|a| a == id).unwrap_or(false))
            }))
            .find_first()
            .unwrap_or_else(|e| panic!("element with automation id: {e}"))
    }

    fn set_query(&self, q: &str) {
        self.by_id(ID_SEARCH)
            .get_pattern::<UIValuePattern>()
            .unwrap()
            .set_value(q)
            .unwrap();
    }

    fn list_hwnd(&self) -> HWND {
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetDlgItem(self.hwnd, 102) }
    }

    fn tree_hwnd(&self) -> HWND {
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetDlgItem(self.hwnd, 104) }
    }

    fn rows(&self) -> i32 {
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW(
                self.list_hwnd(),
                0x1004, /* LVM_GETITEMCOUNT */
                0,
                0,
            ) as i32
        }
    }

    fn tree_rows(&self) -> i32 {
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::SendMessageW(
                self.tree_hwnd(),
                0x1004,
                0,
                0,
            ) as i32
        }
    }

    /// Selects tree row `i` (LVM_SETITEMSTATE across processes needs no
    /// pointer: we post a keyboard-free selection via UIA instead).
    fn select_tree_row(&self, name: &str) {
        let tree = self.by_id("104");
        let n = name.to_string();
        let row = self
            .ua
            .create_matcher()
            .from_ref(&tree)
            .control_type(ControlType::ListItem)
            .filter_fn(Box::new(move |e: &UIElement| {
                Ok(e.get_name().map(|x| x == n).unwrap_or(false))
            }))
            .timeout(5000)
            .find_first()
            .unwrap_or_else(|e| panic!("tree row {name}: {e}"));
        row.get_pattern::<UISelectionItemPattern>()
            .unwrap()
            .select()
            .unwrap();
    }

    /// Tree cell text (columns: Name, Size, % of Parent, Files, Online-only,
    /// Date Modified, Change, Trend).
    fn tree_cell(&self, row: i32, col: i32) -> String {
        const HEADERS: [&str; 8] = [
            "Name",
            "Size",
            "% of Parent",
            "Files",
            "Online-only",
            "Date Modified",
            "Change",
            "Trend",
        ];
        msaa_cell(self.tree_hwnd(), row, col, &HEADERS)
    }

    /// Selects the result row named `name`.
    fn select_row(&self, name: &str) {
        let list = self.by_id(ID_LIST);
        let n = name.to_string();
        let row = self
            .ua
            .create_matcher()
            .from_ref(&list)
            .control_type(ControlType::ListItem)
            .filter_fn(Box::new(move |e: &UIElement| {
                Ok(e.get_name().map(|x| x == n).unwrap_or(false))
            }))
            .timeout(5000)
            .find_first()
            .unwrap_or_else(|e| panic!("list row {name}: {e}"));
        row.get_pattern::<UISelectionItemPattern>()
            .unwrap()
            .select()
            .unwrap();
    }

    /// The status bar's left part.
    fn status(&self) -> String {
        // By handle: searching from the window walks every list row first.
        let h = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetDlgItem(self.hwnd, 103) };
        let bar = self
            .ua
            .element_from_handle(windows::Win32::Foundation::HWND(h).into())
            .expect("status bar");
        let parts = self
            .ua
            .create_matcher()
            .from_ref(&bar)
            .control_type(ControlType::Text)
            .timeout(1000)
            .find_all()
            .unwrap_or_default();
        parts
            .first()
            .and_then(|p| p.get_name().ok())
            .unwrap_or_default()
    }

    /// What the preview pane says it shows (its window text).
    fn preview_state(&self) -> String {
        self.by_id("106").get_name().unwrap_or_default()
    }

    fn tree_names(&self) -> Vec<String> {
        (0..self.tree_rows())
            .map(|i| self.tree_cell(i, 0))
            .collect()
    }

    /// Cell text through MSAA: the owner-data list view answers for any row
    /// (which makes the GUI fetch that block from the service). Column 0 is
    /// the item name; other columns come from the "Header: value, ..."
    /// description.
    fn cell(&self, row: i32, col: i32) -> String {
        const HEADERS: [&str; 5] = ["Name", "Path", "Size", "Date Modified", "Type"];
        msaa_cell(self.list_hwnd(), row, col, &HEADERS)
    }
}

/// A list view cell through MSAA (name for column 0, else parsed from the
/// "Header: value, ..." description).
fn msaa_cell(list: HWND, row: i32, col: i32, headers: &[&str]) -> String {
    {
        use windows::Win32::System::Variant::{
            VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_I4,
        };
        use windows::Win32::UI::Accessibility::{AccessibleObjectFromWindow, IAccessible};
        use windows::core::Interface;
        let mut p = std::ptr::null_mut();
        let acc: IAccessible = unsafe {
            if AccessibleObjectFromWindow(
                windows::Win32::Foundation::HWND(list),
                0xFFFF_FFFC, /* OBJID_CLIENT */
                &IAccessible::IID,
                &mut p,
            )
            .is_err()
                || p.is_null()
            {
                return String::new();
            }
            IAccessible::from_raw(p)
        };
        let child = VARIANT {
            Anonymous: VARIANT_0 {
                Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                    vt: VT_I4,
                    wReserved1: 0,
                    wReserved2: 0,
                    wReserved3: 0,
                    Anonymous: VARIANT_0_0_0 { lVal: row + 1 },
                }),
            },
        };
        if col == 0 {
            return unsafe { acc.get_accName(&child) }
                .map(|b| b.to_string())
                .unwrap_or_default();
        }
        let desc = unsafe { acc.get_accDescription(&child) }
            .map(|b| b.to_string())
            .unwrap_or_default();
        let key = format!("{}: ", headers[col as usize]);
        let Some(start) = desc.find(&key).map(|i| i + key.len()) else {
            return String::new();
        };
        let rest = &desc[start..];
        // Columns can be shown in another order than their index: any header ends the value.
        let end = headers
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != col as usize)
            .map(|(_, h)| h)
            .filter_map(|h| rest.find(&format!(", {h}: ")))
            .min()
            .unwrap_or(rest.len());
        rest[..end].to_string()
    }
}

impl Gui {
    /// Clicks (a real mouse click) until the list shows the expected first
    /// row; a click can be swallowed while the window is being activated.
    fn click_until(&self, el: &UIElement, what: &str, n: usize, first: &str) {
        let pause = Duration::from_millis(
            u64::from(unsafe {
                windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime()
            }) + 150,
        );
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
    fn expect(
        &self,
        q: &str,
        flags: SearchFlags,
        sort: SortKey,
        desc: bool,
    ) -> (usize, Vec<String>) {
        let rs = self
            .engine
            .query(
                q,
                flags,
                &SearchOptions {
                    sort,
                    descending: desc,
                    limit: 0,
                },
            )
            .unwrap();
        let names = self.engine.with_indexes(|ixs| {
            rs.first_page()
                .iter()
                .map(|h| ixs[h.vol as usize].name(h.idx).to_string())
                .collect()
        });
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
                panic!(
                    "timed out waiting for {what}: want {n} rows starting {first:?}, have {rows} rows starting {c0:?} (title {:?})",
                    self.win.get_name()
                );
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
    assert!(
        g.cell(0, 2).ends_with("KB"),
        "size column: {}",
        g.cell(0, 2)
    );
    assert!(!g.cell(0, 4).is_empty(), "type column from the shell");

    // 3. Header click sorts by size (descending first), again flips.
    g.set_query("file0000*");
    let (n, _) = g.expect("file0000*", none, SortKey::Name, false);
    g.wait_for("wildcard rows", n, None);
    let header =
        g.ua.create_matcher()
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
    let full = engine
        .query(
            "",
            none,
            &SearchOptions {
                sort: SortKey::Size,
                descending: false,
                limit: 0,
            },
        )
        .unwrap();
    let last = engine.with_indexes(|ixs| {
        let h = full.first_page()[total - 1];
        ixs[h.vol as usize].name(h.idx).to_string()
    });
    eventually("last row text", Duration::from_secs(8), || {
        (g.cell(total as i32 - 1, 0) == last).then_some(())
    });

    // 5. Filter dropdown: Picture.
    let combo = g.by_id(ID_FILTER);
    combo
        .get_pattern::<UIExpandCollapsePattern>()
        .unwrap()
        .expand()
        .unwrap();
    let item =
        g.ua.create_matcher()
            .control_type(ControlType::ListItem)
            .name("Picture")
            .timeout(5000)
            .find_first()
            .expect("Picture in the filter list");
    let _ = item
        .get_pattern::<UISelectionItemPattern>()
        .map(|p| p.select());
    item.click().unwrap();
    let (pics, _) = g.expect("pic:", none, SortKey::Size, false);
    g.wait_for("picture filter", pics, None);
    combo
        .get_pattern::<UIExpandCollapsePattern>()
        .unwrap()
        .expand()
        .unwrap();
    let everything =
        g.ua.create_matcher()
            .control_type(ControlType::ListItem)
            .name("Everything")
            .timeout(5000)
            .find_first()
            .unwrap();
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
        let (root, rec) = (ix.root, ix.rec_count);
        ix.add_link(
            rec,
            0,
            root,
            "zz-live-test.txt",
            0x20,
            1,
            common::BASE_TIME,
            common::BASE_TIME,
        );
    }
    g.wait_for("live file shows up", 1, Some("zz-live-test.txt"));

    // 9. Context menu → Copy Full Path.
    g.set_query("notes.txt");
    g.wait_for("notes row", 1, Some("notes.txt"));
    let list = g.by_id(ID_LIST);
    let row =
        g.ua.create_matcher()
            .from_ref(&list)
            .control_type(ControlType::ListItem)
            .timeout(5000)
            .find_first()
            .expect("list item");
    row.get_pattern::<UISelectionItemPattern>()
        .unwrap()
        .select()
        .unwrap();
    row.right_click().unwrap();
    let copy =
        g.ua.create_matcher()
            .control_type(ControlType::MenuItem)
            .name("Copy Full Path")
            .timeout(5000)
            .find_first()
            .expect("context menu item");
    copy.click().unwrap();
    let clip = eventually("clipboard", Duration::from_secs(5), || {
        uiautomation::clipboards::Clipboard::open()
            .ok()
            .and_then(|c| c.get_text().ok())
            .filter(|t| t.contains("notes.txt"))
    });
    assert_eq!(clip, r"C:\Users\me\Documents\notes.txt");

    // 10. Options: add an exclude pattern; the service applies it.
    g.command(CMD_OPTIONS);
    let pid = g.child.id();
    let opt_hwnd = eventually("options window", Duration::from_secs(5), || {
        find_window(pid, "quickseek.options")
    });
    let opt =
        g.ua.element_from_handle(windows::Win32::Foundation::HWND(opt_hwnd).into())
            .unwrap();
    let tab =
        g.ua.create_matcher()
            .from_ref(&opt)
            .control_type(ControlType::TabItem)
            .name("Exclude")
            .timeout(5000)
            .find_first()
            .unwrap();
    tab.click().unwrap();
    let find = |id: &str| {
        let id = id.to_string();
        g.ua.create_matcher()
            .from_ref(&opt)
            .timeout(5000)
            .filter_fn(Box::new(move |e: &UIElement| {
                Ok(e.get_automation_id().map(|a| a == id).unwrap_or(false))
            }))
            .find_first()
            .unwrap()
    };
    find("944")
        .get_pattern::<UIValuePattern>()
        .unwrap()
        .set_value("Downloads")
        .unwrap();
    find("945")
        .get_pattern::<UIInvokePattern>()
        .unwrap()
        .invoke()
        .unwrap(); // Add Pattern
    find("1")
        .get_pattern::<UIInvokePattern>()
        .unwrap()
        .invoke()
        .unwrap(); // OK
    eventually("config applied", Duration::from_secs(5), || {
        engine
            .config()
            .exclude_folders
            .contains(&"Downloads".to_string())
            .then_some(())
    });
    g.set_query("setup.exe");
    g.wait_for("excluded folder hidden", 0, None);

    // 11. Esc clears the search box.
    g.set_query("something");
    let edit = g.by_id(ID_SEARCH);
    edit.set_focus().unwrap();
    edit.send_keys("{Esc}", 10).unwrap();
    eventually("search cleared", Duration::from_secs(5), || {
        edit.get_pattern::<UIValuePattern>()
            .unwrap()
            .get_value()
            .unwrap()
            .is_empty()
            .then_some(())
    });

    // 12. Folder size tree: volumes open one level; a folder holding a
    // single folder opens through to it.
    g.set_query("");
    g.command(CMD_TREE);
    use quickseek_core::tree::{NodeRef, Tree, TreeSort};
    let mut t = Tree::build(&engine, "", none).unwrap();
    let at = |t: &mut Tree, p: &str| {
        t.children(
            &engine,
            &NodeRef {
                path: p.into(),
                ..Default::default()
            },
            TreeSort::Size,
            true,
            0,
            100,
        )
        .unwrap()
    };
    let c = at(&mut t, "C:");
    let me = at(&mut t, r"C:\Users\me");
    let short = || {
        g.tree_names()
            .iter()
            .map(|n| n.split("   ").next().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    let wait_rows = |what: &str, want: &[String]| {
        let t0 = Instant::now();
        while short() != want {
            assert!(
                t0.elapsed() < Duration::from_secs(8),
                "{what}: want {want:?}, have {:?}",
                g.tree_names()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let mut want: Vec<String> = vec!["C:".into()];
    want.extend(c.rows.iter().map(|r| r.name.clone()));
    wait_rows("volume opened", &want);
    assert!(
        g.tree_cell(0, 0).contains("free of") || g.tree_cell(0, 0) == "C:",
        "root label: {}",
        g.tree_cell(0, 0)
    );
    g.select_tree_row("Users");
    g.command(CMD_EXPAND);
    let users = want.iter().position(|n| n == "Users").unwrap();
    want.splice(
        users + 1..users + 1,
        std::iter::once("me".to_string()).chain(me.rows.iter().map(|r| r.name.clone())),
    );
    wait_rows("Users drilled to me", &want);
    assert_eq!(g.tree_cell(users as i32 + 1, 2), "100.0 %"); // me is all of Users
    assert_eq!(
        g.tree_cell(users as i32 + 2, 3),
        crate_group(me.rows[0].files)
    );

    // 13. Filtered: only *.mkv counts, and the only path opens down to the movie.
    g.set_query("*.mkv");
    let mkv: Vec<String> = ["C:", "Users", "me", "Videos", "movie.mkv"]
        .map(String::from)
        .to_vec();
    wait_rows("filtered tree", &mkv);
    assert_eq!(g.tree_cell(3, 1), "3.7 GB");
    assert_eq!(g.tree_cell(4, 2), "100.0 %");
    g.select_tree_row("Videos");
    g.command(CMD_COLLAPSE);
    wait_rows("collapsed", &mkv[..4]);
    g.command(CMD_EXPAND);
    wait_rows("expanded again", &mkv);

    // 14. Back to the list.
    g.command(CMD_TREE);
    g.wait_for("list again", 1, Some("movie.mkv"));

    let _ = TreeScope::Descendants;
}

/// Comparing with history: yesterday's growth opens down to the folder that
/// grew, with the change in its own column; a month back shows the cleanup.
#[test]
fn gui_growth_compare() {
    let engine = Arc::new(Engine::from_indexes(vec![common::sample_index('C', 2_000)]));
    let hist =
        std::env::temp_dir().join(format!("quickseek-uitest-history-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&hist);
    engine.enable_history(hist.clone());
    common::seed_history(&engine);
    let g = Gui::launch(engine);
    g.command(CMD_TREE);
    eventually("tree", Duration::from_secs(8), || {
        (g.tree_rows() > 1).then_some(())
    });
    g.command(CMD_COMPARE_DAY);
    let row = |name: &str| {
        g.tree_names()
            .iter()
            .position(|n| n.split("   ").next() == Some(name))
            .map(|i| i as i32)
    };
    let videos = eventually("drilled to Videos", Duration::from_secs(8), || {
        row("Videos").filter(|&i| g.tree_cell(i, 6).starts_with('+'))
    });
    let change = g.tree_cell(videos, 6);
    assert!(
        change.ends_with("MB"),
        "Videos grew about 117 MB since yesterday: {change}"
    );
    assert_eq!(g.tree_cell(0, 6), change, "all of C:'s growth is in Videos");
    assert!(g.status().contains("Compared with"), "{}", g.status());
    // A month back: Downloads shrank by 2e9 bytes in the cleanup.
    g.command(CMD_COMPARE_MONTH);
    eventually("Downloads shrank", Duration::from_secs(8), || {
        row("Downloads").filter(|&i| g.tree_cell(i, 6) == "\u{2212}1.9 GB")
    });
    g.command(CMD_STOP_COMPARE);
    eventually("comparing stopped", Duration::from_secs(8), || {
        (!g.status().contains("Compared with")).then_some(())
    });
    let _ = std::fs::remove_dir_all(&hist);
}

/// Deleting from the tree: Del on a folder of thousands of files deletes it
/// in the background (the window keeps answering), the tree takes its size
/// off at once, and when the index notices (here: the test removes it) the
/// live tree drops it without a refresh.
#[test]
fn gui_delete_in_background() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_KEYDOWN, WM_NULL,
    };
    let dir = std::env::temp_dir().join(format!("quickseek-uitest-delete-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in 0..30 {
        let d = dir.join("big").join(format!("part{sub:02}"));
        std::fs::create_dir_all(&d).unwrap();
        for f in 0..100 {
            std::fs::write(d.join(format!("f{f:03}.bin")), [7u8; 2048]).unwrap();
        }
    }
    std::fs::create_dir_all(dir.join("keep")).unwrap();
    std::fs::write(dir.join("keep").join("small.txt"), "stays").unwrap();
    let root = dir.to_string_lossy().to_string();
    let engine = Arc::new(Engine::from_indexes(vec![quickseek_core::folder::build(
        &root,
        quickseek_core::fold::FoldMode::Lower,
    )]));
    let g = Gui::launch(engine.clone());
    g.command(CMD_TREE);
    let has = |name: &str| {
        g.tree_names()
            .iter()
            .any(|n| n.split("   ").next() == Some(name))
    };
    eventually("tree shows big and keep", Duration::from_secs(8), || {
        (has("big") && has("keep")).then_some(())
    });
    g.select_tree_row("big");
    unsafe {
        PostMessageW(g.tree_hwnd(), WM_KEYDOWN, 0x2E /* VK_DELETE */, 0)
    };
    // While the files go, the window answers right away.
    let t0 = Instant::now();
    let mut slowest = Duration::ZERO;
    let mut saw_marked = false;
    while dir.join("big").exists() {
        let t = Instant::now();
        let mut out = 0usize;
        let ok =
            unsafe { SendMessageTimeoutW(g.hwnd, WM_NULL, 0, 0, SMTO_ABORTIFHUNG, 2000, &mut out) };
        assert!(ok != 0, "the window stopped answering while deleting");
        slowest = slowest.max(t.elapsed());
        saw_marked |= g.tree_names().iter().any(|n| n.contains("deleting"));
        assert!(t0.elapsed() < Duration::from_secs(60), "big wasn't deleted");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        slowest < Duration::from_millis(500),
        "the window took {slowest:?} to answer while deleting"
    );
    let t1 = Instant::now();
    while !g.status().contains("Deleted \"big\"") {
        assert!(
            t1.elapsed() < Duration::from_secs(8),
            "status: {:?}",
            g.status()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Usually seen, but a fast disk may finish first.
    eprintln!(
        "deleted 3,000 files in {:?}; slowest answer {slowest:?}; saw the greyed row: {saw_marked}",
        t0.elapsed()
    );
    // The index notices (a watcher would; here the test removes it): the live tree follows.
    let big = format!("{}\\big", root.trim_end_matches('\\'));
    {
        let (v, i) = engine.lookup_path(&big).expect("big in the index");
        engine.volumes()[v].index.write().remove_subtree(i);
    }
    eventually("live tree dropped big", Duration::from_secs(10), || {
        (!has("big") && has("keep")).then_some(())
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// Content search over a real folder: name matches come first, rows whose
/// files don't contain the text drop out as they are read, and the preview
/// pane shows the text with the match highlighted (and pictures as thumbnails).
#[test]
fn gui_content_search_and_preview() {
    let dir = std::env::temp_dir().join(format!("quickseek-uitest-content-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(
        dir.join("alpha.txt"),
        "first line\r\nhere is the needle\r\n",
    )
    .unwrap();
    let mut w16 = vec![0xFF, 0xFE];
    w16.extend(
        "A Needle in UTF-16"
            .encode_utf16()
            .flat_map(u16::to_le_bytes),
    );
    std::fs::write(dir.join("wide.txt"), w16).unwrap();
    std::fs::write(dir.join("sub").join("deep.log"), "deep NEEDLE").unwrap();
    std::fs::write(dir.join("blob.bin"), b"\x00\x01needle\x00\x02").unwrap(); // binary: skipped
    for i in 0..1500 {
        std::fs::write(dir.join(format!("hay{i:04}.txt")), "just hay").unwrap();
    }
    // A 2x2 24-bit BMP.
    let mut bmp = b"BM".to_vec();
    let px = [0u8, 0, 255, 0, 255, 0, 0, 0, 255, 0, 255, 0, 0, 0, 255, 0];
    let size = 54 + px.len() as u32;
    bmp.extend(size.to_le_bytes());
    bmp.extend([0u8; 4]);
    bmp.extend(54u32.to_le_bytes());
    bmp.extend(40u32.to_le_bytes());
    bmp.extend(2i32.to_le_bytes());
    bmp.extend(2i32.to_le_bytes());
    bmp.extend(1u16.to_le_bytes());
    bmp.extend(24u16.to_le_bytes());
    bmp.extend([0u8; 24]);
    bmp.extend(px);
    std::fs::write(dir.join("pic.bmp"), bmp).unwrap();

    let root = dir.to_string_lossy().to_string();
    let folder = quickseek_core::folder::build(&root, quickseek_core::fold::FoldMode::Lower);
    let engine = Arc::new(Engine::from_indexes(vec![
        common::sample_index('C', 2_000),
        folder,
    ]));
    let g = Gui::launch(engine.clone());
    let inside = format!("\"{}\\\"", root.trim_end_matches('\\'));

    // 1. Name matches first, then only files containing "needle" (any case,
    // UTF-8 and UTF-16), sorted by name; folders and binaries drop out.
    g.set_query(&format!("{inside} content:needle"));
    g.wait_for("content matches", 3, Some("alpha.txt"));
    eventually("all three rows", Duration::from_secs(5), || {
        (g.cell(1, 0) == "deep.log" && g.cell(2, 0) == "wide.txt").then_some(())
    });
    let st = eventually("finished status", Duration::from_secs(8), || {
        Some(g.status()).filter(|s| s.contains("files read"))
    });
    assert!(
        st.starts_with("3 objects") && st.contains("skipped 1 binary"),
        "status: {st}"
    );

    // 2. Case-sensitive and negated terms.
    g.set_query(&format!("{inside} case:content:NEEDLE"));
    g.wait_for("case-sensitive content", 1, Some("deep.log"));
    g.set_query(&format!("{inside} *.txt !content:hay"));
    g.wait_for("files without hay", 2, Some("alpha.txt"));

    // 3. Errors in the query show in the status bar.
    g.set_query("content:a | b");
    eventually("error shown", Duration::from_secs(5), || {
        g.status()
            .contains("only be combined with AND")
            .then_some(())
    });

    // 4. Stopping a scan (Esc in the search box): it may already be done.
    g.set_query("content:zzz-not-there");
    g.command(CMD_STOP);
    let st = eventually("stopped or finished", Duration::from_secs(8), || {
        Some(g.status()).filter(|s| s.contains("files read"))
    });
    assert!(
        st.contains("stopped") || st.starts_with("0 objects"),
        "status: {st}"
    );

    // 5. Preview pane: text with the match highlighted, scrolled to it.
    g.set_query(&format!("{inside} content:needle"));
    g.wait_for("content matches again", 3, Some("alpha.txt"));
    g.command(CMD_PREVIEW);
    g.select_row("alpha.txt");
    eventually("text preview", Duration::from_secs(5), || {
        (g.preview_state() == "text · Utf8 · 1 matches").then_some(())
    });
    g.select_row("wide.txt");
    eventually("UTF-16 preview", Duration::from_secs(5), || {
        (g.preview_state() == "text · Utf16Le · 1 matches").then_some(())
    });
    let text = g.by_id("107");
    let body = text
        .get_pattern::<uiautomation::patterns::UITextPattern>()
        .and_then(|t| t.get_document_range())
        .and_then(|r| r.get_text(-1))
        .unwrap_or_default();
    assert!(
        body.contains("A Needle in UTF-16"),
        "preview text: {body:?}"
    );

    // 6. Pictures as shell thumbnails.
    g.set_query(&format!("{inside} pic.bmp"));
    g.wait_for("picture", 1, Some("pic.bmp"));
    g.select_row("pic.bmp");
    eventually("thumbnail", Duration::from_secs(8), || {
        (g.preview_state() == "image").then_some(())
    });
    g.command(CMD_PREVIEW);

    drop(g);
    let _ = std::fs::remove_dir_all(&dir);
}

fn crate_group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A window's pixels as `PrintWindow` renders them (frame included), as
/// 0x00RRGGBB, with its size.
fn capture(h: HWND) -> (Vec<u32>, i32, i32) {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::*;
    use windows_sys::Win32::Storage::Xps::PrintWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;
    unsafe {
        let mut r: RECT = std::mem::zeroed();
        GetWindowRect(h, &mut r);
        let (w, ht) = (r.right - r.left, r.bottom - r.top);
        let screen = GetDC(std::ptr::null_mut());
        let mem = CreateCompatibleDC(screen);
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -ht; // top-down
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(mem, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        let old = SelectObject(mem, dib as _);
        PrintWindow(h, mem, 2 /* PW_RENDERFULLCONTENT */);
        let px = std::slice::from_raw_parts(bits as *const u32, (w * ht) as usize)
            .iter()
            .map(|p| p & 0x00FF_FFFF)
            .collect();
        SelectObject(mem, old);
        DeleteObject(dib as _);
        DeleteDC(mem);
        ReleaseDC(std::ptr::null_mut(), screen);
        (px, w, ht)
    }
}

/// Median luminance (0-255) of a region of a capture.
fn median_luma(img: &(Vec<u32>, i32, i32), x0: i32, y0: i32, x1: i32, y1: i32) -> u8 {
    let (px, w, h) = img;
    let mut v: Vec<u8> = Vec::new();
    for y in y0.max(0)..y1.min(*h) {
        for x in x0.max(0)..x1.min(*w) {
            let p = px[(y * w + x) as usize];
            let (r, g, b) = ((p >> 16) & 0xFF, (p >> 8) & 0xFF, p & 0xFF);
            v.push(((2126 * r + 7152 * g + 722 * b) / 10_000) as u8);
        }
    }
    v.sort_unstable();
    v.get(v.len() / 2).copied().unwrap_or(0)
}

/// Every region of `top` (frame, menu bar, each visible child control) and
/// its median luminance: the background, as text covers little of it.
fn regions(top: HWND) -> Vec<(String, u8)> {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    let img = capture(top);
    let mut wr: RECT = unsafe { std::mem::zeroed() };
    let mut cr: RECT = unsafe { std::mem::zeroed() };
    let mut origin = POINT { x: 0, y: 0 };
    unsafe {
        GetWindowRect(top, &mut wr);
        GetClientRect(top, &mut cr);
        windows_sys::Win32::Graphics::Gdi::ClientToScreen(top, &mut origin);
    }
    let (ox, oy) = (origin.x - wr.left, origin.y - wr.top);
    let mut out = Vec::new();
    // The title bar: the band above the client area (and menu bar), between
    // the icon and the caption buttons.
    let mut mbi: MENUBARINFO = unsafe { std::mem::zeroed() };
    mbi.cbSize = size_of::<MENUBARINFO>() as u32;
    let has_menu = unsafe { GetMenuBarInfo(top, OBJID_MENU, 0, &mut mbi) } != 0
        && mbi.rcBar.bottom > mbi.rcBar.top;
    let title_bottom = if has_menu { mbi.rcBar.top - wr.top } else { oy };
    let w = wr.right - wr.left;
    out.push((
        "title bar".to_string(),
        median_luma(&img, w / 3, 4, w * 2 / 3, title_bottom - 2),
    ));
    if has_menu {
        let (t, b) = (mbi.rcBar.top - wr.top, mbi.rcBar.bottom - wr.top);
        out.push((
            "menu bar".to_string(),
            median_luma(&img, ox + 2, t + 1, w - 4, b - 1),
        ));
    }
    struct Acc {
        top: HWND,
        list: Vec<(HWND, String)>,
    }
    unsafe extern "system" fn each(h: HWND, l: isize) -> i32 {
        let a = unsafe { &mut *(l as *mut Acc) };
        let mut buf = [0u16; 64];
        let n = unsafe { GetClassNameW(h, buf.as_mut_ptr(), 64) } as usize;
        let visible = unsafe { IsWindowVisible(h) } != 0;
        if visible && unsafe { GetAncestor(h, GA_ROOT) } == a.top {
            a.list.push((h, String::from_utf16_lossy(&buf[..n])));
        }
        1
    }
    let mut acc = Acc {
        top,
        list: Vec::new(),
    };
    unsafe { EnumChildWindows(top, Some(each), &mut acc as *mut _ as isize) };
    for (h, class) in acc.list {
        let mut r: RECT = unsafe { std::mem::zeroed() };
        unsafe { GetWindowRect(h, &mut r) };
        if r.right - r.left < 8 || r.bottom - r.top < 8 {
            continue;
        }
        let id = unsafe { GetDlgCtrlID(h) };
        let (x0, y0) = (r.left - wr.left + 2, r.top - wr.top + 2);
        let (x1, y1) = (r.right - wr.left - 2, r.bottom - wr.top - 2);
        out.push((format!("{class} {id}"), median_luma(&img, x0, y0, x1, y1)));
    }
    out
}

/// The toolbar's Tree button, search box, filter drop-down and Preview
/// button: their tops and bottoms, which must all line up.
fn toolbar_edges(top: HWND) -> Vec<(i32, i32, i32)> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetDlgItem, GetWindowRect};
    [105, 100, 101, 108]
        .iter()
        .map(|&id| {
            let mut r: RECT = unsafe { std::mem::zeroed() };
            unsafe { GetWindowRect(GetDlgItem(top, id), &mut r) };
            (id, r.top, r.bottom)
        })
        .collect()
}

/// View → Theme: dark leaves nothing light (title bar, menu bar, every
/// control, the tree, charts, status bar, the options window), light
/// leaves nothing dark, and System follows Windows' app mode.
#[test]
fn gui_theme_switch() {
    const CMD_THEME_SYSTEM: usize = 420;
    const CMD_THEME_LIGHT: usize = 421;
    const CMD_THEME_DARK: usize = 422;
    let engine = Arc::new(Engine::from_indexes(vec![common::sample_index('C', 2_000)]));
    let hist = std::env::temp_dir().join(format!("quickseek-uitest-theme-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&hist);
    engine.enable_history(hist.clone());
    common::seed_history(&engine);
    let g = Gui::launch(engine);
    g.command(CMD_TREE);
    eventually("tree", Duration::from_secs(8), || {
        (g.tree_rows() > 1).then_some(())
    });
    // Comparing shows the history strip and the growth pane too.
    g.command(CMD_COMPARE_DAY);
    std::thread::sleep(Duration::from_millis(1500));

    let check = |top: HWND, dark: bool, what: &str| {
        let r = regions(top);
        assert!(r.len() >= 4, "{what}: regions {r:?}");
        let wrong: Vec<_> = r
            .iter()
            .filter(|(_, l)| if dark { *l > 90 } else { *l < 150 })
            .collect();
        assert!(
            wrong.is_empty(),
            "{what} ({}): these regions aren't: {wrong:?} (all: {r:?})",
            if dark { "dark" } else { "light" }
        );
    };
    g.command(CMD_THEME_DARK);
    std::thread::sleep(Duration::from_millis(1000));
    check(g.hwnd, true, "main window, dark");
    let edges = toolbar_edges(g.hwnd);
    assert!(
        edges.iter().all(|e| (e.1, e.2) == (edges[0].1, edges[0].2)),
        "toolbar controls (id, top, bottom) don't line up: {edges:?}"
    );
    // A dialog opened while dark.
    g.command(CMD_OPTIONS);
    let pid = g.child.id();
    let opt = eventually("options window", Duration::from_secs(5), || {
        find_window(pid, "quickseek.options")
    });
    std::thread::sleep(Duration::from_millis(800));
    check(opt, true, "options, dark");
    // Switching while it's open repaints it too.
    g.command(CMD_THEME_LIGHT);
    std::thread::sleep(Duration::from_millis(1000));
    check(opt, false, "options, light");
    unsafe {
        PostMessageW(opt, 0x0010 /* WM_CLOSE */, 0, 0)
    };
    std::thread::sleep(Duration::from_millis(500));
    check(g.hwnd, false, "main window, light");
    let edges = toolbar_edges(g.hwnd);
    assert!(
        edges.iter().all(|e| (e.1, e.2) == (edges[0].1, edges[0].2)),
        "toolbar controls (id, top, bottom) don't line up in light: {edges:?}"
    );

    // System: as Windows' app mode says.
    g.command(CMD_THEME_SYSTEM);
    std::thread::sleep(Duration::from_millis(1000));
    let out = Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
            "/v",
            "AppsUseLightTheme",
        ])
        .output()
        .unwrap();
    let system_dark = String::from_utf8_lossy(&out.stdout).contains("0x0");
    check(g.hwnd, system_dark, "main window, system");
    let saved = std::fs::read_to_string(
        std::env::temp_dir()
            .join(format!("quickseek-uitest-{}", std::process::id()))
            .join("gui.json"),
    )
    .unwrap_or_default();
    assert!(saved.contains("\"theme\": \"system\""), "{saved}");
    drop(g);
    let _ = std::fs::remove_dir_all(&hist);
}

/// The example mock service (a separate process, so a test can stop and
/// restart it the way the real service restarts on an update).
/// Killed when dropped, so a failing test doesn't leave it running.
struct Mock(Child);

impl Drop for Mock {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_mock(pipe: &str) -> Mock {
    let exe = std::path::Path::new(env!("CARGO_BIN_EXE_quickseek-gui"))
        .parent()
        .unwrap()
        .join("examples")
        .join("mock_service.exe");
    assert!(
        exe.exists(),
        "{} is missing: run the whole suite (cargo test -p quickseek-gui) so the examples are built",
        exe.display()
    );
    // No console window of its own, and not holding the test's output open.
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let child = Command::new(exe)
        .args(["--pipe", pipe, "--files", "2000"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("starting the mock service");
    let mock = Mock(child);
    eventually("mock service", Duration::from_secs(30), || {
        quickseek_core::ipc::Client::connect_to(pipe).ok()
    });
    mock
}

/// The service goes away (restarted, updated) under a connected window:
/// it says it's reconnecting, then comes back by itself with results.
#[test]
fn gui_reconnects_after_service_restart() {
    let pipe = format!(r"\\.\pipe\quickseek-uitest-restart-{}", std::process::id());
    let mock = start_mock(&pipe);
    let g = Gui::launch_on(&pipe, Arc::new(Engine::from_indexes(Vec::new())));
    eventually("first results", Duration::from_secs(15), || {
        (g.rows() > 0).then_some(())
    });
    drop(mock);
    // Searching while the service is away.
    g.set_query("file");
    eventually("reconnecting shown", Duration::from_secs(15), || {
        g.status().contains("Reconnecting").then_some(())
    });
    let _mock = start_mock(&pipe);
    let t = Instant::now();
    loop {
        let (st, rows) = (g.status(), g.rows());
        if !st.contains("Reconnecting") && !st.contains("service connection") && rows > 0 {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "not back after the restart: status {st:?}, {rows} rows"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(g);
}

/// An updater asks the windows running from the folder it's about to
/// replace to close; they do (and, outside tests, start again after it).
#[test]
fn gui_closes_for_update() {
    let g = Gui::launch(Arc::new(Engine::from_indexes(vec![common::sample_index(
        'C', 200,
    )])));
    let dir = std::path::Path::new(env!("CARGO_BIN_EXE_quickseek-gui"))
        .parent()
        .unwrap()
        .to_path_buf();
    let closed = quickseek_core::upgrade::close_windows_for_update(&dir);
    assert!(closed >= 1, "closed {closed}");
    let mut g = g;
    eventually("window process exits", Duration::from_secs(10), || {
        g.child.try_wait().ok().flatten()
    });
}

/// Options > Folders: a share gets user name and password fields (a local
/// folder doesn't). The test service isn't the real one, so setting a login
/// is refused with a message saying the service keeps them.
#[test]
fn gui_folder_login_fields() {
    let g = Gui::launch(Arc::new(Engine::from_indexes(vec![common::sample_index(
        'C', 200,
    )])));
    g.command(CMD_OPTIONS);
    let pid = g.child.id();
    let opt_hwnd = eventually("options window", Duration::from_secs(5), || {
        find_window(pid, "quickseek.options")
    });
    let opt =
        g.ua.element_from_handle(windows::Win32::Foundation::HWND(opt_hwnd).into())
            .unwrap();
    g.ua.create_matcher()
        .from_ref(&opt)
        .control_type(ControlType::TabItem)
        .name("Folders")
        .timeout(5000)
        .find_first()
        .unwrap()
        .click()
        .unwrap();
    let find = |id: &str| {
        let id = id.to_string();
        g.ua.create_matcher()
            .from_ref(&opt)
            .timeout(5000)
            .filter_fn(Box::new(move |e: &UIElement| {
                Ok(e.get_automation_id().map(|a| a == id).unwrap_or(false))
            }))
            .find_first()
            .unwrap()
    };
    let add_path = |p: &str| {
        find("935")
            .get_pattern::<UIValuePattern>()
            .unwrap()
            .set_value(p)
            .unwrap();
        find("936")
            .get_pattern::<UIInvokePattern>()
            .unwrap()
            .invoke()
            .unwrap();
    };
    add_path(r"D:\local");
    assert!(
        !find("938").is_enabled().unwrap(),
        "a local folder has no login"
    );
    add_path(r"\\nas\share");
    eventually(
        "login fields on for a share",
        Duration::from_secs(5),
        || find("938").is_enabled().unwrap().then_some(()),
    );
    assert!(find("939").is_enabled().unwrap() && find("951").is_enabled().unwrap());
    assert!(!find("952").is_enabled().unwrap(), "no login to clear yet");
    if let Ok(dir) = std::env::var("QUICKSEEK_SHOT_DIR") {
        // For looking at the layout: tools/shot.ps1 on the options window.
        let _ = Command::new("powershell")
            .args([
                "-NoProfile",
                "-File",
                concat!(env!("CARGO_MANIFEST_DIR"), "/../../tools/shot.ps1"),
            ])
            .args(["-ClassName", "quickseek.options", "-Out"])
            .arg(format!("{dir}/options-folders.png"))
            .status();
    }
}
