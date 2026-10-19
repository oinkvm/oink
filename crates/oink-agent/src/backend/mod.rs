//! Native sharing host and its independent UI client.
//!
//! macOS: the LSUIElement backend owns input, overlays and the status item. One
//! on-demand Tauri process owns both normal windows and the nonactivating panel.
//! Windows: the SYSTEM service creates a user-session UIAccess backend; the
//! normal-user Tauri process owns the tray and all WebViews. Neither platform
//! automatically restarts a dead backend. UI loss never stops sharing.

pub mod cli;
pub mod foreground;
pub mod launch;
pub mod overlay;

use crate::api::{ApiError, ApiEvent, ApiResult};
use crate::node::Node;
#[cfg(target_os = "macos")]
use std::sync::Arc;
use std::sync::Mutex;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) mod smoke;
static NODE: Mutex<Option<Node>> = Mutex::new(None);

pub(crate) fn attach(node: &Node) {
    *NODE.lock().unwrap() = Some(node.clone());
    crate::update::attach(node);
    #[cfg(windows)]
    node.subscribe_views(
        std::sync::Arc::new(|event: &ApiEvent| {
            if foreground::user_event(event)
                && let Err(error) = foreground::open_from_hotkey()
            {
                tracing::warn!(%error, "could not foreground UI for local emergency input");
            }
        }),
        crate::api::LiveViews::default(),
    );
    #[cfg(target_os = "macos")]
    node.subscribe_views(
        Arc::new(|event: &ApiEvent| {
            if let ApiEvent::DeviceCards(view) = event {
                macos::update_status_icon(view);
            }
            if opens_window(event)
                && let Err(error) = deliver_ui(event.clone())
            {
                tracing::error!(%error, "could not open UI");
            }
        }),
        crate::api::LiveViews::default(),
    );
}

pub(crate) fn node() -> ApiResult<Node> {
    NODE.lock().unwrap().clone().ok_or_else(|| ApiError::Unavailable {
        message: "后台尚未启动".into(),
    })
}

/// `CFBundleExecutable` of the macOS bundle: the backend.
pub const MAC_BACKEND_EXECUTABLE: &str = "oink-backend";
/// Started by "reopen Oink": wait for the previous instance to release its single
/// instance lock, then start as if opened by the user.
pub const REOPEN_FLAG: &str = "--reopen";

/// Why the backend (macOS) or the tray-and-UI process (Windows) was started.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Launch {
    /// By the login item: no window.
    Login,
    /// By the user: show the main window.
    User,
    /// By "reopen Oink".
    Reopen,
}

#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
fn shows_on_start(launch: Launch, login_marker: bool) -> bool {
    match launch {
        Launch::Login => false,
        Launch::User => !login_marker,
        Launch::Reopen => true,
    }
}

#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
fn opens_window(event: &ApiEvent) -> bool {
    matches!(event, ApiEvent::ShowWindow) || matches!(event, ApiEvent::EmergencyStop(stop) if stop.local)
}

