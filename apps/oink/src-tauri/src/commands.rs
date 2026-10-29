//! Native operations used by every product view.
use tauri::{AppHandle, Manager};

#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellError {
    InvalidAttributionPath,
    AttributionUnavailable,
    InvalidUrl,
    OpenUrlFailed,
    OpenFolderFailed,
    CacheUnavailable,
    QuitFailed,
    LogLevelUnavailable,
    #[cfg(target_os = "macos")]
    StatusIconOwnedByBackend,
    #[cfg(windows)]
    StatusIconUnavailable,
}

pub fn attribution_path(path: &str) -> Option<&std::path::Path> {
    let allowed = matches!(
        path,
        "third-party-licenses.json" | "THIRD_PARTY_LICENSES.md" | "NOTICE" | "LICENSE" | "LICENSE-ADDITIONAL-TERMS.md"
    ) || (path.starts_with("licenses/texts/") && path.ends_with(".txt"));
    let path = std::path::Path::new(path);
    (allowed && path.components().all(|p| matches!(p, std::path::Component::Normal(_)))).then_some(path)
}

#[tauri::command]
pub async fn read_attribution(app: AppHandle, path: String) -> Result<String, ShellError> {
    let path = attribution_path(&path).ok_or(ShellError::InvalidAttributionPath)?;
    #[cfg(target_os = "macos")]
    let root = {
        let exe = std::env::current_exe().map_err(|_| ShellError::AttributionUnavailable)?;
        // The UI is Contents/Helpers/Oink.app/Contents/MacOS/oink.
        // Attribution is shared once in the outer application's Resources.
        exe.ancestors()
            .nth(5)
            .ok_or(ShellError::AttributionUnavailable)?
            .join("Resources")
    };
    #[cfg(windows)]
    let root = app
        .path()
        .resource_dir()
        .map_err(|_| ShellError::AttributionUnavailable)?;
    let _ = app;
    let file = root.join(path);
    // Packaged assets are read-only. Canonical containment also rejects symlink
    // escapes without giving the frontend a general filesystem command.
    let root = root.canonicalize().map_err(|_| ShellError::AttributionUnavailable)?;
    let file = file.canonicalize().map_err(|_| ShellError::AttributionUnavailable)?;
    if !file.starts_with(root) {
        return Err(ShellError::InvalidAttributionPath);
    }
    std::fs::read_to_string(file).map_err(|_| ShellError::AttributionUnavailable)
}

