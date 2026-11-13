import SwiftUI
import UIKit

// The landscape control page = the full screen (control-page.md §1.2, user 2026-11-12): the touchpad
// covers the whole screen, the top bar floats over it and passes touches through while it is in use, Oink's keyboard
// floats in a container that can be moved, dismissed and expanded to the whole screen, and the
// system keyboard takes the touchpad's place.

/// The UIKit keyboard curve the page follows when it draws the keyboard itself (§3.1.3).
let controlKeyboardCurve = Animation.timingCurve(0.38, 0.7, 0.125, 1, duration: 0.25)

/// The critically damped spring of the floating keyboard (FloatMotion), starting with `velocity`
/// (points per second) toward a target `distance` away.
func floatSpring(velocity: CGFloat = 0, distance: CGFloat = 0) -> Animation {
    let relative = abs(distance) > 0.5 ? Double(velocity / distance) : 0
    return .interpolatingSpring(mass: 1, stiffness: FloatMotion.stiffness, damping: FloatMotion.damping, initialVelocity: relative)
}

struct LandscapeControl<Bar: View>: View {
    @Environment(\.oink) private var oink
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @ObservedObject var model: ControlModel
    /// The home-indicator inset (HomeInset).
    let home: CGFloat
    @ViewBuilder let bar: () -> Bar
    /// Dynamic Island on the right (§1.2): the floating keyboard rests on the other side.
    @State private var islandRight = false

    private var mode: ControlLandscapeLayout.Keyboard {
        guard model.keyboardOpen else { return .none }
        if model.shownKeyboard == .system { return .system }
        return model.keyboardExpanded ? .expanded : .floating
    }

    var body: some View {
        GeometryReader { safe in
            let insets = safe.safeAreaInsets
            GeometryReader { geo in
                let full = geo.size
                // The keyboard safe area moves with the system keyboard (its own duration and curve), so
                // the control keys above it follow it.
                #if DEBUG
                let bottom = ControlScenes.systemHeightOverride.map { full.height - $0 } ?? insets.bottom
                #else
                let bottom = insets.bottom
                #endif
                let layout = ControlLandscapeLayout(
                    full: full, insets: LayoutInsets(top: insets.top, leading: insets.leading, bottom: bottom, trailing: insets.trailing),
                    home: home, islandRight: islandRight, keyboard: mode)
                ZStack(alignment: .topLeading) {
                    if let pad = layout.pad {
                        TrackpadArea(model: model, cornerRadius: mode == .system ? 24 : 0, releaseBottom: mode == .system ? 14 : layout.releaseBottom)
                            .frame(width: pad.width, height: pad.height)
                            .offset(x: pad.minX, y: pad.minY)
                    }
                    if let panel = layout.systemPanel {
                        systemPanel(layout)
                            .frame(width: panel.width, height: panel.height)
                            .offset(x: panel.minX, y: panel.minY)
                            .transition(.opacity)
                    }
                    if mode == .floating || mode == .expanded {
                        FloatingKeyboard(model: model, layout: layout, islandRight: islandRight)
                            .frame(width: full.width, height: full.height, alignment: .topLeading)
                            .transition(reduceMotion ? .opacity : .move(edge: .bottom))
                    }
                    if mode != .expanded && mode != .system {
                        FloatingBar(model: model) { bar() }
                            .frame(width: layout.bar.width, height: layout.bar.height)
                            .offset(x: layout.bar.minX, y: layout.bar.minY)
                    }
                }
                .frame(width: full.width, height: full.height, alignment: .topLeading)
                .animation(reduceMotion ? nil : controlKeyboardCurve, value: mode)
            }
            // Expand the layout reader itself, not just its drawn child. A child drawn beyond
            // the safe-area-sized host can be visible while UIKit rejects touches at that edge.
            // The outer reader retains the keyboard inset for the system control rows.
            .ignoresSafeArea()
        }
        .background { InterfaceOrientationReader { islandRight = $0 == .landscapeLeft }.frame(width: 1, height: 1) }
    }

