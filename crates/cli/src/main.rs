mod selftest;
mod service;
mod tui;

use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use quickseek_core::config::FolderConfig;
use quickseek_core::engine::{self, Engine};
use quickseek_core::ipc::{self, Backend};
use quickseek_core::ntfs::journal::Journal;
use quickseek_core::ntfs::win;
use quickseek_core::query::{Query, SearchFlags};
use quickseek_core::search::{SearchOptions, SortKey, search};
use quickseek_core::{content, fmt, persist};

#[derive(Parser)]
#[command(
    name = "quickseek",
    version,
    about = "Instant filename search for NTFS volumes"
)]
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
        /// Content search (`content:`): skip files larger than this many MB.
        #[arg(long, default_value_t = 64)]
        max_size_mb: u64,
        /// Content search: also search files that look binary.
        #[arg(long)]
        binary: bool,
        /// Content search: read documents raw instead of through their IFilter.
        #[arg(long)]
        no_ifilter: bool,
    },
    /// Disk usage: the biggest folders and files under a path (all drives by default).
    Du {
        /// Folder to break down, e.g. `C:\Users` (default: every indexed volume).
        path: Option<String>,
        /// Count only what matches this query, e.g. `*.mp4` or `folder: ww:node_modules`.
        #[arg(short, long)]
        query: Option<String>,
        /// Levels to expand.
        #[arg(short, long, default_value_t = 1)]
        depth: usize,
        /// Entries per folder.
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
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
    /// NAS agents (quickseek-agent running on a NAS): pair, list, remove.
    Remotes {
        #[command(subcommand)]
        action: RemoteCmd,
    },
    /// Show or replace the index configuration (excludes, volumes, ...).
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
    /// Folder-size history: snapshots and what grew since one.
    History {
        #[command(subcommand)]
        action: HistoryCmd,
    },
    /// Connect quickseek-mcp to AI assistants (Claude Code/Desktop, Cursor, VS Code, Windsurf). No admin needed.
    Mcp {
        #[command(subcommand)]
        action: McpCmd,
    },
}

#[derive(Subcommand)]
enum HistoryCmd {
    /// Snapshots kept (one a day, thinned out over time; save points kept a year).
    List,
    /// Take a snapshot now, e.g. before a cleanup.
    Save { name: Vec<String> },
    /// What grew (or shrank) since a snapshot, biggest change first.
    Growth {
        /// Folder (default: every drive).
        path: Option<String>,
        /// Days back (picks the snapshot closest to then).
        #[arg(long, default_value_t = 7)]
        days: u32,
        #[arg(short = 'n', long, default_value_t = 15)]
        limit: usize,
    },
}

#[derive(Subcommand)]
enum McpCmd {
    /// Register with every detected client (or only `--client ...`). Copies
    /// quickseek-mcp.exe to %LOCALAPPDATA%\Programs\quickseek first unless it
    /// already lives in a stable folder.
    Install {
        /// claude-code, claude-desktop, cursor, vscode, vscode-insiders, windsurf
        #[arg(short, long)]
        client: Vec<String>,
        /// quickseek-mcp.exe to register (default: the one next to this program).
        #[arg(long)]
        exe: Option<std::path::PathBuf>,
    },
    /// Remove the registration (from every client, or only `--client ...`).
    Uninstall {
        #[arg(short, long)]
        client: Vec<String>,
    },
    /// Which clients are installed and where Quickseek is registered.
    Status,
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Copy this binary to %ProgramData%\quickseek, register and start the service (admin).
    /// Run from outside the install folder (a development build), it also
    /// updates the programs there, so they all stay the same build.
    Install,
    /// Stop and remove the service (admin). Index files are kept.
    Uninstall,
    /// Replace the installed binary with this one, restarting the service (admin).
    Update,
    /// Start the installed service (admin).
    Start,
    /// Stop the service; it saves its index first (admin).
    Stop,
    /// Install, or update if already installed (used by the installer).
    #[command(hide = true)]
    InstallOrUpdate,
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
        /// Sign in to the share as this user (DOMAIN\name, name@domain, or a
        /// name on the server); asks for the password.
        #[arg(long)]
        user: Option<String>,
    },
    /// Remove a folder (and its login).
    Remove {
        path: String,
    },
    /// Sign in to a share with its own user name and password (asked for,
    /// never on the command line), instead of yours.
    Login {
        path: String,
        /// DOMAIN\name, name@domain, or a name on the server.
        #[arg(long)]
        user: String,
    },
    /// Forget a share's login (it's reached as you again).
    Logout {
        path: String,
    },
}

