//! Source-owned physical input preferences. Tables contain HID keys, including sides.
use crate::{DeviceId, input::KeyCode};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyPreset {
    WinFeel,
    MacFeel,
    Custom,
    #[default]
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyMapping {
    pub from: KeyCode,
    /// `KeyCode::NONE` drops both edges; it is never a source key.
    pub to: KeyCode,
}

/// Only physical Caps Lock has tap/hold semantics; outputs are final HID keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsTapHold {
    pub tap: KeyCode,
    pub hold: KeyCode,
}
pub const CAPS_HOLD_US: u64 = 500_000;

/// Editor memory, independent of the active preset and never used for routing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedCustomKeymap {
    pub table: Vec<KeyMapping>,
    pub caps_hold: Option<KeyCode>,
    pub caps_split: bool,
    pub allow_fn: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyboardMapping {
    pub preset: KeyPreset,
    pub table: Vec<KeyMapping>,
    /// Custom mapping. MacFeel resolves its conditional default at the source.
    pub caps_tap_hold: Option<CapsTapHold>,
    /// Absent in released 0.2.x settings; retain the existing active map on load.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_custom: Option<SavedCustomKeymap>,
}
impl KeyboardMapping {
    pub fn preset(preset: KeyPreset, source_os: &str, target_os: &str) -> Self {
        use KeyCode as K;
        let pairs: &[(KeyCode, KeyCode)] = match (preset, source_os, target_os) {
            (KeyPreset::MacFeel, "macos", "windows") | (KeyPreset::MacFeel, "windows", "macos") => &[
                (K::LEFT_CTRL, K::LEFT_GUI),
                (K::LEFT_GUI, K::LEFT_CTRL),
                (K::RIGHT_CTRL, K::RIGHT_GUI),
                (K::RIGHT_GUI, K::RIGHT_CTRL),
            ],
            (KeyPreset::WinFeel, "macos", "windows") | (KeyPreset::WinFeel, "windows", "macos") => &[
                (K::LEFT_ALT, K::LEFT_GUI),
                (K::LEFT_GUI, K::LEFT_ALT),
                (K::RIGHT_ALT, K::RIGHT_GUI),
                (K::RIGHT_GUI, K::RIGHT_ALT),
            ],
            _ => &[],
        };
        Self {
            preset,
            table: pairs.iter().map(|&(from, to)| KeyMapping { from, to }).collect(),
            caps_tap_hold: None,
            saved_custom: None,
        }
    }
    pub fn caps_tap_hold(&self, source_os: &str, target_os: &str, mac_caps_switch: bool) -> Option<CapsTapHold> {
        match self.preset {
            KeyPreset::Custom => self.caps_tap_hold,
            KeyPreset::MacFeel if source_os == "macos" && target_os == "windows" && mac_caps_switch => {
                Some(CapsTapHold {
                    tap: KeyCode::LEFT_SHIFT,
                    hold: KeyCode::CAPS_LOCK,
                })
            }
            _ => None,
        }
    }
    pub fn map(&self, code: KeyCode) -> KeyCode {
        self.table.iter().find(|m| m.from == code).map_or(code, |m| m.to)
    }
    fn validate(&self) -> Result<(), String> {
        if let Some(caps) = self.caps_tap_hold {
            if self.preset != KeyPreset::Custom {
                return Err("Caps Lock 短按 / 长按请使用自定义预设".into());
            }
            if [caps.tap, caps.hold]
                .iter()
                .any(|k| *k != KeyCode::NONE && !candidate_keys().contains(k))
            {
                return Err("Caps Lock 短按 / 长按需要支持的 HID 目标键".into());
            }
            if self.table.iter().any(|m| m.from == KeyCode::CAPS_LOCK) {
                return Err("Caps Lock 不能同时设置普通映射与短按 / 长按".into());
            }
        }
        validate_table(&self.table)?;
        if let Some(saved) = &self.saved_custom {
            validate_table(&saved.table)?;
            if saved
                .caps_hold
                .is_some_and(|k| k != KeyCode::NONE && !candidate_keys().contains(&k))
                || (saved.caps_split && saved.caps_hold.is_none())
                || (saved.allow_fn && !saved.table.iter().any(|m| m.from == KeyCode::APPLE_FN))
            {
                return Err("保存的自定义键映射选项无效".into());
            }
        }
        Ok(())
    }
}

