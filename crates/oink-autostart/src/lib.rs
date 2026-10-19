//! OS login item registration and login-launch detection.
//!
//! - **状态以系统为准**：不另存开关。[`status`] 每次都问系统（macOS 的 `SMAppService.mainApp`、Windows 的
//!   `HKCU\…\Run` 注册表项），用户在系统设置里关掉它时界面能看到。
//! - Registration follows a Settings edit, an onboarding confirmation, or an MSI choice.
//!   Merely starting Oink or reading preferences never registers a login item.
//! - [`launched_at_login`] / [`handle_autostart_arg`]：本次是否由登录项启动（是的话界面不弹主窗口）。
//! - 没有含 C 代码的依赖，Windows 部分可以在 Mac 上交叉检查。

use std::process::ExitCode;

use serde::{Deserialize, Serialize};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(windows, test))]
mod run_entry;
#[cfg(windows)]
mod windows;

/// 登录项在系统里的状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchAtLoginState {
    /// 已注册，登录时会启动。
    Enabled,
    /// 没有注册（默认）。
    Disabled,
    /// 已注册，但用户在系统里把它关掉了：登录时不会启动，要用户到系统设置里重新允许。
    RequiresApproval,
    /// 这个进程不能注册登录项（不是从打包的应用运行、平台不支持）。
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchAtLoginProblem {
    MainAppRequired,
    InstalledAppRequired,
    ApprovalRequired,
    DifferentInstallation,
    InvalidRunEntry,
    SystemUnavailable,
    UnsupportedPlatform,
}

impl LaunchAtLoginProblem {
    pub fn code(self) -> &'static str {
        match self {
            Self::MainAppRequired => "main_app_required",
            Self::InstalledAppRequired => "installed_app_required",
            Self::ApprovalRequired => "approval_required",
            Self::DifferentInstallation => "different_installation",
            Self::InvalidRunEntry => "invalid_run_entry",
            Self::SystemUnavailable => "system_unavailable",
            Self::UnsupportedPlatform => "unsupported_platform",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchAtLoginStatus {
    pub state: LaunchAtLoginState,
    pub problem: Option<LaunchAtLoginProblem>,
    /// The backend fills this from its local confirmation marker, never OS state.
    pub configured: bool,
}

impl LaunchAtLoginStatus {
    pub fn new(state: LaunchAtLoginState, problem: Option<LaunchAtLoginProblem>) -> Self {
        Self {
            state,
            problem,
            configured: false,
        }
    }
    pub fn unavailable(problem: LaunchAtLoginProblem) -> Self {
        Self::new(LaunchAtLoginState::Unavailable, Some(problem))
    }
}

#[cfg(any(target_os = "macos", test))]
fn registration_result(
    enabled: bool,
    result: Result<(), String>,
    after: LaunchAtLoginStatus,
) -> Result<LaunchAtLoginStatus, String> {
    let reached = if enabled {
        matches!(
            after.state,
            LaunchAtLoginState::Enabled | LaunchAtLoginState::RequiresApproval
        )
    } else {
        after.state == LaunchAtLoginState::Disabled
    };
    match result {
        Ok(()) => Ok(after),
        Err(_) if reached => Ok(after),
        Err(error) => Err(error),
    }
}

/// 向系统查询登录项的当前状态。只读，可以在打开设置页、窗口重新获得焦点时调用。
pub fn status() -> LaunchAtLoginStatus {
    #[cfg(target_os = "macos")]
    return macos::status();
    #[cfg(windows)]
    return windows::status();
    #[cfg(not(any(target_os = "macos", windows)))]
    LaunchAtLoginStatus::unavailable(LaunchAtLoginProblem::UnsupportedPlatform)
}

/// Register/unregister after an explicit edit or confirmation, returning OS state.
///
/// 打开之后系统仍可能要求用户批准：这时返回 `Ok`，状态是 [`LaunchAtLoginState::RequiresApproval`]。
/// Failure returns a fixed problem code; native error details stay in local logs.
pub fn set_enabled(enabled: bool) -> Result<LaunchAtLoginStatus, String> {
    apply_enabled(enabled, status(), |enabled| {
        #[cfg(target_os = "macos")]
        return macos::set_enabled(enabled);
        #[cfg(windows)]
        return windows::set_enabled(enabled);
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            let _ = enabled;
            Err("unsupported_platform".into())
        }
    })
}

/// Confirming the shown value does not rewrite an existing MSI registration or
/// ask macOS to register an already approved/pending item again.
fn apply_enabled(
    enabled: bool,
    current: LaunchAtLoginStatus,
    mutate: impl FnOnce(bool) -> Result<LaunchAtLoginStatus, String>,
) -> Result<LaunchAtLoginStatus, String> {
    let matches = match current.state {
        LaunchAtLoginState::Enabled | LaunchAtLoginState::RequiresApproval => enabled,
        LaunchAtLoginState::Disabled => !enabled && current.problem.is_none(),
        LaunchAtLoginState::Unavailable => false,
    };
    if matches { Ok(current) } else { mutate(enabled) }
}

/// 打开系统里管理登录项的设置页（macOS「系统设置 → 通用 → 登录项」、Windows「设置 → 应用 → 启动」）。
pub fn open_system_settings() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return macos::open_system_settings();
    #[cfg(windows)]
    return windows::open_system_settings();
    #[cfg(not(any(target_os = "macos", windows)))]
    Err("unsupported_platform".into())
}

/// Windows 的登录项命令行里带的参数（见 [`handle_autostart_arg`]）。
pub const AUTOSTART_ARG: &str = "--autostart";

