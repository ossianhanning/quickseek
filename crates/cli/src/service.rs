//! Windows service: runs elevated (LocalSystem), owns the index for all NTFS
//! volumes, keeps it live from the USN journal, and answers searches over the
//! `\\.\pipe\rapidfind` named pipe, so the CLI and UI can run unelevated.
//!
//! Install layout (all under a directory only SYSTEM/Administrators can write,
//! so nobody else can swap the binary a SYSTEM service runs):
//!   %ProgramData%\rapidfind\rapidfind.exe   copy of the installing binary
//!   %ProgramData%\rapidfind\index\C.db ...  saved indexes
//!   %ProgramData%\rapidfind\service.log

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Context, bail};
use rapidfind_core::engine::{Engine, EngineOptions};
use rapidfind_core::{ipc, persist};
use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceInfo,
    ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, service_dispatcher};

pub const NAME: &str = "rapidfind";
const DISPLAY: &str = "rapidfind file index";
const DESCRIPTION: &str = "Indexes NTFS volumes from the MFT and USN journal and answers filename searches over \\\\.\\pipe\\rapidfind.";

pub fn home() -> PathBuf {
    let base = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("rapidfind")
}

fn installed_exe() -> PathBuf {
    home().join("rapidfind.exe")
}

fn manager(access: ServiceManagerAccess) -> anyhow::Result<ServiceManager> {
    ServiceManager::local_computer(None::<&str>, access).context("can't open the Service Control Manager (run elevated)")
}

pub fn install() -> anyhow::Result<()> {
    let dir = home();
    ipc::create_private_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let src = std::env::current_exe()?;
    let dst = installed_exe();
    if src != dst {
        std::fs::copy(&src, &dst).with_context(|| format!("copying binary to {} (is the service running? stop it first)", dst.display()))?;
    }
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
    let svc = m.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START).context("creating service")?;
    svc.set_description(DESCRIPTION)?;
    svc.start(&[] as &[&OsStr]).context("starting service")?;
    println!("installed and started service '{NAME}' ({})", dst.display());
    Ok(())
}

/// Copies the current binary over the installed one (stopping/restarting the service).
pub fn update() -> anyhow::Result<()> {
    let was_running = matches!(query_state()?, Some(ServiceState::Running | ServiceState::StartPending));
    if was_running {
        stop()?;
    }
    std::fs::copy(std::env::current_exe()?, installed_exe()).context("copying binary")?;
    println!("updated {}", installed_exe().display());
    if was_running {
        start()?;
    }
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    let m = manager(ServiceManagerAccess::CONNECT)?;
    let svc = m
        .open_service(NAME, ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE)
        .context("opening service (is it installed?)")?;
    if svc.query_status()?.current_state != ServiceState::Stopped {
        let _ = svc.stop();
        wait_for(&svc, ServiceState::Stopped)?;
    }
    svc.delete()?;
    println!("uninstalled service '{NAME}' (index files left in {})", home().display());
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

/// Entry point for `rapidfind service run`, invoked by the SCM.
pub fn run() -> anyhow::Result<()> {
    service_dispatcher::start(NAME, ffi_service_main).context("not started by the Service Control Manager (use `rapidfind service start`)")
}

fn init_logging() {
    let dir = home();
    let _ = ipc::create_private_dir(&dir);
    let mut b = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("service.log")) {
        b.target(env_logger::Target::Pipe(Box::new(f)));
    }
    let _ = b.try_init();
}

fn service_main(_args: Vec<OsString>) {
    init_logging();
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
    unsafe { std::env::set_var("RAPIDFIND_SERVICE", "1") };
    let engine = Arc::new(Engine::new(EngineOptions {
        config_path: Some(home().join("config.json")),
        runs_path: Some(home().join("runs.json")),
        ..Default::default()
    }));
    let slot: Arc<OnceLock<Arc<Engine>>> = Arc::new(OnceLock::new());
    let _ = slot.set(engine.clone());
    let server = std::thread::Builder::new().name("pipe".into()).spawn(move || {
        if let Err(e) = ipc::serve(ipc::engine_handler(slot)) {
            log::error!("pipe server failed: {e} (is another process holding {}?)", ipc::PIPE_NAME);
        }
    })?;

    let eng = engine.clone();
    std::thread::Builder::new().name("volumes".into()).spawn(move || {
        log::info!("loading indexes");
        eng.sync_volumes();
        if let Err(err) = eng.save_all(rapidfind_core::engine::SAVE_THRESHOLD) {
            log::error!("save failed: {err:#}");
        }
        log::info!("ready");
        loop {
            std::thread::sleep(Duration::from_secs(3));
            eng.sync_if_changed();
        }
    })?;

    // Wait for stop (or the pipe server dying, which makes the service useless).
    loop {
        if stop_rx.recv_timeout(Duration::from_secs(1)).is_ok() || server.is_finished() {
            break;
        }
    }
    handle.set_service_status(status(ServiceState::StopPending, ServiceControlAccept::empty(), Duration::from_secs(60)))?;
    log::info!("saving indexes");
    if let Err(err) = engine.save_all(1) {
        log::error!("save failed: {err:#}");
    }
    log::info!("stopped");
    handle.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty(), Duration::ZERO))?;
    // Journal watcher threads are blocked in the kernel; just exit.
    std::process::exit(0);
}
