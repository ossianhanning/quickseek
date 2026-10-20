//! Folder-size tree mode: a flattened, expandable tree in an owner-data
//! list view, with a +/− box per folder and a bar showing each entry's share
//! of its parent. The service computes sizes (see `rapidfind_core::tree`);
//! this module keeps the visible rows and draws them.

use std::ptr::null;

use rapidfind_core::fmt;
use rapidfind_core::tree::{NodeRef, TreeRow, TreeSort};
use windows_sys::Win32::Foundation::{COLORREF, HWND, RECT};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::UI::Controls::*;
use windows_sys::Win32::UI::HiDpi::GetSystemMetricsForDpi;
use windows_sys::Win32::UI::WindowsAndMessaging::SM_CXSMICON;

use crate::shell;
use crate::w::{self, px, send};

pub const COLUMNS: [(&str, i32, bool); 8] = [
    ("Name", 380, false),
    ("Size", 110, true),
    ("% of Parent", 170, false),
    ("Files", 100, true),
    ("Online-only", 110, true),
    ("Date Modified", 140, false),
    ("Change", 110, true),
    ("Trend", 110, false),
];
/// Display order: Change and Trend sit next to Size and the bar.
pub const COLUMN_ORDER: [i32; 8] = [0, 1, 6, 2, 7, 3, 4, 5];
pub const COL_CHANGE: i32 = 6;
pub const COL_TREND: i32 = 7;

/// Growth (red) and shrinkage (blue), as COLORREF (0x00BBGGRR).
pub const GROW: COLORREF = 0x004A_4BE2;
pub const SHRINK: COLORREF = 0x00DD_8A37;
const GHOST: COLORREF = 0x0080_8788;

/// `+1.2 GB` / `−300.0 MB` / `0 B`.
pub fn signed_size(d: i64) -> String {
    match d {
        0 => "0 B".into(),
        d if d > 0 => format!("+{}", fmt::size(d as u64)),
        d => format!("\u{2212}{}", fmt::size(d.unsigned_abs())),
    }
}

pub fn sort_from(s: &str) -> TreeSort {
    match s {
        "name" => TreeSort::Name,
        "files" => TreeSort::Files,
        "modified" => TreeSort::Modified,
        "change" => TreeSort::Change,
        _ => TreeSort::Size,
    }
}

pub fn sort_name(s: TreeSort) -> &'static str {
    match s {
        TreeSort::Name => "name",
        TreeSort::Size => "size",
        TreeSort::Files => "files",
        TreeSort::Modified => "modified",
        TreeSort::Change => "change",
    }
}

