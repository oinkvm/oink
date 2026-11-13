//! 调节 › 键盘布局 (docs/control-page.md §3.1.2): the keyboard layout the focus
//! computer types with, shared by both phones. The emulated keyboard keeps its
//! US faces; a character key is sent as the key (plus Shift / AltGr) that types
//! that character on the chosen layout, so the computer types what the key shows
//! (and a shortcut such as Ctrl+Z hits the key that is Z there).
//!
//! Each table holds only the characters of the emulated keyboard whose key is
//! certain on that layout and system; the others go as text (always right, but
//! no key: no repeat, no shortcut) unless Ctrl / Alt / ⌘ is down. "auto" is the
//! US layout until the focus computer reports its own layout (it does not yet).
//! Korean computers type Latin letters with the US layout.
//!
//! Key codes are the core's: (HID usage page << 16) | usage, keyboard page 0x07.

use std::collections::HashMap;
use std::sync::LazyLock;

const KEYBOARD_PAGE: u32 = 0x07 << 16;

/// A keyboard-page usage as a key code.
pub const fn key(usage: u16) -> u32 {
    KEYBOARD_PAGE | usage as u32
}

/// Left Alt: Option on a Mac.
pub const LEFT_ALT: u32 = key(0xE2);
/// Right Alt: AltGr on Windows.
pub const RIGHT_ALT: u32 = key(0xE6);

/// One key press that types a character on the focus computer: the key,
/// whether Shift and AltGr (Option on a Mac) go down with it, and whether it is
/// a dead key that only types the character when Space follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stroke {
    pub code: u32,
    pub shift: bool,
    pub alt: bool,
    pub dead: bool,
}

/// The modifiers the user holds down (held or locked) on the control page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModifiersDown {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// ⌘ / Windows key.
    pub meta: bool,
}

/// What one character key of the emulated keyboard sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypedChar {
    /// `code` goes down (and up when the finger lifts). `auto_shift`: Shift goes
    /// down before it and up after it, around just this key. `auto_alt`: that
    /// key (AltGr on Windows, Option on a Mac) goes around it the same way,
    /// inside the Shift. `dead`: the key is tapped at once, followed by a Space
    /// tap, and is neither held nor repeated.
    Key {
        code: u32,
        auto_shift: bool,
        auto_alt: Option<u32>,
        dead: bool,
    },
    /// No certain key: sent as text (the system keyboard's committed text path).
    Text(String),
}

/// The key that types `ch` on `layout` ("auto", "us", "uk", "de", "fr", "es",
/// "jis", "ko"; anything else is US) of a Mac (`mac`) or Windows computer;
/// None when the layout has no certain key for it.
pub fn stroke(layout: &str, mac: bool, ch: char) -> Option<Stroke> {
    table(layout, mac).get(&ch).copied()
}

/// A character key of the emulated keyboard: `ch` as on the key without Shift.
/// It types the character its face shows (capital / shifted symbol while the
/// user's Shift is down) on the focus computer's layout: that key, with Shift
/// and AltGr (Option) around just this key when the character needs them and
/// the user does not hold them already; the user's own Shift is never released.
///
/// When the layout has no key for the character, or the character needs no
/// Shift there but the user's Shift is down, it goes as text — except that a
/// shortcut (Ctrl / Alt / ⌘ down) still sends a key, the nearest one: the
/// layout's key for the unshifted character, else the US one.
pub fn typed_char(layout: &str, mac: bool, ch: char, mods: ModifiersDown) -> TypedChar {
    let face = if !mods.shift {
        ch
    } else if ch.is_alphabetic() {
        single_upper(ch)
    } else {
        us_shift_label(ch).unwrap_or(ch)
    };
    let shortcut = mods.ctrl || mods.alt || mods.meta;
    let mut chosen = stroke(layout, mac, face).filter(|s| !mods.shift || s.shift);
    if chosen.is_none() && shortcut {
        chosen = stroke(layout, mac, ch).or_else(|| us_char(ch).map(|(code, shift)| Stroke { shift, ..k_code(code) }));
    }
    let Some(chosen) = chosen else {
        return TypedChar::Text(face.to_string());
    };
    // On a Mac the user's own Alt is Option already; Windows' AltGr is the right Alt.
    let auto_alt = (chosen.alt && !(mac && mods.alt)).then_some(if mac { LEFT_ALT } else { RIGHT_ALT });
    TypedChar::Key {
        code: chosen.code,
        auto_shift: chosen.shift && !mods.shift,
        auto_alt,
        dead: chosen.dead,
    }
}

