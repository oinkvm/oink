import CoreGraphics

// Geometry of the control page that is not plain stacking (control-page.md §1, 用户 2026-11-12):
// the landscape page with its keyboards, and the focus capsule's padding. Plain values so the
// unit tests can check them (Tests/ControlLayoutTests.swift).

/// Insets without SwiftUI's EdgeInsets (this file is also compiled into the test host).
struct LayoutInsets: Equatable {
    var top: CGFloat = 0
    var leading: CGFloat = 0
    var bottom: CGFloat = 0
    var trailing: CGFloat = 0
}

/// The landscape control page (§1.2, user 2026-11-12), in screen coordinates.
///
/// The touchpad covers the whole screen (its edges and the safe areas; the screen's corners round
/// it). The top bar floats over it: one 48-high row 12 below the screen's top edge, 16 inside the
/// safe area at the sides. With a keyboard up:
/// - Oink's keyboard floats over the pad in a fixed reference-phone-width container (clamped to the safe width) from the bar's row (12) down to
///   the bottom safe edge (FloatMotion has its gestures), or covers the whole screen when expanded
///   (padding: 8 above, the safe area at both sides, the home indicator + 4 below);
/// - system keyboard: H < 135 gives two full-width rows; H >= 135 splits the remaining safe
///   area into the touchpad on the left and three rows on the right (both grips).
struct ControlLandscapeLayout: Equatable {
    enum Keyboard: Equatable { case none, floating, expanded, system }

    static let barTop: CGFloat = 12
    static let barRow: CGFloat = 48
    static let side: CGFloat = 16
    /// The control keys above the system keyboard: padding 7 / 6 (8 at the screen edge).
    static let systemPadding = LayoutInsets(top: 7, leading: 6, bottom: 7, trailing: 6)
    static let screenEdgePadding: CGFloat = 8

    /// The whole window.
    var full: CGSize
    /// The page's safe area insets; `bottom` includes the system keyboard while it is up.
    var insets: LayoutInsets
    /// The window's own bottom inset (home indicator), without the keyboard.
    var home: CGFloat
    /// Dynamic Island (or camera) on the right.
    var islandRight: Bool
    var keyboard: Keyboard

    /// The safe area (above the system keyboard while it is up).
    var safe: CGRect {
        CGRect(x: insets.leading, y: insets.top,
               width: max(0, full.width - insets.leading - insets.trailing),
               height: max(0, full.height - insets.top - insets.bottom))
    }

    /// The floating top bar's row.
    var bar: CGRect {
        CGRect(x: safe.minX + Self.side, y: Self.barTop, width: max(0, safe.width - 2 * Self.side), height: Self.barRow)
    }

    static let systemReferenceWidth: CGFloat = 405
    static let systemThreshold = systemReferenceWidth / 3
    var systemHeight: CGFloat { max(0, full.height - insets.bottom) }
    var systemTwoRows: Bool { systemHeight < Self.systemThreshold }

    var pad: CGRect? {
        guard keyboard == .system else { return CGRect(origin: .zero, size: full) }
        guard !systemTwoRows else { return nil }
        return CGRect(x: safe.minX + 6, y: 6, width: max(0, safe.width / 2 - 12), height: max(0, systemHeight - 12))
    }

    /// The release capsule of the edge lock: 14 above the home indicator.
    var releaseBottom: CGFloat { 14 + home }

    /// The control keys above the system keyboard.
    var systemPanel: CGRect? {
        guard keyboard == .system else { return nil }
        let left = systemTwoRows ? safe.minX : safe.midX
        let width = systemTwoRows ? safe.width : safe.width / 2
        return CGRect(x: left, y: 0, width: width, height: systemHeight)
    }

    var systemPadding: LayoutInsets {
        var p = Self.systemPadding
        if !islandRight { p.trailing = Self.screenEdgePadding }
        return p
    }

    /// Fixed from the reference phone's measured portrait width, not the current window.
    /// iPhone Air: short edge 1260 pixels / scale 3 = 420 pt (real device, 2026-11-13).
    static let floatingReferenceWidth: CGFloat = 420
    /// Narrow windows still clamp inside both safe edges.
    var floatScale: CGFloat {
        min(Self.floatingReferenceWidth / FloatMotion.width, max(0, full.width - max(8, insets.leading) - max(8, insets.trailing)) / FloatMotion.width)
    }
    var floatWidth: CGFloat { FloatMotion.width * floatScale }

    /// Sideways bounds now keep both sides inside the safe area.
    var floatBounds: FloatMotion.Bounds {
        FloatMotion.Bounds(lo: max(8, insets.leading), hi: full.width - max(8, insets.trailing) - floatWidth)
    }

    /// Height already fills the screen. Fixed key dimensions scale; flexible letter rows share
    /// the remaining height, rather than extending the container beyond the bottom safe area.
    func floating(x: CGFloat) -> CGRect {
        CGRect(x: x, y: FloatMotion.top, width: floatWidth, height: max(0, full.height - FloatMotion.top - home))
    }

