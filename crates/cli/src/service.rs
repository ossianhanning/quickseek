//! Windows service: runs elevated (LocalSystem), owns the index for all NTFS
//! volumes, keeps it live from the USN journal, and answers searches over the
//! `\\.\pipe\quickseek` named pipe, so the CLI and UI can run unelevated.
//!
//! Install layout (all under a directory only SYSTEM/Administrators can write,
//! so nobody else can swap the binary a SYSTEM service runs):
//!   %ProgramData%\quickseek\quickseek.exe   copy of the installing binary
//!   %ProgramData%\quickseek\index\C.db ...  saved indexes
//!   %ProgramData%\quickseek\service.log
//!
//! The service runs its own copy rather than the one in the install folder
//! because that folder can be anywhere (`quickseek-setup --dir`, a build
//! folder), and a folder off a drive's root inherits write access for every
//! user, which would let anyone replace what LocalSystem runs. The installer
//! makes the copy from the install folder's binary; installing or updating
//! from anywhere else (a development build) updates the install folder too
//! (see [`sync_install_dir`]), so the service, CLI, GUI and MCP server stay
//! the same build.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, bail};
use quickseek_core::engine::{Engine, EngineOptions};
use quickseek_core::{ipc, persist, upgrade};
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

pub const NAME: &str = "quickseek";
const DISPLAY: &str = "Quickseek file index";
const DESCRIPTION: &str = "Indexes NTFS volumes from the MFT and USN journal and answers filename searches over \\\\.\\pipe\\quickseek.";

pub fn home() -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("quickseek")
}

fn installed_exe() -> PathBuf {
    home().join("quickseek.exe")
}

fn manager(access: ServiceManagerAccess) -> anyhow::Result<ServiceManager> {
    ServiceManager::local_computer(None::<&str>, access)
        .context("can't open the Service Control Manager (run elevated)")
}

/// When this binary isn't the installed one (a development build): copies
/// it, and the GUI and MCP server next to it, into the install folder, so
/// everything there is the same build as the service. Returns whether it did.
fn sync_install_dir() -> anyhow::Result<bool> {
    if std::env::var_os(upgrade::NO_SYNC_ENV).is_some() {
        return Ok(false);
    }
    let Some(dir) = upgrade::install_dir() else {
        return Ok(false);
    };
    let me = std::env::current_exe()?;
    let Some(src) = me.parent() else {
        return Ok(false);
    };
    if upgrade::same_dir(src, &dir) {
        return Ok(false);
    }
    let gui = "quickseek-gui.exe";
    if std::fs::read(src.join(gui))
        .ok()
        .is_some_and(|new| std::fs::read(dir.join(gui)).ok() != Some(new))
    {
        let closed = upgrade::close_windows_for_update(&dir);
        if closed > 0 {
            println!("closed {closed} Quickseek window(s); they start again when this ends");
        }
    }
    let mut any = false;
    for name in upgrade::PROGRAMS {
        let (from, to) = (src.join(name), dir.join(name));
        if !from.exists() {
            continue;
        }
        let data = std::fs::read(&from).with_context(|| format!("reading {}", from.display()))?;
        if std::fs::read(&to).is_ok_and(|old| old == data) {
            continue;
        }
        upgrade::replace_file(&to, &data).map_err(anyhow::Error::msg)?;
        println!("updated {}", to.display());
        any = true;
    }
    Ok(any)
}

/// After the programs changed: MCP servers that AI apps started from the
/// install folder still run the old code; end them (the apps start the new
/// one on their next call) and delete moved-aside copies.
fn retire_old_programs() {
    if let Some(dir) = upgrade::install_dir() {
        let (ended, _) = upgrade::retire_old_programs(&dir);
        if ended > 0 {
            println!("restarted {ended} running quickseek-mcp server(s)");
        }
    }
}

