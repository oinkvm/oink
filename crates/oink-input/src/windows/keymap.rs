//! HID Usage ↔ Windows 扫描码（Set 1）映射。
//!
//! 数据来源：Chromium `ui/events/keycodes/dom/dom_code_data.inc` 的 USB HID 列与 Windows 列。
//! 扫描码表示为 `u16`：扩展键（E0 前缀）为 `0xE0xx`，否则为 `0x00xx`，与低级键盘钩子里
//! `scanCode | (LLKHF_EXTENDED ? 0xE000 : 0)` 的组合方式一致。
//!
//! 特殊键：
//! - Pause：键盘实际发送 `E1 1D 45`，钩子里是 `scanCode = 0x45`、无扩展标志；NumLock 是 `0x45` 带扩展标志。
//!   注入 Pause 时用虚拟键 `VK_PAUSE`（扫描码注入无法表达 E1 前缀）。
//! - Ctrl+Pause（Break）在钩子里是 `0xE046`，Alt+PrintScreen（SysRq）是 `0x54`：只在反查时映射到 Pause / PrintScreen。
//! - 媒体键（Consumer 页）注入时用虚拟键（`VK_MEDIA_*` 等，带扩展标志），不依赖键盘布局的 E0 扫描码表。

use oink_foundation::input::KeyCode;

/// 一条映射。`vk != 0` 时注入用虚拟键而不是扫描码。
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(windows), allow(dead_code))] // Other OS editors read only HID support.
pub(crate) struct KeyEntry {
    pub hid: u32,
    pub scancode: u16,
    pub vk: u16,
}

const fn k(usage: u16, scancode: u16) -> KeyEntry {
    KeyEntry {
        hid: ((KeyCode::PAGE_KEYBOARD as u32) << 16) | usage as u32,
        scancode,
        vk: 0,
    }
}
const fn c(usage: u16, scancode: u16, vk: u16) -> KeyEntry {
    KeyEntry {
        hid: ((KeyCode::PAGE_CONSUMER as u32) << 16) | usage as u32,
        scancode,
        vk,
    }
}
const fn gd(usage: u16, scancode: u16, vk: u16) -> KeyEntry {
    KeyEntry {
        hid: (0x01 << 16) | usage as u32,
        scancode,
        vk,
    }
}

/// Conventional extended laptop brightness scan codes, when delivered to
/// WH_KEYBOARD_LL. HID/ACPI/firmware-only controls never enter that hook.
/// Ordinary F-keys and non-extended keypad codes are intentionally excluded.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn local_brightness_key(scancode: u16) -> Option<KeyCode> {
    Some(KeyCode::new(
        KeyCode::PAGE_CONSUMER,
        match scancode {
            0xE054 => 0x6F,
            0xE04C => 0x70,
            _ => return None,
        },
    ))
}

const VK_PAUSE: u16 = 0x13;

