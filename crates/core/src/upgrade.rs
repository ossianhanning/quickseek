//! Replacing Quickseek's programs while copies of them run. The installer
//! and `quickseek service update` put new files in place (moving running ones
//! aside), then end the MCP servers still running the old code (AI apps start
//! a new one, from the new file, on their next call) and delete the moved-aside
//! copies that are no longer in use.

use std::path::Path;
use std::ptr::null;

use windows_sys::Win32::Foundation::{CloseHandle, MAX_PATH};
use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
};

use crate::ntfs::win::wide;

/// The programs an install folder holds (besides the installer).
pub const PROGRAMS: [&str; 3] = ["quickseek.exe", "quickseek-gui.exe", MCP_EXE];
pub const MCP_EXE: &str = "quickseek-mcp.exe";
/// Set by the installer for the `quickseek service ...` it runs: don't copy
/// programs into the install folder (it already has).
pub const NO_SYNC_ENV: &str = "QUICKSEEK_NO_SYNC";
/// The installer's Apps & Features entry (HKLM).
pub const ARP_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\quickseek";

/// Where quickseek-setup.exe installed Quickseek, if it did.
pub fn install_dir() -> Option<std::path::PathBuf> {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    let (k, v) = (wide(ARP_KEY), wide("InstallLocation"));
    let mut buf = vec![0u16; 1024];
    let mut n = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr() as _,
            &mut n,
        )
    };
    if r != 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(std::path::PathBuf::from(String::from_utf16_lossy(
        &buf[..len],
    )))
    .filter(|p| p.join("quickseek.exe").exists())
}

/// Runs `cmd` to completion with its output captured, like
/// `Command::output`, but ends it after `limit`: an installer must never
/// wait forever on a program (a silent install has nobody to notice).
/// Stdin is closed, so nothing can wait for input either.
pub fn output_with_timeout(
    cmd: &mut std::process::Command,
    limit: std::time::Duration,
) -> std::io::Result<std::process::Output> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let read = |mut r: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = r.read_to_end(&mut v);
            v
        })
    };
    let out = read(Box::new(child.stdout.take().unwrap()));
    let err = read(Box::new(child.stderr.take().unwrap()));
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("no answer after {} s, ended", limit.as_secs()),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    Ok(std::process::Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

pub fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// Writes `data` to `dst`; if `dst` is in use (a running program), moves it
/// aside first to `<name>.old-<pid>` (Windows allows renaming a running exe),
/// to be deleted by [`remove_leftovers`] or at the next reboot. Returns
/// whether it had to.
pub fn replace_file(dst: &Path, data: &[u8]) -> Result<bool, String> {
    if std::fs::write(dst, data).is_ok() {
        return Ok(false);
    }
    let old = dst.with_extension(format!("old-{}", std::process::id()));
    let _ = std::fs::remove_file(&old);
    std::fs::rename(dst, &old)
        .map_err(|e| format!("{} is in use and can't be moved aside: {e}", dst.display()))?;
    delete_on_reboot(&old);
    std::fs::write(dst, data).map_err(|e| format!("writing {}: {e}", dst.display()))?;
    Ok(true)
}

pub fn delete_on_reboot(p: &Path) {
    let w = wide(&p.to_string_lossy());
    unsafe { MoveFileExW(w.as_ptr(), null(), MOVEFILE_DELAY_UNTIL_REBOOT) };
}

/// Ends running copies of these programs started from `dir` (also ones whose
/// file was moved aside since) and waits briefly for them to exit. Returns
/// how many it ended.
/// The message the installer and `quickseek service update` send each
/// Quickseek window before replacing its program; wparam is the updater's
/// process id. The window arranges to start again once that process has
/// exited (unelevated, as itself), then closes.
pub const CLOSE_FOR_UPDATE: &str = "Quickseek.CloseForUpdate";

/// Asks the Quickseek windows (`quickseek.main`) running from `dir` to close
/// for an update, and waits up to 10 s for them to exit. Returns how many
/// closed; each starts again by itself after this process ends.
pub fn close_windows_for_update(dir: &Path) -> usize {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetWindowThreadProcessId, RegisterWindowMessageW,
        SMTO_ABORTIFHUNG, SendMessageTimeoutW,
    };
    let mut prefix = dir.to_string_lossy().to_lowercase();
    if !prefix.ends_with('\\') {
        prefix.push('\\');
    }
    struct Found(Vec<(HWND, u32)>);
    unsafe extern "system" fn each(h: HWND, l: LPARAM) -> i32 {
        let f = unsafe { &mut *(l as *mut Found) };
        let mut buf = [0u16; 32];
        let n = unsafe { GetClassNameW(h, buf.as_mut_ptr(), 32) } as usize;
        if String::from_utf16_lossy(&buf[..n]) == "quickseek.main" {
            let mut pid = 0;
            unsafe { GetWindowThreadProcessId(h, &mut pid) };
            f.0.push((h, pid));
        }
        1
    }
    let mut found = Found(Vec::new());
    unsafe { EnumWindows(Some(each), &mut found as *mut Found as LPARAM) };
    let msg = unsafe { RegisterWindowMessageW(wide(CLOSE_FOR_UPDATE).as_ptr()) };
    let mut waiting = Vec::new();
    for (h, pid) in found.0 {
        let p = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if p.is_null() {
            continue;
        }
        let mut buf = [0u16; MAX_PATH as usize * 2];
        let mut len = buf.len() as u32;
        let ours = unsafe {
            QueryFullProcessImageNameW(p, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len)
        } != 0
            && String::from_utf16_lossy(&buf[..len as usize])
                .to_lowercase()
                .starts_with(&prefix);
        let mut answer = 0usize;
        if ours
            && unsafe {
                SendMessageTimeoutW(
                    h,
                    msg,
                    std::process::id() as usize,
                    0,
                    SMTO_ABORTIFHUNG,
                    3000,
                    &mut answer,
                )
            } != 0
            && answer == 1
        {
            waiting.push(p);
        } else {
            unsafe { CloseHandle(p) };
        }
    }
    let n = waiting.len();
    for p in waiting {
        unsafe {
            WaitForSingleObject(p, 10_000);
            CloseHandle(p);
        }
    }
    n
}

