//! HID Usage ↔ macOS 虚拟键码（`kVK_*`，Carbon `Events.h`）。
//!
//! 参考 Chromium `ui/events/keycodes/dom/dom_code_data.inc` 与 macOS 对 PC 键盘的处理：
//! - `PAIRS` 是一一对应的主表，双向使用；
//! - `HID_ONLY` 是只用于 HID → mac 的补充（没有专属 mac 键码，按 macOS 对 PC 键盘的习惯映射到相近的键）；
//! - `MAC_ONLY` 是只用于 mac → HID 的补充；
//! - `NX_KEYS` 是系统定义事件（NX_SYSDEFINED，subtype 8）里的媒体键 / 亮度键 ↔ HID。

use oink_foundation::input::KeyCode;

/// macOS 虚拟键码。
pub type MacKeyCode = u16;

const fn k(usage: u16) -> KeyCode {
    KeyCode::keyboard(usage)
}

const fn c(usage: u16) -> KeyCode {
    KeyCode::new(KeyCode::PAGE_CONSUMER, usage)
}

/// `kVK_CapsLock`。
pub const MAC_CAPS_LOCK: MacKeyCode = 0x39;

/// Apple 厂商页（0xFF）的 Fn 键（`kHIDUsage_AV_TopCase_KeyboardFn`）。
pub const APPLE_FN: KeyCode = KeyCode::new(0x00FF, 0x0003);