/// 主表。同一个扫描码出现多次时，**靠前的条目**用于「扫描码 → HID」反查。
#[rustfmt::skip]
pub(crate) const TABLE: &[KeyEntry] = &[
    // 字母
    k(0x04, 0x1E), k(0x05, 0x30), k(0x06, 0x2E), k(0x07, 0x20), k(0x08, 0x12), k(0x09, 0x21),
    k(0x0A, 0x22), k(0x0B, 0x23), k(0x0C, 0x17), k(0x0D, 0x24), k(0x0E, 0x25), k(0x0F, 0x26),
    k(0x10, 0x32), k(0x11, 0x31), k(0x12, 0x18), k(0x13, 0x19), k(0x14, 0x10), k(0x15, 0x13),
    k(0x16, 0x1F), k(0x17, 0x14), k(0x18, 0x16), k(0x19, 0x2F), k(0x1A, 0x11), k(0x1B, 0x2D),
    k(0x1C, 0x15), k(0x1D, 0x2C),
    // 数字 1..0
    k(0x1E, 0x02), k(0x1F, 0x03), k(0x20, 0x04), k(0x21, 0x05), k(0x22, 0x06), k(0x23, 0x07),
    k(0x24, 0x08), k(0x25, 0x09), k(0x26, 0x0A), k(0x27, 0x0B),
    // Enter Esc Backspace Tab Space - = [ ] \
    k(0x28, 0x1C), k(0x29, 0x01), k(0x2A, 0x0E), k(0x2B, 0x0F), k(0x2C, 0x39), k(0x2D, 0x0C),
    k(0x2E, 0x0D), k(0x2F, 0x1A), k(0x30, 0x1B), k(0x31, 0x2B),
    // ; ' ` , . / CapsLock
    k(0x33, 0x27), k(0x34, 0x28), k(0x35, 0x29), k(0x36, 0x33), k(0x37, 0x34), k(0x38, 0x35),
    k(0x39, 0x3A),
    // F1..F12
    k(0x3A, 0x3B), k(0x3B, 0x3C), k(0x3C, 0x3D), k(0x3D, 0x3E), k(0x3E, 0x3F), k(0x3F, 0x40),
    k(0x40, 0x41), k(0x41, 0x42), k(0x42, 0x43), k(0x43, 0x44), k(0x44, 0x57), k(0x45, 0x58),
    // PrintScreen ScrollLock Pause Insert Home PageUp Delete End PageDown → ← ↓ ↑
    k(0x46, 0xE037), k(0x47, 0x46),
    KeyEntry { hid: (0x07 << 16) | 0x48, scancode: 0x45, vk: VK_PAUSE },
    k(0x49, 0xE052), k(0x4A, 0xE047), k(0x4B, 0xE049), k(0x4C, 0xE053), k(0x4D, 0xE04F),
    k(0x4E, 0xE051), k(0x4F, 0xE04D), k(0x50, 0xE04B), k(0x51, 0xE050), k(0x52, 0xE048),
    // 小键盘
    k(0x53, 0xE045), k(0x54, 0xE035), k(0x55, 0x37), k(0x56, 0x4A), k(0x57, 0x4E), k(0x58, 0xE01C),
    k(0x59, 0x4F), k(0x5A, 0x50), k(0x5B, 0x51), k(0x5C, 0x4B), k(0x5D, 0x4C), k(0x5E, 0x4D),
    k(0x5F, 0x47), k(0x60, 0x48), k(0x61, 0x49), k(0x62, 0x52), k(0x63, 0x53),
    // IntlBackslash ContextMenu Power NumpadEqual
    k(0x64, 0x56), k(0x65, 0xE05D), k(0x66, 0xE05E), k(0x67, 0x59),
    // F13..F24
    k(0x68, 0x64), k(0x69, 0x65), k(0x6A, 0x66), k(0x6B, 0x67), k(0x6C, 0x68), k(0x6D, 0x69),
    k(0x6E, 0x6A), k(0x6F, 0x6B), k(0x70, 0x6C), k(0x71, 0x6D), k(0x72, 0x6E), k(0x73, 0x76),
    // Help Undo Cut Copy Paste
    k(0x75, 0xE03B), k(0x7A, 0xE008), k(0x7B, 0xE017), k(0x7C, 0xE018), k(0x7D, 0xE00A),
    // 日文、韩文键：NumpadComma IntlRo KanaMode IntlYen Convert NonConvert Lang1..Lang4
    k(0x85, 0x7E), k(0x87, 0x73), k(0x88, 0x70), k(0x89, 0x7D), k(0x8A, 0x79), k(0x8B, 0x7B),
    k(0x90, 0x72), k(0x91, 0x71), k(0x92, 0x78), k(0x93, 0x77),
    // 修饰键
    k(0xE0, 0x1D), k(0xE1, 0x2A), k(0xE2, 0x38), k(0xE3, 0xE05B),
    k(0xE4, 0xE01D), k(0xE5, 0x36), k(0xE6, 0xE038), k(0xE7, 0xE05C),
    // Consumer 页（媒体、浏览器、启动）：注入用虚拟键
    c(0xB5, 0xE019, 0xB0), // 下一曲 VK_MEDIA_NEXT_TRACK
    c(0xB6, 0xE010, 0xB1), // 上一曲
    c(0xB7, 0xE024, 0xB2), // 停止
    c(0xCD, 0xE022, 0xB3), // 播放/暂停
    c(0xE2, 0xE020, 0xAD), // 静音
    c(0xE9, 0xE030, 0xAF), // 音量 +
    c(0xEA, 0xE02E, 0xAE), // 音量 -
    c(0x183, 0xE06D, 0xB5), // 媒体选择
    c(0x18A, 0xE06C, 0xB4), // 邮件
    c(0x192, 0xE021, 0xB7), // 计算器（LaunchApp2）
    c(0x194, 0xE06B, 0xB6), // 我的电脑（LaunchApp1）
    c(0x221, 0xE065, 0xAA), // 浏览器搜索
    c(0x223, 0xE032, 0xAC), // 浏览器主页
    c(0x224, 0xE06A, 0xA6), // 浏览器后退
    c(0x225, 0xE069, 0xA7), // 浏览器前进
    c(0x226, 0xE068, 0xA9), // 浏览器停止
    c(0x227, 0xE067, 0xA8), // 浏览器刷新
    c(0x22A, 0xE066, 0xAB), // 浏览器收藏
    // Generic Desktop 页：睡眠、唤醒（电源键见上面的 Keyboard 0x66）
    gd(0x82, 0xE05F, 0x5F), // VK_SLEEP
    gd(0x83, 0xE063, 0),
    // —— 以下为重复扫描码的次要条目：只用于 HID → 扫描码 ——
    k(0x32, 0x2B),   // IntlHash（非美式 #~）与 Backslash 同扫描码
    k(0x7F, 0xE020), // Keyboard 页的 Mute / VolumeUp / VolumeDown（反查时优先 Consumer 页）
    k(0x80, 0xE030),
    k(0x81, 0xE02E),
    k(0x94, 0x76),   // Lang5 与 F24 同扫描码
    gd(0x81, 0xE05E, 0), // System Power Down 与 Keyboard Power 同扫描码
    // Mac 的快进 / 快退键（Apple 键盘的上一曲 / 下一曲键发的是这两个）：Windows 没有对应的
    // 虚拟键，按下一曲 / 上一曲注入。
    c(0xB3, 0xE019, 0xB0),
    c(0xB4, 0xE010, 0xB1),
];

