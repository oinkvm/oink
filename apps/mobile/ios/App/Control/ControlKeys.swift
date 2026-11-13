import Foundation

// Key rules of the control page keyboards (docs control-page.md §3.1). Pure state and
// tables, no UI: the views and the model call these and send what they return.

/// One key event for the focus computer. `code` is an oink `KeyCode`: `(page << 16) | usage`,
/// keyboard page 0x07, consumer page 0x0C (media keys).
struct KeyEvent: Equatable {
    var code: UInt32
    var down: Bool
}

enum HID {
    static func key(_ usage: UInt16) -> UInt32 { (0x07 << 16) | UInt32(usage) }
    static func consumer(_ usage: UInt16) -> UInt32 { (0x0C << 16) | UInt32(usage) }

    static let returnKey = key(0x28), escape = key(0x29), backspace = key(0x2A), tab = key(0x2B), space = key(0x2C)
    static let home = key(0x4A), pageUp = key(0x4B), forwardDelete = key(0x4C), end = key(0x4D), pageDown = key(0x4E)
    static let right = key(0x4F), left = key(0x50), down = key(0x51), up = key(0x52)
    static let leftControl = key(0xE0), leftShift = key(0xE1), leftAlt = key(0xE2), leftGUI = key(0xE3)
    static func function(_ n: Int) -> UInt32 { key(UInt16(0x3A + n - 1)) } // F1 … F12
    static let c = key(0x06), v = key(0x19)
    static let capsLock = key(0x39), printScreen = key(0x46)

    // Consumer page (media keys; the receiver maps them to NX_KEYTYPE_* / VK_MEDIA_*).
    static let previousTrack = consumer(0xB6), playPause = consumer(0xCD), nextTrack = consumer(0xB5)
    static let mute = consumer(0xE2), volumeDown = consumer(0xEA), volumeUp = consumer(0xE9)
}

/// The focus computer's OS decides the control key names and the copy / paste modifier.
enum KeyTarget: Equatable {
    case windows, mac
    init(platform: String?) { self = platform == "macos" ? .mac : .windows }
}

/// The four shared modifiers (control area; the letter layer's Shift is the same one).
enum Modifier: String, CaseIterable, Identifiable {
    case shift, ctrl, alt, meta
    var id: String { rawValue }
    var code: UInt32 {
        switch self {
        case .shift: HID.leftShift
        case .ctrl: HID.leftControl
        case .alt: HID.leftAlt
        case .meta: HID.leftGUI
        }
    }
}

enum ModifierState: Equatable { case held, locked }

/// Modifier three-state rules (§3.1 修饰键三态, user 2026-11-12; the same rules as Android's
/// KeyboardModel). Entering held / locked sends the modifier down, returning to released sends it
/// up. A tap holds a released modifier and releases a held or locked one; a second tap within
/// `doubleTap` of the previous one makes the pair a double tap: one press + release of the
/// modifier on the remote (down + up), ending released. A long press locks, or unlocks a locked
/// modifier.
struct ModifierBoard: Equatable {
    static let doubleTap: TimeInterval = 0.3
    /// How long a finger stays on a modifier to lock it (same as the lock button's long press).
    static let longPress: TimeInterval = 0.45
    private(set) var states: [Modifier: ModifierState] = [:]
    /// When each modifier was last tapped, while its next tap would complete a double tap.
    private var lastTap: [Modifier: TimeInterval] = [:]

    var anyDown: Bool { !states.isEmpty }
    func state(_ m: Modifier) -> ModifierState? { states[m] }
    func isDown(_ m: Modifier) -> Bool { states[m] != nil }

    mutating func tap(_ m: Modifier, at now: TimeInterval) -> [KeyEvent] {
        let quick = lastTap[m].map { now - $0 <= Self.doubleTap } ?? false
        switch states[m] {
        case nil where quick:
            // Second tap of a double tap that began by releasing the modifier: one press + release.
            lastTap[m] = nil
            return [KeyEvent(code: m.code, down: true), KeyEvent(code: m.code, down: false)]
        case nil:
            lastTap[m] = now
            states[m] = .held
            return [KeyEvent(code: m.code, down: true)]
        case .held:
            // A quick second tap: the first tap's down and this up are the double tap's press + release.
            states[m] = nil
            lastTap[m] = quick ? nil : now
            return [KeyEvent(code: m.code, down: false)]
        case .locked:
            states[m] = nil
            lastTap[m] = now
            return [KeyEvent(code: m.code, down: false)]
        }
    }

    /// Long press: released or held → locked (a held one is already down on the remote);
    /// locked → released.
    mutating func longPress(_ m: Modifier) -> [KeyEvent] {
        lastTap[m] = nil
        switch states[m] {
        case nil:
            states[m] = .locked
            return [KeyEvent(code: m.code, down: true)]
        case .held:
            states[m] = .locked
            return []
        case .locked:
            states[m] = nil
            return [KeyEvent(code: m.code, down: false)]
        }
    }

