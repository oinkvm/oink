import Combine
import SwiftUI
import UIKit

// Keyboards of the control page (§3.1). A hidden text field is the first responder: in portrait
// the control keys are its inputAccessoryView (both keyboards), the emulated keyboard is its
// inputView (nil = the system keyboard). Open / close and switching therefore use the
// system keyboard animation and swap in place.
//
// Landscape (§1.2, 用户 2026-11-12) puts the keyboard in one half of the screen, which an input
// view cannot do (the keyboard window always spans the full width). There the page draws the
// control keys and the emulated keyboard itself (`InPageKeys`): the field is first responder only
// for the system keyboard, without the accessory bar, and the control keys sit in the page above it.

/// The accessory bar: three control rows of 36, gap 6, padding 7 / 6.
private let accessoryHeight: CGFloat = 134

struct ControlKeyboardHost: UIViewRepresentable {
    @ObservedObject var model: ControlModel
    let palette: OinkPalette
    let landscape: Bool

    func makeUIView(context: Context) -> ControlInputField { ControlInputField(model: model, palette: palette) }
    func updateUIView(_ field: ControlInputField, context: Context) { field.sync(palette: palette, landscape: landscape) }
    /// 1 × 1 whatever it holds: the text view's own size grows with the text it keeps (up to
    /// RemoteInputField.maxField characters), and the page must not lay out or hit-test that.
    func sizeThatFits(_ proposal: ProposedViewSize, uiView: ControlInputField, context: Context) -> CGSize? {
        CGSize(width: 1, height: 1)
    }
}

/// Lays the hosted keys and the key touch layer over them into `container`.
private func embedKeys(_ host: UIHostingController<AnyView>, in container: UIView, model: ControlModel,
                       areas: @escaping () -> [CGRect]) {
    // The keyboard window's safe area must not move the keys inside their input views.
    host.safeAreaRegions = []
    host.view.backgroundColor = .clear
    // Over the keys: takes the touches that land on keys with a bubble (§3.1.4); every other
    // touch falls through to the SwiftUI keys below.
    let touches = KeyTouchView(model: model, areas: areas)
    for view: UIView in [host.view, touches] {
        view.translatesAutoresizingMaskIntoConstraints = false
        container.addSubview(view)
        NSLayoutConstraint.activate([
            view.leadingAnchor.constraint(equalTo: container.leadingAnchor),
            view.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            view.topAnchor.constraint(equalTo: container.topAnchor),
            view.bottomAnchor.constraint(equalTo: container.bottomAnchor),
        ])
    }
}

/// Keys drawn in the page (landscape, §1.2) instead of in a keyboard input view, with the same key
/// touch layer over them, so sliding between keys and the preview bubbles work as on the
/// keyboards. The keys observe the model themselves; the hosted view is rebuilt only when the
/// colours or `layout` change (rebuilding re-lays the keys out).
struct InPageKeys<Content: View>: UIViewControllerRepresentable {
    let model: ControlModel
    let palette: OinkPalette
    let layout: String
    @ViewBuilder let content: () -> Content

    final class Coordinator {
        let surface = UUID()
        weak var spots: KeySpots?
        var host: UIHostingController<AnyView>?
        var built: (dark: Bool, accent: OinkPalette.Accent, layout: String)?
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeUIViewController(context: Context) -> UIViewController {
        let controller = UIViewController()
        let container: UIView = controller.view
        container.backgroundColor = .clear
        let surface = context.coordinator.surface
        let host = UIHostingController(rootView: rooted(surface))
        controller.addChild(host)
        // Leaving the keys' area ends a finger (§3.1.4).
        embedKeys(host, in: container, model: model) { [weak container] in
            guard let container, container.window != nil else { return [] }
            return [container.convert(container.bounds, to: nil)]
        }
        host.didMove(toParent: controller)
        model.keySpots.register(surface: surface) { [weak container] in container?.window != nil }
        context.coordinator.spots = model.keySpots
        context.coordinator.host = host
        context.coordinator.built = (palette.dark, palette.accentName, layout)
        return controller
    }

    func updateUIViewController(_ controller: UIViewController, context: Context) {
        let c = context.coordinator
        guard c.built?.dark != palette.dark || c.built?.accent != palette.accentName || c.built?.layout != layout else { return }
        c.built = (palette.dark, palette.accentName, layout)
        c.host?.rootView = rooted(c.surface)
    }

    static func dismantleUIViewController(_ controller: UIViewController, coordinator: Coordinator) {
        // The model outlives the page's keys; their surface goes with them.
        coordinator.spots?.unregister(surface: coordinator.surface)
    }

    private func rooted(_ surface: UUID) -> AnyView {
        AnyView(content().environment(\.oink, palette).environment(\.keySurface, surface))
    }
}

/// The bottom safe-area inset under the portrait emulated keyboard (the home indicator), handed to
/// its SwiftUI keys as a value. Reading the window's safe area inside a SwiftUI body stops that body
/// from ever running again (ControlPage, HomeInset), so UIKit reports it from the input view itself.
final class KeyboardInset: ObservableObject {
    @Published var bottom: CGFloat = 0
}

/// The emulated keyboard's input view: reports its bottom safe-area inset when UIKit lays it out.
final class InsetInputView: UIInputView {
    var onBottomInset: ((CGFloat) -> Void)?
    private var reported: CGFloat?

    override func safeAreaInsetsDidChange() {
        super.safeAreaInsetsDidChange()
        let bottom = safeAreaInsets.bottom
        guard bottom != reported else { return }
        reported = bottom
        // Outside the layout pass that called this.
        DispatchQueue.main.async { [weak self] in self?.onBottomInset?(bottom) }
    }
}

/// Hidden field receiving the input method (§3.1.1, RemoteInputField: the committed text stays
/// in it and its changes go to the focus computer), with the control keys as its accessory bar
/// and the emulated keyboard as its input view.
final class ControlInputField: RemoteInputField {
    private let model: ControlModel
    private var palette: OinkPalette
    private let accessory: UIInputView
    private let accessoryHost: UIHostingController<AnyView>
    private var emulated: UIInputView?
    private var emulatedHost: UIHostingController<AnyView>?
    private var shownKind: KeyboardKind?
    /// The home-indicator strip under the emulated input view, from UIKit (InsetInputView).
    private let emulatedInset = KeyboardInset()
    /// KeySpots surfaces of the two input views' keys.
    private let accessorySurface = UUID()
    private let emulatedSurface = UUID()
    /// The key preview bubble (§3.1.4): over the accessory bar and the touchpad, in its own window.
    private let bubbleView = KeyBubbleView()
    /// The part of the bubble over the emulated keyboard's keys, inside it (see showBubble).
    private let innerBubble = KeyBubbleView()
    private var bubbleWatch: AnyCancellable?
    /// The emulated keyboard's keys above the home indicator (§3.1.2, user 2026-11-12): the system
    /// keyboard last seen with the accessory bar while it is a standard one, else the design's 266
    /// (EmulatedKeyboardHeight), so a switch moves nothing but a tall input-method panel does not
    /// make the emulated keyboard tall. Kept across launches for an emulated keyboard that opens
    /// before the system one has shown in this run.
    private static let keysKey = "controlEmulatedKeysHeight"
    private var keysHeight: CGFloat = {
        let stored = UserDefaults.standard.double(forKey: ControlInputField.keysKey)
        return EmulatedKeyboardHeight.keys(observed: stored > 0 ? stored : nil)
    }()
    /// The input view's height: the keys plus the home indicator strip under them down to the
    /// screen edge, which the keyboard colour fills (InsetInputView).
    private var emulatedHeight: CGFloat { keysHeight + (window?.safeAreaInsets.bottom ?? 0) }
    /// While this field types for the computer, three fingers on the touchpad are its gestures,
    /// not the system's undo / redo / copy / paste editing gestures (user 2026-11-12).
    override var editingInteractionConfiguration: UIEditingInteractionConfiguration { .none }

    init(model: ControlModel, palette: OinkPalette) {
        self.model = model
        self.palette = palette
        accessory = UIInputView(frame: CGRect(x: 0, y: 0, width: 0, height: accessoryHeight), inputViewStyle: .default)
        accessoryHost = UIHostingController(rootView: AnyView(EmptyView()))
        super.init()
        sink = model

        NotificationCenter.default.addObserver(self, selector: #selector(keyboardFrame(_:)),
                                               name: UIResponder.keyboardWillChangeFrameNotification, object: nil)
        NotificationCenter.default.addObserver(self, selector: #selector(applicationActivated),
                                               name: UIApplication.didBecomeActiveNotification, object: nil)
        accessory.allowsSelfSizing = false
        accessory.autoresizingMask = [.flexibleWidth]
        embed(accessoryHost, in: accessory)
        inputAccessoryView = accessory
        model.keySpots.register(surface: accessorySurface) { [weak self] in
            guard let self else { return false }
            return accessory.window != nil && inputAccessoryView === accessory
        }
        model.keySpots.register(surface: emulatedSurface) { [weak self] in self?.shownEmulated != nil }
        model.slide.feel = { slide in slide ? ControlHaptics.tick() : ControlHaptics.tap() }
        bubbleWatch = model.slide.$bubble.sink { [weak self] bubble in
            // @Published fires before the value is stored; show on the next turn of the run loop.
            DispatchQueue.main.async { self?.showBubble(bubble) }
        }
        sync(palette: palette, landscape: landscape)
    }
    required init?(coder: NSCoder) { fatalError() }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        guard window != nil else { return }
        // The page's keys, clicks and focus changes restart the field that is on screen.
        model.restartField = { [weak self] why in self?.restart(why) }
        sync(palette: palette, landscape: landscape)
    }

    private func embed(_ host: UIHostingController<AnyView>, in container: UIView) {
        embedKeys(host, in: container, model: model) { [weak self] in self?.keyboardAreas() ?? [] }
    }

    /// The keyboard surfaces on screen (window coordinates): leaving them ends a finger.
    private func keyboardAreas() -> [CGRect] {
        [accessory, emulated].compactMap { view -> CGRect? in
            guard let view, view.window != nil, !view.isHidden else { return nil }
            return view.convert(view.bounds, to: nil)
        }
    }

