import GameController
import SwiftUI
import UIKit

/// 单指划动 (§2.1).
enum OneFingerSwipe: String, CaseIterable { case pointer, scroll
    var label: String { self == .pointer ? "移动指针" : "滚动" }
}
/// 键盘布局 (§2.1): the focus computer's layout, which the emulated keyboard's keys are mapped to
/// (§3.1.2). Kept on the phone.
enum ControlKeyboardLayout: String, CaseIterable { case auto, us, uk, de, fr, es, jis, ko
    var label: String {
        switch self {
        case .auto: "自动"; case .us: "美式英语（US）"; case .uk: "英式英语（UK）"; case .de: "德语（QWERTZ）"
        case .fr: "法语（AZERTY）"; case .es: "西班牙语"; case .jis: "日语（JIS）"; case .ko: "韩语"
        }
    }

    /// What the character key `c` (as on the key without Shift) sends on a `target` computer with
    /// this layout while `board` holds its modifiers: the key with the Shift / AltGr (Option) it
    /// needs, or text when the layout has no certain key for it (the shared Rust tables,
    /// `KeyLayouts.typed`). Every read of the setting goes through here. 自动 types the US keys
    /// until computers report their layout.
    func typedKey(_ c: Character, target: KeyTarget, board: ModifierBoard) -> TypedChar? {
        KeyLayouts.typed(c, layout: rawValue, target: target, board: board)
    }
}

/// State and actions of the control page. Input goes to the focus computer through the
/// control channel; every rule about when keys go out lives in ControlKeys.swift.
@MainActor
final class ControlModel: ObservableObject {
    @Published private(set) var snapshot = ControlSnapshot()
    @Published private(set) var lastEdgeReport: ControlEdgeReport?
    /// Bumped when the backend dropped input for lack of a target (the page shows a hint).
    @Published private(set) var noTargetPrompt = 0
    @Published private(set) var errorText: String?

    // Keyboard
    @Published private(set) var keyboardOpen = false { didSet { if keyboardOpen != oldValue { updateBar() } } }
    @Published private(set) var choice: KeyboardChoice
    @Published private(set) var modifiers = ModifierBoard()
    @Published private(set) var fn = FnLayer() {
        didSet { if fn.isOn != oldValue.isOn { slide.relayout() } }
    }
    @Published var symbols = false {
        didSet { if symbols != oldValue { slide.relayout() } }
    }
    /// The 调节 menu is open (§2.1): shared by the 调节 button in the header and the menu layer.
    @Published var adjustOpen = false { didSet { if adjustOpen != oldValue { updateBar() } } }
    /// The page is in its landscape layout (the full screen, §1.2); set by the page.
    @Published private(set) var landscape = false
    /// The landscape Oink keyboard is expanded to the whole screen (remembered, user 2026-11-12).
    @Published private(set) var keyboardExpanded: Bool
    /// The floating bar hides for keyboards; touchpad operations keep it visible.
    @Published private(set) var barShown = true
    /// Actual fingers on the pad (not a drag lock with every finger lifted).
    /// Read synchronously by the UIKit bar gate; no redraw is needed to route a new finger.
    private(set) var padTouching = false
    /// 全屏 / 恢复 (§1.2).
    @Published private(set) var fullscreen = FullscreenRules()
    private var tiltObserver: NSObjectProtocol?
    private var caps = CapsKey()
    /// System or Oink keyboard per orientation (portrait default system, landscape Oink's).
    private var preferredPortrait: KeyboardKind
    private var preferredLandscape: KeyboardKind
    /// Fingers sliding over the keys with a preview bubble (§3.1.4), and the keys on screen.
    let slide = KeySlide()
    let keySpots = KeySpots()
    /// Keys sent down and not yet up (按下集合).
    private var pressed = PressedKeys() { didSet { stopRepeatIfUp() } }
    /// Auto-repeat of the newest key in `pressed` (§3.1 按住重复).
    private let repeater = KeyRepeater()

