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
                let view = match action {
                    oink_agent::native_controls::ShellAction::Show => crate::shell_policy::View::Main,
                    oink_agent::native_controls::ShellAction::About => crate::shell_policy::View::About,
                    oink_agent::native_controls::ShellAction::Quit => crate::shell_policy::View::Quit,
                };
                let _ = crate::shell::shell_open(app, view).await;
            }
            Ok(Ok(None)) => {}
            error => tracing::warn!(?error, "native control failed"),
        }
    });
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