    /// Key preview (§3.1.4): on the newest finger's key, or gone. Not shown to VoiceOver users
    /// (they activate keys by double tap).
    ///
    /// The emulated keyboard (the input view) lives in UIRemoteKeyboardWindow, which is not in
    /// the app's window scene and is composited above every window the app draws in (logged in
    /// integration round 3: scene = UIWindow@0, UITextEffectsWindow@1, the bubble window@2). A bubble in the accessory bar's window (first try) and then in its own window one
    /// level above both keyboard windows (second try) were both drawn under the emulated keys —
    /// in the integration screenshots of 2026-11-11 the q bubble's head showed over the
    /// accessory bar and was cut off exactly at the emulated keyboard's top edge, g / d / space
    /// showed nothing. So the bubble is drawn in two parts, each in the surface it covers:
    /// `innerBubble` inside the emulated keyboard (clipped to it) for the part over its keys, and
    /// `bubbleView` in its own window for the part over the accessory bar and the touchpad, with
    /// the emulated keyboard's area masked out so the two never overlap. Over the system keyboard
    /// only the window part exists.
    private func showBubble(_ bubble: SlideBubble?) {
        guard let bubble, !UIAccessibility.isVoiceOverRunning,
              let spot = model.keySpots.spot(bubble.key.id), let face = spot.bubble,
              let source = keysWindow,
              let window = bubbleHost() else {
            bubbleView.hide()
            innerBubble.hide()
            return
        }
        #if DEBUG
        logWindowsOnce(window)
        #endif
        // Key frames are in the keyboard windows' coordinates (both cover the screen and agree).
        let width = window.bounds.width
        if bubbleView.superview !== window { window.addSubview(bubbleView) }
        bubbleView.frame = window.bounds
        bubbleView.exclude(emulatedArea(in: window))
        bubbleView.show(key: source.convert(spot.frame, to: window), face: face, palette: palette, glide: bubble.glide, width: width)
        if let e = shownEmulated {
            if innerBubble.superview !== e { e.addSubview(innerBubble) }
            e.bringSubviewToFront(innerBubble)
            innerBubble.frame = e.bounds
            innerBubble.clipsToBounds = true // only the part over the emulated keys
            // Draw in the keyboard window's coordinates, like the key frames.
            innerBubble.bounds.origin = e.convert(CGPoint.zero, to: nil)
            innerBubble.show(key: spot.frame, face: face, palette: palette, glide: bubble.glide, width: width)
        } else {
            innerBubble.hide()
        }
    }

    /// Where the emulated keyboard covers the screen, in `window`'s coordinates: from the
    /// accessory bar's bottom edge down. Taken from the accessory bar because the emulated
    /// keyboard's own window (UIRemoteKeyboardWindow) is not in the app's window scene, and
    /// converting its frame into the bubble window gave a wrong rect: the mask then hid the head
    /// of the q bubble over the accessory bar (integration round 3, 2026-11-11).
    private func emulatedArea(in window: UIWindow) -> CGRect? {
        guard shownEmulated != nil, let bar = accessory.window else { return nil }
        let bottom = bar.convert(accessory.convert(accessory.bounds, to: nil), to: window).maxY
        return CGRect(x: window.bounds.minX, y: bottom, width: window.bounds.width, height: max(0, window.bounds.maxY - bottom))
    }

    /// The window the keys are in (their frames are in its coordinates): the keyboard windows in
    /// portrait, the page's own window in landscape, where the page draws the keys (§1.2).
    private var keysWindow: UIWindow? {
        landscape ? window : accessory.window ?? emulated?.window
    }

    /// The emulated keyboard while it is the input view on screen.
    private var shownEmulated: UIInputView? {
        guard let emulated, inputView === emulated, emulated.window != nil else { return nil }
        return emulated
    }

    #if DEBUG
    private var windowsLogged = false
    /// Detection only: which windows the keyboards and the bubble are in, for the next screenshots.
    private func logWindowsOnce(_ bubbleWindow: UIWindow) {
        guard !windowsLogged, shownEmulated != nil else { return }
        windowsLogged = true
        let all = bubbleWindow.windowScene?.windows.map { "\(type(of: $0))@\($0.windowLevel.rawValue)" } ?? []
        NSLog("KeyBubble windows: scene=%@ accessory=%@ emulated=%@ bubble=%@", all.joined(separator: ","),
              String(describing: accessory.window.map { type(of: $0) }), String(describing: emulated?.window.map { type(of: $0) }),
              "\(bubbleWindow.windowLevel.rawValue)")
    }
    #endif

    /// The bubble's own window, above every window of the scene (the keyboard windows
    /// included); it never takes touches and is hidden from the screen reader.
    private var bubbleWindow: UIWindow?
    private func bubbleHost() -> UIWindow? {
        guard let scene = keysWindow?.windowScene else { return nil }
        let others = scene.windows.filter { $0 !== bubbleWindow }
        let window: UIWindow
        if let existing = bubbleWindow, existing.windowScene === scene {
            window = existing
        } else {
            window = PassthroughWindow(windowScene: scene)
            window.backgroundColor = .clear
            window.isUserInteractionEnabled = false
            window.accessibilityElementsHidden = true
            bubbleWindow = window
        }
        let top = others.map(\.windowLevel.rawValue).max() ?? UIWindow.Level.normal.rawValue
        window.windowLevel = UIWindow.Level(rawValue: top + 1)
        window.frame = scene.screen.bounds
        window.isHidden = false
        return window
    }

    /// What the hosted keyboards were last built for. They observe the model themselves, so
    /// they are rebuilt only when the colours or the keyboard kind change (rebuilding them on
    /// every page update re-lays the keyboard out, which updates the page again).
    private var built: (dark: Bool, accent: OinkPalette.Accent, kind: KeyboardKind)?
    /// The page is in landscape (§1.2): no input views of Oink's own; the page draws the keys.
    private var landscape = false
    private var shownLandscape: Bool?
    /// Set while the field gives up first responder because the page draws the keyboard itself,
    /// which is not the user closing the keyboard.
    private var resigningForLayout = false

    func sync(palette: OinkPalette, landscape: Bool) {
        self.palette = palette
        self.landscape = landscape
        let kind = model.shownKeyboard
        if built?.dark != palette.dark || built?.accent != palette.accentName || built?.kind != kind {
            built = (palette.dark, palette.accentName, kind)
            keyboardAppearance = palette.dark ? .dark : .light
            accessory.backgroundColor = UIColor(palette.kbBg)
            accessoryHost.rootView = AnyView(ControlKeysBar(model: model, style: .accessory).environment(\.oink, palette).environment(\.keySurface, accessorySurface))
            if kind == .emulated {
                let view = emulatedView()
                emulatedHost?.rootView = AnyView(EmulatedKeyboard(model: model, inset: emulatedInset).environment(\.oink, palette).environment(\.keySurface, emulatedSurface))
                view.backgroundColor = UIColor(palette.kbBg)
            }
        }
        guard kind != shownKind || landscape != shownLandscape || wantsResponder != isFirstResponder else { return }
        // Responder and input-view changes post keyboard notifications synchronously, which
        // update the model; they run after the current SwiftUI update, not inside it.
        DispatchQueue.main.async { [weak self] in self?.applyKeyboard(kind) }
    }

    /// First responder: the keyboard is open and it is a keyboard window's (not the landscape
    /// emulated keyboard, which the page draws).
    private var wantsResponder: Bool {
        model.keyboardOpen && !(landscape && model.shownKeyboard == .emulated)
    }

    private func applyKeyboard(_ kind: KeyboardKind) {
        let previous = shownKind
        let previousLandscape = shownLandscape
        shownKind = kind
        shownLandscape = landscape
        let wanted: UIView? = kind == .emulated && !landscape ? emulatedView() : nil
        let wantedAccessory: UIView? = landscape ? nil : accessory
        // The keys under the fingers change with the keyboard or the layout: every finger is cancelled.
        if (previous != nil && previous != kind) || (previousLandscape != nil && previousLandscape != landscape) {
            model.slide.reset()
        }
        var reload = false
        if inputAccessoryView !== wantedAccessory {
            inputAccessoryView = wantedAccessory
            reload = true
        }
        let swapped = inputView !== wanted
        if swapped {
            inputView = wanted
            reload = true
        }
        if reload && isFirstResponder {
            reloadInputViews()
            // Switching in place: the part below the control keys fades in (120 ms).
            if swapped, previous != nil, previousLandscape == landscape, let wanted, !UIAccessibility.isReduceMotionEnabled {
                wanted.alpha = 0
                UIView.animate(withDuration: 0.12, delay: 0, options: .curveEaseOut) { wanted.alpha = 1 }
            }
        }
        if wantsResponder && !isFirstResponder && window?.windowScene?.activationState == .foregroundActive {
            becomeFirstResponder()
        } else if !wantsResponder && isFirstResponder {
            resigningForLayout = model.keyboardOpen
            resignFirstResponder()
            resigningForLayout = false
        }
        // No keyboard, no bubble: its window goes away until the next one shows.
        if !model.keyboardOpen { bubbleWindow?.isHidden = true }
    }

    private func emulatedView() -> UIInputView {
        if let emulated {
            if abs(emulated.frame.height - emulatedHeight) > 0.5 { emulated.frame.size.height = emulatedHeight }
            return emulated
        }
        let view = InsetInputView(frame: CGRect(x: 0, y: 0, width: 0, height: emulatedHeight), inputViewStyle: .default)
        view.onBottomInset = { [weak self] bottom in self?.emulatedInset.bottom = bottom }
        view.allowsSelfSizing = false
        view.autoresizingMask = [.flexibleWidth]
        let host = UIHostingController(rootView: AnyView(EmptyView()))
        embed(host, in: view)
        emulated = view
        emulatedHost = host
        return view
    }

    @objc private func keyboardFrame(_ note: Notification) {
        guard let end = note.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? CGRect, let window else { return }
        let screen = window.screen.bounds
        // A floating / split keyboard (iPad) does not reach the bottom: its height is not one to copy.
        let docked = end.maxY >= screen.maxY - 1 && end.minY < screen.maxY
        // Only the portrait system keyboard with the accessory bar counts: the emulated input view
        // exists in portrait only (the landscape keyboards are drawn in the page,
        // ControlLandscapeLayout).
        guard isFirstResponder, inputView == nil, inputAccessoryView === accessory, !landscape,
              screen.width < screen.height, docked else { return }
        let observed = end.height - accessoryHeight - window.safeAreaInsets.bottom
        guard observed > 120 else { return }
        keysHeight = EmulatedKeyboardHeight.keys(observed: observed)
        UserDefaults.standard.set(Double(keysHeight), forKey: Self.keysKey)
    }

