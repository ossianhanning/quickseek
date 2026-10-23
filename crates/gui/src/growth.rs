//! Folder growth in tree mode: comparing with a snapshot, the timeline
//! strip above the tree (history of the selected folder's size, stacked by
//! its biggest children, with a forecast; clicking a day compares with it)
//! and the details pane beside it: where the space went between then and
//! now as a waterfall, the chain of folders that grew most, a treemap
//! colored by change and a calendar of daily changes.

use std::collections::HashMap;
use std::ptr::{null, null_mut};

use rapidfind_core::fmt;
use rapidfind_core::history::SnapInfo;
use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Graphics::GdiPlus::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::theme;
use crate::tree::signed_size;
use crate::w::{self, px, wide};

pub const STRIP_CLASS: &str = "rapidfind.timeline";
pub const PANE_CLASS: &str = "rapidfind.growthpane";
pub const ID_STRIP: i32 = 106;
pub const ID_PANE: i32 = 107;
/// Not in the windows-sys modules we import.
const WM_MOUSELEAVE: u32 = 0x02A3;

/// Comparison state and cached history.
#[derive(Default)]
pub struct Growth {
    /// Snapshots, oldest first.
    pub snaps: Vec<SnapInfo>,
    /// Comparing with this one.
    pub base: Option<SnapInfo>,
    /// Folder path key → size in every snapshot (`u64::MAX` = not recorded).
    pub series: HashMap<String, Vec<u64>>,
    /// Volume root key → free space in every snapshot.
    pub free: HashMap<String, Vec<u64>>,
    /// Tree sort before comparing (restored after).
    pub prev_sort: Option<(String, bool)>,
    /// Pointer over the strip: x in client pixels.
    pub hover_x: Option<i32>,
    /// Details pane: 0 = waterfall, 1 = growth path, 2 = map, 3 = calendar.
    pub tab: u8,
    /// Pointer over the details pane (client pixels).
    pub pane_hover: Option<(i32, i32)>,
    pub pane: Option<PaneData>,
    /// What the strip shows (rebuilt when the selection changes).
    pub strip: Option<StripData>,
}

pub fn key(path: &str) -> String {
    rapidfind_core::history::path_key(path)
}

/// "Oct 13" / "Oct 13 14:05" for a unix time.
pub fn day(t: i64, with_time: bool) -> String {
    let s = fmt::time((t + 11_644_473_600) * 10_000_000);
    // fmt::time gives "YYYY-MM-DD HH:MM".
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let m: usize = s.get(5..7).and_then(|x| x.parse().ok()).unwrap_or(1);
    let d: u32 = s.get(8..10).and_then(|x| x.parse().ok()).unwrap_or(1);
    let base = format!("{} {d}", months[(m.clamp(1, 12)) - 1]);
    if with_time {
        format!("{base} {}", s.get(11..16).unwrap_or(""))
    } else {
        base
    }
}

/// "today", "yesterday", "7 days ago".
pub fn ago(t: i64) -> String {
    let now = rapidfind_core::history::now_unix();
    let today = rapidfind_core::history::local_midnight(now);
    if t >= today {
        "today".into()
    } else if t >= today - 86_400 {
        "yesterday".into()
    } else {
        format!("{} days ago", (today - t + 86_399) / 86_400)
    }
}

/// The snapshot closest to `days` ago.
pub fn snap_days_ago(snaps: &[SnapInfo], days: i64) -> Option<SnapInfo> {
    let target = rapidfind_core::history::now_unix() - days * 86_400;
    snaps
        .iter()
        .min_by_key(|s| (s.time - target).abs())
        .cloned()
}

// ---- data the drawings take ------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Layer {
    pub label: String,
    pub values: Vec<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct StripData {
    pub title: String,
    /// Unix times: every snapshot, then now.
    pub times: Vec<i64>,
    /// Stacked bottom-up; the last is "everything else".
    pub layers: Vec<Layer>,
    /// Disk size (drives only; shown in the title by the caller).
    #[allow(dead_code)]
    pub capacity: u64,
    /// Free space at each time (drives only).
    pub free: Vec<u64>,
    /// Index into `times` being compared with.
    pub base: Option<usize>,
    pub save_points: Vec<usize>,
}

impl StripData {
    fn totals(&self) -> Vec<u64> {
        (0..self.times.len())
            .map(|i| {
                self.layers
                    .iter()
                    .map(|l| l.values.get(i).copied().unwrap_or(0))
                    .sum()
            })
            .collect()
    }

    /// "full in ~19 days (Nov 8)" from the free-space trend of the last two
    /// weeks, or a growth rate when there's no disk to fill.
    pub fn forecast(&self) -> Option<String> {
        let now = *self.times.last()?;
        let from = now - 14 * 86_400;
        let series: Vec<(f64, f64)> = if !self.free.is_empty() {
            self.times
                .iter()
                .zip(&self.free)
                .filter(|(t, f)| **t >= from && **f != u64::MAX)
                .map(|(t, f)| (*t as f64, *f as f64))
                .collect()
        } else {
            self.times
                .iter()
                .zip(self.totals())
                .filter(|(t, _)| **t >= from)
                .map(|(t, v)| (*t as f64, v as f64))
                .collect()
        };
        if series.len() < 3 || series.last()?.0 - series[0].0 < 2.0 * 86_400.0 {
            return None;
        }
        let n = series.len() as f64;
        let (mx, my) = (
            series.iter().map(|p| p.0).sum::<f64>() / n,
            series.iter().map(|p| p.1).sum::<f64>() / n,
        );
        let num: f64 = series.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
        let den: f64 = series.iter().map(|p| (p.0 - mx).powi(2)).sum();
        if den == 0.0 {
            return None;
        }
        let slope = num / den; // bytes per second
        let per_week = slope * 7.0 * 86_400.0;
        if !self.free.is_empty() {
            let free_now = *self.free.last()? as f64;
            if slope >= -1.0 {
                return Some("free space steady or growing".into());
            }
            let days = free_now / -slope / 86_400.0;
            if days > 730.0 {
                return None;
            }
            return Some(format!(
                "full in ~{} days ({}) at {}/week",
                days.round() as i64,
                day(now + (days * 86_400.0) as i64, false),
                signed_size(-per_week as i64)
            ));
        }
        (per_week.abs() >= (1u64 << 20) as f64)
            .then(|| format!("{}/week", signed_size(per_week as i64)))
    }
}

/// The selected folder's change, for the details pane.
#[derive(Clone, Debug, Default)]
pub struct PaneData {
    pub title: String,
    pub since: String,
    pub before: u64,
    pub now: u64,
    /// Children's changes, biggest first (the rest summed into "other").
    pub parts: Vec<(String, i64)>,
    /// Drives: free space then and now.
    pub free: Option<(u64, u64)>,
    /// From this folder down, the child that grew most at each level.
    pub chain: Vec<(String, i64, u64)>,
    /// The treemap (filled only while the Map tab shows): children, the
    /// biggest with their own children.
    pub map: Vec<MapItem>,
    /// Change per local day, oldest first (the Calendar tab).
    pub days: Vec<DayChange>,
}

/// A box of the treemap.
#[derive(Clone, Debug, Default)]
pub struct MapItem {
    pub name: String,
    /// Empty for summary boxes ("other", "deleted or moved"): not clickable.
    pub path: String,
    pub size: u64,
    pub before: u64,
    pub kids: Vec<MapItem>,
}

impl MapItem {
    /// Boxes are as big as the item was at its largest, so what shrank or
    /// went away still shows.
    pub fn area(&self) -> u64 {
        self.size.max(self.before)
    }
    pub fn change(&self) -> i64 {
        self.size as i64 - self.before as i64
    }
}

/// How much a folder changed on one local day.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DayChange {
    /// Local midnight starting the day.
    pub start: i64,
    pub delta: i64,
    /// Spread from a snapshot interval longer than a day and a half.
    pub estimated: bool,
}