/// Moves an install of rapidfind (Quickseek's former name) over, before
/// Quickseek's service is installed or updated: removes the old service,
/// moves its indexes, snapshot history and settings, connects the AI
/// assistants it was connected to to Quickseek instead (and removes its
/// skills), and removes the old programs, shortcut, PATH and Apps & Features
/// entries. Does nothing when there's no old install.
pub fn migrate_from_rapidfind() -> anyhow::Result<()> {
    use quickseek_core::legacy;
    use quickseek_mcp::register as reg;
    let roots = legacy::Roots::from_env();
    let m = manager(ServiceManagerAccess::CONNECT)?;
    let old = m
        .open_service(
            legacy::OLD_SERVICE,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )
        .ok();
    if old.is_none() && !legacy::present(&roots) {
        return Ok(());
    }
    let from_installer = std::env::var_os(upgrade::NO_SYNC_ENV).is_some();
    legacy::check_may_move(legacy::old_install_dir().as_deref(), from_installer)
        .map_err(anyhow::Error::msg)?;
    println!("moving rapidfind (Quickseek's former name) over:");
    if let Some(svc) = old {
        if svc.query_status()?.current_state != ServiceState::Stopped {
            let _ = svc.stop();
            wait_for(&svc, ServiceState::Stopped)?;
        }
        svc.delete().context("removing the rapidfind service")?;
        drop(svc);
        println!("  removed the rapidfind service");
    }
    let ended = legacy::end_old_programs(&roots);
    if ended > 0 {
        println!("  ended {ended} running rapidfind program(s)");
    }
    // The service's process can hold its files a moment after it stopped.
    let mut tries = 0;
    let moved = loop {
        let log = legacy::move_data(&roots);
        if !log.iter().any(|l| l.starts_with("couldn't")) || tries == 20 {
            break log;
        }
        tries += 1;
        std::thread::sleep(Duration::from_millis(250));
    };
    for l in moved {
        println!("  {l}");
    }
    let me = std::env::current_exe()?;
    let here = me
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default();
    let mcp = here.join(reg::EXE);
    if mcp.exists() {
        let exe = reg::plain_path(reg::stable_exe(&mcp).map_err(anyhow::Error::msg)?);
        for c in reg::ALL {
            match reg::migrate_legacy(c, &exe) {
                Ok(true) => println!("  {}: now connected to Quickseek", c.label()),
                Ok(false) => {}
                Err(e) => println!("  {}: {e}", c.label()),
            }
        }
    }
    let gui = here.join("quickseek-gui.exe");
    for l in legacy::remove_old_install(gui.exists().then_some(gui.as_path())) {
        println!("  {l}");
    }
    Ok(())
}

