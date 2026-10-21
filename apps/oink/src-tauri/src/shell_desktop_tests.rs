//! Opt-in Windows desktop regression. No backend or installed app is used.
//! Native focus/cursor handling may synthesize Alt or release the OS cursor clip.
//! Explicit input-capable desktop authorization is required; never run under a hold.
//! The test executable needs the Common-Controls v6 manifest used by Tauri's
//! product binary. Run alone with an external process timeout for regressions
//! that block the event loop before a command can return.
use std::sync::{Arc, Mutex};

use tauri::{AppHandle, Manager};

use super::{Shell, View, open_normal, shell_open, shell_ready};

#[tauri::command]
fn inspect_window(app: AppHandle, view: View, hide: bool) -> Result<isize, String> {
    let window = app.get_webview_window(view.label()).ok_or("missing window")?;
    if !window.is_visible().map_err(|e| e.to_string())? || window.is_minimized().map_err(|e| e.to_string())? {
        return Err(format!("{} is hidden or minimized", view.label()));
    }
    let position = window.outer_position().map_err(|e| e.to_string())?;
    let size = window.outer_size().map_err(|e| e.to_string())?;
    let on_screen = app
        .available_monitors()
        .map_err(|e| e.to_string())?
        .iter()
        .any(|monitor| {
            let p = monitor.position();
            let s = monitor.size();
            position.x < p.x + s.width as i32
                && position.y < p.y + s.height as i32
                && position.x + size.width as i32 > p.x
                && position.y + size.height as i32 > p.y
        });
    if !on_screen || size.width == 0 || size.height == 0 {
        return Err(format!("{} is off screen or empty", view.label()));
    }
    println!("{} visible on screen: {position:?} {size:?}", view.label());
    if hide {
        window.hide().map_err(|e| e.to_string())?;
    }
    Ok(window.hwnd().map_err(|e| e.to_string())?.0 as isize)
}

#[derive(Clone, Default)]
struct Outcome(Arc<Mutex<Option<String>>>);

#[tauri::command]
fn desktop_done(app: AppHandle, error: String) {
    *app.state::<Outcome>().0.lock().unwrap() = Some(error);
    app.exit(0);
}

#[tauri::command]
fn boot_info() -> crate::boot::BootInfo {
    crate::boot::BootInfo::new(false)
}

#[tauri::command]
fn frontend_error(message: String) {
    eprintln!("isolated frontend: {message}");
}

#[test]
#[ignore = "requires explicit desktop input authorization, isolated OINK_HOME, Common-Controls manifest and external timeout; run alone with custom-protocol"]
fn secondary_windows_open_from_webview_ipc() {
    let home = std::env::var_os("OINK_HOME").expect("set an isolated OINK_HOME before running");
    assert_ne!(
        std::path::PathBuf::from(&home),
        oink_foundation::paths::StoragePaths::windows(&std::path::PathBuf::from(
            std::env::var_os("LOCALAPPDATA").unwrap()
        ))
        .data
    );
    let outcome = Outcome::default();
    let result = outcome.clone();
    let script = r#"
                    if (new URLSearchParams(location.search).get('view') === 'main') (async () => {
                        const invoke = window.__TAURI_INTERNALS__.invoke;
                        const delay = () => new Promise(resolve => setTimeout(resolve, 2500));
                        try {
                            for (const view of ['main', 'about', 'quit']) {
                                await Promise.all([invoke('shell_open', {view}), invoke('shell_open', {view})]);
                                await delay();
                                const original = await invoke('inspect_window', {view, hide: true});
                                await invoke('shell_open', {view});
                                const reopened = await invoke('inspect_window', {view, hide: false});
                                if (original !== reopened) throw new Error(`${view} was recreated`);
                            }
                            await invoke('desktop_done', {error: ''});
                        } catch (error) {
                            await invoke('desktop_done', {error: String(error)});
                        }
                    })();
                "#;
    let app = tauri::Builder::default()
        // Match the product shell: this fixture needs no raw device events.
        .device_event_filter(tauri::DeviceEventFilter::Always)
        .any_thread()
        .plugin(tauri::plugin::Builder::<tauri::Wry>::new("creation-regression").js_init_script(script).build())
        .manage(Shell::default())
        .manage(crate::app_icon::AppIcon::default())
        .manage(crate::live_views::Demand::default())
        .manage(outcome)
        .invoke_handler(tauri::generate_handler![
            shell_open,
            shell_ready,
            inspect_window,
            desktop_done,
            boot_info,
            frontend_error
        ])
        .setup(|app| {
            // Even the first driver uses the production queue. Its initialization
            // script may issue IPC while its constructor is still pumping.
            open_normal(app.handle(), View::Main);
            Ok(())
        })
        .build(tauri::generate_context!())
        .unwrap();
    assert_eq!(app.run_return(|_, _| {}), 0);
    assert_eq!(result.0.lock().unwrap().as_deref(), Some(""));
}
