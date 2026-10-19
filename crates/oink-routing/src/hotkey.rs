//! 全局快捷键：把界面保存的键名解析成组合键，并在本机截获的按键里识别。
//!
//! - 识别在**物理键盘所在的设备**上做（路由的 `local_key`），不论焦点此刻在哪台设备。
//! - 组合键 = 一组修饰键（Ctrl / Alt / Shift / Meta，左右任意）+ 一个普通键。普通键**按下**时，本机键盘按着的
//!   修饰键**恰好**是这一组（不多不少）才算匹配；Fn、Caps Lock 等不算修饰键，不影响匹配。
//! - 动作用字符串标识（界面 API 的 `HotkeyAction` 的 snake_case 名字），路由只负责识别、吞键并发出
//!   `StatusEvent::Hotkey`，动作由服务进程执行。新增动作不需要改这里。
//!
//! 键名约定与 `oink_agent::api::Hotkey` 相同：修饰键 `Ctrl` `Alt` `Shift` `Meta`（Mac 上是 Cmd，Windows 上是
//! Win 键），其余为 `KeyboardEvent.code` 去掉 `Key` / `Digit` 前缀（`L`、`5`、`F12`、`Backquote`、`Numpad1`）。

use oink_foundation::input::KeyCode;

/// 修饰键集合（左右不分）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Mods(u8);

impl Mods {
    pub const NONE: Self = Self(0);
    pub const CTRL: Self = Self(1);
    pub const ALT: Self = Self(2);
    pub const SHIFT: Self = Self(4);
    pub const META: Self = Self(8);

    /// 这个键是哪个修饰键（只认 HID 键盘页的 8 个修饰键）。
    pub fn of(code: KeyCode) -> Option<Self> {
        Some(match code {
            KeyCode::LEFT_CTRL | KeyCode::RIGHT_CTRL => Self::CTRL,
            KeyCode::LEFT_ALT | KeyCode::RIGHT_ALT => Self::ALT,
            KeyCode::LEFT_SHIFT | KeyCode::RIGHT_SHIFT => Self::SHIFT,
            KeyCode::LEFT_GUI | KeyCode::RIGHT_GUI => Self::META,
            _ => return None,
        })
    }

    /// 一组按着的键里的修饰键。
    pub fn held(keys: impl IntoIterator<Item = KeyCode>) -> Self {
        keys.into_iter().filter_map(Self::of).fold(Self::NONE, |a, b| a | b)
    }

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for Mods {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// 一个组合键：修饰键 + 一个普通键。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub mods: Mods,
    pub key: KeyCode,
}

/// 键名解析失败的原因。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ChordError {
    #[error("没有设置按键")]
    Empty,
    #[error("只有修饰键，缺少一个普通键")]
    NoKey,
    #[error("有两个普通键：{0} 与 {1}")]
    TwoKeys(String, String),
    #[error("无法识别的键：{0}")]
    UnknownKey(String),
    #[error("{0} 不能用作快捷键")]
    Unsupported(String),
    #[error("Ctrl + Alt + Shift + Esc 是紧急暂停")]
    Emergency,
}

impl Chord {
    /// 解析界面保存的键名列表（顺序无关）。
    pub fn parse<S: AsRef<str>>(keys: &[S]) -> Result<Self, ChordError> {
        if keys.is_empty() {
            return Err(ChordError::Empty);
        }
        let mut mods = Mods::NONE;
        let mut key: Option<(KeyCode, &str)> = None;
        for name in keys {
            let name = name.as_ref();
            let m = match name {
                "Ctrl" => Mods::CTRL,
                "Alt" => Mods::ALT,
                "Shift" => Mods::SHIFT,
                "Meta" => Mods::META,
                _ => {
                    let code = key_code(name).ok_or_else(|| ChordError::UnknownKey(name.to_string()))?;
                    if code == KeyCode::CAPS_LOCK {
                        // 大写锁定在 Mac 上由截获单独处理（切换输入法、锁定状态），不作为快捷键。
                        return Err(ChordError::Unsupported(name.to_string()));
                    }
                    if let Some((_, first)) = key {
                        return Err(ChordError::TwoKeys(first.to_string(), name.to_string()));
                    }
                    key = Some((code, name));
                    continue;
                }
            };
            mods = mods | m;
        }
        let (key, _) = key.ok_or(ChordError::NoKey)?;
        let chord = Self { mods, key };
        if chord.is_emergency() {
            return Err(ChordError::Emergency);
        }
        Ok(chord)
    }