#[derive(Subcommand)]
enum RemoteCmd {
    /// Agents and their shares, with each share's state.
    List,
    /// Pair with the agent on HOST and index its shares as \\HOST\SHARE volumes.
    Add {
        /// Host name or address of the NAS (hearth.local).
        host: String,
        /// The pairing code the agent shows (its log, or `quickseek-agent pair`); asked for if left out.
        #[arg(long)]
        pair: Option<String>,
        #[arg(long, default_value_t = quickseek_remote::DEFAULT_PORT)]
        port: u16,
    },
    /// Forget an agent; a folder index of the same share comes back if you had one.
    Remove { host: String },
    /// Turn one share of an agent off or on (by its UNC root).
    Enable {
        unc: String,
        #[arg(long, action = clap::ArgAction::Set, default_value_t = true)]
        on: bool,
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
    /// Store date created (8 bytes/file; `dc:` and that sort). Turning it on rescans once.
    IndexCreated {
        #[arg(action = clap::ArgAction::Set)]
        on: bool,
    },
    /// Keep a precomputed rank for a sort key (4 bytes/file).
    FastSort {
        #[arg(value_parser = ["size", "modified", "path"])]
        key: String,
        #[arg(action = clap::ArgAction::Set)]
        on: bool,
    },
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
fn remotes(cli: &Cli, action: &RemoteCmd) -> anyhow::Result<()> {
    use serde_json::{Value, json};
    let mut be = open_backend(cli, false)?;
    let call = |be: &mut Box<dyn Backend>, a: Value| be.remote(&a).map_err(anyhow::Error::msg);
    match action {
        RemoteCmd::List => {
            let list = call(&mut be, json!({ "op": "list" }))?;
            let list = list.as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!(
                    "no NAS agents; add one with `quickseek remotes add <host> --pair <code>`"
                );
            }
            for r in list {
                let st = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_string();
                println!(
                    "{}:{}  agent {}{}  certificate {}",
                    st(&r, "host"),
                    r["port"],
                    st(&r, "agent"),
                    if r["paired"].as_bool() == Some(true) {
                        ""
                    } else {
                        " (not paired)"
                    },
                    st(&r, "fingerprint")
                );
                for sh in r["shares"].as_array().into_iter().flatten() {
                    println!(
                        "  {:<28} {:<20} {:>10} entries  {}",
                        st(sh, "unc"),
                        st(sh, "state"),
                        sh["entries"].as_u64().unwrap_or(0),
                        st(sh, "root")
                    );
                }
            }
        }
        RemoteCmd::Add { host, pair, port } => {
            let code = match pair {
                Some(c) => c.clone(),
                None => {
                    eprint!("Pairing code from the agent (its log, or `quickseek-agent pair`): ");
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    line.trim().to_string()
                }
            };
            let r = call(
                &mut be,
                json!({ "op": "add", "host": host, "port": port, "code": code }),
            )?;
            println!(
                "paired with {} ({host})",
                r["agent"].as_str().unwrap_or("?")
            );
            for sh in r["shares"].as_array().into_iter().flatten() {
                println!(
                    "  {}  ←  {}",
                    sh["unc"].as_str().unwrap_or(""),
                    sh["root"].as_str().unwrap_or("")
                );
            }
            println!("`quickseek remotes list` shows when they're ready.");
        }
        RemoteCmd::Remove { host } => {
            call(&mut be, json!({ "op": "remove", "host": host }))?;
            println!("removed {host} (`quickseek-agent revoke` on the NAS forgets this PC there)");
        }
        RemoteCmd::Enable { unc, on } => {
            call(
                &mut be,
                json!({ "op": "enable", "unc": unc, "enabled": on }),
            )?;
            println!("{unc}: {}", if *on { "on" } else { "off" });
        }
    }
    be.finish()?;
    Ok(())
}

fn open_backend(cli: &Cli, verbose: bool) -> anyhow::Result<Box<dyn Backend>> {
    // The service indexes everything; a volume filter means local mode.
    if !cli.local && cli.volume.is_empty() {
        match ipc::Client::connect() {
            Ok(c) => return Ok(Box::new(ipc::Remote::new(c))),
            Err(e) => log::info!("service not reachable ({e}); using a local index"),
        }
    }
    if !win::is_elevated() {
        bail!(concat!(
            "the Quickseek service isn't running, and indexing in-process needs administrator rights.\n",
            "Either install the service once from an elevated terminal (`quickseek service install`),\n",
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
        bail!(
            "this command reads the NTFS master file table directly and must run as administrator.\nRe-run from an elevated terminal."
        );
    }
    Ok(())
}

/// Config edits go through the service when it runs, otherwise to the local
/// config file (no admin needed).
fn edit_config(
    cli: &Cli,
    f: impl FnOnce(&mut quickseek_core::config::Config) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    if !cli.local
        && let Ok(c) = ipc::Client::connect()
    {
        let mut be = ipc::Remote::new(c);
        let mut cfg = be.get_config().map_err(anyhow::Error::msg)?;
        f(&mut cfg)?;
        return be.set_config(&cfg).map_err(anyhow::Error::msg);
    }
    let path = persist::db_dir().join("config.json");
    let mut cfg = quickseek_core::config::Config::load(&path);
    f(&mut cfg)?;
    cfg.save(&path)?;
    eprintln!(
        "saved {} (takes effect next time the index is opened)",
        path.display()
    );
    Ok(())
}

/// The service (folder logins are kept only there).
fn service(cli: &Cli) -> anyhow::Result<ipc::Remote> {
    if cli.local {
        bail!("folder logins are kept by the Quickseek service; leave out --local");
    }
    let c = ipc::Client::connect()
        .context("folder logins are kept by the Quickseek service, which isn't running")?;
    Ok(ipc::Remote::new(c))
}

/// Asks for `user`'s password and has the service sign in to `path` with it.
fn folder_login(cli: &Cli, path: &str, user: &str) -> anyhow::Result<()> {
    let mut be = service(cli)?;
    let password = read_password(&format!("Password for {user} on {path}: "))?;
    be.folder_login(path, user, password)
        .map_err(anyhow::Error::msg)?;
    println!("{path}: signs in as {user}");
    Ok(())
}

/// Reads a line from the console without showing it.
fn read_password(prompt: &str) -> anyhow::Result<quickseek_core::credstore::Secret> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, read};
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        bail!(
            "the password is typed into the console: run this in PowerShell, Command Prompt \
             or Windows Terminal (from Git Bash: winpty quickseek ...)"
        );
    }
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    crossterm::terminal::enable_raw_mode()?;
    let mut pw = String::new();
    let result = loop {
        match read() {
            Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => match k.code {
                KeyCode::Enter => break Ok(()),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err(anyhow::anyhow!("cancelled"));
                }
                KeyCode::Esc => break Err(anyhow::anyhow!("cancelled")),
                KeyCode::Backspace => {
                    pw.pop();
                }
                KeyCode::Char(c) => pw.push(c),
                _ => {}
            },
            Ok(Event::Paste(t)) => pw.push_str(&t),
            Ok(_) => {}
            Err(e) => break Err(e.into()),
        }
    };
    let _ = crossterm::terminal::disable_raw_mode();
    eprintln!();
    let pw = quickseek_core::credstore::Secret::new(pw);
    result?;
    if pw.is_empty() {
        bail!("no password given");
    }
    Ok(pw)
}