    /// The expanded keyboard covers the whole screen; its keys keep these distances.
    var expandedPadding: LayoutInsets {
        let sides = max(insets.leading, insets.trailing, 8)
        return LayoutInsets(top: 8, leading: sides, bottom: home + 4, trailing: sides)
    }
}

/// Focus capsule padding (§2.2, 用户 2026-11-12): what is drawn sits as far from the left edge as
/// the chevron from the right edge — 16 in portrait (40 high), 20 in the landscape bar (48 high).
/// The chevron is drawn 4 inside its 16 box; the logo about 5.5 inside its 26 slot (0.62 scale,
/// −2 margins on both sides), so the padding is 12.5 / 16.5 left against 12 / 16 right. Without a
/// focus computer the text starts at 16 / 20.
/// Height of the portrait emulated keyboard's keys above the home indicator (§3.1.2, user
/// 2026-11-12): it follows the system keyboard last seen with the accessory bar while that one is
/// within ±10 % of the iPhone's standard keyboard (the design's 266), otherwise it is 266.
///
/// Why ±10 % (±26.6): the standard keyboard differs between iPhones and keyboards by less than
/// that (measured: 267 on the iPhone 18 Pro simulator, 284 on an iPhone Air, both 简体拼音), so
/// those follow and a switch moves nothing; a panel stacked on the keyboard adds at least one strip
/// of 34–49 (13–18 %: 九键's pinyin and candidate rows, a third-party keyboard's toolbar, emoji
/// search, dictation), and then the emulated keyboard keeps 266 instead of growing with it.
enum EmulatedKeyboardHeight {
    static let baseline: CGFloat = 266
    static let tolerance: CGFloat = 0.10

    /// Whether a system keyboard of `observed` above the home indicator is one to follow.
    static func follows(_ observed: CGFloat) -> Bool {
        abs(observed - baseline) <= baseline * tolerance
    }

    /// The keys' height for the last system keyboard seen (nil: none seen yet).
    static func keys(observed: CGFloat?) -> CGFloat {
        guard let observed, follows(observed) else { return baseline }
        return observed
    }
}

struct FocusCapsuleMetrics: Equatable {
    var height: CGFloat
    var leading: CGFloat
    var leadingNoLogo: CGFloat
    var trailing: CGFloat

    static let portrait = FocusCapsuleMetrics(height: 40, leading: 12.5, leadingNoLogo: 16, trailing: 12)
    static let landscape = FocusCapsuleMetrics(height: 48, leading: 16.5, leadingNoLogo: 20, trailing: 16)
}

/// The same 26 keys, in the user-approved system-keyboard arrangements. Fn preserves Esc and
/// switch; its twelve function keys replace the entire middle of the first row in either layout.
enum SystemControlKey: Equatable {
    case esc, copy, paste, home, end, pageUp, pageDown, previous, play, next, mute, volumeDown, volumeUp, switchKeyboard
    case caps, ctrl, meta, alt, tab, delete, fn, left, up, down, right, hideKeyboard
    case function(Int)

    static func rows(two: Bool, fn: Bool, mac: Bool) -> [[SystemControlKey]] {
        let modifiers: [Self] = mac ? [.ctrl, .alt, .meta] : [.ctrl, .meta, .alt]
        let media: [Self] = [.previous, .play, .next, .mute, .volumeDown, .volumeUp]
        let navigation: [Self] = [.home, .end, .pageUp, .pageDown]
        let middle: [Self] = fn ? (1...12).map { .function($0) } : [.copy, .paste] + (two ? navigation : []) + media
        let first: [Self] = [.esc] + middle + [.switchKeyboard]
        if two {
            return [first, [.caps] + modifiers + [.tab, .delete, .fn, .left, .up, .down, .right, .hideKeyboard]]
        }
        return [first, [.caps] + navigation + [.fn, .up, .hideKeyboard], modifiers + [.tab, .delete, .left, .down, .right]]
    }
}

/// The control page's portrait bar and keyboard button form one centred group.
enum PortraitControlBarMetrics {
    static let gap: CGFloat = 8
    static let button: CGFloat = 62
    static let tabTravel: CGFloat = (gap + button) / 2
}

/// Only the floating keyboard: normal keys take 3/4 after handle, padding and section gap.
struct FloatingKeyHeights {
    var available: CGFloat
    var gap: CGFloat
    var control: CGFloat { max(0, available - gap) / 4 }
    var normal: CGFloat { control * 3 }
}

/// Capsule/circle hit geometry for the visible top-bar items, excluding gaps and round corners.
enum ControlBarHitTest {
    static func contains(_ point: CGPoint, in frame: CGRect) -> Bool {
        guard !frame.isEmpty, frame.contains(point) else { return false }
        let radius = min(frame.width, frame.height) / 2
        let x = min(frame.maxX - radius, max(frame.minX + radius, point.x))
        let y = min(frame.maxY - radius, max(frame.minY + radius, point.y))
        return hypot(point.x - x, point.y - y) <= radius
    }
}
