//! The agent updating itself from the project's signed releases
//! (`QUICKSEEK_UPDATE`: `off`, `ask` (the default: only logs that there is
//! one) or `auto`).
//!
//! The image is read-only, so a new binary goes to the config volume:
//! `<config>/bin/quickseek-agent.zst` (exactly the release file) with the
//! release's `update.json` and `update.json.sig`. When it starts, the image's
//! binary checks that the staged one is newer, that the manifest's signature
//! verifies with the key built into *this* binary, and that the file matches
//! the manifest, and only then runs it (`exec`, same arguments and
//! environment). A staged binary that's no longer newer (the image itself
//! was updated) is deleted. After installing, the running agent saves its
//! indexes and execs into the new one, keeping its process (and container).

use std::path::{Path, PathBuf};
use std::time::Duration;

use quickseek_remote::update::{self as upd, Manifest};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const EVERY: Duration = Duration::from_secs(24 * 3600);
const FIRST_AFTER: Duration = Duration::from_secs(10 * 60);
/// Set in the environment of a staged binary run by the image's one.
const STAGED_ENV: &str = "QUICKSEEK_AGENT_STAGED";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    Ask,
    Auto,
}

impl std::str::FromStr for Mode {
    type Err = String;
    fn from_str(s: &str) -> Result<Mode, String> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "off" | "no" | "never" => Mode::Off,
            "ask" | "notify" => Mode::Ask,
            "auto" | "on" | "yes" => Mode::Auto,
            _ => return Err(format!("off, ask or auto, not {s}")),
        })
    }
}

fn bin(config: &Path) -> PathBuf {
    config.join("bin")
}

/// The release file for this machine.
pub fn file_name(version: &str) -> String {
    format!(
        "quickseek-agent-{version}-{}-linux-musl.zst",
        std::env::consts::ARCH
    )
}

/// The staged binary if it's newer than this one and checks out: (version, binary).
fn staged(config: &Path) -> Result<Option<(String, Vec<u8>)>, String> {
    let dir = bin(config);
    let (Ok(raw), Ok(sig)) = (
        std::fs::read(dir.join(upd::MANIFEST)),
        std::fs::read(dir.join(upd::SIGNATURE)),
    ) else {
        return Ok(None);
    };
    let m = upd::verify(&raw, &sig)?;
    if !upd::newer(&m.version, VERSION) {
        // The image caught up (or went past it): the staged copy is stale.
        let _ = std::fs::remove_dir_all(&dir);
        return Ok(None);
    }
    let name = file_name(&m.version);
    let f = m
        .file(&name)
        .ok_or_else(|| format!("the staged release has no {name}"))?;
    let z = std::fs::read(dir.join("quickseek-agent.zst")).map_err(|e| format!("{name}: {e}"))?;
    upd::check_file(&z, f)?;
    let exe = quickseek_remote::wire::decompress(&z).map_err(|e| format!("{name}: {e}"))?;
    Ok(Some((m.version, exe)))
}

/// At start: runs the staged newer binary instead of this one, if there is
/// a good one. Returns (to carry on with this binary) otherwise.
pub fn run_staged(config: &Path) {
    if std::env::var_os(STAGED_ENV).is_some() {
        return; // this is the staged one
    }
    match staged(config) {
        Ok(Some((version, exe))) => exec(config, &version, &exe),
        Ok(None) => {}
        Err(e) => log::warn!("update: the staged binary isn't used: {e}"),
    }
}

#[cfg(unix)]
fn exec(config: &Path, version: &str, exe: &[u8]) {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    // Written fresh from the checked file each time (what runs is what was checked).
    let path = bin(config).join("quickseek-agent");
    let tmp = bin(config).join(".quickseek-agent.tmp");
    let r = std::fs::write(&tmp, exe)
        .and_then(|_| std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)))
        .and_then(|_| std::fs::rename(&tmp, &path));
    if let Err(e) = r {
        log::warn!("update: writing {}: {e}", path.display());
        return;
    }
    log::info!("update: running quickseek-agent {version} (this is {VERSION})");
    let e = std::process::Command::new(&path)
        .args(std::env::args_os().skip(1))
        .env(STAGED_ENV, "1")
        .exec();
    log::warn!("update: running {}: {e}", path.display());
}