/// Splits each change between consecutive points over the local days the
/// interval covers, in proportion to the time in each. `u64::MAX` values
/// (not recorded) break the series.
pub fn daily(times: &[i64], vals: &[u64]) -> Vec<DayChange> {
    let mut by_day: std::collections::BTreeMap<i64, (f64, bool)> =
        std::collections::BTreeMap::new();
    for i in 1..times.len().min(vals.len()) {
        let (t0, t1, v0, v1) = (times[i - 1], times[i], vals[i - 1], vals[i]);
        if v0 == u64::MAX || v1 == u64::MAX || t1 <= t0 {
            continue;
        }
        let delta = v1 as f64 - v0 as f64;
        let estimated = t1 - t0 > 36 * 3600;
        let mut ds = rapidfind_core::history::local_midnight(t0);
        while ds < t1 {
            // Noon of the next day, back to its midnight: safe across DST changes.
            let next = rapidfind_core::history::local_midnight(ds + 36 * 3600);
            let overlap = (next.min(t1) - ds.max(t0)).max(0);
            if overlap > 0 {
                let e = by_day.entry(ds).or_insert((0.0, false));
                e.0 += delta * overlap as f64 / (t1 - t0) as f64;
                e.1 |= estimated;
            }
            ds = next;
        }
    }
    by_day
        .into_iter()
        .map(|(start, (d, estimated))| DayChange {
            start,
            delta: d.round() as i64,
            estimated,
        })
        .collect()
}

/// Squarified treemap (Bruls, Huizing, van Wijk): rectangles `(x, y, w, h)`
/// for `areas` (biggest first, all > 0) that tile `r` with boxes as close
/// to square as the order allows.
pub fn squarify(areas: &[f64], r: (f64, f64, f64, f64)) -> Vec<(f64, f64, f64, f64)> {
    let total: f64 = areas.iter().sum();
    let (mut x, mut y, mut w, mut h) = r;
    if total <= 0.0 || w <= 0.0 || h <= 0.0 {
        return vec![(x, y, 0.0, 0.0); areas.len()];
    }
    let scale = w * h / total;
    let a: Vec<f64> = areas.iter().map(|v| v * scale).collect();
    let mut out = Vec::with_capacity(a.len());
    let mut i = 0;
    while i < a.len() {
        let side = w.min(h);
        let worst = |row: &[f64]| {
            let s: f64 = row.iter().sum();
            let (mx, mn) = row
                .iter()
                .fold((0f64, f64::MAX), |(mx, mn), &v| (mx.max(v), mn.min(v)));
            (side * side * mx / (s * s)).max(s * s / (side * side * mn))
        };
        let mut j = i + 1;
        while j < a.len() && worst(&a[i..=j]) <= worst(&a[i..j]) {
            j += 1;
        }
        let row = &a[i..j];
        let s: f64 = row.iter().sum();
        if w >= h {
            // A column along the left edge.
            let cw = s / h;
            let mut yy = y;
            for v in row {
                let hh = h * v / s;
                out.push((x, yy, cw, hh));
                yy += hh;
            }
            x += cw;
            w -= cw;
        } else {
            // A row along the top.
            let rh = s / w;
            let mut xx = x;
            for v in row {
                let ww = w * v / s;
                out.push((xx, y, ww, rh));
                xx += ww;
            }
            y += rh;
            h -= rh;
        }
        i = j;
    }
    out
}

// ---- GDI+ ------------------------------------------------------------------------

pub fn gdiplus_start() {
    static TOKEN: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    TOKEN.get_or_init(|| {
        let input = GdiplusStartupInput {
            GdiplusVersion: 1,
            DebugEventCallback: 0,
            SuppressBackgroundThread: 0,
            SuppressExternalCodecs: 0,
        };
        let mut token = 0usize;
        unsafe { GdiplusStartup(&mut token, &input, null_mut()) };
        token
    });
}

struct Gfx(*mut GpGraphics);

fn argb(c: COLORREF, a: u8) -> u32 {
    // COLORREF is 0x00BBGGRR.
    (a as u32) << 24 | (c & 0xFF) << 16 | (c & 0xFF00) | (c >> 16) & 0xFF
}

impl Gfx {
    fn new(hdc: HDC) -> Gfx {
        let mut g = null_mut();
        unsafe {
            GdipCreateFromHDC(hdc, &mut g);
            GdipSetSmoothingMode(g, SmoothingModeAntiAlias);
        }
        Gfx(g)
    }
    fn poly(&self, pts: &[Point], c: COLORREF, a: u8) {
        unsafe {
            let mut b = null_mut();
            GdipCreateSolidFill(argb(c, a), &mut b);
            GdipFillPolygonI(
                self.0,
                b as *mut GpBrush,
                pts.as_ptr(),
                pts.len() as i32,
                FillModeAlternate,
            );
            GdipDeleteBrush(b as *mut GpBrush);
        }
    }
    fn rect(&self, x: i32, y: i32, w: i32, h: i32, c: COLORREF, a: u8) {
        if w <= 0 || h <= 0 {
            return;
        }
        unsafe {
            let mut b = null_mut();
            GdipCreateSolidFill(argb(c, a), &mut b);
            GdipFillRectangleI(self.0, b as *mut GpBrush, x, y, w, h);
            GdipDeleteBrush(b as *mut GpBrush);
        }
    }
    fn lines(&self, pts: &[Point], c: COLORREF, width: f32, dashed: bool) {
        if pts.len() < 2 {
            return;
        }
        unsafe {
            let mut p = null_mut();
            GdipCreatePen1(argb(c, 255), width, UnitPixel, &mut p);
            if dashed {
                GdipSetPenDashStyle(p, DashStyleDash);
            }
            GdipDrawLinesI(self.0, p, pts.as_ptr(), pts.len() as i32);
            GdipDeletePen(p);
        }
    }
    fn dot(&self, x: i32, y: i32, r: i32, c: COLORREF) {
        unsafe {
            let mut b = null_mut();
            GdipCreateSolidFill(argb(c, 255), &mut b);
            GdipFillEllipseI(self.0, b as *mut GpBrush, x - r, y - r, 2 * r, 2 * r);
            GdipDeleteBrush(b as *mut GpBrush);
        }
    }
}

impl Drop for Gfx {
    fn drop(&mut self) {
        unsafe { GdipDeleteGraphics(self.0) };
    }
}

fn text(hdc: HDC, s: &str, rc: &RECT, flags: u32, color: COLORREF) {
    let w16: Vec<u16> = s.encode_utf16().collect();
    let mut r = *rc;
    unsafe {
        SetBkMode(hdc, TRANSPARENT as i32);
        SetTextColor(hdc, color);
        DrawTextW(
            hdc,
            w16.as_ptr(),
            w16.len() as i32,
            &mut r,
            flags | DT_NOPREFIX | DT_SINGLELINE,
        );
    }
}

fn text_width(hdc: HDC, s: &str) -> i32 {
    let w16: Vec<u16> = s.encode_utf16().collect();
    let mut sz = windows_sys::Win32::Foundation::SIZE { cx: 0, cy: 0 };
    unsafe { GetTextExtentPoint32W(hdc, w16.as_ptr(), w16.len() as i32, &mut sz) };
    sz.cx
}

// Colours from the theme's palette (crate::theme): layers in the Cove
// categorical order, then grey for "everything else"; growth (danger) red,
// shrinkage (accent) blue.
fn layer(i: usize) -> COLORREF {
    let l = theme::p().layers;
    l[i % l.len()]
}
fn other() -> COLORREF {
    theme::p().other
}
fn ink() -> COLORREF {
    theme::p().ink
}
fn muted() -> COLORREF {
    theme::p().muted
}
fn grid() -> COLORREF {
    theme::p().grid
}
fn accent() -> COLORREF {
    theme::p().shrink
}
fn danger() -> COLORREF {
    theme::p().grow
}
/// Growth and shrinkage as fills that carry white labels (deeper in dark).
fn danger_fill() -> COLORREF {
    theme::p().grow_fill
}
fn accent_fill() -> COLORREF {
    theme::p().shrink_fill
}
fn neutral() -> COLORREF {
    theme::p().neutral
}
fn window_brush() -> HBRUSH {
    theme::brush(theme::p().window)
}

