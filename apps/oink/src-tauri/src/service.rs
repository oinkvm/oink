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
/// SYSTEM pre-login backend, created only by OinkInput on the logon desktop.
pub const PRELOGIN_ARG: &str = "--prelogin";
pub const STOP_SERVICE_ARG: &str = "--stop-service";
pub const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Frontend API calls wait asynchronously while the broker retires the old
/// process. Shell menus and Quit remain independent of backend readiness.
#[derive(Debug)]
pub(crate) struct Startup(tokio::sync::watch::Sender<Option<Result<(), String>>>);
impl Startup {
    pub(crate) fn new(pending: bool) -> Self {
        Self(tokio::sync::watch::channel(if pending { None } else { Some(Ok(())) }).0)
    }
    pub(crate) fn ready(&self) -> bool {
        matches!(*self.0.borrow(), Some(Ok(())))
    }
    #[cfg(any(windows, test))]
    pub(crate) fn finish(&self, result: Result<(), String>) {
        self.0.send_replace(Some(result));
    }
    pub(crate) async fn wait(&self) -> Result<(), String> {
        let mut state = self.0.subscribe();
        loop {
            if let Some(result) = state.borrow().clone() {
                return result;
            }
            state
                .changed()
                .await
                .map_err(|_| "backend_startup_cancelled".to_owned())?;
        }
    }
}
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
        // Logging starts inside: its roots are fixed only after the entry checks.
        #[cfg(windows)]
        PRELOGIN_ARG => Some(ExitCode::from(oink_agent::prelogin::run())),
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
    let launched = match oink_agent::instance::is_running(api.data_dir()) {
        Some(true) => {
            seen_owner = true;
            None
        }
        Some(false) => Some(
            oink_agent::backend::launch::start_backend_tracked(api.data_dir())
                .map_err(|e| format!("UIAccess backend launch failed: {e}"))?,
        ),
        None => return Err("backend_ownership_unavailable".into()),
    };
    let _ = exe;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(process) = &launched
            && let Some(code) = process
                .exit_code()
                .map_err(|e| format!("backend exit query failed: {e}"))?
        {
            return Err(format!("backend_exited:{code}"));
        }
        // A live backend may still be retrying its endpoint publication after
        // Windows scanner denial. Keep reading the file while that owner lives;
        // the launch deadline only bounds acquiring ownership, not publication.
        let owned = match oink_agent::instance::is_running(api.data_dir()) {
            Some(true) => {
                seen_owner = true;
                true
            }
            Some(false) if seen_owner && launched.is_none() => return Err("backend_unavailable".into()),
            Some(false) if Instant::now() >= deadline => return Err("backend_start_timeout".into()),
            Some(false) => false,
            None => return Err("backend_ownership_unavailable".into()),
        };
        if !owned {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        api.reset_connections();
        match api.hello_with_timeout(Duration::from_millis(100)) {
            Ok(info) if launched.as_ref().is_none_or(|process| process.id() == info.pid) => {
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

/// Follows the backend for the UI's lifetime. There is no spawn path here and
/// no reconnect to a backend that went away. Two exceptions subscribe again
/// and let `resubscribed` reload every view, as the IPC contract describes: a
/// backend that is still running and dropped this subscription for falling
/// behind, and one that restarts in its own process after a network-layer
/// failure (it announces that, keeps its process ownership and comes back
/// on a new endpoint). Exiting instead left the user with no UI at all.
pub fn follow(
    api: &RemoteApi,
    connected: impl FnOnce(&ServiceInfo),
    resubscribed: impl Fn(&ServiceInfo),
    ended: impl FnOnce(EventsEnd),
) {
    let mut end = api.follow_events(connected);
    loop {
        match end {
            EventsEnd::Lagged => {
                tracing::warn!("backend dropped a lagging event subscription; subscribing again");
                end = api.follow_events(&resubscribed);
            }
            EventsEnd::Restarting => {
                tracing::info!("backend restarts after a network-layer failure; waiting to subscribe again");
                end = match wait_for_restart(api) {
                    Ok(()) => api.follow_events(&resubscribed),
                    Err(end) => end,
                };
            }
            _ => break,
        }
    }
    ended(end);
}

/// The longest restart back-off (30 s) plus a normal backend startup.
const RESTART_WAIT: Duration = Duration::from_secs(90);

/// Waits until the restarted backend answers on its new endpoint.
fn wait_for_restart(api: &RemoteApi) -> Result<(), EventsEnd> {
    let deadline = Instant::now() + RESTART_WAIT;
    loop {
        // The process keeps its ownership through the restart: losing it
        // means the backend exited instead of restarting.
        if oink_agent::instance::is_running(api.data_dir()) != Some(true) {
            return Err(EventsEnd::Lost("backend exited while restarting".into()));
        }
        api.reset_connections();
        if api.hello_with_timeout(Duration::from_millis(200)).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(EventsEnd::Lost("backend did not come back after its restart".into()));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
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

#[cfg(test)]
mod startup_wait_tests {
    use super::*;
    #[tokio::test]
    async fn pending_handoff_waits_without_reporting_backend_failure_and_wakes_all_requests() {
        let startup = std::sync::Arc::new(Startup::new(true));
        let mut waiting = Vec::new();
        for _ in 0..3 {
            let startup = startup.clone();
            waiting.push(tokio::spawn(async move { startup.wait().await }));
        }
        tokio::task::yield_now().await;
        assert!(!startup.ready());
        assert!(waiting.iter().all(|request| !request.is_finished()));
        startup.finish(Ok(()));
        for request in waiting {
            request.await.unwrap().unwrap();
        }
        assert!(startup.ready());
        startup.wait().await.unwrap();
    }
    #[tokio::test]
    async fn permanent_startup_failure_is_reported_without_losing_an_early_notification() {
        let startup = Startup::new(true);
        startup.finish(Err("launch denied".into()));
        assert_eq!(startup.wait().await, Err("launch denied".into()));
        assert!(!startup.ready());
    }
}
