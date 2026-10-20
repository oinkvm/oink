//! Tauri IPC client: main window, and the Windows tray.
use std::sync::{Arc, Mutex};

use oink_agent::api::{self, AgentApi, ApiError, ApiEvent, Reply, Request};
#[cfg(windows)]
use tauri::tray::TrayIconBuilder;
#[cfg(windows)]
use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State};

use oink_agent::remote::RemoteApi;

use crate::boot::BootInfo;

pub(crate) const ERROR_REPORTER: &str = r#"window.addEventListener('keydown',e=>{if(e.key==='Escape'&&new URLSearchParams(location.search).get('view')==='panel'){e.preventDefault();window.__TAURI_INTERNALS__?.invoke('shell_close')}});window.addEventListener('error',e=>window.__TAURI_INTERNALS__?.invoke('frontend_error',{message:String(e.error||e.message)}));window.addEventListener('unhandledrejection',e=>window.__TAURI_INTERNALS__?.invoke('frontend_error',{message:String(e.reason)}));"#;

#[tauri::command]
fn frontend_error(message: String) {
    tracing::warn!(%message, "界面脚本错误");
}

/// 推给前端的事件名；载荷为 [`ApiEvent`]（`{"type": ..., "data": ...}`）。
const EVENT: &str = "agent-event";

pub(crate) struct Backend {
    #[cfg(target_os = "macos")]
    pub(crate) uninstalling: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) api: Option<Arc<dyn AgentApi>>,
    boot: BootInfo,
    startup_error: Option<String>,
    pub(crate) remote: Option<RemoteApi>,
    capture: Mutex<Option<oink_agent::telemetry::UiCapture>>,
    pub(crate) control: Mutex<Option<oink_agent::ipc::UiControl>>,
    _ui_lock: Option<oink_agent::instance::InstanceLock>,
}

/// A duplicate never creates a second UI. Manual launches ask the existing
/// UI to show its main window; login/background launches simply exit.
fn hand_off_to_running_ui(api: &RemoteApi, launched_at_login: bool) -> bool {
    if launched_at_login {
        return true;
    }
    #[cfg(not(windows))]
    if let Err(error) = crate::service::check_backend_ownership(api.data_dir()) {
        tracing::warn!(%error, "cannot hand off without backend process ownership");
        return false;
    }
    #[cfg(windows)]
    return match crate::foreground_windows::handoff(api.data_dir()) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(%error, "foreground handoff failed");
            false
        }
    };
    #[cfg(not(windows))]
    match api.show_window() {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("another Oink UI is running but could not be asked to show its window: {e}");
            false
        }
    }
}

/// Explicit product quit stops sharing; closing the UI never does.
#[cfg(windows)]
fn quit_app(app: &AppHandle) {
    let remote = app.state::<Backend>().remote.clone();
    if let Some(remote) = remote {
        tauri::async_runtime::spawn_blocking(move || {
            if let Err(error) = remote.request_stop() {
                tracing::error!(%error, "could not stop backend");
            }
        });
    } else {
        app.exit(0);
    }
}

#[tauri::command]
fn boot_info(state: State<'_, Backend>) -> BootInfo {
    state.boot.clone()
}

/// 执行一次 API 调用。放到阻塞线程池里：部分命令（如 `connect_by_ip`）会等待网络。
#[tauri::command]
async fn api_call(
    window: tauri::WebviewWindow,
    state: State<'_, Backend>,
    request: Request,
) -> Result<Reply, ApiError> {
    if crate::shell_policy::View::from_label(window.label()).is_none() {
        return Err(ApiError::InvalidState {
            message: "unknown product window".into(),
        });
    }
    let api = state.api.clone().ok_or_else(|| ApiError::Unavailable {
        message: state
            .startup_error
            .clone()
            .unwrap_or_else(|| "服务尚未启动，请退出 Oink 后重新打开。".into()),
    })?;
    tauri::async_runtime::spawn_blocking(move || api::dispatch(api.as_ref(), request))
        .await
        .map_err(|e| ApiError::Internal { message: e.to_string() })?
}

