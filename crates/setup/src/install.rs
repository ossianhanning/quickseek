//! What installing and uninstalling do, step by step. Every system-wide
//! step can be switched off, so the file part is testable in a temp folder.

use std::path::{Path, PathBuf};
use std::process::Command;

use quickseek_core::upgrade;
use quickseek_mcp::register as reg;

use crate::sys;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub use quickseek_core::upgrade::ARP_KEY;
pub const SETUP_EXE: &str = "quickseek-setup.exe";

static PAYLOAD: [(&str, &[u8]); 3] = [
    (
        "quickseek.exe",
        include_bytes!(concat!(env!("OUT_DIR"), "/quickseek.exe.zst")),
    ),
    (
        "quickseek-gui.exe",
        include_bytes!(concat!(env!("OUT_DIR"), "/quickseek-gui.exe.zst")),
    ),
    (
        "quickseek-mcp.exe",
        include_bytes!(concat!(env!("OUT_DIR"), "/quickseek-mcp.exe.zst")),
    ),
];

/// License notices installed next to the programs (MIT and Apache-2.0 ask
/// for them to go with binary copies).
pub const NOTICES: [(&str, &str); 3] = [
    ("LICENSE-MIT.txt", include_str!("../../../LICENSE-MIT")),
    (
        "LICENSE-APACHE.txt",
        include_str!("../../../LICENSE-APACHE"),
    ),
    ("THIRD-PARTY.md", include_str!("../../../THIRD-PARTY.md")),
];

pub fn has_payload() -> bool {
    PAYLOAD.iter().all(|(_, d)| !d.is_empty())
}

#[derive(Debug, Clone)]
pub struct Options {
    pub dir: PathBuf,
    /// Install or update the indexing service.
    pub service: bool,
    pub shortcut: bool,
    /// Add the folder to the machine PATH (for the `quickseek` command).
    pub path: bool,
    /// Start the GUI (in the tray) when this user logs in.
    pub startup: bool,
    /// Register the MCP server with these AI clients.
    pub mcp: Vec<reg::Client>,
    /// Apps & Features entry (with the uninstaller).
    pub arp: bool,
}