fn show_config(cli: &Cli) -> anyhow::Result<quickseek_core::config::Config> {
    if !cli.local
        && let Ok(c) = ipc::Client::connect()
    {
        return ipc::Remote::new(c).get_config().map_err(anyhow::Error::msg);
    }
    Ok(quickseek_core::config::Config::load(
        &persist::db_dir().join("config.json"),
    ))
}

fn main() -> anyhow::Result<()> {
    let mut cli = Cli::parse();
    let cmd = cli.cmd.take().unwrap_or(Cmd::Ui);
    // `service run` installs its own file logger in service_main; a global logger set here
    // first would make that try_init fail and send everything to the service's absent stderr.
    if !matches!(
        cmd,
        Cmd::Service {
            action: ServiceCmd::Run
        }
    ) {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    }
    match cmd {
        Cmd::Service { action } => match action {
            ServiceCmd::Install => service::install()?,
            ServiceCmd::Uninstall => service::uninstall()?,
            ServiceCmd::Update => service::update()?,
            ServiceCmd::InstallOrUpdate => {
                if service::query_state()?.is_some() {
                    service::update()?
                } else {
                    service::install()?
                }
            }
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
            println!("this program: quickseek {}", ipc::BUILD);
            match ipc::Client::connect().and_then(|mut c| {
                Ok((
                    c.call(&ipc::Request::Status)?,
                    c.service_build().unwrap_or("an older build").to_string(),
                    c.protocol(),
                ))
            }) {
                Ok((ipc::Response::Status(s), build, ver)) => {
                    println!(
                        "pipe {}: {} (service {build}, protocol v{ver})",
                        ipc::PIPE_NAME,
                        if s.ready {
                            "ready"
                        } else {
                            "index still loading"
                        }
                    );
                    for v in &s.volumes {
                        println!(
                            "  {:<28} {:<6} {:>10} entries  {}",
                            v.key, v.kind, v.entries, v.state
                        );
                    }
                }
                Ok(_) => println!("pipe {}: unexpected answer", ipc::PIPE_NAME),
                Err(e) if ipc::is_version_mismatch(&e) => println!(
                    "pipe {}: {e}; reinstall Quickseek so they match",
                    ipc::PIPE_NAME
                ),
                Err(e) => println!("pipe {}: not available ({e})", ipc::PIPE_NAME),
            }
            println!("local indexes ({}):", persist::db_dir().display());
            for v in win::list_volumes() {
                let db = persist::db_path(v.letter);
                let size = std::fs::metadata(&db)
                    .map(|m| fmt::size(m.len()))
                    .unwrap_or_else(|_| "-".into());
                println!(
                    "  {}:  {:<6} type {} serial {:08X}  index {}",
                    v.letter, v.fs, v.drive_type, v.serial, size
                );
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
            eprintln!(
                "done in {} ms (index saved to {})",
                t.elapsed().as_millis(),
                persist::db_dir().display()
            );
        }
        Cmd::Search {
            ref query,
            sort,
            desc,
            limit,
            long,
            case,
            path,
            ww,
            regex,
            max_size_mb,
            binary,
            no_ifilter,
        } => {
            let mut be = open_backend(&cli, false)?;
            let flags = SearchFlags {
                case,
                path,
                whole_word: ww,
                regex,
            };
            let page = if limit == 0 { u32::MAX } else { limit as u32 };
            let (names, content) =
                content::split_query(&query.join(" "), flags).map_err(anyhow::Error::msg)?;
            let mut res = be
                .search(
                    &names,
                    flags,
                    sort.into(),
                    desc,
                    if content.is_some() {
                        1
                    } else {
                        page.min(10_000)
                    },
                )
                .map_err(anyhow::Error::msg)?;
            let mut stats = None;
            if let Some(cq) = content {
                // Contents are read here, as the user running the command.
                let opts = content::ContentOptions {
                    max_size: max_size_mb << 20,
                    binary,
                    ifilter: !no_ifilter,
                    byte_budget: None,
                    time_limit: None,
                };
                let want = (limit != 0).then_some(limit);
                let (rows, st) =
                    content::matching_rows(&mut |r| be.call(r), Arc::new(cq), opts, want)
                        .map_err(anyhow::Error::msg)?;
                res.rows = rows;
                if let Some(w) = want {
                    res.rows.truncate(w);
                }
                stats = Some(st);
            } else {
                let want = if limit == 0 {
                    res.total
                } else {
                    (limit as u64).min(res.total)
                };
                while (res.rows.len() as u64) < want {
                    let more = be
                        .fetch(res.rows.len() as u64, 10_000)
                        .map_err(anyhow::Error::msg)?;
                    if more.rows.is_empty() {
                        break;
                    }
                    res.rows.extend(more.rows);
                }
            }
            let out = std::io::stdout();
            let mut out = out.lock();
            for row in &res.rows {
                if long {
                    let size = if row.has_size || !row.is_dir {
                        fmt::size(row.size)
                    } else {
                        String::new()
                    };
                    writeln!(
                        out,
                        "{size:>10}  {:16}  {}",
                        fmt::time(row.modified),
                        row.full_path()
                    )?;
                } else {
                    writeln!(out, "{}", row.full_path())?;
                }
            }
            match stats {
                Some(st) => {
                    let more = if st.done {
                        ""
                    } else {
                        " (stopped at the limit)"
                    };
                    let skipped = st.skipped();
                    eprintln!(
                        "{} name matches in {:.2} ms; contents: {} of {} checked, {} matched, {} read in {:.0} ms{more}{}{} via {}",
                        res.total,
                        res.elapsed_us as f64 / 1000.0,
                        st.checked,
                        st.total,
                        st.matched,
                        fmt::size(st.bytes),
                        st.elapsed.as_secs_f64() * 1000.0,
                        if skipped.is_empty() { "" } else { "; skipped " },
                        skipped,
                        be.label()
                    );
                }
                None => eprintln!(
                    "{} results ({} shown) in {:.2} ms via {}",
                    res.total,
                    res.rows.len(),
                    res.elapsed_us as f64 / 1000.0,
                    be.label()
                ),
            }
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
        Cmd::Mcp { ref action } => mcp(action)?,
        Cmd::History { ref action } => {
            let mut be = open_backend(&cli, false)?;
            history_cmd(&mut *be, action)?;
        }
        Cmd::Du {
            ref path,
            ref query,
            depth,
            limit,
        } => {
            use quickseek_core::tree::{NodeRef, TreeRow, TreeSort};
            let mut be = open_backend(&cli, false)?;
            let t = Instant::now();
            let roots = be
                .tree(
                    query.as_deref().unwrap_or(""),
                    SearchFlags::default(),
                    TreeSort::Size,
                    true,
                )
                .map_err(anyhow::Error::msg)?;
            let built = t.elapsed();
            let (top, total) = match path {
                Some(p) => {
                    let l = be
                        .children(
                            &NodeRef {
                                path: p.clone(),
                                ..Default::default()
                            },
                            TreeSort::Size,
                            true,
                            0,
                            limit as u32,
                        )
                        .map_err(anyhow::Error::msg)?;
                    println!("{:>10}  {:>9} files  {}", fmt::size(l.size), l.files, p);
                    (l.rows, l.size)
                }
                None => {
                    let total = roots.size;
                    (roots.rows.into_iter().take(limit).collect(), total)
                }
            };
            fn show(
                be: &mut dyn Backend,
                rows: &[TreeRow],
                parent: u64,
                level: usize,
                depth: usize,
                limit: usize,
            ) -> anyhow::Result<()> {
                for r in rows {
                    let pct = if parent > 0 {
                        r.size as f64 * 100.0 / parent as f64
                    } else {
                        0.0
                    };
                    let bar = "█".repeat((pct / 10.0).round() as usize);
                    let files = if r.is_dir {
                        format!("{:>9} files", r.files)
                    } else {
                        " ".repeat(15)
                    };
                    let mut free = if r.capacity > 0 {
                        format!(
                            "  ({} free of {})",
                            fmt::size(r.free),
                            fmt::size(r.capacity)
                        )
                    } else {
                        String::new()
                    };
                    if r.cloud > 0 {
                        free.push_str(&format!("  [+{} online-only]", fmt::size(r.cloud)));
                    }
                    if r.hardlink {
                        free.push_str("  [hard link, counted elsewhere]");
                    }
                    println!(
                        "{:>10} {:>5.1}% {:<10} {files}  {}{}{}{free}",
                        fmt::size(r.size),
                        pct,
                        bar,
                        "  ".repeat(level),
                        r.name,
                        if r.is_dir { "\\" } else { "" }
                    );
                    if r.is_dir && r.has_children && level + 1 < depth {
                        let node = NodeRef {
                            vol: r.vol.clone(),
                            idx: r.idx,
                            epoch: r.epoch,
                            path: r.full_path(),
                        };
                        let l = be
                            .children(&node, TreeSort::Size, true, 0, limit as u32)
                            .map_err(anyhow::Error::msg)?;
                        show(be, &l.rows, r.size, level + 1, depth, limit)?;
                    }
                }
                Ok(())
            }
            show(&mut *be, &top, total, 0, depth, limit)?;
            be.tree_close();
            eprintln!(
                "tree built in {} ms, listed in {} ms via {}",
                built.as_millis(),
                (t.elapsed() - built).as_millis(),
                be.label()
            );
            be.finish()?;
        }
        Cmd::Watch => {
            require_admin()?;
            let letter = cli
                .volume
                .first()
                .copied()
                .unwrap_or('C')
                .to_ascii_uppercase();
            let mut j = Journal::open(letter, None, None).map_err(|e| anyhow::anyhow!("{e}"))?;
            eprintln!(
                "watching {letter}: (journal {:#x}); Ctrl+C to stop",
                j.journal_id
            );
            loop {
                for r in j.read(true).map_err(|e| anyhow::anyhow!("{e}"))? {
                    println!(
                        "{:>12} rec {:>8} parent {:>8} reason {:08x} attrs {:05x} {}",
                        r.usn,
                        r.record(),
                        r.parent_record(),
                        r.reason,
                        r.attrs,
                        r.name
                    );
                }
            }
        }
        Cmd::Selftest => {
            require_admin()?;
            selftest::run(
                cli.volume
                    .first()
                    .copied()
                    .unwrap_or('C')
                    .to_ascii_uppercase(),
            )?;
        }
        Cmd::Folders { ref action } => match action {
            FolderCmd::List => {
                for f in show_config(&cli)?.folders {
                    let user = f.user.map(|u| format!(" user={u}")).unwrap_or_default();
                    println!(
                        "{}  watch={} rescan={}min{user}",
                        f.path, f.watch, f.rescan_minutes
                    );
                }
            }
            FolderCmd::Add {
                path,
                rescan_minutes,
                no_watch,
                user,
            } => {
                let unc = win::universal_path(path);
                let path = quickseek_core::folder::root_name(&unc);
                quickseek_core::folder::check_root(&path).map_err(anyhow::Error::msg)?;
                if !std::path::Path::new(&format!("{path}\\")).is_dir() {
                    eprintln!(
                        "warning: {path} isn't reachable right now; it will be indexed when it is"
                    );
                }
                let entry = FolderConfig {
                    path: path.clone(),
                    watch: !no_watch,
                    rescan_minutes: *rescan_minutes,
                    user: None,
                };
                edit_config(&cli, |c| {
                    c.folders.retain(|f| !f.path.eq_ignore_ascii_case(&path));
                    c.folders.push(entry);
                    Ok(())
                })?;
                println!("added {path}");
                if let Some(user) = user {
                    folder_login(&cli, &path, user)?;
                }
            }
            FolderCmd::Login { path, user } => {
                let path = quickseek_core::folder::root_name(&win::universal_path(path));
                folder_login(&cli, &path, user)?;
            }
            FolderCmd::Logout { path } => {
                let path = quickseek_core::folder::root_name(&win::universal_path(path));
                service(&cli)?
                    .folder_login(&path, "", Default::default())
                    .map_err(anyhow::Error::msg)?;
                println!("{path}: login removed");
            }
            FolderCmd::Remove { path } => {
                let path = quickseek_core::folder::root_name(&win::universal_path(path));
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
        Cmd::Remotes { ref action } => remotes(&cli, action)?,
        Cmd::Config { ref action } => match action {
            ConfigCmd::Show => println!("{}", serde_json::to_string_pretty(&show_config(&cli)?)?),
            ConfigCmd::Set { file } => {
                let text = std::fs::read_to_string(file)
                    .with_context(|| format!("reading {}", file.display()))?;
                let new: quickseek_core::config::Config = serde_json::from_str(&text)?;
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
            ConfigCmd::IndexCreated { on } => edit_config(&cli, |c| {
                c.index_date_created = *on;
                Ok(())
            })?,
            ConfigCmd::FastSort { key, on } => edit_config(&cli, |c| {
                match key.as_str() {
                    "size" => c.fast_sort_size = *on,
                    "modified" => c.fast_sort_modified = *on,
                    _ => c.fast_sort_path = *on,
                }
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

fn mcp_clients(ids: &[String]) -> anyhow::Result<Vec<quickseek_mcp::register::Client>> {
    use quickseek_mcp::register::{ALL, Client};
    if ids.is_empty() {
        return Ok(ALL.into_iter().filter(|c| c.detected()).collect());
    }
    ids.iter()
        .flat_map(|s| s.split(','))
        .map(|id| {
            Client::from_id(id.trim()).ok_or_else(|| {
                anyhow::anyhow!(
                    "unknown client {id:?}; one of: {}",
                    ALL.map(|c| c.id()).join(", ")
                )
            })
        })
        .collect()
}

fn mcp(action: &McpCmd) -> anyhow::Result<()> {
    use quickseek_mcp::register as reg;
    match action {
        McpCmd::Status => {
            for s in reg::status_all() {
                let state = match (&s.registered, &s.problem) {
                    (Some(cmd), _) => format!(
                        "registered: {cmd}{}",
                        if s.skill { " (+ Quickseek skill)" } else { "" }
                    ),
                    (None, Some(p)) => format!("can't tell: {p}"),
                    (None, None) if s.detected => "not registered".into(),
                    _ => "not installed".into(),
                };
                println!("  {:<18} {state}", s.client.label());
            }
        }
        McpCmd::Install { client, exe } => {
            let src = match exe {
                Some(e) => reg::plain_path(
                    e.canonicalize()
                        .with_context(|| format!("{}", e.display()))?,
                ),
                None => reg::sibling_exe()
                    .context("quickseek-mcp.exe isn't next to this program; pass --exe")?,
            };
            let exe = reg::stable_exe(&src).map_err(anyhow::Error::msg)?;
            let clients = mcp_clients(client)?;
            if clients.is_empty() {
                bail!(
                    "no supported AI client found (Claude Code, Claude Desktop, Cursor, VS Code, Windsurf, Codex, Antigravity); use --client to register anyway"
                );
            }
            println!("registering {}", exe.display());
            let mut failed = 0;
            for c in clients {
                match reg::register(c, &exe) {
                    Ok(()) => println!("  {:<18} done: {}", c.label(), reg::restart_hint(c)),
                    Err(e) => {
                        failed += 1;
                        println!("  {:<18} failed: {e}", c.label());
                    }
                }
            }
            if failed > 0 {
                bail!("{failed} client(s) failed");
            }
        }
        McpCmd::Uninstall { client } => {
            let clients = if client.is_empty() {
                quickseek_mcp::register::ALL.to_vec()
            } else {
                mcp_clients(client)?
            };
            for c in clients {
                match reg::unregister(c) {
                    Ok(true) => println!("  {:<18} removed", c.label()),
                    Ok(false) => {}
                    Err(e) => println!("  {:<18} failed: {e}", c.label()),
                }
            }
        }
    }
    Ok(())
}

/// `+1.2 GB` / `-300.0 MB`.
fn signed_size(d: i64) -> String {
    format!(
        "{}{}",
        if d < 0 { "-" } else { "+" },
        fmt::size(d.unsigned_abs())
    )
}

fn history_cmd(be: &mut dyn Backend, action: &HistoryCmd) -> anyhow::Result<()> {
    use quickseek_core::tree::{NodeRef, TreeSort};
    let when = |t: i64| fmt::time((t + 11_644_473_600) * 10_000_000);
    match action {
        HistoryCmd::List => {
            for s in be.history().map_err(anyhow::Error::msg)? {
                println!(
                    "{}  {:<14} {:>7} folders {:>9}{}",
                    when(s.time),
                    s.id,
                    s.folders,
                    fmt::size(s.bytes),
                    if s.name.is_empty() {
                        String::new()
                    } else {
                        format!("  \"{}\"", s.name)
                    }
                );
            }
        }
        HistoryCmd::Save { name } => {
            let t = Instant::now();
            let s = be.save_point(&name.join(" ")).map_err(anyhow::Error::msg)?;
            println!(
                "saved {} ({}) in {} ms",
                s.id,
                when(s.time),
                t.elapsed().as_millis()
            );
        }
        HistoryCmd::Growth { path, days, limit } => {
            let list = be.history().map_err(anyhow::Error::msg)?;
            let target = quickseek_core::history::now_unix() - *days as i64 * 86_400;
            let base = list
                .iter()
                .min_by_key(|s| (s.time - target).abs())
                .context("no snapshots yet (the service takes one a day)")?;
            be.compare(&base.id).map_err(anyhow::Error::msg)?;
            let roots = be
                .tree("", SearchFlags::default(), TreeSort::Change, true)
                .map_err(anyhow::Error::msg)?;
            let level = match path {
                Some(p) => be
                    .children(
                        &NodeRef {
                            path: p.clone(),
                            ..Default::default()
                        },
                        TreeSort::Change,
                        true,
                        0,
                        *limit as u32,
                    )
                    .map_err(anyhow::Error::msg)?,
                None => roots,
            };
            println!(
                "since {} ({}): {} -> {} ({})",
                when(base.time),
                base.id,
                fmt::size(level.before),
                fmt::size(level.size),
                signed_size(level.size as i64 - level.before as i64)
            );
            for r in level.rows.iter().take(*limit) {
                let note = if r.changed {
                    "  (new or changed)"
                } else if r.is_dir && r.before == 0 && r.size > 0 {
                    "  (new or was small)"
                } else {
                    ""
                };
                let free = if r.capacity > 0 && r.free_before > 0 {
                    format!(
                        "  free {} -> {}",
                        fmt::size(r.free_before),
                        fmt::size(r.free)
                    )
                } else {
                    String::new()
                };
                println!(
                    "{:>11}  {:>10}  {}{}{free}{note}",
                    signed_size(r.change()),
                    fmt::size(r.size),
                    r.name,
                    if r.is_dir { "\\" } else { "" }
                );
            }
            be.compare("").map_err(anyhow::Error::msg)?;
            be.tree_close();
        }
    }
    Ok(())
}