/// (HID, kVK) 一一对应。
const PAIRS: &[(KeyCode, MacKeyCode)] = &[
    // 字母
    (k(0x04), 0x00), // A
    (k(0x05), 0x0B), // B
    (k(0x06), 0x08), // C
    (k(0x07), 0x02), // D
    (k(0x08), 0x0E), // E
    (k(0x09), 0x03), // F
    (k(0x0A), 0x05), // G
    (k(0x0B), 0x04), // H
    (k(0x0C), 0x22), // I
    (k(0x0D), 0x26), // J
    (k(0x0E), 0x28), // K
    (k(0x0F), 0x25), // L
    (k(0x10), 0x2E), // M
    (k(0x11), 0x2D), // N
    (k(0x12), 0x1F), // O
    (k(0x13), 0x23), // P
    (k(0x14), 0x0C), // Q
    (k(0x15), 0x0F), // R
    (k(0x16), 0x01), // S
    (k(0x17), 0x11), // T
    (k(0x18), 0x20), // U
    (k(0x19), 0x09), // V
    (k(0x1A), 0x0D), // W
    (k(0x1B), 0x07), // X
    (k(0x1C), 0x10), // Y
    (k(0x1D), 0x06), // Z
    // 数字
    (k(0x1E), 0x12), // 1
    (k(0x1F), 0x13), // 2
    (k(0x20), 0x14), // 3
    (k(0x21), 0x15), // 4
    (k(0x22), 0x17), // 5
    (k(0x23), 0x16), // 6
    (k(0x24), 0x1A), // 7
    (k(0x25), 0x1C), // 8
    (k(0x26), 0x19), // 9
    (k(0x27), 0x1D), // 0
    // 控制与符号
    (k(0x28), 0x24), // Return
    (k(0x29), 0x35), // Escape
    (k(0x2A), 0x33), // Backspace（kVK_Delete）
    (k(0x2B), 0x30), // Tab
    (k(0x2C), 0x31), // Space
    (k(0x2D), 0x1B), // - _
    (k(0x2E), 0x18), // = +
    (k(0x2F), 0x21), // [ {
    (k(0x30), 0x1E), // ] }
    (k(0x31), 0x2A), // \ |
    (k(0x33), 0x29), // ; :
    (k(0x34), 0x27), // ' "
    (k(0x35), 0x32), // ` ~
    (k(0x36), 0x2B), // , <
    (k(0x37), 0x2F), // . >
    (k(0x38), 0x2C), // / ?
    (k(0x39), 0x39), // Caps Lock
    // F1–F12
    (k(0x3A), 0x7A),
    (k(0x3B), 0x78),
    (k(0x3C), 0x63),
    (k(0x3D), 0x76),
    (k(0x3E), 0x60),
    (k(0x3F), 0x61),
    (k(0x40), 0x62),
    (k(0x41), 0x64),
    (k(0x42), 0x65),
    (k(0x43), 0x6D),
    (k(0x44), 0x67),
    (k(0x45), 0x6F),
    // 编辑与导航
    (k(0x49), 0x72), // Insert ↔ kVK_Help（Apple 全尺寸键盘上同一位置）
    (k(0x4A), 0x73), // Home
    (k(0x4B), 0x74), // Page Up
    (k(0x4C), 0x75), // Delete（kVK_ForwardDelete）
    (k(0x4D), 0x77), // End
    (k(0x4E), 0x79), // Page Down
    (k(0x4F), 0x7C), // →
    (k(0x50), 0x7B), // ←
    (k(0x51), 0x7D), // ↓
    (k(0x52), 0x7E), // ↑
    // 小键盘
    (k(0x53), 0x47), // Num Lock ↔ kVK_ANSI_KeypadClear
    (k(0x54), 0x4B), // KP /
    (k(0x55), 0x43), // KP *
    (k(0x56), 0x4E), // KP -
    (k(0x57), 0x45), // KP +
    (k(0x58), 0x4C), // KP Enter
    (k(0x59), 0x53), // KP 1
    (k(0x5A), 0x54), // KP 2
    (k(0x5B), 0x55), // KP 3
    (k(0x5C), 0x56), // KP 4
    (k(0x5D), 0x57), // KP 5
    (k(0x5E), 0x58), // KP 6
    (k(0x5F), 0x59), // KP 7
    (k(0x60), 0x5B), // KP 8
    (k(0x61), 0x5C), // KP 9
    (k(0x62), 0x52), // KP 0
    (k(0x63), 0x41), // KP .
    (k(0x64), 0x0A), // ISO 左 Shift 旁的键 ↔ kVK_ISO_Section
    (k(0x65), 0x6E), // Application / 菜单键
    (k(0x67), 0x51), // KP =
    // F13–F20
    (k(0x68), 0x69),
    (k(0x69), 0x6B),
    (k(0x6A), 0x71),
    (k(0x6B), 0x6A),
    (k(0x6C), 0x40),
    (k(0x6D), 0x4F),
    (k(0x6E), 0x50),
    (k(0x6F), 0x5A),
    // 音量
    (k(0x7F), 0x4A), // Mute
    (k(0x80), 0x48), // Volume Up
    (k(0x81), 0x49), // Volume Down
    // 日文键盘
    (k(0x85), 0x5F), // KP ,（kVK_JIS_KeypadComma）
    (k(0x87), 0x5E), // International1 ろ ↔ kVK_JIS_Underscore
    (k(0x89), 0x5D), // International3 ¥ ↔ kVK_JIS_Yen
    (k(0x90), 0x68), // Lang1 かな ↔ kVK_JIS_Kana
    (k(0x91), 0x66), // Lang2 英数 ↔ kVK_JIS_Eisu
    // 修饰键
    (k(0xE0), 0x3B),  // Left Control
    (k(0xE1), 0x38),  // Left Shift
    (k(0xE2), 0x3A),  // Left Option
    (k(0xE3), 0x37),  // Left Command
    (k(0xE4), 0x3E),  // Right Control
    (k(0xE5), 0x3C),  // Right Shift
    (k(0xE6), 0x3D),  // Right Option
    (k(0xE7), 0x36),  // Right Command
    (APPLE_FN, 0x3F), // Fn（kVK_Function）
    // 新款 Mac 键盘的功能键：普通的 keyDown / keyUp（Carbon 没有常量），系统按符号热键处理。
    // 2026-09-29 本机 macOS 26.6 注入实测：0xA0 打开调度中心，0xB1 打开聚焦搜索，0x83 打开启动台（Apps）。
    (c(0x029F), 0xA0),                  // 调度中心 ↔ AC Desktop Show All Windows
    (c(0x02A2), 0x83),                  // 启动台 / Apps ↔ AC Desktop Show All Applications
    (c(0x0221), 0xB1),                  // 聚焦搜索 ↔ AC Search
    (c(0x00CF), 0xB0),                  // 听写 ↔ Voice Command
    (KeyCode::new(0x01, 0x009B), 0xB2), // 勿扰模式 ↔ Generic Desktop「System Do Not Disturb」
];

