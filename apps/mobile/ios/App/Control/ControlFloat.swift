import CoreGraphics

// The landscape control page's full screen and its floating keyboard (control-page.md §1.2, user
// 2026-11-12): the numbers and rules that decide, kept apart from the views so the unit tests check
// them (Tests/ControlFloatTests.swift). The same values and rules as Android's FloatMotion /
// Fullscreen (apps/mobile/android/.../control).

/// The landscape emulated keyboard's container: the mappings of its handle gestures, in points,
/// points per second and the morph progress q.
///
/// - Sideways: the container follows the finger 1:1 between `Bounds.lo` and `Bounds.hi` (a third of
///   the travel past them); a flick rests at Apple's projection (WWDC18 "Designing Fluid
///   Interfaces"): projected = position + velocity (pt/s) ÷ 1000 × r ÷ (1 − r), r =
///   `UIScrollView.DecelerationRate.normal` = 0.998, kept inside the bounds and snapped to a bound
///   within `snap`.
/// - Down (dismiss): p = down ÷ `fadeAt`; the container shrinks toward its centre to 1 − 0.25 p and
///   fades to 1 − 0.6 p; letting go with a projected travel past `dismissAt` finishes (scale 0.6,
///   opacity 0, 200 ms ease-in) and hides the keyboard, otherwise it springs back.
/// - Double-tap the handle to expand. Upward drags do not expand.
/// - The axis locks after `slop` of movement: mostly sideways = move, downward = dismiss.
/// - Springs: critically damped, damping ratio 1, response 0.35 s.
enum FloatMotion {
    static let width: CGFloat = 400
    static let keyboardScale: CGFloat = 1.2
    static let edge: CGFloat = 8
    static let snap: CGFloat = 24
    static let slop: CGFloat = 10
    static let deceleration: CGFloat = 0.998
    static let dismissAt: CGFloat = 80
    static let fadeAt: CGFloat = 160
    static let response: Double = 0.35
    /// Mass 1, critically damped: stiffness (2π ÷ response)², damping 2√stiffness.
    static var stiffness: Double { let w = 2 * Double.pi / response; return w * w }
    static var damping: Double { 2 * stiffness.squareRoot() }
    static let dismissFinish: Double = 0.2
    static let expandedTop: CGFloat = 8
    static let expandFade: Double = 0.16
    static let iconFade: Double = 0.12
    static let restoreFade: Double = 0.12
    /// The container's top: in line with the floating top bar's row, 12 below the screen's top edge.
    static let top: CGFloat = 12
    static let radius: CGFloat = 24
    static let handleHeight: CGFloat = 32

    /// Where the projection says a flick comes to rest.
    static func project(_ position: CGFloat, velocity perSecond: CGFloat) -> CGFloat {
        position + perSecond / 1000 * deceleration / (1 - deceleration)
    }

    /// Sideways bounds of the container's left edge in screen points: the island side stops at the
    /// safe area, the free side `edge` from the screen edge.
    struct Bounds: Equatable {
        var lo: CGFloat
        var hi: CGFloat

        init(lo: CGFloat, hi: CGFloat) { self.lo = lo; self.hi = max(lo, hi) }

        /// `screenWidth`, the safe-area insets on both sides, and where the Dynamic Island is.
        init(screenWidth: CGFloat, width: CGFloat = FloatMotion.width, safeLeft: CGFloat, safeRight: CGFloat, islandRight: Bool) {
            let lo = islandRight ? FloatMotion.edge : safeLeft
            let hi = islandRight ? screenWidth - safeRight - width : screenWidth - FloatMotion.edge - width
            self.init(lo: lo, hi: hi)
        }

        /// The default place: on the side away from the island.
        func home(islandRight: Bool) -> CGFloat { islandRight ? lo : hi }

        /// Past a bound the container moves a third of the finger's travel.
        func rubber(_ x: CGFloat) -> CGFloat {
            if x < lo { return lo - (lo - x) / 3 }
            if x > hi { return hi + (x - hi) / 3 }
            return x
        }

