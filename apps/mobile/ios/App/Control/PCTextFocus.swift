import Foundation

/// The focus computer's text-input focus (`ControlTextFocus`, protocol-v2 §14.7). Never any text.
struct PCTextFocusReport: Equatable {
    var device: String
    /// Input-focus episode: increases once per keyboard focus into an editable element.
    var seq: UInt64
    var editable: Bool
    /// A password field.
    var secure: Bool
}

/// What the control page does with its keyboard after a text-input focus change.
enum PCTextFocusAction: Equatable {
    case none
    /// Pop the keyboard up: the Oink keyboard for a password field, else the remembered kind.
    case open(secure: Bool)
    /// The popped-up keyboard follows focus to another field (password field ↔ ordinary field).
    case follow(secure: Bool)
    /// Focus left the field: the popped-up keyboard hides.
    case close
}

/// The keyboard follows the focus computer's text fields (control-page.md §9):
/// - an editable field gets focus → the keyboard pops up (the remembered kind; the Oink keyboard for a
///   password field, without changing the remembered kind);
/// - focus leaves it (no editable element, no focus) → only a keyboard that popped up this way hides;
///   a keyboard the user opened never does;
/// - the user hides a keyboard during an input focus → no pop-up until the next input focus (another
///   field, or leaving and coming back: a new `seq`).
/// Only while the page is showing, and only for its current focus computer. The core sends every change
/// and the current state when the page opens.
///
/// While the touchpad is in use (a finger down, the locked drag held) and until it has been idle for
/// `TrackpadRecognizer.touchpadIdleDelay` (1.2 s), nothing changes (user 2026-11-12): moving the pointer across to another computer must
/// not pop the keyboard up or hide it, which would resize the pad under the finger. A change that waits
/// is applied from the latest state once the pad is quiet; changes in between are never applied.
struct PCTextFocusGate {

    private struct Episode: Equatable {
        var device: String
        var seq: UInt64
    }
    private var latest: PCTextFocusReport?
    /// The keyboard is open because focus popped it up, for this episode and kind.
    private var popped: (episode: Episode, secure: Bool)?
    private var suppressed: Episode?
    /// 键盘自动弹出 (`OinkPrefs.keyboardAutoPopup`, off by default, user 2026-11-12). Off: focus never pops the
    /// keyboard up or hides it (so no password keyboard either); the user's own opening and hiding work as
    /// always, and a keyboard popped up before it was turned off counts as the user's.
    var autoPopup = false
    private var touching = false
    private var lastTouch: TimeInterval = -.infinity
    /// A decision waits for the touchpad to be quiet.
    private(set) var pending = false

    /// A report from the core; then `decide`.
    mutating func record(_ report: PCTextFocusReport) { latest = report }

    mutating func report(_ report: PCTextFocusReport, focus: String?, active: Bool, keyboardOpen: Bool,
                         now: TimeInterval) -> PCTextFocusAction {
        record(report)
        return decide(focus: focus, active: active, keyboardOpen: keyboardOpen, now: now)
    }

    /// The touchpad is in use (`busy`: a finger down, the locked drag held) or just stopped, at `now`
    /// (system uptime). Report every change of `busy`; activity while busy may be reported too.
    mutating func touchpad(busy: Bool, at now: TimeInterval) {
        if busy || touching { lastTouch = now }
        touching = busy
    }

    /// When `decide` should run again for a waiting change; nil when nothing waits or the pad is in use.
    func retryAt() -> TimeInterval? { pending && !touching ? idleAt : nil }

    /// Compared as `now < lastTouch + delay` (never as a difference) so `retryAt()` itself is idle.
    private var idleAt: TimeInterval { lastTouch + TrackpadRecognizer.touchpadIdleDelay }

    /// After a report, a change of the focus computer or of the page's visibility, or at `retryAt()`.
    mutating func decide(focus: String?, active: Bool, keyboardOpen: Bool, now: TimeInterval) -> PCTextFocusAction {
        guard autoPopup else {
            pending = false
            popped = nil
            return .none
        }
        if touching || now < idleAt {
            pending = true
            return .none
        }
        pending = false
        return evaluate(focus: focus, active: active, keyboardOpen: keyboardOpen)
    }

    private mutating func evaluate(focus: String?, active: Bool, keyboardOpen: Bool) -> PCTextFocusAction {
        if !keyboardOpen { popped = nil }
        guard active else { return .none }
        guard let focus else { return hidePopped() }
        // Another computer just became the focus: wait for its state.
        guard let state = latest, state.device == focus else { return .none }
        guard state.editable else { return hidePopped() }
        let episode = Episode(device: state.device, seq: state.seq)
        if let popped {
            if popped.episode == episode && popped.secure == state.secure { return .none }
            self.popped = (episode, state.secure)
            return .follow(secure: state.secure)
        }
        if keyboardOpen || suppressed == episode { return .none }
        popped = (episode, state.secure)
        return .open(secure: state.secure)
    }

    private mutating func hidePopped() -> PCTextFocusAction {
        guard popped != nil else { return .none }
        popped = nil
        return .close
    }

    /// The user hid the keyboard (keyboard button, system dismissal): no pop-up again during this input focus.
    mutating func userHid(focus: String?) {
        popped = nil
        if let state = latest, state.device == focus, state.editable {
            suppressed = Episode(device: state.device, seq: state.seq)
        }
    }

    /// The user opened the keyboard: focus never hides it.
    mutating func userOpened() { popped = nil }
}