pub fn column_sort(col: i32) -> Option<TreeSort> {
    match col {
        0 => Some(TreeSort::Name),
        1 | 2 => Some(TreeSort::Size),
        3 => Some(TreeSort::Files),
        5 => Some(TreeSort::Modified),
        6 | 7 => Some(TreeSort::Change),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub row: TreeRow,
    pub depth: u16,
    pub expanded: bool,
    /// What the bar is relative to: the parent's size (the disk's capacity for a root).
    pub parent_size: u64,
}

impl Node {
    pub fn node_ref(&self) -> NodeRef {
        NodeRef { vol: self.row.vol.clone(), idx: self.row.idx, epoch: self.row.epoch, path: self.row.full_path() }
    }

    pub fn expandable(&self) -> bool {
        self.row.is_dir && self.row.has_children
    }

    pub fn fraction(&self) -> f64 {
        if self.parent_size == 0 { 0.0 } else { (self.row.size as f64 / self.parent_size as f64).clamp(0.0, 1.0) }
    }

    pub fn path(&self) -> String {
        self.row.full_path()
    }

    /// The bar's "then" share of the parent (now), when comparing.
    pub fn before_fraction(&self) -> Option<f64> {
        (self.row.has_before && self.parent_size > 0).then(|| (self.row.before as f64 / self.parent_size as f64).clamp(0.0, 1.0))
    }
}

#[derive(Default)]
pub struct Model {
    pub nodes: Vec<Node>,
    pub size: u64,
    pub cloud: u64,
    pub files: u64,
    pub elapsed_ms: f64,
    pub query: String,
}

impl Model {
    pub fn set_roots(&mut self, size: u64, files: u64, rows: Vec<TreeRow>) {
        self.size = size;
        self.files = files;
        self.nodes = rows
            .into_iter()
            .map(|r| {
                let parent_size = if r.capacity > 0 { r.capacity } else { size };
                Node { row: r, depth: 0, expanded: false, parent_size }
            })
            .collect();
    }

    /// Inserts `children` under node `i` (marking it expanded).
    pub fn expand(&mut self, i: usize, children: Vec<TreeRow>) {
        let Some(n) = self.nodes.get_mut(i) else { return };
        n.expanded = true;
        let (depth, size) = (n.depth + 1, n.row.size);
        let new: Vec<Node> = children.into_iter().map(|r| Node { row: r, depth, expanded: false, parent_size: size }).collect();
        self.nodes.splice(i + 1..i + 1, new);
    }

    /// Removes everything below node `i`.
    pub fn collapse(&mut self, i: usize) {
        let Some(n) = self.nodes.get_mut(i) else { return };
        n.expanded = false;
        let d = n.depth;
        let end = self.nodes[i + 1..].iter().position(|x| x.depth <= d).map_or(self.nodes.len(), |p| i + 1 + p);
        self.nodes.drain(i + 1..end);
    }

    pub fn parent_of(&self, i: usize) -> Option<usize> {
        let d = self.nodes.get(i)?.depth;
        (0..i).rev().find(|&j| self.nodes[j].depth < d)
    }

    /// Paths of expanded nodes, parents first (to restore after a rebuild).
    pub fn expanded_paths(&self) -> Vec<String> {
        self.nodes.iter().filter(|n| n.expanded).map(Node::path).collect()
    }

    pub fn find(&self, path: &str) -> Option<usize> {
        let p = path.trim_end_matches('\\');
        self.nodes.iter().position(|n| n.path().trim_end_matches('\\').eq_ignore_ascii_case(p))
    }
}

/// Is `path` this node's path or inside it?
pub fn contains(node_path: &str, path: &str) -> bool {
    let (a, b) = (node_path.trim_end_matches('\\').to_lowercase(), path.trim_end_matches('\\').to_lowercase());
    b == a || (b.starts_with(&a) && b.as_bytes().get(a.len()) == Some(&b'\\'))
}

pub fn cell_text(n: &Node, col: i32) -> String {
    let r = &n.row;
    match col {
        0 if r.capacity > 0 => format!("{}   {} free of {}", r.name, fmt::size(r.free), fmt::size(r.capacity)),
        0 => r.name.clone(),
        1 if r.has_size || !r.is_dir => fmt::size(r.size),
        2 => format!("{:.1} %", n.fraction() * 100.0),
        3 if r.is_dir => shell::group(r.files),
        3 if r.hardlink => "hard link".into(),
        3 if r.cloud > 0 && r.size == 0 => "online-only".into(),
        4 if r.cloud > 0 => fmt::size(r.cloud),
        5 if r.modified != 0 => fmt::time(r.modified),
        6 if r.has_before && r.changed => format!("{} new or changed", signed_size(r.change())),
        6 if r.has_before => signed_size(r.change()),
        _ => String::new(),
    }
}

// ---- drawing -----------------------------------------------------------------

/// Rectangle of column `col` in row `i` (header order, horizontally scrolled).
pub fn cell_rect(list: HWND, i: usize, col: i32) -> RECT {
    let mut row = RECT { left: LVIR_BOUNDS as i32, top: 0, right: 0, bottom: 0 };
    send(list, LVM_GETITEMRECT, i, &mut row as *mut _ as isize);
    let header = send(list, LVM_GETHEADER, 0, 0) as HWND;
    let mut h = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    send(header, HDM_GETITEMRECT, col as usize, &mut h as *mut _ as isize);
    RECT { left: row.left + h.left, top: row.top, right: row.left + h.right, bottom: row.bottom }
}

/// Indent step and the +/− box's horizontal span within a name cell.
pub fn glyph_span(list: HWND, cell: &RECT, depth: u16) -> (i32, i32) {
    let step = px(list, 16);
    let x = cell.left + px(list, 4) + depth as i32 * step;
    (x, x + step)
}

pub struct Paint {
    pub hdc: HDC,
    pub font: HFONT,
    pub bold: HFONT,
    pub selected: bool,
    pub classic_sel: bool,
}

fn colors(p: &Paint) -> (COLORREF, COLORREF) {
    unsafe {
        if p.classic_sel { (GetSysColor(COLOR_HIGHLIGHT), GetSysColor(COLOR_HIGHLIGHTTEXT)) } else { (GetSysColor(COLOR_WINDOW), GetSysColor(COLOR_WINDOWTEXT)) }
    }
}

fn fill(hdc: HDC, rc: &RECT, c: COLORREF) {
    unsafe {
        let br = CreateSolidBrush(c);
        FillRect(hdc, rc, br);
        DeleteObject(br as _);
    }
}

fn text_at(hdc: HDC, s: &str, x: i32, rc: &RECT, right_align: bool) -> i32 {
    let w16: Vec<u16> = s.encode_utf16().collect();
    let mut sz = windows_sys::Win32::Foundation::SIZE { cx: 0, cy: 0 };
    unsafe {
        GetTextExtentPoint32W(hdc, w16.as_ptr(), w16.len() as i32, &mut sz);
        let x = if right_align { rc.right - sz.cx } else { x };
        let y = rc.top + (rc.bottom - rc.top - sz.cy) / 2;
        ExtTextOutW(hdc, x, y, ETO_CLIPPED, rc, w16.as_ptr(), w16.len() as u32, null());
        x + sz.cx
    }
}

/// Name cell: indentation, +/− box, icon, name (bold when the query matched it).
pub fn draw_name(list: HWND, p: &Paint, n: &Node, rc: RECT, icon: i32, image_list: isize) {
    let (bg, fg) = colors(p);
    unsafe {
        if !p.selected || p.classic_sel {
            fill(p.hdc, &rc, bg);
        }
        let (gx, gx2) = glyph_span(list, &rc, n.depth);
        let cy = (rc.top + rc.bottom) / 2;
        if n.expandable() {
            let half = px(list, 4);
            let cx = (gx + gx2) / 2;
            let pen = CreatePen(PS_SOLID, 1, if p.classic_sel { fg } else { 0x0080_8080 });
            let old_pen = SelectObject(p.hdc, pen as _);
            let old_brush = SelectObject(p.hdc, GetStockObject(NULL_BRUSH));
            Rectangle(p.hdc, cx - half, cy - half, cx + half + 1, cy + half + 1);
            let ink = CreatePen(PS_SOLID, 1, fg);
            SelectObject(p.hdc, ink as _);
            let arm = half - px(list, 2);
            MoveToEx(p.hdc, cx - arm, cy, null_mut());
            LineTo(p.hdc, cx + arm + 1, cy);
            if !n.expanded {
                MoveToEx(p.hdc, cx, cy - arm, null_mut());
                LineTo(p.hdc, cx, cy + arm + 1);
            }
            SelectObject(p.hdc, old_pen);
            SelectObject(p.hdc, old_brush);
            DeleteObject(pen as _);
            DeleteObject(ink as _);
        }
        let sz = GetSystemMetricsForDpi(SM_CXSMICON, w::dpi(list) as u32);
        let ix = gx2 + px(list, 2);
        ImageList_Draw(image_list as HIMAGELIST, icon, p.hdc, ix, cy - sz / 2, ILD_TRANSPARENT);
        SetBkMode(p.hdc, TRANSPARENT as i32);
        SetTextColor(p.hdc, fg);
        let old = SelectObject(p.hdc, if n.row.matched { p.bold } else { p.font } as _);
        let text_rc = RECT { left: ix + sz + px(list, 4), ..rc };
        let end = text_at(p.hdc, &n.row.name, text_rc.left, &text_rc, false);
        SelectObject(p.hdc, p.font as _);
        if n.row.capacity > 0 {
            SetTextColor(p.hdc, if p.classic_sel { fg } else { 0x0070_7070 });
            let extra = format!("   {} free of {}", fmt::size(n.row.free), fmt::size(n.row.capacity));
            text_at(p.hdc, &extra, end, &text_rc, false);
        }
        SelectObject(p.hdc, old);
    }
}

/// "% of Parent" cell: a bar for the share of the parent, and the percentage.
pub fn draw_bar(list: HWND, p: &Paint, n: &Node, rc: RECT) {
    let (bg, fg) = colors(p);
    unsafe {
        if !p.selected || p.classic_sel {
            fill(p.hdc, &rc, bg);
        }
        let pad = px(list, 3);
        let text_w = px(list, 52);
        let track = RECT { left: rc.left + pad * 2, top: rc.top + pad, right: (rc.right - text_w).max(rc.left + pad * 2), bottom: rc.bottom - pad };
        fill(p.hdc, &track, 0x00EC_E8E4);
        let span = (track.right - track.left) as f64;
        let w = (span * n.fraction()).round() as i32;
        // Folders in a stronger blue than files; roots (share of the disk) in teal.
        let c: COLORREF = if n.row.capacity > 0 { 0x0098_8A00 } else if n.row.is_dir { 0x00D7_7800 } else { 0x00F0_B060 };
        match n.before_fraction() {
            // Comparing: solid bar up to the smaller of then and now, the
            // growth in red, and a dashed outline where shrunk space was.
            Some(bf) => {
                let bw = (span * bf).round() as i32;
                let common = w.min(bw);
                if common > 0 {
                    fill(p.hdc, &RECT { right: track.left + common, ..track }, c);
                }
                if w > bw {
                    fill(p.hdc, &RECT { left: track.left + bw, right: track.left + w, ..track }, GROW);
                } else if bw > w {
                    let pen = CreatePen(PS_DOT, 1, SHRINK);
                    let old_pen = SelectObject(p.hdc, pen as _);
                    let old_brush = SelectObject(p.hdc, GetStockObject(NULL_BRUSH));
                    Rectangle(p.hdc, track.left + w, track.top, track.left + bw, track.bottom);
                    SelectObject(p.hdc, old_pen);
                    SelectObject(p.hdc, old_brush);
                    DeleteObject(pen as _);
                }
                // The "then" edge as a thin mark.
                if bw > 0 && bw != w {
                    let pen = CreatePen(PS_SOLID, 1, GHOST);
                    let old_pen = SelectObject(p.hdc, pen as _);
                    MoveToEx(p.hdc, track.left + bw, track.top - 1, null_mut());
                    LineTo(p.hdc, track.left + bw, track.bottom + 1);
                    SelectObject(p.hdc, old_pen);
                    DeleteObject(pen as _);
                }
            }
            None if w > 0 => fill(p.hdc, &RECT { right: track.left + w, ..track }, c),
            None => {}
        }
        SetBkMode(p.hdc, TRANSPARENT as i32);
        SetTextColor(p.hdc, fg);
        let old = SelectObject(p.hdc, p.font as _);
        let text_rc = RECT { left: track.right, right: rc.right - pad * 2, ..rc };
        text_at(p.hdc, &format!("{:.1} %", n.fraction() * 100.0), 0, &text_rc, true);
        SelectObject(p.hdc, old);
    }
}

/// "Change" cell: the signed change, red for growth, blue for shrinkage.
pub fn draw_change(list: HWND, p: &Paint, n: &Node, rc: RECT) {
    let (bg, fg) = colors(p);
    unsafe {
        if !p.selected || p.classic_sel {
            fill(p.hdc, &rc, bg);
        }
        if !n.row.has_before {
            return;
        }
        let d = n.row.change();
        SetBkMode(p.hdc, TRANSPARENT as i32);
        SetTextColor(p.hdc, if p.classic_sel { fg } else if d > 0 { GROW } else if d < 0 { SHRINK } else { 0x0080_8080 });
        let old = SelectObject(p.hdc, if d.unsigned_abs() >= 1 << 30 { p.bold } else { p.font } as _);
        let pad = px(list, 6);
        let text = if n.row.changed { format!("{} new", signed_size(d)) } else { signed_size(d) };
        text_at(p.hdc, &text, 0, &RECT { right: rc.right - pad, ..rc }, true);
        SelectObject(p.hdc, old);
    }
}

/// "Trend" cell: a sparkline of the size in every snapshot, then now.
/// `series` holds `u64::MAX` where a snapshot didn't record the folder.
pub fn draw_trend(list: HWND, p: &Paint, n: &Node, rc: RECT, series: Option<&[u64]>) {
    let (bg, _) = colors(p);
    unsafe {
        if !p.selected || p.classic_sel {
            fill(p.hdc, &rc, bg);
        }
        let Some(series) = series else { return };
        let mut v: Vec<u64> = series.iter().map(|&x| if x == u64::MAX { 0 } else { x }).collect();
        v.push(n.row.size);
        if v.len() < 2 {
            return;
        }
        let (lo, hi) = (*v.iter().min().unwrap(), *v.iter().max().unwrap());
        let pad = px(list, 4);
        let (x0, x1, y0, y1) = (rc.left + pad, rc.right - pad, rc.top + pad, rc.bottom - pad);
        let span = (hi - lo).max(1) as f64;
        let pts: Vec<POINT> = v
            .iter()
            .enumerate()
            .map(|(i, &x)| POINT { x: x0 + ((x1 - x0) as f64 * i as f64 / (v.len() - 1) as f64) as i32, y: y1 - (((x - lo) as f64 / span) * (y1 - y0) as f64) as i32 })
            .collect();
        let rising = v.last() > v.first();
        let flat = hi - lo < (hi / 100).max(1 << 20);
        let pen = CreatePen(PS_SOLID, px(list, 2).max(1), if flat { 0x0080_8780 } else if rising { GROW } else { SHRINK });
        let old = SelectObject(p.hdc, pen as _);
        Polyline(p.hdc, pts.as_ptr(), pts.len() as i32);
        SelectObject(p.hdc, old);
        DeleteObject(pen as _);
    }
}

use std::ptr::null_mut;
use windows_sys::Win32::Foundation::POINT;

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, dir: &str, size: u64, is_dir: bool) -> TreeRow {
        TreeRow { name: name.into(), dir: dir.into(), size, is_dir, has_children: is_dir, has_size: true, ..Default::default() }
    }

    #[test]
    fn expand_collapse_and_paths() {
        let mut m = Model::default();
        m.set_roots(100, 3, vec![TreeRow { capacity: 400, ..row("C:", "", 100, true) }, row("D:", "", 50, true)]);
        assert_eq!(m.nodes[0].parent_size, 400); // a root's bar is its share of the disk
        assert_eq!(m.nodes[1].parent_size, 100);
        m.expand(0, vec![row("Users", r"C:\", 80, true), row("pagefile.sys", r"C:\", 20, false)]);
        m.expand(1, vec![row("me", r"C:\Users", 60, true)]);
        assert_eq!(m.nodes.iter().map(|n| n.depth).collect::<Vec<_>>(), [0, 1, 2, 1, 0]);
        assert_eq!(m.nodes[2].path(), r"C:\Users\me");
        assert_eq!(m.nodes[2].parent_size, 80);
        assert!((m.nodes[1].fraction() - 0.8).abs() < 1e-9);
        assert_eq!(m.parent_of(2), Some(1));
        assert_eq!(m.parent_of(1), Some(0));
        assert_eq!(m.expanded_paths(), [r"C:\", r"C:\Users"]);
        assert_eq!(m.find(r"c:\users"), Some(1));
        m.collapse(0);
        assert_eq!(m.nodes.len(), 2);
        assert!(contains(r"C:\Users", r"c:\users\me\x") && !contains(r"C:\Use", r"C:\Users") && contains(r"C:\", r"C:\a"));
        assert_eq!(cell_text(&m.nodes[0], 2), "25.0 %");
        assert_eq!(column_sort(3), Some(TreeSort::Files));
        assert_eq!(sort_from(sort_name(TreeSort::Modified)), TreeSort::Modified);
        assert_eq!(sort_from(sort_name(TreeSort::Change)), TreeSort::Change);
        assert_eq!((signed_size(5 << 30), signed_size(-(3 << 20)), signed_size(0)), ("+5.0 GB".into(), "\u{2212}3.0 MB".into(), "0 B".into()));
        m.nodes[0].row = TreeRow { has_before: true, before: 200, ..m.nodes[0].row.clone() };
        assert_eq!(m.nodes[0].before_fraction(), Some(0.5));
        assert_eq!(cell_text(&m.nodes[0], 6), "\u{2212}100 B");
    }
}