/// Time → x across the plot: proportional to time, so gaps show.
fn x_of(times: &[i64], t: i64, x0: i32, x1: i32) -> i32 {
    let (a, b) = (times[0], *times.last().unwrap());
    if b <= a {
        return x1;
    }
    x0 + ((x1 - x0) as f64 * (t - a) as f64 / (b - a) as f64) as i32
}

/// Index of the time nearest to pixel `x`.
pub fn nearest(times: &[i64], x: i32, x0: i32, x1: i32) -> Option<usize> {
    (0..times.len()).min_by_key(|&i| (x_of(times, times[i], x0, x1) - x).abs())
}

/// The plot area of the strip's client rect (room for labels around it).
pub fn plot_rect(hwnd: HWND, rc: &RECT) -> RECT {
    RECT {
        left: rc.left + px(hwnd, 12),
        top: rc.top + px(hwnd, 26),
        right: rc.right - px(hwnd, 12),
        bottom: rc.bottom - px(hwnd, 18),
    }
}

pub fn draw_strip(
    hwnd: HWND,
    hdc: HDC,
    rc: &RECT,
    d: Option<&StripData>,
    hover_x: Option<i32>,
    font: HFONT,
) {
    unsafe {
        let bg = window_brush();
        FillRect(hdc, rc, bg);
        SelectObject(hdc, font as _);
    }
    let pr = plot_rect(hwnd, rc);
    let Some(d) = d.filter(|d| d.times.len() >= 2) else {
        let msg = match d {
            Some(_) => {
                "History starts today: rapidfind keeps a snapshot of folder sizes every day. Come back tomorrow to see what grew."
            }
            None => "Folder size history isn't available.",
        };
        text(
            hdc,
            msg,
            &RECT {
                left: pr.left,
                top: rc.top,
                right: pr.right,
                bottom: rc.bottom,
            },
            DT_VCENTER | DT_LEFT | DT_END_ELLIPSIS,
            muted(),
        );
        return;
    };
    let totals = d.totals();
    // Plot change since the first point, not absolute size: growth of a few
    // GB on a 500 GB folder would be invisible otherwise. Each layer's gain
    // stacks above the zero line, its loss below; a line shows the total.
    let n = d.times.len();
    let deltas: Vec<Vec<i64>> = d
        .layers
        .iter()
        .map(|l| {
            l.values
                .iter()
                .map(|&v| v as i64 - l.values[0] as i64)
                .collect()
        })
        .collect();
    let total_delta: Vec<i64> = (0..n).map(|i| deltas.iter().map(|l| l[i]).sum()).collect();
    let pos_max = (0..n)
        .map(|i| deltas.iter().map(|l| l[i].max(0)).sum::<i64>())
        .max()
        .unwrap_or(0);
    let neg_min = (0..n)
        .map(|i| deltas.iter().map(|l| l[i].min(0)).sum::<i64>())
        .min()
        .unwrap_or(0);
    let (lo, hi) = (neg_min.min(0), pos_max.max(0));
    let range = ((hi - lo) as f64 * 1.08).max(1.0);
    let y_of = |v: i64| {
        pr.bottom
            - ((pr.bottom - pr.top) as f64 * ((v - lo) as f64 + (hi - lo) as f64 * 0.04) / range)
                as i32
    };
    let xs: Vec<i32> = d
        .times
        .iter()
        .map(|&t| x_of(&d.times, t, pr.left, pr.right))
        .collect();
    {
        let g = Gfx::new(hdc);
        // Comparison span shaded.
        if let Some(b) = d.base {
            g.rect(
                xs[b],
                pr.top,
                pr.right - xs[b],
                pr.bottom - pr.top,
                accent(),
                28,
            );
        }
        let (mut up, mut down) = (vec![0i64; n], vec![0i64; n]);
        for (li, l) in deltas.iter().enumerate() {
            let c = if li + 1 == deltas.len() {
                other()
            } else {
                layer(li)
            };
            for positive in [true, false] {
                let base = if positive { &up } else { &down };
                let top: Vec<i64> = base
                    .iter()
                    .zip(l)
                    .map(|(b, v)| {
                        if positive {
                            b + v.max(&0)
                        } else {
                            b + v.min(&0)
                        }
                    })
                    .collect();
                if top != *base {
                    let mut pts: Vec<Point> = xs
                        .iter()
                        .zip(&top)
                        .map(|(&x, &v)| Point { X: x, Y: y_of(v) })
                        .collect();
                    pts.extend(
                        xs.iter()
                            .zip(base)
                            .rev()
                            .map(|(&x, &v)| Point { X: x, Y: y_of(v) }),
                    );
                    g.poly(&pts, c, 210);
                }
                if positive { up = top } else { down = top }
            }
        }
        let y0 = y_of(0);
        g.lines(
            &[Point { X: pr.left, Y: y0 }, Point { X: pr.right, Y: y0 }],
            muted(),
            1.0,
            false,
        );
        let line: Vec<Point> = xs
            .iter()
            .zip(&total_delta)
            .map(|(&x, &v)| Point { X: x, Y: y_of(v) })
            .collect();
        g.lines(&line, ink(), 1.5, false);
        for &i in &d.save_points {
            g.dot(xs[i], pr.top + px(hwnd, 3), px(hwnd, 3), ink());
        }
        if let Some(b) = d.base {
            g.lines(
                &[
                    Point {
                        X: xs[b],
                        Y: pr.top,
                    },
                    Point {
                        X: xs[b],
                        Y: pr.bottom,
                    },
                ],
                accent(),
                2.0,
                false,
            );
        }
        if let Some(hx) = hover_x
            && let Some(i) = nearest(&d.times, hx, pr.left, pr.right)
        {
            g.lines(
                &[
                    Point {
                        X: xs[i],
                        Y: pr.top,
                    },
                    Point {
                        X: xs[i],
                        Y: pr.bottom,
                    },
                ],
                ink(),
                1.0,
                true,
            );
            g.dot(xs[i], y_of(total_delta[i]), px(hwnd, 3), ink());
        }
    }
    // Scale: the extremes, beside the plot.
    for (v, y) in [(hi, pr.top), (lo, pr.bottom - px(hwnd, 14))] {
        if v != 0 {
            text(
                hdc,
                &signed_size(v),
                &RECT {
                    left: pr.left + px(hwnd, 4),
                    top: y,
                    right: pr.right,
                    bottom: y + px(hwnd, 14),
                },
                DT_LEFT | DT_TOP,
                muted(),
            );
        }
    }
    // Labels: title and forecast on top, dates below, the hovered value.
    let top = RECT {
        left: pr.left,
        top: rc.top + px(hwnd, 4),
        right: pr.right,
        bottom: pr.top - px(hwnd, 2),
    };
    let mut title = d.title.clone();
    if let Some(f) = d.forecast() {
        title.push_str(&format!("   ·   {f}"));
    }
    text(
        hdc,
        &title,
        &top,
        DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS,
        ink(),
    );
    let legend: Vec<String> = d.layers.iter().map(|l| l.label.clone()).collect();
    let mut lx = pr.right;
    for (li, name) in legend.iter().enumerate().rev() {
        let w = text_width(hdc, name);
        let x = lx - w;
        if x < pr.left + text_width(hdc, &title) + px(hwnd, 30) {
            break;
        }
        text(
            hdc,
            name,
            &RECT { left: x, ..top },
            DT_LEFT | DT_VCENTER,
            muted(),
        );
        let c = if li + 1 == legend.len() {
            other()
        } else {
            layer(li)
        };
        unsafe {
            let br = CreateSolidBrush(c);
            let sq = px(hwnd, 9);
            let cy = (top.top + top.bottom) / 2;
            FillRect(
                hdc,
                &RECT {
                    left: x - sq - px(hwnd, 4),
                    top: cy - sq / 2,
                    right: x - px(hwnd, 4),
                    bottom: cy + sq / 2,
                },
                br,
            );
            DeleteObject(br as _);
        }
        lx = x - px(hwnd, 22);
    }
    let bottom = RECT {
        left: pr.left,
        top: pr.bottom + px(hwnd, 2),
        right: pr.right,
        bottom: rc.bottom,
    };
    text(
        hdc,
        &day(d.times[0], false),
        &bottom,
        DT_LEFT | DT_TOP,
        muted(),
    );
    text(hdc, "now", &bottom, DT_RIGHT | DT_TOP, muted());
    if let Some(b) = d.base {
        let label = format!("compared with {}", day(d.times[b], false));
        let w = text_width(hdc, &label);
        let x = (xs[b] - w / 2).clamp(pr.left + px(hwnd, 50), pr.right - w - px(hwnd, 30));
        text(
            hdc,
            &label,
            &RECT { left: x, ..bottom },
            DT_LEFT | DT_TOP,
            accent(),
        );
    }
    if let Some(hx) = hover_x
        && let Some(i) = nearest(&d.times, hx, pr.left, pr.right)
    {
        let mut label = format!(
            "{}: {} ({} since {})",
            if i + 1 == d.times.len() {
                "now".to_string()
            } else {
                day(d.times[i], true)
            },
            fmt::size(totals[i]),
            signed_size(totals[i] as i64 - totals[0] as i64),
            day(d.times[0], false)
        );
        if let Some(f) = d.free.get(i).filter(|&&f| f != u64::MAX) {
            label.push_str(&format!(", {} free", fmt::size(*f)));
        }
        if i + 1 < d.times.len() {
            label.push_str("  (click to compare)");
        }
        let w = text_width(hdc, &label);
        let x = (xs[i] + px(hwnd, 6)).min(pr.right - w);
        let r = RECT {
            left: x - px(hwnd, 3),
            top: pr.top + px(hwnd, 2),
            right: x + w + px(hwnd, 3),
            bottom: pr.top + px(hwnd, 20),
        };
        unsafe {
            FillRect(hdc, &r, window_brush());
        }
        text(
            hdc,
            &label,
            &RECT { left: x, ..r },
            DT_LEFT | DT_VCENTER,
            ink(),
        );
    }
}