/// 只用于「扫描码 → HID」的额外别名。
#[cfg(any(windows, all(test, debug_assertions)))]
const ALIASES: &[(u16, u32)] = &[
    (0xE046, (0x07 << 16) | 0x48), // Ctrl+Pause（Break）→ Pause
    (0x54, (0x07 << 16) | 0x46),   // Alt+PrintScreen（SysRq）→ PrintScreen
];

#[cfg(any(windows, all(test, debug_assertions)))]
fn index(scancode: u16) -> Option<usize> {
    match scancode >> 8 {
        0x00 => Some(scancode as usize & 0xFF),
        0xE0 => Some(0x100 | (scancode as usize & 0xFF)),
        _ => None,
    }
}

#[cfg(any(windows, all(test, debug_assertions)))]
fn reverse_table() -> &'static [u32; 512] {
    static T: std::sync::OnceLock<Box<[u32; 512]>> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut t = Box::new([0u32; 512]);
        for e in TABLE {
            if let Some(i) = index(e.scancode)
                && t[i] == 0
            {
                t[i] = e.hid;
            }
        }
        for &(sc, hid) in ALIASES {
            if let Some(i) = index(sc)
                && t[i] == 0
            {
                t[i] = hid;
            }
        }
        t
    })
}

/// 扫描码（`0xE0xx` 表示扩展键）→ HID。
#[cfg(any(windows, all(test, debug_assertions)))]
pub(crate) fn scancode_to_hid(scancode: u16) -> Option<KeyCode> {
    let hid = reverse_table()[index(scancode)?];
    (hid != 0).then_some(KeyCode(hid))
}

