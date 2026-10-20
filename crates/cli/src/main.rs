mod selftest;
mod tui;

use std::io::Write;
use std::time::Instant;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use rapidfind_core::engine::{self, Engine};
use rapidfind_core::ntfs::journal::Journal;
use rapidfind_core::ntfs::win;
use rapidfind_core::query::Query;
use rapidfind_core::search::{SearchOptions, SortKey, search};
use rapidfind_core::{fmt, persist};

#[derive(Parser)]
#[command(name = "rapidfind", version, about = "Instant filename search for NTFS volumes")]
struct Cli {
    /// Volumes to use, e.g. `-v C -v D` (default: all NTFS volumes).
    #[arg(short, long, global = true)]
    volume: Vec<char>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Sort {
    Name,
    Path,
    Size,
    Modified,
    Created,
}

impl From<Sort> for SortKey {
    fn from(s: Sort) -> Self {
        match s {
            Sort::Name => SortKey::Name,
            Sort::Path => SortKey::Path,
            Sort::Size => SortKey::Size,
            Sort::Modified => SortKey::Modified,
            Sort::Created => SortKey::Created,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Interactive search-as-you-type (default).
    Ui,
    /// Build (or refresh) the index and save it.
    Scan {
        /// Ignore the saved index and rescan the MFT.
        #[arg(long)]
        force: bool,
    },
    /// Run one query and print matching paths.
    Search {
        query: Vec<String>,
        #[arg(short = 's', long, value_enum, default_value_t = Sort::Name)]
        sort: Sort,
        #[arg(short, long)]
        desc: bool,
        /// Max results (0 = all).
        #[arg(short = 'n', long, default_value_t = 100)]
        limit: usize,
        /// Also print size and modified date.
        #[arg(short, long)]
        long: bool,
    },
    /// Print USN journal records as they happen (debugging).
    Watch,
    /// Time a query repeatedly.
    Bench {
        query: Vec<String>,
        #[arg(short, long, default_value_t = 20)]
        runs: usize,
    },
    /// List volumes and index files.
    Status,
    /// Verify the index against the real filesystem and test live updates.
    Selftest,
}

fn require_admin() -> anyhow::Result<()> {
    if !win::is_elevated() {
        bail!("rapidfind reads the NTFS master file table directly and must run as administrator.\nRe-run from an elevated terminal.");
    }
    Ok(())
}

fn open_engine(volumes: &[char], force: bool, verbose: bool) -> Engine {
    let volumes: Vec<char> = volumes.iter().map(|c| c.to_ascii_uppercase()).collect();
    Engine::open(&volumes, force, |l, r| match r {
        Ok(rep) if verbose => eprintln!("{l}: {rep}"),
        Ok(_) => {}
        Err(e) => eprintln!("{l}: failed: {e:#}"),
    })
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cli = Cli::parse();
    let cmd = cli.cmd.unwrap_or(Cmd::Ui);
    match cmd {
        Cmd::Status => {
            for v in win::list_volumes() {
                let db = persist::db_path(v.letter);
                let size = std::fs::metadata(&db).map(|m| fmt::size(m.len())).unwrap_or_else(|_| "-".into());
                println!("{}:  {:<6} serial {:08X}  index {}  ({})", v.letter, v.fs, v.serial, db.display(), size);
            }
            println!("elevated: {}", win::is_elevated());
        }
        Cmd::Scan { force } => {
            require_admin()?;
            let t = Instant::now();
            let eng = open_engine(&cli.volume, force, true);
            eng.with_indexes(|ixs| {
                for ix in ixs {
                    eprintln!(
                        "{}: {} entries, {} in memory ({}){}",
                        ix.letter,
                        ix.len_live(),
                        fmt::size(ix.memory_bytes() as u64),
                        ix.memory_breakdown(),
                        if ix.has_sizes { "" } else { " (no sizes: fallback scan)" }
                    );
                }
            });
            let ts = Instant::now();
            eng.save_all(1)?;
            eprintln!("saved in {} ms", ts.elapsed().as_millis());
            eprintln!("done in {} ms (index saved to {})", t.elapsed().as_millis(), persist::db_dir().display());
        }
        Cmd::Search { query, sort, desc, limit, long } => {
            require_admin()?;
            let eng = open_engine(&cli.volume, false, false);
            let q = Query::parse(&query.join(" ")).map_err(anyhow::Error::msg)?;
            let hits = eng.search(&q, &SearchOptions { sort: sort.into(), descending: desc, limit });
            let out = std::io::stdout();
            let mut out = out.lock();
            eng.with_indexes(|ixs| -> std::io::Result<()> {
                for h in &hits.items {
                    let ix = ixs[h.vol as usize];
                    let path = ix.path(h.idx);
                    if long {
                        let e = &ix.entries[h.idx as usize];
                        writeln!(out, "{:>10}  {:16}  {}", fmt::size(e.size), fmt::time(e.modified), path)?;
                    } else {
                        writeln!(out, "{path}")?;
                    }
                }
                Ok(())
            })?;
            eprintln!("{} results ({} shown) in {:.2} ms", hits.total, hits.items.len(), hits.elapsed.as_secs_f64() * 1000.0);
            eng.save_all(engine::SAVE_THRESHOLD)?;
        }
        Cmd::Bench { query, runs } => {
            require_admin()?;
            let eng = open_engine(&cli.volume, false, true);
            let q = Query::parse(&query.join(" ")).map_err(anyhow::Error::msg)?;
            let mut times = Vec::new();
            let mut total = 0;
            for _ in 0..runs.max(1) {
                let h = eng.with_indexes(|ixs| search(ixs, &q, &SearchOptions::default()));
                total = h.total;
                times.push(h.elapsed.as_secs_f64() * 1000.0);
            }
            times.sort_by(f64::total_cmp);
            let entries: usize = eng.with_indexes(|ixs| ixs.iter().map(|i| i.len_live()).sum());
            eprintln!(
                "{total} of {entries} entries matched; min {:.2} ms, median {:.2} ms, max {:.2} ms over {} runs",
                times[0],
                times[times.len() / 2],
                times[times.len() - 1],
                times.len()
            );
            eng.save_all(engine::SAVE_THRESHOLD)?;
        }
        Cmd::Watch => {
            require_admin()?;
            let letter = cli.volume.first().copied().unwrap_or('C').to_ascii_uppercase();
            let mut j = Journal::open(letter, None, None).map_err(|e| anyhow::anyhow!("{e}"))?;
            eprintln!("watching {letter}: (journal {:#x}); Ctrl+C to stop", j.journal_id);
            loop {
                for r in j.read(true).map_err(|e| anyhow::anyhow!("{e}"))? {
                    println!("{:>12} rec {:>8} parent {:>8} reason {:08x} attrs {:05x} {}", r.usn, r.record(), r.parent_record(), r.reason, r.attrs, r.name);
                }
            }
        }
        Cmd::Selftest => {
            require_admin()?;
            selftest::run(cli.volume.first().copied().unwrap_or('C').to_ascii_uppercase())?;
        }
        Cmd::Ui => {
            require_admin()?;
            eprintln!("loading index...");
            let eng = open_engine(&cli.volume, false, true);
            if eng.volumes.is_empty() {
                bail!("no NTFS volumes could be indexed");
            }
            let res = tui::run(&eng);
            eng.save_all(engine::SAVE_THRESHOLD).context("saving index")?;
            res?;
        }
    }
    Ok(())
}
