//! Native menus share the UI's explicit language choice.
use oink_agent::config::UiLanguage;
use tauri::AppHandle;

#[cfg(any(target_os = "macos", test))]
#[derive(serde::Deserialize)]
struct MenuLabels {
    about: String,
    open: String,
    quit: String,
    edit: String,
    undo: String,
    redo: String,
    cut: String,
    copy: String,
    paste: String,
    select_all: String,
}

#[cfg(any(target_os = "macos", test))]
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MenuCatalog {
    en: MenuLabels,
    #[serde(rename = "zh-Hans")]
    zh_hans: MenuLabels,
    ja: MenuLabels,
    ko: MenuLabels,
}

#[cfg(any(target_os = "macos", test))]
fn catalog() -> &'static MenuCatalog {
    static CATALOG: std::sync::OnceLock<MenuCatalog> = std::sync::OnceLock::new();
    CATALOG.get_or_init(|| {
        serde_json::from_str(include_str!("../locales/native-menu.json")).expect("complete native menu catalogue")
    })
}

#[cfg(any(target_os = "macos", test))]
fn labels(language: UiLanguage) -> &'static MenuLabels {
    let catalog = catalog();
    match language {
        UiLanguage::Auto => labels(crate::system_locale::auto_language()),
        UiLanguage::SimplifiedChinese => &catalog.zh_hans,
        UiLanguage::English => &catalog.en,
        UiLanguage::Japanese => &catalog.ja,
        UiLanguage::Korean => &catalog.ko,
    }
}

#[tauri::command]
pub fn shell_set_language(app: AppHandle, language: UiLanguage) {
    crate::app::on_main(&app, move |app| {
        if let Err(error) = install(app, language) {
            tracing::error!(%error, "native menu language update failed");
        }
    });
}

#[cfg(target_os = "macos")]
pub fn install(app: &AppHandle, language: UiLanguage) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
    let labels = labels(language);
    let about = MenuItem::with_id(app, "shell-about", &labels.about, true, None::<&str>)?;
    let open = MenuItem::with_id(app, "shell-main", &labels.open, true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "shell-quit", &labels.quit, true, Some("CmdOrCtrl+Q"))?;
    let product = Submenu::with_items(
        app,
        "Oink",
        true,
        &[&about, &open, &PredefinedMenuItem::separator(app)?, &quit],
    )?;
    let edit = Submenu::with_items(
        app,
        &labels.edit,
        true,
        &[
            &PredefinedMenuItem::undo(app, Some(&labels.undo))?,
            &PredefinedMenuItem::redo(app, Some(&labels.redo))?,
            &PredefinedMenuItem::separator(app)?,
            &PredefinedMenuItem::cut(app, Some(&labels.cut))?,
            &PredefinedMenuItem::copy(app, Some(&labels.copy))?,
            &PredefinedMenuItem::paste(app, Some(&labels.paste))?,
            &PredefinedMenuItem::select_all(app, Some(&labels.select_all))?,
        ],
    )?;
    app.set_menu(Menu::with_items(app, &[&product, &edit])?)?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn install(_app: &AppHandle, _language: UiLanguage) -> tauri::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use oink_agent::config::UiLanguage;

    #[test]
    fn about_menu_uses_the_product_name_in_every_language() {
        assert_eq!(super::labels(UiLanguage::English).about, "About Oink");
        assert_eq!(super::labels(UiLanguage::SimplifiedChinese).about, "关于 Oink");
        assert_eq!(super::labels(UiLanguage::Japanese).about, "Oink について");
        assert_eq!(super::labels(UiLanguage::Korean).about, "Oink에 관하여");
    }

    #[test]
    fn native_menu_catalog_contains_every_complete_language() {
        let catalog = super::catalog();
        for labels in [&catalog.en, &catalog.zh_hans, &catalog.ja, &catalog.ko] {
            for label in [
                &labels.about,
                &labels.open,
                &labels.quit,
                &labels.edit,
                &labels.undo,
                &labels.redo,
                &labels.cut,
                &labels.copy,
                &labels.paste,
                &labels.select_all,
            ] {
                assert!(!label.trim().is_empty());
            }
        }
    }
}