    /// After a non-modifier key (or a touchpad click): held modifiers release, locked ones stay.
    mutating func releaseHeld() -> [KeyEvent] {
        let held = Modifier.allCases.filter { states[$0] == .held }
        for m in held { states[m] = nil }
        return held.map { KeyEvent(code: $0.code, down: false) }
    }

    /// Manual switch to the system keyboard, closing the keyboard, leaving the page.
    mutating func releaseAll() -> [KeyEvent] {
        let down = Modifier.allCases.filter { states[$0] != nil }
        states = [:]
        return down.map { KeyEvent(code: $0.code, down: false) }
    }
}

/// Oink's own F-key layer switch (§3.1 Fn). Nothing is sent for Fn itself.
struct FnLayer: Equatable {
    private(set) var state: ModifierState?
    private var lastTap: TimeInterval?
    var isOn: Bool { state != nil }

    mutating func tap(at now: TimeInterval) {
        let quick = lastTap.map { now - $0 <= ModifierBoard.doubleTap } ?? false
        lastTap = now
        switch state {
        case nil: state = .held
        case .held where quick: state = .locked
        case .held, .locked: state = nil
        }
    }
    /// An F key was pressed: a held layer goes back to the default row.
    mutating func usedFKey() { if state == .held { state = nil } }
    mutating func reset() { state = nil; lastTap = nil }
}

/// Copy / paste carry their own modifier (Ctrl on Windows, ⌘ on Mac) and merge with the user's.
enum ComboKey {
    case copy, paste
    var code: UInt32 { self == .copy ? HID.c : HID.v }

    /// Down + up of the combination. The combo modifier is wrapped round the key only when the
    /// user does not already hold it down on the remote.
    func events(target: KeyTarget, board: ModifierBoard) -> [KeyEvent] {
        let mod: Modifier = target == .mac ? .meta : .ctrl
        let wrap = !board.isDown(mod)
        var out: [KeyEvent] = []
        if wrap { out.append(KeyEvent(code: mod.code, down: true)) }
        out += [KeyEvent(code: code, down: true), KeyEvent(code: code, down: false)]
        if wrap { out.append(KeyEvent(code: mod.code, down: false)) }
        return out
    }
}

/// What one character key of the emulated keyboard sends (§3.1.2).
enum TypedChar: Equatable {
    case key(TypedKey)
    /// The layout has no certain key for the character: it goes as committed text.
    case text(String)
}

/// The physical key a character key of the emulated keyboard sends on the focus computer's layout.
struct TypedKey: Equatable {
    var code: UInt32
    /// Shift around just this key: the character needs it and the user's Shift is not down
    /// (§3.1.2 数字和符号层与 Shift; the user's Shift is never released by it).
    var autoShift = false
    /// AltGr (right Alt, Windows) / Option (left Alt, Mac) around just this key, inside the Shift.
    var autoAlt: UInt32?
    /// A dead key (^ ~ on European layouts): tapped at once with a Space after it, never held or
    /// repeated.
    var dead = false

    /// Press: Shift, then Alt, then the key.
    var downEvents: [KeyEvent] {
        var out: [KeyEvent] = []
        if autoShift { out.append(KeyEvent(code: HID.leftShift, down: true)) }
        if let autoAlt { out.append(KeyEvent(code: autoAlt, down: true)) }
        out.append(KeyEvent(code: code, down: true))
        return out
    }
    /// Release: the key, then Alt, then Shift.
    var upEvents: [KeyEvent] {
        var out = [KeyEvent(code: code, down: false)]
        if let autoAlt { out.append(KeyEvent(code: autoAlt, down: false)) }
        if autoShift { out.append(KeyEvent(code: HID.leftShift, down: false)) }
        return out
    }
    /// A dead key: pressed and released at once, then Space so the character appears on its own.
    var deadEvents: [KeyEvent] {
        downEvents + upEvents + [KeyEvent(code: HID.space, down: true), KeyEvent(code: HID.space, down: false)]
    }
}

/// 调节 › 键盘布局 (§3.1.2): which key a character key sends on the focus computer's layout. The
/// tables (US, UK, German, French, Spanish, JIS on Windows and macOS, with AltGr / Option, dead
/// keys and characters sent as text) live in the shared Rust core (`controlTypedChar`,
/// oink-mobile-core key_layouts), the same ones Android uses.
enum KeyLayouts {
    /// The character key `c` (as on the key without Shift) on a `target` computer with `layout`
    /// (the setting's raw value; "auto" and "ko" are US), with the modifiers `board` holds down.
    /// Nil without the core (nothing could be sent).
    static func typed(_ c: Character, layout: String, target: KeyTarget, board: ModifierBoard) -> TypedChar? {
        #if OINK_MOBILE_CORE_FFI
        let modifiers = ControlModifiersDown(shift: board.isDown(.shift), ctrl: board.isDown(.ctrl),
                                             alt: board.isDown(.alt), meta: board.isDown(.meta))
        switch controlTypedChar(layout: layout, targetMac: target == .mac, ch: String(c), modifiers: modifiers) {
        case let .key(code, autoShift, autoAlt, dead):
            return .key(TypedKey(code: code, autoShift: autoShift, autoAlt: autoAlt, dead: dead))
        case let .text(text):
            return .text(text)
        }
        #else
        return nil
        #endif
    }
}

