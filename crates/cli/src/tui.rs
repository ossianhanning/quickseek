//! Minimal search-as-you-type terminal UI.

use std::io::{Write, stdout};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{self, ClearType};
use crossterm::{cursor, execute, queue};
use rapidfind_core::engine::Engine;
use rapidfind_core::fmt;
use rapidfind_core::query::Query;
use rapidfind_core::search::{Hits, SearchOptions, SortKey};

struct Row {
    name: String,
    dir: String,
    size: String,
    modified: String,
    full: String,
}

struct State {
    query: String,
    sort: SortKey,
    desc: bool,
    selected: usize,
    scroll: usize,
    total: usize,
    ms: f64,
    error: Option<String>,
    rows: Vec<Row>,
    generation: u64,
}

fn truncate(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return format!("{s:w$}");
    }
    let mut t: String = s.chars().take(w.saturating_sub(1)).collect();
    t.push('…');
    t
}

fn run_query(eng: &Engine, st: &mut State, height: usize) {
    st.generation = eng.generation();
    let q = match Query::parse(&st.query) {
        Ok(q) => q,
        Err(e) => {
            st.error = Some(e);
            return;
        }
    };
    st.error = None;
    let limit = st.scroll + height;
    let hits: Hits = eng.search(&q, &SearchOptions { sort: st.sort, descending: st.desc, limit });
    st.total = hits.total;
    st.ms = hits.elapsed.as_secs_f64() * 1000.0;
    st.rows = eng.with_indexes(|ixs| {
        hits.items
            .iter()
            .map(|h| {
                let ix = ixs[h.vol as usize];
                let e = &ix.entries[h.idx as usize];
                Row {
                    name: ix.name(h.idx).to_string(),
                    dir: ix.parent_path(h.idx),
                    size: if e.is_dir() && !ix.has_sizes { String::new() } else { fmt::size(e.size) },
                    modified: fmt::time(e.modified),
                    full: ix.path(h.idx),
                }
            })
            .collect()
    });
    if st.selected >= st.total.max(1) {
        st.selected = st.total.saturating_sub(1);
    }
}

fn draw(st: &State, w: u16, h: u16) -> std::io::Result<()> {
    let mut out = stdout();
    let w = w as usize;
    queue!(out, cursor::MoveTo(0, 0), terminal::Clear(ClearType::All))?;
    queue!(out, SetAttribute(Attribute::Bold), Print("> "), SetAttribute(Attribute::Reset), Print(&st.query))?;
    let sort = format!("{:?}{}", st.sort, if st.desc { " ↓" } else { " ↑" });
    let status = match &st.error {
        Some(e) => format!("error: {e}"),
        None => format!("{} results in {:.1} ms · sort {sort} · Tab sort  Ctrl+R reverse  Enter reveal  Esc quit", st.total, st.ms),
    };
    queue!(out, cursor::MoveTo(0, 1), SetAttribute(Attribute::Dim), Print(truncate(&status, w)), SetAttribute(Attribute::Reset))?;

    let (size_w, date_w) = (10usize, 16usize);
    let rest = w.saturating_sub(size_w + date_w + 4);
    let name_w = (rest * 2 / 5).max(10);
    let dir_w = rest.saturating_sub(name_w);
    let visible = (h as usize).saturating_sub(2);
    for (k, row) in st.rows.iter().skip(st.scroll).take(visible).enumerate() {
        let line = format!(
            "{} {} {:>size_w$} {}",
            truncate(&row.name, name_w),
            truncate(&row.dir, dir_w),
            row.size,
            truncate(&row.modified, date_w)
        );
        queue!(out, cursor::MoveTo(0, (k + 2) as u16))?;
        if st.scroll + k == st.selected {
            queue!(out, SetAttribute(Attribute::Reverse), Print(truncate(&line, w)), SetAttribute(Attribute::Reset))?;
        } else {
            queue!(out, Print(truncate(&line, w)))?;
        }
    }
    queue!(out, cursor::MoveTo((2 + st.query.chars().count()) as u16, 0))?;
    out.flush()
}

pub fn run(eng: &Engine) -> anyhow::Result<()> {
    terminal::enable_raw_mode()?;
    execute!(stdout(), terminal::EnterAlternateScreen)?;
    let res = run_inner(eng);
    execute!(stdout(), terminal::LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;
    res
}

fn run_inner(eng: &Engine) -> anyhow::Result<()> {
    let mut st = State {
        query: String::new(),
        sort: SortKey::Name,
        desc: false,
        selected: 0,
        scroll: 0,
        total: 0,
        ms: 0.0,
        error: None,
        rows: Vec::new(),
        generation: 0,
    };
    let (mut w, mut h) = terminal::size()?;
    let page = |h: u16| (h as usize).saturating_sub(2).max(1);
    run_query(eng, &mut st, page(h));
    draw(&st, w, h)?;
    let mut last_poll = Instant::now();
    loop {
        if event::poll(Duration::from_millis(100))? {
            let mut requery = false;
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => {
                    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                    match k.code {
                        KeyCode::Esc => return Ok(()),
                        KeyCode::Char('c') if ctrl => return Ok(()),
                        KeyCode::Char('u') if ctrl => {
                            st.query.clear();
                            requery = true;
                        }
                        KeyCode::Char('r') if ctrl => {
                            st.desc = !st.desc;
                            requery = true;
                        }
                        KeyCode::Char(c) => {
                            st.query.push(c);
                            requery = true;
                        }
                        KeyCode::Backspace => {
                            st.query.pop();
                            requery = true;
                        }
                        KeyCode::Tab => {
                            st.sort = match st.sort {
                                SortKey::Name => SortKey::Path,
                                SortKey::Path => SortKey::Size,
                                SortKey::Size => SortKey::Modified,
                                SortKey::Modified => SortKey::Created,
                                SortKey::Created => SortKey::Name,
                            };
                            st.desc = matches!(st.sort, SortKey::Size | SortKey::Modified | SortKey::Created);
                            requery = true;
                        }
                        KeyCode::Down => st.selected = (st.selected + 1).min(st.total.saturating_sub(1)),
                        KeyCode::Up => st.selected = st.selected.saturating_sub(1),
                        KeyCode::PageDown => st.selected = (st.selected + page(h)).min(st.total.saturating_sub(1)),
                        KeyCode::PageUp => st.selected = st.selected.saturating_sub(page(h)),
                        KeyCode::Home => st.selected = 0,
                        KeyCode::Enter => {
                            if let Some(row) = st.rows.get(st.selected) {
                                let _ = std::process::Command::new("explorer").arg(format!("/select,{}", row.full)).spawn();
                            }
                        }
                        _ => {}
                    }
                    if requery {
                        st.selected = 0;
                        st.scroll = 0;
                    }
                }
                Event::Resize(nw, nh) => {
                    (w, h) = (nw, nh);
                    requery = true;
                }
                _ => {}
            }
            // Keep the selection on screen; fetch more rows when scrolling down.
            let vis = page(h);
            if st.selected < st.scroll {
                st.scroll = st.selected;
            } else if st.selected >= st.scroll + vis {
                st.scroll = st.selected + 1 - vis;
            }
            if requery || st.scroll + vis > st.rows.len() && st.rows.len() < st.total {
                run_query(eng, &mut st, vis);
            }
            draw(&st, w, h)?;
        } else if last_poll.elapsed() > Duration::from_millis(500) {
            // Live updates: re-run when the journal changed something.
            last_poll = Instant::now();
            if eng.generation() != st.generation {
                run_query(eng, &mut st, page(h));
                draw(&st, w, h)?;
            }
        }
    }
}
