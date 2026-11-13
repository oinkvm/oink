import SwiftUI
import UIKit

// The touchpad slab (§4.0), its gestures (§4.1) and edge lock (§4.2). Touches are not drawn
// (user 2026-11-09: no finger dot and no dashed ring for the locked finger); the edge glow,
// lock badge with its progress ring, the haptic, the 释放 capsule and the click ripples stay.

struct Ripple: Identifiable, Equatable {
    let id = UUID()
    var kind: ClickKind
    var at: CGPoint
}

/// Runs the recognizer on UIKit touches and turns its outputs into control calls and visuals.
@MainActor
final class TrackpadDriver: ObservableObject {
    #if DEBUG
    @Published private(set) var sceneTouchCount = 0
    #endif
    @Published private(set) var press: CGPoint?
    @Published private(set) var edge: EdgeLock?
    @Published private(set) var locked = false
    @Published private(set) var ripples: [Ripple] = []

    private var recognizer = TrackpadRecognizer(slop: CGFloat(1.5 * ControlModel.pointsPerMm))
    weak var model: ControlModel?
    /// Every finger on the pad (pad points), reported to the core after each touch event.
    private var fingers: [Int: CGPoint] = [:]
    /// The contacts keep the frame and pad size of the touch's start: a layout change mid-touch
    /// (which moves or resizes the pad) must not move them on the target.
    private var contactShift = CGVector.zero
    private var contactSize: CGSize = .zero
    private var size: CGSize = .zero
    private var dragTravel = DragTickTravel()

    func configure(size: CGSize, oneFingerScroll: Bool) {
        self.size = size
        recognizer.padSize = size
        recognizer.oneFingerScroll = oneFingerScroll
    }

    // Each touch event: the frame goes to the core first (contacts and its multi-finger
    // recognizer), the recognizer here learns the core's claim, and the core's outputs go out
    // after this recognizer's (a held-back right click comes before a scroll's Began).
    func began(_ id: Int, _ p: CGPoint, _ t: TimeInterval) {
        #if DEBUG
        if DesignScene.hasPrefix("control") { sceneTouchCount += 1 }
        #endif
        if fingers.isEmpty {
            dragTravel.reset()
            contactShift = .zero
            contactSize = size
        }
        fingers[id] = p
        let core = report(t)
        handle(recognizer.began(id, at: p, time: t, core: core.claim))
        core.forward()
    }
    /// Every finger that moved in one touch event, at the event's time.
    func moved(_ points: [Int: CGPoint], _ t: TimeInterval) {
        for (id, p) in points where fingers[id] != nil { fingers[id] = p }
        let core = report(t)
        handle(recognizer.moved(points, time: t, core: core.claim))
        core.forward()
    }
    func ended(_ id: Int, _ p: CGPoint, _ t: TimeInterval) {
        fingers[id] = nil
        let core = report(t)
        handle(recognizer.ended(id, at: p, time: t, core: core.claim))
        core.forward()
    }
    func cancelled(_ t: TimeInterval) {
        fingers = [:]
        let core = report(t, cancelled: true)
        handle(recognizer.cancelled(at: t))
        core.forward()
    }

    /// The pad moved under the fingers by `d` during a touch (layout change): no finger moved, so
    /// nothing may move on the computer — the recognizer and the finger list are rebased.
    func layoutShifted(by d: CGVector) {
        guard !fingers.isEmpty, d != .zero else { return }
        fingers = fingers.mapValues { CGPoint(x: $0.x + d.dx, y: $0.y + d.dy) }
        contactShift.dx += d.dx
        contactShift.dy += d.dy
        recognizer.shift(by: d)
    }

    /// The fingers as the trackpad's raw contacts (a new touch also stops the core's glide), in the
    /// touch's own frame, and through the core's multi-finger recognizer.
    private func report(_ t: TimeInterval, cancelled: Bool = false) -> PadCoreFrame {
        let frame = fingers.mapValues { CGPoint(x: $0.x - contactShift.dx, y: $0.y - contactShift.dy) }
        return model?.touches(frame, padSize: contactSize == .zero ? size : contactSize, at: t, cancelled: cancelled,
                              multiFinger: recognizer.coreMayRecognize(fingers: fingers.count)) ?? .unclaimed
    }

    /// 释放 capsule: the locked finger lifts and the left button goes up.
    func release() { handle(recognizer.releaseLock()) }