/// The capital of a letter when it is one character (as Kotlin's `uppercaseChar`).
fn single_upper(ch: char) -> char {
    let mut upper = ch.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(c), None) => c,
        _ => ch,
    }
}

/// What a base key types with Shift down on a US layout (the symbol layer's
/// faces while Shift is down).
pub fn us_shift_label(ch: char) -> Option<char> {
    Some(match ch {
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        _ => return None,
    })
}

/// A character as a US-layout physical key: the key code and whether the
/// character needs Shift.
pub fn us_char(ch: char) -> Option<(u32, bool)> {
    let usage: u16 = match ch {
        'a'..='z' => return Some((key(0x04 + (ch as u16 - 'a' as u16)), false)),
        'A'..='Z' => return Some((key(0x04 + (ch as u16 - 'A' as u16)), true)),
        '1'..='9' => return Some((key(0x1E + (ch as u16 - '1' as u16)), false)),
        '0' => return Some((key(0x27), false)),
        ' ' => 0x2C,
        '-' => 0x2D,
        '=' => 0x2E,
        '[' => 0x2F,
        ']' => 0x30,
        '\\' => 0x31,
        ';' => 0x33,
        '\'' => 0x34,
        '`' => 0x35,
        ',' => 0x36,
        '.' => 0x37,
        '/' => 0x38,
        _ => {
            let shifted = match ch {
                '!' => 0x1E,
                '@' => 0x1F,
                '#' => 0x20,
                '$' => 0x21,
                '%' => 0x22,
                '^' => 0x23,
                '&' => 0x24,
                '*' => 0x25,
                '(' => 0x26,
                ')' => 0x27,
                '_' => 0x2D,
                '+' => 0x2E,
                '{' => 0x2F,
                '}' => 0x30,
                '|' => 0x31,
                ':' => 0x33,
                '"' => 0x34,
                '~' => 0x35,
                '<' => 0x36,
                '>' => 0x37,
                '?' => 0x38,
                _ => return None,
            };
            return Some((key(shifted), true));
        }
    };
    Some((key(usage), false))
}

type Table = HashMap<char, Stroke>;

fn table(layout: &str, mac: bool) -> &'static LazyLock<Table> {
    match (layout, mac) {
        ("uk", false) => &WIN_UK,
        ("uk", true) => &MAC_UK,
        ("de", false) => &WIN_DE,
        ("de", true) => &MAC_DE,
        ("fr", false) => &WIN_FR,
        ("fr", true) => &MAC_FR,
        ("es", false) => &WIN_ES,
        ("es", true) => &MAC_ES,
        ("jis", false) => &WIN_JIS,
        ("jis", true) => &MAC_JIS,
        _ => &US,
    }
}

const fn k_code(code: u32) -> Stroke {
    Stroke {
        code,
        shift: false,
        alt: false,
        dead: false,
    }
}
const fn k(usage: u16) -> Stroke {
    k_code(key(usage))
}
const fn s(usage: u16) -> Stroke {
    Stroke {
        shift: true,
        ..k(usage)
    }
}
const fn a(usage: u16) -> Stroke {
    Stroke { alt: true, ..k(usage) }
}
const fn sa(usage: u16) -> Stroke {
    Stroke {
        shift: true,
        alt: true,
        ..k(usage)
    }
}
const fn dead(stroke: Stroke) -> Stroke {
    Stroke { dead: true, ..stroke }
}

