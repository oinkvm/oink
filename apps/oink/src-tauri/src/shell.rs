//! One UI event loop owns every product WebView and its activation policy.
use std::sync::Mutex;

use oink_agent::backend::PanelAnchor;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

use crate::shell_policy::{Activation, OpenTrigger, PANEL_HEIGHT, PANEL_WIDTH, View, panel_bounds};

#[derive(Default)]
pub struct Shell {
    anchor: Mutex<Option<PanelAnchor>>,
    creation: crate::creation_queue::CreationQueue<WindowRequest>,
    window_ready: Mutex<crate::shell_policy::WindowReadyGate>,
    activation: Mutex<std::collections::HashMap<String, Activation>>,
    #[cfg(target_os = "macos")]
    idle_exit: Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
}

#[tauri::command]
pub async fn shell_titlebar_center(window: tauri::WebviewWindow) -> Result<Option<f64>, String> {
    #[cfg(target_os = "macos")]
    {
        if window.label() != "main" {
            return Ok(None);
        }
        let (send, receive) = tokio::sync::oneshot::channel();
        window
            .with_webview(move |webview| {
                // SAFETY: with_webview runs on AppKit's main thread and lends the
                // live WKWebView, which inherits NSView, for this callback only.
                let view = unsafe { &*webview.inner().cast::<objc2_app_kit::NSView>() };
                let center = view
                    .window()
                    .and_then(|native| crate::main_window_macos::traffic_light_center(&native, view));
                let _ = send.send(center);
            })
            .map_err(|error| error.to_string())?;
        receive.await.map_err(|error| error.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = window;
        Ok(None)
    }
}

enum WindowRequest {
    Normal(
        View,
        Activation,
        Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
    ),
    Panel(PanelAnchor),
}

fn enqueue(app: &AppHandle, request: WindowRequest) {
    if !app.state::<Shell>().creation.submit(request) {
        return;
    }
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        handle.state::<Shell>().creation.drain(|mut request| {
            let (view, reply) = match &mut request {
                WindowRequest::Normal(view, _, reply) => (*view, reply.take()),
                WindowRequest::Panel(_) => (View::Panel, None),
            };
            #[cfg(windows)]
            let result = execute_request(&handle, request);
            #[cfg(target_os = "macos")]
            let result = {
                let request_handle = handle.clone();
                on_ui(&handle, move || execute_request(&request_handle, request))
            };
            let result = result.map_err(|error| error.to_string());
            if let Err(error) = &result {
                tracing::error!(view = view.label(), %error, "could not open product window");
            }
            if let Some(reply) = reply {
                let _ = reply.send(result);
            }
        });
    });
}

fn execute_request(app: &AppHandle, request: WindowRequest) -> tauri::Result<()> {
    match request {
        WindowRequest::Normal(view, activation, _) => {
            app.state::<Shell>()
                .activation
                .lock()
                .unwrap()
                .insert(view.label().into(), activation);
            on_ui(app, {
                let app = app.clone();
                move || {
                    if activation == Activation::User {
                        close_panel_on_main(&app);
                    }
                    Ok(())
                }
            })?;
            let result = create_normal(app, view);
            if result.is_err() {
                let app = app.clone();
                let _ = on_ui(&app.clone(), move || {
                    sync_activation(&app);
                    Ok(())
                });
            }
            result
        }
        WindowRequest::Panel(anchor) => create_or_close_panel(app, anchor),
    }
}

/// UI callbacks must only enqueue. WebView2's native creation pump can reenter
/// IPC/tray/window callbacks while Wry owns its WebContext mutex. One worker
/// drains all normal and panel requests, with no gate held across construction.
/// AppKit constructors are dispatched to its main thread by that worker; the
/// reentrant UI thread never waits for the worker or starts another constructor.
pub fn open_normal(app: &AppHandle, view: View) {
    debug_assert!(view.normal());
    open_with_activation(app, view, OpenTrigger::Launch.activation());
}

pub fn open_with_activation(app: &AppHandle, view: View, activation: Activation) {
    enqueue(app, WindowRequest::Normal(view, activation, None));
}

