//! System pieces of installing: registry, PATH, shortcuts.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::null_mut;

use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE};

use crate::install::{ARP_KEY, SETUP_EXE, VERSION};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn hidden(c: &mut Command) -> &mut Command {
    c.creation_flags(CREATE_NO_WINDOW)
}

pub fn program_data() -> PathBuf {
    std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
}

// ---- registry --------------------------------------------------------------------

fn get_string(root: HKEY, key: &str, value: &str) -> Option<String> {
    let (k, v) = (wide(key), wide(value));
    let mut size = 0u32;
    unsafe {
        if RegGetValueW(root, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND, null_mut(), null_mut(), &mut size) != 0 {
            return None;
        }
        let mut buf = vec![0u16; size as usize / 2 + 1];
        if RegGetValueW(root, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND, null_mut(), buf.as_mut_ptr() as *mut _, &mut size) != 0 {
            return None;
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..n]))
    }
}

fn set_string(root: HKEY, key: &str, value: &str, data: &str, kind: REG_VALUE_TYPE) -> Result<(), String> {
    let (k, v, d) = (wide(key), wide(value), wide(data));
    let rc = unsafe { RegSetKeyValueW(root, k.as_ptr(), v.as_ptr(), kind, d.as_ptr() as *const _, (d.len() * 2) as u32) };
    if rc == 0 { Ok(()) } else { Err(format!("writing registry value {key}\\{value} (error {rc})")) }
}

fn set_dword(root: HKEY, key: &str, value: &str, data: u32) -> Result<(), String> {
    let (k, v) = (wide(key), wide(value));
    let rc = unsafe { RegSetKeyValueW(root, k.as_ptr(), v.as_ptr(), REG_DWORD, &data as *const u32 as *const _, 4) };
    if rc == 0 { Ok(()) } else { Err(format!("writing registry value {key}\\{value} (error {rc})")) }
}

/// Where a previous install put rapidfind (from its Apps & Features entry).
pub fn installed_dir() -> Option<PathBuf> {
    get_string(HKEY_LOCAL_MACHINE, ARP_KEY, "InstallLocation").map(PathBuf::from).filter(|p| p.join("rapidfind.exe").exists())
}

pub fn installed_version() -> Option<String> {
    get_string(HKEY_LOCAL_MACHINE, ARP_KEY, "DisplayVersion")
}

pub fn write_arp(dir: &Path, size_kb: u32) -> Result<(), String> {
    let setup = dir.join(SETUP_EXE).to_string_lossy().into_owned();
    let k = ARP_KEY;
    let m = HKEY_LOCAL_MACHINE;
    set_string(m, k, "DisplayName", "rapidfind", REG_SZ)?;
    set_string(m, k, "DisplayVersion", VERSION, REG_SZ)?;
    set_string(m, k, "Publisher", "Ossian Hanning", REG_SZ)?;
    set_string(m, k, "InstallLocation", &dir.to_string_lossy(), REG_SZ)?;
    set_string(m, k, "DisplayIcon", &dir.join("rapidfind-gui.exe").to_string_lossy(), REG_SZ)?;
    set_string(m, k, "UninstallString", &format!("\"{setup}\" --uninstall"), REG_SZ)?;
    set_string(m, k, "QuietUninstallString", &format!("\"{setup}\" --uninstall --silent"), REG_SZ)?;
    set_dword(m, k, "EstimatedSize", size_kb)?;
    set_dword(m, k, "NoModify", 1)?;
    set_dword(m, k, "NoRepair", 1)
}

pub fn delete_arp() {
    let k = wide(ARP_KEY);
    unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, k.as_ptr()) };
}

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// The same Run entry the GUI's "Start with Windows" uses.
pub fn set_run_at_login(gui: Option<&Path>) -> Result<(), String> {
    match gui {
        Some(g) => set_string(HKEY_CURRENT_USER, RUN_KEY, "rapidfind", &format!("\"{}\" --background", g.display()), REG_SZ),
        None => {
            let (k, v) = (wide(RUN_KEY), wide("rapidfind"));
            unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), v.as_ptr()) };
            Ok(())
        }
    }
}