/// Letters by their key: `swaps` moves letters (QWERTZ, AZERTY); capitals add
/// Shift. Digits with Shift when `digits_shifted` (AZERTY). Space is the same
/// everywhere.
fn base(digits_shifted: bool, swaps: &[(char, u16)]) -> Table {
    let mut m = Table::new();
    for c in 'a'..='z' {
        let usage = swaps
            .iter()
            .find(|(from, _)| *from == c)
            .map_or(0x04 + (c as u16 - 'a' as u16), |(_, to)| *to);
        m.insert(c, k(usage));
        m.insert(c.to_ascii_uppercase(), s(usage));
    }
    for d in 0..=9u16 {
        let ch = char::from(b'0' + d as u8);
        let usage = if d == 0 { 0x27 } else { 0x1E + d - 1 };
        m.insert(ch, if digits_shifted { s(usage) } else { k(usage) });
    }
    m.insert(' ', k(0x2C));
    m
}

fn with(mut table: Table, keys: &[(char, Stroke)]) -> Table {
    table.extend(keys.iter().copied());
    table
}

static US: LazyLock<Table> = LazyLock::new(|| {
    (32u8..=126)
        .map(char::from)
        .filter_map(|c| us_char(c).map(|(code, shift)| (c, Stroke { shift, ..k_code(code) })))
        .collect()
});

// Windows "United Kingdom": " on 2, @ on ', # ~ on the ISO key by Return, \ | on the ISO key by Z.
#[rustfmt::skip]
const WIN_UK_KEYS: &[(char, Stroke)] = &[
    ('!', s(0x1E)), ('"', s(0x1F)), ('$', s(0x21)), ('%', s(0x22)), ('^', s(0x23)), ('&', s(0x24)), ('*', s(0x25)),
    ('(', s(0x26)), (')', s(0x27)), ('-', k(0x2D)), ('_', s(0x2D)), ('=', k(0x2E)), ('+', s(0x2E)), ('[', k(0x2F)),
    ('{', s(0x2F)), (']', k(0x30)), ('}', s(0x30)), (';', k(0x33)), (':', s(0x33)), ('\'', k(0x34)), ('@', s(0x34)),
    ('#', k(0x32)), ('~', s(0x32)), ('\\', k(0x64)), ('|', s(0x64)), ('`', k(0x35)), (',', k(0x36)), ('<', s(0x36)),
    ('.', k(0x37)), ('>', s(0x37)), ('/', k(0x38)), ('?', s(0x38)),
];
static WIN_UK: LazyLock<Table> = LazyLock::new(|| with(base(false, &[]), WIN_UK_KEYS));

// macOS "British": the US keys, except Shift+3 = £ and # = Option+3.
static MAC_UK: LazyLock<Table> = LazyLock::new(|| with((*US).clone(), &[('#', a(0x20))]));

const QWERTZ: &[(char, u16)] = &[('y', 0x1D), ('z', 0x1C)];
const AZERTY: &[(char, u16)] = &[('a', 0x14), ('q', 0x04), ('z', 0x1A), ('w', 0x1D), ('m', 0x33)];

// Windows "German": QWERTZ, AltGr for @ [ ] { } \ | ~, ^ dead on the key left of 1.
#[rustfmt::skip]
const WIN_DE_KEYS: &[(char, Stroke)] = &[
    ('!', s(0x1E)), ('"', s(0x1F)), ('$', s(0x21)), ('%', s(0x22)), ('&', s(0x23)), ('/', s(0x24)), ('(', s(0x25)),
    (')', s(0x26)), ('=', s(0x27)), ('?', s(0x2D)), ('\\', a(0x2D)), ('+', k(0x30)), ('*', s(0x30)), ('~', a(0x30)),
    ('#', k(0x32)), ('\'', s(0x32)), ('<', k(0x64)), ('>', s(0x64)), ('|', a(0x64)), (',', k(0x36)), (';', s(0x36)),
    ('.', k(0x37)), (':', s(0x37)), ('-', k(0x38)), ('_', s(0x38)), ('@', a(0x14)), ('{', a(0x24)), ('[', a(0x25)),
    (']', a(0x26)), ('}', a(0x27)), ('^', dead(k(0x35))),
];
static WIN_DE: LazyLock<Table> = LazyLock::new(|| with(base(false, QWERTZ), WIN_DE_KEYS));