    // A hardware keyboard's keys reach the focus computer as key codes through GameController
    // (ControlModel); the field swallows them so they are not also typed as text.
    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        if presses.allSatisfy({ $0.key == nil }) { super.pressesBegan(presses, with: event) }
    }
    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        if presses.allSatisfy({ $0.key == nil }) { super.pressesEnded(presses, with: event) }
    }
    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        if presses.allSatisfy({ $0.key == nil }) { super.pressesCancelled(presses, with: event) }
    }

    @objc private func applicationActivated() {
        sync(palette: palette, landscape: landscape)
    }

    override func textViewDidEndEditing(_ textView: UITextView) {
        if resigningForLayout || !model.keyboardOpen {
            super.textViewDidEndEditing(textView)
            return
        }
        // Dictation's system prompt can temporarily end editing as the scene becomes inactive.
        // Decide after that event's lifecycle notifications, rather than closing the input session
        // synchronously and tearing down the very prompt that owns it. Active user dismissals
        // still close normally; foreground reactivation restores a preserved session above.
        DispatchQueue.main.async { [weak self] in
            guard let self, !isFirstResponder,
                  window?.windowScene?.activationState == .foregroundActive,
                  model.keyboardOpen else { return }
            finishSystemDismissal()
        }
    }

    private func finishSystemDismissal() {
        // Restarting the mirror also clears composition, so do it only for a real dismissal,
        // never while the system is borrowing focus for dictation's prompt.
        super.textViewDidEndEditing(self)
        model.closeKeyboard()
    }
}

// MARK: Key caps

private enum KeyFace { case character, function }

/// How a key reacts to the finger (§3.1 按键发送时机).
private enum KeyAction {
    /// Down + up on release while the finger is still on the key; sliding off cancels.
    case click(() -> Void)
    /// Down when the finger lands, up when it lifts or slides off.
    case hold(down: () -> Void, up: () -> Void)
    /// Modifiers (user 2026-11-12): lifting on the key is a tap, or a long press (lock / unlock)
    /// once the finger stayed `ModifierBoard.longPress` (felt as a tick at that moment). Both act
    /// on lift: locking over the system keyboard swaps the input view in place, and doing that
    /// under a finger still on the key could take the touch away mid-gesture.
    case toggle(tap: () -> Void, long: () -> Void)
}

/// UIKit feedback generators defer to the system Haptics setting. Do not replace these with
/// AudioServices vibration or a custom Core Haptics engine, which have different user policies.
enum ControlHaptics {
    private static let key = UIImpactFeedbackGenerator(style: .light)
    private static let selection = UISelectionFeedbackGenerator()
    static func tap() { key.impactOccurred() }
    static func tick() { selection.selectionChanged() }
}

/// A key's preview bubble (§3.1.4): character keys get the tall head (glyph 40), the others the
/// short one (text 22 / 500, icons 28); `content` is what the head shows, in the key ink.
struct BubbleFace {
    let character: Bool
    let content: AnyView
}

/// A key on screen for the slide tracker: its frame (window coordinates) and, for keys with a
/// bubble, what it does and shows.
struct KeySpot {
    var frame: CGRect
    var slide: SlideKey?
    var bubble: BubbleFace?
}

/// Keys of both keyboards by view instance. Plain storage, not observed: the touch view reads it
/// when a finger moves, the keys write it when they lay out or change.
///
/// Each key belongs to a surface (the accessory bar, the emulated input view, the keys the page
/// draws in landscape). Only keys of surfaces on screen count: the same keys exist in portrait and
/// landscape, and a surface off screen keeps its last frames, which in the other orientation can
/// lie under a finger on the keys that are shown.
final class KeySpots {
    private var spots: [UUID: KeySpot] = [:]
    private var surfaceOf: [UUID: UUID] = [:]
    private var surfaces: [UUID: () -> Bool] = [:]

    func set(_ token: UUID, frame: CGRect, surface: UUID?) {
        spots[token, default: KeySpot(frame: frame)].frame = frame
        surfaceOf[token] = surface
    }
    func set(_ token: UUID, slide: SlideKey?, bubble: BubbleFace?) {
        spots[token, default: KeySpot(frame: .null)].slide = slide
        spots[token]?.bubble = bubble
    }
    func remove(_ token: UUID) { spots[token] = nil; surfaceOf[token] = nil }

    /// A surface and whether it is on screen now.
    func register(surface: UUID, onScreen: @escaping () -> Bool) { surfaces[surface] = onScreen }
    func unregister(surface: UUID) { surfaces[surface] = nil }

    private var shown: [KeySpot] {
        spots.compactMap { token, spot in
            guard !spot.frame.isNull else { return nil }
            if let surface = surfaceOf[token], !(surfaces[surface]?() ?? false) { return nil }
            return spot
        }
    }
    var all: [(CGRect, SlideKey?)] { shown.map { ($0.frame, $0.slide) } }
    func spot(_ id: String) -> KeySpot? { shown.first { $0.slide?.id == id } }
    /// The key with a bubble right under a point (a finger landing).
    func key(at p: CGPoint) -> SlideKey? { shown.first { $0.slide != nil && $0.frame.contains(p) }?.slide }
}

private struct KeySurfaceKey: EnvironmentKey { static let defaultValue: UUID? = nil }

extension EnvironmentValues {
    /// The key surface the keys below belong to (KeySpots).
    var keySurface: UUID? {
        get { self[KeySurfaceKey.self] }
        set { self[KeySurfaceKey.self] = newValue }
    }
}

/// What a key with a bubble needs from its keyboard: its slide id, the bubble face, whether a
/// finger is on it, and where to register.
private struct SlideSpec {
    let id: String
    let face: BubbleFace
    let down: Bool
    let spots: KeySpots
}

/// One keycap: key colours, press feedback (the two key colours swap for 80 ms), the held /
/// locked modifier styles.
///
/// A key with `slide` (§3.1.4, user 2026-11-11) has no touch handling of its own: the
/// keyboard's touch view and slide tracker press it, show its bubble and let fingers slide
/// between such keys; it is drawn pressed while a finger is on it. Keys without one (modifiers,
/// Fn, the switch key, 123 / ABC) stay plain buttons: sliding off cancels a click key and lifts
/// a held key. Every key registers its frame, so the tracker knows the keys without a bubble too.
private struct KeyCap<Label: View>: View {
    @Environment(\.keySurface) private var surface
    @Environment(\.oink) private var oink
    var face: KeyFace = .function
    var state: ModifierState? = nil
    var radius: CGFloat = 6
    let action: KeyAction
    let accessibility: String
    var slide: SlideSpec? = nil
    var spots: KeySpots? = nil
    @ViewBuilder var label: () -> Label
    @State private var touched = false
    @State private var began = false
    @State private var pressedAt: Date?
    @State private var longTick: Task<Void, Never>?
    @State private var size: CGSize = .zero
    @State private var token = UUID()

    var body: some View {
        let pressed = slide?.down ?? touched
        let base = face == .character ? oink.kbKey : oink.kbKey2
        let pressedBG = face == .character ? oink.kbKey2 : oink.kbKey
        let bg: Color = switch state {
        case .held: oink.accentSoft2
        case .locked: oink.accent
        case nil: pressed ? pressedBG : base
        }
        let ink: Color = switch state {
        case .held: oink.accentInk
        case .locked: oink.onAccent
        case nil: oink.kbInk
        }
        let registry = slide?.spots ?? spots
        // Keep the tracker's copy of what this key does current (layers swap keys in place).
        let _ = registry?.set(token, slide: slideKey, bubble: slide?.face)
        let cap = ZStack {
            RoundedRectangle(cornerRadius: radius, style: .continuous).fill(bg)
                .shadow(color: state == .locked ? .clear : oink.kbShadow, radius: 0, x: 0, y: 1)
            if state == .held {
                RoundedRectangle(cornerRadius: radius, style: .continuous).strokeBorder(oink.accent, lineWidth: 1.5)
            }
            label().foregroundStyle(ink)
            if state == .locked {
                RoundedRectangle(cornerRadius: 1).fill(ink).frame(width: 12, height: 2)
                    .frame(maxHeight: .infinity, alignment: .bottom).padding(.bottom, 4)
            }
        }
        .animation(.linear(duration: 0.08), value: pressed)
        .contentShape(Rectangle())
        .onGeometryChange(for: CGRect.self) { $0.frame(in: .global) } action: { frame in
            size = frame.size
            registry?.set(token, frame: frame, surface: surface)
        }
        .onDisappear { registry?.remove(token) }
        return Group {
            if slide != nil {
                cap
            } else {
                cap.gesture(buttonGesture)
            }
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(state.map { accessibility + ($0 == .held ? "（按住）" : "（锁定）") } ?? accessibility)
        .accessibilityAddTraits(state != nil ? [.isButton, .isSelected] : .isButton)
        .accessibilityAction {
            ControlHaptics.tap()
            switch action {
            case .click(let fire): fire()
            case .hold(let down, let up): down(); up()
            case .toggle(let tap, _): tap()
            }
        }
        .accessibilityActions {
            // The screen reader cannot long press a key: (un)locking a modifier is a named action.
            if let long = longAction { Button(state == .locked ? "解锁" : "锁定") { ControlHaptics.tick(); long() } }
        }
    }

    private var longAction: (() -> Void)? {
        if case .toggle(_, let long) = action { return long }
        return nil
    }

    private var slideKey: SlideKey? {
        guard let slide else { return nil }
        switch action {
        case .click(let fire): return SlideKey(id: slide.id, hold: false, fire: fire)
        case .hold(let down, let up): return SlideKey(id: slide.id, hold: true, down: down, up: up)
        case .toggle(let tap, _): return SlideKey(id: slide.id, hold: false, fire: tap)
        }
    }

    private var buttonGesture: some Gesture {
        DragGesture(minimumDistance: 0, coordinateSpace: .local)
            .onChanged { value in
                if !began {
                    began = true
                    touched = true
                    pressedAt = value.time
                    ControlHaptics.tap()
                    switch action {
                    case .hold(let down, _): down()
                    case .toggle:
                        // The tick that tells the finger a lift now locks.
                        longTick = Task { @MainActor in
                            try? await Task.sleep(for: .seconds(ModifierBoard.longPress))
                            if !Task.isCancelled && touched { ControlHaptics.tap() }
                        }
                    case .click: break
                    }
                    return
                }
                guard touched else { return }
                let inside = CGRect(origin: .zero, size: size).insetBy(dx: -4, dy: -4).contains(value.location)
                if !inside {
                    // Slid off: a held key goes up now; a click key or a modifier is cancelled.
                    touched = false
                    longTick?.cancel()
                    if case .hold(_, let up) = action { up() }
                }
            }
            .onEnded { value in
                defer { began = false; touched = false; pressedAt = nil; longTick?.cancel(); longTick = nil }
                guard touched else { return }
                switch action {
                case .click(let fire): fire()
                case .hold(_, let up): up()
                case .toggle(let tap, let long):
                    let held = pressedAt.map { value.time.timeIntervalSince($0) } ?? 0
                    if held >= ModifierBoard.longPress { long() } else { tap() }
                }
            }
    }
}

private struct KeyText: View {
    let text: String
    var size: CGFloat = 13
    var body: some View { Text(text).font(.system(size: size)).lineLimit(1).minimumScaleFactor(0.7) }
}

/// Bubble faces (§3.1.4): text 22 weight 500, icons 28, the Mac ⌦ 30, characters 40.
private enum Faces {
    static func text(_ t: String, size: CGFloat = 22) -> BubbleFace {
        BubbleFace(character: false, content: AnyView(Text(t).font(.system(size: size, weight: .medium)).lineLimit(1)))
    }
    static func lucide(_ name: String) -> BubbleFace { BubbleFace(character: false, content: AnyView(Lucide(name: name, size: 28))) }
    static func asset(_ name: String, size: CGFloat = 28) -> BubbleFace { BubbleFace(character: false, content: AnyView(AssetIcon(name, size: size))) }
    static func character(_ t: String) -> BubbleFace {
        BubbleFace(character: true, content: AnyView(Text(t).font(.system(size: 40)).lineLimit(1)))
    }
}

// MARK: Control keys

/// Where the control keys are (§3.1, §1.2).
enum ControlBarStyle: Equatable {
    /// Portrait: the accessory bar over either keyboard — rows 36, gap 6, padding 7 / 6. Right of
    /// ↑ is 收起键盘; 切换键盘 is in the top bar's top-right (用户 2026-11-12).
    case accessory
    /// Landscape, at the top of the floating Oink keyboard: equal-height rows, gap 5, padding 0 / 3, text 12.
    /// Right of ↑ is 切换键盘 (the top bar hides while a keyboard is up; 收起 is the handle's
    /// downward drag, user 2026-11-12).
    case landscape
    /// Adaptive landscape system area: two full-width rows below H135, otherwise three rows
    /// in the right half. The page supplies height and padding (control-page.md §1.2.1).
    case landscapeSystem(twoRows: Bool)