    /// The page left the screen: whatever the pad holds down goes up, the visuals clear.
    func reset() {
        let now = ProcessInfo.processInfo.systemUptime
        if !fingers.isEmpty {
            fingers = [:]
            report(now, cancelled: true).forward()
        }
        handle(recognizer.reset(at: now))
        press = nil; edge = nil; locked = false
        model?.touchpadActivity(busy: false)
    }

    private func handle(_ outputs: [TrackpadOutput]) {
        for output in outputs {
            switch output {
            case .move(let d, let at, let since):
                model?.pointerMove(d, at: at, since: since)
                if recognizer.leftDown {
                    // A fast sample may cross several detents; UIKit coalesces feedback in the
                    // same event, while the remainder still tracks the exact travelled distance.
                    if dragTravel.move(d) > 0 { ControlHaptics.tick() }
                } else { dragTravel.reset() }
            case .scroll(let d, let phase, let at, let since): model?.scroll(d, phase: phase, at: at, since: since)
            case .gesture(let gesture): model?.gesture(gesture)
            case .button(let button, let down):
                model?.button(button == .left ? .left : .right, down: down)
            case .tapFeedback: ControlHaptics.tap()
            case .ripple(let kind, let at):
                let ripple = Ripple(kind: kind, at: at)
                ripples.append(ripple)
                Task { @MainActor [weak self] in
                    try? await Task.sleep(for: .milliseconds(700))
                    self?.ripples.removeAll { $0.id == ripple.id }
                }
                UIAccessibility.post(notification: .announcement, argument: kind == .single ? "单击" : kind == .double ? "双击" : "右键")
            case .startTapWindow:
                Task { @MainActor [weak self] in
                    try? await Task.sleep(for: .seconds(TrackpadRecognizer.dragWindow))
                    guard let self else { return }
                    self.handle(self.recognizer.tapWindowEnded(at: ProcessInfo.processInfo.systemUptime))
                }
            case .startRightTapWindow:
                Task { @MainActor [weak self] in
                    try? await Task.sleep(for: .seconds(TrackpadRecognizer.twoFingerDoubleTapTime))
                    guard let self else { return }
                    self.handle(self.recognizer.rightTapWindowEnded(at: ProcessInfo.processInfo.systemUptime))
                }
            case .press(let p):
                press = p
            case .edge(let e):
                edge = e
            case .edgeFull:
                ControlHaptics.tap()
            case .locked:
                locked = true
                UIAccessibility.post(notification: .announcement, argument: "手指已锁定")
            case .unlocked:
                locked = false
            }
        }
        // Touchpad in use (a finger down, the locked drag held): text-focus keyboard changes wait.
        model?.touchpadActivity(busy: !fingers.isEmpty || locked, touching: !fingers.isEmpty)
    }

    #if DEBUG
    func installScene(press: CGPoint?, edge: EdgeLock?, locked: Bool, ripple: Ripple? = nil) {
        self.press = press; self.edge = edge; self.locked = locked
        ripples = ripple.map { [$0] } ?? []
    }
    #endif
}

/// The pad: slab, hint, edge-lock visuals, ripples and the release capsule, over a UIKit
/// multi-touch surface. The glow is not clipped so it can reach past the pad's edge.
struct TrackpadArea: View {
    @Environment(\.oink) private var oink
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @ObservedObject var model: ControlModel
    /// The slab's corner radius: 32, or 0 when it covers the whole landscape screen (the screen's
    /// own corners round it, user 2026-11-12).
    var cornerRadius: CGFloat = 32
    /// The release capsule's distance from the pad's bottom edge (over the home indicator when the
    /// pad reaches the screen bottom).
    var releaseBottom: CGFloat = 14
    @StateObject private var driver = TrackpadDriver()
    @State private var size: CGSize = .zero

