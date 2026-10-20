//! Local notification calls are filtered before choosing or contacting a screen.
use super::ScreenKey;
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationLevel {
    Notice,
    Info,
    Debug,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationSetting {
    Disabled,
    Minimal,
    #[default]
    Default,
    Verbose,
}
impl NotificationSetting {
    pub fn allows(self, level: NotificationLevel) -> bool {
        match self {
            Self::Disabled => false,
            Self::Minimal => level == NotificationLevel::Notice,
            Self::Default => level != NotificationLevel::Debug,
            Self::Verbose => true,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    pub level: NotificationLevel,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub screen: Option<ScreenKey>,
    #[serde(default)]
    pub persistent: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NotificationReceipt {
    pub id: String,
    /// None when the destination resolves its current pointer screen.
    pub screen: Option<ScreenKey>,
    pub remote: bool,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_level_filter_combinations() {
        use NotificationLevel::*;
        use NotificationSetting::*;
        for (setting, expected) in [
            (Disabled, [false, false, false]),
            (Minimal, [true, false, false]),
            (Default, [true, true, false]),
            (Verbose, [true, true, true]),
        ] {
            assert_eq!([Notice, Info, Debug].map(|l| setting.allows(l)), expected);
        }
        assert_eq!(NotificationSetting::default(), Default);
    }
}