    var rowHeight: CGFloat? { switch self { case .accessory: 36; case .landscape: nil; case .landscapeSystem: nil } }
    var rowGap: CGFloat { self == .accessory ? 6 : 5 }
    var horizontalPadding: CGFloat { switch self { case .accessory: 6; case .landscape: 3; case .landscapeSystem: 0 } }
    var verticalPadding: CGFloat { self == .accessory ? 7 : 0 }
    var textSize: CGFloat { switch self { case .accessory: 13; case .landscape: 12; case .landscapeSystem(let two): two ? 14 : 13 } }
    /// Right of ↑: 收起键盘, or 切换键盘 on the floating keyboard.
    var hideKeyBesideUp: Bool { self != .landscape }
    /// Height of the three rows with their padding, when it is fixed.
    var height: CGFloat? {
        rowHeight.map { 3 * $0 + 2 * rowGap + 2 * verticalPadding }
    }
}

/// The control area (§3.1): three fixed rows shared by both keyboards. Left of the 120 pt arrow
/// block the keys form one 5-column grid; the top row's media keys share the rest equally; Fn
/// cross-fades everything right of Esc to F1–F12.
struct ControlKeysBar: View {
    @Environment(\.oink) private var oink
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @ObservedObject var model: ControlModel
    @ObservedObject var slide: KeySlide
    let style: ControlBarStyle

    init(model: ControlModel, style: ControlBarStyle) {
        self.model = model
        self.slide = model.slide
        self.style = style
    }

    var body: some View {
        if case .landscapeSystem(let twoRows) = style {
            systemRows(two: twoRows)
        } else {
            GeometryReader { geo in
                let row = geo.size.width - 2 * style.horizontalPadding
                let cw = max(20, (row - 120 - 6 - 12) / 5)
                let arrows: CGFloat = 120
                VStack(spacing: style.rowGap) {
                    topRow(cw: cw).rowHeight(style.rowHeight)
                    middleRow(cw: cw, arrows: arrows).rowHeight(style.rowHeight)
                    bottomRow(cw: cw, arrows: arrows).rowHeight(style.rowHeight)
                }
                .padding(.horizontal, style.horizontalPadding)
                .padding(.vertical, style.verticalPadding)
                .frame(width: geo.size.width, height: geo.size.height, alignment: .top)
            }
            .frame(height: style.height)
            .background(style == .accessory ? oink.kbBg : .clear)
            .overlay(alignment: .bottom) {
                if style == .accessory && system { Rectangle().fill(oink.kbShadow).frame(height: 0.5) }
            }
        }
    }

    private func systemRows(two: Bool) -> some View {
        GeometryReader { geo in
            let rows = SystemControlKey.rows(two: two, fn: model.fn.isOn, mac: mac)
            VStack(spacing: 3) {
                ForEach(Array(rows.enumerated()), id: \.offset) { _, row in
                    let width = max(0, (geo.size.width - CGFloat(row.count - 1) * 3) / CGFloat(row.count))
                    HStack(spacing: 3) {
                        ForEach(Array(row.enumerated()), id: \.offset) { _, key in
                            systemKey(key, width: width).frame(width: width).frame(maxHeight: .infinity)
                        }
                    }
                }
            }
        }
    }

    @ViewBuilder private func systemKey(_ key: SystemControlKey, width: CGFloat) -> some View {
        switch key {
        case .caps: capsKey(cw: width)
        case .ctrl: modifierKey(.ctrl, cw: width)
        case .meta: modifierKey(.meta, cw: width)
        case .alt: modifierKey(.alt, cw: width)
        case .switchKeyboard: switchKey
        case .hideKeyboard:
            KeyCap(action: .click { model.closeKeyboard() }, accessibility: "收起键盘", spots: model.keySpots) {
                Image(systemName: "keyboard.chevron.compact.down").font(.system(size: 17))
            }
        case .fn:
            KeyCap(state: model.fn.state, action: .click { model.tapFn() }, accessibility: "Fn", spots: model.keySpots) { KeyText(text: "Fn", size: textSize) }
        case .left: arrow("arrow-left", "左", HID.left, id: "left")
        case .up: arrow("arrow-up", "上", HID.up, id: "up")
        case .down: arrow("arrow-down", "下", HID.down, id: "down")
        case .right: arrow("arrow-right", "右", HID.right, id: "right")
        case .copy, .paste:
            let copy = key == .copy
            let icon = copy ? "copy" : "clipboard-paste"
            KeyCap(action: .click { model.combo(copy ? .copy : .paste) },
                   accessibility: copy ? (mac ? "复制（⌘C）" : "复制（Ctrl+C）") : (mac ? "粘贴（⌘V）" : "粘贴（Ctrl+V）"),
                   slide: spec(copy ? "copy" : "paste", Faces.lucide(icon))) { Lucide(name: icon, size: 17) }
        case .previous, .play, .next, .mute, .volumeDown, .volumeUp:
            systemMedia(key)
        default:
            let (id, label, face, code): (String, String, String, UInt32) = switch key {
            case .esc: ("esc", "Esc", "Esc", HID.escape)
            case .home: ("home", "Home", "Home", HID.home)
            case .end: ("end", "End", "End", HID.end)
            case .pageUp: ("pgup", "Page Up", "PgUp", HID.pageUp)
            case .pageDown: ("pgdn", "Page Down", "PgDn", HID.pageDown)
            case .tab: ("tab", "Tab", "Tab", HID.tab)
            case .delete: ("del", "向前删除", mac ? "⌦" : "Del", HID.forwardDelete)
            case .function(let n): ("f\(n)", "F\(n)", "F\(n)", 0)
            default: preconditionFailure("handled above")
            }
            KeyCap(action: .click { if case .function(let n) = key { model.fKey(n) } else { model.clickKey(code) } },
                   accessibility: label, slide: spec(id, Faces.text(face))) {
                KeyText(text: face, size: key == .delete && mac ? 18 : textSize)
            }
        }
    }

    private func systemMedia(_ key: SystemControlKey) -> some View {
        let (id, name, code, asset, size): (String, String, UInt32, String, CGFloat) = switch key {
        case .previous: ("prev", "快退 / 上一首", HID.previousTrack, "Lucide-rewind-fill-2", 17)
        case .play: ("play", "播放 / 暂停", HID.playPause, "Control-play-pause", 19)
        case .next: ("next", "快进 / 下一首", HID.nextTrack, "Lucide-fast-forward-fill-2", 17)
        case .mute: ("mute", "静音", HID.mute, "PCGlyph-speaker0", 18)
        case .volumeDown: ("vol-", "音量-", HID.volumeDown, "PCGlyph-speaker1", 18)
        default: ("vol+", "音量+", HID.volumeUp, "PCGlyph-speaker3", 18)
        }
        let action: KeyAction = key == .play || key == .mute ? .click { model.mediaClick(code) } : heldMedia(id, code)
        return KeyCap(action: action, accessibility: name, slide: spec(id, Faces.asset(asset))) { AssetIcon(asset, size: size) }
    }

    private var system: Bool { model.shownKeyboard == .system }
    private var mac: Bool { model.keyTarget == .mac }
    private var textSize: CGFloat { style.textSize }

    /// 切换键盘 (the key in landscape, the top-bar button in portrait).
    static func switchLabel(system: Bool) -> String {
        system ? "切换键盘：换到模拟键盘（发送原始键码）" : "切换键盘：换到系统键盘（手机输入法）"
    }

    private func spec(_ id: String, _ face: BubbleFace) -> SlideSpec {
        SlideSpec(id: id, face: face, down: slide.down.contains(id), spots: model.keySpots)
    }

    private func heldMedia(_ id: String, _ code: UInt32) -> KeyAction {
        .hold(down: { model.heldDown(id, code, combines: false) }, up: { model.heldUp(id) })
    }

