import XCTest
@testable import OinkAudioTestHost

/// The keyboard follows the focus computer's text fields (control-page.md §9, protocol-v2 §14.7).
final class PCTextFocusTests: XCTestCase {
    private static let pc = String(repeating: "a", count: 64)
    private static let mac = String(repeating: "b", count: 64)

    private func r(_ seq: UInt64, _ editable: Bool, secure: Bool = false,
                   device: String = PCTextFocusTests.pc) -> PCTextFocusReport {
        PCTextFocusReport(device: device, seq: seq, editable: editable, secure: secure)
    }

    /// The gate with the page's keyboard state, applying each action like ControlModel does.
    private struct Page {
        /// 键盘自动弹出 turned on (the default is off: `testWithAutoPopupOffFocusNeverChangesTheKeyboard`).
        var gate: PCTextFocusGate = { var gate = PCTextFocusGate(); gate.autoPopup = true; return gate }()
        var open = false
        var secure = false
        var focus: String? = PCTextFocusTests.pc
        var active = true
        /// System uptime (binary fractions keep the sums exact); the pad has never been touched.
        var t: TimeInterval = 1000

        @discardableResult
        mutating func apply(_ action: PCTextFocusAction) -> PCTextFocusAction {
            switch action {
            case .none: break
            case .open(let s): open = true; secure = s
            case .follow(let s): secure = s
            case .close: open = false; secure = false
            }
            return action
        }
        @discardableResult
        mutating func report(_ report: PCTextFocusReport) -> PCTextFocusAction {
            apply(gate.report(report, focus: focus, active: active, keyboardOpen: open, now: t))
        }
        @discardableResult
        mutating func evaluate() -> PCTextFocusAction {
            apply(gate.decide(focus: focus, active: active, keyboardOpen: open, now: t))
        }
        mutating func userHide() { gate.userHid(focus: focus); open = false; secure = false }
        mutating func userOpen() { gate.userOpened(); open = true }
        mutating func touchpad(_ busy: Bool) { gate.touchpad(busy: busy, at: t) }
    }

    func testWithAutoPopupOffFocusNeverChangesTheKeyboard() {
        var page = Page()
        page.gate = PCTextFocusGate()
        XCTAssertFalse(page.gate.autoPopup, "off by default")
        XCTAssertEqual(page.report(r(1, true)), .none)
        XCTAssertEqual(page.report(r(2, true, secure: true)), .none, "no password keyboard either")
        XCTAssertNil(page.gate.retryAt())
        // The user's own keyboard: focus leaving does not hide it.
        page.userOpen()
        XCTAssertEqual(page.report(r(2, false)), .none)
        XCTAssertTrue(page.open)
        page.userHide()
        // Turned on while a field has focus: it pops up (the user hid none during this focus).
        page.gate.autoPopup = true
        XCTAssertEqual(page.report(r(3, true)), .open(secure: false))
        // Turned off again: that keyboard is the user's now and focus no longer hides it.
        page.gate.autoPopup = false
        XCTAssertEqual(page.report(r(3, false)), .none)
        page.gate.autoPopup = true
        XCTAssertEqual(page.evaluate(), .none)
        XCTAssertTrue(page.open)
    }

    func testAFieldPopsTheKeyboardUpAndLeavingHidesIt() {
        var page = Page()
        XCTAssertEqual(page.report(r(1, true)), .open(secure: false))
        XCTAssertEqual(page.evaluate(), .none)
        XCTAssertEqual(page.report(r(1, false)), .close)
        XCTAssertFalse(page.open)
    }

    func testAKeyboardTheUserHidStaysHiddenUntilTheNextInputFocus() {
        var page = Page()
        page.report(r(1, true))
        page.userHide()
        XCTAssertEqual(page.evaluate(), .none)
        XCTAssertEqual(page.report(r(1, true)), .none)
        // Leaving and coming back is a new input focus.
        page.report(r(1, false))
        XCTAssertEqual(page.report(r(2, true)), .open(secure: false))
    }

    func testAKeyboardTheUserOpenedNeverHidesWithFocus() {
        var page = Page()
        page.userOpen()
        XCTAssertEqual(page.report(r(1, true)), .none)
        XCTAssertEqual(page.report(r(1, false)), .none)
        XCTAssertTrue(page.open)
    }

