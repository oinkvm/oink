//! Local appearance and process preferences. Owner options are edited separately.
use crate::config::{LogLevel, LogMaxMb, ThemeAccent, ThemeMode, UiLanguage};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiPreferences {
    pub ui_language: UiLanguage,
    #[serde(default)]
    pub notification_level: super::NotificationSetting,
    pub theme_mode: ThemeMode,
    pub theme_accent: ThemeAccent,
    pub show_status_icon: bool,
    pub confirm_quit: bool,
    pub log_level: LogLevel,
    pub log_max_mb: LogMaxMb,
}
impl From<&crate::config::Settings> for UiPreferences {
    fn from(settings: &crate::config::Settings) -> Self {
        Self {
            ui_language: settings.ui_language,
            notification_level: settings.notification_level,
            theme_mode: settings.theme_mode,
            theme_accent: settings.theme_accent.clone(),
            show_status_icon: settings.show_status_icon,
            confirm_quit: settings.confirm_quit,
            log_level: settings.log_level,
            log_max_mb: settings.log_max_mb,
        }
    }
}
impl UiPreferences {
    pub(crate) fn apply(&self, settings: &mut crate::config::Settings) {
        settings.ui_language = self.ui_language;
        settings.notification_level = self.notification_level;
        settings.theme_mode = self.theme_mode;
        settings.theme_accent = self.theme_accent.clone();
        settings.show_status_icon = self.show_status_icon;
        settings.confirm_quit = self.confirm_quit;
        settings.log_level = self.log_level;
        settings.log_max_mb = self.log_max_mb;
    }
}