/// HID → 注入用的条目。
pub(crate) fn lookup_hid(code: KeyCode) -> Option<&'static KeyEntry> {
    TABLE.iter().find(|e| e.hid == code.0)
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn hid_codes_are_unique() {
        let mut seen = HashSet::new();
        for e in TABLE {
            assert!(seen.insert(e.hid), "HID 0x{:08X} 重复", e.hid);
        }
    }

    #[test]
    fn primary_entries_roundtrip() {
        // 除了次要条目，每个 HID → 扫描码 → HID 都能回到自己
        let secondary: HashSet<u32> = [
            (0x07 << 16) | 0x32,
            (0x07 << 16) | 0x7F,
            (0x07 << 16) | 0x80,
            (0x07 << 16) | 0x81,
            (0x07 << 16) | 0x94,
            (0x01 << 16) | 0x81,
            (0x0C << 16) | 0xB3,
            (0x0C << 16) | 0xB4,
        ]
        .into_iter()
        .collect();
        let mut n = 0;
        for e in TABLE.iter().filter(|e| !secondary.contains(&e.hid)) {
            assert_eq!(
                scancode_to_hid(e.scancode),
                Some(KeyCode(e.hid)),
                "扫描码 0x{:04X} 反查错误",
                e.scancode
            );
            assert_eq!(lookup_hid(KeyCode(e.hid)).unwrap().scancode, e.scancode);
            n += 1;
        }
        assert!(n > 140, "主表条目太少：{n}");
    }

    #[test]
    fn well_known_keys() {
        assert_eq!(lookup_hid(KeyCode::keyboard(0x04)).unwrap().scancode, 0x1E); // A
        assert_eq!(lookup_hid(KeyCode::LEFT_CTRL).unwrap().scancode, 0x1D);
        assert_eq!(lookup_hid(KeyCode::RIGHT_CTRL).unwrap().scancode, 0xE01D);
        assert_eq!(lookup_hid(KeyCode::RIGHT_ALT).unwrap().scancode, 0xE038);
        assert_eq!(lookup_hid(KeyCode::LEFT_GUI).unwrap().scancode, 0xE05B);
        assert_eq!(lookup_hid(KeyCode::CAPS_LOCK).unwrap().scancode, 0x3A);
        assert_eq!(lookup_hid(KeyCode::ESCAPE).unwrap().scancode, 0x01);
        // 方向键、小键盘回车与主键盘回车
        assert_eq!(scancode_to_hid(0xE048), Some(KeyCode::keyboard(0x52)));
        assert_eq!(scancode_to_hid(0x48), Some(KeyCode::keyboard(0x60)));
        assert_eq!(scancode_to_hid(0xE01C), Some(KeyCode::keyboard(0x58)));
        assert_eq!(scancode_to_hid(0x1C), Some(KeyCode::keyboard(0x28)));
        // NumLock 与 Pause 共用 0x45，靠扩展标志区分
        assert_eq!(scancode_to_hid(0xE045), Some(KeyCode::keyboard(0x53)));
        assert_eq!(scancode_to_hid(0x45), Some(KeyCode::keyboard(0x48)));
        assert_eq!(lookup_hid(KeyCode::keyboard(0x48)).unwrap().vk, VK_PAUSE);
    }

    #[test]
    fn duplicates_prefer_primary() {
        assert_eq!(scancode_to_hid(0x2B), Some(KeyCode::keyboard(0x31))); // Backslash
        assert_eq!(
            scancode_to_hid(0xE020),
            Some(KeyCode::new(KeyCode::PAGE_CONSUMER, 0xE2))
        ); // Mute
        assert_eq!(scancode_to_hid(0x76), Some(KeyCode::keyboard(0x73))); // F24
        assert_eq!(scancode_to_hid(0xE05E), Some(KeyCode::keyboard(0x66))); // Power
        // 次要条目仍可注入
        assert_eq!(lookup_hid(KeyCode::keyboard(0x32)).unwrap().scancode, 0x2B);
        assert_eq!(lookup_hid(KeyCode::keyboard(0x7F)).unwrap().scancode, 0xE020);
    }

    #[test]
    fn aliases_and_unknown() {
        assert_eq!(scancode_to_hid(0xE046), Some(KeyCode::keyboard(0x48)));
        assert_eq!(scancode_to_hid(0x54), Some(KeyCode::keyboard(0x46)));
        assert_eq!(scancode_to_hid(0xE02A), None); // 假 Shift
        assert_eq!(scancode_to_hid(0x00), None);
        assert_eq!(scancode_to_hid(0xE11D), None);
        assert!(lookup_hid(KeyCode::keyboard(0x01)).is_none());
    }

    #[test]
    fn consumer_keys_inject_with_vk() {
        for e in TABLE.iter().filter(|e| (e.hid >> 16) as u16 == KeyCode::PAGE_CONSUMER) {
            assert_ne!(e.vk, 0, "Consumer 0x{:X} 缺少虚拟键", e.hid & 0xFFFF);
            assert_eq!(e.scancode >> 8, 0xE0);
        }
    }
}

#[cfg(test)]
mod brightness_tests {
    use super::*;
    #[test]
    fn guard_brightness_codes_exclude_function_and_keypad_keys() {
        for sc in [0xE054, 0xE04C] {
            assert!(local_brightness_key(sc).unwrap().is_brightness());
        }
        for sc in [0x54, 0x4C, 0x3B, 0x3C, 0xE030, 0xE02E, 0xE05E, 0] {
            assert_eq!(local_brightness_key(sc), None);
        }
    }
}