    /// An arrow: goes down when pressed, repeats while held and combines with modifiers.
    private func held(_ id: String, _ code: UInt32) -> KeyAction {
        .hold(down: { model.heldDown(id, code, combines: true) }, up: { model.heldUp(id) })
    }

    private func topRow(cw: CGFloat) -> some View {
        HStack(spacing: 3) {
            KeyCap(action: .click { model.clickKey(HID.escape) }, accessibility: "Esc", slide: spec("esc", Faces.text("Esc"))) { KeyText(text: "Esc", size: textSize) }
                .frame(width: cw)
            // Only the shown layer exists (so the screen reader never meets the hidden one); the
            // two cross-fade in place.
            ZStack {
                if !model.fn.isOn {
                HStack(spacing: 3) {
                    KeyCap(action: .click { model.combo(.copy) }, accessibility: mac ? "复制（⌘C）" : "复制（Ctrl+C）",
                           slide: spec("copy", Faces.lucide("copy"))) {
                        Lucide(name: "copy", size: 17)
                    }
                    .frame(width: cw)
                    KeyCap(action: .click { model.combo(.paste) }, accessibility: mac ? "粘贴（⌘V）" : "粘贴（Ctrl+V）",
                           slide: spec("paste", Faces.lucide("clipboard-paste"))) {
                        Lucide(name: "clipboard-paste", size: 17)
                    }
                    .frame(width: cw)
                    KeyCap(action: heldMedia("prev", HID.previousTrack), accessibility: "快退 / 上一首",
                           slide: spec("prev", Faces.asset("Lucide-rewind-fill-2"))) { AssetIcon("Lucide-rewind-fill-2", size: 17) }
                    KeyCap(action: .click { model.mediaClick(HID.playPause) }, accessibility: "播放 / 暂停",
                           slide: spec("play", Faces.asset("Control-play-pause", size: 31))) { AssetIcon("Control-play-pause", size: 19) }
                    KeyCap(action: heldMedia("next", HID.nextTrack), accessibility: "快进 / 下一首",
                           slide: spec("next", Faces.asset("Lucide-fast-forward-fill-2"))) { AssetIcon("Lucide-fast-forward-fill-2", size: 17) }
                    KeyCap(action: .click { model.mediaClick(HID.mute) }, accessibility: "静音",
                           slide: spec("mute", Faces.asset("PCGlyph-speaker0", size: 30))) { AssetIcon("PCGlyph-speaker0", size: 18) }
                    KeyCap(action: heldMedia("vol-", HID.volumeDown), accessibility: "音量-",
                           slide: spec("vol-", Faces.asset("PCGlyph-speaker1", size: 30))) { AssetIcon("PCGlyph-speaker1", size: 18) }
                    KeyCap(action: heldMedia("vol+", HID.volumeUp), accessibility: "音量+",
                           slide: spec("vol+", Faces.asset("PCGlyph-speaker3", size: 30))) { AssetIcon("PCGlyph-speaker3", size: 18) }
                }
                .transition(.opacity)
                } else {
                HStack(spacing: 6) {
                    ForEach(0..<3) { group in
                        HStack(spacing: 3) {
                            ForEach(1...4, id: \.self) { i in
                                let n = group * 4 + i
                                KeyCap(action: .click { model.fKey(n) }, accessibility: "F\(n)", slide: spec("f\(n)", Faces.text("F\(n)"))) {
                                    KeyText(text: "F\(n)", size: 12)
                                }
                            }
                        }
                    }
                }
                .transition(.opacity)
                }
            }
            .animation(reduceMotion ? nil : .easeOut(duration: 0.18), value: model.fn.isOn)
        }
    }

    private var switchKey: some View {
        KeyCap(action: .click { model.switchKeyboard() },
               accessibility: ControlKeysBar.switchLabel(system: system), spots: model.keySpots) {
            AssetIcon("Control-switch-keyboard", size: 22)
        }
    }

    /// Caps Lock (user 2026-11-12; the letters layer keeps its own Shift): a plain key without an
    /// on / off light; it sends Caps Lock with the real touch down/up (ControlModel.capsLock).
    private func capsKey(cw: CGFloat) -> some View {
        KeyCap(action: .hold(down: { model.capsLock(down: true) }, up: { model.capsLock(down: false) }), accessibility: "Caps Lock", slide: spec("caps", Faces.text(mac ? "⇪" : "Caps"))) {
            KeyText(text: mac ? "⇪" : "Caps", size: mac ? 18 : textSize)
        }
        .frame(width: cw)
    }

    private func modifierKey(_ m: Modifier, cw: CGFloat) -> some View {
        let (text, name): (String, String) = switch (m, mac) {
        case (.shift, false): ("Shift", "Shift")
        case (.shift, true): ("⇧", "shift")
        case (.ctrl, false): ("Ctrl", "Ctrl")
        case (.ctrl, true): ("⌃", "control")
        case (.alt, false): ("Alt", "Alt")
        case (.alt, true): ("⌥", "option")
        case (.meta, false): ("", "Windows")
        case (.meta, true): ("⌘", "command")
        }
        return KeyCap(state: model.modifiers.state(m), action: .toggle(tap: { model.tapModifier(m) }, long: { model.longPressModifier(m) }),
                      accessibility: name, spots: model.keySpots) {
            if m == .meta && !mac {
                AssetIcon("Control-win-key", size: 14)
            } else {
                KeyText(text: text, size: mac ? 18 : textSize)
            }
        }
        .frame(width: cw)
    }

    private func middleRow(cw: CGFloat, arrows: CGFloat) -> some View {
        HStack(spacing: 6) {
            HStack(spacing: 3) {
                capsKey(cw: cw)
                ForEach([("Home", "Home", HID.home), ("End", "End", HID.end), ("PgUp", "Page Up", HID.pageUp), ("PgDn", "Page Down", HID.pageDown)],
                        id: \.0) { text, name, code in
                    KeyCap(action: .click { model.clickKey(code) }, accessibility: name, slide: spec(text.lowercased(), Faces.text(text))) {
                        KeyText(text: text, size: textSize)
                    }
                    .frame(width: cw)
                }
            }
            HStack(spacing: 3) {
                KeyCap(state: model.fn.state, action: .click { model.tapFn() }, accessibility: "Fn", spots: model.keySpots) { KeyText(text: "Fn", size: textSize) }
                arrow("arrow-up", "上", HID.up, id: "up")
                if style.hideKeyBesideUp {
                    // 收起键盘 (用户 2026-11-12); 切换键盘 is in the top bar's top-right (portrait) or
                    // ends the top row (above the landscape system keyboard).
                    KeyCap(action: .click { model.closeKeyboard() }, accessibility: "收起键盘", spots: model.keySpots) {
                        Image(systemName: "keyboard.chevron.compact.down").font(.system(size: 17))
                    }
                } else {
                    switchKey
                }
            }
            .frame(width: arrows)
        }
    }

    private func bottomRow(cw: CGFloat, arrows: CGFloat) -> some View {
        HStack(spacing: 6) {
            HStack(spacing: 3) {
                if mac {
                    modifierKey(.ctrl, cw: cw); modifierKey(.alt, cw: cw); modifierKey(.meta, cw: cw)
                } else {
                    modifierKey(.ctrl, cw: cw); modifierKey(.meta, cw: cw); modifierKey(.alt, cw: cw)
                }
                KeyCap(action: .click { model.clickKey(HID.tab) }, accessibility: "Tab", slide: spec("tab", Faces.text("Tab"))) { KeyText(text: "Tab", size: textSize) }
                    .frame(width: cw)
                KeyCap(action: .click { model.clickKey(HID.forwardDelete) }, accessibility: "向前删除",
                       slide: spec("del", mac ? Faces.text("⌦", size: 30) : Faces.text("Del"))) {
                    KeyText(text: mac ? "⌦" : "Del", size: mac ? 18 : textSize)
                }
                .frame(width: cw)
            }
            HStack(spacing: 3) {
                arrow("arrow-left", "左", HID.left, id: "left")
                arrow("arrow-down", "下", HID.down, id: "down")
                arrow("arrow-right", "右", HID.right, id: "right")
            }
            .frame(width: arrows)
        }
    }

    private func arrow(_ icon: String, _ name: String, _ code: UInt32, id: String) -> some View {
        KeyCap(action: held(id, code), accessibility: name, slide: spec(id, Faces.lucide(icon))) {
            Lucide(name: icon, size: 15)
        }
    }
}

// MARK: Emulated keyboard

/// Oink's own keyboard (§3.1.2): letters or symbols under a number row that both layers keep (用户
/// 2026-11-12), raw key codes (through `ControlKeyboardLayout.typedKey`). Keys go down when the
/// finger lands, the newest held one repeats, and they go up when it lifts. Both layers have five
/// key rows that share the height equally, so switching layers moves nothing.
///
/// Portrait: the input view, as high as the system keyboard, its colour down to the screen bottom.
/// Landscape (`landscape`): the lower part of the keyboard the page draws in one half (§1.2) —
/// rows 6 apart, keys 19, the page pads it.
struct EmulatedKeyboard: View {
    @Environment(\.oink) private var oink
    @ObservedObject var model: ControlModel
    @ObservedObject var slide: KeySlide
    /// Portrait input view: the home-indicator strip its colour runs over.
    @ObservedObject private var inset: KeyboardInset
    let landscape: Bool

    init(model: ControlModel, landscape: Bool = false, inset: KeyboardInset = KeyboardInset()) {
        self.model = model
        self.slide = model.slide
        self.landscape = landscape
        self.inset = inset
    }

    var body: some View {
        if landscape {
            keys
        } else {
            keys
                .padding(.horizontal, 3)
                .padding(.top, 1)
                .padding(.bottom, 10 + inset.bottom)
                .background(oink.kbBg)
                .ignoresSafeArea()
        }
    }

    /// The key rows, each 3 in from the sides.
    private var keys: some View {
        GeometryReader { geo in
            let row = geo.size.width - 6
            let kw = max(10, (row - 54) / 10)
            let rows = model.symbols ? EmulatedRows.symbols : EmulatedRows.letters
            VStack(spacing: landscape ? 6 : 10) {
                HStack(spacing: 6) {
                    ForEach(EmulatedRows.numbers, id: \.self) { c in charKey(c, number: true).frame(width: kw) }
                }
                .frame(maxWidth: .infinity)
                ForEach(Array(rows.enumerated()), id: \.offset) { index, keys in
                    rowView(index: index, keys: keys, kw: kw)
                }
                bottomRow(kw: kw)
            }
            .padding(.horizontal, 3)
            .frame(width: geo.size.width, height: geo.size.height)
        }
    }


