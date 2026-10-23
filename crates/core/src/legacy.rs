//! Moving an install of rapidfind (Quickseek's former name) over: its data
//! folders (saved indexes, snapshot history, settings) and what its
//! installer added. The service and the AI assistants' registrations are
//! handled by the CLI (`quickseek service install/update`) and
//! `register::migrate_legacy`; this is the files and the registry.

use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HWND};
use windows_sys::Win32::System::Registry::*;

use crate::ntfs::win::wide;

/// The old name: folders, service, registry entries.
pub const OLD: &str = "rapidfind";
pub const OLD_SERVICE: &str = "rapidfind";
/// The old installer's Apps & Features entry (HKLM).
pub const OLD_ARP_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\rapidfind";
/// The old programs (in its install folder, and copies elsewhere).
pub const OLD_PROGRAMS: [&str; 4] = [
    "rapidfind.exe",
    "rapidfind-gui.exe",
    "rapidfind-mcp.exe",
    "rapidfind-setup.exe",
];
const NEW: &str = "quickseek";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const ENV_KEY: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";

/// The folders the data lives under: %ProgramData% (the service's indexes,
/// history, config), %LOCALAPPDATA% (local indexes) and %APPDATA% (the
/// window's settings). Tests point them at temporary folders.
#[derive(Debug, Clone)]
pub struct Roots(pub Vec<PathBuf>);

impl Roots {
    pub fn from_env() -> Roots {
        Roots(
            ["ProgramData", "LOCALAPPDATA", "APPDATA"]
                .iter()
                .filter_map(std::env::var_os)
                .map(PathBuf::from)
                .collect(),
        )
    }
}

/// Whether anything of an old install is left to move.
pub fn present(roots: &Roots) -> bool {
    roots.0.iter().any(|r| r.join(OLD).exists()) || old_install_dir().is_some()
}

/// Ends the old programs still running: from the old install folder, and
/// the copies in the data folders (an MCP server an AI app started).
pub fn end_old_programs(roots: &Roots) -> usize {
    let mut n = 0;
    for dir in roots.0.iter().map(|r| r.join(OLD)).chain(old_install_dir()) {
        n += crate::upgrade::kill_in(&dir, &OLD_PROGRAMS);
    }
    n
}

/// Moves `<root>\rapidfind` to `<root>\quickseek` under each root. Where
/// Quickseek's folder already exists (it ran before), the old entries it
/// doesn't have move in and the ones it has are dropped (Quickseek's are
/// newer). The old programs' copies (the service ran its own) are deleted,
/// not moved. Returns what it did, a line each.
pub fn move_data(roots: &Roots) -> Vec<String> {
    let mut log = Vec::new();
    for root in &roots.0 {
        let (old, new) = (root.join(OLD), root.join(NEW));
        if !old.is_dir() {
            continue;
        }
        for p in OLD_PROGRAMS {
            let _ = std::fs::remove_file(old.join(p));
        }
        let r = if new.exists() {
            merge(&old, &new).map(|()| {
                let _ = std::fs::remove_dir_all(&old);
            })
        } else {
            std::fs::rename(&old, &new).or_else(|_| merge(&old, &new))
        };
        log.push(match r {
            Ok(()) => format!("moved {} to {}", old.display(), new.display()),
            Err(e) => format!("couldn't move {} to {}: {e}", old.display(), new.display()),
        });
    }
    log
}

/// Moves `from`'s entries into `to`, keeping what `to` already has.
fn merge(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let dst = to.join(e.file_name());
        if !dst.exists() {
            if std::fs::rename(e.path(), &dst).is_err() {
                // Across volumes, or in use: copy what can be copied.
                if e.file_type()?.is_dir() {
                    merge(&e.path(), &dst)?;
                } else {
                    std::fs::copy(e.path(), &dst)?;
                }
            }
        } else if e.file_type()?.is_dir() && dst.is_dir() {
            merge(&e.path(), &dst)?;
        }
    }
    Ok(())
}

fn get_string(root: HKEY, key: &str, value: &str) -> Option<String> {
    let (k, v) = (wide(key), wide(value));
    let mut buf = vec![0u16; 4096];
    let mut n = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            root,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as _,
            &mut n,
        )
    };
    (r == ERROR_SUCCESS).then(|| {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    })
}

fn set_string(root: HKEY, key: &str, value: &str, data: &str, kind: REG_VALUE_TYPE) -> bool {
    let (k, v, d) = (wide(key), wide(value), wide(data));
    unsafe {
        RegSetKeyValueW(
            root,
            k.as_ptr(),
            v.as_ptr(),
            kind,
            d.as_ptr() as _,
            (d.len() * 2) as u32,
        ) == ERROR_SUCCESS
    }
}

fn delete_value(root: HKEY, key: &str, value: &str) -> bool {
    let (k, v) = (wide(key), wide(value));
    unsafe { RegDeleteKeyValueW(root, k.as_ptr(), v.as_ptr()) == ERROR_SUCCESS }
}

