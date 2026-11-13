import SwiftUI
import UIKit

/// Colour tokens of the Oink mobile design (mockup `src/theme.css`), resolved for one
/// colour scheme and one theme accent. Views read the palette from the environment
/// (`@Environment(\.oink)`); the root view rebuilds it when the scheme or accent changes.
struct OinkPalette {
    enum Accent: String, CaseIterable, Identifiable {
        case pink, orange, violet, blue, teal
        var id: String { rawValue }
        var label: String {
            switch self { case .pink: "粉色"; case .orange: "橙色"; case .violet: "紫色"; case .blue: "蓝色"; case .teal: "青色" }
        }
        /// `--accent`
        var base: UInt32 {
            switch self { case .pink: 0xFF6B8E; case .orange: 0xFF8A3D; case .violet: 0x8B6CF6; case .blue: 0x3D8BFD; case .teal: 0x14B8A6 }
        }
        /// `--accent-deep` (accent text on light surfaces)
        var deep: UInt32 {
            switch self { case .pink: 0xD42E62; case .orange: 0xC2530C; case .violet: 0x6440D8; case .blue: 0x1D62D0; case .teal: 0x0B7D70 }
        }
        /// `--accent-dark-ink`
        var darkInk: UInt32 {
            switch self { case .pink: 0xFF8FAB; case .orange: 0xFFA463; case .violet: 0xAB95FF; case .blue: 0x6FA8FF; case .teal: 0x3FD6C4 }
        }
    }

    let dark: Bool
    let accentName: Accent

    // Surfaces
    let bg, card, sheet, control, elev, tile, field, fill, chip, track, level: Color
    /// `--c-input-disabled`: the transfer composer's field while nothing can be sent
    /// (user 2026-11-11: darker than `fill`, the enabled field, so the two tell apart).
    let inputDisabled: Color
    // Text and icons
    let ink, bubbleInk, text2, muted, faint, hint, dotOff: Color
    // Lines
    let line, lineSolid, border, handle, progressTrack: Color
    // Status
    let good, goodSoft, goodInk, bad, badSoft, badInk, warning, onWarning: Color
    // Overlays and special layers
    let overlay, press, shadow, toast, toastInk, toastActionBG, onAccent, thumb: Color
    // Content colours
    let osWindows, osAndroid, file, folder, systemApp: Color
    /// Android privileged-mode crown: fill, and the 0.5 pt rim under it (light only needs a
    /// darker rim for a 3:1 outline on white; dark uses the fill colour).
    let crown, crownEdge: Color
    // Accent
    let accent, accentDeep, accentInk, accentSoft, accentSoft2, accentSoft3, accentBubble: Color
    /// Rich text in a transfer bubble (native-rewrite.md「富文本气泡」, user 2026-11-12): links,
    /// list markers and picture alt text, quote text, rules (quote bar, divider, table header
    /// line), and the tint under inline code, code blocks and tables. The last two are
    /// translucent so they work on the grey and the accent bubble alike.
    let richLink, richMarker, richQuoteInk, richRule, richCodeBg: Color
    // Control page: keyboards, iOS menu material, touchpad slab (mockup theme.css / control.css)
    let kbBg, kbKey, kbKey2, kbInk, kbShadow, menuIOS, menuIOSLine: Color
    let padHi, padMid, padLo, padHighlight, padEdge, padInner, padOutline, padDrop, padGlow, padPress, padHint: Color
    let padGrain: Double
    // Share card
    let shareSurface, shareBorder, shareTrack, shareText, shareArrow, shareScrim, shareTile, shareTile2: Color