/// Claim ownership before creating any backend. A duplicate only asks the existing
/// process to show its UI, through the same IPC used by all other clients.
#[cfg(any(target_os = "macos", all(test, debug_assertions)))]
fn claim_instance(data_dir: &std::path::Path) -> anyhow::Result<Option<crate::instance::InstanceLock>> {
    use crate::api::AgentApi;
    match crate::instance::acquire(data_dir) {
        Ok(lock) => Ok(Some(lock)),
        Err(error) if error.downcast_ref::<crate::instance::AlreadyRunning>().is_some() => {
            crate::remote::RemoteApi::new(data_dir).show_window()?;
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Global desktop coordinates. macOS uses top-left logical points; Windows uses
/// physical pixels and supplies the containing monitor's scale factor.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PanelRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PanelAnchor {
    pub rect: PanelRect,
    pub work_area: PanelRect,
    pub scale_factor: f64,
}

#[cfg(any(target_os = "macos", test))]
#[derive(Default)]
struct UiDelivery {
    #[cfg(target_os = "macos")]
    starting: Option<std::time::Instant>,
    ready: bool,
    pending: Option<ApiEvent>,
}

#[cfg(any(target_os = "macos", test))]
impl UiDelivery {
    #[cfg(target_os = "macos")]
    fn should_launch(&self, event: &ApiEvent, running: bool) -> bool {
        !running
            && (opens_window(event)
                || !self
                    .starting
                    .is_some_and(|start| start.elapsed() < std::time::Duration::from_secs(10)))
    }

    fn request(&mut self, event: ApiEvent, running: bool) -> bool {
        if !self.ready || !running {
            self.pending = Some(event);
            false
        } else {
            true
        }
    }

    fn ready(&mut self) -> Option<ApiEvent> {
        self.ready = true;
        #[cfg(target_os = "macos")]
        {
            self.starting = None;
        }
        self.pending.take()
    }
}

#[cfg(target_os = "macos")]
static UI: Mutex<UiDelivery> = Mutex::new(UiDelivery {
    starting: None,
    ready: false,
    pending: None,
});

/// Called over authenticated local IPC after the UI has subscribed. First-click
/// coordinates are retained until then, rather than lost during WebView startup.
pub(crate) fn ui_ready(node: &Node) {
    #[cfg(target_os = "macos")]
    {
        let pending = UI.lock().unwrap().ready();
        if let Some(event) = pending {
            node.emit(event);
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = node;
}

/// macOS backend entry: single-instance lock (taken → ask the running backend to show
/// the UI, exit) → logs, settings, IPC listener (failure is a start-up failure), menu bar
/// → threads, config module, business modules (state file unreadable: IPC and menu bar
/// only) → launch the UI unless started by the login item. Runs the application main
/// loop on the calling (main) thread and returns the exit code.
pub fn run(launch: Launch) -> u8 {
    #[cfg(target_os = "macos")]
    {
        macos::run(launch, false)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = launch;
        crate::host::run_service()
    }
}

/// Packaging check: an empty private home and no desktop I/O or UI helper.
#[cfg(target_os = "macos")]
pub fn run_launch_smoke() -> u8 {
    macos::run(Launch::Login, true)
}

/// Ask the existing UI to show main, launching its helper on demand on macOS.
pub fn show_ui() -> std::io::Result<()> {
    node().map_err(std::io::Error::other)?.emit(ApiEvent::ShowWindow);
    Ok(())
}

#[cfg(target_os = "macos")]
fn deliver_ui(event: ApiEvent) -> std::io::Result<()> {
    let mut ui = UI.lock().unwrap();
    let running = crate::instance::is_ui_running(&oink_foundation::paths::data_dir())
        .ok_or_else(|| std::io::Error::other("UI process ownership unavailable"))?;
    let deliver_now = ui.request(event.clone(), running);
    if deliver_now && matches!(event, ApiEvent::TogglePanel(_)) {
        // Status clicks enter here directly. Publishing only after readiness
        // prevents an initial toggle reaching a subscribed client both before
        // ui_ready and again when the retained click is drained.
        drop(ui);
        node().map_err(std::io::Error::other)?.emit(event);
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let contents = exe
        .parent()
        .and_then(std::path::Path::parent)
        .ok_or_else(|| std::io::Error::other("application Contents unavailable"))?;
    let helper = contents.join("Helpers/Oink.app");
    if running {
        if opens_window(&event) {
            // A resident helper needs a LaunchServices activation request as
            // well as IPC. activate() in the other process does not inherit
            // the outer app's launch/reopen permission. Without -n, this sends
            // reopen to the same helper and leaves both process lifetimes intact.
            let status = ui_launch_command(&helper, &oink_foundation::paths::data_dir(), true, false).status()?;
            if !status.success() {
                return Err(std::io::Error::other("UI LaunchServices activation failed"));
            }
        }
        return Ok(());
    }
    if !ui.should_launch(&event, running) {
        return Ok(());
    }
    ui.ready = false;
    tracing::info!(foreground = opens_window(&event), "launching UI helper");
    let status =
        ui_launch_command(&helper, &oink_foundation::paths::data_dir(), opens_window(&event), true).status()?;
    if !status.success() {
        return Err(std::io::Error::other("UI LaunchServices request failed"));
    }
    // No watchdog or resident `open -W` process: a failed startup can be retried
    // by the next user click after this bounded in-flight interval.
    ui.starting = Some(std::time::Instant::now());
    Ok(())
}

#[cfg(target_os = "macos")]
fn ui_launch_command(
    helper: &std::path::Path,
    data_dir: &std::path::Path,
    foreground: bool,
    new_instance: bool,
) -> std::process::Command {
    // LaunchServices assigns the helper its own application identity and WebKit
    // data responsibility. Directly executing Contents/MacOS inherits the
    // parent's responsibility and can attribute WebKit permissions to that app
    // (observed with a worktree dev bundle on macOS 26.6.2).
    // Only panel starts use -g/--background. Passing -g for a user-requested main
    // window leaves Regular UI inactive behind the previous app on macOS 26.6.2.
    // A foreground launch lets LaunchServices transfer activation to the helper;
    // panel starts retain LSUIElement/Accessory without creating a normal window.
    let mut home = std::ffi::OsString::from("OINK_HOME=");
    home.push(data_dir);
    let mut command = std::process::Command::new("/usr/bin/open");
    if new_instance {
        command.arg("-n");
    }
    if !foreground {
        command.arg("-g");
    }
    command
        .arg(helper)
        .args([
            "--stdin",
            "/dev/null",
            "--stdout",
            "/dev/null",
            "--stderr",
            "/dev/null",
            "--env",
        ])
        .arg(home)
        .arg("--env")
        .arg(format!("{}=0", oink_autostart::LOGIN_LAUNCH_ENV));
    if !foreground {
        command.args(["--args", "--background"]);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
}

/// `AgentApi::reopen_oink`. macOS: launch the bundle with [`REOPEN_FLAG`], then quit
/// in the normal order. Windows: done by the tray-and-UI process; without one this
/// returns an error saying so.
pub fn reopen() -> ApiResult<()> {
    let node = node()?;
    #[cfg(target_os = "macos")]
    {
        let prepared = crate::update::prepare_reopen(&node)?;
        let exe = std::env::current_exe().map_err(|e| ApiError::Internal { message: e.to_string() })?;
        launch::reopen_entry(&exe, &oink_foundation::paths::data_dir())
            .map_err(|e| ApiError::Unavailable { message: e.to_string() })?;
        prepared.shutdown().map_err(|_| ApiError::Unavailable {
            message: "update drain expired".into(),
        })?;
        Ok(())
    }
    #[cfg(windows)]
    {
        if crate::instance::is_ui_running(&oink_foundation::paths::data_dir()) != Some(true) {
            return Err(ApiError::Unavailable {
                message: "没有运行中的托盘与界面进程；请用命令行 reopen".into(),
            });
        }
        node.emit(ApiEvent::ReopenOink);
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = node;
        Err(ApiError::Unsupported {
            message: "这个平台不支持重新打开 Oink".into(),
        })
    }
}

/// Quit in order: announce and end subscriptions (the UI side exits), clean up (release
/// keys, restore the cursor, disconnect every link), delete the IPC info file, exit.
pub fn quit() {
    crate::update::request_quit();
}

/// Hand native window and drag work to the backend's application thread.
/// Callers keep blocking preparation on ordinary workers and return input results
/// through InputThread commands after this callback completes.
pub(crate) fn on_main(work: impl FnOnce() + Send + 'static) {
    #[cfg(target_os = "macos")]
    macos::on_main(work);
    #[cfg(windows)]
    overlay::on_main(work);
    #[cfg(not(any(target_os = "macos", windows)))]
    work();
}

/// Run the host worker while servicing the process main loop. On macOS this
/// closure runs OFF the main thread; AppKit work must use `on_main` instead.
pub(crate) fn run_main<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    #[cfg(windows)]
    {
        overlay::run_main(work)
    }
    #[cfg(not(windows))]
    {
        oink_input::run_with_main_run_loop(work)
    }
}

/// Apply a committed native preference in the process that owns the Mac item.
#[cfg(target_os = "macos")]
pub fn set_status_icon_visible(visible: bool) {
    macos::set_status_icon_visible(visible);
}

#[cfg(all(test, debug_assertions))]
mod startup_tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn first_panel_click_waits_for_subscription_and_is_consumed_once() {
        let mut ui = UiDelivery::default();
        let anchor = PanelAnchor {
            rect: PanelRect {
                x: 10.0,
                y: 20.0,
                width: 22.0,
                height: 22.0,
            },
            work_area: PanelRect {
                x: 0.0,
                y: 24.0,
                width: 1000.0,
                height: 800.0,
            },
            scale_factor: 1.0,
        };
        assert!(!ui.request(ApiEvent::TogglePanel(anchor), false));
        // A process may already own its lock and subscribe before acknowledging
        // readiness. That click must still be retained, never published twice.
        assert!(!ui.request(ApiEvent::TogglePanel(anchor), true));
        assert!(matches!(ui.ready(), Some(ApiEvent::TogglePanel(a)) if a == anchor));
        assert!(ui.ready().is_none());
        assert!(ui.request(ApiEvent::TogglePanel(anchor), true));
        assert!(ui.pending.is_none());
        assert!(ui.request(ApiEvent::ShowWindow, true));
        assert!(ui.pending.is_none());
        assert!(!ui.request(ApiEvent::ShowWindow, false));
        assert!(matches!(ui.ready(), Some(ApiEvent::ShowWindow)));
    }

    #[test]
    fn login_stays_closed_and_explicit_reopen_always_opens() {
        for marker in [false, true] {
            assert!(!shows_on_start(Launch::Login, marker));
            assert!(shows_on_start(Launch::Reopen, marker));
        }
        assert!(shows_on_start(Launch::User, false));
        assert!(!shows_on_start(Launch::User, true));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn manual_relaunch_retries_a_helper_that_exited_before_ready() {
        let ui = UiDelivery {
            starting: Some(std::time::Instant::now()),
            ready: false,
            pending: Some(ApiEvent::ShowWindow),
        };
        assert!(ui.should_launch(&ApiEvent::ShowWindow, false));
        assert!(!ui.should_launch(&ApiEvent::ShowWindow, true));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn only_panel_launches_suppress_launchservices_activation() {
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("Oink.app");
        for foreground in [false, true] {
            let command = ui_launch_command(&helper, dir.path(), foreground, true);
            assert_eq!(command.get_program(), "/usr/bin/open");
            let args: Vec<_> = command.get_args().collect();
            assert!(args.contains(&std::ffi::OsStr::new("-n")));
            assert!(args.contains(&helper.as_os_str()));
            assert_eq!(args.contains(&std::ffi::OsStr::new("-g")), !foreground);
            assert_eq!(args.contains(&std::ffi::OsStr::new("--background")), !foreground);
            assert!(args.contains(&std::ffi::OsStr::new("OINK_LAUNCHED_AT_LOGIN=0")));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn reopening_a_resident_helper_uses_launchservices_without_creating_another_instance() {
        let dir = tempfile::tempdir().unwrap();
        let helper = dir.path().join("Oink.app");
        let command = ui_launch_command(&helper, dir.path(), true, false);
        let args: Vec<_> = command.get_args().collect();
        assert!(args.contains(&helper.as_os_str()));
        assert!(!args.contains(&std::ffi::OsStr::new("-n")));
        assert!(!args.contains(&std::ffi::OsStr::new("-g")));
    }

    #[test]
    fn only_local_emergency_and_explicit_request_open_the_window() {
        assert!(opens_window(&ApiEvent::ShowWindow));
        for local in [false, true] {
            assert_eq!(
                opens_window(&ApiEvent::EmergencyStop(crate::api::EmergencyStop {
                    device: oink_foundation::DeviceId([4; 32]).into(),
                    device_name: "test device".into(),
                    local,
                    at_ms: 1,
                })),
                local
            );
        }
        assert!(!opens_window(&ApiEvent::ReopenOink));
    }

    #[test]
    fn duplicate_backend_only_requests_window_on_existing_instance() {
        crate::test_watchdog::run(
            module_path!(),
            "duplicate_backend_only_requests_window_on_existing_instance",
            Some(2),
            duplicate_backend_only_requests_window_on_existing_instance_scenario(),
        );
    }

    async fn duplicate_backend_only_requests_window_on_existing_instance_scenario() {
        let dir = tempfile::tempdir().unwrap();
        let first = claim_instance(dir.path()).unwrap().unwrap();
        let node = Node::start(crate::node::NodeOptions {
            net: oink_test_support::network_config(dir.path().into(), "duplicate", 0),
            input: Arc::new(oink_test_support::input::FakeInput::new()),
            displays: vec![],
            display_backend: None,
            clipboard: None,
            mode_switch_in_callback: true,
            capture_reports_key_repeat: true,
            cursor_trail: false,
        })
        .await
        .unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        node.subscribe(Arc::new(move |event| {
            if matches!(event, ApiEvent::ShowWindow) {
                sent.send(()).unwrap();
            }
        }));
        let server = crate::ipc::Server::start_on(node.clone(), 0).await.unwrap();
        assert!(claim_instance(dir.path()).unwrap().is_none());
        received.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert!(received.try_recv().is_err(), "only one open request is sent");
        assert_eq!(crate::instance::is_running(dir.path()), Some(true));
        server.close().await;
        node.shutdown().await;
        drop(first);
        assert_eq!(crate::instance::is_running(dir.path()), Some(false));
    }
}
