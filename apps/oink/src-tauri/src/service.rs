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
    match oink_agent::instance::is_running(api.data_dir()) {
        Some(true) => {
            api.hello().map_err(|_| "backend_unavailable".to_owned())?;
            return validate_running_backend(api);
        }
        Some(false) => {
            oink_agent::backend::launch::start_backend(api.data_dir())
                .map_err(|e| format!("UIAccess backend launch failed: {e}"))?;
        }
        None => return Err("backend_ownership_unavailable".into()),
    }
    let _ = exe;
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        api.reset_connections();
        match api.hello() {
            Ok(_) if oink_agent::instance::is_running(api.data_dir()) == Some(true) => {
                return validate_running_backend(api);
            }
            Ok(_) if Instant::now() >= deadline => return Err("backend_ownership_unavailable".into()),
            Ok(_) => {}
            Err(error) if api.build_mismatch() => return Err(error.to_string()),
            Err(error) if Instant::now() >= deadline => return Err(format!("后台启动超时：{error}")),
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

/// The interactive shell never inherits SYSTEM, administrator elevation or
/// UIAccess. Installer custom actions cannot accidentally create a user WebView.
#[cfg(windows)]
pub fn require_normal_user_ui() -> std::io::Result<()> {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::{
        Foundation::CloseHandle,
        Security::{GetTokenInformation, TOKEN_QUERY, TokenElevation, TokenSessionId, TokenUIAccess},
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    let mut token = HANDLE::default();
    // SAFETY: current-process pseudo-handle, valid token and DWORD output buffers.
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)?;
        let query = |class| -> windows::core::Result<u32> {
            let mut value = 0u32;
            let mut len = 0;
            GetTokenInformation(token, class, Some((&mut value as *mut u32).cast()), 4, &mut len)?;
            Ok(value)
        };
        let result = (|| -> windows::core::Result<bool> {
            Ok(normal_user_ui(
                query(TokenElevation)?,
                query(TokenUIAccess)?,
                query(TokenSessionId)?,
            ))
        })();
        let _ = CloseHandle(token);
        if result? {
            Ok(())
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "UI requires an unelevated interactive user token",
            ))
        }
    }
}

#[cfg(any(windows, test))]
fn normal_user_ui(elevated: u32, uiaccess: u32, session: u32) -> bool {
    elevated == 0 && uiaccess == 0 && session != 0
}

#[cfg(test)]
mod token_tests {
    #[test]
    fn installer_and_backend_tokens_cannot_launch_the_shell() {
        assert!(super::normal_user_ui(0, 0, 1));
        for token in [(1, 0, 1), (0, 1, 1), (0, 0, 0), (1, 1, 0)] {
            assert!(!super::normal_user_ui(token.0, token.1, token.2));
        }
    }
}