/// The backend creates/returns its local store folder; only this UI shell
/// launches Finder/Explorer in the interactive user's session.
#[tauri::command]
async fn open_diagnostics_folder(window: tauri::WebviewWindow, state: State<'_, Backend>) -> Result<(), ApiError> {
    let Reply::DiagnosticsFolder(path) = api_call(window, state, Request::DiagnosticsFolder).await? else {
        unreachable!("diagnostics folder reply");
    };
    crate::commands::open_folder(path.into())
        .await
        .map_err(|_| ApiError::Internal {
            message: "open_folder_failed".into(),
        })
}

pub fn run() {
    oink_agent::logs::init_ui();
    tracing::info!(pid = std::process::id(), "UI process starting");
    let mut startup_error = None;
    let ui_lock;
    let mut remote_api = None;
    let api: Option<Arc<dyn AgentApi>> = {
        let data_dir = oink_foundation::paths::data_dir();
        let remote = RemoteApi::new(data_dir.clone());
        if std::env::args().any(|arg| arg == oink_agent::backend::REOPEN_FLAG)
            && let Err(error) = crate::service::wait_previous_pair(&data_dir)
        {
            tracing::error!(%error);
            return;
        }
        match oink_agent::instance::acquire_ui(&data_dir) {
            Ok(lock) => {
                ui_lock = Some(lock);
                #[cfg(windows)]
                if let Err(error) = crate::foreground_windows::publish() {
                    tracing::error!(%error, "could not publish foreground owner");
                    return;
                }
            }
            Err(error) => {
                if error.downcast_ref::<oink_agent::instance::AlreadyRunning>().is_some() {
                    let _ = hand_off_to_running_ui(
                        &remote,
                        oink_autostart::launched_at_login() || std::env::args().any(|arg| arg == "--background"),
                    );
                } else {
                    tracing::error!(%error, "UI single-instance lock failed");
                }
                return;
            }
        }
        #[cfg(windows)]
        if let Err(error) = std::env::current_exe()
            .map_err(|e| e.to_string())
            .and_then(|exe| crate::service::start_once(&remote, &exe))
        {
            tracing::error!(%error);
            startup_error = Some(error);
        }
        if remote.build_mismatch() {
            return;
        }
        #[cfg(target_os = "macos")]
        if let Err(error) = crate::service::check_backend_ownership(&data_dir) {
            tracing::error!(%error, "backend process ownership unavailable");
            return;
        }
        if startup_error.is_none()
            && let Err(error) = remote.hello()
        {
            tracing::error!(%error, "backend unavailable");
            if remote.build_mismatch() {
                return;
            }
            if cfg!(windows) {
                startup_error = Some(error.to_string());
            } else {
                return;
            }
        }
        if startup_error.is_some() {
            None
        } else {
            remote_api = Some(remote.clone());
            Some(Arc::new(remote))
        }
    };
    let boot = BootInfo::new(remote_api.is_some());
    let app = tauri::Builder::default()
        // The UI uses window/tray events and backend IPC, not raw device events.
        // `Always` means always filter them out: tao removes the Windows mouse/
        // keyboard Raw Input registrations, avoiding WM_INPUT work at mouse rate.
        .device_event_filter(tauri::DeviceEventFilter::Always)
        .manage(crate::shell::Shell::default())
        .manage(crate::app_icon::AppIcon::default())
        .manage(crate::live_views::Demand::default())
        .manage(Backend {
            #[cfg(target_os = "macos")]
            uninstalling: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            api,
            boot,
            startup_error,
            remote: remote_api,
            capture: Mutex::new(Some(oink_agent::telemetry::UiCapture::new(
                oink_foundation::paths::data_dir(),
            ))),
            control: Mutex::new(None),
            _ui_lock: ui_lock,
        })
        .invoke_handler(tauri::generate_handler![
            boot_info,
            open_diagnostics_folder,
            frontend_error,
            api_call,
            crate::sponsor::shell_sponsor_qr,
            crate::live_views::live_views,
            crate::shell::shell_open,
            crate::shell::shell_ready,
            crate::shell::shell_close,
            crate::shell::shell_titlebar_center,
            crate::shell::shell_resize_panel,
            crate::shell::shell_show_panel,
            crate::commands::read_attribution,
            crate::commands::shell_alignment_haptic,
            crate::commands::shell_open_url,
            crate::commands::shell_quit,
            #[cfg(target_os = "macos")]
            crate::commands::shell_uninstall,
            crate::commands::open_cache_folder,
            crate::commands::shell_set_status_icon,
            crate::commands::shell_set_log_level,
            crate::native_menu::shell_set_language
        ])
        .setup(|app| {
            app.manage(crate::sponsor::SponsorQr::start());
            #[cfg(target_os = "macos")]
            {
                crate::native_menu::install(app.handle(), oink_agent::config::UiLanguage::Auto)?;
                app.on_menu_event(|app, event| {
                    let view = match event.id().as_ref() {
                        "shell-about" => crate::shell_policy::View::About,
                        "shell-main" => crate::shell_policy::View::Main,
                        "shell-quit" => crate::shell_policy::View::Quit,
                        _ => return,
                    };
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        // shell_open logs failures for both menu and IPC requests.
                        let _ = crate::shell::shell_open(app, view).await;
                    });
                });
            }
            let handle = app.handle().clone();
            let quitting = handle.clone();
            let opening = handle.clone();
            let control = tauri::async_runtime::block_on(oink_agent::ipc::UiControl::start_with_open(
                &oink_foundation::paths::data_dir(),
                move || quitting.exit(0),
                move || show_main(&opening),
            ))?;
            *handle.state::<Backend>().control.lock().unwrap() = Some(control);
            crate::app_icon::initialize(&handle);
            // A login launch on Windows has no WebView at all. macOS only starts
            // this executable when its backend has a reason to show a window.
            let show_at_start = crate::shell_policy::shows_on_start(
                oink_autostart::launched_at_login(),
                std::env::args().any(|arg| arg == "--background"),
            );
            #[cfg(windows)]
            {
                crate::foreground_windows::install(&handle);
                setup_tray(&handle)?;
            }
            forward_events(&handle);
            if let Some(remote) = handle.state::<Backend>().remote.clone() {
                let connected = handle.clone();
                std::thread::spawn(move || {
                    crate::service::follow(
                        &remote,
                        |_| {
                            #[cfg(windows)]
                            if let Ok(settings) = remote.input_settings() {
                                crate::foreground_windows::configure(&connected, &settings.hotkeys);
                            }
                            // Seed the native icon before the first window becomes visible.
                            if let Ok(preferences) = remote.ui_preferences() {
                                crate::app_icon::preferences(&connected, &preferences);
                            }
                            match remote.report_status() {
                                Ok(status) => apply_report_status(&connected, &status),
                                Err(error) => tracing::warn!(%error, "UI report status unavailable"),
                            }
                            // Subscribe before the WebView starts its initial API reads,
                            // so changes during that load are already queued as events.
                            if show_at_start {
                                show_main(&connected);
                            }
                            if let Err(error) = oink_agent::ipc::Client::connect(remote.data_dir()).and_then(|c| c.ui_ready()) {
                                tracing::error!(%error, "UI readiness handshake failed");
                                connected.exit(1);
                            }
                        },
                        |end| {
                            #[cfg(target_os = "macos")]
                            if end != oink_agent::remote::EventsEnd::BuildMismatch
                                && handle.state::<Backend>().uninstalling.load(std::sync::atomic::Ordering::Acquire) {
                                tracing::info!(?end, "backend subscription ended during active uninstall preparation; deferring UI exit");
                                return;
                            }
                            tracing::info!(?end, "backend subscription ended; exiting UI");
                            handle.exit(0);
                        },
                    )
                });
            } else if show_at_start {
                crate::shell::open_normal(&handle, crate::shell_policy::View::Main);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("启动 Oink 界面失败");
    #[cfg(target_os = "macos")]
    let app = {
        let mut app = app;
        // Tao defaults to Regular even for an LSUIElement bundle. Configure its
        // initial policy before run(), which applies it at didFinishLaunching.
        // Doing this in setup() is too late: Regular -> Accessory -> Regular
        // races Dock's asynchronous registration and can leave a zero-width
        // tile although the UI reports Regular (observed on macOS 26). The first
        // normal-window request now makes the only Accessory -> Regular change;
        // panel/login launches never create an initial Regular Dock tile.
        app.set_activation_policy(tauri::ActivationPolicy::Accessory);
        app
    };
    #[cfg(target_os = "macos")]
    let _visibility_observers = crate::live_views::watch_application(app.handle());
    app.run(|handle, event| {
        if let tauri::RunEvent::WindowEvent {
            event: tauri::WindowEvent::ThemeChanged(theme),
            ..
        } = &event
        {
            crate::app_icon::theme_changed(handle, *theme);
        }
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::WindowEvent {
            label,
            event: tauri::WindowEvent::Focused(true),
            ..
        } = &event
            && crate::shell_policy::View::from_label(label).is_some_and(crate::shell_policy::View::normal)
        {
            // The first Accessory→Regular activation registers the Dock tile
            // asynchronously. Reapply once focus confirms that activation.
            on_main(handle, |app| crate::app_icon::dock(app, true));
        }
        #[cfg(windows)]
        if let tauri::RunEvent::WindowEvent {
            event: tauri::WindowEvent::ScaleFactorChanged { .. },
            ..
        } = &event
        {
            crate::app::on_main(handle, crate::app_icon::refresh);
        }
        if let tauri::RunEvent::WindowEvent { label, event, .. } = &event {
            match event {
                tauri::WindowEvent::Destroyed => {
                    crate::live_views::closed(handle, label);
                    #[cfg(target_os = "macos")]
                    if label == "main"
                        && oink_agent::instance::is_running(&oink_foundation::paths::data_dir()) == Some(false)
                    {
                        // A failed uninstall keeps its dialog alive after backend
                        // stop. Closing it must release the UI lock for a relaunch.
                        handle.exit(0);
                    }
                    #[cfg(windows)]
                    crate::app_icon::closed(handle, label);
                }
                _ => crate::live_views::refresh(handle),
            }
        }
        if let tauri::RunEvent::WindowEvent { label, event, .. } = &event {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                let label = label.clone();
                crate::app::on_main(handle, move |app| crate::shell::window_destroyed(app, &label));
            }
            #[cfg(windows)]
            if label == "panel" && matches!(event, tauri::WindowEvent::Focused(false)) {
                crate::shell::panel_lost_focus(handle);
            }
            let _ = label;
        }
        if let tauri::RunEvent::ExitRequested { code: None, api, .. } = &event {
            // Last-window close destroys its WebView, leaving an empty event loop
            // available for the next status-item click. Explicit exit still exits.
            api.prevent_exit();
        }
        if matches!(event, tauri::RunEvent::Exit) {
            crate::shell::shutdown(handle);
        }
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Reopen { .. } = event {
            show_main(handle);
        }
        let _ = (handle, event);
    });
}