/// Windows entry-to-resident handoff only. macOS uses its current launch Apple
/// event; an inherited environment value cannot describe a later manual launch.
pub const LOGIN_LAUNCH_ENV: &str = "OINK_LAUNCHED_AT_LOGIN";

/// 本次是否由系统的登录项启动。**macOS 上必须在应用完成启动的回调里调用**（Tauri 的 `setup`），
/// 原因见 `macos::launched_as_login_item`。
pub fn launched_at_login() -> bool {
    #[cfg(target_os = "macos")]
    return macos::launched_as_login_item();
    #[cfg(not(target_os = "macos"))]
    std::env::var_os(LOGIN_LAUNCH_ENV).is_some_and(|v| v == "1")
}

/// Windows Run entries use a short-lived argument-bearing entry. The resident
/// tray is argument-free so installation role checks identify it unambiguously.
/// The login marker suppresses its normal window. A failed launch returns failure
/// before creating a tray or WebView; it never registers a login item implicitly.
pub fn handle_autostart_arg() -> Option<ExitCode> {
    if std::env::args().nth(1).as_deref() != Some(AUTOSTART_ARG) {
        return None;
    }
    let spawned = std::env::current_exe().and_then(|exe| login_command(&exe).spawn());
    match spawned {
        Ok(_) => Some(ExitCode::SUCCESS),
        Err(error) => {
            eprintln!("Oink login UI launch failed: {error}");
            Some(ExitCode::FAILURE)
        }
    }
}

fn login_command(exe: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new(exe);
    command.env(LOGIN_LAUNCH_ENV, "1");
    // Explorer starts login items in the system directory. Both resident roles
    // use the installed program directory instead, with no persistent arguments.
    if let Some(dir) = exe.parent() {
        command.current_dir(dir);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_respawn_has_no_arguments_and_carries_the_hidden_start_marker() {
        let exe = std::path::Path::new("installed/Oink/oink.exe");
        let command = login_command(exe);
        assert_eq!(command.get_program(), exe);
        assert_eq!(command.get_args().count(), 0);
        assert_eq!(command.get_current_dir(), exe.parent());
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == LOGIN_LAUNCH_ENV && value == Some(std::ffi::OsStr::new("1")))
        );
    }

    #[test]
    fn confirmation_applies_and_removes_but_keeps_existing_msi_choice() {
        // Both native implementations share this decision layer; no registration
        // or registry access occurs in these tests.
        for (before, requested, expected_mutation) in [
            (LaunchAtLoginState::Disabled, true, true),
            (LaunchAtLoginState::Disabled, false, false),
            (LaunchAtLoginState::Enabled, true, false),
            (LaunchAtLoginState::Enabled, false, true),
            (LaunchAtLoginState::RequiresApproval, true, false),
            (LaunchAtLoginState::RequiresApproval, false, true),
        ] {
            let mut mutation = None;
            let status = apply_enabled(requested, LaunchAtLoginStatus::new(before, None), |enabled| {
                mutation = Some(enabled);
                Ok(LaunchAtLoginStatus::new(
                    if enabled {
                        LaunchAtLoginState::Enabled
                    } else {
                        LaunchAtLoginState::Disabled
                    },
                    None,
                ))
            })
            .unwrap();
            assert_eq!(mutation, expected_mutation.then_some(requested));
            assert_eq!(status.state == LaunchAtLoginState::Disabled, !requested);
        }
    }

    #[test]
    fn registration_failure_is_reported_and_invalid_entries_are_removed() {
        let disabled = LaunchAtLoginStatus::new(LaunchAtLoginState::Disabled, None);
        assert_eq!(
            apply_enabled(true, disabled, |_| Err("denied".into())),
            Err("denied".into())
        );
        let invalid = LaunchAtLoginStatus::new(
            LaunchAtLoginState::Disabled,
            Some(LaunchAtLoginProblem::InvalidRunEntry),
        );
        let mut removed = false;
        apply_enabled(false, invalid, |enabled| {
            assert!(!enabled);
            removed = true;
            Ok(LaunchAtLoginStatus::new(LaunchAtLoginState::Disabled, None))
        })
        .unwrap();
        assert!(removed);
    }

    #[test]
    fn status_json_shape() {
        let s = LaunchAtLoginStatus::new(
            LaunchAtLoginState::RequiresApproval,
            Some(LaunchAtLoginProblem::ApprovalRequired),
        );
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            serde_json::json!({"state": "requires_approval", "problem": "approval_required", "configured": false})
        );
        for (state, name) in [
            (LaunchAtLoginState::Enabled, "enabled"),
            (LaunchAtLoginState::Disabled, "disabled"),
            (LaunchAtLoginState::Unavailable, "unavailable"),
        ] {
            assert_eq!(serde_json::to_value(state).unwrap(), serde_json::json!(name));
        }
    }

    #[test]
    fn registration_result_uses_system_readback() {
        let enabled = LaunchAtLoginStatus::new(LaunchAtLoginState::Enabled, None);
        let approval = LaunchAtLoginStatus::new(LaunchAtLoginState::RequiresApproval, None);
        let disabled = LaunchAtLoginStatus::new(LaunchAtLoginState::Disabled, None);
        assert!(registration_result(true, Err("duplicate".into()), enabled).is_ok());
        assert!(registration_result(true, Err("approval".into()), approval).is_ok());
        assert!(registration_result(false, Err("absent".into()), disabled.clone()).is_ok());
        assert_eq!(
            registration_result(true, Err("denied".into()), disabled),
            Err("denied".into())
        );
    }
}