    init(dark: Bool, accent: Accent) {
        self.dark = dark
        accentName = accent
        let c = { (light: UInt32, darkValue: UInt32) in Color(hex: dark ? darkValue : light) }
        bg = c(0xFFFFFF, 0x14161C); card = c(0xF4F4F6, 0x1C1F27); sheet = c(0xF9F9FB, 0x1C1F27)
        control = c(0xFFFFFF, 0x232731); elev = c(0xFFFFFF, 0x232731); tile = c(0xFFFFFF, 0x2D323E)
        field = c(0xF9F9FB, 0x1A1D24); fill = c(0xF1F1F4, 0x232731); chip = c(0xECECEF, 0x2D323E)
        track = c(0xE2E2E7, 0x2D323E); level = c(0xFFFFFF, 0x4A5161)
        inputDisabled = c(0xE4E4EA, 0x191C23)
        ink = c(0x1B1C22, 0xECEEF2); bubbleInk = c(0x34353D, 0xDFE2E8); text2 = c(0x5F606A, 0xA7ADBA)
        muted = c(0x7B7D88, 0x868D9D); faint = c(0x989AA4, 0x767D8E); hint = c(0xA3A5AE, 0x6F7687)
        dotOff = c(0xB8B9C0, 0x4A505E)
        line = dark ? Color(hex: 0xECEEF2).opacity(0.08) : Color(hex: 0x1B1C22).opacity(0.07)
        lineSolid = c(0xEEEEF2, 0x262A34); border = c(0xE9E9ED, 0x2D323E); handle = c(0xC7C8CE, 0x4A505E)
        progressTrack = c(0xDEDEE5, 0x2D323E)
        good = Color(hex: 0x2FBF71); goodInk = c(0x1C8F50, 0x5FD39A)
        goodSoft = dark ? Color(mixing: 0x2FBF71, 0.18, with: 0x14161C) : Color(hex: 0xE2F6EB)
        bad = Color(hex: 0xF04452); badInk = c(0xD42F3D, 0xFF8D7E)
        badSoft = dark ? Color(mixing: 0xF04452, 0.18, with: 0x14161C) : Color(hex: 0xFDE8EA)
        warning = Color(hex: 0xF5A524); onWarning = Color(hex: 0x1B1C22)
        overlay = dark ? Color.black.opacity(0.55) : Color(hex: 0x1B1C22).opacity(0.24)
        press = dark ? Color(hex: 0xECEEF2).opacity(0.06) : Color(hex: 0x1B1C22).opacity(0.05)
        shadow = dark ? Color.black.opacity(0.45) : Color(hex: 0x1B1C22).opacity(0.12)
        toast = c(0x292A31, 0xECEEF2); toastInk = c(0xFFFFFF, 0x14161C)
        toastActionBG = dark ? Color(hex: 0x14161C).opacity(0.07) : Color.white.opacity(0.1)
        onAccent = .white; thumb = .white
        // Android green as filled in Google's own android-head_flat.svg, both themes.
        osWindows = c(0x0078D4, 0x4CA6EE); osAndroid = Color(hex: 0x34A853)
        crown = c(0xF0B400, 0xF5B400); crownEdge = c(0xA87500, 0xF5B400)
        file = Color(hex: 0x8698B4); folder = Color(hex: 0xD5A348); systemApp = Color(hex: 0x3D8BFD)

        let bgHex: UInt32 = dark ? 0x14161C : 0xFFFFFF
        self.accent = Color(hex: accent.base)
        accentDeep = Color(hex: accent.deep)
        accentInk = Color(hex: dark ? accent.darkInk : accent.deep)
        if dark {
            accentSoft = Color(mixing: accent.base, 0.20, with: bgHex)
            accentSoft2 = Color(mixing: accent.base, 0.24, with: bgHex)
            accentSoft3 = Color(mixing: accent.base, 0.30, with: bgHex)
            accentBubble = Color(mixing: accent.base, 0.22, with: 0x1C1F27)
        } else if accent == .pink {
            accentSoft = Color(hex: 0xFFE7ED); accentSoft2 = Color(hex: 0xFFE1EA)
            accentSoft3 = Color(hex: 0xFFDAE5); accentBubble = Color(hex: 0xFFE9EF)
        } else {
            accentSoft = Color(mixing: accent.base, 0.15, with: bgHex)
            accentSoft2 = Color(mixing: accent.base, 0.19, with: bgHex)
            accentSoft3 = Color(mixing: accent.base, 0.25, with: bgHex)
            accentBubble = Color(mixing: accent.base, 0.14, with: bgHex)
        }

        // = accentInk, muted, text2 (self is not readable before every token is set).
        richLink = Color(hex: dark ? accent.darkInk : accent.deep)
        richMarker = c(0x7B7D88, 0x868D9D); richQuoteInk = c(0x5F606A, 0xA7ADBA)
        richRule = dark ? Color(hex: 0xECEEF2).opacity(0.22) : Color(hex: 0x1B1C22).opacity(0.16)
        richCodeBg = dark ? Color(hex: 0xECEEF2).opacity(0.08) : Color(hex: 0x1B1C22).opacity(0.06)

        let inkBase = Color(hex: 0x1B1C22)
        kbBg = c(0xD6D8DE, 0x1C1E24); kbKey = c(0xFFFFFF, 0x454A56); kbKey2 = c(0xB5B9C3, 0x30343D); kbInk = c(0x1B1C22, 0xECEEF2)
        kbShadow = Color.black.opacity(dark ? 0.5 : 0.28)
        menuIOS = dark ? Color(red: 40 / 255, green: 42 / 255, blue: 48 / 255).opacity(0.84) : Color(red: 249 / 255, green: 249 / 255, blue: 250 / 255).opacity(0.84)
        menuIOSLine = dark ? Color(red: 235 / 255, green: 235 / 255, blue: 245 / 255).opacity(0.14) : Color(red: 60 / 255, green: 60 / 255, blue: 67 / 255).opacity(0.18)
        padHi = c(0xFBFBFC, 0x3A3D45); padMid = c(0xEFF0F3, 0x2C2F36); padLo = c(0xE5E6EA, 0x25272D)
        padGrain = dark ? 0.40 : 0.32
        padHighlight = Color.white.opacity(dark ? 0.09 : 0.95); padEdge = Color.white.opacity(dark ? 0.035 : 0.5)
        padInner = dark ? Color.black.opacity(0.28) : inkBase.opacity(0.035)
        padOutline = dark ? Color.black.opacity(0.55) : inkBase.opacity(0.10)
        padDrop = dark ? Color.black.opacity(0.3) : inkBase.opacity(0.05)
        padGlow = Color.white.opacity(dark ? 0.07 : 0.55)
        padPress = dark ? Color.black.opacity(0.25) : inkBase.opacity(0.05)
        padHint = dark ? Color.white.opacity(0.2) : inkBase.opacity(0.24)

        shareSurface = c(0xFFFFFF, 0x1C1F27); shareBorder = dark ? Color(hex: 0x2D323E) : .clear
        shareTrack = c(0xECEEF2, 0x2D323E); shareText = c(0x555B6A, 0xA7ADBA); shareArrow = c(0x1B1E28, 0xF2F3F5)
        shareScrim = dark ? Color.black.opacity(0.48) : Color(red: 20 / 255, green: 22 / 255, blue: 28 / 255).opacity(0.32)
        shareTile = c(0xE3E5EA, 0x232731); shareTile2 = c(0xD5D8DF, 0x2D323E)
    }

