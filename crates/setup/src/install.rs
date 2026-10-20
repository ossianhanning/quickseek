//! What installing and uninstalling do, step by step. Every system-wide
//! step can be switched off, so the file part is testable in a temp folder.

use std::path::{Path, PathBuf};
use std::process::Command;

use rapidfind_mcp::register as reg;

use crate::sys;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const ARP_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\rapidfind";
pub const SETUP_EXE: &str = "rapidfind-setup.exe";

static PAYLOAD: [(&str, &[u8]); 3] = [
    ("rapidfind.exe", include_bytes!(concat!(env!("OUT_DIR"), "/rapidfind.exe.zst"))),
    ("rapidfind-gui.exe", include_bytes!(concat!(env!("OUT_DIR"), "/rapidfind-gui.exe.zst"))),
    ("rapidfind-mcp.exe", include_bytes!(concat!(env!("OUT_DIR"), "/rapidfind-mcp.exe.zst"))),
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
    /// Add the folder to the machine PATH (for the `rapidfind` command).
    pub path: bool,
    /// Start the GUI (in the tray) when this user logs in.
    pub startup: bool,
    /// Register the MCP server with these AI clients.
    pub mcp: Vec<reg::Client>,
    /// Apps & Features entry (with the uninstaller).
    pub arp: bool,
}

pub fn default_dir() -> PathBuf {
    std::env::var_os("ProgramW6432").or_else(|| std::env::var_os("ProgramFiles")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Program Files")).join("rapidfind")
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

/// Writes `data` to `dst`; if `dst` is in use (a running GUI or MCP server),
/// moves it aside first (Windows allows renaming a running exe) and deletes
/// the old copy at the next reboot.
fn replace_file(dst: &Path, data: &[u8]) -> Result<(), String> {
    if std::fs::write(dst, data).is_ok() {
        return Ok(());
    }
    let old = dst.with_extension(format!("old-{}", std::process::id()));
    std::fs::rename(dst, &old).map_err(|e| format!("{} is in use and can't be moved aside: {e}", dst.display()))?;
    sys::delete_on_reboot(&old);
    std::fs::write(dst, data).map_err(|e| format!("writing {}: {e}", dst.display()))
}

/// Runs the installed CLI (for the service steps), returning its output.
fn cli(dir: &Path, args: &[&str]) -> Result<String, String> {
    let exe = dir.join("rapidfind.exe");
    let out = sys::hidden(Command::new(&exe).args(args)).output().map_err(|e| format!("running {}: {e}", exe.display()))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)).trim().to_string();
    if out.status.success() { Ok(text) } else { Err(format!("rapidfind {}: {text}", args.join(" "))) }
}