/// Where the old installer put rapidfind (its Apps & Features entry).
pub fn old_install_dir() -> Option<PathBuf> {
    get_string(HKEY_LOCAL_MACHINE, OLD_ARP_KEY, "InstallLocation")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// `path` (a `;` list) without `dir`.
pub fn without_dir(path: &str, dir: &str) -> String {
    let norm = |p: &str| p.trim().trim_end_matches('\\').to_lowercase();
    path.split(';')
        .filter(|p| !p.trim().is_empty() && norm(p) != norm(dir))
        .collect::<Vec<_>>()
        .join(";")
}

/// Removes the old install: its programs and folder (the processes running
/// from it are ended first), Start menu entry, PATH entry, uninstall entry,
/// and its "start with Windows" entry, which moves to `new_gui` if given.
/// Run elevated, after the old service is gone.
pub fn remove_old_install(new_gui: Option<&Path>) -> Vec<String> {
    let mut log = Vec::new();
    let dir = old_install_dir();
    if let Some(dir) = &dir {
        let ended = crate::upgrade::kill_in(dir, &OLD_PROGRAMS);
        if ended > 0 {
            log.push(format!("ended {ended} running rapidfind program(s)"));
        }
        // Only a folder of its own (Program Files\rapidfind): it holds just
        // the programs and the notices.
        let ours = dir
            .file_name()
            .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(OLD));
        if ours && dir.is_dir() {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_file() && std::fs::remove_file(&p).is_err() {
                    crate::upgrade::delete_on_reboot(&p);
                }
            }
            match std::fs::remove_dir(dir) {
                Ok(()) => log.push(format!("removed {}", dir.display())),
                Err(_) => {
                    crate::upgrade::delete_on_reboot(dir);
                    log.push(format!("{} goes at the next restart", dir.display()));
                }
            }
        }
        // PATH (the machine's).
        let d = dir.to_string_lossy().into_owned();
        if let Some(cur) = get_string(HKEY_LOCAL_MACHINE, ENV_KEY, "Path") {
            let new = without_dir(&cur, &d);
            if new != cur && set_string(HKEY_LOCAL_MACHINE, ENV_KEY, "Path", &new, REG_EXPAND_SZ) {
                broadcast_environment();
                log.push("removed rapidfind from PATH".into());
            }
        }
    }
    if let Some(pd) = std::env::var_os("ProgramData") {
        let link = PathBuf::from(pd).join(r"Microsoft\Windows\Start Menu\Programs\rapidfind.lnk");
        if std::fs::remove_file(&link).is_ok() {
            log.push("removed the rapidfind Start menu entry".into());
        }
    }
    if get_string(HKEY_CURRENT_USER, RUN_KEY, OLD).is_some() {
        delete_value(HKEY_CURRENT_USER, RUN_KEY, OLD);
        if let Some(g) = new_gui {
            set_string(
                HKEY_CURRENT_USER,
                RUN_KEY,
                NEW,
                &format!("\"{}\" --background", g.display()),
                REG_SZ,
            );
        }
        log.push("moved \"start with Windows\" to Quickseek".into());
    }
    let k = wide(OLD_ARP_KEY);
    if unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, k.as_ptr()) } == ERROR_SUCCESS {
        log.push("removed rapidfind from Apps & Features".into());
    }
    log
}

fn broadcast_environment() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
    };
    let env = wide("Environment");
    let mut r = 0usize;
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST as HWND,
            WM_SETTINGCHANGE,
            0,
            env.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            2000,
            &mut r,
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("quickseek-legacy-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    /// A fake old install: the service's folder (indexes, history, the
    /// service's own copy of the program), local indexes and the window's
    /// settings move over, the program copy doesn't; where Quickseek has a
    /// folder already, its own files win and the old ones fill the gaps.
    #[test]
    fn moves_old_folders_keeping_newer_ones() {
        let base = tmp("move");
        let (pd, local, roam) = (base.join("pd"), base.join("local"), base.join("roam"));
        write(&pd.join(r"rapidfind\index\C.db"), "old C index");
        write(
            &pd.join(r"rapidfind\history\2026-10-01.snap"),
            "old snapshot",
        );
        write(&pd.join(r"rapidfind\history\2026-10-23.snap"), "old today");
        write(&pd.join(r"rapidfind\config.json"), "{}");
        write(&pd.join(r"rapidfind\rapidfind.exe"), "program");
        write(&local.join(r"rapidfind\F.db"), "local F index");
        write(&local.join(r"rapidfind\rapidfind-mcp.exe"), "program");
        write(&roam.join(r"rapidfind\gui.json"), "{\"theme\":\"dark\"}");
        // Quickseek already ran once on this machine (today's snapshot).
        write(&pd.join(r"quickseek\history\2026-10-23.snap"), "new today");
        let roots = Roots(vec![pd.clone(), local.clone(), roam.clone()]);
        assert!(present(&roots));

        let log = move_data(&roots);
        assert_eq!(log.len(), 3, "{log:?}");
        let read = |p: PathBuf| std::fs::read_to_string(p).unwrap();
        assert_eq!(read(pd.join(r"quickseek\index\C.db")), "old C index");
        assert_eq!(
            read(pd.join(r"quickseek\history\2026-10-01.snap")),
            "old snapshot"
        );
        assert_eq!(
            read(pd.join(r"quickseek\history\2026-10-23.snap")),
            "new today"
        );
        assert!(pd.join(r"quickseek\config.json").exists());
        assert!(
            !pd.join(r"quickseek\rapidfind.exe").exists(),
            "not the old program"
        );
        assert_eq!(read(local.join(r"quickseek\F.db")), "local F index");
        assert!(!local.join(r"quickseek\rapidfind-mcp.exe").exists());
        assert_eq!(
            read(roam.join(r"quickseek\gui.json")),
            "{\"theme\":\"dark\"}"
        );
        for r in [&pd, &local, &roam] {
            assert!(!r.join("rapidfind").exists(), "{} left behind", r.display());
        }
        // Nothing left to do the second time.
        assert!(move_data(&roots).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn path_loses_only_the_old_folder() {
        assert_eq!(
            without_dir(
                r"C:\a;C:\Program Files\rapidfind\;C:\b",
                r"C:\Program Files\rapidfind"
            ),
            r"C:\a;C:\b"
        );
        assert_eq!(without_dir(r"C:\a;C:\b", r"C:\x"), r"C:\a;C:\b");
    }
}