#[tauri::command]
pub async fn shell_open(app: AppHandle, view: View) -> Result<(), String> {
    if !view.normal() {
        // Only a status-item/tray click supplies a trustworthy monitor anchor.
        return Ok(());
    }
    let (send, receive) = tokio::sync::oneshot::channel();
    enqueue(
        &app,
        WindowRequest::Normal(view, OpenTrigger::WindowAction.activation(), Some(send)),
    );
    receive.await.map_err(|error| {
        tracing::error!(view = view.label(), %error, "window creation worker ended without a result");
        error.to_string()
    })?
}

#[tauri::command]
pub fn shell_close(app: AppHandle, window: tauri::WebviewWindow) {
    let label = window.label().to_owned();
    crate::app::on_main(&app, move |app| {
        if label == "panel" {
            close_panel_on_main(app);
        } else if let Some(window) = app.get_webview_window(&label) {
            let _ = window.destroy();
        }
    });
}

#[tauri::command]
pub fn shell_resize_panel(app: AppHandle, window: tauri::WebviewWindow, width: f64, height: f64) {
    if window.label() != "panel" || !(240.0..=800.0).contains(&width) || !(100.0..=1000.0).contains(&height) {
        return;
    }
    crate::app::on_main(&app, move |app| {
        let Some(anchor) = *app.state::<Shell>().anchor.lock().unwrap() else {
            return;
        };
        let Some(rect) = panel_bounds(anchor, width, height) else {
            return;
        };
        #[cfg(target_os = "macos")]
        crate::panel_macos::resize(&window, rect);
        #[cfg(windows)]
        if let Some(panel) = app.get_webview_window("panel") {
            let _ = panel.set_size(tauri::PhysicalSize::new(rect.width, rect.height));
            let _ = panel.set_position(tauri::PhysicalPosition::new(rect.x, rect.y));
        }
    });
}

#[tauri::command]
pub fn shell_show_panel(app: AppHandle, window: tauri::WebviewWindow) {
    if window.label() != "panel" {
        return;
    }
    #[cfg(target_os = "macos")]
    crate::app::on_main(&app, move |_| crate::panel_macos::show(&window));
    #[cfg(not(target_os = "macos"))]
    let _ = app;
}

pub fn toggle_panel(app: &AppHandle, anchor: PanelAnchor) {
    enqueue(app, WindowRequest::Panel(anchor));
}

fn create_or_close_panel(app: &AppHandle, anchor: PanelAnchor) -> tauri::Result<()> {
    if app.get_webview_window("panel").is_some() {
        return on_ui(app, {
            let app = app.clone();
            move || {
                close_panel_on_main(&app);
                Ok(())
            }
        });
    }
    let Some(rect) = panel_bounds(anchor, PANEL_WIDTH, PANEL_HEIGHT) else {
        return Ok(());
    };
    *app.state::<Shell>().anchor.lock().unwrap() = Some(anchor);
    let builder = builder(app, View::Panel)
        .inner_size(PANEL_WIDTH, PANEL_HEIGHT)
        .decorations(false)
        .resizable(false)
        .skip_taskbar(true)
        .visible(false)
        .focused(false);
    #[cfg(windows)]
    let builder = builder.always_on_top(true);
    // Tauri/Wry clears both native and WebView backings. The macOS panel paints
    // while invisible and is destroyed on close, so no hidden view keeps running.
    #[cfg(target_os = "macos")]
    let builder = builder
        .transparent(true)
        .background_color(tauri::window::Color(0, 0, 0, 0))
        .background_throttling(tauri::utils::config::BackgroundThrottlingPolicy::Disabled)
        .shadow(false);
    let result = builder.build().and_then(|window| {
        let handle = app.clone();
        on_ui(app, move || {
            #[cfg(target_os = "macos")]
            {
                let _ = window;
                crate::panel_macos::prepare(&handle, rect, anchor)
            }
            #[cfg(windows)]
            {
                let _ = handle;
                crate::cursor_dispatch_windows::prepare(&window)?;
                window.set_position(tauri::PhysicalPosition::new(rect.x, rect.y))?;
                window.set_size(tauri::PhysicalSize::new(rect.width, rect.height))?;
                show_windows(&window, OpenTrigger::Tray.activation())
            }
        })
    });
    if result.is_err() {
        let handle = app.clone();
        let _ = on_ui(app, move || {
            close_panel_on_main(&handle);
            Ok(())
        });
    }
    result
}

