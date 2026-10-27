//! `quickseek-agent` command line.

use std::path::PathBuf;

use quickseek_agent::{Options, parse_share, state::Store};
use quickseek_remote::pair;

const USAGE: &str = "\
quickseek-agent: serves a NAS's folders to Quickseek on Windows

Usage:
  quickseek-agent [run] [options]     index and serve (the default)
  quickseek-agent pair                make a pairing code for `quickseek remotes add <host> --pair <code>`
  quickseek-agent clients             list paired PCs
  quickseek-agent revoke <name>       forget a paired PC
  quickseek-agent fingerprint         show the certificate fingerprint
  quickseek-agent version

Options (environment variable in brackets):
  --share NAME=PATH   a folder to serve, repeatable; PATH alone is named after its last folder
                      [QUICKSEEK_SHARES, separated by commas]
  --config DIR        certificate, paired PCs, snapshots [QUICKSEEK_CONFIG, default /config]
  --listen ADDR       [QUICKSEEK_LISTEN, default 0.0.0.0]
  --port N            [QUICKSEEK_PORT, default 48623]
  --watch KIND        auto, fanotify, inotify or none [QUICKSEEK_WATCH, default auto]
  --mark PATH         mount for the fanotify filesystem mark, repeatable [QUICKSEEK_MARK]
                      (default: the mount holding each share; for btrfs it must be the volume's root)
  --threads N         walker threads [QUICKSEEK_THREADS, default min(4, cores)]
  --rewalk-hours N    safety re-walk interval, 0 = never [QUICKSEEK_REWALK_HOURS, default 24]
  --name NAME         shown on the PC [QUICKSEEK_NAME, default the host name]
  --walk-io KIND      I/O priority of walks after the first: idle (only when the disks are otherwise
                      free) or normal [QUICKSEEK_WALK_IO, default idle]; the first walk and live
                      changes read at normal
";

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn parse(args: &[String]) -> anyhow::Result<(String, Vec<String>, Options)> {
    let mut o = Options::default();
    if let Some(v) = env("QUICKSEEK_CONFIG") {
        o.config = v.into();
    }
    if let Some(v) = env("QUICKSEEK_SHARES") {
        for s in v.split(',').filter(|s| !s.trim().is_empty()) {
            o.shares.push(parse_share(s)?);
        }
    }
    if let Some(v) = env("QUICKSEEK_LISTEN") {
        o.listen = v;
    }
    if let Some(v) = env("QUICKSEEK_PORT") {
        o.port = v.parse()?;
    }
    if let Some(v) = env("QUICKSEEK_WATCH") {
        o.watch = v.parse().map_err(anyhow::Error::msg)?;
    }
    if let Some(v) = env("QUICKSEEK_MARK") {
        o.marks = v.split(',').map(PathBuf::from).collect();
    }
    if let Some(v) = env("QUICKSEEK_THREADS") {
        o.threads = v.parse()?;
    }
    if let Some(v) = env("QUICKSEEK_REWALK_HOURS") {
        o.rewalk_hours = v.parse()?;
    }
    if let Some(v) = env("QUICKSEEK_NAME") {
        o.name = v;
    }
    if let Some(v) = env("QUICKSEEK_WALK_IO") {
        o.idle_walks = match v.as_str() {
            "idle" => true,
            "normal" => false,
            _ => anyhow::bail!("QUICKSEEK_WALK_IO: idle or normal, not {v}"),
        };
    }
    let mut cmd = String::from("run");
    let mut rest = Vec::new();
    let mut from_cli = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{a} needs a value"))
        };
        match a.as_str() {
            "--share" => from_cli.push(parse_share(&val()?)?),
            "--config" => o.config = val()?.into(),
            "--listen" => o.listen = val()?,
            "--port" => o.port = val()?.parse()?,
            "--watch" => o.watch = val()?.parse().map_err(anyhow::Error::msg)?,
            "--mark" => o.marks.push(val()?.into()),
            "--threads" => o.threads = val()?.parse()?,
            "--rewalk-hours" => o.rewalk_hours = val()?.parse()?,
            "--name" => o.name = val()?,
            "--walk-io" => {
                o.idle_walks = match val()?.as_str() {
                    "idle" => true,
                    "normal" => false,
                    v => anyhow::bail!("--walk-io: idle or normal, not {v}"),
                }
            }
            "-h" | "--help" | "help" => cmd = "help".into(),
            "-V" | "--version" => cmd = "version".into(),
            s if s.starts_with('-') => anyhow::bail!("unknown option {s}\n\n{USAGE}"),
            s if rest.is_empty()
                && cmd == "run"
                && ["run", "pair", "clients", "revoke", "fingerprint", "version"].contains(&s) =>
            {
                cmd = s.to_string()
            }
            s => rest.push(s.to_string()),
        }
    }
    if !from_cli.is_empty() {
        o.shares = from_cli;
    }
    Ok((cmd, rest, o))
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_target(false)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("quickseek-agent: {e:#}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> anyhow::Result<()> {
    let (cmd, rest, o) = parse(args)?;
    match cmd.as_str() {
        "help" => print!("{USAGE}"),
        "version" => println!("{}", quickseek_agent::server::BUILD),
        "pair" => {
            let s = Store::new(&o.config)?;
            let c = s.new_code()?;
            println!("Pairing code: {}", c.code);
            println!(
                "Valid for {} minutes. On the PC: quickseek remotes add <this NAS's name> --pair {}",
                quickseek_agent::state::CODE_MINUTES,
                c.code
            );
            println!("Certificate: {}", pair::show_fingerprint(&s.fingerprint()?));
        }
        "clients" => {
            let s = Store::new(&o.config)?;
            let list = s.clients();
            if list.is_empty() {
                println!("no PC is paired");
            }
            for c in list {
                println!("{}\tpaired {}", c.name, c.paired);
            }
        }
        "revoke" => {
            let name = rest
                .first()
                .ok_or_else(|| anyhow::anyhow!("revoke: which PC?"))?;
            let s = Store::new(&o.config)?;
            if s.revoke(name)? {
                println!(
                    "{name} is no longer paired (connections it has open end at the next restart)"
                );
            } else {
                anyhow::bail!("no paired PC called {name}");
            }
        }
        "fingerprint" => println!(
            "{}",
            pair::show_fingerprint(&Store::new(&o.config)?.fingerprint()?)
        ),
        _ => {
            lower_priority();
            let r = quickseek_agent::start(o)?;
            let _keep = r;
            loop {
                std::thread::park();
            }
        }
    }
    Ok(())
}

/// Background CPU priority: the NAS's own work (Samba, media servers) wins.
/// Called before any thread starts, so every thread inherits it. (Walker
/// threads also get idle I/O priority, see `Options::idle_walks`.)
#[cfg(target_os = "linux")]
fn lower_priority() {
    unsafe { libc::setpriority(libc::PRIO_PROCESS as _, 0, 10) };
}

#[cfg(not(target_os = "linux"))]
fn lower_priority() {}