    /// Above the system keyboard: the control keys, full width, rows stretched (no touchpad).
    private func systemPanel(_ layout: ControlLandscapeLayout) -> some View {
        let p = layout.systemPadding
        return InPageKeys(model: model, palette: oink, layout: "system-\(layout.systemTwoRows)-\(p.trailing)") {
            ControlKeysBar(model: model, style: .landscapeSystem(twoRows: layout.systemTwoRows))
                .padding(EdgeInsets(top: p.top, leading: p.leading, bottom: p.bottom, trailing: p.trailing))
        }
        .background(oink.kbBg)
    }
}

/// The bar stays visible during pad operations. UIKit tests the gate synchronously for each
/// new touch, so an extra finger belongs to the underlying pad even before SwiftUI redraws.
/// A finger that started on the pad already keeps its UIKit touch target when crossing the bar.
private struct FloatingBar<Content: View>: UIViewControllerRepresentable {
    @Environment(\.oink) private var palette
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @EnvironmentObject private var core: CoreSession
    @EnvironmentObject private var toasts: ToastCenter
    @ObservedObject var model: ControlModel
    @ViewBuilder let content: () -> Content

    final class Gate: UIView {
        weak var model: ControlModel?
        let areas = ControlBarHitAreas()
        override func hitTest(_ point: CGPoint, with event: UIEvent?) -> UIView? {
            guard model?.padTouching == false, model?.barShown == true else { return nil }
            // Liquid Glass may extend a button's hit region into its neighbour's gap (a tap
            // between keyboard and Restore activated Restore on iOS 27's simulator). Filter by
            // our drawn capsules before UIKit's hosting hit test, keeping those gaps on the pad.
            guard areas.frames.values.contains(where: { ControlBarHitTest.contains(point, in: $0) }) else { return nil }
            let hit = super.hitTest(point, with: event)
            return hit === self ? nil : hit
        }
    }
    final class Controller: UIViewController {
        var host: UIHostingController<AnyView>?
        override func loadView() { view = Gate() }
    }
    func makeUIViewController(context: Context) -> Controller {
        let controller = Controller()
        let gate = controller.view as! Gate
        gate.model = model
        gate.backgroundColor = .clear
        let host = UIHostingController(rootView: rooted(areas: gate.areas))
        host.safeAreaRegions = []
        host.view.backgroundColor = .clear
        controller.addChild(host)
        host.view.translatesAutoresizingMaskIntoConstraints = false
        gate.addSubview(host.view)
        NSLayoutConstraint.activate([
            host.view.leadingAnchor.constraint(equalTo: gate.leadingAnchor),
            host.view.trailingAnchor.constraint(equalTo: gate.trailingAnchor),
            host.view.topAnchor.constraint(equalTo: gate.topAnchor),
            host.view.bottomAnchor.constraint(equalTo: gate.bottomAnchor),
        ])
        host.didMove(toParent: controller)
        controller.host = host
        return controller
    }
    func updateUIViewController(_ controller: Controller, context: Context) {
        controller.host?.rootView = rooted(areas: (controller.view as! Gate).areas)
    }
    private func rooted(areas: ControlBarHitAreas) -> AnyView {
        AnyView(content().coordinateSpace(name: "control-floating-bar").environment(\.controlBarHitAreas, areas).environment(\.oink, palette).environmentObject(core).environmentObject(toasts)
            .opacity(model.barShown ? 1 : 0)
            .offset(y: model.barShown || reduceMotion ? 0 : -10)
            .accessibilityHidden(!model.barShown)
            .animation(reduceMotion ? nil : model.barShown ? .timingCurve(0.22, 1, 0.36, 1, duration: 0.24) : .easeIn(duration: 0.16), value: model.barShown))
    }
}

// MARK: Floating keyboard

/// Landscape + Oink's keyboard (§1.2, user 2026-11-12): the control keys and the keys in a rounded
/// container (420 wide, radius scaled with its width, the keyboard colour, a 0.5 edge and a shadow) floating over the
/// full-screen touchpad, from the bar's row to the bottom safe edge, by default on the side away
/// from the island. A handle (40 × 5 in a 32-high strip, scaled) on top moves it: sideways with a flick's
/// inertia, down to dismiss, double-tap to expand to the whole screen (FloatMotion has every number). Touches
/// in the container go to the keys (the handle strip to the drag); outside it, to the touchpad.
///
/// Every state drawn is a value set at once; animations only carry it there, so nothing depends on
/// an animation finishing (HomeInset).
private struct FloatingKeyboard: View {
    @Environment(\.oink) private var oink
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @EnvironmentObject private var toasts: ToastCenter
    @ObservedObject var model: ControlModel
    let layout: ControlLandscapeLayout
    let islandRight: Bool