/// Acquire and release UI handles inside the event loop, including their destructors.
pub(crate) fn on_main(app: &AppHandle, f: impl FnOnce(&AppHandle) + Send + 'static) {
    let handle = app.clone();
    if let Err(error) = app.run_on_main_thread(move || f(&handle)) {
        tracing::warn!(%error, "could not schedule UI update");
    }
}

pub(crate) fn show_main(app: &AppHandle) {
    on_main(app, show_main_on_main);
}

fn show_background(app: &AppHandle) {
    #[cfg(windows)]
    crate::shell::open_with_activation(
        app,
        crate::shell_policy::View::Main,
        crate::shell_policy::OpenTrigger::Backend.activation(),
    );
    #[cfg(not(windows))]
    show_main(app);
}

fn show_main_on_main(app: &AppHandle) {
    crate::shell::close_panel(app);
    crate::shell::open_normal(app, crate::shell_policy::View::Main);
}

#[cfg(windows)]
fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let icon = crate::app_icon::tray_image(app).map_err(|error| tauri::Error::Io(std::io::Error::other(error)))?;
    TrayIconBuilder::with_id("main")
        .icon(icon)
        .tooltip("Oink")
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            #[cfg(windows)]
            match event {
                TrayIconEvent::Click {
                    button: MouseButton::Left | MouseButton::Right,
                    button_state: MouseButtonState::Up,
                    rect,
                    ..
                } => {
                    if let Some(anchor) = crate::shell::tray_anchor(tray.app_handle(), rect) {
                        crate::shell::toggle_panel(tray.app_handle(), anchor);
                    }
                }
                TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } => show_main(tray.app_handle()),
                _ => {}
            }
            #[cfg(not(windows))]
            let _ = (tray, event);
        })
        .build(app)?;
    crate::app_icon::tray_created(app);
    Ok(())
}

