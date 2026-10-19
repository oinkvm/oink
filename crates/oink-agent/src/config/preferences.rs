//! Typed local preferences shared by the backend and native UI. These never
//! become group registers or remote owner commands.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}
impl LogLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

/// Decimal megabytes of ordinary logs only. Diagnostics have separate budgets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct LogMaxMb(u32);
impl LogMaxMb {
    pub const fn get(self) -> u32 {
        self.0
    }
    pub const fn bytes(self) -> u64 {
        self.0 as u64 * 1_000_000
    }
}
impl Default for LogMaxMb {
    fn default() -> Self {
        Self(1024)
    }
}
impl TryFrom<u32> for LogMaxMb {
    type Error = &'static str;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        if value == 0 {
            Err("log_max_mb must be positive")
        } else {
            Ok(Self(value))
        }
    }
}
impl From<LogMaxMb> for u32 {
    fn from(value: LogMaxMb) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiLanguage {
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "zh-Hans")]
    SimplifiedChinese,
    #[serde(rename = "en")]
    English,
    #[serde(rename = "ja")]
    Japanese,
    #[serde(rename = "ko")]
    Korean,
}
impl UiLanguage {
    /// Map `Auto` and the system's first preferred language to a UI language:
    /// any Japanese or Korean tag selects that language, zh/zh-Hans/zh-CN/zh-SG
    /// select Simplified Chinese, and everything else (Traditional Chinese
    /// included) is English. The web UI applies the same rule.
    pub fn resolve(self, system_language: &str) -> Self {
        match self {
            Self::Auto => {
                let mut parts = system_language.split(['-', '_']);
                let base = parts.next().unwrap_or("");
                if base.eq_ignore_ascii_case("ja") {
                    return Self::Japanese;
                }
                if base.eq_ignore_ascii_case("ko") {
                    return Self::Korean;
                }
                let simplified = parts.next().is_none_or(|v| {
                    ["Hans", "CN", "SG"]
                        .iter()
                        .any(|allowed| v.eq_ignore_ascii_case(allowed))
                });
                if base.eq_ignore_ascii_case("zh") && simplified {
                    Self::SimplifiedChinese
                } else {
                    Self::English
                }
            }
            explicit => explicit,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

/// Opaque sRGB, serialized as #RRGGBB. Alpha is deliberately not configurable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ThemeColor(String);
impl ThemeColor {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn rgb(&self) -> [u8; 3] {
        std::array::from_fn(|i| u8::from_str_radix(&self.0[1 + i * 2..3 + i * 2], 16).expect("validated RGB"))
    }
}
impl TryFrom<String> for ThemeColor {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() != 7 || !value.starts_with('#') || !value.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit) {
            return Err("theme color must be #RRGGBB");
        }
        Ok(Self(value.to_ascii_uppercase()))
    }
}
impl From<ThemeColor> for String {
    fn from(value: ThemeColor) -> Self {
        value.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemeAccent {
    pub light: ThemeColor,
    pub dark: ThemeColor,
}
impl ThemeAccent {
    pub fn resolved(&self, mode: ThemeMode, system_dark: bool) -> &ThemeColor {
        if mode == ThemeMode::Dark || (mode == ThemeMode::System && system_dark) {
            &self.dark
        } else {
            &self.light
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u32", into = "u32")]
pub struct EmergencyHoldMs(u32);
impl Default for EmergencyHoldMs {
    fn default() -> Self {
        Self(5000)
    }
}
impl EmergencyHoldMs {
    pub const fn get(self) -> u32 {
        self.0
    }
}
impl TryFrom<u32> for EmergencyHoldMs {
    type Error = &'static str;
    fn try_from(value: u32) -> Result<Self, Self::Error> {
        if matches!(value, 0 | 5000) {
            Ok(Self(value))
        } else {
            Err("emergency_hold_ms must be 0 or 5000")
        }
    }
}
impl From<EmergencyHoldMs> for u32 {
    fn from(value: EmergencyHoldMs) -> Self {
        value.0
    }
}

pub const DEFAULT_WINDOW_WIDTH: u32 = 1040;
pub const DEFAULT_WINDOW_HEIGHT: u32 = 680;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounds_and_platform_independent_defaults() {
        assert_eq!(LogMaxMb::default().get(), 1024);
        assert_eq!(LogMaxMb::default().bytes(), 1_024_000_000);
        assert!(serde_json::from_str::<LogMaxMb>("0").is_err());
        assert_eq!(LogMaxMb::try_from(u32::MAX).unwrap().bytes(), 4_294_967_295_000_000);
        assert_eq!(UiLanguage::Auto.resolve("zh-TW"), UiLanguage::English);
        assert_eq!(UiLanguage::Auto.resolve("fr"), UiLanguage::English);
        assert_eq!(EmergencyHoldMs::default().get(), 5000);
        assert!(EmergencyHoldMs::try_from(1000).is_err());
        assert_eq!((DEFAULT_WINDOW_WIDTH, DEFAULT_WINDOW_HEIGHT), (1040, 680));
        assert!(ThemeColor::try_from("#FF0088FF".to_owned()).is_err());
        assert!(ThemeColor::try_from("#红色".to_owned()).is_err());
        assert_eq!(
            ThemeColor::try_from("#12aBcD".to_owned()).unwrap().rgb(),
            [18, 171, 205]
        );
    }

    #[test]
    fn automatic_language_selects_japanese_and_korean() {
        for locale in ["ja", "ja-JP", "JA_jp", "ja-Jpan-JP"] {
            assert_eq!(UiLanguage::Auto.resolve(locale), UiLanguage::Japanese, "{locale}");
        }
        for locale in ["ko", "ko-KR", "ko_KP", "ko-Kore-KR"] {
            assert_eq!(UiLanguage::Auto.resolve(locale), UiLanguage::Korean, "{locale}");
        }
        for locale in ["jav", "kok"] {
            assert_eq!(UiLanguage::Auto.resolve(locale), UiLanguage::English, "{locale}");
        }
        assert_eq!(UiLanguage::Korean.resolve("ja-JP"), UiLanguage::Korean);
        assert_eq!(serde_json::to_string(&UiLanguage::Japanese).unwrap(), "\"ja\"");
        assert_eq!(
            serde_json::from_str::<UiLanguage>("\"ko\"").unwrap(),
            UiLanguage::Korean
        );
    }

    #[test]
    fn automatic_language_distinguishes_chinese_scripts_and_explicit_choices() {
        for locale in ["zh", "zh-Hans", "zh-Hans-CN", "zh-CN", "zh-SG", "ZH_cn"] {
            assert_eq!(
                UiLanguage::Auto.resolve(locale),
                UiLanguage::SimplifiedChinese,
                "{locale}"
            );
        }
        for locale in [
            "zh-Hant",
            "zh-Hant-CN",
            "zh-TW",
            "zh-HK",
            "zh-MO",
            "zh-Hant-HK",
            "en-US",
            "fr",
            "",
            "zhx",
            "zh-XX",
        ] {
            assert_eq!(UiLanguage::Auto.resolve(locale), UiLanguage::English, "{locale}");
        }
        assert_eq!(UiLanguage::English.resolve("zh-CN"), UiLanguage::English);
        assert_eq!(
            UiLanguage::SimplifiedChinese.resolve("en-US"),
            UiLanguage::SimplifiedChinese
        );
    }
}

impl Default for ThemeAccent {
    fn default() -> Self {
        // The UI kit's pink preset uses the same opaque accent in both modes.
        let pink = ThemeColor::try_from("#FF6B8E".to_owned()).expect("pink preset");
        Self {
            light: pink.clone(),
            dark: pink,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GameModePreference {
    #[default]
    Auto,
    On,
    Off,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeedMode {
    #[default]
    Physical,
    ScreenProportional,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScreenPreference {
    pub display: String,
    pub gain: f64,
    pub enabled: bool,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScreenPreferences {
    pub screens: Vec<ScreenPreference>,
    pub device_gain: f64,
}
impl Default for ScreenPreferences {
    fn default() -> Self {
        Self {
            screens: Vec::new(),
            device_gain: 1.0,
        }
    }
}
