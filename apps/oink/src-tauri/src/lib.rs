//! The UI is an IPC client; the backend owns all sharing and native overlays.
#[cfg(any(target_os = "macos", windows))]
mod app_icon;
pub mod boot;
#[cfg(any(target_os = "macos", windows, test))]
mod creation_queue;
#[cfg(any(windows, test))]
mod cursor_dispatch_windows;
#[cfg(windows)]
mod foreground_windows;
#[cfg(any(target_os = "macos", windows, test))]
mod icon_render;
#[cfg(any(windows, test))]
pub mod installer_finish;
#[cfg(target_os = "macos")]
mod local_drop;
#[cfg(windows)]
mod local_drop_windows;
#[cfg(any(windows, test))]
mod menu_icons_windows;
pub mod service;
pub mod shell_policy;
#[cfg(any(target_os = "macos", windows, test))]
mod sponsor;
#[cfg(any(windows, test))]
mod tray_windows;
#[cfg(any(target_os = "macos", test))]
mod uninstall;
mod update_window;
#[cfg(any(target_os = "macos", windows, test))]
mod window_activity;
#[cfg(any(windows, test))]
mod windows_hotkey;

#[cfg(any(target_os = "macos", windows))]
mod commands;
#[cfg(target_os = "macos")]
mod dock_controls;
#[cfg(windows)]
mod jump_controls;
#[cfg(target_os = "macos")]
mod main_window_macos;
#[cfg(windows)]
mod main_window_windows;
#[cfg(any(target_os = "macos", windows))]
mod native_controls;
#[cfg(any(target_os = "macos", windows))]
mod native_menu;
#[cfg(target_os = "macos")]
mod panel_macos;
#[cfg(any(target_os = "macos", windows))]
mod shell;
#[cfg(any(target_os = "macos", windows))]
mod system_locale;

#[cfg(any(target_os = "macos", windows))]
mod app;
#[cfg(any(target_os = "macos", windows))]
mod live_views;
#[cfg(any(target_os = "macos", windows))]
pub use app::run;

#[cfg(test)]
mod tests {
    use crate::boot::BootInfo;
    #[test]
    fn boot_identifies_the_native_platform() {
        let value = serde_json::to_value(BootInfo::new(false)).unwrap();
        assert_eq!(value["backend"], "none");
        assert!(value["platform"].is_string());
        assert!(value["version"].is_string());
    }
}