pub fn close_panel(app: &AppHandle) {
    crate::app::on_main(app, close_panel_on_main);
}

fn close_panel_on_main(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    crate::panel_macos::close();
    if let Some(panel) = app.get_webview_window("panel") {
        let _ = panel.destroy();
    }
    *app.state::<Shell>().anchor.lock().unwrap() = None;
    crate::live_views::closed(app, "panel");
}

fn builder(app: &AppHandle, view: View) -> WebviewWindowBuilder<'_, tauri::Wry, AppHandle> {
    // App URLs preserve the query in both devUrl and the embedded asset protocol.
    let path = format!("index.html?view={}", view.label());
    let builder = WebviewWindowBuilder::new(app, view.label(), WebviewUrl::App(path.into()))
        .title("Oink")
        .initialization_script(crate::app::ERROR_REPORTER)
        .initialization_script(crate::system_locale::initialization_script());
    #[cfg(windows)]
    let builder = builder.data_directory(oink_foundation::paths::data_dir().join("WebView2"));
    builder
}

#[tauri::command]
pub fn shell_ready(app: AppHandle, window: tauri::WebviewWindow, generation: u32) {
    if !View::from_label(window.label()).is_some_and(View::normal) {
        return;
    }
    crate::app::on_main(&app, move |app| {
        let ready = {
            let shell = app.state::<Shell>();
            let mut gate = shell.window_ready.lock().unwrap();
            gate.ready(window.label(), generation)
        };
        if ready && let Err(error) = show_normal(app, &window) {
            tracing::error!(view = window.label(), %error, "could not reveal ready product window");
        }
    });
}

fn create_normal(app: &AppHandle, view: View) -> tauri::Result<()> {
    debug_assert!(view.normal());
    #[cfg(target_os = "macos")]
    {
        crate::app_icon::dock(app, true);
        app.set_activation_policy(tauri::ActivationPolicy::Regular)?;
        crate::app_icon::dock(app, true);
    }
    let label = view.label();
    let created = app.get_webview_window(label).is_none();
    if created {
        tracing::info!(view = label, "creating product window");
        let (width, height) = match view {
            View::Main => (1040.0, 680.0),
            View::About => (360.0, 500.0),
            View::Quit => (480.0, 320.0),
            View::Panel => unreachable!(),
        };
        // The transparent gutter contains only the web silhouette's shadow.
        // Keep the actual product content at its original size/minimum.
        let gutter = if view == View::Main { 24.0 } else { 0.0 };
        let builder = builder(app, view)
            .inner_size(width + gutter * 2.0, height + gutter * 2.0)
            .center()
            .visible(false)
            .focused(false);
        let builder = if view == View::Main {
            builder
                .min_inner_size(820.0 + gutter * 2.0, 560.0 + gutter * 2.0)
                .visible(false)
                // Tauri/Wry clears both the native backing and WebView backing
                // (including WKWebView drawsBackground/underPageBackgroundColor).
                // Disabling the system shadow avoids a rounded rectangle behind
                // the CSS superellipse. About keeps its ordinary native frame.
                .transparent(true)
                .background_color(tauri::window::Color(0, 0, 0, 0))
                .shadow(false)
        } else {
            builder
                .resizable(false)
                .maximizable(false)
                .minimizable(view != View::About)
        };
        #[cfg(target_os = "macos")]
        let builder = builder
            .title_bar_style(tauri::TitleBarStyle::Overlay)
            .hidden_title(true)
            // Keep the prototype's inset. The web toolbar aligns to the actual
            // native control bounds via shell_titlebar_center: this inset alone
            // produces different visible centres on macOS 14 and 26.
            .traffic_light_position(tauri::LogicalPosition::new(18.0 + gutter, 24.0 + gutter));
        #[cfg(windows)]
        let builder = builder.decorations(view == View::About).shadow(view != View::Main);
        let generation = app.state::<Shell>().window_ready.lock().unwrap().begin_creation(label);
        let builder = builder.initialization_script(format!(
            "Object.defineProperty(window, '__OINK_WINDOW_GENERATION__', {{value: {generation}}});"
        ));
        let window = match builder.build() {
            Ok(window) => window,
            Err(error) => {
                app.state::<Shell>().window_ready.lock().unwrap().forget(label);
                return Err(error);
            }
        };
        #[cfg(windows)]
        let _ = window;
        #[cfg(target_os = "macos")]
        if view == View::Main {
            crate::main_window_macos::prepare(&window)?;
        }
        // Keep the native window ordered out until React commits its initial
        // DOM/theme and fonts. No alpha-zero orderFront or WebKit rAF wait:
        // invisible/occluded WKWebViews can stop frames even with background
        // throttling disabled (observed on macOS 26). The DOM-ready IPC runs
        // without a frame; showing/activating then permits the first paint.
    }
    if let Some(window) = app.get_webview_window(label) {
        tracing::info!(view = label, "product window prepared");
        #[cfg(windows)]
        return finish_normal_windows(app, window, created, created && view == View::Main);
        #[cfg(target_os = "macos")]
        {
            let reveal = {
                let shell = app.state::<Shell>();
                let mut gate = shell.window_ready.lock().unwrap();
                if created {
                    gate.created(label)
                } else {
                    gate.can_show(label)
                }
            };
            if reveal {
                show_normal(app, &window)?;
            }
        }
    }
    Ok(())
}