    /// The container's left edge; nil = its default place.
    @State private var x: CGFloat?
    @State private var drag: Drag?
    @State private var handlePressed = false
    /// Downward travel (dismiss) and animated expansion progress.
    @State private var down: CGFloat = 0
    @State private var q: CGFloat = 0
    @State private var finish: CGFloat = 0
    /// 还原: the floating layout fades in at full size, then springs back.
    @State private var restoring = false
    @State private var restoreFade: CGFloat = 1
    /// The full-screen layout fades in after expanding.
    @State private var fullFade: CGFloat = 1

    private struct Drag: Equatable {
        var startX: CGFloat
        var axis: Axis?
    }

    var body: some View {
        let bounds = layout.floatBounds
        let restX = min(bounds.hi, max(bounds.lo, x ?? bounds.home(islandRight: islandRight)))
        let left = drag?.axis == .horizontal ? (x ?? restX) : restX
        let floatBox = layout.floating(x: left)
        let screen = CGRect(origin: .zero, size: layout.full)
        let expanded = model.keyboardExpanded && !restoring
        let qc = expanded ? 1 : min(1, q)
        let box = CGRect(x: FloatMotion.lerp(floatBox.minX, screen.minX, qc), y: FloatMotion.lerp(floatBox.minY, screen.minY, qc),
                         width: FloatMotion.lerp(floatBox.width, screen.width, qc), height: FloatMotion.lerp(floatBox.height, screen.height, qc))
        let radius = FloatMotion.lerp(FloatMotion.radius * layout.floatScale, 0, qc)
        let look = FloatMotion.morphLook(q: q)
        let dismiss = FloatMotion.dismissLook(down: down, finish: finish)
        let shape = RoundedRectangle(cornerRadius: radius, style: .continuous)
        ZStack(alignment: .top) {
            if expanded {
                fullLayout(top: FloatMotion.expandedTop).opacity(fullFade)
            } else {
                let floatO = restoring ? restoreFade : look.floating
                if floatO > 0.001 {
                    floatingLayout.opacity(floatO)
                }
                if look.icon > 0.001 && !restoring {
                    AssetIcon("PCGlyph-keyboard", size: 44, color: oink.kbInk)
                        .opacity(look.icon * 0.55)
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                        .allowsHitTesting(false)
                }
                handle
            }
        }
        .frame(width: box.width, height: box.height, alignment: .top)
        .background(oink.kbBg, in: shape)
        .clipShape(shape)
        .overlay { shape.strokeBorder(oink.kbShadow, lineWidth: 0.5).opacity(1 - qc) }
        .shadow(color: oink.shadow.opacity(1 - qc), radius: 18, y: 12)
        .contentShape(shape)
        .scaleEffect(dismiss.scale)
        .opacity(dismiss.opacity)
        .overlay(alignment: .top) {
            if !expanded {
                // The otherwise exposed pad strip above the keyboard belongs to its handle.
                // Reserve enough height below too, so the hit target never steals a key row.
                KeyboardHandleInput(pressed: { handlePressed = $0 }, changed: moveHandle,
                                    ended: endHandle, doubleTapped: { Haptics.selection(); expand(velocity: 0) })
                    .frame(height: max(44, FloatMotion.top + FloatMotion.handleHeight * layout.floatScale))
                    .offset(y: -FloatMotion.top)
                    .accessibilityElement()
                    .accessibilityLabel("拖动键盘")
                    .accessibilityHint("左右拖动移动键盘，向下拖动收起，双击展开到全屏")
                    .accessibilityAction(named: "收起键盘") { Haptics.selection(); model.closeKeyboard() }
                    .accessibilityAction(named: "展开到全屏") { Haptics.selection(); expand(velocity: 0) }
            }
        }
        .offset(x: box.minX, y: box.minY)
    }

    // MARK: Layouts