    var body: some View {
        ZStack(alignment: .topLeading) {
            PadSlab(press: driver.press, radius: cornerRadius)
            PadHint(hidden: model.keyboardOpen)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            TouchSurface(driver: driver)
                .accessibilityLabel("触摸板")
                #if DEBUG
                .accessibilityValue(DesignScene.hasPrefix("control") ? String(driver.sceneTouchCount) : "")
                #endif
                .accessibilityAddTraits(.allowsDirectInteraction)
            ForEach(driver.ripples) { RippleView(ripple: $0).position($0.at) }
            if let edge = driver.edge {
                EdgeGlow(edge: edge).position(edge.center)
                LockBadge(progress: edge.progress).position(edge.badge)
            }
            if driver.locked {
                ReleaseCapsule { Haptics.selection(); driver.release() }
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .bottom)
                    .padding(.bottom, releaseBottom)
                    .transition(reduceMotion ? .identity : .opacity.combined(with: .offset(y: 8)))
            }
        }
        .animation(reduceMotion ? nil : .easeOut(duration: 0.2), value: driver.locked)
        .onGeometryChange(for: CGSize.self) { $0.size } action: { size = $0; configure() }
        .onChange(of: model.oneFingerSwipe) { _, _ in configure() }
        .onAppear {
            driver.model = model
            model.releasePad = { [weak driver] in driver?.reset() }
            configure()
            #if DEBUG
            ControlScenes.installPad(driver, size: size)
            #endif
        }
        #if DEBUG
        .onChange(of: size) { _, value in ControlScenes.installPad(driver, size: value) }
        #endif
    }

    private func configure() { driver.configure(size: size, oneFingerScroll: model.oneFingerSwipe == .scroll) }
}

/// UIKit multi-touch capture: every touch with a stable id, in pad points.
private struct TouchSurface: UIViewRepresentable {
    let driver: TrackpadDriver
    func makeUIView(context: Context) -> TouchView {
        let view = TouchView()
        view.driver = driver
        return view
    }
    func updateUIView(_ view: TouchView, context: Context) {
        view.driver = driver
    }

    final class TouchView: UIView {
        weak var driver: TrackpadDriver?
        private var ids: [ObjectIdentifier: Int] = [:]
        private var nextID = 0
        /// The pad's origin in the window at the last touch event. `location(in: self)` is measured
        /// from the pad's current frame, so a layout change during a touch (keyboard, rotation, the
        /// tab bar) makes resting fingers appear to jump; the driver rebases instead.
        private var origin: CGPoint?

        private func followLayout() {
            let now = convert(CGPoint.zero, to: nil)
            if let before = origin, before != now {
                driver?.layoutShifted(by: CGVector(dx: before.x - now.x, dy: before.y - now.y))
            }
            origin = now
        }
        override init(frame: CGRect) {
            super.init(frame: frame)
            isMultipleTouchEnabled = true
            backgroundColor = .clear
        }
        required init?(coder: NSCoder) { fatalError() }

        private func id(_ touch: UITouch) -> Int {
            if let id = ids[ObjectIdentifier(touch)] { return id }
            nextID += 1
            ids[ObjectIdentifier(touch)] = nextID
            return nextID
        }
        override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
            if driver?.model?.keyboardOpen == false && !isFirstResponder { becomeFirstResponder() }
            followLayout()
            for t in touches { driver?.began(id(t), t.location(in: self), t.timestamp) }
        }
        override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
            followLayout()
            // One call per event with all its fingers: the pointer moves once per event.
            var points: [Int: CGPoint] = [:]
            for t in touches { points[id(t)] = t.location(in: self) }
            guard let time = touches.map(\.timestamp).max() else { return }
            driver?.moved(points, time)
        }
        override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
            followLayout()
            for t in touches {
                driver?.ended(id(t), t.location(in: self), t.timestamp)
                ids[ObjectIdentifier(t)] = nil
            }
        }
        override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
            for t in touches { ids[ObjectIdentifier(t)] = nil }
            driver?.cancelled(touches.map(\.timestamp).max() ?? ProcessInfo.processInfo.systemUptime)
        }
        // The control page is a trackpad: three fingers are its gestures, not the system's
        // undo / redo / copy / paste editing gestures (user 2026-11-12).
        override var editingInteractionConfiguration: UIEditingInteractionConfiguration { .none }
    }
}

/// Magic-Trackpad-like slab (§4.0): radial light from the upper left, fine grain, an inner top
/// highlight, a hairline edge, a faint inner shadow along the bottom, outline and drop shadow;
/// a soft light follows a pressing finger.
private struct PadSlab: View {
    @Environment(\.oink) private var oink
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    var press: CGPoint?
    var radius: CGFloat = 32
    @State private var glowAt: CGPoint = .zero

    private var shape: RoundedRectangle { RoundedRectangle(cornerRadius: radius, style: .continuous) }