pub const TAB_MAP: u8 = 2;
pub const TAB_CALENDAR: u8 = 3;
const TABS: [(&str, i32); 4] = [
    ("Where it went", 30),
    ("Path", 17),
    ("Map", 16),
    ("Calendar", 23),
];

/// Clickable tab labels of the details pane: (rect, tab), widths in
/// proportion to their labels.
pub fn pane_tabs(hwnd: HWND, rc: &RECT) -> Vec<(RECT, u8)> {
    let y = rc.top + px(hwnd, 56);
    let h = px(hwnd, 22);
    let total = rc.right - rc.left - px(hwnd, 24);
    let weights: i32 = TABS.iter().map(|t| t.1).sum();
    let mut x = rc.left + px(hwnd, 12);
    TABS.iter()
        .enumerate()
        .map(|(i, t)| {
            let w = total * t.1 / weights;
            let r = RECT {
                left: x,
                top: y,
                right: x + w,
                bottom: y + h,
            };
            x += w;
            (r, i as u8)
        })
        .collect()
}

/// Where a tab draws its content.
fn content_area(hwnd: HWND, rc: &RECT) -> RECT {
    let m = px(hwnd, 12);
    RECT {
        left: rc.left + m,
        top: pane_tabs(hwnd, rc)[0].0.bottom + px(hwnd, 12),
        right: rc.right - m,
        bottom: rc.bottom - m,
    }
}

fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

// ---- the treemap ----

/// A laid-out box: its rect, the item and its level (0 = child of the scope).
pub struct MapBox<'a> {
    pub rect: RECT,
    pub item: &'a MapItem,
    pub level: u8,
    /// Its children are drawn inside it.
    pub opened: bool,
}

fn to_rect(r: (f64, f64, f64, f64)) -> RECT {
    RECT {
        left: r.0.round() as i32,
        top: r.1.round() as i32,
        right: (r.0 + r.2).round() as i32,
        bottom: (r.1 + r.3).round() as i32,
    }
}

/// Lays out `items` (and the children of boxes big enough to show them) in `r`.
pub fn map_layout<'a>(hwnd: HWND, items: &'a [MapItem], r: &RECT) -> Vec<MapBox<'a>> {
    let mut out = Vec::new();
    let place = |items: &'a [MapItem], r: &RECT| -> Vec<(RECT, &'a MapItem)> {
        let list: Vec<&MapItem> = items.iter().filter(|i| i.area() > 0).collect();
        let areas: Vec<f64> = list.iter().map(|i| i.area() as f64).collect();
        let rects = squarify(
            &areas,
            (
                r.left as f64,
                r.top as f64,
                (r.right - r.left) as f64,
                (r.bottom - r.top) as f64,
            ),
        );
        list.into_iter()
            .zip(rects)
            .map(|(i, q)| (to_rect(q), i))
            .collect()
    };
    let header = px(hwnd, 17);
    let pad = px(hwnd, 2);
    for (rect, item) in place(items, r) {
        let opened = !item.kids.is_empty()
            && rect.right - rect.left > px(hwnd, 48)
            && rect.bottom - rect.top > header + px(hwnd, 30);
        out.push(MapBox {
            rect,
            item,
            level: 0,
            opened,
        });
        if opened {
            let inner = RECT {
                left: rect.left + pad,
                top: rect.top + header,
                right: rect.right - pad,
                bottom: rect.bottom - pad,
            };
            for (r2, k) in place(&item.kids, &inner) {
                out.push(MapBox {
                    rect: r2,
                    item: k,
                    level: 1,
                    opened: false,
                });
            }
        }
    }
    out
}

/// The map's own area inside the content (a line at the bottom for the hover text).
fn map_rect(hwnd: HWND, area: &RECT) -> RECT {
    RECT {
        bottom: area.bottom - px(hwnd, 40),
        ..*area
    }
}

/// The deepest box under the pointer.
pub fn map_hit<'a>(hwnd: HWND, rc: &RECT, d: &'a PaneData, x: i32, y: i32) -> Option<&'a MapItem> {
    let r = map_rect(hwnd, &content_area(hwnd, rc));
    map_layout(hwnd, &d.map, &r)
        .into_iter()
        .rev()
        .find(|b| inside(&b.rect, x, y))
        .map(|b| b.item)
}

fn mix(a: COLORREF, b: COLORREF, t: f64) -> COLORREF {
    let ch = |c: COLORREF, s: u32| ((c >> s) & 0xFF) as f64;
    let m = |s: u32| ((ch(a, s) * (1.0 - t) + ch(b, s) * t).round() as u32) << s;
    m(0) | m(8) | m(16)
}

/// Red for growth, blue for shrinkage, stronger as the change nears the box's size.
fn change_color(change: i64, area: u64) -> (COLORREF, bool) {
    if area == 0 || change == 0 {
        return (neutral(), false);
    }
    let t = ((change.unsigned_abs() as f64 / area as f64) * 3.0)
        .min(1.0)
        .sqrt()
        * 0.92;
    (
        mix(
            neutral(),
            if change > 0 {
                danger_fill()
            } else {
                accent_fill()
            },
            t,
        ),
        t > 0.55,
    )
}