fn open_external(target: &std::ffi::OsStr) -> Result<(), ()> {
    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("/usr/bin/open")
            .arg("--")
            .arg(target)
            .status()
            .map_err(|_| ())?;
        if !status.success() {
            return Err(());
        }
    }
    #[cfg(windows)]
    {
        use windows::{
            Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
            core::{HSTRING, PCWSTR, w},
        };
        // SAFETY: ShellExecute handles a URL/path directly, without a command interpreter.
        let result = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                &HSTRING::from(target),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        if result.0 as usize <= 32 {
            return Err(());
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn shell_open_url(url: String) -> Result<(), ShellError> {
    let parsed = tauri::Url::parse(&url).map_err(|_| ShellError::InvalidUrl)?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(ShellError::InvalidUrl);
    }
    tauri::async_runtime::spawn_blocking(move || open_external(std::ffi::OsStr::new(parsed.as_str())))
        .await
        .map_err(|_| ShellError::OpenUrlFailed)?
        .map_err(|_| ShellError::OpenUrlFailed)
}

pub async fn open_folder(path: std::path::PathBuf) -> Result<(), ShellError> {
    tauri::async_runtime::spawn_blocking(move || open_external(path.as_os_str()))
        .await
        .map_err(|_| ShellError::OpenFolderFailed)?
        .map_err(|_| ShellError::OpenFolderFailed)
}

#[tauri::command]
pub async fn open_cache_folder() -> Result<(), ShellError> {
    let path = oink_foundation::paths::cache_dir();
    std::fs::create_dir_all(&path).map_err(|_| ShellError::CacheUnavailable)?;
    open_folder(path).await
}

/// Apply the committed preference to this process's ordinary logs. D1's hook
/// accepts only the six product levels and leaves diagnostics independent.
#[tauri::command]
pub fn shell_set_log_level(level: String) -> Result<(), ShellError> {
    oink_agent::logs::set_level(&level).map_err(|_| ShellError::LogLevelUnavailable)
}

/// Windows owns its tray in this UI process. Mac visibility is applied by the
/// backend immediately after its authoritative native preference is committed.
#[tauri::command]
pub fn shell_set_status_icon(app: AppHandle, visible: bool) -> Result<(), ShellError> {
    #[cfg(windows)]
    {
        let tray = app.tray_by_id("main").ok_or(ShellError::StatusIconUnavailable)?;
        tray.set_visible(visible).map_err(|_| ShellError::StatusIconUnavailable)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (app, visible);
        Err(ShellError::StatusIconOwnedByBackend)
    }
}

#[tauri::command]
pub async fn shell_quit(app: AppHandle) -> Result<(), ShellError> {
    // An uninstall preparation error can leave its error window alive after
    // stopping the backend. Explicit Quit must not call that dead endpoint.
    if oink_agent::instance::is_running(&oink_foundation::paths::data_dir()) == Some(false) {
        app.exit(0);
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    if app
        .state::<crate::app::Backend>()
        .uninstalling
        .load(std::sync::atomic::Ordering::Acquire)
    {
        let data = oink_foundation::paths::data_dir();
        if !tauri::async_runtime::spawn_blocking(move || {
            oink_agent::host::stop_backend(&data, std::time::Duration::from_secs(15))
        })
        .await
        .map_err(|_| ShellError::QuitFailed)?
        .map_err(|_| ShellError::QuitFailed)?
        {
            return Err(ShellError::QuitFailed);
        }
        app.exit(0);
        return Ok(());
    }
    let remote = app.state::<crate::app::Backend>().remote.clone();
    if let Some(remote) = remote {
        tauri::async_runtime::spawn_blocking(move || remote.request_stop())
            .await
            .map_err(|_| ShellError::QuitFailed)?
            .map_err(|_| ShellError::QuitFailed)?;
    }
    app.exit(0);
    Ok(())
}

#[cfg(target_os = "macos")]
#[tauri::command]
pub async fn shell_uninstall(
    app: AppHandle,
    window: tauri::WebviewWindow,
    messages: crate::uninstall::Messages,
) -> Result<(), crate::uninstall::Failure> {
    if window.label() != "main" {
        return Err(crate::uninstall::Step::InstalledApp.into());
    }
    let state = app.state::<crate::app::Backend>();
    let remote = state
        .remote
        .clone()
        .ok_or(crate::uninstall::Failure::from(crate::uninstall::Step::StopBackend))?;
    let active = state.uninstalling.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        crate::uninstall::preparing(&active, || crate::uninstall::start(&remote, messages))
    })
    .await
    .map_err(|_| crate::uninstall::Failure::from(crate::uninstall::Step::Finalizer))?;
    if let Err(step) = result {
        tracing::warn!(?step, "uninstall preparation failed; normal quit handling restored");
        return Err(step);
    }
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn attribution_reads_only_named_packaged_assets() {
        for path in [
            "third-party-licenses.json",
            "THIRD_PARTY_LICENSES.md",
            "NOTICE",
            "LICENSE",
            "LICENSE-ADDITIONAL-TERMS.md",
            "licenses/texts/mit.txt",
        ] {
            assert!(attribution_path(path).is_some());
        }
        for path in [
            "../device.key",
            "/tmp/NOTICE",
            "licenses/texts/../../device.key.txt",
            "licenses/texts/../secret.txt",
            "settings.json",
            "docs/other.md",
            "https://example.com",
        ] {
            assert!(attribution_path(path).is_none(), "{path}");
        }
    }
}

/// Alignment feedback belongs to the UI process; Windows has no general
/// precision-touchpad haptic API, so the same command is a no-op there.
#[tauri::command]
pub fn shell_alignment_haptic(app: AppHandle) {
    #[cfg(target_os = "macos")]
    {
        // AppKit feedback must run on its owning main thread, including when
        // the invocation originates from a Tauri worker thread.
        if let Err(error) = app.run_on_main_thread(|| {
            use objc2_app_kit::{
                NSHapticFeedbackManager, NSHapticFeedbackPattern, NSHapticFeedbackPerformanceTime,
                NSHapticFeedbackPerformer,
            };
            NSHapticFeedbackManager::defaultPerformer().performFeedbackPattern_performanceTime(
                NSHapticFeedbackPattern::Alignment,
                NSHapticFeedbackPerformanceTime::Now,
            );
        }) {
            tracing::debug!(%error, "alignment haptic unavailable");
        }
    }
    #[cfg(windows)]
    let _ = app;
}