/// 只用于 HID → mac。
const HID_ONLY: &[(KeyCode, MacKeyCode)] = &[
    (k(0x32), 0x2A), // Non-US # ~（ISO 回车旁）：macOS 报为 kVK_ANSI_Backslash
    (k(0x46), 0x69), // Print Screen → F13（macOS 对 PC 键盘的习惯）
    (k(0x47), 0x6B), // Scroll Lock → F14
    (k(0x48), 0x71), // Pause → F15
    (k(0x75), 0x72), // Help → kVK_Help
    (k(0x88), 0x68), // International2 カタカナ/ひらがな → kVK_JIS_Kana
];

/// 只用于 mac → HID。
const MAC_ONLY: &[(MacKeyCode, KeyCode)] = &[
    (0x34, k(0x58)), // 旧 PowerBook 的 Enter（kVK 0x34）→ KP Enter
];

/// NX 键类型（IOKit `ev_keymap.h` 的 `NX_KEYTYPE_*`）。
pub type NxKey = i64;

pub const NX_KEYTYPE_SOUND_UP: NxKey = 0;
pub const NX_KEYTYPE_SOUND_DOWN: NxKey = 1;
pub const NX_KEYTYPE_MUTE: NxKey = 7;
pub const NX_KEYTYPE_PLAY: NxKey = 16;
pub const NX_KEYTYPE_NEXT: NxKey = 17;
pub const NX_KEYTYPE_PREVIOUS: NxKey = 18;
pub const NX_KEYTYPE_FAST: NxKey = 19;
pub const NX_KEYTYPE_REWIND: NxKey = 20;

/// 系统定义事件里**发往正在操作的那台电脑**的键（用户规则）。表里没有的（键盘背光
/// `NX_KEYTYPE_ILLUMINATION_*`、电源、弹出、Caps Lock 等）留在本机、原样放行。一一对应，双向使用。
const NX_KEYS: &[(NxKey, KeyCode)] = &[
    (NX_KEYTYPE_SOUND_UP, c(0xE9)),   // Volume Increment
    (NX_KEYTYPE_SOUND_DOWN, c(0xEA)), // Volume Decrement
    (NX_KEYTYPE_MUTE, c(0xE2)),       // Mute
    (NX_KEYTYPE_PLAY, c(0xCD)),       // Play/Pause
    (NX_KEYTYPE_NEXT, c(0xB5)),       // Scan Next Track
    (NX_KEYTYPE_PREVIOUS, c(0xB6)),   // Scan Previous Track
    (NX_KEYTYPE_FAST, c(0xB3)),       // Fast Forward（Apple 键盘的「下一曲」键，短按下一曲、长按快进）
    (NX_KEYTYPE_REWIND, c(0xB4)),     // Rewind
];

/// 只用于 HID → NX：键盘页的音量键（`kVK_VolumeUp` 等普通按键系统不认，注入时改发系统定义事件）。
const NX_HID_ONLY: &[(KeyCode, NxKey)] = &[
    (k(0x7F), NX_KEYTYPE_MUTE),
    (k(0x80), NX_KEYTYPE_SOUND_UP),
    (k(0x81), NX_KEYTYPE_SOUND_DOWN),
];

/// 系统定义事件的键类型 → HID。不在转发之列的键为 `None`。
pub fn nx_to_hid(nx: NxKey) -> Option<KeyCode> {
    NX_KEYS.iter().find(|(n, _)| *n == nx).map(|(_, h)| *h)
}

/// HID → 注入用的系统定义事件键类型（音量、播放控制）。
pub fn hid_to_nx(code: KeyCode) -> Option<NxKey> {
    NX_KEYS
        .iter()
        .find(|(_, h)| *h == code)
        .map(|(n, _)| *n)
        .or_else(|| NX_HID_ONLY.iter().find(|(h, _)| *h == code).map(|(_, n)| *n))
}

/// 系统定义事件的 `data1`（`NX_SUBTYPE_AUX_CONTROL_BUTTONS`）：`键类型 << 16 | 状态 << 8 | 重复`，
/// 状态 0xA = 按下、0xB = 抬起。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuxKey {
    pub key: NxKey,
    pub pressed: bool,
    pub repeat: bool,
}

/// `NX_SUBTYPE_AUX_CONTROL_BUTTONS`。
pub const NX_SUBTYPE_AUX_CONTROL_BUTTONS: i16 = 8;