fn draw_map(
    hwnd: HWND,
    hdc: HDC,
    area: &RECT,
    d: &PaneData,
    hover: Option<(i32, i32)>,
    bold: HFONT,
    font: HFONT,
) {
    if d.map.is_empty() {
        text(hdc, "Nothing to map here.", area, DT_LEFT | DT_TOP, muted());
        return;
    }
    let r = map_rect(hwnd, area);
    let boxes = map_layout(hwnd, &d.map, &r);
    let hovered = hover.and_then(|(x, y)| boxes.iter().rposition(|b| inside(&b.rect, x, y)));
    unsafe {
        let gap = window_brush();
        for b in &boxes {
            let (c, dark) = change_color(b.item.change(), b.item.area());
            let br = CreateSolidBrush(c);
            FillRect(hdc, &b.rect, br);
            DeleteObject(br as _);
            FrameRect(hdc, &b.rect, gap);
            let (w, h) = (b.rect.right - b.rect.left, b.rect.bottom - b.rect.top);
            let ink = if dark { theme::p().on_strong } else { ink() };
            let lr = RECT {
                left: b.rect.left + px(hwnd, 4),
                top: b.rect.top + px(hwnd, 1),
                right: b.rect.right - px(hwnd, 3),
                bottom: b.rect.top + px(hwnd, 17),
            };
            let has_kids = b.opened;
            if b.level == 0 && (w > px(hwnd, 36) && h > px(hwnd, 15)) {
                SelectObject(hdc, bold as _);
                let mut label = b.item.name.clone();
                if b.item.change() != 0 && (has_kids || h > px(hwnd, 34)) {
                    label = format!("{label}  {}", signed_size(b.item.change()));
                }
                text(hdc, &label, &lr, DT_LEFT | DT_TOP | DT_END_ELLIPSIS, ink);
                if !has_kids && h > px(hwnd, 34) {
                    SelectObject(hdc, font as _);
                    text(
                        hdc,
                        &fmt::size(b.item.size),
                        &RECT {
                            top: lr.bottom,
                            bottom: lr.bottom + px(hwnd, 16),
                            ..lr
                        },
                        DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
                        ink,
                    );
                }
                SelectObject(hdc, font as _);
            } else if b.level == 1 && w > px(hwnd, 40) && h > px(hwnd, 16) {
                text(
                    hdc,
                    &b.item.name,
                    &lr,
                    DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
                    ink,
                );
                if h > px(hwnd, 32) && b.item.change() != 0 {
                    text(
                        hdc,
                        &signed_size(b.item.change()),
                        &RECT {
                            top: lr.bottom,
                            bottom: lr.bottom + px(hwnd, 16),
                            ..lr
                        },
                        DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
                        ink,
                    );
                }
            }
        }
        if let Some(i) = hovered {
            let b = &boxes[i];
            let pen = CreatePen(PS_SOLID, px(hwnd, 2), ink());
            let old = SelectObject(hdc, pen as _);
            let oldb = SelectObject(hdc, GetStockObject(NULL_BRUSH));
            Rectangle(hdc, b.rect.left, b.rect.top, b.rect.right, b.rect.bottom);
            SelectObject(hdc, oldb);
            SelectObject(hdc, old);
            DeleteObject(pen as _);
        }
    }
    // Below: the hovered box, or how to use the map.
    let info = RECT {
        left: area.left,
        top: r.bottom + px(hwnd, 4),
        right: area.right,
        bottom: r.bottom + px(hwnd, 22),
    };
    let info2 = RECT {
        top: info.bottom,
        bottom: info.bottom + px(hwnd, 18),
        ..info
    };
    match hovered.map(|i| boxes[i].item) {
        Some(it) => {
            unsafe { SelectObject(hdc, bold as _) };
            text(
                hdc,
                &it.name,
                &info,
                DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
                ink(),
            );
            unsafe { SelectObject(hdc, font as _) };
            let c = it.change();
            let line = if it.path.is_empty() && it.size == 0 {
                format!("{} then, gone now", fmt::size(it.before))
            } else {
                format!(
                    "{} now, {} then  ({})",
                    fmt::size(it.size),
                    fmt::size(it.before),
                    signed_size(c)
                )
            };
            text(
                hdc,
                &line,
                &info2,
                DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
                if c > 0 {
                    danger()
                } else if c < 0 {
                    accent()
                } else {
                    muted()
                },
            );
        }
        None => {
            text(
                hdc,
                "Box size: the bigger of then and now. Red grew, blue shrank.",
                &info,
                DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
                muted(),
            );
            text(
                hdc,
                "Click a folder to open it, right-click to go up.",
                &info2,
                DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
                muted(),
            );
        }
    }
}

// ---- the calendar ----

/// Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// Local (year, month, day, weekday with 0 = Monday) of a unix time.
fn local_date(t: i64) -> (i64, i64, i64, i64) {
    let s = fmt::time((t + 11_644_473_600) * 10_000_000);
    let num = |a: usize, b: usize| s.get(a..b).and_then(|x| x.parse::<i64>().ok()).unwrap_or(1);
    let (y, m, d) = (num(0, 4), num(5, 7), num(8, 10));
    (y, m, d, (days_from_civil(y, m, d) + 3).rem_euclid(7))
}

struct CalLayout {
    /// Local midnight of the first cell (a Monday).
    first: i64,
    weeks: i32,
    x0: i32,
    y0: i32,
    cell: i32,
    step: i32,
}

fn cal_layout(hwnd: HWND, area: &RECT) -> CalLayout {
    let label_w = px(hwnd, 30);
    let cell = px(hwnd, 15);
    let step = cell + px(hwnd, 3);
    let weeks = ((area.right - area.left - label_w) / step).clamp(1, 53);
    let now = rapidfind_core::history::now_unix();
    let today = rapidfind_core::history::local_midnight(now);
    let wd = local_date(today + 43_200).3;
    // Back to this week's Monday, then whole weeks.
    let monday = rapidfind_core::history::local_midnight(today - wd * 86_400 + 43_200);
    let first =
        rapidfind_core::history::local_midnight(monday - (weeks as i64 - 1) * 7 * 86_400 + 43_200);
    CalLayout {
        first,
        weeks,
        x0: area.left + label_w,
        y0: area.top + px(hwnd, 18),
        cell,
        step,
    }
}

impl CalLayout {
    /// Local midnight of the cell in column `w`, row `d`.
    fn day(&self, w: i32, d: i32) -> i64 {
        rapidfind_core::history::local_midnight(
            self.first + (w as i64 * 7 + d as i64) * 86_400 + 43_200,
        )
    }
    fn rect(&self, w: i32, d: i32) -> RECT {
        let (x, y) = (self.x0 + w * self.step, self.y0 + d * self.step);
        RECT {
            left: x,
            top: y,
            right: x + self.cell,
            bottom: y + self.cell,
        }
    }
    fn hit(&self, x: i32, y: i32) -> Option<i64> {
        (0..self.weeks)
            .flat_map(|w| (0..7).map(move |d| (w, d)))
            .find(|&(w, d)| inside(&self.rect(w, d), x, y))
            .map(|(w, d)| self.day(w, d))
    }
}

/// The day under the pointer (its local midnight), if it isn't in the future.
pub fn calendar_hit(hwnd: HWND, rc: &RECT, x: i32, y: i32) -> Option<i64> {
    let l = cal_layout(hwnd, &content_area(hwnd, rc));
    l.hit(x, y)
        .filter(|&d| d <= rapidfind_core::history::now_unix())
}