    private var shift: Bool { model.modifiers.isDown(.shift) }
    /// Character keys 22 (landscape 19); the number row 19.
    private var charSize: CGFloat { landscape ? 19 : 22 }

    private func spec(_ id: String, _ face: BubbleFace) -> SlideSpec {
        SlideSpec(id: id, face: face, down: slide.down.contains(id), spots: model.keySpots)
    }

    /// The three rows under the number row. Letters: the second row indented half a key, Shift and
    /// backspace on the third (3.6 further from the letters). Symbols: backspace ends the third
    /// row at the normal key gap.
    @ViewBuilder private func rowView(index: Int, keys: [Character], kw: CGFloat) -> some View {
        HStack(spacing: 6) {
            if !model.symbols && index == 2 {
                KeyCap(state: model.modifiers.state(.shift), action: .toggle(tap: { model.tapModifier(.shift) }, long: { model.longPressModifier(.shift) }),
                       accessibility: "Shift", spots: model.keySpots) {
                    Lucide(name: "arrow-up", size: 18)
                }
                .padding(.trailing, 3.6)
            }
            ForEach(Array(keys.enumerated()), id: \.offset) { _, c in
                charKey(c).frame(width: kw)
            }
            if index == 2 {
                heldKey("backspace", HID.backspace, "退格", face: .function, bubble: Faces.lucide("delete")) { Lucide(name: "delete", size: 19) }
                    .padding(.leading, model.symbols ? 0 : 3.6)
            }
        }
        .padding(.horizontal, !model.symbols && index == 1 ? kw / 2 + 3 : 0)
        .frame(maxWidth: .infinity)
    }

    /// A character key: down (with auto-Shift when needed) when the first finger enters, repeated
    /// while it is the newest held key, up when the last one leaves. Its face follows Shift
    /// (`EmulatedRows.face`).
    private func charKey(_ c: Character, number: Bool = false) -> some View {
        let label = EmulatedRows.face(c, shift: shift)
        let id = "vk-\(c)"
        return KeyCap(face: .character, action: .hold(down: { model.typedDown(id, c) }, up: { model.heldUp(id) }),
                      accessibility: label, slide: spec(id, Faces.character(label))) {
            Text(label).font(.system(size: number ? 19 : charSize)).lineLimit(1).minimumScaleFactor(0.6)
        }
    }

    private func heldKey<L: View>(_ id: String, _ code: UInt32, _ name: String, face: KeyFace, bubble: BubbleFace,
                                  @ViewBuilder label: @escaping () -> L) -> some View {
        KeyCap(face: face, action: .hold(down: { model.heldDown(id, code, combines: true) }, up: { model.heldUp(id) }),
               accessibility: name, slide: spec(id, bubble), label: label)
    }

    /// The same on both layers: the layer switch (`#+=` ↔ `ABC`, 1.3 kw; the digits stay above), `,`,
    /// space, `.`, return (2.2 kw).
    private func bottomRow(kw: CGFloat) -> some View {
        HStack(spacing: 6) {
            KeyCap(action: .click { model.symbols.toggle() }, accessibility: model.symbols ? "字母" : "符号", spots: model.keySpots) {
                KeyText(text: model.symbols ? "ABC" : "#+=", size: 14)
            }
            .frame(width: kw * 1.3)
            charKey(",").frame(width: kw)
            heldKey("space", HID.space, "空格", face: .character, bubble: Faces.text("空格")) { KeyText(text: "空格", size: landscape ? 13 : 15) }
            charKey(".").frame(width: kw)
            heldKey("return", HID.returnKey, "回车", face: .function, bubble: Faces.lucide("corner-down-left")) { Lucide(name: "corner-down-left", size: 19) }
                .frame(width: kw * 2.2)
        }
    }
}

/// Landscape: the emulated keyboard drawn by the page (§1.2) — the control keys (1/4) and,
/// 6 below them, the five key rows sharing the other 3/4.
struct LandscapeEmulatedKeyboard: View {
    let model: ControlModel
    var body: some View {
        GeometryReader { geo in
            let heights = FloatingKeyHeights(available: geo.size.height, gap: 6)
            VStack(spacing: 6) {
                ControlKeysBar(model: model, style: .landscape).frame(height: heights.control)
                EmulatedKeyboard(model: model, landscape: true).frame(height: heights.normal)
            }
        }
    }
}

private extension View {
    /// A control-key row: its fixed height, or an equal share of the height (`nil`).
    @ViewBuilder func rowHeight(_ height: CGFloat?) -> some View {
        if let height { frame(height: height) } else { frame(maxHeight: .infinity) }
    }
}

// MARK: Expanded keyboard

/// One key of an expanded-keyboard row: its width in key units (a row is 15 units).
private struct ExKey {
    let units: CGFloat
    let view: AnyView
}

/// The landscape Oink keyboard expanded to the whole screen (user 2026-11-12, control-page.md
/// §3.1.5): closer to a real keyboard.
///
/// Two function rows on top, column-aligned — nine equal keys and a 64 column: Esc, copy, paste and
/// the six media keys | 收起; Home, End, PgUp, PgDn, Del, PrtSc, 接收音频, 共享麦克风, 切换键盘 | 还原
/// (收起 / 还原 are icons only). Then five rows laid out like a laptop (15 key units each):
/// `` ` `` 1–0 - = ⌫(2) / Tab(1.5) q–p [ ] \(1.5) / Caps(1.75) a–l ; ' ↵(2.25) / ⇧(2.25) z–m , . / ⇧(2.75) /
/// Fn(1) Ctrl Win Alt(1.25 each) space(6) Alt(1.25) arrows(3, ↑↓ half height); Mac: Fn ⌃ ⌥ ⌘ space ⌘.
/// Every symbol is on its real key: Shift gives the upper ones (the faces follow), so there is no
/// symbol layer; with Fn on the number row is F1–F12. Both Shifts and both right-hand modifiers are
/// the same modifier.
struct ExpandedKeyboard: View {
    @Environment(\.oink) private var oink
    @ObservedObject var model: ControlModel
    @ObservedObject var slide: KeySlide
    let onRestore: () -> Void
    let onAudioError: (String) -> Void

    init(model: ControlModel, onRestore: @escaping () -> Void, onAudioError: @escaping (String) -> Void) {
        self.model = model
        self.slide = model.slide
        self.onRestore = onRestore
        self.onAudioError = onAudioError
    }

    static let functionRow: CGFloat = 34
    static let sideColumn: CGFloat = 64
    private let gap: CGFloat = 5

    var body: some View {
        VStack(spacing: 8) {
            VStack(spacing: gap) {
                functionRow(first: true).frame(height: Self.functionRow)
                functionRow(first: false).frame(height: Self.functionRow)
            }
            VStack(spacing: 6) {
                ForEach(0..<5, id: \.self) { index in
                    GeometryReader { geo in
                        let keys = row(index)
                        let free = geo.size.width - CGFloat(keys.count - 1) * gap
                        HStack(spacing: gap) {
                            ForEach(Array(keys.enumerated()), id: \.offset) { _, key in
                                key.view.frame(width: max(0, free * key.units / 15))
                            }
                        }
                    }
                }
            }
        }
    }

    private var mac: Bool { model.keyTarget == .mac }
    private var shift: Bool { model.modifiers.isDown(.shift) }

    private func spec(_ id: String, _ face: BubbleFace) -> SlideSpec {
        SlideSpec(id: id, face: face, down: slide.down.contains(id), spots: model.keySpots)
    }

    // MARK: Function rows

    private func functionRow(first: Bool) -> some View {
        HStack(spacing: gap) {
            if first {
                fkey(.click { model.clickKey(HID.escape) }, "Esc", spec("esc", Faces.text("Esc"))) { KeyText(text: "Esc", size: 13) }
                fkey(.click { model.combo(.copy) }, mac ? "复制（⌘C）" : "复制（Ctrl+C）", spec("copy", Faces.lucide("copy"))) { Lucide(name: "copy", size: 17) }
                fkey(.click { model.combo(.paste) }, mac ? "粘贴（⌘V）" : "粘贴（Ctrl+V）", spec("paste", Faces.lucide("clipboard-paste"))) { Lucide(name: "clipboard-paste", size: 17) }
                fkey(media("prev", HID.previousTrack), "快退 / 上一首", spec("prev", Faces.asset("Lucide-rewind-fill-2"))) { AssetIcon("Lucide-rewind-fill-2", size: 17) }
                fkey(.click { model.mediaClick(HID.playPause) }, "播放 / 暂停", spec("play", Faces.asset("Control-play-pause", size: 31))) { AssetIcon("Control-play-pause", size: 19) }
                fkey(media("next", HID.nextTrack), "快进 / 下一首", spec("next", Faces.asset("Lucide-fast-forward-fill-2"))) { AssetIcon("Lucide-fast-forward-fill-2", size: 17) }
                fkey(.click { model.mediaClick(HID.mute) }, "静音", spec("mute", Faces.asset("PCGlyph-speaker0", size: 30))) { AssetIcon("PCGlyph-speaker0", size: 18) }
                fkey(media("vol-", HID.volumeDown), "音量-", spec("vol-", Faces.asset("PCGlyph-speaker1", size: 30))) { AssetIcon("PCGlyph-speaker1", size: 18) }
                fkey(media("vol+", HID.volumeUp), "音量+", spec("vol+", Faces.asset("PCGlyph-speaker3", size: 30))) { AssetIcon("PCGlyph-speaker3", size: 18) }
                KeyCap(action: .click { model.closeKeyboard() }, accessibility: "收起键盘", spots: model.keySpots) {
                    Image(systemName: "keyboard.chevron.compact.down").font(.system(size: 17))
                }
                .frame(width: Self.sideColumn)
            } else {
                ForEach([("Home", "Home", HID.home), ("End", "End", HID.end), ("PgUp", "Page Up", HID.pageUp), ("PgDn", "Page Down", HID.pageDown)],
                        id: \.0) { text, name, code in
                    fkey(.click { model.clickKey(code) }, name, spec(text.lowercased(), Faces.text(text))) { KeyText(text: text, size: 13) }
                }
                fkey(.click { model.clickKey(HID.forwardDelete) }, "向前删除", spec("del", mac ? Faces.text("⌦", size: 30) : Faces.text("Del"))) {
                    KeyText(text: mac ? "⌦" : "Del", size: mac ? 18 : 13)
                }
                // Print Screen; Mac targets get the same key code (no function there).
                fkey(.click { model.clickKey(HID.printScreen) }, "Print Screen", spec("prtsc", Faces.text("PrtSc"))) { KeyText(text: "PrtSc", size: 13) }
                ExpandedAudioKey(microphone: false, onError: onAudioError).frame(maxWidth: .infinity)
                ExpandedAudioKey(microphone: true, onError: onAudioError).frame(maxWidth: .infinity)
                KeyCap(action: .click { model.switchKeyboard() }, accessibility: ControlKeysBar.switchLabel(system: false), spots: model.keySpots) {
                    AssetIcon("Control-switch-keyboard", size: 22)
                }
                .frame(maxWidth: .infinity)
                KeyCap(action: .click { onRestore() }, accessibility: "还原：回到浮动键盘", spots: model.keySpots) {
                    Image(systemName: "arrow.down.right.and.arrow.up.left").font(.system(size: 16, weight: .medium))
                }
                .frame(width: Self.sideColumn)
            }
        }
    }