    /// Floating: the handle strip, then the control keys (rows 32) and the five key rows.
    private var floatingLayout: some View {
        InPageKeys(model: model, palette: oink, layout: "floating-\(layout.floatScale)") {
            LandscapeEmulatedKeyboard(model: model)
                .padding(.horizontal, 6)
                .padding(.bottom, 8)
                .modifier(KeyboardMagnification(scale: layout.floatScale))
        }
        .padding(.top, FloatMotion.handleHeight * layout.floatScale)
    }

    /// Expanded: the whole screen (8 above, safe area at both sides, home indicator + 4 below).
    private func fullLayout(top: CGFloat) -> some View {
        let p = layout.expandedPadding
        return InPageKeys(model: model, palette: oink, layout: "expanded") {
            ExpandedKeyboard(model: model, onRestore: { restore() },
                             onAudioError: { [toasts] text in toasts.show(text) })
                .modifier(KeyboardMagnification(scale: FloatMotion.keyboardScale))
        }
        .padding(EdgeInsets(top: top, leading: p.leading, bottom: p.bottom, trailing: p.trailing))
    }

    private var handle: some View {
        Capsule().fill(oink.handle)
            .overlay { Capsule().fill(.black.opacity(handlePressed ? 0.3 : 0)) }
            .frame(width: 40 * layout.floatScale, height: 5 * layout.floatScale)
            .frame(maxWidth: .infinity)
            .frame(height: FloatMotion.handleHeight * layout.floatScale)
            .allowsHitTesting(false)

    }

    // MARK: Handle gestures

    private func moveHandle(_ translation: CGSize) {
        let bounds = layout.floatBounds
        if drag == nil {
            drag = Drag(startX: min(bounds.hi, max(bounds.lo, x ?? bounds.home(islandRight: islandRight))))
        }
        guard var d = drag else { return }
        let t = translation
        if d.axis == nil {
            guard hypot(t.width, t.height) >= FloatMotion.slop else { return }
            d.axis = abs(t.height) > abs(t.width) ? .vertical : .horizontal
            drag = d
        }
        var instant = Transaction()
        instant.disablesAnimations = true
        withTransaction(instant) {
            if d.axis == .horizontal {
                x = min(bounds.hi, max(bounds.lo, d.startX + t.width))
            } else if t.height >= 0 {
                down = t.height
            } else {
                down = 0
            }
        }
    }

    private func endHandle(_ velocity: CGSize) {
        guard let d = drag else { return }
        drag = nil
        let v = velocity
        switch d.axis {
        case .horizontal:
            let from = x ?? d.startX
            let target = layout.floatBounds.rest(from, velocity: v.width)
            withAnimation(reduceMotion ? nil : floatSpring(velocity: v.width, distance: target - from)) { x = target }
        case .vertical:
            if down > 0 {
                if FloatMotion.dismisses(down: down, velocity: v.height) {
                    withAnimation(reduceMotion ? nil : .easeIn(duration: FloatMotion.dismissFinish)) { finish = 1 }
                    Task { @MainActor in
                        try? await Task.sleep(for: .seconds(FloatMotion.dismissFinish))
                        model.closeKeyboard()
                    }
                } else {
                    withAnimation(reduceMotion ? nil : floatSpring(velocity: v.height, distance: -down)) { down = 0 }
                }
            }
        case nil:
            break
        }
    }

    /// To the whole screen: the container springs there (`qVelocity` in q per second), then the
    /// full-screen layout replaces the icon (160 ms).
    private func expand(velocity qVelocity: CGFloat) {
        withAnimation(reduceMotion ? nil : floatSpring(velocity: qVelocity, distance: 1 - q)) { q = 1 }
        Task { @MainActor in
            try? await Task.sleep(for: .seconds(reduceMotion ? 0 : FloatMotion.response))
            var instant = Transaction()
            instant.disablesAnimations = true
            withTransaction(instant) {
                q = 0
                fullFade = 0
                model.setKeyboardExpanded(true)
            }
            withAnimation(reduceMotion ? nil : .linear(duration: FloatMotion.expandFade)) { fullFade = 1 }
        }
    }

