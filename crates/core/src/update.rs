//! Updates on Windows: the service looks for a new signed release once a day
//! (config `update`: `off`, `ask` (the default) or `auto`), and installs it
//! when told to (`quickseek update`) or by itself in `auto`.
//!
//! The manifest's signature, the version (only newer than this build) and
//! the installer's SHA-256 and size are checked by
//! [`quickseek_remote::update`] before anything runs; the installer is
//! checked again after it's written to disk, then started as
//! `quickseek-setup-<version>-x64.exe --update`, which replaces the files and
//! restarts the service (ending this process) and leaves the shortcut, PATH,
//! startup entry and AI clients as they are.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use quickseek_remote::update::{self as upd, Manifest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::UpdateMode;
use crate::engine::Engine;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// How often the service looks.
const EVERY: Duration = Duration::from_secs(24 * 3600);
/// The first look after the service starts (not while it loads its indexes).
const FIRST_AFTER: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    /// Unix seconds of the last look.
    checked: i64,
    /// The newest release seen then.
    latest: String,
    /// An install of this version was started at `tried_at` (retried a day later).
    tried: String,
    tried_at: i64,
}

#[derive(Default)]
struct State {
    saved: Saved,
    error: Option<String>,
    manifest: Option<Manifest>,
    installing: Option<String>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
/// One check or install at a time.
static BUSY: Mutex<()> = Mutex::new(());

fn dir() -> PathBuf {
    crate::persist::db_dir().with_file_name("updates")
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut g = STATE.lock();
    let st = g.get_or_insert_with(|| State {
        saved: std::fs::read(dir().join("state.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default(),
        ..Default::default()
    });
    f(st)
}

fn save(s: &Saved) {
    let d = dir();
    if crate::ipc::create_private_dir(&d).is_ok() {
        let _ = std::fs::write(d.join("state.json"), serde_json::to_vec_pretty(s).unwrap());
    }
}

/// The installer in a release's manifest.
fn installer(m: &Manifest) -> Option<&upd::FileEntry> {
    m.file(&format!("quickseek-setup-{}-x64.exe", m.version))
}

/// Looks for a new release now.
pub fn check() -> Result<Option<Manifest>, String> {
    let _one = BUSY.lock();
    let r = upd::fetch_latest();
    with(|st| {
        match &r {
            Ok((_, _, m)) => {
                st.saved.checked = now();
                st.saved.latest = m.version.clone();
                st.error = None;
                st.manifest = Some(m.clone());
            }
            Err(e) => {
                // Offline, or GitHub having a bad moment: again in an hour.
                st.saved.checked = now() - EVERY.as_secs() as i64 + 3600;
                st.error = Some(e.clone());
            }
        }
        save(&st.saved);
    });
    let (_, _, m) = r?;
    if !upd::newer(&m.version, VERSION) {
        return Ok(None);
    }
    if installer(&m).is_none() {
        return Err(format!("release {} has no Windows installer", m.version));
    }
    log::info!(
        "update: Quickseek {} is available (this is {VERSION})",
        m.version
    );
    Ok(Some(m))
}

/// Downloads, checks and starts the installer of `m`.
pub fn install(m: &Manifest) -> Result<(), String> {
    let _one = BUSY.lock();
    if !upd::newer(&m.version, VERSION) {
        return Err(format!("{} isn't newer than this {VERSION}", m.version));
    }
    let f = installer(m).ok_or("the release has no Windows installer")?;
    log::info!("update: downloading {}", f.name);
    let data = upd::download(m, f)?;
    let d = dir();
    crate::ipc::create_private_dir(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    // Older downloads go.
    if let Ok(rd) = std::fs::read_dir(&d) {
        for e in rd.flatten() {
            if e.file_name().to_string_lossy().ends_with(".exe") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let path = d.join(&f.name);
    std::fs::write(&path, &data).map_err(|e| format!("{}: {e}", path.display()))?;
    // What runs is what was checked (the folder is SYSTEM and administrators only).
    upd::check_file(&std::fs::read(&path).map_err(|e| e.to_string())?, f)?;
    with(|st| {
        st.saved.tried = m.version.clone();
        st.saved.tried_at = now();
        st.installing = Some(m.version.clone());
        save(&st.saved);
    });
    log::info!("update: starting {} --update", path.display());
    start_detached(&path)?;
    Ok(())
}

fn start_detached(path: &std::path::Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let run = |flags: u32| {
        std::process::Command::new(path)
            .arg("--update")
            .creation_flags(flags)
            .spawn()
    };
    // Outlive the service, which the installer stops; breaking away from a
    // job isn't always allowed.
    run(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB)
        .or_else(|_| run(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP))
        .map(|_| ())
        .map_err(|e| format!("starting the installer: {e}"))
}

/// The service's updater: looks once a day; installs in `auto`.
pub fn run(eng: Arc<Engine>) {
    std::thread::sleep(FIRST_AFTER);
    loop {
        let mode = eng.config().update;
        let due = with(|st| now() - st.saved.checked >= EVERY.as_secs() as i64);
        if mode != UpdateMode::Off && due {
            match check() {
                Ok(Some(m)) if mode == UpdateMode::Auto => {
                    // A day between attempts at the same version (an install
                    // that failed shouldn't loop).
                    let retry = with(|st| {
                        st.saved.tried != m.version
                            || now() - st.saved.tried_at > EVERY.as_secs() as i64
                    });
                    if retry && let Err(e) = install(&m) {
                        log::warn!("update: {e}");
                        with(|st| {
                            st.error = Some(e);
                            st.installing = None;
                        });
                    }
                }
                Ok(_) => {}
                Err(e) => log::info!("update: looking for a new release: {e}"),
            }
        }
        std::thread::sleep(Duration::from_secs(600));
    }
}

fn status(mode: UpdateMode) -> Value {
    with(|st| {
        let latest = &st.saved.latest;
        json!({
            "version": VERSION,
            "mode": mode.label(),
            "checked": st.saved.checked,
            "latest": latest,
            "available": !latest.is_empty() && upd::newer(latest, VERSION),
            "installing": st.installing,
            "error": st.error,
        })
    })
}

/// IPC actions (through [`crate::remote::action`]):
/// - `{"op":"update"}` → this version, the mode, the newest release seen;
/// - `{"op":"update_check"}` looks now;
/// - `{"op":"update_install"}` looks if needed and installs a newer one.
pub fn action(eng: &Engine, op: &str) -> Result<Value, String> {
    let mode = eng.config().update;
    match op {
        "update" => Ok(status(mode)),
        "update_check" => {
            check()?;
            Ok(status(mode))
        }
        "update_install" => {
            let m = match with(|st| st.manifest.clone()) {
                Some(m) if upd::newer(&m.version, VERSION) => m,
                _ => {
                    check()?.ok_or_else(|| format!("Quickseek {VERSION} is the newest release"))?
                }
            };
            install(&m)?;
            Ok(json!({"installing": m.version}))
        }
        _ => Err(format!("unknown action {op:?}")),
    }
}