fn draw_calendar(
    hwnd: HWND,
    hdc: HDC,
    area: &RECT,
    d: &PaneData,
    hover: Option<(i32, i32)>,
    bold: HFONT,
    font: HFONT,
) {
    let l = cal_layout(hwnd, area);
    let now = rapidfind_core::history::now_unix();
    let by_day: HashMap<i64, DayChange> = d.days.iter().map(|c| (c.start, *c)).collect();
    let visible: Vec<&DayChange> = d.days.iter().filter(|c| c.start >= l.first).collect();
    let max = visible
        .iter()
        .map(|c| c.delta.unsigned_abs())
        .max()
        .unwrap_or(0)
        .max(1);
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let hovered = hover.and_then(|(x, y)| l.hit(x, y)).filter(|&t| t <= now);
    unsafe {
        // Weekday labels.
        for (row, name) in [(0, "Mon"), (2, "Wed"), (4, "Fri"), (6, "Sun")] {
            let r = l.rect(0, row);
            text(
                hdc,
                name,
                &RECT {
                    left: area.left,
                    top: r.top - px(hwnd, 2),
                    right: l.x0 - px(hwnd, 4),
                    bottom: r.bottom + px(hwnd, 2),
                },
                DT_LEFT | DT_VCENTER,
                muted(),
            );
        }
        let empty = CreatePen(PS_SOLID, 1, grid());
        let oldp = SelectObject(hdc, empty as _);
        let oldb = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let mut last_month = 0;
        for w in 0..l.weeks {
            // Month label over the column holding the month's first Monday-week.
            let (_, m, dd, _) = local_date(l.day(w, 0) + 43_200);
            if m != last_month && (dd <= 7 || w == 0) {
                let r = l.rect(w, 0);
                if w > 0 || dd <= 21 {
                    text(
                        hdc,
                        months[(m as usize).clamp(1, 12) - 1],
                        &RECT {
                            left: r.left,
                            top: area.top,
                            right: r.left + px(hwnd, 40),
                            bottom: area.top + px(hwnd, 16),
                        },
                        DT_LEFT | DT_TOP,
                        muted(),
                    );
                }
                last_month = m;
            }
            for dw in 0..7 {
                let day = l.day(w, dw);
                if day > now {
                    continue;
                }
                let r = l.rect(w, dw);
                match by_day.get(&day) {
                    Some(c) => {
                        let t = (c.delta.unsigned_abs() as f64 / max as f64).sqrt();
                        let col = if c.delta == 0 {
                            neutral()
                        } else {
                            mix(
                                neutral(),
                                if c.delta > 0 {
                                    danger_fill()
                                } else {
                                    accent_fill()
                                },
                                0.15 + 0.85 * t,
                            )
                        };
                        let br = CreateSolidBrush(col);
                        FillRect(hdc, &r, br);
                        DeleteObject(br as _);
                        if c.estimated {
                            // Spread over a longer interval: a dotted corner.
                            let dot = RECT {
                                left: r.right - px(hwnd, 5),
                                top: r.top + px(hwnd, 2),
                                right: r.right - px(hwnd, 2),
                                bottom: r.top + px(hwnd, 5),
                            };
                            FillRect(hdc, &dot, theme::brush(theme::p().on_strong));
                        }
                    }
                    None => {
                        Rectangle(hdc, r.left, r.top, r.right, r.bottom);
                    }
                }
                if Some(day) == hovered {
                    let pen = CreatePen(PS_SOLID, px(hwnd, 2), ink());
                    let o = SelectObject(hdc, pen as _);
                    Rectangle(hdc, r.left - 1, r.top - 1, r.right + 1, r.bottom + 1);
                    SelectObject(hdc, o);
                    DeleteObject(pen as _);
                }
            }
        }
        SelectObject(hdc, oldb);
        SelectObject(hdc, oldp);
        DeleteObject(empty as _);
    }
    let line = px(hwnd, 19);
    let mut y = l.y0 + 7 * l.step + px(hwnd, 10);
    let row = |y: i32| RECT {
        left: area.left,
        top: y,
        right: area.right,
        bottom: y + line,
    };
    let date = |t: i64| {
        let (_, _, _, wd) = local_date(t + 43_200);
        format!(
            "{} {}",
            ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"][wd as usize],
            day(t + 43_200, false)
        )
    };
    if let Some(h) = hovered {
        unsafe { SelectObject(hdc, bold as _) };
        text(hdc, &date(h), &row(y), DT_LEFT | DT_TOP, ink());
        unsafe { SelectObject(hdc, font as _) };
        y += line;
        let (msg, col) = match by_day.get(&h) {
            Some(c) => (
                format!(
                    "{}{}",
                    signed_size(c.delta),
                    if c.estimated {
                        "  (spread over a longer gap between snapshots)"
                    } else {
                        ""
                    }
                ),
                if c.delta > 0 {
                    danger()
                } else if c.delta < 0 {
                    accent()
                } else {
                    muted()
                },
            ),
            None => ("no snapshot covers this day".to_string(), muted()),
        };
        text(hdc, &msg, &row(y), DT_LEFT | DT_TOP | DT_END_ELLIPSIS, col);
        y += line;
        text(
            hdc,
            "Click to compare with this day",
            &row(y),
            DT_LEFT | DT_TOP,
            muted(),
        );
        return;
    }
    if visible.is_empty() {
        text(
            hdc,
            "No daily history here yet.",
            &row(y),
            DT_LEFT | DT_TOP,
            muted(),
        );
        return;
    }
    if let Some(c) = visible
        .iter()
        .filter(|c| c.delta > 0)
        .max_by_key(|c| c.delta)
    {
        text(
            hdc,
            &format!(
                "Biggest growth: {}  {}",
                date(c.start),
                signed_size(c.delta)
            ),
            &row(y),
            DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
            danger(),
        );
        y += line;
    }
    if let Some(c) = visible
        .iter()
        .filter(|c| c.delta < 0)
        .min_by_key(|c| c.delta)
    {
        text(
            hdc,
            &format!("Biggest drop: {}  {}", date(c.start), signed_size(c.delta)),
            &row(y),
            DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
            accent(),
        );
        y += line;
    }
    let week: i64 = visible
        .iter()
        .filter(|c| c.start > now - 7 * 86_400)
        .map(|c| c.delta)
        .sum();
    text(
        hdc,
        &format!("Last 7 days: {}", signed_size(week)),
        &row(y),
        DT_LEFT | DT_TOP,
        ink(),
    );
    y += line;
    if visible.iter().any(|c| c.estimated) {
        text(
            hdc,
            "White corner: a share of a longer gap between snapshots.",
            &row(y),
            DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
            muted(),
        );
        y += line;
    }
    text(
        hdc,
        "Click a day to compare with it",
        &row(y),
        DT_LEFT | DT_TOP,
        muted(),
    );
}

#[allow(clippy::too_many_arguments)]
pub fn draw_pane(
    hwnd: HWND,
    hdc: HDC,
    rc: &RECT,
    d: Option<&PaneData>,
    tab: u8,
    hover: Option<(i32, i32)>,
    font: HFONT,
    bold: HFONT,
) {
    unsafe {
        FillRect(hdc, rc, window_brush());
        let pen = CreatePen(PS_SOLID, 1, grid());
        let old = SelectObject(hdc, pen as _);
        MoveToEx(hdc, rc.left, rc.top, null_mut());
        LineTo(hdc, rc.left, rc.bottom);
        SelectObject(hdc, old);
        DeleteObject(pen as _);
        SelectObject(hdc, font as _);
    }
    let m = px(hwnd, 12);
    let inner = RECT {
        left: rc.left + m,
        top: rc.top + m,
        right: rc.right - m,
        bottom: rc.bottom - m,
    };
    let Some(d) = d else {
        text(
            hdc,
            "Select a folder to see where its space went.",
            &inner,
            DT_LEFT | DT_TOP | DT_WORDBREAK,
            muted(),
        );
        return;
    };
    let line = px(hwnd, 20);
    unsafe { SelectObject(hdc, bold as _) };
    text(
        hdc,
        &d.title,
        &RECT {
            bottom: inner.top + line,
            ..inner
        },
        DT_LEFT | DT_TOP | DT_PATH_ELLIPSIS,
        ink(),
    );
    unsafe { SelectObject(hdc, font as _) };
    let change = d.now as i64 - d.before as i64;
    let sub = format!(
        "{} since {}  ({} → {})",
        signed_size(change),
        d.since,
        fmt::size(d.before),
        fmt::size(d.now)
    );
    text(
        hdc,
        &sub,
        &RECT {
            top: inner.top + line,
            bottom: inner.top + 2 * line,
            ..inner
        },
        DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
        if change > 0 {
            danger()
        } else if change < 0 {
            accent()
        } else {
            muted()
        },
    );
    for (r, t) in pane_tabs(hwnd, rc) {
        let label = TABS[t as usize].0;
        unsafe {
            if t == tab {
                FillRect(hdc, &r, theme::brush(theme::p().face));
            }
            SelectObject(hdc, if t == tab { bold } else { font } as _);
        }
        text(
            hdc,
            label,
            &r,
            DT_CENTER | DT_VCENTER,
            if t == tab { ink() } else { muted() },
        );
    }
    unsafe { SelectObject(hdc, font as _) };
    let area = content_area(hwnd, rc);
    match tab {
        0 => draw_waterfall(hwnd, hdc, &area, d),
        1 => draw_chain(hwnd, hdc, &area, d),
        TAB_MAP => draw_map(hwnd, hdc, &area, d, hover, bold, font),
        _ => draw_calendar(hwnd, hdc, &area, d, hover, bold, font),
    }
}