    private func media(_ id: String, _ code: UInt32) -> KeyAction {
        .hold(down: { model.heldDown(id, code, combines: false) }, up: { model.heldUp(id) })
    }

    private func fkey<L: View>(_ action: KeyAction, _ name: String, _ slide: SlideSpec, @ViewBuilder label: @escaping () -> L) -> some View {
        KeyCap(radius: 7, action: action, accessibility: name, slide: slide, label: label).frame(maxWidth: .infinity)
    }

    // MARK: Main rows

    private func row(_ index: Int) -> [ExKey] {
        switch index {
        case 0:
            let number: [ExKey] = ["`", "1", "2", "3", "4", "5", "6", "7", "8", "9", "0", "-", "="].enumerated().map { i, c in
                let ch = Character(c)
                if model.fn.isOn && i > 0 { return fKey(i) }
                return char(ch, digit: ch.isNumber)
            }
            return number + [held("backspace", HID.backspace, "退格", 2, Faces.lucide("delete")) { Lucide(name: "delete", size: 20) }]
        case 1:
            return [click("tab", HID.tab, "Tab", 1.5)] + "qwertyuiop[]".map { char($0) } + [char("\\", units: 1.5)]
        case 2:
            return [caps()] + "asdfghjkl;'".map { char($0) } + [held("return", HID.returnKey, "回车", 2.25, Faces.lucide("corner-down-left")) { Lucide(name: "corner-down-left", size: 20) }]
        case 3:
            return [modifier(.shift, 2.25)] + "zxcvbnm,./".map { char($0) } + [modifier(.shift, 2.75)]
        default:
            let mods: [Modifier] = mac ? [.ctrl, .alt, .meta] : [.ctrl, .meta, .alt]
            return [fn()] + mods.map { modifier($0, 1.25) }
                + [held("space", HID.space, "空格", 6, Faces.text("空格"), face: .character) { KeyText(text: "空格", size: 13) }]
                + [modifier(mods[2], 1.25), arrows()]
        }
    }

    private func char(_ c: Character, digit: Bool = false, units: CGFloat = 1) -> ExKey {
        let label = EmulatedRows.face(c, shift: shift)
        let id = "vk-\(c)"
        return ExKey(units: units, view: AnyView(
            KeyCap(face: .character, radius: 7, action: .hold(down: { model.typedDown(id, c) }, up: { model.heldUp(id) }),
                   accessibility: label, slide: spec(id, Faces.character(label))) {
                Text(label).font(.system(size: digit ? 18 : 20)).lineLimit(1).minimumScaleFactor(0.6)
            }))
    }

    private func fKey(_ n: Int) -> ExKey {
        ExKey(units: 1, view: AnyView(
            KeyCap(radius: 7, action: .click { model.fKey(n) }, accessibility: "F\(n)", slide: spec("f\(n)", Faces.text("F\(n)"))) {
                KeyText(text: "F\(n)", size: 13)
            }))
    }

    private func click(_ id: String, _ code: UInt32, _ text: String, _ units: CGFloat) -> ExKey {
        ExKey(units: units, view: AnyView(
            KeyCap(radius: 7, action: .click { model.clickKey(code) }, accessibility: text, slide: spec(id, Faces.text(text))) {
                KeyText(text: text, size: 13)
            }))
    }

    private func held<L: View>(_ id: String, _ code: UInt32, _ name: String, _ units: CGFloat, _ bubble: BubbleFace,
                               face: KeyFace = .function, @ViewBuilder label: @escaping () -> L) -> ExKey {
        ExKey(units: units, view: AnyView(
            KeyCap(face: face, radius: 7, action: .hold(down: { model.heldDown(id, code, combines: true) }, up: { model.heldUp(id) }),
                   accessibility: name, slide: spec(id, bubble), label: label)))
    }

    private func caps() -> ExKey {
        ExKey(units: 1.75, view: AnyView(
            KeyCap(radius: 7, action: .hold(down: { model.capsLock(down: true) }, up: { model.capsLock(down: false) }), accessibility: "Caps Lock", slide: spec("caps", Faces.text(mac ? "⇪" : "Caps"))) {
                KeyText(text: mac ? "⇪" : "Caps", size: mac ? 18 : 13)
            }))
    }

    private func fn() -> ExKey {
        ExKey(units: 1, view: AnyView(
            KeyCap(state: model.fn.state, radius: 7, action: .click { model.tapFn() }, accessibility: "Fn", spots: model.keySpots) {
                KeyText(text: "Fn", size: 14)
            }))
    }

    private func modifier(_ m: Modifier, _ units: CGFloat) -> ExKey {
        let (text, name): (String, String) = switch (m, mac) {
        case (.shift, false): ("Shift", "Shift")
        case (.shift, true): ("⇧", "shift")
        case (.ctrl, false): ("Ctrl", "Ctrl")
        case (.ctrl, true): ("⌃", "control")
        case (.alt, false): ("Alt", "Alt")
        case (.alt, true): ("⌥", "option")
        case (.meta, false): ("", "Windows")
        case (.meta, true): ("⌘", "command")
        }
        return ExKey(units: units, view: AnyView(
            KeyCap(state: model.modifiers.state(m), radius: 7,
                   action: .toggle(tap: { model.tapModifier(m) }, long: { model.longPressModifier(m) }),
                   accessibility: name, spots: model.keySpots) {
                if m == .meta && !mac {
                    AssetIcon("Control-win-key", size: 15)
                } else {
                    KeyText(text: text, size: mac ? 18 : 14)
                }
            }))
    }

    /// ← (↑ over ↓, half height each) →: an inverted T in three units.
    private func arrows() -> ExKey {
        let key = { (icon: String, name: String, code: UInt32, id: String, size: CGFloat) in
            KeyCap(radius: 7, action: .hold(down: { model.heldDown(id, code, combines: true) }, up: { model.heldUp(id) }),
                   accessibility: name, slide: spec(id, Faces.lucide(icon))) {
                Lucide(name: icon, size: size)
            }
        }
        return ExKey(units: 3, view: AnyView(
            HStack(spacing: gap) {
                key("arrow-left", "左", HID.left, "left", 17)
                VStack(spacing: 3) {
                    key("arrow-up", "上", HID.up, "up", 15)
                    key("arrow-down", "下", HID.down, "down", 15)
                }
                key("arrow-right", "右", HID.right, "right", 17)
            }))
    }
}

/// 接收音频 / 共享麦克风 on the expanded keyboard: the same state as 调节's switches and this phone's
/// device-page settings (CoreSession.localAudio). Off: the speaker / microphone glyph; on: the glyph
/// with the receive / send badge, on the light theme colour.
private struct ExpandedAudioKey: View {
    @ObservedObject private var speakerActions = SpeakerActions.shared
    @Environment(\.oink) private var oink
    @ObservedObject private var core = CoreSession.shared
    let microphone: Bool
    let onError: (String) -> Void

    var body: some View {
        let disabled = !microphone && speakerActions.forwarding
        let on = !disabled && (microphone ? core.localAudio.microphoneOn : core.localAudio.speakerOn)
        PCIcon(glyph: microphone ? "microphone" : "speaker3", badge: on ? (microphone ? .send : .receive) : nil, size: 18,
               color: on ? oink.accentInk : oink.kbInk)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(on ? oink.accentSoft2 : oink.kbKey2, in: RoundedRectangle(cornerRadius: 7, style: .continuous))
            .shadow(color: oink.kbShadow, radius: 0, x: 0, y: 1)
            .contentShape(Rectangle())
            .speakerGestures { longPress in
                guard !disabled else { return }
                if microphone {
                    guard !longPress else { return }
                    Haptics.selection()
                    Task { @MainActor in
                        do { try await core.setLocalMicrophone(!on) }
                        catch { Haptics.error(); onError(error.oinkMessage) }
                    }
                } else {
                    if !longPress { Haptics.selection() }
                    speakerActions.press(exitOn: on, longPress: longPress,
                        preferForward: UserDefaults.standard.string(forKey: SpeakerActions.preferenceKey) == "forward")
                }
            }
            .disabled(disabled)
            .opacity(disabled ? 0.38 : 1)
            .accessibilityIdentifier(microphone ? "audio-key-microphone" : "audio-key-speaker")
        .accessibilityLabel(microphone ? "共享麦克风" : "接收音频")
        .accessibilityValue(on ? "开" : "关")
        .accessibilityAddTraits(on ? [.isButton, .isSelected] : .isButton)
    }
}

// MARK: Key preview

/// Takes the touches that land on keys with a bubble and feeds the slide tracker (§3.1.4); a
/// touch anywhere else falls through to the SwiftUI keys underneath. Once it has a finger it
/// follows it everywhere: over other keys, over keys without a bubble (nothing), off the
/// keyboard (the finger is done until it lifts).
private final class KeyTouchView: UIView {
    private let model: ControlModel
    private let areas: () -> [CGRect]

    init(model: ControlModel, areas: @escaping () -> [CGRect]) {
        self.model = model
        self.areas = areas
        super.init(frame: .zero)
        backgroundColor = .clear
        isMultipleTouchEnabled = true
        isAccessibilityElement = false
    }
    required init?(coder: NSCoder) { fatalError() }

    override func hitTest(_ point: CGPoint, with event: UIEvent?) -> UIView? {
        guard self.point(inside: point, with: event), window != nil else { return nil }
        return model.keySpots.key(at: convert(point, to: nil)) != nil ? self : nil
    }