    /// 还原 (expanded keyboard): the floating layout fades in at the whole screen's size (120 ms),
    /// then springs back to the floating container.
    private func restore() {
        var instant = Transaction()
        instant.disablesAnimations = true
        withTransaction(instant) {
            restoring = true
            q = 1
            restoreFade = 0
            model.setKeyboardExpanded(false)
        }
        withAnimation(reduceMotion ? nil : .linear(duration: FloatMotion.restoreFade)) { restoreFade = 1 }
        Task { @MainActor in
            try? await Task.sleep(for: .seconds(reduceMotion ? 0 : FloatMotion.restoreFade))
            withAnimation(reduceMotion ? nil : floatSpring(distance: -1)) { q = 0 }
            restoring = false
        }
    }
}

/// Scale the complete key layout, including faces, key corners and gaps. Laying it out first in
/// inverse-sized coordinates lets flexible rows fit the existing screen height. Geometry readers
/// in the keys still report window coordinates, so the UIKit slide layer uses the scaled hit areas.
private struct KeyboardMagnification: ViewModifier {
    let scale: CGFloat
    func body(content: Content) -> some View {
        GeometryReader { geo in
            content
                .frame(width: geo.size.width / max(0.01, scale), height: geo.size.height / max(0.01, scale))
                .scaleEffect(scale, anchor: .topLeading)
        }
    }
}

/// UIKit separates a stationary double tap from a pan. SwiftUI's minimumDistance: 0 drag wins
/// over the simultaneous double tap on iOS 27 (simulator), even though both gestures are attached
/// simultaneously. Raw touch callbacks provide immediate pressed feedback without a competing
/// zero-distance recognizer; a pan keeps that feedback until release / cancellation.
private struct KeyboardHandleInput: UIViewRepresentable {
    let pressed: (Bool) -> Void
    let changed: (CGSize) -> Void
    let ended: (CGSize) -> Void
    let doubleTapped: () -> Void

    func makeUIView(context: Context) -> HandleView { HandleView() }
    func updateUIView(_ view: HandleView, context: Context) {
        view.input = self
    }

    final class HandleView: UIView {
        var input: KeyboardHandleInput?
        override init(frame: CGRect) {
            super.init(frame: frame)
            let tap = UITapGestureRecognizer(target: self, action: #selector(doubleTap))
            tap.numberOfTapsRequired = 2
            tap.delaysTouchesBegan = false
            tap.cancelsTouchesInView = false
            let pan = UIPanGestureRecognizer(target: self, action: #selector(panned(_:)))
            pan.cancelsTouchesInView = false
            addGestureRecognizer(tap)
            addGestureRecognizer(pan)
        }
        required init?(coder: NSCoder) { fatalError() }
        override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) { ControlHaptics.tap(); input?.pressed(true) }
        override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) { input?.pressed(false) }
        override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) { input?.pressed(false) }
        @objc private func doubleTap() { input?.doubleTapped() }
        @objc private func panned(_ pan: UIPanGestureRecognizer) {
            switch pan.state {
            case .began, .changed:
                input?.pressed(true)
                let p = pan.translation(in: window)
                input?.changed(CGSize(width: p.x, height: p.y))
            case .ended, .cancelled, .failed:
                input?.pressed(false)
                let v = pan.state == .ended ? pan.velocity(in: window) : .zero
                input?.ended(CGSize(width: v.x, height: v.y))
            default: break
            }
        }
    }
}

/// Explicit intended hit regions in the floating bar's local coordinate space. The UIKit gate
/// reads these synchronously; reporting geometry never triggers a SwiftUI redraw.
final class ControlBarHitAreas {
    var frames: [String: CGRect] = [:]
}
private struct ControlBarHitAreasKey: EnvironmentKey {
    static let defaultValue: ControlBarHitAreas? = nil
}
extension EnvironmentValues {
    var controlBarHitAreas: ControlBarHitAreas? {
        get { self[ControlBarHitAreasKey.self] }
        set { self[ControlBarHitAreasKey.self] = newValue }
    }
}
private struct ControlBarHitArea: ViewModifier {
    @Environment(\.controlBarHitAreas) private var areas
    let key: String
    func body(content: Content) -> some View {
        content.onGeometryChange(for: CGRect.self) { $0.frame(in: .named("control-floating-bar")) } action: {
            areas?.frames[key] = $0
        }
    }
}
extension View {
    func controlBarHitArea(_ key: String) -> some View { modifier(ControlBarHitArea(key: key)) }
}