fn forward_events(app: &AppHandle) {
    let Some(api) = app.state::<Backend>().api.clone() else {
        return;
    };
    let handle = app.clone();
    #[cfg(windows)]
    let data_dir = oink_foundation::paths::data_dir();
    api.subscribe(Arc::new(move |ev: &ApiEvent| {
        #[cfg(windows)]
        if let ApiEvent::DeviceCards(view) = ev {
            crate::app_icon::device_cards(&handle, view);
        }
        #[cfg(windows)]
        if let ApiEvent::InputSettings(settings) = ev {
            crate::foreground_windows::configure(&handle, &settings.hotkeys);
        }
        if let ApiEvent::UiPreferences(preferences) = ev {
            crate::app_icon::preferences(&handle, preferences);
        }
        if let ApiEvent::ReportStatusChanged(status) = ev {
            apply_report_status(&handle, status);
        }
        if !matches!(ev, ApiEvent::Stats(_)) || crate::live_views::stats_enabled(&handle) {
            let _ = handle.emit(EVENT, ev);
        }
        // Update owner has already drained business work. Closing this client
        // must not request backend stop before the installer receives its lease.
        if matches!(ev, ApiEvent::ExitForUpdate) {
            handle.exit(0);
        }
        if let ApiEvent::TogglePanel(anchor) = ev {
            crate::shell::toggle_panel(&handle, *anchor);
        }
        // 紧急暂停：本机触发时把主窗口弹到前台（界面显示「共享已暂停」与「恢复共享」）；
        // 其他设备触发时只在托盘提示（Overview 随后更新托盘文字）。
        #[cfg(not(windows))]
        if let ApiEvent::EmergencyStop(e) = ev
            && e.local
        {
            show_main(&handle);
        }
        #[cfg(windows)]
        if matches!(ev, ApiEvent::ReopenOink) {
            let result =
                std::env::current_exe().and_then(|exe| oink_agent::backend::launch::reopen_entry(&exe, &data_dir));
            match result {
                Ok(_) => quit_app(&handle),
                Err(error) => tracing::error!(%error, "could not reopen Oink"),
            }
        }
        // Backend IPC is not evidence of a Windows foreground grant.
        if matches!(ev, ApiEvent::ShowWindow) {
            show_background(&handle);
        }
    }));
}