// macOS "German": QWERTZ, Option for @ [ ] | { } and Option+Shift+7 = \, ~ = dead Option+N. On a Mac
// the HID key left of 1 (0x35) is kVK_ANSI_Grave = < > there, and 0x64 (kVK_ISO_Section) is the dead ^.
#[rustfmt::skip]
const MAC_DE_KEYS: &[(char, Stroke)] = &[
    ('!', s(0x1E)), ('"', s(0x1F)), ('$', s(0x21)), ('%', s(0x22)), ('&', s(0x23)), ('/', s(0x24)), ('(', s(0x25)),
    (')', s(0x26)), ('=', s(0x27)), ('?', s(0x2D)), ('+', k(0x30)), ('*', s(0x30)), ('#', k(0x32)), ('\'', s(0x32)),
    ('<', k(0x35)), ('>', s(0x35)), (',', k(0x36)), (';', s(0x36)), ('.', k(0x37)), (':', s(0x37)), ('-', k(0x38)),
    ('_', s(0x38)), ('@', a(0x0F)), ('[', a(0x22)), (']', a(0x23)), ('|', a(0x24)), ('\\', sa(0x24)), ('{', a(0x25)),
    ('}', a(0x26)), ('~', dead(a(0x11))), ('^', dead(k(0x64))),
];
static MAC_DE: LazyLock<Table> = LazyLock::new(|| with(base(false, QWERTZ), MAC_DE_KEYS));

// Windows "French": AZERTY, digits with Shift, AltGr on the number row for # { [ | \ ^ @ ] }.
#[rustfmt::skip]
const WIN_FR_KEYS: &[(char, Stroke)] = &[
    ('&', k(0x1E)), ('"', k(0x20)), ('\'', k(0x21)), ('(', k(0x22)), ('-', k(0x23)), ('_', k(0x25)),
    (')', k(0x2D)), ('=', k(0x2E)), ('+', s(0x2E)), ('$', k(0x30)), ('*', k(0x32)), ('%', s(0x34)), ('<', k(0x64)),
    ('>', s(0x64)), (',', k(0x10)), ('?', s(0x10)), (';', k(0x36)), ('.', s(0x36)), (':', k(0x37)), ('/', s(0x37)),
    ('!', k(0x38)), ('#', a(0x20)), ('{', a(0x21)), ('[', a(0x22)), ('|', a(0x23)), ('\\', a(0x25)), ('^', a(0x26)),
    ('@', a(0x27)), (']', a(0x2D)), ('}', a(0x2E)), ('~', dead(a(0x1F))),
];
static WIN_FR: LazyLock<Table> = LazyLock::new(|| with(base(true, AZERTY), WIN_FR_KEYS));

// macOS "French": AZERTY, digits with Shift, @ # on kVK_ISO_Section (HID 0x64), < > on kVK_ANSI_Grave
// (HID 0x35). The Option characters ([ ] { } | \ ~) go as text.
#[rustfmt::skip]
const MAC_FR_KEYS: &[(char, Stroke)] = &[
    ('@', k(0x64)), ('#', s(0x64)), ('&', k(0x1E)), ('"', k(0x20)), ('\'', k(0x21)), ('(', k(0x22)),
    ('!', k(0x25)), (')', k(0x2D)), ('-', k(0x2E)), ('_', s(0x2E)), ('$', k(0x30)), ('*', s(0x30)), ('%', s(0x34)),
    ('<', k(0x35)), ('>', s(0x35)), (',', k(0x10)), ('?', s(0x10)), (';', k(0x36)), ('.', s(0x36)), (':', k(0x37)),
    ('/', s(0x37)), ('=', k(0x38)), ('+', s(0x38)), ('^', dead(k(0x2F))),
];
static MAC_FR: LazyLock<Table> = LazyLock::new(|| with(base(true, AZERTY), MAC_FR_KEYS));