#[cfg(not(unix))]
fn exec(_config: &Path, _version: &str, _exe: &[u8]) {}

/// Downloads and stages the release in `m` for this machine.
fn install(config: &Path, raw: &[u8], sig: &[u8], m: &Manifest) -> Result<(), String> {
    let name = file_name(&m.version);
    let f = m
        .file(&name)
        .ok_or_else(|| format!("release {} has no {name}", m.version))?;
    let z = upd::download(m, f)?;
    // It must unpack before it's worth staging.
    quickseek_remote::wire::decompress(&z).map_err(|e| format!("{name}: {e}"))?;
    let dir = bin(config);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let put = |n: &str, d: &[u8]| {
        let p = dir.join(n);
        std::fs::write(&p, d).map_err(|e| format!("{}: {e}", p.display()))
    };
    // The manifest last: until it's there, nothing is staged.
    let _ = std::fs::remove_file(dir.join(upd::MANIFEST));
    put("quickseek-agent.zst", &z)?;
    put(upd::SIGNATURE, sig)?;
    put(upd::MANIFEST, raw)?;
    // What start-up will check, checked now.
    match staged(config)? {
        Some(_) => Ok(()),
        None => Err("the staged release didn't check out".into()),
    }
}

/// The update thread: looks once a day; in `auto` stages the new release and
/// asks the agent to restart into it (saving its indexes first).
pub fn run(mode: Mode, config: PathBuf) {
    if mode == Mode::Off {
        return;
    }
    std::thread::sleep(FIRST_AFTER);
    loop {
        match upd::fetch_latest() {
            Ok((raw, sig, m)) if upd::newer(&m.version, VERSION) => {
                if mode == Mode::Ask {
                    log::info!(
                        "update: quickseek-agent {} is available (this is {VERSION}); QUICKSEEK_UPDATE=auto installs updates",
                        m.version
                    );
                } else {
                    match install(&config, &raw, &sig, &m) {
                        Ok(()) => {
                            log::info!(
                                "update: quickseek-agent {} is staged; restarting into it",
                                m.version
                            );
                            crate::RESTART.store(true, std::sync::atomic::Ordering::Relaxed);
                            crate::TERM.store(true, std::sync::atomic::Ordering::Relaxed);
                            return;
                        }
                        Err(e) => log::warn!("update: {e}"),
                    }
                }
            }
            Ok(_) => log::debug!("update: {VERSION} is the newest release"),
            Err(e) => log::info!("update: looking for a new release: {e}"),
        }
        std::thread::sleep(EVERY);
    }
}

/// After the indexes are saved for a restart: the staged binary, or nothing.
pub fn restart(config: &Path) {
    match staged(config) {
        Ok(Some((version, exe))) => exec(config, &version, &exe),
        Ok(None) => log::warn!("update: nothing staged to restart into"),
        Err(e) => log::warn!("update: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_staged_is_fine_and_bad_stages_are_refused() {
        let dir = crate::tree::tests::tmp("selfupdate");
        assert!(staged(&dir).unwrap().is_none());
        let b = bin(&dir);
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(
            b.join(upd::MANIFEST),
            br#"{"product":"quickseek","version":"999.0.0","files":[]}"#,
        )
        .unwrap();
        std::fs::write(b.join(upd::SIGNATURE), [0u8; 64]).unwrap();
        assert!(staged(&dir).is_err(), "an unsigned manifest is refused");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn modes_parse() {
        assert_eq!("AUTO".parse::<Mode>().unwrap(), Mode::Auto);
        assert_eq!("ask".parse::<Mode>().unwrap(), Mode::Ask);
        assert!("sometimes".parse::<Mode>().is_err());
        assert!(file_name("1.2.3").starts_with("quickseek-agent-1.2.3-"));
    }
}