/// The worker may wait for native UI work, but UI callbacks never wait for the
/// creation queue. The one-slot channel also supports an inline main-thread call.
fn on_ui(app: &AppHandle, f: impl FnOnce() -> tauri::Result<()> + Send + 'static) -> tauri::Result<()> {
    let (send, receive) = std::sync::mpsc::sync_channel(1);
    app.run_on_main_thread(move || {
        let _ = send.send(f());
    })?;
    receive.recv().map_err(|_| tauri::Error::FailedToReceiveMessage)?
}

#[cfg(windows)]
fn finish_normal_windows(
    app: &AppHandle,
    window: tauri::WebviewWindow,
    created: bool,
    prepare_main: bool,
) -> tauri::Result<()> {
    let handle = app.clone();
    on_ui(app, move || {
        crate::cursor_dispatch_windows::prepare(&window)?;
        // Win32 child creation and subclassing require the owning UI thread.
        if prepare_main {
            crate::main_window_windows::prepare(&window)?;
        }
        let reveal = {
            let shell = handle.state::<Shell>();
            let mut gate = shell.window_ready.lock().unwrap();
            if created {
                gate.created(window.label())
            } else {
                gate.can_show(window.label())
            }
        };
        if reveal {
            show_normal(&handle, &window)?;
        }
        Ok(())
    })
}

fn show_normal(_app: &AppHandle, window: &tauri::WebviewWindow) -> tauri::Result<()> {
    // Startup, retained ShowWindow and LaunchServices Reopen can request the
    // same view. Restore only when needed; an already focused window needs no
    // further activation (and no second initial reveal).
    if window.is_visible()? && window.is_focused()? {
        return Ok(());
    }
    #[cfg(windows)]
    crate::app_icon::prepare_window(_app, window);
    #[cfg(not(windows))]
    window.unminimize()?;
    #[cfg(target_os = "macos")]
    {
        // SAFETY: the requesting NSWindow is live on AppKit's main thread.
        let native = unsafe { &*window.ns_window()?.cast::<objc2_app_kit::NSWindow>() };
        native.setIgnoresMouseEvents(false);
        native.setAlphaValue(1.0);
    }
    #[cfg(windows)]
    {
        let activation = _app
            .state::<Shell>()
            .activation
            .lock()
            .unwrap()
            .get(window.label())
            .copied()
            .unwrap_or(Activation::Background);
        show_windows(window, activation)?;
    }
    #[cfg(not(windows))]
    window.show()?;
    #[cfg(target_os = "macos")]
    {
        // AppKit activation and all native preparation stay on its main thread.
        let mtm = objc2::MainThreadMarker::new().expect("normal window on main thread");
        objc2_app_kit::NSApplication::sharedApplication(mtm).activate();
    }
    #[cfg(not(windows))]
    focus_window(window)?;
    tracing::info!(view = window.label(), "ready product window shown");
    Ok(())
}