// Windows "Spanish": AltGr for \ | @ # [ ] { }; ^ dead with Shift. ~ (AltGr+4) goes as text.
#[rustfmt::skip]
const WIN_ES_KEYS: &[(char, Stroke)] = &[
    ('!', s(0x1E)), ('"', s(0x1F)), ('$', s(0x21)), ('%', s(0x22)), ('&', s(0x23)), ('/', s(0x24)), ('(', s(0x25)),
    (')', s(0x26)), ('=', s(0x27)), ('\'', k(0x2D)), ('?', s(0x2D)), ('+', k(0x30)), ('*', s(0x30)), ('<', k(0x64)),
    ('>', s(0x64)), (',', k(0x36)), (';', s(0x36)), ('.', k(0x37)), (':', s(0x37)), ('-', k(0x38)), ('_', s(0x38)),
    ('\\', a(0x35)), ('|', a(0x1E)), ('@', a(0x1F)), ('#', a(0x20)), ('[', a(0x2F)), (']', a(0x30)), ('{', a(0x34)),
    ('}', a(0x32)), ('^', dead(s(0x2F))),
];
static WIN_ES: LazyLock<Table> = LazyLock::new(|| with(base(false, &[]), WIN_ES_KEYS));

// macOS "Spanish - ISO": as Windows without AltGr; < > on kVK_ANSI_Grave (HID 0x35). The Option
// characters (\ | @ # [ ] { } ~) go as text.
#[rustfmt::skip]
const MAC_ES_KEYS: &[(char, Stroke)] = &[
    ('!', s(0x1E)), ('"', s(0x1F)), ('$', s(0x21)), ('%', s(0x22)), ('&', s(0x23)), ('/', s(0x24)), ('(', s(0x25)),
    (')', s(0x26)), ('=', s(0x27)), ('\'', k(0x2D)), ('?', s(0x2D)), ('+', k(0x30)), ('*', s(0x30)), ('<', k(0x35)),
    ('>', s(0x35)), (',', k(0x36)), (';', s(0x36)), ('.', k(0x37)), (':', s(0x37)), ('-', k(0x38)), ('_', s(0x38)),
    ('^', dead(s(0x2F))),
];
static MAC_ES: LazyLock<Table> = LazyLock::new(|| with(base(false, &[]), MAC_ES_KEYS));

// Japanese (JIS): Shift on the number row gives ! " # $ % & ' ( ), the keys right of P and L are
// @ [ and ; : ], ¥ (HID 0x89) is \ | and ろ (HID 0x87) is \ _ on Windows.
#[rustfmt::skip]
const JIS_COMMON_KEYS: &[(char, Stroke)] = &[
    ('!', s(0x1E)), ('"', s(0x1F)), ('#', s(0x20)), ('$', s(0x21)), ('%', s(0x22)), ('&', s(0x23)),
    ('\'', s(0x24)), ('(', s(0x25)), (')', s(0x26)), ('-', k(0x2D)), ('=', s(0x2D)), ('^', k(0x2E)), ('~', s(0x2E)),
    ('|', s(0x89)), ('@', k(0x2F)), ('`', s(0x2F)), ('[', k(0x30)), ('{', s(0x30)), (';', k(0x33)), ('+', s(0x33)),
    (':', k(0x34)), ('*', s(0x34)), (']', k(0x32)), ('}', s(0x32)), (',', k(0x36)), ('<', s(0x36)), ('.', k(0x37)),
    ('>', s(0x37)), ('/', k(0x38)), ('?', s(0x38)),
];
static WIN_JIS: LazyLock<Table> = LazyLock::new(|| {
    with(
        with(base(false, &[]), JIS_COMMON_KEYS),
        &[('\\', k(0x89)), ('_', s(0x87))],
    )
});
// On a Mac ¥ types ¥ (\ is Option+¥) and ろ's characters depend on the input source: \ and _ go as text.
static MAC_JIS: LazyLock<Table> = LazyLock::new(|| with(base(false, &[]), JIS_COMMON_KEYS));

#[cfg(test)]
mod tests {
    use super::*;

    // ---- the tables (Android's KeyLayoutsTest, user 2026-11-12) ----

    const LAYOUTS: [&str; 8] = ["auto", "us", "uk", "de", "fr", "es", "jis", "ko"];

