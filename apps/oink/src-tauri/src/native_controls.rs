//! Native shell controls resolve every click against current backend facts.
use oink_agent::{config::UiLanguage, device_cards::DeviceCardsView};
use std::sync::Mutex;
use tauri::{AppHandle, Manager};
#[derive(Default)]
struct Snapshot {
    view: Option<DeviceCardsView>,
    language: UiLanguage,
    rendered: Option<oink_agent::native_controls::Model>,
}
static SNAPSHOT: Mutex<Snapshot> = Mutex::new(Snapshot {
    view: None,
    language: UiLanguage::Auto,
    rendered: None,
});

pub fn cards(app: &AppHandle, view: &DeviceCardsView) {
    SNAPSHOT.lock().unwrap().view = Some(view.clone());
    refresh(app);
}
pub fn language(app: &AppHandle, language: UiLanguage) {
    SNAPSHOT.lock().unwrap().language = language;
    refresh(app);
}
pub fn initialize(app: &AppHandle) {
    let handle = app.clone();
    let api = app.state::<crate::app::Backend>().api.clone();
    if let Some(api) = api {
        std::thread::spawn(move || {
            if let Ok(p) = api.ui_preferences() {
                language(&handle, p.ui_language);
            }
            if let Ok(view) = api.device_cards() {
                cards(&handle, &view);
            }
        });
    }
}
pub fn invoke(app: &AppHandle, id: &str) {
    let app = app.clone();
    let id = id.to_owned();
    tauri::async_runtime::spawn(async move {
        let api = app.state::<crate::app::Backend>().api.clone();
        let Some(api) = api else {
            return;
        };
        let result =
            tauri::async_runtime::spawn_blocking(move || oink_agent::native_controls::dispatch(api.as_ref(), &id))
                .await;
        match result {
            Ok(Ok(Some(action))) => {
                shell_action(&app, action).await;
            }
            Ok(Ok(None)) => {}
            error => tracing::warn!(?error, "native control failed"),
        }
    });
}
/// Native Quit is already an explicit choice. Use the same graceful command as
/// the confirmed in-app flow, rather than opening that flow's confirmation view.
pub(crate) async fn shell_action(app: &AppHandle, action: oink_agent::native_controls::ShellAction) {
    route_shell_action(
        action,
        |view| async move {
            let _ = crate::shell::shell_open(app.clone(), view).await;
        },
        || async move {
            if let Err(error) = crate::commands::shell_quit(app.clone()).await {
                tracing::warn!(?error, "native quit failed");
            }
        },
    )
    .await;
}
async fn route_shell_action<O, Q>(
    action: oink_agent::native_controls::ShellAction,
    open: impl FnOnce(crate::shell_policy::View) -> O,
    quit: impl FnOnce() -> Q,
) where
    O: std::future::Future<Output = ()>,
    Q: std::future::Future<Output = ()>,
{
    use crate::shell_policy::View;
    use oink_agent::native_controls::ShellAction;
    match action {
        ShellAction::Show => open(View::Main).await,
        ShellAction::About => open(View::About).await,
        ShellAction::Quit => quit().await,
    }
}

fn refresh(app: &AppHandle) {
    let mut snapshot = SNAPSHOT.lock().unwrap();
    let Some(view) = &snapshot.view else {
        return;
    };
    let language = if snapshot.language == UiLanguage::Auto {
        UiLanguage::Auto.resolve(
            crate::system_locale::preferred_languages()
                .first()
                .map(String::as_str)
                .unwrap_or(""),
        )
    } else {
        snapshot.language
    };
    let model = oink_agent::native_controls::model(view, language);
    if snapshot.rendered.as_ref() == Some(&model) {
        return;
    }
    snapshot.rendered = Some(model.clone());
    drop(snapshot);
    crate::app::on_main(app, move |app| {
        // Snapshot callbacks may arrive on different threads. A delayed older
        // main-thread job must not overwrite a newer menu already scheduled.
        if SNAPSHOT.lock().unwrap().rendered.as_ref() != Some(&model) {
            return;
        }
        #[cfg(target_os = "macos")]
        crate::dock_controls::update(app, &model);
        #[cfg(windows)]
        if let Err(error) = windows_menu(app, &model) {
            tracing::warn!(%error,"native tray menu update failed");
        }
    });
}
#[cfg(windows)]
fn windows_menu(app: &AppHandle, model: &oink_agent::native_controls::Model) -> tauri::Result<()> {
    use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
    let menu = Menu::new(app)?;
    for group in &model.groups {
        let submenu = Submenu::new(app, &group.label, true)?;
        for item in &group.items {
            submenu.append(&CheckMenuItem::with_id(
                app,
                format!("native-{}", item.id),
                &item.label,
                item.enabled,
                item.checked.unwrap_or(false),
                None::<&str>,
            )?)?;
        }
        menu.append(&submenu)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    for item in &model.tasks {
        menu.append(&MenuItem::with_id(
            app,
            format!("native-{}", item.id),
            &item.label,
            item.enabled,
            None::<&str>,
        )?)?;
    }
    if let Some(tray) = app.tray_by_id("main") {
        tray.set_menu(Some(menu.clone()))?;
        crate::tray_windows::set_menu(menu, model)?;
    }
    if let Err(error) = crate::jump_controls::update(app, model) {
        tracing::warn!(%error,"taskbar menu update failed");
    }
    Ok(())
}

#[cfg(test)]
mod quit_tests {
    use super::*;
    #[tokio::test]
    async fn explicit_native_quit_runs_shutdown_without_opening_any_window() {
        let quit = std::cell::Cell::new(0);
        route_shell_action(
            oink_agent::native_controls::ShellAction::Quit,
            |view| async move {
                panic!("native quit opened {view:?} instead of shutting down");
            },
            || async {
                quit.set(quit.get() + 1);
            },
        )
        .await;
        assert_eq!(quit.get(), 1);
    }
    #[tokio::test]
    async fn native_window_actions_keep_their_requested_views() {
        use crate::shell_policy::View;
        use oink_agent::native_controls::ShellAction;
        for (action, expected) in [(ShellAction::Show, View::Main), (ShellAction::About, View::About)] {
            let opened = std::cell::Cell::new(None);
            route_shell_action(
                action,
                |view| {
                    opened.set(Some(view));
                    std::future::ready(())
                },
                || async {
                    panic!("window action requested shutdown");
                },
            )
            .await;
            assert_eq!(opened.get(), Some(expected));
        }
    }
}