    private func finger(_ t: UITouch) -> Int { ObjectIdentifier(t).hashValue }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        for t in touches {
            if let key = model.keySpots.key(at: t.location(in: nil)) { model.slide.press(finger(t), key) }
        }
    }

    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        let keys = model.keySpots.all
        let surfaces = areas()
        for t in touches where model.slide.tracks(finger(t)) {
            let hit = KeySlide.locate(t.location(in: nil), current: model.slide.keyOf(finger(t)), keys: keys, areas: surfaces, near: 8, stick: 5)
            model.slide.move(finger(t), hit)
        }
    }

    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        for t in touches { model.slide.lift(finger(t)) }
    }

    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        for t in touches { model.slide.cancel(finger(t)) }
    }
}

/// The key preview bubble's window: drawn above the keyboards, never the target of a touch.
private final class PassthroughWindow: UIWindow {
    override func hitTest(_ point: CGPoint, with event: UIEvent?) -> UIView? { nil }
}

/// Where the bubble is: the key (its lower part, which the iOS callout covers) and the head.
private struct CalloutGeometry {
    var key: CGRect
    var head: CGRect

    /// iOS: the head is (kw + 24) × 56 for characters (bottom 10 above the key), otherwise
    /// max(kw + 20, 50) × 46 (8 above), at most 96 wide, 3 inside the screen edges.
    init(key: CGRect, character: Bool, width: CGFloat) {
        self.key = key
        let hw = min(96, character ? key.width + 24 : max(key.width + 20, 50))
        let hh: CGFloat = character ? 56 : 46
        let hb = key.minY - (character ? 10 : 8)
        let hx = min(max(key.midX - hw / 2, 3), width - 3 - hw)
        head = CGRect(x: hx, y: hb - hh, width: hw, height: hh)
    }
    init(key: CGRect, head: CGRect) { self.key = key; self.head = head }

    func lerp(to b: CalloutGeometry, _ t: CGFloat) -> CalloutGeometry {
        func l(_ a: CGRect, _ b: CGRect) -> CGRect {
            CGRect(x: a.minX + (b.minX - a.minX) * t, y: a.minY + (b.minY - a.minY) * t,
                   width: a.width + (b.width - a.width) * t, height: a.height + (b.height - a.height) * t)
        }
        return CalloutGeometry(key: l(key, b.key), head: l(head, b.head))
    }

    /// The system callout outline: the key (bottom corners 6) flaring out from 5 below its top,
    /// over the 15-ish gap, into the head (corners 10) — one shape.
    var path: CGPath {
        let L = key.minX, R = key.maxX, B = key.maxY, f0 = key.minY + 5
        let HL = head.minX, HR = head.maxX, HB = head.maxY, HT = head.minY
        let r: CGFloat = 6, rh: CGFloat = 10, c = (f0 - HB) / 2
        let p = CGMutablePath()
        p.move(to: CGPoint(x: L, y: B - r))
        p.addLine(to: CGPoint(x: L, y: f0))
        p.addCurve(to: CGPoint(x: HL, y: HB), control1: CGPoint(x: L, y: f0 - c), control2: CGPoint(x: HL, y: HB + c))
        p.addLine(to: CGPoint(x: HL, y: HT + rh))
        p.addQuadCurve(to: CGPoint(x: HL + rh, y: HT), control: CGPoint(x: HL, y: HT))
        p.addLine(to: CGPoint(x: HR - rh, y: HT))
        p.addQuadCurve(to: CGPoint(x: HR, y: HT + rh), control: CGPoint(x: HR, y: HT))
        p.addLine(to: CGPoint(x: HR, y: HB))
        p.addCurve(to: CGPoint(x: R, y: f0), control1: CGPoint(x: HR, y: HB + c), control2: CGPoint(x: R, y: f0 - c))
        p.addLine(to: CGPoint(x: R, y: B - r))
        p.addQuadCurve(to: CGPoint(x: R - r, y: B), control: CGPoint(x: R, y: B))
        p.addLine(to: CGPoint(x: L + r, y: B))
        p.addQuadCurve(to: CGPoint(x: L, y: B - r), control: CGPoint(x: L, y: B))
        p.closeSubpath()
        return p
    }
}

/// The key preview bubble (§3.1.4): the iOS system callout shape over the key, the key's face
/// magnified in its head. Lives in its own window above both keyboard windows, so it shows over
/// every key and reaches over the touchpad above the accessory bar without being clipped; it
/// never takes touches. Appears in 70 ms (fade, 0.9 → 1 from the key centre), glides to the next key in
/// 100 ms from wherever it is (the content swaps at once and fades in from 0.3 over 80 ms), and
/// fades out in 100 ms after at least 90 ms on screen. Reduced motion: no animation.
private final class KeyBubbleView: UIView {
    private let shape = CAShapeLayer()
    private let glyph = UIImageView()
    private var geometry: CalloutGeometry?
    private var glideFrom: CalloutGeometry?
    private var glideTo: CalloutGeometry?
    private var glideStart: CFTimeInterval = 0
    private var link: CADisplayLink?
    private var shownAt: CFTimeInterval = 0
    private var hiding = false

    init() {
        super.init(frame: .zero)
        isUserInteractionEnabled = false
        isAccessibilityElement = false
        accessibilityElementsHidden = true
        backgroundColor = .clear
        alpha = 0
        layer.addSublayer(shape)
        shape.shadowOffset = CGSize(width: 0, height: 3)
        shape.shadowRadius = 3.5
        shape.shadowOpacity = 1
        shape.lineWidth = 0.5
        glyph.contentMode = .center
        addSubview(glyph)
    }
    required init?(coder: NSCoder) { fatalError() }

    private var still: Bool { UIAccessibility.isReduceMotionEnabled }

    @MainActor
    func show(key: CGRect, face: BubbleFace, palette: OinkPalette, glide: Bool, width: CGFloat) {
        let target = CalloutGeometry(key: key, character: face.character, width: width)
        let dark = palette.dark
        shape.fillColor = (dark ? UIColor(red: 0x5B / 255, green: 0x60 / 255, blue: 0x6D / 255, alpha: 1) : .white).cgColor
        shape.strokeColor = (dark ? UIColor.white.withAlphaComponent(0.08) : UIColor(red: 27 / 255, green: 28 / 255, blue: 34 / 255, alpha: 0.12)).cgColor
        shape.shadowColor = (dark ? UIColor.black.withAlphaComponent(0.62) : UIColor(red: 27 / 255, green: 28 / 255, blue: 34 / 255, alpha: 0.26)).cgColor
        let renderer = ImageRenderer(content: face.content.foregroundStyle(palette.kbInk).environment(\.oink, palette))
        renderer.scale = window?.screen.scale ?? 3
        glyph.image = renderer.uiImage

        let visible = alpha > 0 && !hiding && geometry != nil
        hiding = false
        layer.removeAllAnimations()
        if glide && visible && !still {
            glideFrom = geometry
            glideTo = target
            glideStart = CACurrentMediaTime()
            if link == nil {
                let l = CADisplayLink(target: self, selector: #selector(step))
                l.add(to: .main, forMode: .common)
                link = l
            }
            glyph.alpha = 0.3
            UIView.animate(withDuration: 0.08, delay: 0, options: [.curveEaseOut, .beginFromCurrentState]) { self.glyph.alpha = 1 }
            return
        }
        stopGlide()
        apply(target)
        glyph.alpha = 1
        shownAt = CACurrentMediaTime()
        if still {
            transform = .identity
            alpha = 1
            return
        }
        transform = scaled(0.9, about: CGPoint(x: key.midX, y: key.midY))
        alpha = 0
        UIView.animate(withDuration: 0.07, delay: 0, options: [.beginFromCurrentState], animations: {
            self.transform = .identity
            self.alpha = 1
        })
    }

    /// Leaves `rect` (in this view's coordinates) undrawn: the area another part of the bubble
    /// covers. nil draws everywhere.
    func exclude(_ rect: CGRect?) {
        guard let rect else { layer.mask = nil; return }
        let mask = (layer.mask as? CAShapeLayer) ?? CAShapeLayer()
        let path = CGMutablePath()
        path.addRect(bounds)
        path.addRect(rect)
        mask.fillRule = .evenOdd
        mask.frame = bounds
        mask.path = path
        layer.mask = mask
    }

    func hide() {
        guard alpha > 0, !hiding else { return }
        hiding = true
        let wait = max(0, 0.09 - (CACurrentMediaTime() - shownAt))
        UIView.animate(withDuration: still ? 0 : 0.1, delay: wait, options: [.curveEaseIn, .beginFromCurrentState], animations: {
            self.alpha = 0
        }, completion: { _ in
            guard self.hiding else { return }
            self.stopGlide()
            self.geometry = nil
        })
    }

    @objc private func step() {
        guard let from = glideFrom, let to = glideTo else { stopGlide(); return }
        let t = min(1, (CACurrentMediaTime() - glideStart) / 0.1)
        apply(from.lerp(to: to, Self.ease(CGFloat(t))))
        if t >= 1 { stopGlide() }
    }

    private func stopGlide() {
        link?.invalidate()
        link = nil
        glideFrom = nil
        glideTo = nil
    }

    private func apply(_ g: CalloutGeometry) {
        geometry = g
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        shape.path = g.path
        shape.shadowPath = g.path
        CATransaction.commit()
        glyph.frame = g.head
    }

    private func scaled(_ s: CGFloat, about p: CGPoint) -> CGAffineTransform {
        let c = CGPoint(x: bounds.midX, y: bounds.midY)
        return CGAffineTransform(translationX: p.x - c.x, y: p.y - c.y).scaledBy(x: s, y: s).translatedBy(x: c.x - p.x, y: c.y - p.y)
    }

    /// cubic-bezier(.2, .9, .3, 1) at time t.
    private static func ease(_ t: CGFloat) -> CGFloat {
        func bz(_ u: CGFloat, _ a: CGFloat, _ b: CGFloat) -> CGFloat { 3 * a * u * (1 - u) * (1 - u) + 3 * b * u * u * (1 - u) + u * u * u }
        var lo: CGFloat = 0, hi: CGFloat = 1, u = t
        for _ in 0..<24 {
            let x = bz(u, 0.2, 0.3)
            if abs(x - t) < 1e-4 { break }
            if x < t { lo = u } else { hi = u }
            u = (lo + hi) / 2
        }
        return bz(u, 0.9, 1)
    }
}
