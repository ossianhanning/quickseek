mod selftest;
mod service;
mod tui;

use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use rapidfind_core::config::FolderConfig;
use rapidfind_core::engine::{self, Engine};
use rapidfind_core::ipc::{self, Backend};
use rapidfind_core::ntfs::journal::Journal;
use rapidfind_core::ntfs::win;
use rapidfind_core::query::{Query, SearchFlags};
use rapidfind_core::search::{SearchOptions, SortKey, search};
use rapidfind_core::{fmt, persist};

#[derive(Parser)]
#[command(name = "rapidfind", version, about = "Instant filename search for NTFS volumes")]
struct Cli {
    /// Volumes to use, e.g. `-v C -v D` (default: all included volumes).
    #[arg(short, long, global = true)]
    volume: Vec<char>,
    /// Don't use the service; index in this process (needs administrator).
    #[arg(long, global = true)]
    local: bool,
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
    Runs,
}

impl From<Sort> for SortKey {
    fn from(s: Sort) -> Self {
        match s {
            Sort::Name => SortKey::Name,
            Sort::Path => SortKey::Path,
            Sort::Size => SortKey::Size,
            Sort::Modified => SortKey::Modified,
            Sort::Created => SortKey::Created,
            Sort::Runs => SortKey::RunCount,
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
        /// Case-sensitive.
        #[arg(long)]
        case: bool,
        /// Match terms against full paths.
        #[arg(long)]
        path: bool,
        /// Whole words.
        #[arg(long)]
        ww: bool,
        /// Treat the query as a regular expression.
        #[arg(long)]
        regex: bool,
    },
    /// Print USN journal records as they happen (debugging).
    Watch,
    /// Time a query repeatedly.
    Bench {
        query: Vec<String>,
        #[arg(short, long, default_value_t = 20)]
        runs: usize,
    },
    /// Service, pipe, volumes and index files.
    Status,
    /// Verify the index against the real filesystem and test live updates.
    Selftest,
    /// Manage the background service that lets search run unelevated.
    Service {
        #[command(subcommand)]
        action: ServiceCmd,
    },
    /// Indexed folders: non-NTFS drives and network shares.
    Folders {
        #[command(subcommand)]
        action: FolderCmd,
    },
    /// Show or replace the index configuration (excludes, volumes, ...).
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Copy this binary to %ProgramData%\rapidfind, register and start the service (admin).
    Install,
    /// Stop and remove the service (admin). Index files are kept.
    Uninstall,
    /// Replace the installed binary with this one, restarting the service (admin).
    Update,
    /// Start the installed service (admin).
    Start,
    /// Stop the service; it saves its index first (admin).
    Stop,
    /// Entry point used by the Service Control Manager.
    #[command(hide = true)]
    Run,
}

#[derive(Subcommand)]
enum FolderCmd {
    List,
    /// Add a folder or share (mapped drives are converted to UNC paths).
    Add {
        path: String,
        /// Full rescan interval in minutes (0 = only when watching overflows).
        #[arg(long, default_value_t = 0)]
        rescan_minutes: u32,
        /// Don't watch for changes (rely on rescans).
        #[arg(long)]
        no_watch: bool,
    },
    Remove {
        path: String,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the configuration as JSON.
    Show,
    /// Replace the configuration with a JSON file.
    Set { file: std::path::PathBuf },
    /// Exclude a folder (full path) or folder-name pattern.
    ExcludeFolder { pattern: String },
    /// Exclude files matching a wildcard pattern.
    ExcludeFiles { pattern: String },
}

fn open_engine(volumes: &[char], force: bool, verbose: bool) -> Engine {
    let volumes: Vec<char> = volumes.iter().map(|c| c.to_ascii_uppercase()).collect();
    Engine::open(&volumes, force, move |l, r| match r {
        Ok(rep) if verbose => eprintln!("{l}: {rep}"),
        Ok(_) => {}
        Err(e) => eprintln!("{l}: failed: {e:#}"),
    })
}

/// The service when it's running and usable, otherwise an in-process engine
/// (which needs administrator rights).
fn open_backend(cli: &Cli, verbose: bool) -> anyhow::Result<Box<dyn Backend>> {
    // The service indexes everything; a volume filter means local mode.
    if !cli.local && cli.volume.is_empty() {
        match ipc::Client::connect() {
            Ok(c) => return Ok(Box::new(ipc::Remote(c))),
            Err(e) => log::info!("service not reachable ({e}); using a local index"),
        }
    }
    if !win::is_elevated() {
        bail!(concat!(
            "the rapidfind service isn't running, and indexing in-process needs administrator rights.\n",
            "Either install the service once from an elevated terminal (`rapidfind service install`),\n",
            "or re-run this command elevated."
        ));
    }
    if verbose {
        eprintln!("loading index...");
    }
    let eng = open_engine(&cli.volume, false, verbose);
    if eng.volume_count() == 0 {
        bail!("no volumes could be indexed");
    }
    Ok(Box::new(ipc::Local::new(Arc::new(eng))))
}

fn require_admin() -> anyhow::Result<()> {
    if !win::is_elevated() {
        bail!("this command reads the NTFS master file table directly and must run as administrator.\nRe-run from an elevated terminal.");
    }
    Ok(())
}

/// Config edits go through the service when it runs, otherwise to the local
/// config file (no admin needed).
fn edit_config(cli: &Cli, f: impl FnOnce(&mut rapidfind_core::config::Config) -> anyhow::Result<()>) -> anyhow::Result<()> {
    if !cli.local
        && let Ok(c) = ipc::Client::connect()
    {
        let mut be = ipc::Remote(c);
        let mut cfg = be.get_config().map_err(anyhow::Error::msg)?;
        f(&mut cfg)?;
        return be.set_config(&cfg).map_err(anyhow::Error::msg);
    }
    let path = persist::db_dir().join("config.json");
    let mut cfg = rapidfind_core::config::Config::load(&path);
    f(&mut cfg)?;
    cfg.save(&path)?;
    eprintln!("saved {} (takes effect next time the index is opened)", path.display());
    Ok(())
}

fn show_config(cli: &Cli) -> anyhow::Result<rapidfind_core::config::Config> {
    if !cli.local
        && let Ok(c) = ipc::Client::connect()
    {
        return ipc::Remote(c).get_config().map_err(anyhow::Error::msg);
    }
    Ok(rapidfind_core::config::Config::load(&persist::db_dir().join("config.json")))
}

fn main() -> anyhow::Result<()> {
    let mut cli = Cli::parse();
    let cmd = cli.cmd.take().unwrap_or(Cmd::Ui);
    // `service run` installs its own file logger in service_main; a global logger set here
    // first would make that try_init fail and send everything to the service's absent stderr.
    if !matches!(cmd, Cmd::Service { action: ServiceCmd::Run }) {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    }
    match cmd {
        Cmd::Service { action } => match action {
            ServiceCmd::Install => service::install()?,
            ServiceCmd::Uninstall => service::uninstall()?,
            ServiceCmd::Update => service::update()?,
            ServiceCmd::Start => service::start()?,
            ServiceCmd::Stop => service::stop()?,
            ServiceCmd::Run => service::run()?,
        },
        Cmd::Status => {
            match service::query_state() {
                Ok(Some(st)) => println!("service: {st:?} ({})", service::home().display()),
                Ok(None) => println!("service: not installed"),
                Err(e) => println!("service: unknown ({e})"),
            }
            match ipc::Client::connect().and_then(|mut c| c.call(&ipc::Request::Status)) {
                Ok(ipc::Response::Status(s)) => {
                    println!("pipe {}: {}", ipc::PIPE_NAME, if s.ready { "ready" } else { "index still loading" });
                    for v in &s.volumes {
                        println!("  {:<28} {:<6} {:>10} entries  {}", v.key, v.kind, v.entries, v.state);
                    }
                }
                Ok(_) => println!("pipe {}: unexpected answer", ipc::PIPE_NAME),
                Err(e) => println!("pipe {}: not available ({e})", ipc::PIPE_NAME),
            }
            println!("local indexes ({}):", persist::db_dir().display());
            for v in win::list_volumes() {
                let db = persist::db_path(v.letter);
                let size = std::fs::metadata(&db).map(|m| fmt::size(m.len())).unwrap_or_else(|_| "-".into());
                println!("  {}:  {:<6} type {} serial {:08X}  index {}", v.letter, v.fs, v.drive_type, v.serial, size);
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
                        ix.name(ix.root),
                        ix.len_live(),
                        fmt::size(ix.memory_bytes() as u64),
                        ix.memory_breakdown(),
                        if ix.has_sizes { "" } else { " (no sizes)" }
                    );
                }
            });
            let ts = Instant::now();
            eng.save_all(1)?;
            eprintln!("saved in {} ms", ts.elapsed().as_millis());
            eprintln!("done in {} ms (index saved to {})", t.elapsed().as_millis(), persist::db_dir().display());
        }
        Cmd::Search { ref query, sort, desc, limit, long, case, path, ww, regex } => {
            let mut be = open_backend(&cli, false)?;
            let flags = SearchFlags { case, path, whole_word: ww, regex };
            let page = if limit == 0 { u32::MAX } else { limit as u32 };
            let mut res = be.search(&query.join(" "), flags, sort.into(), desc, page.min(10_000)).map_err(anyhow::Error::msg)?;
            let want = if limit == 0 { res.total } else { (limit as u64).min(res.total) };
            while (res.rows.len() as u64) < want {
                let more = be.fetch(res.rows.len() as u64, 10_000).map_err(anyhow::Error::msg)?;
                if more.rows.is_empty() {
                    break;
                }
                res.rows.extend(more.rows);
            }
            let out = std::io::stdout();
            let mut out = out.lock();
            for row in &res.rows {
                if long {
                    let size = if row.has_size || !row.is_dir { fmt::size(row.size) } else { String::new() };
                    writeln!(out, "{size:>10}  {:16}  {}", fmt::time(row.modified), row.full_path())?;
                } else {
                    writeln!(out, "{}", row.full_path())?;
                }
            }
            eprintln!("{} results ({} shown) in {:.2} ms via {}", res.total, res.rows.len(), res.elapsed_us as f64 / 1000.0, be.label());
            be.finish()?;
        }
        Cmd::Bench { ref query, runs } => {
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
        Cmd::Folders { ref action } => match action {
            FolderCmd::List => {
                for f in show_config(&cli)?.folders {
                    println!("{}  watch={} rescan={}min", f.path, f.watch, f.rescan_minutes);
                }
            }
            FolderCmd::Add { path, rescan_minutes, no_watch } => {
                let unc = win::universal_path(path);
                let path = rapidfind_core::folder::root_name(&unc);
                if !std::path::Path::new(&format!("{path}\\")).is_dir() {
                    eprintln!("warning: {path} isn't reachable right now; it will be indexed when it is");
                }
                let entry = FolderConfig { path: path.clone(), watch: !no_watch, rescan_minutes: *rescan_minutes };
                edit_config(&cli, |c| {
                    c.folders.retain(|f| !f.path.eq_ignore_ascii_case(&path));
                    c.folders.push(entry);
                    Ok(())
                })?;
                println!("added {path}");
            }
            FolderCmd::Remove { path } => {
                let path = rapidfind_core::folder::root_name(&win::universal_path(path));
                edit_config(&cli, |c| {
                    let before = c.folders.len();
                    c.folders.retain(|f| !f.path.eq_ignore_ascii_case(&path));
                    if c.folders.len() == before {
                        bail!("{path} isn't an indexed folder");
                    }
                    Ok(())
                })?;
                println!("removed {path}");
            }
        },
        Cmd::Config { ref action } => match action {
            ConfigCmd::Show => println!("{}", serde_json::to_string_pretty(&show_config(&cli)?)?),
            ConfigCmd::Set { file } => {
                let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
                let new: rapidfind_core::config::Config = serde_json::from_str(&text)?;
                edit_config(&cli, |c| {
                    *c = new;
                    Ok(())
                })?;
            }
            ConfigCmd::ExcludeFolder { pattern } => edit_config(&cli, |c| {
                c.exclude_folders.push(pattern.clone());
                Ok(())
            })?,
            ConfigCmd::ExcludeFiles { pattern } => edit_config(&cli, |c| {
                c.exclude_files.push(pattern.clone());
                Ok(())
            })?,
        },
        Cmd::Ui => {
            let mut be = open_backend(&cli, true)?;
            let res = tui::run(be.as_mut());
            be.finish().context("saving index")?;
            res?;
        }
    }
    Ok(())
}
