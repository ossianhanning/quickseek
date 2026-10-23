//! Installer logic (library part, so its tests run without elevation).

pub mod install;
pub mod sys;

use std::io::Write;
use std::path::PathBuf;

use quickseek_mcp::register as reg;

pub struct Args {
    pub silent: bool,
    pub uninstall: bool,
    pub purge: bool,
    pub launch: bool,
    pub opts: install::Options,
    /// `--mcp` / `--no-mcp` given (else: interactive offers detected clients, silent connects none).
    pub mcp_choice: Option<bool>,
}

pub fn parse(args: &[String]) -> Result<Args, String> {
    let mut a = Args {
        silent: false,
        uninstall: false,
        purge: false,
        launch: true,
        opts: install::Options::defaults(),
        mcp_choice: None,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.to_ascii_lowercase().trim_start_matches(['-', '/']) {
            "s" | "silent" | "quiet" | "q" | "verysilent" => a.silent = true,
            "uninstall" | "u" => a.uninstall = true,
            "purge" => a.purge = true,
            "dir" | "d" => a.opts.dir = PathBuf::from(it.next().ok_or("--dir needs a folder")?),
            "no-service" => a.opts.service = false,
            "no-shortcut" => a.opts.shortcut = false,
            "no-path" => a.opts.path = false,
            "no-startup" => a.opts.startup = false,
            "no-arp" => a.opts.arp = false,
            "no-launch" => a.launch = false,
            "log" => {
                let _ = LOG.set(PathBuf::from(it.next().ok_or("--log needs a file")?));
            }
            "mcp" => a.mcp_choice = Some(true),
            "no-mcp" => a.mcp_choice = Some(false),
            "mcp-clients" => {
                let list = it.next().ok_or("--mcp-clients needs a list")?;
                a.opts.mcp = list
                    .split(',')
                    .map(|c| reg::Client::from_id(c.trim()).ok_or(format!("unknown client {c:?}")))
                    .collect::<Result<_, _>>()?;
                a.mcp_choice = Some(true);
            }
            other => return Err(format!("unknown option --{other}")),
        }
    }
    // AI assistants are connected by default (silent installs and winget
    // too), unless --no-mcp, or when running as SYSTEM (deployment tools):
    // that account's profile isn't anybody's, so there's no one to connect.
    if a.mcp_choice == Some(false) || (a.mcp_choice.is_none() && running_as_system()) {
        a.opts.mcp.clear();
    }
    Ok(a)
}

static LOG: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Appends to `--log <file>` (default %TEMP%\quickseek-setup.log).
/// Running as the LocalSystem account (its profile lives under system32).
pub fn running_as_system() -> bool {
    std::env::var("USERPROFILE").is_ok_and(|p| p.to_lowercase().contains(r"\\systemprofile"))
}

pub fn log_line(s: &str) {
    let p = LOG
        .get()
        .cloned()
        .unwrap_or_else(|| std::env::temp_dir().join("quickseek-setup.log"));
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
    {
        let _ = writeln!(f, "{s}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Args {
        parse(&s.split_whitespace().map(String::from).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn arguments() {
        let a = p("/S --dir D:\\Apps\\rf --no-path");
        assert!(a.silent && !a.opts.path && a.opts.service);
        assert_eq!(a.opts.dir, PathBuf::from(r"D:\Apps\rf"));
        assert_eq!(
            a.opts.mcp.is_empty(),
            running_as_system() || !reg::ALL.iter().any(|c| c.detected()),
            "silent installs connect detected clients"
        );
        assert!(p("--silent --no-mcp").opts.mcp.is_empty());
        let a = p("--silent --mcp-clients cursor,claude-code");
        assert_eq!(a.opts.mcp, [reg::Client::Cursor, reg::Client::ClaudeCode]);
        assert!(p("--uninstall --purge").purge);
        assert!(parse(&["--frobnicate".into()]).is_err());
        assert!(parse(&["--mcp-clients".into(), "emacs".into()]).is_err());
    }
}
