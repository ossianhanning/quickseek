//! Minimal search-as-you-type terminal UI.

use std::io::{Write, stdout};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{self, ClearType};
use crossterm::{cursor, execute, queue};
use quickseek_core::fmt;
use quickseek_core::search::SortKey;

use quickseek_core::ipc::Backend;
use quickseek_core::query::SearchFlags;

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
    generation: Option<u64>,
    source: &'static str,
}

fn truncate(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return format!("{s:w$}");
    }
    let mut t: String = s.chars().take(w.saturating_sub(1)).collect();
    t.push('…');
    t
}

fn run_query(be: &mut dyn Backend, st: &mut State, height: usize) {
    let limit = st.scroll + height;
    match be.search(
        &st.query,
        SearchFlags::default(),
        st.sort,
        st.desc,
        limit as u32,
    ) {
        Ok(page) => {
            st.error = None;
            st.generation = Some(page.generation);
            st.total = page.total as usize;
            st.ms = page.elapsed_us as f64 / 1000.0;
            st.rows = page
                .rows
                .into_iter()
                .map(|r| Row {
                    size: if r.is_dir && !r.has_size {
                        String::new()
                    } else {
                        fmt::size(r.size)
                    },
                    modified: fmt::time(r.modified),
                    full: r.full_path(),
                    name: r.name,
                    dir: r.dir,
                })
                .collect();
        }
        Err(e) => {
            st.error = Some(e);
            st.generation = None;
        }
    }
    if st.selected >= st.total.max(1) {
        st.selected = st.total.saturating_sub(1);
    }
}

fn draw(st: &State, w: u16, h: u16) -> std::io::Result<()> {
    let mut out = stdout();
    let w = w as usize;
    queue!(out, cursor::MoveTo(0, 0), terminal::Clear(ClearType::All))?;
    queue!(
        out,
        SetAttribute(Attribute::Bold),
        Print("> "),
        SetAttribute(Attribute::Reset),
        Print(&st.query)
    )?;
    let sort = format!("{:?}{}", st.sort, if st.desc { " ↓" } else { " ↑" });
    let status = match &st.error {
        Some(e) => format!("error: {e}"),
        None => format!(
            "{} results in {:.1} ms ({}) · sort {sort} · Tab sort  Ctrl+R reverse  Enter reveal  Esc quit",
            st.total, st.ms, st.source
        ),
    };
    queue!(
        out,
        cursor::MoveTo(0, 1),
        SetAttribute(Attribute::Dim),
        Print(truncate(&status, w)),
        SetAttribute(Attribute::Reset)
    )?;

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
            queue!(
                out,
                SetAttribute(Attribute::Reverse),
                Print(truncate(&line, w)),
                SetAttribute(Attribute::Reset)
            )?;
        } else {
            queue!(out, Print(truncate(&line, w)))?;
        }
    }
    queue!(
        out,
        cursor::MoveTo((2 + st.query.chars().count()) as u16, 0)
    )?;
    out.flush()
}

pub fn run(be: &mut dyn Backend) -> anyhow::Result<()> {
    terminal::enable_raw_mode()?;
    execute!(stdout(), terminal::EnterAlternateScreen)?;
    let res = run_inner(be);
    execute!(stdout(), terminal::LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;
    res
}

fn run_inner(be: &mut dyn Backend) -> anyhow::Result<()> {
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
        generation: None,
        source: be.label(),
    };
    let (mut w, mut h) = terminal::size()?;
    let page = |h: u16| (h as usize).saturating_sub(2).max(1);
    run_query(be, &mut st, page(h));
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
                                SortKey::Created => SortKey::RunCount,
                                SortKey::RunCount => SortKey::Name,
                            };
                            st.desc = matches!(
                                st.sort,
                                SortKey::Size
                                    | SortKey::Modified
                                    | SortKey::Created
                                    | SortKey::RunCount
                            );
                            requery = true;
                        }
                        KeyCode::Down => {
                            st.selected = (st.selected + 1).min(st.total.saturating_sub(1))
                        }
                        KeyCode::Up => st.selected = st.selected.saturating_sub(1),
                        KeyCode::PageDown => {
                            st.selected = (st.selected + page(h)).min(st.total.saturating_sub(1))
                        }
                        KeyCode::PageUp => st.selected = st.selected.saturating_sub(page(h)),
                        KeyCode::Home => st.selected = 0,
                        KeyCode::Enter => {
                            if let Some(row) = st.rows.get(st.selected) {
                                let _ = std::process::Command::new("explorer")
                                    .arg(format!("/select,{}", row.full))
                                    .spawn();
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
                run_query(be, &mut st, vis);
            }
            draw(&st, w, h)?;
        } else if last_poll.elapsed() > Duration::from_millis(500) {
            // Live updates (and the service finishing its initial load):
            // re-run when the index generation changed.
            last_poll = Instant::now();
            if be.generation() != st.generation {
                run_query(be, &mut st, page(h));
                draw(&st, w, h)?;
            }
        }
    }
}