    #[test]
    fn every_layout_types_letters_digits_and_space_with_keys() {
        for layout in LAYOUTS {
            for mac in [false, true] {
                for c in 'a'..='z' {
                    let lower = stroke(layout, mac, c);
                    let upper = stroke(layout, mac, c.to_ascii_uppercase());
                    let lower = lower.unwrap_or_else(|| panic!("{layout} {mac} {c}"));
                    assert_eq!(Some(Stroke { shift: true, ..lower }), upper, "{layout} {mac} {c}");
                }
                for c in '0'..='9' {
                    assert!(stroke(layout, mac, c).is_some(), "{layout} {mac} {c}");
                }
                assert_eq!(Some(k(0x2C)), stroke(layout, mac, ' '), "{layout} {mac}");
            }
        }
    }

    #[test]
    fn us_keys_are_the_emulated_keyboards_own() {
        for c in "abc123-=[];',./!@#$%^&*()_+{}:\"<>?|\\~".chars() {
            let (code, shift) = us_char(c).unwrap();
            for layout in ["auto", "us", "ko"] {
                assert_eq!(
                    Some(Stroke { shift, ..k_code(code) }),
                    stroke(layout, false, c),
                    "{layout} {c}"
                );
            }
        }
    }

    #[test]
    fn letters_move_on_qwertz_and_azerty() {
        for mac in [false, true] {
            assert_eq!(Some(k(0x1C)), stroke("de", mac, 'z'));
            assert_eq!(Some(k(0x1D)), stroke("de", mac, 'y'));
            assert_eq!(Some(k(0x14)), stroke("fr", mac, 'a'));
            assert_eq!(Some(k(0x04)), stroke("fr", mac, 'q'));
            assert_eq!(Some(k(0x1A)), stroke("fr", mac, 'z'));
            assert_eq!(Some(k(0x1D)), stroke("fr", mac, 'w'));
            assert_eq!(Some(k(0x33)), stroke("fr", mac, 'm'));
            assert_eq!(Some(s(0x1E)), stroke("fr", mac, '1'));
        }
    }

    #[test]
    fn symbols_follow_the_layout_and_system() {
        assert_eq!(Some(s(0x1F)), stroke("uk", false, '"'));
        assert_eq!(Some(s(0x34)), stroke("uk", false, '@'));
        assert_eq!(Some(k(0x32)), stroke("uk", false, '#'));
        assert_eq!(Some(a(0x20)), stroke("uk", true, '#'));
        assert_eq!(Some(s(0x1F)), stroke("uk", true, '@'));
        assert_eq!(Some(k(0x64)), stroke("de", false, '<'));
        assert_eq!(Some(k(0x35)), stroke("de", true, '<'));
        assert_eq!(Some(s(0x24)), stroke("de", false, '/'));
        assert_eq!(Some(k(0x2F)), stroke("jis", false, '@'));
        assert_eq!(Some(s(0x87)), stroke("jis", false, '_'));
        // Characters whose key is not certain go as text.
        assert_eq!(None, stroke("fr", true, '['));
        assert_eq!(None, stroke("es", false, '~'));
        assert_eq!(None, stroke("jis", true, '\\'));
    }

    #[test]
    fn unknown_layout_is_us() {
        assert_eq!(stroke("us", false, 'z'), stroke("dvorak", false, 'z'));
        assert_eq!(stroke("us", true, '@'), stroke("", true, '@'));
    }

    // ---- what a character key sends (Android's KeyboardModelTest layout cases) ----

    const NONE: ModifiersDown = ModifiersDown {
        shift: false,
        ctrl: false,
        alt: false,
        meta: false,
    };
    const SHIFT: ModifiersDown = ModifiersDown { shift: true, ..NONE };
    const CTRL: ModifiersDown = ModifiersDown { ctrl: true, ..NONE };
    const ALT: ModifiersDown = ModifiersDown { alt: true, ..NONE };

    fn plain(usage: u16) -> TypedChar {
        TypedChar::Key {
            code: key(usage),
            auto_shift: false,
            auto_alt: None,
            dead: false,
        }
    }
    fn shifted(usage: u16) -> TypedChar {
        TypedChar::Key {
            code: key(usage),
            auto_shift: true,
            auto_alt: None,
            dead: false,
        }
    }
    fn alt_around(usage: u16, alt: u32) -> TypedChar {
        TypedChar::Key {
            code: key(usage),
            auto_shift: false,
            auto_alt: Some(alt),
            dead: false,
        }
    }
    fn text(t: &str) -> TypedChar {
        TypedChar::Text(t.into())
    }

