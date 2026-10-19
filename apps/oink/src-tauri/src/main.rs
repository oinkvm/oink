// 发布构建在 Windows 上不弹控制台窗口。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    oink_agent::telemetry::fatal_boundary(run)
}

fn run() -> std::process::ExitCode {
    if let Err(error) = oink_foundation::process::initialize_priority() {
        eprintln!("Oink process priority initialization failed: {error}");
        return std::process::ExitCode::FAILURE;
    }
    #[cfg(windows)]
    if let Some(code) = oink_agent::dispatch_raw_clip_guard() {
        return code;
    }
    // Backend/maintenance roles finish before any interactive shell is created.
    if let Some(code) = oink_ui::service::dispatch_args() {
        return code;
    }
    #[cfg(windows)]
    if let Err(error) = oink_ui::service::require_normal_user_ui() {
        eprintln!("{error}");
        return std::process::ExitCode::FAILURE;
    }
    #[cfg(windows)]
    if let Some(code) = oink_ui::installer_finish::dispatch_args() {
        return code;
    }
    if let Some(code) = oink_autostart::handle_autostart_arg() {
        return code;
    }
    #[cfg(any(target_os = "macos", windows))]
    {
        oink_ui::run();
        std::process::ExitCode::SUCCESS
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        eprintln!("Oink 界面只支持 macOS 与 Windows");
        std::process::ExitCode::FAILURE
    }
}
