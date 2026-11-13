import Foundation

// The control page's link to the phone's embedded router (CHANGE-PLAN §3, protocol-v2 §14.6):
// `MobileSession.control(listener)` → `MobileControl`. The page talks to `ControlChannel`; the
// only implementation calls the real FFI.

enum ControlButtonKind: Equatable { case left, right, middle }

struct ControlTargetInfo: Equatable, Identifiable {
    var device: String
    var name: String
    /// "macos", "windows" or "android".
    var platform: String
    var online: Bool
    var id: String { device }
}

/// `MobileControlState`.
struct ControlSnapshot: Equatable {
    var focusDevice: String?
    var selectedDevice: String?
    /// The latest input was dropped: no focus and nothing selected.
    var noTarget = false
    var locked = false
    var gameMode = ControlGame.off
    var gameActive = false
    var targets: [ControlTargetInfo] = []
}

/// `ControlEdge` (focus computer → this source: the remote pointer is held at a screen edge or is
/// about to cross). Received and kept; the page does not draw it in 0.4.0.
struct ControlEdgeReport: Equatable {
    enum State: Equatable { case clear, blocked, approaching }
    enum Side: Equatable { case left, right, top, bottom }
    var state: State
    var side: Side
    var toDevice: String?
}

/// One finger on the pad for the core's raw contacts, millimetres from the pad's top left.
struct PadContact: Equatable {
    var id: UInt64
    var x: Float
    var y: Float
}

/// The core's shared multi-finger recognizer on one touch event: what it does with the touch, and
/// its outputs, which `forward()` sends after what the page sends itself for the same event.
struct PadCoreFrame {
    var claim: CoreClaim
    var forward: () -> Void
    static var unclaimed: PadCoreFrame { PadCoreFrame(claim: .unclaimed, forward: {}) }
}

@MainActor
protocol ControlChannel: AnyObject {
    func setSettings(pointerSpeed: UInt8, pointsPerMm: Float)
    /// One combined motion per touch event; `timeUs` / `sinceUs` are the touch times (µs of
    /// system uptime) of this motion and of the sample it is measured from.
    func pointerMove(dx: Float, dy: Float, timeUs: UInt64, sinceUs: UInt64)
    func button(_ button: ControlButtonKind, down: Bool)
    /// A touchpad scroll of the fingers with its phase, timed like `pointerMove`.
    func scroll(phase: PadScrollPhase, dx: Float, dy: Float, timeUs: UInt64, sinceUs: UInt64)
    /// Every finger on the pad (millimetres from its top left) after a touch event: the trackpad's
    /// raw contacts, and the same frame through the core's multi-finger recognizer (shared with
    /// Android). `multiFinger`: the page leaves the touch's fingers to it.
    func touches(timeUs: UInt64, width: Float, height: Float, touches: [PadContact], cancelled: Bool,
                 multiFinger: Bool) -> PadCoreFrame
    /// A gesture the page recognised itself (the two-finger double tap's smart zoom).
    func gesture(_ gesture: PadGesture)
    func key(_ code: UInt32, down: Bool)
    func inputText(_ text: String) throws
    func movePointer(to device: String) throws
    func setFocusLock(_ locked: Bool) throws
    func setGameMode(_ mode: ControlGame) throws
    func close()
}

enum ControlChannels {
    /// Opens the control handle on the live session. Nil when there is no session.
    @MainActor
    static func open(core: CoreSession, onState: @escaping (ControlSnapshot) -> Void,
                     onEdge: @escaping (ControlEdgeReport) -> Void,
                     onTextRefused: @escaping (TextRefusal) -> Void,
                     onTextFocus: @escaping (PCTextFocusReport) -> Void) -> ControlChannel? {
        #if OINK_MOBILE_CORE_FFI
        guard let session = core.mobileSession else { return nil }
        return FFIControlChannel(session: session, onState: onState, onEdge: onEdge, onTextRefused: onTextRefused,
                                 onTextFocus: onTextFocus)
        #else
        return nil
        #endif
    }
}

#if OINK_MOBILE_CORE_FFI
/// Listener callbacks arrive on a backend thread; they are moved to the main actor here.
private final class ControlListener: MobileControlListener, @unchecked Sendable {
    let state: @Sendable (MobileControlState) -> Void
    let edge: @Sendable (ControlEdge) -> Void
    let refused: @Sendable (ControlTextRefusal) -> Void
    let textFocus: @Sendable (String, ControlTextFocus) -> Void
    init(state: @escaping @Sendable (MobileControlState) -> Void, edge: @escaping @Sendable (ControlEdge) -> Void,
         refused: @escaping @Sendable (ControlTextRefusal) -> Void, textFocus: @escaping @Sendable (String, ControlTextFocus) -> Void) {
        self.state = state; self.edge = edge; self.refused = refused; self.textFocus = textFocus
    }
    func onState(state value: MobileControlState) { state(value) }
    func onEdge(edge value: ControlEdge) { edge(value) }
    func onTextRefused(reason: ControlTextRefusal) { refused(reason) }
    /// The focus computer's text-input focus changed, or the page just opened (protocol-v2 §14.7).
    func onTextFocus(device: String, focus: ControlTextFocus) { textFocus(device, focus) }
}