    // Settings (phone-local)
    @Published var pointerSpeed: Int { didSet { save(); pushSettings() } }
    @Published var oneFingerSwipe: OneFingerSwipe { didSet { save() } }
    @Published var keyboardLayout: ControlKeyboardLayout { didSet { save() } }
    /// 键盘自动弹出 (§9): the focus computer's text fields pop the keyboard up / hide it. Off by default.
    @Published var keyboardAutoPopup: Bool {
        didSet {
            save()
            textFocus.autoPopup = keyboardAutoPopup
            reevaluateTextFocus()
        }
    }

    private var channel: ControlChannel?
    /// The control page is shown in the foreground, including transient system prompts.
    @Published private(set) var active = false
    /// Set by the touchpad: lifts whatever the pad holds down (a pending tap click, a drag, the
    /// locked drag) before the handle closes.
    var releasePad: (() -> Void)?
    private let defaults = UserDefaults.standard

    init() {
        let d = UserDefaults.standard
        // The Mac trackpad's tracking-speed step; macOS's default is step 4 (user 2026-11-12).
        let speed = d.object(forKey: OinkPrefs.pointerSpeed) as? Int ?? 4
        pointerSpeed = min(10, max(1, speed))
        oneFingerSwipe = OneFingerSwipe(rawValue: d.string(forKey: OinkPrefs.oneFingerSwipe) ?? "") ?? .pointer
        keyboardLayout = ControlKeyboardLayout(rawValue: d.string(forKey: OinkPrefs.keyboardLayout) ?? "") ?? .auto
        let portrait = KeyboardKind(rawValue: d.string(forKey: OinkPrefs.keyboardKindPortrait) ?? "") ?? .system
        preferredPortrait = portrait
        preferredLandscape = KeyboardKind(rawValue: d.string(forKey: OinkPrefs.keyboardKindLandscape) ?? "") ?? .emulated
        choice = KeyboardChoice(preferred: portrait)
        keyboardExpanded = d.bool(forKey: OinkPrefs.keyboardExpanded)
        keyboardAutoPopup = d.bool(forKey: OinkPrefs.keyboardAutoPopup)
        textFocus.autoPopup = keyboardAutoPopup
    }

    private func save() {
        defaults.set(pointerSpeed, forKey: OinkPrefs.pointerSpeed)
        defaults.set(oneFingerSwipe.rawValue, forKey: OinkPrefs.oneFingerSwipe)
        defaults.set(keyboardLayout.rawValue, forKey: OinkPrefs.keyboardLayout)
        defaults.set(keyboardAutoPopup, forKey: OinkPrefs.keyboardAutoPopup)
    }

    // MARK: Session

    /// The page became visible with a live session (or the session was rebuilt).
    func activate(core: CoreSession) {
        active = true
        applyFullscreen()
        channel?.close()
        channel = ControlChannels.open(core: core, onState: { [weak self] in self?.apply($0) },
                                       onEdge: { [weak self] in self?.lastEdgeReport = $0 },
                                       onTextRefused: { [weak self] in if let text = $0.message { self?.errorText = text } },
                                       onTextFocus: { [weak self] in self?.pcTextFocus($0) })
        pushSettings()
        watchHardwareKeyboard(true)
    }

    // A hardware keyboard (Bluetooth / USB) is read through GameController: its key codes are
    // HID usages, and it needs no first responder (a view holding first responder for this made
    // iOS raise the software keyboard whenever a menu opened).
    private var keyboardObserver: NSObjectProtocol?