impl AuxKey {
    /// 解析 `data1`；状态既不是按下也不是抬起时为 `None`。
    pub fn parse(data1: i64) -> Option<Self> {
        let key = (data1 >> 16) & 0xFFFF;
        let pressed = match (data1 >> 8) & 0xFF {
            0x0A => true,
            0x0B => false,
            _ => return None,
        };
        Some(Self {
            key,
            pressed,
            repeat: data1 & 1 != 0,
        })
    }

    pub fn data1(self) -> i64 {
        let state: i64 = if self.pressed { 0x0A } else { 0x0B };
        (self.key << 16) | (state << 8) | i64::from(self.repeat)
    }
}

/// HID Usage → mac 虚拟键码。没有对应键时为 `None`。
pub fn hid_to_mac(code: KeyCode) -> Option<MacKeyCode> {
    PAIRS.iter().chain(HID_ONLY).find(|(h, _)| *h == code).map(|(_, m)| *m)
}

/// mac 虚拟键码 → HID Usage。
pub fn mac_to_hid(code: MacKeyCode) -> Option<KeyCode> {
    PAIRS
        .iter()
        .find(|(_, m)| *m == code)
        .map(|(h, _)| *h)
        .or_else(|| MAC_ONLY.iter().find(|(m, _)| *m == code).map(|(_, h)| *h))
}

/// 修饰键（含 Caps Lock、Fn）在 mac 上以 `FlagsChanged` 事件出现。
pub fn is_mac_modifier(code: MacKeyCode) -> bool {
    matches!(code, 0x36..=0x3F)
}

/// 小键盘键：macOS 给这些键的事件带 `NumericPad` 标志（方向键也带）。
pub fn is_mac_keypad(code: MacKeyCode) -> bool {
    matches!(
        code,
        0x41 | 0x43 | 0x45 | 0x47 | 0x4B | 0x4C | 0x4E | 0x51..=0x59 | 0x5B | 0x5C | 0x5F | 0x7B..=0x7E
    )
}