        /// The resting place of a flick let go at `x` with `velocity` pt/s.
        func rest(_ x: CGFloat, velocity: CGFloat) -> CGFloat {
            let p = min(hi, max(lo, FloatMotion.project(x, velocity: velocity)))
            if p - lo < FloatMotion.snap { return lo }
            if hi - p < FloatMotion.snap { return hi }
            return p
        }
    }

    /// Dismiss feedback for a downward travel and the finish progress (0…1): scale and opacity.
    static func dismissLook(down: CGFloat, finish: CGFloat) -> (scale: CGFloat, opacity: CGFloat) {
        let p = min(1, max(0, down / fadeAt))
        let s0 = 1 - 0.25 * p, o0 = 1 - 0.6 * p
        return (s0 + (0.6 - s0) * finish, o0 * (1 - finish))
    }

    static func dismisses(down: CGFloat, velocity: CGFloat) -> Bool { project(down, velocity: velocity) > dismissAt }

    static func smoothstep(_ a: CGFloat, _ b: CGFloat, _ x: CGFloat) -> CGFloat {
        let t = min(1, max(0, (x - a) / (b - a)))
        return t * t * (3 - 2 * t)
    }

    /// Double-tap expansion passes through a keyboard icon before the full layout appears.
    struct Look: Equatable { var floating: CGFloat; var icon: CGFloat }

    static func morphLook(q: CGFloat) -> Look {
        let qc = min(1, q)
        return Look(floating: 1 - smoothstep(0.15, 0.45, qc), icon: smoothstep(0.30, 0.50, qc))
    }

    static func lerp(_ a: CGFloat, _ b: CGFloat, _ q: CGFloat) -> CGFloat { a + (b - a) * min(1, max(0, q)) }
}

/// 全屏 / 恢复 (user 2026-11-12, replaces the 屏幕方向 setting): the control page's landscape layout is
/// its full screen. It is entered by turning the phone (the system rotates the window) or with 全屏
/// (portrait top right), which turns the interface to landscape even while the phone is upright; 恢复
/// (landscape top right) turns it back to portrait even while the phone is sideways.
///
/// After 恢复 from a full screen that a rotation entered, turning the phone no longer enters it until
/// the user leaves the control page (another tab, or the app going to the background). After 恢复 from
/// a full screen that 全屏 entered, rotation works as usual again — once the phone is held upright
/// (until then the interface would turn straight back to landscape).
struct FullscreenRules: Equatable {
    enum Request: Equatable { case system, landscape, portrait }

    /// The current full screen was entered with 全屏.
    private(set) var byButton = false
    /// 恢复 after a rotation: portrait until the user leaves the page.
    private(set) var noAuto = false
    /// 恢复 after 全屏 with the phone not upright yet: portrait until it is.
    private(set) var holdPortrait = false

    /// The orientation the page asks for while it is shown.
    var request: Request {
        if byButton { return .landscape }
        if noAuto || holdPortrait { return .portrait }
        return .system
    }

    /// Whether the phone's own tilt matters (`tilt` should be reported).
    var watchesTilt: Bool { holdPortrait }

    /// 全屏.
    mutating func enter() {
        byButton = true
        holdPortrait = false
    }

    /// 恢复; `upright`: the phone is held upright now (nil: unknown, e.g. lying flat).
    mutating func restore(upright: Bool?) {
        if byButton {
            byButton = false
            holdPortrait = upright != true
        } else {
            noAuto = true
        }
    }

    /// The phone's tilt while `watchesTilt`.
    mutating func tilt(upright: Bool) {
        if upright { holdPortrait = false }
    }

    /// The user left the control page.
    mutating func left() {
        byButton = false
        noAuto = false
        holdPortrait = false
    }
}

/// When the control page counts as left for 全屏 / 恢复 (FullscreenRules.left; user 2026-11-12):
/// another tab, or the app in the background. The app only inactive — Control Center or
/// Notification Center pulled down, a system alert over it — keeps the full screen and its rules
/// (「横屏游戏也不会在拉下控制中心就交还给系统」).
enum ControlPageExit {
    enum Phase: Equatable { case active, inactive, background }

    static func left(tabShown: Bool, phase: Phase) -> Bool { !tabShown || phase == .background }
    static func keepsInput(tabShown: Bool, phase: Phase) -> Bool { !left(tabShown: tabShown, phase: phase) }
}