    private func watchHardwareKeyboard(_ on: Bool) {
        if let keyboardObserver { NotificationCenter.default.removeObserver(keyboardObserver) }
        keyboardObserver = nil
        GCKeyboard.coalesced?.keyboardInput?.keyChangedHandler = nil
        guard on else { return }
        keyboardObserver = NotificationCenter.default.addObserver(forName: .GCKeyboardDidConnect, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.attachHardwareKeyboard() }
        }
        attachHardwareKeyboard()
    }

    private func attachHardwareKeyboard() {
        GCKeyboard.coalesced?.keyboardInput?.keyChangedHandler = { [weak self] _, _, code, pressed in
            let usage = code.rawValue
            Task { @MainActor in self?.hardwareKey(usage, down: pressed) }
        }
    }

    /// The page left the screen or the app went to the background: release everything that is
    /// down on the remote, close the keyboard and the handle.
    func deactivate() {
        guard active else { return }
        releasePad?()
        watchHardwareKeyboard(false)
        pendingCheck?.cancel()
        pending = nil
        active = false
        closeKeyboard(byUser: false)
        textFocusRetry?.cancel()
        textFocusRetry = nil
        adjustOpen = false
        channel?.close()
        channel = nil
    }

    private func apply(_ value: ControlSnapshot) {
        let prompt = value.noTarget && !snapshot.noTarget
        snapshot = value
        if prompt { noTargetPrompt += 1 }
        reevaluateTextFocus()
        if let pending, pending.met(focus: value.focusDevice, locked: value.locked, game: value.gameMode) { self.pending = nil }
    }

    // Pointer move, lock and game mode: a call that returns only means it was sent. The state that
    // comes back within a second decides; otherwise the page says the request did not take.
    private var pending: ControlExpectation?
    private var pendingCheck: Task<Void, Never>?

    private func expect(_ expectation: ControlExpectation) {
        pending = (pending ?? ControlExpectation()).merged(with: expectation)
        pendingCheck?.cancel()
        pendingCheck = Task { @MainActor [weak self] in
            try? await Task.sleep(for: .seconds(ControlExpectation.window))
            guard !Task.isCancelled, let self, let pending = self.pending else { return }
            self.pending = nil
            let s = self.snapshot
            if let text = pending.failureText(focus: s.focusDevice, locked: s.locked, game: s.gameMode,
                                              name: { id in s.targets.first { $0.device == id }?.name }) {
                self.errorText = text
            }
        }
    }

    private func pushSettings() {
        channel?.setSettings(pointerSpeed: UInt8(pointerSpeed), pointsPerMm: Float(Self.pointsPerMm))
    }

    /// Points per millimetre of this screen. iOS has no public pixel-density API: current iPhones
    /// are 460 ppi at @3x (153 pt/in) and @2x iPhones 326 ppi (163 pt/in); iPads 264 ppi at @2x.
    static var pointsPerMm: Double {
        if UIDevice.current.userInterfaceIdiom == .pad { return 132 / 25.4 }
        return (UIScreen.main.nativeScale >= 3 ? 153.3 : 163) / 25.4
    }

    // MARK: Focus, lock, game

    var focus: ControlTargetInfo? { snapshot.targets.first { $0.device == snapshot.focusDevice } }
    /// The OS the keys are for: the core's control state names each target's OS (its DeviceFacts);
    /// the focus computer's, else the selected one's while input is about to go there (as
    /// Android's `ControlState.focus`).
    var keyTarget: KeyTarget {
        KeyTarget(platform: (focus ?? snapshot.targets.first { $0.device == snapshot.selectedDevice })?.platform)
    }
    /// 可控制的在线设备: online Windows / Mac (and a privileged Android when it is listed).
    var controllable: [ControlTargetInfo] { snapshot.targets.filter(\.online) }

    var lockState: LockButtonState {
        LockButtonState(game: snapshot.gameMode, gameActive: snapshot.gameActive, locked: snapshot.locked)
    }

    func moveFocus(to device: String) {
        if attempt({ try $0.movePointer(to: device) }) { expect(ControlExpectation(focus: device)) }
    }

    /// Tap on the lock button, by what it shows: unlocked → locked; locked → unlocked; the gamepad
    /// (on, or auto while on) → unlocked with game mode off.
    func lockTapped() {
        switch lockState {
        case .unlocked: if attempt({ try $0.setFocusLock(true) }) { expect(ControlExpectation(locked: true)) }
        case .locked: if attempt({ try $0.setFocusLock(false) }) { expect(ControlExpectation(locked: false)) }
        case .gameOn, .gameAuto:
            if attempt({ try $0.setGameMode(.off); try $0.setFocusLock(false) }) { expect(ControlExpectation(locked: false, game: .off)) }
        }
    }

    /// 锁定指针 / 解锁指针 in the long-press menu. Locking turns game mode off.
    func setPointerLock(_ locked: Bool) {
        let gameOff = locked && snapshot.gameMode != .off
        let sent = attempt { channel in
            if gameOff { try channel.setGameMode(.off) }
            try channel.setFocusLock(locked)
        }
        if sent { expect(ControlExpectation(locked: locked, game: gameOff ? .off : nil)) }
    }

    func setGameMode(_ mode: ControlGame) {
        if attempt({ try $0.setGameMode(mode) }) { expect(ControlExpectation(game: mode)) }
    }

    /// Runs a request on the channel; false when there is none or it was refused at once.
    @discardableResult
    private func attempt(_ body: (ControlChannel) throws -> Void) -> Bool {
        guard let channel else { noTargetPrompt += 1; return false }
        do { try body(channel); return true } catch { errorText = ControlModel.message(error); return false }
    }

    func consumeError() -> String? { defer { errorText = nil }; return errorText }

    static func message(_ error: Error) -> String {
        #if OINK_MOBILE_CORE_FFI
        if case MobileError.Command(.deviceUnavailable, _) = error {
            return "请先选择要控制的电脑"
        }
        #endif
        return "操作没有完成，请重试"
    }

    // MARK: Touchpad

    /// `at` / `since`: touch times (`UITouch.timestamp`, seconds of system uptime) of this motion
    /// and of the sample it is measured from; the core uses them for the finger speed.
    func pointerMove(_ d: CGVector, at: TimeInterval, since: TimeInterval) {
        channel?.pointerMove(dx: Float(d.dx), dy: Float(d.dy), timeUs: Self.micros(at), sinceUs: Self.micros(since))
    }
    private static func micros(_ t: TimeInterval) -> UInt64 { t > 0 ? UInt64(t * 1_000_000) : 0 }
    /// A touchpad scroll of the fingers with its phase, timed like `pointerMove`; the core
    /// accelerates it and glides on after Ended, as a Mac trackpad does.
    func scroll(_ d: CGVector, phase: PadScrollPhase, at: TimeInterval, since: TimeInterval) {
        channel?.scroll(phase: phase, dx: Float(d.dx), dy: Float(d.dy), timeUs: Self.micros(at), sinceUs: Self.micros(since))
    }
    func gesture(_ gesture: PadGesture) { channel?.gesture(gesture) }
    /// Every finger on the pad (pad points) after a touch event, sent as the trackpad's contacts
    /// in millimetres and run through the core's multi-finger recognizer (`multiFinger`: the
    /// touch's fingers are free for it). The caller sends the frame's outputs with `forward()`
    /// after its own for the event.
    func touches(_ fingers: [Int: CGPoint], padSize: CGSize, at: TimeInterval, cancelled: Bool,
                 multiFinger: Bool) -> PadCoreFrame {
        let mm = CGFloat(Self.pointsPerMm)
        let list = fingers.sorted { $0.key < $1.key }.map { id, p in
            PadContact(id: UInt64(max(0, id)), x: Float(p.x / mm), y: Float(p.y / mm))
        }
        return channel?.touches(timeUs: Self.micros(at), width: Float(padSize.width / mm),
                                height: Float(padSize.height / mm), touches: list, cancelled: cancelled,
                                multiFinger: multiFinger) ?? .unclaimed
    }

    /// Pad buttons (taps, tap-to-drag, the locked drag). When a click or drag releases its button,
    /// held modifiers release too (Ctrl + tap = Ctrl + click).
    func button(_ button: ControlButtonKind, down: Bool) {
        channel?.button(button, down: down)
        // A click also places the computer's caret: the hidden field starts afresh.
        if down { restartField?("click") } else { send(modifiers.releaseHeld()) }
    }

    // MARK: Keyboard

    var shownKeyboard: KeyboardKind { choice.shown(modifiersDown: modifiers.anyDown) }
    var temporaryEmulated: Bool { choice.temporary(modifiersDown: modifiers.anyDown) }

    func toggleKeyboard() { keyboardOpen ? closeKeyboard() : openKeyboard() }

    /// The focus computer's text fields pop the keyboard up and hide it (control-page.md §9,
    /// PCTextFocus.swift).
    private var textFocus = PCTextFocusGate()

    func pcTextFocus(_ report: PCTextFocusReport) {
        if fieldRules.textFocus(report) { restartField?("text focus") }
        textFocus.record(report)
        reevaluateTextFocus()
    }

    /// After a report, a change of the focus computer, and when a change waiting for the touchpad to be
    /// quiet is due.
    private func reevaluateTextFocus() {
        if fieldRules.focusDevice(snapshot.focusDevice) { restartField?("focus computer") }
        follow(textFocus.decide(focus: snapshot.focusDevice, active: active, keyboardOpen: keyboardOpen, now: Self.now))
        scheduleTextFocusRetry()
    }

    /// The touchpad is in use (a finger down, the locked drag held) or just stopped (TrackpadDriver):
    /// keyboard changes from the computer's text focus wait until it has been idle (TrackpadRecognizer.touchpadIdleDelay).
    func touchpadActivity(busy: Bool, touching: Bool = false) {
        if padTouching != touching { padTouching = touching }
        textFocus.touchpad(busy: busy, at: Self.now)
        scheduleTextFocusRetry()
    }

    private var textFocusRetry: Task<Void, Never>?

    private func scheduleTextFocusRetry() {
        textFocusRetry?.cancel()
        textFocusRetry = nil
        guard let at = textFocus.retryAt() else { return }
        textFocusRetry = Task { @MainActor [weak self] in
            try? await Task.sleep(for: .seconds(max(0, at - ControlModel.now)))
            guard !Task.isCancelled, let self else { return }
            self.textFocusRetry = nil
            self.reevaluateTextFocus()
        }
    }

    private func follow(_ action: PCTextFocusAction) {
        switch action {
        case .none: break
        case .open(let secure): choice.secure = secure; keyboardOpen = true
        case .follow(let secure): choice.secure = secure
        case .close: closeKeyboard(byUser: false)
        }
    }

    /// The keyboard button: a keyboard opened here never hides with the computer's focus.
    func openKeyboard() {
        textFocus.userOpened()
        keyboardOpen = true
    }

    /// The header button and the system keyboard's own dismissal (the user's: no pop-up again during
    /// this input focus).
    func closeKeyboard() { closeKeyboard(byUser: true) }

    /// Every modifier goes up and the F-key layer returns to the default row.
    private func closeKeyboard(byUser: Bool) {
        guard keyboardOpen else { return }
        if byUser { textFocus.userHid(focus: snapshot.focusDevice) }
        keyboardOpen = false
        capsLock(down: false)
        choice.secure = false
        slide.reset()
        send(pressed.releaseAll())
        send(modifiers.releaseAll())
        fn.reset()
        symbols = false
    }

    /// 切换键盘 key. In the temporary emulated state it means "back to the system keyboard":
    /// all modifiers release. A manual choice is remembered.
    func switchKeyboard() {
        // The password keyboard (§9) means "to the system keyboard" like the emulated one.
        if choice.secure {
            choice.secure = false
            if choice.preferred == .system { send(modifiers.releaseAll()); return }
        }
        if temporaryEmulated || choice.preferred == .emulated {
            send(modifiers.releaseAll())
            choice.preferred = .system
        } else {
            choice.preferred = .emulated
        }
        // Remembered for the current orientation only (user 2026-11-12).
        if landscape {
            preferredLandscape = choice.preferred
            defaults.set(choice.preferred.rawValue, forKey: OinkPrefs.keyboardKindLandscape)
        } else {
            preferredPortrait = choice.preferred
            defaults.set(choice.preferred.rawValue, forKey: OinkPrefs.keyboardKindPortrait)
        }
    }

    // MARK: Landscape: orientation, full screen, top bar, expanded keyboard

    /// The page's layout turned (rotation, 全屏 or 恢复): the keyboard choice of that orientation
    /// applies; an open keyboard swaps in place to it. The keys under the fingers change.
    func setLandscape(_ value: Bool) {
        guard value != landscape else { return }
        landscape = value
        let wanted = value ? preferredLandscape : preferredPortrait
        if choice.preferred != wanted {
            if wanted == .system { send(modifiers.releaseAll()) }
            choice.preferred = wanted
        }
        slide.reset()
        updateBar()
    }

    /// The user left the control page (ControlPageExit: another tab, or the app in the background —
    /// not merely inactive): every 全屏 / 恢复 rule ends and the orientation goes back to the system
    /// (also for the design states, which never activate). The input session ends separately
    /// (`deactivate`) when the page is left or the app enters the background.
    func leftPage() {
        fullscreen.left()
        watchTilt(false)
        OrientationLock.apply(.all)
    }

    /// 全屏: the interface turns to landscape even while the phone is upright.
    func enterFullscreen() {
        fullscreen.enter()
        applyFullscreen()
    }

    /// 恢复: back to portrait even while the phone is sideways (see FullscreenRules for when
    /// turning the phone enters the full screen again).
    func restoreFromFullscreen() {
        fullscreen.restore(upright: Self.phoneUpright)
        applyFullscreen()
    }

    private func applyFullscreen() {
        switch fullscreen.request {
        case .system: OrientationLock.apply(.all)
        case .landscape: OrientationLock.apply(.landscape)
        case .portrait: OrientationLock.apply(.portrait)
        }
        watchTilt(fullscreen.watchesTilt)
    }

    /// The phone's own tilt (not the interface's): upright, sideways, or unknown (lying flat).
    private static var phoneUpright: Bool? {
        switch UIDevice.current.orientation {
        case .portrait, .portraitUpsideDown: true
        case .landscapeLeft, .landscapeRight: false
        default: nil
        }
    }

    /// After 恢复 from a 全屏 full screen with the phone still sideways: once it is upright, turning
    /// it enters the full screen again.
    private func watchTilt(_ on: Bool) {
        if let tiltObserver { NotificationCenter.default.removeObserver(tiltObserver) }
        tiltObserver = nil
        guard on else { return }
        UIDevice.current.beginGeneratingDeviceOrientationNotifications()
        tiltObserver = NotificationCenter.default.addObserver(forName: UIDevice.orientationDidChangeNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, let upright = Self.phoneUpright else { return }
                self.fullscreen.tilt(upright: upright)
                if !self.fullscreen.watchesTilt { self.applyFullscreen() }
            }
        }
    }

    /// The Oink keyboard drawn over the whole landscape screen, or back to its floating container;
    /// remembered.
    func setKeyboardExpanded(_ value: Bool) {
        guard value != keyboardExpanded else { return }
        keyboardExpanded = value
        defaults.set(value, forKey: OinkPrefs.keyboardExpanded)
        slide.reset()
    }

    /// A keyboard still hides the floating bar. Pad use only changes event routing; it never
    /// changes visibility, and there is no idle timer or delayed return.
    private func updateBar() {
        barShown = adjustOpen || !keyboardOpen
    }

    // MARK: Caps Lock

    /// Caps goes down/up with the touch, including slide-off and cancellation. It has no repeat
    /// timer and leaves modifiers and any other key's repeat alone.
    func capsLock(down: Bool) { send(caps.setDown(down)) }

    /// Tap: hold / release (a double tap = one press + release). Long press: lock / unlock.
    func tapModifier(_ m: Modifier) { send(modifiers.tap(m, at: Self.now)) }
    func longPressModifier(_ m: Modifier) { send(modifiers.longPress(m)) }
    func tapFn() { fn.tap(at: Self.now) }

    /// Control-area click keys (Esc, Home…, Tab, Del, F keys): down + up on release, no repeat,
    /// then held modifiers release (unless another finger still holds a combining key, §3.1.4).
    func clickKey(_ code: UInt32) {
        send([KeyEvent(code: code, down: true), KeyEvent(code: code, down: false)])
        send(pressed.afterClick(board: &modifiers))
    }

    func fKey(_ n: Int) {
        clickKey(HID.function(n))
        fn.usedFKey()
    }

    func combo(_ key: ComboKey) {
        send(key.events(target: keyTarget, board: modifiers))
        send(pressed.afterClick(board: &modifiers))
    }

    /// Play / pause and mute: down + up on release; media keys never take or release modifiers.
    func mediaClick(_ code: UInt32) { send([KeyEvent(code: code, down: true), KeyEvent(code: code, down: false)]) }

    /// Held keys (arrows, rewind / fast-forward, volume, space, backspace, return), by key id:
    /// down when the first finger enters, then repeated while it is the newest held key…
    /// `combines` is false for media keys.
    func heldDown(_ id: String, _ code: UInt32, combines: Bool) {
        pressDown(id, pressed.holdDown(id, code: code, combines: combines))
    }
    /// …up when the last finger leaves or lifts; held modifiers let go once no combining key is
    /// down any more.
    func heldUp(_ id: String) { send(pressed.up(id, board: &modifiers)) }

    /// Emulated keyboard character keys, mapped through 键盘布局 (auto-Shift / AltGr when the
    /// character needs them); repeated like held keys; up via `heldUp`. A dead key is tapped with
    /// a Space after it, and a character the layout has no key for goes as text; both then let
    /// the held modifiers go like a click (the same rules as Android's KeyboardModel).
    func typedDown(_ id: String, _ c: Character) {
        guard pressed.entries[id] == nil,
              let typed = keyboardLayout.typedKey(c, target: keyTarget, board: modifiers) else { return }
        switch typed {
        case .key(let key) where key.dead:
            send(key.deadEvents)
            send(pressed.afterClick(board: &modifiers))
        case .key(let key):
            pressDown(id, pressed.typedDown(id, key))
        case .text(let text):
            commitText(text)
            send(pressed.afterClick(board: &modifiers))
        }
    }

    /// A key that just went down (`events` empty: another finger already holds it) becomes the
    /// one that repeats.
    private func pressDown(_ id: String, _ events: [KeyEvent]) {
        guard !events.isEmpty else { return }
        send(events)
        // The repeat timer runs on the main run loop.
        repeater.start(id) { [weak self] id in MainActor.assumeIsolated { self?.repeatKey(id) } }
    }

    /// The repeating key left the pressed set (up, slide-off, cancel, layer or keyboard switch,
    /// keyboard closed): its repeat stops at once.
    private func stopRepeatIfUp() {
        if let key = repeater.key, pressed.entries[key] == nil { repeater.stopAll() }
    }

    /// One repeat: the held key's down again; the router forwards it as `KeyRepeat` to the
    /// computer that got the press.
    private func repeatKey(_ id: String) {
        guard let entry = pressed.entries[id] else { return repeater.stopAll() }
        channel?.key(entry.code, down: true)
    }

    // System keyboard (hidden field, RemoteInputField)

    /// The hidden field mirrors the text in front of the focus computer's caret (control-page.md
    /// §3.1.1): whatever else types there or moves that caret — the page's keys, a touchpad click,
    /// a hardware key, another text field or computer (FieldRestartRules) — restarts it. Set by
    /// the field on screen.
    var restartField: ((String) -> Void)?
    private var fieldRules = FieldRestartRules()
    /// The field's own text and keys are going out (they do not restart it).
    private var fromField = false

    /// Text the page types as text (a character the layout has no key for): the field restarts.
    func commitText(_ text: String) {
        sendText(text)
        restartField?("page text")
    }

    /// One ordered stream: a rejected piece stops the rest, so nothing arrives out of order. Line
    /// breaks and tabs are the Return / Tab keys.
    private func sendText(_ text: String) {
        guard let channel else { noTargetPrompt += 1; return }
        for piece in TextPiece.pieces(text) {
            switch piece {
            case .text(let value):
                do { try channel.inputText(value) } catch { errorText = Self.message(error); return }
            case .key(let code): send([KeyEvent(code: code, down: true), KeyEvent(code: code, down: false)])
            }
        }
    }

    /// A hardware keyboard (Bluetooth / USB): its real HID usages go out as they are, with no
    /// three-state modifier rules (§3.1 实体键盘).
    func hardwareKey(_ usage: Int, down: Bool) {
        guard usage > 0, usage < 0xFFFF else { return }
        send([KeyEvent(code: HID.key(UInt16(usage)), down: down)])
    }

    /// Every key event goes out here. A key down that may type, delete or move the computer's
    /// caret, from anywhere but the hidden field, restarts the field.
    private func send(_ events: [KeyEvent]) {
        guard let channel else { return }
        for e in events { channel.key(e.code, down: e.down) }
        if !fromField, events.contains(where: { $0.down && FieldRestartRules.restarts(keyDown: $0.code) }) {
            restartField?("key")
        }
    }

    static var now: TimeInterval { ProcessInfo.processInfo.systemUptime }

    #if DEBUG
    /// Design-state screenshots (`-oink-ui-scene control-*`): sets the state the views read.
    func installScene(snapshot: ControlSnapshot, keyboard: KeyboardKind?, preferred: KeyboardKind = .system,
                      modifiers mods: [Modifier: ModifierState] = [:], fn fnState: ModifierState? = nil, symbols: Bool = false,
                      expanded: Bool = false) {
        self.snapshot = snapshot
        choice = KeyboardChoice(preferred: preferred)
        // A scene's keyboard holds in both orientations (not saved).
        preferredPortrait = preferred
        preferredLandscape = preferred
        var board = ModifierBoard()
        for (m, s) in mods { _ = s == .locked ? board.longPress(m) : board.tap(m, at: 0) }
        modifiers = board
        var layer = FnLayer()
        if let fnState { layer.tap(at: 0); if fnState == .locked { layer.tap(at: 0.1) } }
        fn = layer
        self.symbols = symbols
        keyboardExpanded = expanded
        keyboardOpen = keyboard != nil
    }
    #endif
}