pub fn install() -> anyhow::Result<()> {
    migrate_from_rapidfind()?;
    let dir = home();
    ipc::create_private_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let src = std::env::current_exe()?;
    let dst = installed_exe();
    if src != dst {
        std::fs::copy(&src, &dst).with_context(|| {
            format!(
                "copying binary to {} (is the service running? stop it first)",
                dst.display()
            )
        })?;
    }
    let synced = sync_install_dir()?;
    let m = manager(ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE)?;
    let info = ServiceInfo {
        name: NAME.into(),
        display_name: DISPLAY.into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: dst.clone(),
        launch_arguments: vec![OsString::from("service"), OsString::from("run")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let svc = m
        .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)
        .context("creating service")?;
    svc.set_description(DESCRIPTION)?;
    svc.start(&[] as &[&OsStr]).context("starting service")?;
    println!(
        "installed and started service '{NAME}' ({}, {})",
        dst.display(),
        ipc::BUILD
    );
    if synced {
        retire_old_programs();
    }
    Ok(())
}

/// Copies the current binary over the installed one (stopping/restarting the service).
pub fn update() -> anyhow::Result<()> {
    migrate_from_rapidfind()?;
    let was_running = matches!(
        query_state()?,
        Some(ServiceState::Running | ServiceState::StartPending)
    );
    if was_running {
        stop()?;
    }
    let changed =
        std::fs::read(installed_exe()).ok() != Some(std::fs::read(std::env::current_exe()?)?);
    // SERVICE_STOPPED is reported before the process has exited, so the old
    // binary can stay locked for a moment.
    let mut tries = 0;
    while let Err(e) = std::fs::copy(std::env::current_exe()?, installed_exe()) {
        tries += 1;
        if tries > 100 || e.raw_os_error() != Some(32) {
            return Err(e).context("copying binary");
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    println!("updated {} ({})", installed_exe().display(), ipc::BUILD);
    let synced = sync_install_dir();
    if was_running {
        start()?;
    }
    if changed || matches!(synced, Ok(true)) {
        retire_old_programs();
    }
    synced.map(|_| ())
}

pub fn uninstall() -> anyhow::Result<()> {
    let m = manager(ServiceManagerAccess::CONNECT)?;
    let svc = m
        .open_service(
            NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
        )
        .context("opening service (is it installed?)")?;
    if svc.query_status()?.current_state != ServiceState::Stopped {
        let _ = svc.stop();
        wait_for(&svc, ServiceState::Stopped)?;
    }
    svc.delete()?;
    println!(
        "uninstalled service '{NAME}' (index files left in {})",
        home().display()
    );
    Ok(())
}

fn wait_for(svc: &windows_service::service::Service, state: ServiceState) -> anyhow::Result<()> {
    for _ in 0..300 {
        if svc.query_status()?.current_state == state {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("service did not reach {state:?} in 30 s")
}

pub fn start() -> anyhow::Result<()> {
    let m = manager(ServiceManagerAccess::CONNECT)?;
    let svc = m.open_service(NAME, ServiceAccess::START | ServiceAccess::QUERY_STATUS)?;
    svc.start(&[] as &[&OsStr])?;
    wait_for(&svc, ServiceState::Running)?;
    println!("service started");
    Ok(())
}

pub fn stop() -> anyhow::Result<()> {
    let m = manager(ServiceManagerAccess::CONNECT)?;
    let svc = m.open_service(NAME, ServiceAccess::STOP | ServiceAccess::QUERY_STATUS)?;
    svc.stop()?;
    wait_for(&svc, ServiceState::Stopped)?;
    println!("service stopped");
    Ok(())
}

/// `None` if not installed. Works unelevated.
pub fn query_state() -> anyhow::Result<Option<ServiceState>> {
    let m = manager(ServiceManagerAccess::CONNECT)?;
    match m.open_service(NAME, ServiceAccess::QUERY_STATUS) {
        Ok(svc) => Ok(Some(svc.query_status()?.current_state)),
        Err(_) => Ok(None),
    }
}

// ---- running as a service ------------------------------------------------------

define_windows_service!(ffi_service_main, service_main);

/// Entry point for `quickseek service run`, invoked by the SCM.
pub fn run() -> anyhow::Result<()> {
    service_dispatcher::start(NAME, ffi_service_main)
        .context("not started by the Service Control Manager (use `quickseek service start`)")
}

fn init_logging() {
    let dir = home();
    let _ = ipc::create_private_dir(&dir);
    let mut b = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if let Ok(f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("service.log"))
    {
        b.target(env_logger::Target::Pipe(Box::new(f)));
    }
    let _ = b.try_init();
}

fn service_main(_args: Vec<OsString>) {
    init_logging();
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    log::info!(
        "quickseek service {} starting ({exe}, protocol v{}-v{}, pid {})",
        ipc::BUILD,
        ipc::MIN_PROTOCOL,
        ipc::PROTOCOL,
        std::process::id()
    );
    if let Err(e) = run_service() {
        log::error!("service failed: {e:#}");
    }
}

fn status(state: ServiceState, accept: ServiceControlAccept, wait: Duration) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: wait,
        process_id: None,
    }
}

fn run_service() -> anyhow::Result<()> {
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let handle = service_control_handler::register(NAME, move |c| match c {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;
    let accept = ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN;
    handle.set_service_status(status(ServiceState::Running, accept, Duration::ZERO))?;

    let index_dir = home().join("index");
    ipc::create_private_dir(&index_dir)?;
    persist::set_db_dir(index_dir);

    // The engine exists right away (searches cover whatever volumes have
    // finished loading); a background loop loads volumes and keeps up with
    // drives being attached/removed and config changes.
    // SAFETY: set before any other thread is started.
    unsafe { std::env::set_var("QUICKSEEK_SERVICE", "1") };
    let engine = Arc::new(Engine::new(EngineOptions {
        config_path: Some(home().join("config.json")),
        runs_path: Some(home().join("runs.json")),
        ..Default::default()
    }));
    engine.enable_history(home().join("history"));
    let slot: Arc<OnceLock<Arc<Engine>>> = Arc::new(OnceLock::new());
    let _ = slot.set(engine.clone());
    let server = std::thread::Builder::new()
        .name("pipe".into())
        .spawn(move || {
            if let Err(e) = ipc::serve(ipc::engine_handler(slot)) {
                log::error!(
                    "pipe server failed: {e} (is another process holding {}?)",
                    ipc::PIPE_NAME
                );
            }
        })?;

    let eng = engine.clone();
    std::thread::Builder::new()
        .name("volumes".into())
        .spawn(move || {
            log::info!("loading indexes");
            eng.sync_volumes();
            if let Err(err) = eng.save_all(quickseek_core::engine::SAVE_THRESHOLD) {
                log::error!("save failed: {err:#}");
            }
            log::info!("ready");
            loop {
                std::thread::sleep(Duration::from_secs(3));
                eng.sync_if_changed();
                eng.maybe_snapshot();
                // Volumes rebuilt by that sync (u64::MAX = never saved) get saved right away.
                if let Err(err) = eng.save_all(u64::MAX) {
                    log::error!("save failed: {err:#}");
                }
            }
        })?;

    // Wait for stop (or the pipe server dying, which makes the service useless).
    loop {
        if stop_rx.recv_timeout(Duration::from_secs(1)).is_ok() || server.is_finished() {
            break;
        }
    }
    handle.set_service_status(status(
        ServiceState::StopPending,
        ServiceControlAccept::empty(),
        Duration::from_secs(60),
    ))?;
    log::info!("saving indexes");
    if let Err(err) = engine.save_all(1) {
        log::error!("save failed: {err:#}");
    }
    log::info!("stopped");
    handle.set_service_status(status(
        ServiceState::Stopped,
        ServiceControlAccept::empty(),
        Duration::ZERO,
    ))?;
    // Journal watcher threads are blocked in the kernel; just exit.
    std::process::exit(0);
}