/// Installs (or updates) everything `o` asks for. `log` gets one line per step.
pub fn install(o: &Options, log: &mut dyn FnMut(&str)) -> Result<(), String> {
    if !has_payload() {
        return Err("this rapidfind-setup.exe was built without the program files; build it with tools\\build-release.ps1".into());
    }
    std::fs::create_dir_all(&o.dir).map_err(|e| format!("creating {}: {e}", o.dir.display()))?;
    log(&format!("Copying files to {}", o.dir.display()));
    for (name, z) in &PAYLOAD {
        let data = zstd::decode_all(*z).map_err(|e| format!("unpacking {name}: {e}"))?;
        replace_file(&o.dir.join(name), &data)?;
    }
    // A copy of the installer is the uninstaller.
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    let setup = o.dir.join(SETUP_EXE);
    if !same_file(&me, &setup) {
        replace_file(&setup, &std::fs::read(&me).map_err(|e| e.to_string())?)?;
    }
    if o.service {
        log("Installing the rapidfind service");
        cli(&o.dir, &["service", "install-or-update"])?;
    }
    if o.path {
        log("Adding rapidfind to PATH");
        sys::path_add(&o.dir)?;
    }
    let gui = o.dir.join("rapidfind-gui.exe");
    if o.shortcut {
        log("Adding a Start menu shortcut");
        sys::shortcut(&sys::start_menu_link(), &gui, "--", "Instant file search and folder sizes")?;
    }
    if o.startup {
        sys::set_run_at_login(Some(&gui))?;
    }
    if !o.mcp.is_empty() {
        let exe = o.dir.join(reg::EXE);
        for c in &o.mcp {
            match reg::register(*c, &exe) {
                Ok(()) => log(&format!("Connected {}", c.label())),
                Err(e) => log(&format!("Couldn't connect {}: {e}", c.label())),
            }
        }
    }
    if o.arp {
        sys::write_arp(&o.dir, dir_size_kb(&o.dir))?;
    }
    log(&format!("rapidfind {VERSION} is installed"));
    Ok(())
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn dir_size_kb(dir: &Path) -> u32 {
    let bytes: u64 = std::fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum();
    (bytes / 1024) as u32
}

#[derive(Debug, Clone)]
pub struct Removal {
    pub dir: PathBuf,
    pub service: bool,
    pub system: bool,
    /// Also delete the index and settings in %ProgramData%\rapidfind.
    pub purge: bool,
}

/// Removes what [`install`] added. Files that are in use (this uninstaller)
/// are deleted by a short-lived helper after we exit.
pub fn uninstall(r: &Removal, log: &mut dyn FnMut(&str)) -> Result<(), String> {
    let mcp = r.dir.join(reg::EXE).to_string_lossy().to_lowercase();
    for c in reg::ALL {
        if reg::status(c).registered.is_some_and(|p| p.to_lowercase() == mcp) {
            match reg::unregister(c) {
                Ok(_) => log(&format!("Disconnected {}", c.label())),
                Err(e) => log(&format!("Couldn't disconnect {}: {e}", c.label())),
            }
        }
    }
    sys::kill_in(&r.dir, &["rapidfind-gui.exe", "rapidfind-mcp.exe"]);
    if r.service && r.dir.join("rapidfind.exe").exists() {
        log("Removing the rapidfind service");
        if let Err(e) = cli(&r.dir, &["service", "uninstall"]) {
            log(&format!("  {e}"));
        }
        let data = sys::program_data().join("rapidfind");
        let _ = std::fs::remove_file(data.join("rapidfind.exe"));
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
    for name in ["rapidfind.exe", "rapidfind-gui.exe", "rapidfind-mcp.exe", SETUP_EXE] {
        let p = r.dir.join(name);
        if me.as_ref().is_some_and(|m| same_file(m, &p)) {
            continue;
        }
        if std::fs::remove_file(&p).is_err() && p.exists() {
            sys::delete_on_reboot(&p);
        }
    }
    // Leftovers from in-use updates.
    for e in std::fs::read_dir(&r.dir).into_iter().flatten().flatten() {
        if e.file_name().to_string_lossy().contains(".old-") {
            let _ = std::fs::remove_file(e.path());
        }
    }
    match &me {
        Some(m) if m.starts_with(&r.dir) => sys::delete_after_exit(m, &r.dir),
        _ => {
            let _ = std::fs::remove_dir(&r.dir);
        }
    }
    log("rapidfind is uninstalled");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_moves_a_locked_file_aside() {
        let d = std::env::temp_dir().join(format!("rapidfind-setup-test-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("x.exe");
        std::fs::write(&p, b"old").unwrap();
        // Hold the file open without sharing write access, like a running exe.
        use std::os::windows::fs::OpenOptionsExt;
        let lock = std::fs::OpenOptions::new().read(true).share_mode(1 | 4 /* read | delete */).open(&p).unwrap();
        replace_file(&p, b"new").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
        drop(lock);
        let leftovers = std::fs::read_dir(&d).unwrap().count();
        assert_eq!(leftovers, 2, "the old copy waits for deletion");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn options_default_to_program_files() {
        let o = Options::defaults();
        assert!(o.service && o.shortcut && o.path && o.arp);
        assert!(o.dir.ends_with("rapidfind"));
    }
}