#[cfg(windows)]
fn show_windows(window: &tauri::WebviewWindow, activation: Activation) -> tauri::Result<()> {
    use windows::Win32::UI::WindowsAndMessaging::{SW_SHOWNOACTIVATE, ShowWindow};
    let hwnd = windows::Win32::Foundation::HWND(window.hwnd()?.0);
    // Tao show/unminimize may activate before our policy runs. Reveal/restore
    // without activation first, then use only SetForegroundWindow for explicit
    // user actions. Never use Tao's Alt-injection fallback or AttachThreadInput.
    // Windows 10/11 documented API; interactive verification is still pending.
    // SAFETY: the live product HWND belongs to this UI thread.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    }
    if activation == Activation::User {
        focus_window(window)?;
    }
    Ok(())
}

fn focus_window(window: &tauri::WebviewWindow) -> tauri::Result<()> {
    #[cfg(windows)]
    {
        // Tao's set_focus falls back to injecting Alt down/up when Windows
        // refuses foreground activation. Input sharing can forward those keys
        // to another device. Respect the foreground policy without synthesizing
        // input; a visible window remains available from the taskbar if denied.
        let hwnd = windows::Win32::Foundation::HWND(window.hwnd()?.0);
        // SAFETY: this is the live product HWND on its owning UI thread.
        unsafe {
            if !windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd).as_bool() {
                tracing::warn!(view = window.label(), "Windows declined foreground activation");
            }
        }
        Ok(())
    }
    #[cfg(not(windows))]
    window.set_focus()
}

pub fn sync_activation(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    {
        let normal = app
            .webview_windows()
            .keys()
            .any(|label| View::from_label(label).is_some_and(View::normal));
        let policy = if normal {
            tauri::ActivationPolicy::Regular
        } else {
            tauri::ActivationPolicy::Accessory
        };
        if normal {
            crate::app_icon::dock(app, true);
        }
        if let Err(error) = app.set_activation_policy(policy) {
            tracing::error!(%error, "activation policy failed");
        }
        let shell = app.state::<Shell>();
        let mut timer = shell.idle_exit.lock().unwrap();
        if let Some(timer) = timer.take() {
            timer.abort();
        }
        if app.webview_windows().is_empty() {
            let app = app.clone();
            *timer = Some(tauri::async_runtime::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                crate::app::on_main(&app, |app| {
                    if app.webview_windows().is_empty() {
                        app.exit(0);
                    }
                });
            }));
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = app;
}

pub fn shutdown(app: &AppHandle) {
    app.state::<crate::app::Backend>().control.lock().unwrap().take();
    close_panel_on_main(app);
    if let Some(remote) = &app.state::<crate::app::Backend>().remote {
        remote.stop_following();
    }
    crate::app::shutdown_capture(app);
}

#[cfg(windows)]
pub fn panel_lost_focus(app: &AppHandle) {
    use windows::Win32::{Foundation::POINT, UI::WindowsAndMessaging::GetCursorPos};
    let mut point = POINT::default();
    // SAFETY: GetCursorPos writes one POINT and never changes pointer position.
    let at_status = unsafe { GetCursorPos(&mut point) }.is_ok()
        && app.state::<Shell>().anchor.lock().unwrap().is_some_and(|a| {
            point.x as f64 >= a.rect.x
                && (point.x as f64) < a.rect.x + a.rect.width
                && point.y as f64 >= a.rect.y
                && (point.y as f64) < a.rect.y + a.rect.height
        });
    if !at_status {
        close_panel(app);
    }
}

pub fn panel_visible(app: &AppHandle) -> bool {
    app.state::<Shell>().anchor.lock().unwrap().is_some() && app.get_webview_window("panel").is_some()
}

pub fn window_destroyed(app: &AppHandle, label: &str) {
    app.state::<Shell>().window_ready.lock().unwrap().forget(label);
    #[cfg(target_os = "macos")]
    if label == "main" {
        crate::main_window_macos::closed();
    }
    if label == "panel" {
        #[cfg(target_os = "macos")]
        crate::panel_macos::close();
        *app.state::<Shell>().anchor.lock().unwrap() = None;
    }
    sync_activation(app);
}

#[cfg(all(test, windows))]
#[path = "shell_desktop_tests.rs"]
mod desktop_tests;