pub fn kill_in(dir: &Path, names: &[&str]) -> usize {
    let mut prefix = dir.to_string_lossy().to_lowercase();
    if !prefix.ends_with('\\') {
        prefix.push('\\');
    }
    let me = std::process::id();
    let mut ended = 0;
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap.is_null() || snap as isize == -1 {
            return 0;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut e) != 0;
        while ok {
            let n = e
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..n]);
            if e.th32ProcessID != me && names.iter().any(|x| x.eq_ignore_ascii_case(&name)) {
                let h = OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                    0,
                    e.th32ProcessID,
                );
                if !h.is_null() {
                    let mut buf = [0u16; MAX_PATH as usize * 2];
                    let mut len = buf.len() as u32;
                    if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len)
                        != 0
                        && String::from_utf16_lossy(&buf[..len as usize])
                            .to_lowercase()
                            .starts_with(&prefix)
                        && TerminateProcess(h, 1) != 0
                    {
                        WaitForSingleObject(h, 2000);
                        ended += 1;
                    }
                    CloseHandle(h);
                }
            }
            ok = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    ended
}

/// Deletes the `*.old-<pid>` copies [`replace_file`] left in `dir` that are
/// no longer running (the rest wait for the reboot). Returns how many.
pub fn remove_leftovers(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| is_leftover(&e.file_name().to_string_lossy()))
        .filter(|e| std::fs::remove_file(e.path()).is_ok())
        .count()
}

fn is_leftover(name: &str) -> bool {
    name.rsplit_once(".old-").is_some_and(|(stem, pid)| {
        !stem.is_empty() && !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())
    })
}

/// After replacing the programs in `dir`: ends the MCP servers still running
/// old code from there and deletes leftovers. Returns (servers ended, files deleted).
pub fn retire_old_programs(dir: &Path) -> (usize, usize) {
    let ended = kill_in(dir, &[MCP_EXE]);
    (ended, remove_leftovers(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_that_hang_are_ended() {
        let mut slow = std::process::Command::new("cmd");
        slow.args(["/c", "ping -n 30 127.0.0.1 >nul"]);
        let t = std::time::Instant::now();
        let e = output_with_timeout(&mut slow, std::time::Duration::from_millis(500)).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::TimedOut);
        assert!(t.elapsed() < std::time::Duration::from_secs(10));
        let mut quick = std::process::Command::new("cmd");
        quick.args(["/c", "echo hi"]);
        let o = output_with_timeout(&mut quick, std::time::Duration::from_secs(30)).unwrap();
        assert!(o.status.success() && String::from_utf8_lossy(&o.stdout).contains("hi"));
    }

    /// A running "MCP server" (a copy of ping.exe) is moved aside, ended and cleaned up.
    #[test]
    fn replaces_a_running_program_and_retires_it() {
        let d = std::env::temp_dir().join(format!("quickseek-upgrade-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join(MCP_EXE);
        let ping =
            std::path::PathBuf::from(std::env::var("SystemRoot").unwrap_or(r"C:\Windows".into()))
                .join(r"System32\PING.EXE");
        std::fs::copy(&ping, &p).unwrap();
        let mut child = std::process::Command::new(&p)
            .args(["-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        assert!(replace_file(&p, b"new").unwrap(), "in use: moved aside");
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
        assert_eq!(
            std::fs::read_dir(&d).unwrap().count(),
            2,
            "the old copy waits for deletion"
        );
        assert_eq!(remove_leftovers(&d), 0, "still running");
        assert_eq!(
            kill_in(&d.with_file_name("quickseek-upgrade"), &[MCP_EXE]),
            0,
            "only programs in that folder"
        );
        assert_eq!(retire_old_programs(&d), (1, 1));
        assert!(child.try_wait().unwrap().is_some());
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1);
        assert!(
            !replace_file(&p, b"newer").unwrap(),
            "not in use: written in place"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn leftover_names() {
        assert!(is_leftover("quickseek-mcp.old-1234"));
        assert!(!is_leftover("quickseek-mcp.exe"));
        assert!(!is_leftover("notes.old-copy"));
        assert!(!is_leftover(".old-12"));
    }
}