@MainActor
final class FFIControlChannel: ControlChannel {
    private let control: MobileControl

    init(session: MobileSession, onState: @escaping (ControlSnapshot) -> Void, onEdge: @escaping (ControlEdgeReport) -> Void,
         onTextRefused: @escaping (TextRefusal) -> Void, onTextFocus: @escaping (PCTextFocusReport) -> Void) {
        let listener = ControlListener(state: { value in
            let snapshot = ControlSnapshot(
                focusDevice: value.focusDevice, selectedDevice: value.selectedDevice, noTarget: value.noTarget,
                locked: value.locked, gameMode: Self.game(value.gameMode), gameActive: value.gameActive,
                targets: value.targets.map { ControlTargetInfo(device: $0.device, name: $0.name, platform: $0.platform, online: $0.online) })
            Task { @MainActor in onState(snapshot) }
        }, edge: { value in
            let report = ControlEdgeReport(state: Self.edgeState(value.state), side: Self.edgeSide(value.edge), toDevice: value.toDevice)
            Task { @MainActor in onEdge(report) }
        }, refused: { reason in
            let refusal = Self.refusal(reason)
            Task { @MainActor in onTextRefused(refusal) }
        }, textFocus: { device, focus in
            let report = PCTextFocusReport(device: device, seq: focus.seq, editable: focus.editable, secure: focus.secure)
            Task { @MainActor in onTextFocus(report) }
        })
        control = session.control(listener: listener)
    }

    nonisolated private static func game(_ mode: ControlGameMode) -> ControlGame {
        switch mode { case .off: .off; case .on: .on; case .auto: .auto }
    }
    nonisolated private static func refusal(_ reason: ControlTextRefusal) -> TextRefusal {
        switch reason { case .paused: .paused; case .notJoined: .notJoined; case .duplicate: .duplicate; case .unsupported: .unsupported }
    }
    nonisolated private static func edgeState(_ state: ControlEdgeState) -> ControlEdgeReport.State {
        switch state { case .clear: .clear; case .blocked: .blocked; case .approaching: .approaching }
    }
    nonisolated private static func edgeSide(_ side: ControlEdgeSide) -> ControlEdgeReport.Side {
        switch side { case .left: .left; case .right: .right; case .top: .top; case .bottom: .bottom }
    }
    private static func ffiButton(_ button: ControlButtonKind) -> ControlButton {
        switch button { case .left: .left; case .right: .right; case .middle: .middle }
    }
    private static func ffiGame(_ mode: ControlGame) -> ControlGameMode {
        switch mode { case .off: .off; case .on: .on; case .auto: .auto }
    }

    func setSettings(pointerSpeed: UInt8, pointsPerMm: Float) {
        control.setSettings(settings: MobileControlSettings(pointerSpeed: pointerSpeed, pointsPerMm: pointsPerMm))
    }
    func pointerMove(dx: Float, dy: Float, timeUs: UInt64, sinceUs: UInt64) {
        control.pointerMove(dx: dx, dy: dy, timeUs: timeUs, sinceUs: sinceUs)
    }
    func button(_ button: ControlButtonKind, down: Bool) {
        control.button(button: Self.ffiButton(button), down: down)
    }
    func scroll(phase: PadScrollPhase, dx: Float, dy: Float, timeUs: UInt64, sinceUs: UInt64) {
        let ffi: ControlScrollPhase = switch phase {
        case .began: .began
        case .changed: .changed
        case .ended: .ended
        case .cancelled: .cancelled
        }
        control.scroll(phase: ffi, dx: dx, dy: dy, timeUs: timeUs, sinceUs: sinceUs)
    }
    func touches(timeUs: UInt64, width: Float, height: Float, touches: [PadContact], cancelled: Bool,
                 multiFinger: Bool) -> PadCoreFrame {
        let list = touches.map { ControlTouch(id: $0.id, x: $0.x, y: $0.y) }
        control.touches(timeUs: timeUs, width: width, height: height, touches: list, cancelled: cancelled)
        let recognition = control.recognize(timeUs: timeUs, touches: list, cancelled: cancelled, multiFinger: multiFinger)
        let claim: CoreClaim = switch recognition.claim {
        case .unclaimed: .unclaimed
        case .scroll: .scroll
        case .gesture: .gesture
        }
        let outputs = recognition.outputs
        if outputs.isEmpty { return PadCoreFrame(claim: claim, forward: {}) }
        let control = self.control
        return PadCoreFrame(claim: claim, forward: { control.sendRecognized(outputs: outputs) })
    }
    func gesture(_ gesture: PadGesture) {
        let ffi: ControlGesture = switch gesture {
        case .smartMagnify: .smartMagnify
        }
        control.gesture(gesture: ffi)
    }
    func key(_ code: UInt32, down: Bool) { control.key(code: code, down: down) }
    func inputText(_ text: String) throws { try control.inputText(text: text) }
    func movePointer(to device: String) throws { try control.movePointerTo(device: device) }
    func setFocusLock(_ locked: Bool) throws { try control.setFocusLock(locked: locked) }
    func setGameMode(_ mode: ControlGame) throws {
        try control.setGameMode(mode: Self.ffiGame(mode))
    }
    func close() { control.leave() }
}
#endif