    /// 与紧急暂停的组合键相同（路由先判断紧急暂停，这样的快捷键永远不会触发）。
    pub fn is_emergency(&self) -> bool {
        self.key == KeyCode::ESCAPE && self.mods == Mods::CTRL | Mods::ALT | Mods::SHIFT
    }
}

/// 一条快捷键绑定。`action` 是界面 API 的动作名（例如 `"lock_focus"`），焦点锁由路由在物理按下时立即处理；其余动作交给 agent。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HotkeyBinding {
    pub chord: Chord,
    pub action: String,
}

/// 本机的快捷键表。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Hotkeys {
    bindings: Vec<HotkeyBinding>,
}

impl Hotkeys {
    pub fn new(bindings: Vec<HotkeyBinding>) -> Self {
        Self { bindings }
    }

    pub fn bindings(&self) -> &[HotkeyBinding] {
        &self.bindings
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// 普通键 `key` 按下、本机键盘按着的修饰键为 `held` 时触发的动作。修饰键本身按下不触发。
    pub fn matching(&self, key: KeyCode, held: Mods) -> Option<&str> {
        if Mods::of(key).is_some() {
            return None;
        }
        self.bindings
            .iter()
            .find(|b| b.chord.key == key && b.chord.mods == held)
            .map(|b| b.action.as_str())
    }
}

/// 键名（`KeyboardEvent.code` 去掉 `Key` / `Digit` 前缀）→ HID 键码。修饰键名不在这里（见 [`Chord::parse`]）。
pub fn key_code(name: &str) -> Option<KeyCode> {
    let k = KeyCode::keyboard;
    let b = name.as_bytes();
    // 字母、数字。
    if b.len() == 1 {
        let c = b[0];
        return match c {
            b'A'..=b'Z' => Some(k(0x04 + u16::from(c - b'A'))),
            b'1'..=b'9' => Some(k(0x1E + u16::from(c - b'1'))),
            b'0' => Some(k(0x27)),
            _ => None,
        };
    }
    // F1–F24。
    if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<u16>().ok()) {
        return match n {
            1..=12 => Some(k(0x3A + n - 1)),
            13..=24 => Some(k(0x68 + n - 13)),
            _ => None,
        };
    }
    // 小键盘数字。
    if let Some(n) = name.strip_prefix("Numpad").and_then(|n| n.parse::<u16>().ok()) {
        return match n {
            1..=9 => Some(k(0x59 + n - 1)),
            0 => Some(k(0x62)),
            _ => None,
        };
    }
    let usage = match name {
        "Enter" => 0x28,
        "Escape" => 0x29,
        "Backspace" => 0x2A,
        "Tab" => 0x2B,
        "Space" => 0x2C,
        "Minus" => 0x2D,
        "Equal" => 0x2E,
        "BracketLeft" => 0x2F,
        "BracketRight" => 0x30,
        "Backslash" => 0x31,
        "IntlHash" => 0x32,
        "Semicolon" => 0x33,
        "Quote" => 0x34,
        "Backquote" => 0x35,
        "Comma" => 0x36,
        "Period" => 0x37,
        "Slash" => 0x38,
        "CapsLock" => 0x39,
        "PrintScreen" => 0x46,
        "ScrollLock" => 0x47,
        "Pause" => 0x48,
        "Insert" => 0x49,
        "Home" => 0x4A,
        "PageUp" => 0x4B,
        "Delete" => 0x4C,
        "End" => 0x4D,
        "PageDown" => 0x4E,
        "ArrowRight" => 0x4F,
        "ArrowLeft" => 0x50,
        "ArrowDown" => 0x51,
        "ArrowUp" => 0x52,
        "NumLock" => 0x53,
        "NumpadDivide" => 0x54,
        "NumpadMultiply" => 0x55,
        "NumpadSubtract" => 0x56,
        "NumpadAdd" => 0x57,
        "NumpadEnter" => 0x58,
        "NumpadDecimal" => 0x63,
        "IntlBackslash" => 0x64,
        "ContextMenu" => 0x65,
        "NumpadEqual" => 0x67,
        "Help" => 0x75,
        "NumpadComma" => 0x85,
        "IntlRo" => 0x87,
        "KanaMode" => 0x88,
        "IntlYen" => 0x89,
        "Convert" => 0x8A,
        "NonConvert" => 0x8B,
        "Lang1" => 0x90,
        "Lang2" => 0x91,
        _ => return None,
    };
    Some(k(usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(keys: &[&str]) -> Chord {
        Chord::parse(keys).unwrap()
    }

    #[test]
    fn parses_key_names() {
        assert_eq!(key_code("A"), Some(KeyCode::keyboard(0x04)));
        assert_eq!(key_code("Z"), Some(KeyCode::keyboard(0x1D)));
        assert_eq!(key_code("1"), Some(KeyCode::keyboard(0x1E)));
        assert_eq!(key_code("0"), Some(KeyCode::keyboard(0x27)));
        assert_eq!(key_code("F1"), Some(KeyCode::keyboard(0x3A)));
        assert_eq!(key_code("F12"), Some(KeyCode::keyboard(0x45)));
        assert_eq!(key_code("F13"), Some(KeyCode::keyboard(0x68)));
        assert_eq!(key_code("F24"), Some(KeyCode::keyboard(0x73)));
        assert_eq!(key_code("F25"), None);
        assert_eq!(key_code("Numpad0"), Some(KeyCode::keyboard(0x62)));
        assert_eq!(key_code("Numpad9"), Some(KeyCode::keyboard(0x61)));
        assert_eq!(key_code("Backquote"), Some(KeyCode::keyboard(0x35)));
        assert_eq!(key_code("Escape"), Some(KeyCode::ESCAPE));
        assert_eq!(key_code("a"), None);
        assert_eq!(key_code("KeyA"), None);
        assert_eq!(key_code("Ctrl"), None);
    }

    #[test]
    fn parses_chords() {
        let c = chord(&["Ctrl", "Alt", "L"]);
        assert_eq!(c.mods, Mods::CTRL | Mods::ALT);
        assert_eq!(c.key, KeyCode::keyboard(0x0F));
        // 顺序无关。
        assert_eq!(chord(&["L", "Alt", "Ctrl"]), c);
        assert_eq!(chord(&["F13"]).mods, Mods::NONE);
        assert_eq!(Chord::parse::<&str>(&[]), Err(ChordError::Empty));
        assert_eq!(Chord::parse(&["Ctrl", "Alt"]), Err(ChordError::NoKey));
        assert_eq!(
            Chord::parse(&["Ctrl", "A", "B"]),
            Err(ChordError::TwoKeys("A".into(), "B".into()))
        );
        assert_eq!(
            Chord::parse(&["Ctrl", "Hyper"]),
            Err(ChordError::UnknownKey("Hyper".into()))
        );
        assert_eq!(
            Chord::parse(&["Ctrl", "CapsLock"]),
            Err(ChordError::Unsupported("CapsLock".into()))
        );
        assert_eq!(
            Chord::parse(&["Ctrl", "Alt", "Shift", "Escape"]),
            Err(ChordError::Emergency)
        );
        assert!(Chord::parse(&["Ctrl", "Alt", "Escape"]).is_ok());
    }

    #[test]
    fn held_modifiers_ignore_side_and_non_modifiers() {
        let held = Mods::held([
            KeyCode::RIGHT_CTRL,
            KeyCode::LEFT_ALT,
            KeyCode::CAPS_LOCK,
            KeyCode::new(0xFF, 0x03), // Mac 的 Fn
            KeyCode::keyboard(0x04),
        ]);
        assert_eq!(held, Mods::CTRL | Mods::ALT);
        assert_eq!(Mods::held([KeyCode::LEFT_GUI, KeyCode::RIGHT_GUI]), Mods::META);
    }

    #[test]
    fn matches_exact_modifiers_only() {
        let hk = Hotkeys::new(vec![
            HotkeyBinding {
                chord: chord(&["Ctrl", "Alt", "L"]),
                action: "lock_focus".into(),
            },
            HotkeyBinding {
                chord: chord(&["Meta", "Shift", "H"]),
                action: "return_pointer_home".into(),
            },
        ]);
        let l = key_code("L").unwrap();
        let h = key_code("H").unwrap();
        assert_eq!(hk.matching(l, Mods::CTRL | Mods::ALT), Some("lock_focus"));
        // 多一个或少一个修饰键都不算。
        assert_eq!(hk.matching(l, Mods::CTRL | Mods::ALT | Mods::SHIFT), None);
        assert_eq!(hk.matching(l, Mods::CTRL), None);
        assert_eq!(hk.matching(l, Mods::NONE), None);
        assert_eq!(hk.matching(h, Mods::META | Mods::SHIFT), Some("return_pointer_home"));
        assert_eq!(hk.matching(h, Mods::CTRL | Mods::ALT), None);
        // 修饰键本身按下不触发。
        assert_eq!(hk.matching(KeyCode::LEFT_ALT, Mods::CTRL | Mods::ALT), None);
    }
}