/// Horizontal waterfall, top to bottom: then → each child's change → now
/// (for a drive, free space then → … → free space now).
fn draw_waterfall(hwnd: HWND, hdc: HDC, area: &RECT, d: &PaneData) {
    let free_mode = d.free.is_some();
    let (start, end) = match d.free {
        Some((then, now)) => (then as i64, now as i64),
        None => (d.before as i64, d.now as i64),
    };
    // Rows: start, parts (free space goes the other way), end.
    let mut rows: Vec<(String, i64, i64, u8)> = vec![(
        if free_mode {
            "free then".into()
        } else {
            "then".into()
        },
        0,
        start,
        0,
    )];
    let mut level = start;
    for (name, delta) in &d.parts {
        let step = if free_mode { -delta } else { *delta };
        rows.push((
            name.clone(),
            level,
            level + step,
            if *delta > 0 { 1 } else { 2 },
        ));
        level += step;
    }
    rows.push((
        if free_mode {
            "free now".into()
        } else {
            "now".into()
        },
        0,
        end,
        0,
    ));
    let lo = rows.iter().map(|r| r.1.min(r.2)).min().unwrap_or(0).min(0);
    let hi = rows.iter().map(|r| r.1.max(r.2)).max().unwrap_or(1).max(1);
    let label_w = px(hwnd, 110);
    let value_w = px(hwnd, 64);
    let (x0, x1) = (area.left + label_w, area.right - value_w);
    let xv = |v: i64| x0 + ((x1 - x0) as f64 * (v - lo) as f64 / (hi - lo).max(1) as f64) as i32;
    let row_h = px(hwnd, 24).min((area.bottom - area.top) / rows.len().max(1) as i32);
    let g = Gfx::new(hdc);
    for (i, (name, a, b, kind)) in rows.iter().enumerate() {
        let y = area.top + i as i32 * row_h;
        let (l, r) = (xv(*a.min(b)), xv(*a.max(b)));
        let c = match kind {
            0 => theme::p().ghost,
            1 => danger(),
            _ => accent(),
        };
        g.rect(
            l,
            y + px(hwnd, 4),
            (r - l).max(2),
            row_h - px(hwnd, 8),
            c,
            230,
        );
        let lr = RECT {
            left: area.left,
            top: y,
            right: x0 - px(hwnd, 6),
            bottom: y + row_h,
        };
        text(
            hdc,
            name,
            &lr,
            DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS,
            ink(),
        );
        // A part is shown as the folder's own change (in free-space mode it took that much free space).
        let v = if *kind == 0 {
            fmt::size(*b as u64)
        } else {
            signed_size(if free_mode { a - b } else { b - a })
        };
        text(
            hdc,
            &v,
            &RECT {
                left: x1 + px(hwnd, 4),
                top: y,
                right: area.right,
                bottom: y + row_h,
            },
            DT_RIGHT | DT_VCENTER,
            if *kind == 1 {
                danger()
            } else if *kind == 2 {
                accent()
            } else {
                ink()
            },
        );
    }
}

/// The chain of folders that grew most, one bar per level, width = growth.
fn draw_chain(hwnd: HWND, hdc: HDC, area: &RECT, d: &PaneData) {
    if d.chain.is_empty() {
        text(hdc, "Nothing here grew.", area, DT_LEFT | DT_TOP, muted());
        return;
    }
    let top = d.chain.iter().map(|c| c.1).max().unwrap_or(1).max(1);
    let row_h = px(hwnd, 30).min((area.bottom - area.top) / d.chain.len().max(1) as i32);
    let g = Gfx::new(hdc);
    for (i, (name, delta, size)) in d.chain.iter().enumerate() {
        let y = area.top + i as i32 * row_h;
        let w = ((area.right - area.left) as f64 * (*delta.max(&0) as f64 / top as f64)) as i32;
        let shade = (230 - (i as i32 * 18).min(110)) as u8;
        g.rect(
            area.left,
            y + px(hwnd, 2),
            w.max(px(hwnd, 3)),
            row_h - px(hwnd, 4),
            danger_fill(),
            shade,
        );
        let label = format!(
            "{name}  {}  ({} now)",
            signed_size(*delta),
            fmt::size(*size)
        );
        text(
            hdc,
            &label,
            &RECT {
                left: area.left + px(hwnd, 6),
                top: y,
                right: area.right,
                bottom: y + row_h,
            },
            DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS,
            if w > text_width(hdc, &label) + px(hwnd, 12) {
                theme::p().on_strong
            } else {
                ink()
            },
        );
    }
}

// ---- the two windows -------------------------------------------------------------------

fn paint(h: HWND, draw: impl Fn(HDC, &RECT)) {
    unsafe {
        let mut ps: PAINTSTRUCT = std::mem::zeroed();
        let hdc = BeginPaint(h, &mut ps);
        let rc = w::client_rect(h);
        // Double-buffered.
        let mem = CreateCompatibleDC(hdc);
        let bmp = CreateCompatibleBitmap(hdc, rc.right.max(1), rc.bottom.max(1));
        let old = SelectObject(mem, bmp as _);
        draw(mem, &rc);
        BitBlt(hdc, 0, 0, rc.right, rc.bottom, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old);
        DeleteObject(bmp as _);
        DeleteDC(mem);
        EndPaint(h, &ps);
    }
}

unsafe extern "system" fn strip_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(app) = crate::app::app() else {
        return unsafe { DefWindowProcW(h, msg, wp, lp) };
    };
    let x = (lp & 0xFFFF) as i16 as i32;
    match msg {
        WM_PAINT => {
            paint(h, |hdc, rc| {
                let g = app.growth.borrow();
                draw_strip(h, hdc, rc, g.strip.as_ref(), g.hover_x, app.ui_font());
            });
            0
        }
        WM_ERASEBKGND => 1,
        WM_MOUSEMOVE => {
            let first = app.growth.borrow().hover_x.is_none();
            app.growth.borrow_mut().hover_x = Some(x);
            if first {
                let mut t = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: h,
                    dwHoverTime: 0,
                };
                unsafe { TrackMouseEvent(&mut t) };
            }
            unsafe { InvalidateRect(h, null(), 0) };
            0
        }
        WM_MOUSELEAVE => {
            app.growth.borrow_mut().hover_x = None;
            unsafe { InvalidateRect(h, null(), 0) };
            0
        }
        WM_LBUTTONDOWN => {
            app.strip_click(x);
            0
        }
        WM_SETCURSOR => {
            unsafe { SetCursor(LoadCursorW(null_mut(), IDC_HAND)) };
            1
        }
        _ => unsafe { DefWindowProcW(h, msg, wp, lp) },
    }
}