pub fn default_dir() -> PathBuf {
    std::env::var_os("ProgramW6432")
        .or_else(|| std::env::var_os("ProgramFiles"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
        .join("Quickseek")
}

impl Options {
    pub fn defaults() -> Options {
        Options {
            dir: sys::installed_dir().unwrap_or_else(default_dir),
            service: true,
            shortcut: true,
            path: true,
            startup: true,
            mcp: reg::ALL.into_iter().filter(|c| c.detected()).collect(),
            arp: true,
        }
    }
}

/// Runs the installed CLI (for the service steps), returning its output.
fn cli(dir: &Path, args: &[&str]) -> Result<String, String> {
    let exe = dir.join("quickseek.exe");
    // We put every program in place ourselves (maybe in a new --dir): the CLI
    // mustn't copy itself into the previous install's folder.
    let out = sys::hidden(Command::new(&exe).args(args).env(upgrade::NO_SYNC_ENV, "1"))
        .output()
        .map_err(|e| format!("running {}: {e}", exe.display()))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .trim()
    .to_string();
    if out.status.success() {
        Ok(text)
    } else {
        Err(format!("quickseek {}: {text}", args.join(" ")))
    }
}

/// Installs (or updates) everything `o` asks for. `log` gets one line per step.
pub fn install(o: &Options, log: &mut dyn FnMut(&str)) -> Result<(), String> {
    if !has_payload() {
        return Err("this quickseek-setup.exe was built without the program files; build it with tools\\build-release.ps1".into());
    }
    std::fs::create_dir_all(&o.dir).map_err(|e| format!("creating {}: {e}", o.dir.display()))?;
    log(&format!("Copying files to {}", o.dir.display()));
    for (name, z) in &PAYLOAD {
        let data = zstd::decode_all(*z).map_err(|e| format!("unpacking {name}: {e}"))?;
        upgrade::replace_file(&o.dir.join(name), &data)?;
    }
    for (name, text) in NOTICES {
        let p = o.dir.join(name);
        std::fs::write(&p, text).map_err(|e| format!("writing {}: {e}", p.display()))?;
    }
    // A copy of the installer is the uninstaller.
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let setup = o.dir.join(SETUP_EXE);
    if !same_file(&me, &setup) {
        upgrade::replace_file(&setup, &std::fs::read(&me).map_err(|e| e.to_string())?)?;
    }
    let service = if o.service {
        log("Installing the Quickseek service");
        cli(&o.dir, &["service", "install-or-update"]).map(|_| ())
    } else {
        Ok(())
    };
    // MCP servers AI apps started from the old files still run the old code:
    // end them (the apps start the new one on their next call), now that the
    // service is the new one too, and delete the moved-aside copies.
    let (ended, _) = upgrade::retire_old_programs(&o.dir);
    if ended > 0 {
        log(&format!(
            "Restarted {ended} running Quickseek MCP server{}",
            if ended == 1 { "" } else { "s" }
        ));
    }
    service?;
    if o.path {
        log("Adding Quickseek to PATH");
        sys::path_add(&o.dir)?;
    }
    let gui = o.dir.join("quickseek-gui.exe");
    if o.shortcut {
        log("Adding a Start menu shortcut");
        // Replaced, not overwritten: an older lowercase quickseek.lnk would
        // keep its name's case.
        let _ = std::fs::remove_file(sys::start_menu_link());
        sys::shortcut(
            &sys::start_menu_link(),
            &gui,
            "--",
            "Instant file search and folder sizes",
        )?;
    }
    if o.startup {
        sys::set_run_at_login(Some(&gui))?;
    }
    let user_copy = reg::user_dir();
    let registered: Vec<(reg::Client, Option<String>)> = reg::ALL
        .into_iter()
        .map(|c| (c, reg::status(c).registered))
        .collect();
    let clients = clients_to_register(&o.mcp, &registered, &user_copy);
    if !clients.is_empty() {
        let exe = o.dir.join(reg::EXE);
        for c in &clients {
            match reg::register(*c, &exe) {
                Ok(()) => log(&format!("Connected {}", c.label())),
                Err(e) => log(&format!("Couldn't connect {}: {e}", c.label())),
            }
        }
    }
    if reg::retire_user_copy() {
        log(&format!(
            "Removed the per-user copy in {}",
            user_copy.display()
        ));
    }
    if o.arp {
        sys::write_arp(&o.dir, dir_size_kb(&o.dir))?;
    }
    log(&format!("Quickseek {VERSION} is installed"));
    Ok(())
}

/// The clients to register with the installed server: the ones chosen, and
/// any registered with the per-user copy in `user_copy` (a development
/// install, or `quickseek service install` from a build folder), which move
/// to the installed one.
pub fn clients_to_register(
    chosen: &[reg::Client],
    registered: &[(reg::Client, Option<String>)],
    user_copy: &Path,
) -> Vec<reg::Client> {
    let mut out = chosen.to_vec();
    for (c, r) in registered {
        if r.as_deref().is_some_and(|r| reg::runs_from(r, user_copy)) && !out.contains(c) {
            out.push(*c);
        }
    }
    out
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn dir_size_kb(dir: &Path) -> u32 {
    let bytes: u64 = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum();
    (bytes / 1024) as u32
}

#[derive(Debug, Clone)]
pub struct Removal {
    pub dir: PathBuf,
    pub service: bool,
    pub system: bool,
    /// Also delete the index and settings in %ProgramData%\quickseek.
    pub purge: bool,
}

/// Removes what [`install`] added. Files that are in use (this uninstaller)
/// are deleted by a short-lived helper after we exit.
pub fn uninstall(r: &Removal, log: &mut dyn FnMut(&str)) -> Result<(), String> {
    let mcp = r.dir.join(reg::EXE).to_string_lossy().to_lowercase();
    for c in reg::ALL {
        if reg::status(c)
            .registered
            .is_some_and(|p| p.to_lowercase() == mcp)
        {
            match reg::unregister(c) {
                Ok(_) => log(&format!("Disconnected {}", c.label())),
                Err(e) => log(&format!("Couldn't disconnect {}: {e}", c.label())),
            }
        }
    }
    upgrade::kill_in(&r.dir, &["quickseek-gui.exe", upgrade::MCP_EXE]);
    if r.service && r.dir.join("quickseek.exe").exists() {
        log("Removing the Quickseek service");
        if let Err(e) = cli(&r.dir, &["service", "uninstall"]) {
            log(&format!("  {e}"));
        }
        let data = sys::program_data().join("quickseek");
        let _ = std::fs::remove_file(data.join("quickseek.exe"));
        if r.purge {
            log("Deleting the index and settings");
            let _ = std::fs::remove_dir_all(&data);
        }
    }
    if r.system {
        sys::path_remove(&r.dir)?;
        let _ = std::fs::remove_file(sys::start_menu_link());
        sys::set_run_at_login(None)?;
        sys::delete_arp();
    }
    log("Removing files");
    let me = std::env::current_exe().ok();
    for (name, _) in NOTICES {
        let _ = std::fs::remove_file(r.dir.join(name));
    }
    for name in upgrade::PROGRAMS.into_iter().chain([SETUP_EXE]) {
        let p = r.dir.join(name);
        if me.as_ref().is_some_and(|m| same_file(m, &p)) {
            continue;
        }
        if std::fs::remove_file(&p).is_err() && p.exists() {
            upgrade::delete_on_reboot(&p);
        }
    }
    upgrade::remove_leftovers(&r.dir);
    match &me {
        Some(m) if m.starts_with(&r.dir) => sys::delete_after_exit(m, &r.dir),
        _ => {
            let _ = std::fs::remove_dir(&r.dir);
        }
    }
    log("Quickseek is uninstalled");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A machine moved over by a build folder's `service install`: clients
    /// registered with the per-user copy move to the installed server, even
    /// ones not chosen; others stay as they are.
    #[test]
    fn per_user_registrations_move_to_the_install() {
        use reg::Client::*;
        let user = Path::new(r"C:\Users\a\AppData\Local\Programs\quickseek");
        let copy = Some(user.join(reg::EXE).to_string_lossy().into_owned());
        let elsewhere = Some(r"D:\tools\quickseek-mcp.exe".to_string());
        let registered = [
            (ClaudeCode, copy.clone()),
            (Cursor, copy),
            (VsCode, elsewhere),
            (Codex, None),
        ];
        let got = clients_to_register(&[ClaudeCode, Windsurf], &registered, user);
        assert_eq!(got, vec![ClaudeCode, Windsurf, Cursor]);
        assert!(clients_to_register(&[], &[(VsCode, None)], user).is_empty());
    }

    #[test]
    fn options_default_to_program_files() {
        let o = Options::defaults();
        assert!(o.service && o.shortcut && o.path && o.arp);
        assert!(o.dir.ends_with("Quickseek"));
    }
}