    /// Status tone of a PC device icon (PC `.dev-ico[data-tone]`).
    func tone(_ tone: IconTone) -> Color {
        switch tone { case .good: good; case .bad: bad; case .warn: warning; case .accent: accent; case .muted: faint }
    }
}

enum IconTone { case good, bad, warn, accent, muted }

private struct OinkPaletteKey: EnvironmentKey {
    static let defaultValue = OinkPalette(dark: false, accent: .pink)
}

extension EnvironmentValues {
    var oink: OinkPalette {
        get { self[OinkPaletteKey.self] }
        set { self[OinkPaletteKey.self] = newValue }
    }
}

extension Color {
    init(hex: UInt32) {
        self.init(.sRGB, red: Double((hex >> 16) & 0xFF) / 255, green: Double((hex >> 8) & 0xFF) / 255,
                  blue: Double(hex & 0xFF) / 255, opacity: 1)
    }
    /// `color-mix(in srgb, a p, b)`, precomputed as an opaque colour.
    init(mixing a: UInt32, _ p: Double, with b: UInt32) {
        func ch(_ v: UInt32, _ s: UInt32) -> Double { Double((v >> s) & 0xFF) / 255 }
        self.init(.sRGB, red: ch(a, 16) * p + ch(b, 16) * (1 - p), green: ch(a, 8) * p + ch(b, 8) * (1 - p),
                  blue: ch(a, 0) * p + ch(b, 0) * (1 - p), opacity: 1)
    }
}

/// User preferences. The theme accent lives in the App Group defaults so the share
/// extension draws with the same accent; the others are app-only standard defaults.
enum OinkPrefs {
    static var shared: UserDefaults { SharedStore.defaults }
    static let accent = "themeColor"
    static let scheme = "colorScheme"
    static let language = "appLanguage"
    static let homeTab = "homeTab"
    static let micAutoOff = "microphoneIdleMinutes"
    static let speakerAutoOff = "speakerIdleMinutes"
    static let echoCancel = "aecEnabled"
    // Control page (phone-local, docs control-page.md §2.1 / §3.1; CHANGE-PLAN §3.5)
    static let pointerSpeed = "controlPointerSpeed"
    static let oneFingerSwipe = "controlOneFingerSwipe"
    static let keyboardLayout = "controlKeyboardLayout"
    /// System or Oink keyboard, remembered per orientation (portrait default system, landscape
    /// default Oink's; user 2026-11-12).
    static let keyboardKindPortrait = "controlKeyboardKindPortrait"
    static let keyboardKindLandscape = "controlKeyboardKindLandscape"
    /// The landscape Oink keyboard was last left expanded to the whole screen (user 2026-11-12).
    static let keyboardExpanded = "controlKeyboardExpanded"
    /// 键盘自动弹出 (control-page.md §9), Bool, absent = off (user 2026-11-12).
    static let keyboardAutoPopup = "controlKeyboardAutoPopup"
}

/// Applies the palette for the current scheme and the stored accent, and tints the
/// system components (tab bar, menus, alerts) with the theme accent.
struct OinkThemed: ViewModifier {
    @Environment(\.colorScheme) private var scheme
    @AppStorage(OinkPrefs.accent, store: OinkPrefs.shared) private var accent = "pink"
    func body(content: Content) -> some View {
        let palette = OinkPalette(dark: scheme == .dark, accent: OinkPalette.Accent(rawValue: accent) ?? .pink)
        content
            .environment(\.oink, palette)
            .tint(palette.accentInk)
    }
}

extension View {
    func oinkThemed() -> some View { modifier(OinkThemed()) }
}

/// Fonts: the mockup uses the system font (SF / PingFang). CSS weight 650 renders as semibold.
extension Font {
    static func oink(_ size: CGFloat, _ weight: Font.Weight = .regular) -> Font {
        .system(size: size, weight: weight)
    }
}