/// Key faces of the emulated keyboard (always US faces, §3.1.2).
enum USLayout {
    /// What a base key types with Shift down (mockup `US_SHIFTED`, plus ` → ~).
    static let shifted: [Character: Character] = [
        "1": "!", "2": "@", "3": "#", "4": "$", "5": "%", "6": "^", "7": "&", "8": "*", "9": "(", "0": ")",
        "-": "_", "=": "+", "[": "{", "]": "}", "\\": "|", ";": ":", "'": "\"", ",": "<", ".": ">", "/": "?", "`": "~",
    ]

    /// Key face on the number + symbol layer: with Shift down a base key shows its shifted
    /// character; keys that already need Shift keep their label.
    static func symbolFace(_ c: Character, shift: Bool) -> Character {
        shift ? shifted[c] ?? c : c
    }
}

/// The emulated keyboard's rows (§3.1.2, 用户 2026-11-12): the number row stays on top of both
/// layers, then three letter or symbol rows, then the bottom row (layer switch, `,`, space, `.`,
/// return) — five rows of one height on both layers, so switching layers moves nothing.
enum EmulatedRows {
    static let numbers: [Character] = Array("1234567890")
    static let letters: [[Character]] = [Array("qwertyuiop"), Array("asdfghjkl"), Array("zxcvbnm")]
    static let symbols: [[Character]] = [Array("-/:;()$&@\""), Array("[]{}#%^*+="), Array("_\\|~<>?!'")]
    /// Beside the space bar on both layers.
    static let besideSpace: [Character] = [",", "."]
    static let rowCount = 1 + 3 + 1

    /// A character key's face: letters in upper case while Shift is down; every other key shows
    /// what Shift + its physical key types on a US keyboard (`1` → `!`, `,` → `<`).
    static func face(_ c: Character, shift: Bool) -> String {
        c.isLetter ? (shift ? c.uppercased() : String(c)) : String(USLayout.symbolFace(c, shift: shift))
    }
}

/// Which keyboard shows (§3.1): the user's remembered choice, except that any modifier down over
/// the system keyboard brings the emulated one up temporarily (combinations need raw key codes).
enum KeyboardKind: String { case system, emulated }

struct KeyboardChoice: Equatable {
    var preferred: KeyboardKind
    /// A password field popped the keyboard up (control-page.md §9): the emulated keyboard is shown
    /// whatever the remembered kind, so the password never goes through the phone's input method.
    /// Cleared when the keyboard closes or the switch key is tapped; `preferred` is unchanged.
    var secure = false
    func shown(modifiersDown: Bool) -> KeyboardKind {
        secure || (preferred == .system && modifiersDown) ? .emulated : preferred
    }
    func temporary(modifiersDown: Bool) -> Bool { !secure && preferred == .system && modifiersDown }
}

/// Committed text for `input_text` (§3.1.1): line breaks and tabs become Return / Tab keys, other
/// control characters are dropped (the backend rejects them), and text longer than one
/// `InputText` (4096 UTF-8 bytes) is split on character boundaries.
enum TextPiece: Equatable {
    case text(String)
    case key(UInt32)

    static let maxBytes = 4096

    static func pieces(_ text: String) -> [TextPiece] {
        var out: [TextPiece] = []
        var current = ""
        var bytes = 0
        func flush() {
            if !current.isEmpty { out.append(.text(current)) }
            current = ""; bytes = 0
        }
        for ch in text {
            if ch == "\n" || ch == "\r\n" || ch == "\r" || ch == "\u{2028}" || ch == "\u{2029}" {
                flush(); out.append(.key(HID.returnKey)); continue
            }
            if ch == "\t" { flush(); out.append(.key(HID.tab)); continue }
            if ch.unicodeScalars.allSatisfy({ $0.properties.generalCategory == .control }) { continue }
            let size = String(ch).utf8.count
            if bytes + size > maxBytes { flush() }
            current.append(ch); bytes += size
        }
        flush()
        return out
    }
}

/// Caps follows the real touch (user 2026-11-13), without a timer or auto-repeat. It always sends
/// Caps, without the PC's tap/hold mapping, and neither takes nor releases held modifiers.
struct CapsKey {
    private(set) var down = false

    mutating func setDown(_ value: Bool) -> [KeyEvent] {
        guard down != value else { return [] }
        down = value
        return [KeyEvent(code: HID.capsLock, down: value)]
    }
}
