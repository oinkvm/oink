//! macOS: `SMAppService.mainApp` (macOS 13+; Oink supports macOS 14.2+).
//!
//! 登录项登记的是**应用本身**（`Oink.app`），不是另一个辅助程序：登录时系统像用户双击一样打开它，「辅助功能」
//! 「输入监控」等授权仍然算在 `Oink.app` 上。系统在「系统设置 → 通用 → 登录项」的「登录时打开」里列出它，
//! 用户可以在那里关掉；[`status`] 每次都读系统的状态。

use objc2_foundation::{NSAppleEventManager, NSBundle, NSError};
use objc2_service_management::{SMAppService, SMAppServiceStatus};

use crate::{LaunchAtLoginProblem, LaunchAtLoginState, LaunchAtLoginStatus};

/// 当前进程是不是从一个 `.app` 运行。`SMAppService.mainApp` 指的是「主程序包」，裸可执行文件
/// （`cargo run`、`tauri build --no-bundle`、命令行工具）没有可登记的东西。
fn bundle_problem_for(path: &str, identifier: Option<&str>) -> Option<LaunchAtLoginProblem> {
    if !path.ends_with(".app") || identifier != Some("com.hachiwii.oink") {
        return Some(LaunchAtLoginProblem::MainAppRequired);
    }
    if path.contains("/AppTranslocation/") {
        return Some(LaunchAtLoginProblem::InstalledAppRequired);
    }
    None
}

fn bundle_problem() -> Option<LaunchAtLoginProblem> {
    let bundle = NSBundle::mainBundle();
    let identifier = bundle.bundleIdentifier().map(|s| s.to_string());
    bundle_problem_for(&bundle.bundlePath().to_string(), identifier.as_deref())
}

pub(crate) fn status() -> LaunchAtLoginStatus {
    if let Some(problem) = bundle_problem() {
        return LaunchAtLoginStatus::unavailable(problem);
    }
    // SAFETY: 无参数的类方法，返回代表主程序包的服务对象。
    let service = unsafe { SMAppService::mainAppService() };
    // SAFETY: 只读属性，可以在任意线程读取。
    let status = unsafe { service.status() };
    match status {
        SMAppServiceStatus::Enabled => LaunchAtLoginStatus::new(LaunchAtLoginState::Enabled, None),
        SMAppServiceStatus::RequiresApproval => LaunchAtLoginStatus::new(
            LaunchAtLoginState::RequiresApproval,
            Some(LaunchAtLoginProblem::ApprovalRequired),
        ),
        // 文档里「没有登记」是 NotRegistered，NotFound 说的是找不到辅助程序的 plist；但主程序包从未登记过时，
        // 系统回答的是 NotFound（实测 macOS 14.7 与 26，已签名的 .app，原始值 3）。所以两者都算「未启用」，
        // 不能把 NotFound 当成「不可用」——那样开关永远点不了。
        SMAppServiceStatus::NotRegistered | SMAppServiceStatus::NotFound => {
            LaunchAtLoginStatus::new(LaunchAtLoginState::Disabled, None)
        }
        _ => LaunchAtLoginStatus::unavailable(LaunchAtLoginProblem::SystemUnavailable),
    }
}

pub(crate) fn set_enabled(enabled: bool) -> Result<LaunchAtLoginStatus, String> {
    if let Some(problem) = bundle_problem() {
        return Err(problem.code().into());
    }
    // SAFETY: 同 `status`。
    let service = unsafe { SMAppService::mainAppService() };
    // SAFETY: 同步调用系统的登录项服务；不要求主线程。错误以 NSError 返回。
    let result = unsafe {
        if enabled {
            service.registerAndReturnError()
        } else {
            service.unregisterAndReturnError()
        }
    };
    crate::registration_result(enabled, result.map_err(|e| describe(&e)), status())
}

fn describe(e: &NSError) -> String {
    tracing::warn!(domain = %e.domain(), code = e.code(), "login item update failed");
    "login_item_update_failed".into()
}

pub(crate) fn open_system_settings() -> Result<(), String> {
    // SAFETY: 无参数的类方法，打开系统设置的登录项页面。
    unsafe { SMAppService::openSystemSettingsLoginItems() };
    Ok(())
}

const fn four_cc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

/// The login Apple event exists only during applicationDidFinishLaunching.
/// SMAppService launches its main app without arguments. The public `prdt/lgit`
/// Apple-event marker is observed there on macOS 14.7/26; ordinary reopen events
/// must not suppress a user-requested window. No settings are changed here.
pub(crate) fn launched_as_login_item() -> bool {
    let Some(event) = NSAppleEventManager::sharedAppleEventManager().currentAppleEvent() else {
        return false;
    };
    // AERegistry.h: keyAEPropData / keyAELaunchedAsLogInItem on kAEOpenApplication.
    let marker = event
        .paramDescriptorForKeyword(four_cc(b"prdt"))
        .map(|d| d.enumCodeValue());
    let login = login_event(event.eventClass(), event.eventID(), marker);
    tracing::info!(
        class = event.eventClass(),
        id = event.eventID(),
        ?marker,
        login,
        "macOS launch Apple event"
    );
    login
}

fn login_event(class: u32, id: u32, marker: Option<u32>) -> bool {
    class == four_cc(b"aevt") && id == four_cc(b"oapp") && marker == Some(four_cc(b"lgit"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manual_process_ignores_inherited_login_environment() {
        if std::env::var_os("OINK_TEST_LOGIN_CHILD").is_some() {
            assert!(!crate::launched_at_login());
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "macos::tests::manual_process_ignores_inherited_login_environment",
            ])
            .env("OINK_TEST_LOGIN_CHILD", "1")
            .env(crate::LOGIN_LAUNCH_ENV, "1")
            .status()
            .unwrap();
        assert!(status.success());
    }
    #[test]
    fn only_the_main_installed_bundle_can_register() {
        assert!(bundle_problem_for("/Applications/Oink.app", Some("com.hachiwii.oink")).is_none());
        for (path, id) in [
            ("/test/bin", None),
            ("/Applications/Oink.app", Some("com.hachiwii.oink.ui")),
            ("/private/AppTranslocation/random/Oink.app", Some("com.hachiwii.oink")),
        ] {
            assert!(bundle_problem_for(path, id).is_some());
        }
    }

    #[test]
    fn login_event_detection_has_no_dependency_on_host_environment() {
        for (class, id, marker, expected) in [
            (four_cc(b"aevt"), four_cc(b"oapp"), Some(four_cc(b"lgit")), true),
            (four_cc(b"aevt"), four_cc(b"oapp"), None, false),
            (four_cc(b"aevt"), four_cc(b"rapp"), Some(four_cc(b"lgit")), false),
            (four_cc(b"aevt"), four_cc(b"oapp"), Some(four_cc(b"svit")), false),
            (four_cc(b"misc"), four_cc(b"oapp"), Some(four_cc(b"lgit")), false),
        ] {
            assert_eq!(login_event(class, id, marker), expected);
        }
    }
}