    func testAPasswordFieldPopsTheOinkKeyboardAndTheNextFieldFollowsThePreference() {
        var page = Page()
        XCTAssertEqual(page.report(r(1, true, secure: true)), .open(secure: true))
        page.report(r(1, false))
        XCTAssertEqual(page.report(r(2, true)), .open(secure: false))
        // Tab from a user name to a password field while popped up: the keyboard follows.
        XCTAssertEqual(page.report(r(3, true, secure: true)), .follow(secure: true))
        XCTAssertEqual(page.report(r(4, true)), .follow(secure: false))
    }

    func testOnlyTheFocusComputerAndOnlyWhileThePageShows() {
        var page = Page()
        XCTAssertEqual(page.report(r(1, true, device: Self.mac)), .none)
        page.active = false
        XCTAssertEqual(page.report(r(1, true)), .none)
        // The page shows again while that field still has focus: the keyboard pops up.
        page.active = true
        XCTAssertEqual(page.evaluate(), .open(secure: false))
    }

    func testFocusMovingToAnotherComputerWaitsForItsState() {
        var page = Page()
        page.report(r(1, true))
        page.focus = Self.mac
        XCTAssertEqual(page.evaluate(), .none)
        XCTAssertEqual(page.report(r(7, false, device: Self.mac)), .close)
    }

    func testAChangeWhileTheTouchpadIsIdleAppliesAtOnce() {
        var page = Page()
        page.touchpad(true); page.t += 0.5; page.touchpad(false)
        page.t += TrackpadRecognizer.touchpadIdleDelay
        XCTAssertEqual(page.report(r(1, true)), .open(secure: false))
        XCTAssertNil(page.gate.retryAt())
    }

    func testAChangeDuringTouchWaitsUntilThePadHasBeenIdleFor1200Ms() {
        var page = Page()
        page.touchpad(true)
        XCTAssertEqual(page.report(r(1, true)), .none)
        XCTAssertNil(page.gate.retryAt(), "no retry while a finger is down")
        page.t += 0.5; page.touchpad(true)
        page.t += 0.25; page.touchpad(false)
        let lift = page.t
        XCTAssertEqual(page.gate.retryAt(), lift + TrackpadRecognizer.touchpadIdleDelay)
        page.t = lift + 1.199
        XCTAssertEqual(page.evaluate(), .none)
        page.t = lift + TrackpadRecognizer.touchpadIdleDelay
        XCTAssertEqual(page.evaluate(), .open(secure: false))
        XCTAssertNil(page.gate.retryAt())
        // Hiding waits too: the pointer crossed to another computer without a field.
        page.touchpad(true)
        page.focus = Self.mac
        XCTAssertEqual(page.report(r(4, false, device: Self.mac)), .none)
        page.touchpad(false)
        page.t += TrackpadRecognizer.touchpadIdleDelay
        XCTAssertEqual(page.evaluate(), .close)
    }

    func testOnlyTheLatestOfTheChangesDuringTheWaitApplies() {
        var page = Page()
        page.touchpad(true)
        // Across three computers while the finger is down: a field, none, a password field.
        page.report(r(1, true))
        page.focus = Self.mac
        page.report(r(2, false, device: Self.mac))
        page.focus = Self.pc
        page.report(r(3, true, secure: true))
        page.touchpad(false)
        page.t += TrackpadRecognizer.touchpadIdleDelay
        XCTAssertEqual(page.evaluate(), .open(secure: true))
        // A field gained and left again during the wait: nothing happens at all.
        page.report(r(3, false))
        XCTAssertFalse(page.open)
        page.touchpad(true)
        page.report(r(4, true))
        page.report(r(4, false))
        page.touchpad(false)
        page.t += TrackpadRecognizer.touchpadIdleDelay
        XCTAssertEqual(page.evaluate(), .none)
        XCTAssertFalse(page.open)
    }

    func testThePasswordKeyboardIsShownWithoutChangingThePreference() {
        var choice = KeyboardChoice(preferred: .system)
        choice.secure = true
        XCTAssertEqual(choice.shown(modifiersDown: false), .emulated)
        XCTAssertFalse(choice.temporary(modifiersDown: true))
        XCTAssertEqual(choice.preferred, .system)
        choice.secure = false
        XCTAssertEqual(choice.shown(modifiersDown: false), .system)
    }
}