    #[test]
    fn german_layout_types_what_the_keys_show() {
        // Z is where US has Y.
        assert_eq!(plain(0x1C), typed_char("de", false, 'z', NONE));
        assert_eq!(plain(0x38), typed_char("de", false, '-', NONE));
        // @ = AltGr+Q on Windows (right Alt around just this key) …
        assert_eq!(alt_around(0x14, RIGHT_ALT), typed_char("de", false, '@', NONE));
        // … and Option+L on a Mac.
        assert_eq!(alt_around(0x0F, LEFT_ALT), typed_char("de", true, '@', NONE));
    }

    #[test]
    fn the_users_alt_on_a_mac_is_the_option_already() {
        assert_eq!(plain(0x0F), typed_char("de", true, '@', ALT));
        // Windows' AltGr is the right Alt, not the user's left one.
        assert_eq!(alt_around(0x14, RIGHT_ALT), typed_char("de", false, '@', ALT));
    }

    #[test]
    fn dead_key_is_marked_dead() {
        assert_eq!(
            TypedChar::Key {
                code: key(0x35),
                auto_shift: false,
                auto_alt: None,
                dead: true
            },
            typed_char("de", false, '^', NONE)
        );
        assert_eq!(
            TypedChar::Key {
                code: key(0x2F),
                auto_shift: true,
                auto_alt: None,
                dead: true
            },
            typed_char("es", false, '^', NONE)
        );
    }

    #[test]
    fn french_digits_take_shift_and_users_shift_keeps_the_face() {
        assert_eq!(shifted(0x1E), typed_char("fr", false, '1', NONE));
        // Shift down + the 1 key shows "!": on AZERTY "!" is an unshifted key, so it goes as text.
        assert_eq!(text("!"), typed_char("fr", false, '1', SHIFT));
    }

    #[test]
    fn character_without_a_key_on_the_layout_goes_as_text_but_shortcuts_stay_keys() {
        assert_eq!(text("["), typed_char("fr", true, '[', NONE));
        // The US key, as before.
        assert_eq!(plain(0x2F), typed_char("fr", true, '[', CTRL));
        // A shortcut with Shift down takes the layout's key for the unshifted character: on AZERTY
        // the 1 key shows "!" (an unshifted key there), so Ctrl+Shift+1 hits the 1 key itself.
        assert_eq!(
            plain(0x1E),
            typed_char("fr", false, '1', ModifiersDown { shift: true, ..CTRL })
        );
    }

    #[test]
    fn auto_and_us_and_korean_keep_the_us_keys() {
        for layout in ["auto", "us", "ko"] {
            assert_eq!(plain(0x1D), typed_char(layout, false, 'z', NONE), "{layout}");
            assert_eq!(shifted(0x1F), typed_char(layout, false, '@', NONE), "{layout}");
        }
    }

    #[test]
    fn users_shift_types_capitals_and_shifted_symbols_without_auto_shift() {
        // Ctrl + Shift held, the t key: T on its own key, the user's Shift already down.
        assert_eq!(
            plain(0x17),
            typed_char("us", false, 't', ModifiersDown { shift: true, ..CTRL })
        );
        // The symbol layer's 1 shows ! while Shift is down: Shift + 1.
        assert_eq!(plain(0x1E), typed_char("us", false, '1', SHIFT));
        // A symbol that needs Shift anyway keeps its key, with no second Shift.
        assert_eq!(plain(0x1F), typed_char("us", false, '@', SHIFT));
        assert_eq!(shifted(0x1E), typed_char("us", false, '!', CTRL));
    }

    #[test]
    fn a_character_the_us_layout_has_no_key_for_goes_as_text() {
        assert_eq!(text("é"), typed_char("us", false, 'é', NONE));
        assert_eq!(text("É"), typed_char("us", false, 'é', SHIFT));
        assert_eq!(text("é"), typed_char("us", false, 'é', CTRL));
    }
}