/// Native capture follows authenticated, applied consent even with no WebView.
/// Submit recovered numeric records outside its lock and off the event reader.
fn apply_report_status(app: &AppHandle, status: &oink_agent::telemetry::ReportStatus) {
    let state = app.state::<Backend>();
    let recovered = {
        let mut slot = state.capture.lock().unwrap_or_else(|e| e.into_inner());
        let Some(capture) = slot.as_mut() else { return };
        match capture.apply(status) {
            Ok(recovered) => recovered,
            Err(error) => {
                tracing::warn!(?error, "UI report capture unavailable");
                return;
            }
        }
    };
    if recovered.is_empty() {
        return;
    }
    if let Some(remote) = state.remote.clone() {
        let generation = status.consent.generation;
        tauri::async_runtime::spawn_blocking(move || {
            for (crash, collected_at) in recovered {
                if let Err(error) = remote.submit_crash_capsule(generation, crash, collected_at) {
                    tracing::warn!(%error, "UI crash record was not admitted");
                }
            }
        });
    }
}

pub(crate) fn shutdown_capture(app: &AppHandle) {
    // Taking the slot retires this lifetime. A late initial RPC or event cannot
    // re-arm capture after the UI's orderly exit has started.
    if let Some(mut capture) = app
        .state::<Backend>()
        .capture
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        capture.shutdown();
    }
}
