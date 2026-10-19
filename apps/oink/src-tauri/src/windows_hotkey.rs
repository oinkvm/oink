//! RegisterHotKey uses virtual-key names rather than the router's HID usages.
pub fn chord(keys: &[String]) -> Option<(u32, u32)> {
    let mut mods = 0;
    let mut key = None;
    for name in keys {
        mods |= match name.as_str() {
            "Alt" => 1,
            "Ctrl" => 2,
            "Shift" => 4,
            "Meta" => 8,
            name => {
                if key.is_some() {
                    return None;
                }
                key = virtual_key(name);
                key?;
                0
            }
        };
    }
    Some((mods, key?))
}
fn virtual_key(name: &str) -> Option<u32> {
    let bytes = name.as_bytes();
    if bytes.len() == 1 && (bytes[0].is_ascii_uppercase() || bytes[0].is_ascii_digit()) {
        return Some(u32::from(bytes[0]));
    }
    if let Some(n) = name
        .strip_prefix('F')
        .and_then(|n| n.parse::<u32>().ok())
        .filter(|n| (1..=24).contains(n))
    {
        return Some(0x70 + n - 1);
    }
    if let Some(n) = name
        .strip_prefix("Numpad")
        .and_then(|n| n.parse::<u32>().ok())
        .filter(|n| *n <= 9)
    {
        return Some(0x60 + n);
    }
    Some(match name {
        "Enter" | "NumpadEnter" => 0x0D,
        "Escape" => 0x1B,
        "Backspace" => 8,
        "Tab" => 9,
        "Space" => 0x20,
        "Minus" => 0xBD,
        "Equal" => 0xBB,
        "BracketLeft" => 0xDB,
        "BracketRight" => 0xDD,
        "Backslash" => 0xDC,
        "Semicolon" => 0xBA,
        "Quote" => 0xDE,
        "Backquote" => 0xC0,
        "Comma" => 0xBC,
        "Period" => 0xBE,
        "Slash" => 0xBF,
        "PrintScreen" => 0x2C,
        "ScrollLock" => 0x91,
        "Pause" => 0x13,
        "Insert" => 0x2D,
        "Home" => 0x24,
        "PageUp" => 0x21,
        "Delete" => 0x2E,
        "End" => 0x23,
        "PageDown" => 0x22,
        "ArrowRight" => 0x27,
        "ArrowLeft" => 0x25,
        "ArrowDown" => 0x28,
        "ArrowUp" => 0x26,
        "NumLock" => 0x90,
        "NumpadDivide" => 0x6F,
        "NumpadMultiply" => 0x6A,
        "NumpadSubtract" => 0x6D,
        "NumpadAdd" => 0x6B,
        "NumpadDecimal" => 0x6E,
        "IntlBackslash" => 0xE2,
        "ContextMenu" => 0x5D,
        "Help" => 0x2F,
        "KanaMode" => 0x15,
        "Convert" => 0x1C,
        "NonConvert" => 0x1D,
        _ => return None,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn parse(keys: &[&str]) -> Option<(u32, u32)> {
        chord(&keys.iter().map(|k| (*k).into()).collect::<Vec<_>>())
    }
    #[test]
    fn native_hotkey_preserves_modifiers_and_rejects_invalid_chords() {
        assert_eq!(parse(&["Ctrl", "Alt", "A"]), Some((3, 65)));
        assert_eq!(parse(&["Meta", "Shift", "F24"]), Some((12, 0x87)));
        assert_eq!(parse(&["Alt", "Numpad0"]), Some((1, 0x60)));
        for keys in [&[][..], &["Ctrl"], &["A", "B"], &["Hyper", "A"]] {
            assert_eq!(parse(keys), None);
        }
    }
}