    var body: some View {
        let pressed = press != nil
        ZStack {
            Canvas { ctx, size in
                // radial-gradient(120% 85% at 28% 0%): an ellipse with radii 1.2 w × 0.85 h.
                let rx = size.width * 1.2, ry = size.height * 0.85
                ctx.translateBy(x: size.width * 0.28, y: 0)
                ctx.scaleBy(x: rx / ry, y: 1)
                let gradient = Gradient(stops: [.init(color: oink.padHi, location: 0), .init(color: oink.padMid, location: 0.58),
                                                .init(color: oink.padLo, location: 1)])
                ctx.fill(Path(CGRect(x: -20000, y: -20000, width: 40000, height: 40000)),
                         with: .radialGradient(gradient, center: .zero, startRadius: 0, endRadius: ry))
            }
            Image(uiImage: PadGrain.image)
                .resizable(resizingMode: .tile)
                .blendMode(.softLight)
                .opacity(oink.padGrain)
            RadialGradient(stops: [.init(color: oink.padGlow, location: 0), .init(color: oink.padGlow.opacity(0), location: 0.7)],
                           center: .center, startRadius: 0, endRadius: 150)
                .frame(width: 300, height: 300)
                .position(glowAt)
                .opacity(pressed ? 1 : 0)
                .animation(reduceMotion ? nil : .easeOut(duration: pressed ? 0.12 : 0.3), value: pressed)
        }
        .clipShape(shape)
        .overlay {
            // Inner shadows: the top highlight and the bottom shade (deeper and with a press shade
            // while pressed). The fill is invisible; only its inner shadows show.
            shape.fill(Color.white.opacity(0.001)
                .shadow(.inner(color: oink.padHighlight, radius: 0, x: 0, y: 1))
                .shadow(.inner(color: oink.padInner, radius: pressed ? 9 : 12, x: 0, y: pressed ? -6 : -10))
                .shadow(.inner(color: pressed ? oink.padPress : .clear, radius: 5, x: 0, y: 2)))
        }
        .overlay { shape.strokeBorder(oink.padEdge, lineWidth: 1) }
        .overlay { shape.inset(by: -0.25).stroke(oink.padOutline, lineWidth: 0.5) }
        .background { shape.fill(oink.padMid).shadow(color: oink.padDrop, radius: 1.5, x: 0, y: 1) }
        .onChange(of: press) { _, p in if let p { glowAt = p } }
        .allowsHitTesting(false)
        .accessibilityHidden(true)
    }
}

/// 160 × 160 grey noise tile, made once (soft-light grain of the slab).
private enum PadGrain {
    static let image: UIImage = {
        let side = 160
        var rng = SplitMix(seed: 0x0151_4B)
        var pixels = [UInt8](repeating: 0, count: side * side)
        for i in pixels.indices {
            // Two octaves: fine noise plus a softer 2 × 2 component, centred on mid grey.
            let fine = Double(rng.next() % 256)
            let x = i % side, y = i / side
            var coarse = SplitMix(seed: UInt64((y / 2) * 97 + (x / 2)) &* 0x9E37_79B9)
            let soft = Double(coarse.next() % 256)
            pixels[i] = UInt8(max(0, min(255, 128 + (fine * 0.65 + soft * 0.35 - 128) * 0.45)))
        }
        let provider = CGDataProvider(data: Data(pixels) as CFData)!
        let cg = CGImage(width: side, height: side, bitsPerComponent: 8, bitsPerPixel: 8, bytesPerRow: side,
                         space: CGColorSpaceCreateDeviceGray(), bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.none.rawValue),
                         provider: provider, decode: nil, shouldInterpolate: false, intent: .defaultIntent)!
        return UIImage(cgImage: cg, scale: 1, orientation: .up)
    }()

    struct SplitMix {
        var state: UInt64
        init(seed: UInt64) { state = seed }
        mutating func next() -> UInt64 {
            state &+= 0x9E37_79B9_7F4A_7C15
            var z = state
            z = (z ^ (z >> 30)) &* 0xBF58_476D_1CE4_E5B9
            z = (z ^ (z >> 27)) &* 0x94D0_49BB_1331_11EB
            return z ^ (z >> 31)
        }
    }
}

/// 触摸移动 / 轻触单击 in the middle of the pad; hidden while a keyboard is up.
private struct PadHint: View {
    @Environment(\.oink) private var oink
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    var hidden: Bool
    var body: some View {
        VStack(spacing: 1) {
            Text("触摸移动")
            Text("轻触单击")
        }
        .font(.system(size: 11.5, weight: .medium))
        .tracking(0.3)
        .lineSpacing(0)
        .frame(minHeight: 31)
        .foregroundStyle(oink.padHint)
        .opacity(hidden ? 0 : 1)
        .animation(reduceMotion ? nil : hidden ? .easeIn(duration: 0.16) : .timingCurve(0.22, 1, 0.36, 1, duration: 0.24).delay(0.12),
                   value: hidden)
        .allowsHitTesting(false)
        .accessibilityHidden(true)
    }
}