unsafe extern "system" fn pane_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let Some(app) = crate::app::app() else {
        return unsafe { DefWindowProcW(h, msg, wp, lp) };
    };
    let pt = POINT {
        x: (lp & 0xFFFF) as i16 as i32,
        y: ((lp >> 16) & 0xFFFF) as i16 as i32,
    };
    match msg {
        WM_PAINT => {
            paint(h, |hdc, rc| {
                let g = app.growth.borrow();
                draw_pane(
                    h,
                    hdc,
                    rc,
                    g.pane.as_ref(),
                    g.tab,
                    g.pane_hover,
                    app.ui_font(),
                    app.ui_bold(),
                );
            });
            0
        }
        WM_ERASEBKGND => 1,
        WM_MOUSEMOVE => {
            let pt = (pt.x, pt.y);
            let mut g = app.growth.borrow_mut();
            let first = g.pane_hover.is_none();
            g.pane_hover = Some(pt);
            if first {
                let mut t = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: h,
                    dwHoverTime: 0,
                };
                unsafe { TrackMouseEvent(&mut t) };
            }
            if g.tab >= TAB_MAP {
                unsafe { InvalidateRect(h, null(), 0) };
            }
            0
        }
        WM_MOUSELEAVE => {
            app.growth.borrow_mut().pane_hover = None;
            unsafe { InvalidateRect(h, null(), 0) };
            0
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
            let rc = w::client_rect(h);
            if let Some((_, t)) = pane_tabs(h, &rc)
                .into_iter()
                .find(|(r, _)| inside(r, pt.x, pt.y))
            {
                if msg == WM_LBUTTONDOWN {
                    app.set_growth_tab(t);
                }
                return 0;
            }
            let tab = app.growth.borrow().tab;
            if tab == TAB_MAP {
                if msg == WM_RBUTTONDOWN {
                    app.map_up();
                } else {
                    let path = {
                        app.growth
                            .borrow()
                            .pane
                            .as_ref()
                            .and_then(|d| map_hit(h, &rc, d, pt.x, pt.y))
                            .map(|i| i.path.clone())
                    };
                    if let Some(p) = path.filter(|p| !p.is_empty()) {
                        app.map_open(&p);
                    }
                }
            } else if tab == TAB_CALENDAR
                && msg == WM_LBUTTONDOWN
                && let Some(day) = calendar_hit(h, &rc, pt.x, pt.y)
            {
                app.compare_day(day);
            }
            0
        }
        WM_SETCURSOR => {
            let tab = app.growth.borrow().tab;
            let mut p = POINT { x: 0, y: 0 };
            unsafe {
                GetCursorPos(&mut p);
                windows_sys::Win32::Graphics::Gdi::ScreenToClient(h, &mut p);
            }
            let rc = w::client_rect(h);
            let g = app.growth.borrow();
            let hand = pane_tabs(h, &rc).iter().any(|(r, _)| inside(r, p.x, p.y))
                || (tab == TAB_MAP
                    && g.pane
                        .as_ref()
                        .and_then(|d| map_hit(h, &rc, d, p.x, p.y))
                        .is_some_and(|i| !i.path.is_empty()))
                || (tab == TAB_CALENDAR && calendar_hit(h, &rc, p.x, p.y).is_some());
            unsafe {
                SetCursor(LoadCursorW(
                    null_mut(),
                    if hand { IDC_HAND } else { IDC_ARROW },
                ))
            };
            1
        }
        _ => unsafe { DefWindowProcW(h, msg, wp, lp) },
    }
}

/// Registers both window classes and creates the (hidden) windows.
pub fn create(parent: HWND) -> (HWND, HWND) {
    gdiplus_start();
    unsafe {
        for (class, proc) in [
            (
                STRIP_CLASS,
                strip_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
            ),
            (PANE_CLASS, pane_proc),
        ] {
            let c = wide(class);
            let wc = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: w::hinstance(),
                hIcon: null_mut(),
                hCursor: LoadCursorW(null_mut(), IDC_ARROW),
                hbrBackground: null_mut(),
                lpszMenuName: null(),
                lpszClassName: c.as_ptr(),
                hIconSm: null_mut(),
            };
            RegisterClassExW(&wc);
        }
        let strip = w::child(parent, STRIP_CLASS, "History", 0, 0, ID_STRIP);
        let pane = w::child(parent, PANE_CLASS, "Growth details", 0, 0, ID_PANE);
        ShowWindow(strip, SW_HIDE);
        ShowWindow(pane, SW_HIDE);
        (strip, pane)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forecast_from_free_space() {
        let now = rapidfind_core::history::now_unix();
        let times: Vec<i64> = (0..8).map(|i| now - (7 - i) * 2 * 86_400).collect();
        // Free space drops 1 GB a day from 20 GB: about 6 days left at the end.
        let free: Vec<u64> = (0..8)
            .map(|i| (20u64 << 30).saturating_sub((i as u64 * 2) << 30))
            .collect();
        let d = StripData {
            times: times.clone(),
            free,
            layers: vec![Layer {
                label: "x".into(),
                values: vec![1; 8],
            }],
            capacity: 100 << 30,
            ..Default::default()
        };
        let f = d.forecast().unwrap();
        assert!(
            f.starts_with("full in ~6 days") || f.starts_with("full in ~5 days"),
            "{f}"
        );
        let steady = StripData {
            free: vec![5 << 30; 8],
            ..d.clone()
        };
        assert_eq!(steady.forecast().unwrap(), "free space steady or growing");
        let growth = StripData {
            free: vec![],
            layers: vec![Layer {
                label: "x".into(),
                values: (0..8).map(|i| i as u64 * (1 << 30)).collect(),
            }],
            ..d
        };
        assert!(growth.forecast().unwrap().contains("/week"));
        assert_eq!(nearest(&times, 0, 0, 100), Some(0));
        assert_eq!(nearest(&times, 100, 0, 100), Some(7));
    }

    #[test]
    fn treemap_tiles_the_rect() {
        let areas = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0];
        let r = squarify(&areas, (10.0, 20.0, 600.0, 400.0));
        assert_eq!(r.len(), areas.len());
        let total: f64 = areas.iter().sum();
        for (q, a) in r.iter().zip(areas) {
            // Each box gets its share of the area, inside the rect.
            assert!((q.2 * q.3 - a / total * 240_000.0).abs() < 1e-6, "{q:?}");
            assert!(
                q.0 >= 10.0 - 1e-9
                    && q.1 >= 20.0 - 1e-9
                    && q.0 + q.2 <= 610.0 + 1e-9
                    && q.1 + q.3 <= 420.0 + 1e-9,
                "{q:?}"
            );
        }
        // No two overlap.
        for i in 0..r.len() {
            for j in i + 1..r.len() {
                let (a, b) = (r[i], r[j]);
                let ox = (a.0 + a.2).min(b.0 + b.2) - a.0.max(b.0);
                let oy = (a.1 + a.3).min(b.1 + b.3) - a.1.max(b.1);
                assert!(ox <= 1e-6 || oy <= 1e-6, "{a:?} {b:?}");
            }
        }
        // Squarified: no sliver worse than 3:1 for this classic example.
        assert!(r.iter().all(|q| q.2.max(q.3) / q.2.min(q.3) < 3.0), "{r:?}");
        assert_eq!(squarify(&[], (0.0, 0.0, 10.0, 10.0)), vec![]);
    }

    #[test]
    fn daily_changes() {
        let mid = rapidfind_core::history::local_midnight(rapidfind_core::history::now_unix())
            - 10 * 86_400;
        let next = |t: i64| rapidfind_core::history::local_midnight(t + 36 * 3600);
        let (d1, d2, d3) = (next(mid), next(next(mid)), next(next(next(mid))));
        // Noon to noon: half of the change lands on each day.
        let t = [mid + 43_200, d1 + 43_200, d3 + 43_200];
        let v = [1000, 3000, 3000 + 4000];
        let days = daily(&t, &v);
        assert_eq!(
            days.iter().map(|d| d.start).collect::<Vec<_>>(),
            [mid, d1, d2, d3]
        );
        assert_eq!(days.iter().map(|d| d.delta).sum::<i64>(), 6000);
        assert_eq!((days[0].delta, days[1].delta), (1000, 1000 + 1000));
        assert!(!days[0].estimated && days[2].estimated);
        // A gap in the series (folder not recorded) is skipped.
        let gap = daily(&t, &[1000, u64::MAX, 5000]);
        assert!(gap.is_empty());
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(
            (days_from_civil(2026, 10, 20) + 3).rem_euclid(7),
            1,
            "a Tuesday"
        );
    }

    #[test]
    fn days() {
        assert_eq!(day(0, false), day(0, false));
        let now = rapidfind_core::history::now_unix();
        assert_eq!(ago(now), "today");
        assert_eq!(
            ago(rapidfind_core::history::local_midnight(now) - 3600),
            "yesterday"
        );
    }
}