// ---- PATH --------------------------------------------------------------------------

const ENV_KEY: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";

fn norm(p: &str) -> String {
    p.trim().trim_end_matches('\\').to_lowercase()
}

/// `path` with `dir` added (once) or removed.
pub fn edit_path_list(path: &str, dir: &str, add: bool) -> String {
    let mut parts: Vec<&str> = path.split(';').filter(|p| !p.trim().is_empty()).collect();
    parts.retain(|p| norm(p) != norm(dir));
    if add {
        parts.push(dir);
    }
    parts.join(";")
}

fn set_machine_path(f: impl Fn(&str) -> String) -> Result<(), String> {
    let cur = get_string(HKEY_LOCAL_MACHINE, ENV_KEY, "Path").unwrap_or_default();
    let new = f(&cur);
    if new != cur {
        set_string(HKEY_LOCAL_MACHINE, ENV_KEY, "Path", &new, REG_EXPAND_SZ)?;
        let env = wide("Environment");
        let mut r = 0usize;
        unsafe { SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, env.as_ptr() as isize, SMTO_ABORTIFHUNG, 2000, &mut r) };
    }
    Ok(())
}

pub fn path_add(dir: &Path) -> Result<(), String> {
    let d = dir.to_string_lossy().into_owned();
    set_machine_path(|p| edit_path_list(p, &d, true))
}

pub fn path_remove(dir: &Path) -> Result<(), String> {
    let d = dir.to_string_lossy().into_owned();
    set_machine_path(|p| edit_path_list(p, &d, false))
}

// ---- shortcut ------------------------------------------------------------------------

/// All users' Start menu entry.
pub fn start_menu_link() -> PathBuf {
    program_data().join(r"Microsoft\Windows\Start Menu\Programs\rapidfind.lnk")
}

pub fn shortcut(link: &Path, target: &Path, _args: &str, description: &str) -> Result<(), String> {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, IPersistFile};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::{HSTRING, Interface};
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(|e| format!("shell link: {e}"))?;
        sl.SetPath(&HSTRING::from(target.as_os_str())).map_err(|e| e.to_string())?;
        if let Some(d) = target.parent() {
            sl.SetWorkingDirectory(&HSTRING::from(d.as_os_str())).map_err(|e| e.to_string())?;
        }
        sl.SetDescription(&HSTRING::from(description)).map_err(|e| e.to_string())?;
        let pf: IPersistFile = sl.cast().map_err(|e| e.to_string())?;
        if let Some(d) = link.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        pf.Save(&HSTRING::from(link.as_os_str()), true).map_err(|e| format!("saving {}: {e}", link.display()))
    }
}

// ---- leftovers ------------------------------------------------------------------------

/// Deletes the running uninstaller (and its folder, if empty) once it has exited.
pub fn delete_after_exit(exe: &Path, dir: &Path) {
    let cmd = format!("ping -n 3 127.0.0.1 >nul & del /f /q \"{}\" & rmdir \"{}\"", exe.display(), dir.display());
    let _ = hidden(Command::new("cmd").arg("/c").raw_arg(&cmd)).spawn();
}

/// Starts `exe` as the logged-on user (not elevated like us), via Explorer.
pub fn launch_unelevated(exe: &Path) {
    let _ = Command::new("explorer.exe").arg(exe).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_list_edits() {
        assert_eq!(edit_path_list(r"C:\a;C:\b", r"C:\Program Files\rapidfind", true), r"C:\a;C:\b;C:\Program Files\rapidfind");
        assert_eq!(edit_path_list(r"C:\a;c:\program files\RAPIDFIND\;C:\b;", r"C:\Program Files\rapidfind", true), r"C:\a;C:\b;C:\Program Files\rapidfind");
        assert_eq!(edit_path_list(r"C:\a;C:\Program Files\rapidfind;C:\b", r"C:\Program Files\rapidfind", false), r"C:\a;C:\b");
        assert_eq!(edit_path_list("", r"D:\x", true), r"D:\x");
    }
}
