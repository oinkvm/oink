//! The UI starts the Windows backend once, then follows one IPC subscription.
//! Ending that subscription ends the UI; this module never restarts a crashed backend.
use oink_agent::ipc::ServiceInfo;
use oink_agent::remote::{EventsEnd, RemoteApi};
use std::path::Path;
#[cfg(windows)]
use std::process::Command;
use std::process::ExitCode;
use std::time::{Duration, Instant};

pub const SERVICE_ARG: &str = "--service";
pub const STOP_SERVICE_ARG: &str = "--stop-service";
pub const STOP_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(windows)]
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// 程序入口：按命令行参数选择服务进程、停止服务或界面。返回 `None` 表示运行界面。
pub fn dispatch_args() -> Option<ExitCode> {
    let arg = std::env::args().nth(1)?;
    match arg.as_str() {
        #[cfg(windows)]
        oink_agent::backend::REOPEN_FLAG => {
            let result = wait_previous_pair(&oink_foundation::paths::data_dir()).and_then(|()| {
                let exe = std::env::current_exe().map_err(|e| e.to_string())?;
                // Persistent UI command lines stay argument-free, as required by the
                // installation role checks; --reopen belongs only to this waiting entry.
                // A tray started at login carries this marker for its lifetime.
                // Explicit reopen must show the new window even in that case.
                let mut command = Command::new(exe);
                command.env_remove(oink_autostart::LOGIN_LAUNCH_ENV);
                oink_agent::backend::launch::spawn_detached(&mut command)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            });
            Some(match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("{error}");
                    ExitCode::FAILURE
                }
            })
        }
        #[cfg(windows)]
        SERVICE_ARG => {
            oink_agent::logs::init_service();
            Some(ExitCode::from(oink_agent::host::run_service()))
        }
        #[cfg(windows)]
        STOP_SERVICE_ARG => {
            oink_agent::logs::init();
            let dir = oink_foundation::paths::data_dir();
            Some(match oink_agent::host::stop_service(&dir, STOP_TIMEOUT) {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => {
                    tracing::error!("service process did not exit within {STOP_TIMEOUT:?}");
                    ExitCode::from(2)
                }
                Err(e) => {
                    tracing::error!("stop service: {e:#}");
                    ExitCode::FAILURE
                }
            })
        }
        _ => None,
    }
}

/// Startup only; a Windows UI requests the SYSTEM launch broker exactly once.
#[cfg(windows)]
pub fn start_once(api: &RemoteApi, exe: &Path) -> Result<(), String> {
    let mut seen_owner = false;
    match oink_agent::instance::is_running(api.data_dir()) {
        Some(true) => seen_owner = true,
        Some(false) => {
            oink_agent::backend::launch::start_backend(api.data_dir())
                .map_err(|e| format!("UIAccess backend launch failed: {e}"))?;
        }
        None => return Err("backend_ownership_unavailable".into()),
    }
    let _ = exe;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        // A live backend may still be retrying its endpoint publication after
        // Windows scanner denial. Keep reading the file while that owner lives;
        // the launch deadline only bounds acquiring ownership, not publication.
        let owned = match oink_agent::instance::is_running(api.data_dir()) {
            Some(true) => {
                seen_owner = true;
                true
            }
            Some(false) if seen_owner => return Err("backend_unavailable".into()),
            Some(false) if Instant::now() >= deadline => return Err("backend_start_timeout".into()),
            Some(false) => false,
            None => return Err("backend_ownership_unavailable".into()),
        };
        api.reset_connections();
        match api.hello() {
            Ok(_) if owned => {
                return validate_running_backend(api);
            }
            Ok(_) => {}
            Err(error) if api.build_mismatch() => return Err(error.to_string()),
            Err(_) => {}
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(windows)]
fn validate_running_backend(_api: &RemoteApi) -> Result<(), String> {
    #[cfg(windows)]
    {
        use oink_agent::api::AgentApi;
        if _api.status().map_err(|e| e.to_string())?.backend_uiaccess != Some(true) {
            return Err("现有后台没有 UIAccess；请退出后通过 OinkInput 系统服务重新打开".into());
        }
    }
    Ok(())
}

/// An endpoint file is never evidence that a backend process owns this home.
pub fn check_backend_ownership(data_dir: &Path) -> Result<(), String> {
    match oink_agent::instance::is_running(data_dir) {
        Some(true) => Ok(()),
        Some(false) => Err("backend_unavailable".into()),
        None => Err("backend_ownership_unavailable".into()),
    }
}

/// One subscription for the UI's lifetime. There is no reconnect or spawn path here.
pub fn follow(api: &RemoteApi, connected: impl FnOnce(&ServiceInfo), ended: impl FnOnce(EventsEnd)) {
    ended(api.follow_events(connected));
}

/// Explicit reopen waits for both roles before creating their replacements.
pub fn wait_previous_pair(data_dir: &Path) -> Result<(), String> {
    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        let backend = oink_agent::instance::is_running(data_dir).ok_or("backend_ownership_unavailable")?;
        let ui = oink_agent::instance::is_ui_running(data_dir).ok_or("ui_ownership_unavailable")?;
        if !backend && !ui {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("原有 Oink 进程未退出".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Only a user's interactive token may host WebView2. Administrator launches
/// (including the sole elevated token with UAC disabled) are supported. UIAccess
/// is reserved for the input backend: WebView content never needs its UIPI bypass.
#[cfg(windows)]
pub fn require_normal_user_ui() -> std::io::Result<()> {
    oink_winsvc::backend::require_interactive_ui()
}