/// 功能键与导航键：macOS 给这些键的事件带 `SecondaryFn` 标志（`CGEventCreateKeyboardEvent` 也会自动带上）。
///
/// 新款键盘的调度中心、启动台 / Apps、听写、聚焦搜索、勿扰键（0xA0、0x83、0xB0–0xB2）也带：系统的符号热键
/// 只认带 `SecondaryFn` 的这些键码。2026-09-29 本机 macOS 26.6 实测：注入 kVK 0xB1 / 0xB2 / 0xB0 不带这个标志
/// 没有反应，带上后分别打开聚焦搜索、切换勿扰、启动听写（用户报告「F4–F6 在远端不生效」的根因）。
pub fn is_mac_function(code: MacKeyCode) -> bool {
    matches!(
        code,
        0x40 | 0x4F
            | 0x50
            | 0x5A
            | 0x60..=0x65
            | 0x67
            | 0x69..=0x6B
            | 0x6D
            | 0x6F
            | 0x71..=0x7E
            | 0x83
            | 0xA0
            | 0xB0..=0xB2
    )
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn pairs_are_bijective() {
        let mut hids = HashSet::new();
        let mut macs = HashSet::new();
        for (h, m) in PAIRS {
            assert!(hids.insert(*h), "HID {:#x} 重复", h.0);
            assert!(macs.insert(*m), "kVK {m:#x} 重复");
            assert_eq!(hid_to_mac(*h), Some(*m));
            assert_eq!(mac_to_hid(*m), Some(*h));
        }
        for (h, _) in HID_ONLY {
            assert!(!hids.contains(h), "HID_ONLY 与主表重复：{:#x}", h.0);
        }
        for (m, _) in MAC_ONLY {
            assert!(!macs.contains(m), "MAC_ONLY 与主表重复：{m:#x}");
        }
    }

    #[test]
    fn covers_the_full_standard_keyboard() {
        // 字母、数字、符号、F1–F20、导航、小键盘、修饰键都能映射。
        let usages = (0x04..=0x31)
            .chain(0x33..=0x45)
            .chain(0x49..=0x67)
            .filter(|u| *u != 0x66) // Power
            .chain(0x68..=0x6F)
            .chain(0xE0..=0xE7);
        for u in usages {
            assert!(hid_to_mac(k(u)).is_some(), "HID {u:#x} 没有映射");
        }
        // mac 上所有常见键码都能映射回 HID。
        for m in (0x00..=0x32).chain(0x35..=0x3F) {
            assert!(mac_to_hid(m).is_some(), "kVK {m:#x} 没有映射");
        }
    }

    #[test]
    fn known_keys() {
        assert_eq!(hid_to_mac(k(0x04)), Some(0x00)); // A
        assert_eq!(hid_to_mac(KeyCode::LEFT_GUI), Some(0x37));
        assert_eq!(hid_to_mac(KeyCode::ESCAPE), Some(0x35));
        assert_eq!(mac_to_hid(0x7E), Some(k(0x52))); // ↑
        assert_eq!(mac_to_hid(0x33), Some(k(0x2A))); // Backspace
        assert_eq!(hid_to_mac(k(0x46)), Some(0x69)); // Print Screen → F13
        assert_eq!(mac_to_hid(0x69), Some(k(0x68))); // F13 → F13
        assert_eq!(hid_to_mac(k(0x66)), None); // Power
    }

    #[test]
    fn nx_keys_map_both_ways() {
        let mut nx = HashSet::new();
        for (n, h) in NX_KEYS {
            assert!(nx.insert(*n), "NX {n} 重复");
            assert_eq!(nx_to_hid(*n), Some(*h));
            assert_eq!(hid_to_nx(*h), Some(*n));
            assert_eq!(h.page(), KeyCode::PAGE_CONSUMER);
            // 系统定义事件的键不是普通 kVK：注入不能走 keyDown（系统不认）。
            assert_eq!(hid_to_mac(*h), None, "Consumer {:#x}", h.usage());
        }
        // 留在本机的：键盘背光（21–23）、Caps Lock（4）、电源（6）、弹出（14）。
        for n in [2, 3, 4, 6, 14, 21, 22, 23] {
            assert_eq!(nx_to_hid(n), None);
        }
        // 键盘页的音量键注入时也走系统定义事件。
        assert_eq!(hid_to_nx(k(0x80)), Some(NX_KEYTYPE_SOUND_UP));
        assert_eq!(hid_to_nx(k(0x7F)), Some(NX_KEYTYPE_MUTE));
        assert_eq!(hid_to_nx(k(0x04)), None);
    }

    #[test]
    fn aux_data1_round_trip() {
        // macOS 实测：音量 + 按下 0xa00、重复 0xa01、抬起 0xb00；音量 − 按下 0x10a00。
        assert_eq!(
            AuxKey::parse(0xa00),
            Some(AuxKey {
                key: 0,
                pressed: true,
                repeat: false
            })
        );
        assert_eq!(
            AuxKey::parse(0xa01),
            Some(AuxKey {
                key: 0,
                pressed: true,
                repeat: true
            })
        );
        assert_eq!(AuxKey::parse(0x10b00).map(|a| (a.key, a.pressed)), Some((1, false)));
        assert_eq!(AuxKey::parse(0x150c00), None);
        for d in [0xa00, 0xa01, 0xb00, 0x10a00, 0x140b00] {
            assert_eq!(AuxKey::parse(d).unwrap().data1(), d);
        }
    }

    #[test]
    fn new_mac_function_keys() {
        assert_eq!(mac_to_hid(0xA0), Some(c(0x029F)));
        assert_eq!(hid_to_mac(c(0x029F)), Some(0xA0));
        assert_eq!(mac_to_hid(0xB1), Some(c(0x0221)));
        assert_eq!(mac_to_hid(0x83), Some(c(0x02A2)));
        assert_eq!(mac_to_hid(0xB0), Some(c(0x00CF)));
        assert_eq!(mac_to_hid(0xB2), Some(KeyCode::new(0x01, 0x9B)));
    }

    #[test]
    fn modifier_and_flag_classes() {
        for h in 0xE0..=0xE7 {
            assert!(is_mac_modifier(hid_to_mac(k(h)).unwrap()));
        }
        assert!(is_mac_modifier(0x39)); // Caps Lock
        assert!(!is_mac_modifier(0x00));
        assert!(is_mac_keypad(0x52) && is_mac_keypad(0x7B));
        assert!(!is_mac_keypad(0x12));
        assert!(is_mac_function(0x7A) && is_mac_function(0x73));
        // 新款键盘的系统功能键（调度中心、启动台、听写、聚焦搜索、勿扰）。
        for m in [0xA0, 0x83, 0xB0, 0xB1, 0xB2] {
            assert!(is_mac_function(m), "{m:#x}");
        }
        assert!(!is_mac_function(0x00));
    }
}