/// Edge glow (§4.2): theme light centred on the edge point (or corner), growing with progress.
private struct EdgeGlow: View {
    @Environment(\.oink) private var oink
    let edge: EdgeLock
    var body: some View {
        let p = edge.progress
        let size: CGSize = switch edge.kind {
        case .sideV: CGSize(width: 28 + 56 * p, height: 150 + 110 * p)
        case .sideH: CGSize(width: 150 + 110 * p, height: 28 + 56 * p)
        case .corner: CGSize(width: 70 + 90 * p, height: 70 + 90 * p)
        }
        EllipticalGradient(stops: [.init(color: oink.accent, location: 0), .init(color: oink.accent.opacity(0.45), location: 0.45),
                                   .init(color: oink.accent.opacity(0), location: 1)])
            .frame(width: size.width, height: size.height)
            .blur(radius: 2)
            .opacity(0.2 + 0.6 * p)
            .allowsHitTesting(false)
            .accessibilityHidden(true)
    }
}

/// Lock badge: a lock inside a progress ring; solid accent once full.
private struct LockBadge: View {
    @Environment(\.oink) private var oink
    let progress: CGFloat
    var body: some View {
        let full = progress >= 1
        ZStack {
            Circle().fill(full ? oink.accent : oink.elev)
                .shadow(color: oink.shadow, radius: 4, y: 2)
            Circle().stroke(full ? .clear : oink.line, lineWidth: 3).frame(width: 30, height: 30)
            Circle().trim(from: 0, to: progress)
                .stroke(full ? oink.onAccent : oink.accent, style: StrokeStyle(lineWidth: 3, lineCap: .round))
                .rotationEffect(.degrees(-90))
                .frame(width: 30, height: 30)
            Lucide(name: "lock-keyhole", size: 14, stroke: "2_2", color: full ? oink.onAccent : oink.accentInk)
        }
        .frame(width: 36, height: 36)
        .scaleEffect(full ? 1.08 : 1)
        .animation(.easeOut(duration: 0.12), value: full)
        .allowsHitTesting(false)
        .accessibilityHidden(true)
    }
}

/// 释放 capsule at the pad's bottom centre while a finger is locked.
private struct ReleaseCapsule: View {
    @Environment(\.oink) private var oink
    let action: () -> Void
    var body: some View {
        Button(action: action) {
            HStack(spacing: 6) {
                AssetIcon("PCAction-unlock", size: 18, color: oink.accentInk)
                Text("释放").font(.oink(15, .semibold)).foregroundStyle(oink.accentInk)
            }
            .padding(.leading, 12)
            .padding(.trailing, 16)
            .frame(height: 40)
            .background(oink.elev, in: Capsule())
            .overlay(Capsule().stroke(oink.border, lineWidth: 0.5))
            .shadow(color: oink.shadow, radius: 7, y: 4)
        }
        .buttonStyle(ShrinkButtonStyle())
        .accessibilityLabel("释放：抬起锁定的手指")
    }
}

/// Click feedback: one ring = click, two rings = double click, one dashed ring = right click.
private struct RippleView: View {
    @Environment(\.oink) private var oink
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    let ripple: Ripple
    var body: some View {
        ZStack {
            Ring(dashed: ripple.kind == .right, delay: 0)
            if ripple.kind == .double { Ring(dashed: false, delay: 0.12) }
        }
        .allowsHitTesting(false)
        .accessibilityHidden(true)
    }

    private struct Ring: View {
        @Environment(\.oink) private var oink
        @Environment(\.accessibilityReduceMotion) private var reduceMotion
        let dashed: Bool
        let delay: Double
        /// 0 → 1 over the ripple: scale 0.4 → 1.25, opacity 0.7 → 0.
        @State private var t: CGFloat = 0
        var body: some View {
            Circle()
                .stroke(oink.accent, style: StrokeStyle(lineWidth: 2, dash: dashed ? [5, 4] : []))
                .frame(width: 44, height: 44)
                .scaleEffect(0.4 + 0.85 * t)
                .opacity(0.7 * (1 - t))
                .onAppear {
                    #if DEBUG
                    if ControlScenes.freezeRipples { t = delay > 0 ? 0.25 : 0.45; return }
                    #endif
                    withAnimation(reduceMotion ? nil : .timingCurve(0.22, 1, 0.36, 1, duration: 0.5).delay(delay)) { t = 1 }
                }
        }
    }
}