/// Supported orientations while the control page applies 全屏 / 恢复 (the app delegate reads it).
@MainActor
enum OrientationLock {
    static var mask: UIInterfaceOrientationMask = .all

    static func apply(_ value: UIInterfaceOrientationMask) {
        guard value != mask else { return }
        mask = value
        for scene in UIApplication.shared.connectedScenes.compactMap({ $0 as? UIWindowScene }) {
            scene.windows.first?.rootViewController?.setNeedsUpdateOfSupportedInterfaceOrientations()
            scene.requestGeometryUpdate(.iOS(interfaceOrientations: value)) { _ in }
        }
    }
}

final class OinkAppDelegate: NSObject, UIApplicationDelegate {
    func application(_ application: UIApplication, supportedInterfaceOrientationsFor window: UIWindow?) -> UIInterfaceOrientationMask {
        MainActor.assumeIsolated { OrientationLock.mask }
    }

    /// Called when iOS ends the running process, e.g. the user swipes Oink
    /// away in the app switcher while audio keeps it running in the
    /// background: the speaker / microphone Live Activities end with it
    /// (AudioControlsPublisher.endActivitiesBeforeExit).
    func applicationWillTerminate(_ application: UIApplication) {
        AudioControlsPublisher.endActivitiesBeforeExit()
    }
}

extension ControlModel: RemoteInputSink {
    /// Committed text from the hidden field (its change since the last edit, RemoteInputField).
    func fieldText(_ text: String) {
        fromField = true
        defer { fromField = false }
        sendText(text)
    }

    /// Backspace from the hidden field: a deleted committed character, or Backspace on the empty
    /// field (each repeat of a held delete key is another one).
    func fieldBackspace() {
        fromField = true
        defer { fromField = false }
        send([KeyEvent(code: HID.backspace, down: true), KeyEvent(code: HID.backspace, down: false)])
    }
}