fn validate_table(table: &[KeyMapping]) -> Result<(), String> {
    if table.len() > 256 {
        return Err("键映射最多 256 项".into());
    }
    let mut seen = std::collections::BTreeSet::new();
    for m in table {
        if !seen.insert(m.from) {
            return Err("同一物理键只能映射一次".into());
        }
        if !candidate_keys().contains(&m.from) || (m.to != KeyCode::NONE && !candidate_keys().contains(&m.to)) {
            return Err("自定义映射需要支持的 HID 物理键".into());
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TargetTransform {
    pub reverse_wheel: bool,
    pub keyboard: KeyboardMapping,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetOverride {
    pub target: DeviceId,
    /// Each component independently inherits the OS setting when absent.
    pub reverse_wheel: Option<bool>,
    pub keyboard: Option<KeyboardMapping>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InputTransform {
    pub macos: TargetTransform,
    pub windows: TargetTransform,
    pub overrides: Vec<TargetOverride>,
}
impl InputTransform {
    pub fn initial(source_os: &str) -> Self {
        let mut value = Self::default();
        if source_os == "macos" {
            value.windows.keyboard = KeyboardMapping::preset(KeyPreset::MacFeel, source_os, "windows");
        }
        if source_os == "windows" {
            value.macos.keyboard = KeyboardMapping::preset(KeyPreset::MacFeel, source_os, "macos");
        }
        value
    }
    pub fn resolve(&self, target: DeviceId, target_os: &str) -> TargetTransform {
        let mut value = match target_os {
            "macos" => self.macos.clone(),
            "windows" => self.windows.clone(),
            _ => TargetTransform::default(),
        };
        if let Some(o) = self.overrides.iter().find(|o| o.target == target) {
            if let Some(wheel) = o.reverse_wheel {
                value.reverse_wheel = wheel;
            }
            if let Some(keyboard) = &o.keyboard {
                value.keyboard = keyboard.clone();
            }
        }
        value
    }
    pub fn validate(&self) -> Result<(), String> {
        self.macos.keyboard.validate()?;
        self.windows.keyboard.validate()?;
        if self.overrides.len() > 16 {
            return Err("目标设备覆盖最多 16 项".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        for o in &self.overrides {
            if !seen.insert(o.target) {
                return Err("目标设备覆盖重复".into());
            }
            if let Some(k) = &o.keyboard {
                k.validate()?;
            }
        }
        Ok(())
    }
}

/// Physical keys offered by the editor; platform tables determine native support.
pub fn candidate_keys() -> Vec<KeyCode> {
    (4..=0xe7)
        .map(KeyCode::keyboard)
        .chain(
            [
                0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xcd, 0xcf, 0xe2, 0xe9, 0xea, 0x183, 0x18a, 0x192, 0x194, 0x221, 0x223,
                0x224, 0x225, 0x226, 0x227, 0x22a, 0x29f, 0x2a2,
            ]
            .map(|usage| KeyCode::new(KeyCode::PAGE_CONSUMER, usage)),
        )
        .chain([
            KeyCode::new(0xff, 3),
            KeyCode::new(1, 0x81),
            KeyCode::new(1, 0x82),
            KeyCode::new(1, 0x83),
            KeyCode::new(1, 0x9b),
        ])
        .collect()
}

/// Session-tap rewriting does not own these separate native state/event paths.
/// Keep the table for remote targets; the local direction passes this entry as-is.
pub fn mac_local_key_hint(code: KeyCode) -> Option<&'static str> {
    if code == KeyCode::CAPS_LOCK {
        Some("控制源设备本机时不支持；Caps Lock 请在 macOS 系统设置 > 键盘 > 修饰键中修改")
    } else if code == KeyCode::new(0xff, 3) {
        Some("控制源设备本机时不支持；Fn / 地球键使用独立系统状态，保留原键")
    } else if (code.page() == KeyCode::PAGE_CONSUMER
        && matches!(code.usage(), 0xb3 | 0xb4 | 0xb5 | 0xb6 | 0xcd | 0xe2 | 0xe9 | 0xea))
        || (code.page() == KeyCode::PAGE_KEYBOARD && matches!(code.usage(), 0x7f..=0x81))
    {
        Some("控制源设备本机时不支持；此媒体键使用 NX 系统事件，保留原键")
    } else {
        None
    }
}
pub fn mac_local_mapping_hint(mapping: KeyMapping) -> Option<&'static str> {
    mac_local_key_hint(mapping.from).or_else(|| mac_local_key_hint(mapping.to))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saved_custom_is_persisted_editor_memory_not_an_active_mapping() {
        let mut keyboard = KeyboardMapping::preset(KeyPreset::MacFeel, "macos", "windows");
        keyboard.saved_custom = Some(SavedCustomKeymap {
            table: vec![
                KeyMapping {
                    from: KeyCode::LEFT_CTRL,
                    to: KeyCode::NONE,
                },
                KeyMapping {
                    from: KeyCode::APPLE_FN,
                    to: KeyCode::NONE,
                },
            ],
            caps_hold: Some(KeyCode::NONE),
            caps_split: true,
            allow_fn: true,
        });
        assert!(keyboard.validate().is_ok());
        assert_eq!(keyboard.map(KeyCode::LEFT_CTRL), KeyCode::LEFT_GUI);
        assert_eq!(keyboard.map(KeyCode::APPLE_FN), KeyCode::APPLE_FN);
        assert_eq!(keyboard.caps_tap_hold("macos", "windows", false), None);
        assert_eq!(
            serde_json::from_str::<KeyboardMapping>(&serde_json::to_string(&keyboard).unwrap()).unwrap(),
            keyboard
        );
        let mut invalid = keyboard.clone();
        invalid.saved_custom.as_mut().unwrap().table[0].from = KeyCode::NONE;
        assert!(invalid.validate().is_err());
        let mut invalid = keyboard.clone();
        invalid.saved_custom.as_mut().unwrap().caps_hold = None;
        assert!(invalid.validate().is_err());
        keyboard.saved_custom.as_mut().unwrap().table.pop();
        assert!(keyboard.validate().is_err());
    }
    #[test]
    fn released_0_2_keyboard_without_saved_custom_loads_without_rewriting_it() {
        // The v0.2.0 KeyboardMapping fields and enum spellings are unchanged.
        let json =
            r#"{"preset":"custom","table":[{"from":458976,"to":458982}],"caps_tap_hold":{"tap":458977,"hold":458809}}"#;
        let keyboard: KeyboardMapping = serde_json::from_str(json).unwrap();
        assert_eq!(keyboard.saved_custom, None);
        assert_eq!(keyboard.map(KeyCode::LEFT_CTRL), KeyCode::RIGHT_ALT);
        assert_eq!(
            keyboard.caps_tap_hold("macos", "windows", false),
            Some(CapsTapHold {
                tap: KeyCode::LEFT_SHIFT,
                hold: KeyCode::CAPS_LOCK
            })
        );
        assert!(keyboard.validate().is_ok());
        assert_eq!(
            serde_json::to_value(&keyboard).unwrap(),
            serde_json::from_str::<serde_json::Value>(json).unwrap()
        );
    }
    #[test]
    fn defaults_presets_and_component_inheritance() {
        let source = InputTransform::initial("macos");
        assert!(source.macos.keyboard.table.is_empty());
        assert_eq!(source.windows.keyboard.map(KeyCode::RIGHT_GUI), KeyCode::RIGHT_CTRL);
        let target = DeviceId([2; 32]);
        let mut source = source;
        source.overrides.push(TargetOverride {
            target,
            reverse_wheel: Some(true),
            keyboard: None,
        });
        let resolved = source.resolve(target, "windows");
        assert!(resolved.reverse_wheel);
        assert_eq!(resolved.keyboard.map(KeyCode::LEFT_GUI), KeyCode::LEFT_CTRL);
        let win = KeyboardMapping::preset(KeyPreset::WinFeel, "windows", "macos");
        assert_eq!(win.map(KeyCode::RIGHT_ALT), KeyCode::RIGHT_GUI);
        assert_eq!(win.map(KeyCode::LEFT_CTRL), KeyCode::LEFT_CTRL);
    }
    #[test]
    fn unknown_fields_are_ignored_and_duplicate_sources_rejected() {
        let mut v: InputTransform = serde_json::from_str(r#"{"future":true,"macos":{"future":1}}"#).unwrap();
        let m = KeyMapping {
            from: KeyCode::keyboard(4),
            to: KeyCode::keyboard(5),
        };
        v.macos.keyboard.table = vec![m, m];
        assert!(v.validate().is_err());
    }
}

#[cfg(test)]
mod caps_tests {
    use super::*;
    #[test]
    fn mac_feel_caps_is_conditional_and_directional() {
        for preset in [
            KeyPreset::MacFeel,
            KeyPreset::WinFeel,
            KeyPreset::None,
            KeyPreset::Custom,
        ] {
            for (source, target) in [
                ("macos", "windows"),
                ("windows", "macos"),
                ("macos", "macos"),
                ("windows", "windows"),
            ] {
                let keyboard = KeyboardMapping::preset(preset, source, target);
                assert_eq!(keyboard.caps_tap_hold(source, target, false), None);
                let expected =
                    (preset == KeyPreset::MacFeel && source == "macos" && target == "windows").then_some(CapsTapHold {
                        tap: KeyCode::LEFT_SHIFT,
                        hold: KeyCode::CAPS_LOCK,
                    });
                assert_eq!(keyboard.caps_tap_hold(source, target, true), expected);
            }
        }
    }
    #[test]
    fn custom_caps_roundtrip_validation_and_override() {
        let id = DeviceId([7; 32]);
        let caps = CapsTapHold {
            tap: KeyCode::RIGHT_SHIFT,
            hold: KeyCode::ESCAPE,
        };
        let keyboard = KeyboardMapping {
            saved_custom: None,
            preset: KeyPreset::Custom,
            caps_tap_hold: Some(caps),
            table: vec![],
        };
        let mut settings = InputTransform::initial("macos");
        settings.overrides.push(TargetOverride {
            target: id,
            keyboard: Some(keyboard.clone()),
            reverse_wheel: None,
        });
        assert_eq!(
            settings
                .resolve(id, "windows")
                .keyboard
                .caps_tap_hold("macos", "windows", false),
            Some(caps)
        );
        let json = serde_json::to_string(&settings).unwrap();
        assert_eq!(serde_json::from_str::<InputTransform>(&json).unwrap(), settings);
        assert!(settings.validate().is_ok());
        let mut bad = keyboard;
        bad.table.push(KeyMapping {
            from: KeyCode::CAPS_LOCK,
            to: KeyCode::LEFT_CTRL,
        });
        assert!(bad.validate().is_err());
        bad.table.clear();
        bad.caps_tap_hold.as_mut().unwrap().hold = KeyCode::new(0xffff, 0xffff);
        assert!(bad.validate().is_err());
        let old: KeyboardMapping = serde_json::from_str(r#"{"preset":"custom","table":[]}"#).unwrap();
        assert_eq!(old.caps_tap_hold, None);
    }
}

#[cfg(test)]
mod empty_mapping_tests {
    use super::*;
    #[test]
    fn none_is_a_destination_only_and_roundtrips_in_tables_and_caps() {
        let mut transform = InputTransform::default();
        transform.windows.keyboard = KeyboardMapping {
            saved_custom: None,
            preset: KeyPreset::Custom,
            table: vec![KeyMapping {
                from: KeyCode::APPLE_FN,
                to: KeyCode::NONE,
            }],
            caps_tap_hold: Some(CapsTapHold {
                tap: KeyCode::NONE,
                hold: KeyCode::NONE,
            }),
        };
        assert!(transform.validate().is_ok());
        let json = serde_json::to_string(&transform).unwrap();
        assert_eq!(serde_json::from_str::<InputTransform>(&json).unwrap(), transform);
        transform.windows.keyboard.table[0].from = KeyCode::NONE;
        assert!(transform.validate().is_err());
    }
}
