import CoreGraphics
import XCTest
@testable import OinkAudioTestHost

/// Sliding between keys and several fingers (docs control-page.md §3.1.4, user 2026-11-11): the
/// spec's examples as key event sequences, plus the bubble owner and the hit test. The keys act
/// as ControlModel's do: held keys through PressedKeys, tap keys as a click then the
/// held-modifier release.
final class KeySlideTests: XCTestCase {
    private var sent: [KeyEvent] = []
    private var board = ModifierBoard()
    private var pressed = PressedKeys()
    private var feel: [Bool] = []
    private var slide: KeySlide!

    override func setUp() {
        sent = []; board = ModifierBoard(); pressed = PressedKeys(); feel = []
        slide = KeySlide()
        slide.feel = { [unowned self] in self.feel.append($0) }
    }

    /// A character key on a US Windows computer (the shared Rust tables), with the modifiers down now.
    private func usKey(_ c: Character, _ board: ModifierBoard = ModifierBoard()) -> TypedKey {
        guard case .key(let key)? = KeyLayouts.typed(c, layout: "us", target: .windows, board: board) else {
            preconditionFailure("\(c) has no US key")
        }
        return key
    }
    private func ch(_ c: Character) -> SlideKey {
        let id = "vk-\(c)"
        return SlideKey(id: id, hold: true,
                        down: { [unowned self] in sent += pressed.typedDown(id, usKey(c, board)) },
                        up: { [unowned self] in sent += pressed.up(id, board: &board) })
    }
    private func hold(_ id: String, _ code: UInt32) -> SlideKey {
        SlideKey(id: id, hold: true,
                 down: { [unowned self] in sent += pressed.holdDown(id, code: code, combines: true) },
                 up: { [unowned self] in sent += pressed.up(id, board: &board) })
    }
    private func tap(_ id: String, _ code: UInt32) -> SlideKey {
        SlideKey(id: id, hold: false, fire: { [unowned self] in
            sent += [KeyEvent(code: code, down: true), KeyEvent(code: code, down: false)]
            sent += pressed.afterClick(board: &board)
        })
    }
    private func d(_ c: Character) -> KeyEvent { KeyEvent(code: usKey(c).code, down: true) }
    private func u(_ c: Character) -> KeyEvent { KeyEvent(code: usKey(c).code, down: false) }
    private func d(_ code: UInt32) -> KeyEvent { KeyEvent(code: code, down: true) }
    private func u(_ code: UInt32) -> KeyEvent { KeyEvent(code: code, down: false) }

    func testCapsFollowsRealTouchWithoutSyntheticHoldOrRepeat() {
        var caps = CapsKey()
        var at: TimeInterval = 0
        var timed: [(TimeInterval, KeyEvent)] = []
        let key = SlideKey(id: "caps", hold: true,
                           down: { timed += caps.setDown(true).map { (at, $0) } },
                           up: { timed += caps.setDown(false).map { (at, $0) } })
        for duration in [0.06, 1.2, 5.0] {
            slide.press(1, key)
            XCTAssertEqual(timed.last?.1, d(HID.capsLock), "down before lifting")
            XCTAssertEqual(slide.down, ["caps"], "ordinary pressed feedback")
            at += duration
            slide.lift(1)
            XCTAssertEqual(timed.last?.1, u(HID.capsLock), "up at lift, without a delay")
            XCTAssertEqual(timed[timed.count - 1].0 - timed[timed.count - 2].0, duration, accuracy: 0.0001)
            at += 0.04 // a rapid re-tap is a new physical press
        }
        XCTAssertEqual(timed.count, 6, "one down/up pair per touch, no repeats")
        XCTAssertFalse(caps.down)
        XCTAssertEqual(caps.setDown(false), [], "duplicate cancellation does not send another up")
    }

    func testCapsSlideOffLiftCancelAndMultipleFingers() {
        var caps = CapsKey()
        let key = SlideKey(id: "caps", hold: true,
                           down: { [unowned self] in sent += caps.setDown(true) },
                           up: { [unowned self] in sent += caps.setDown(false) })
        sent += board.tap(.ctrl, at: 0)
        slide.press(1, key)
        slide.move(1, .out)
        XCTAssertEqual(sent, [d(HID.leftControl), d(HID.capsLock), u(HID.capsLock)])
        slide.move(1, .on(key)); slide.lift(1)
        XCTAssertEqual(sent.count, 3, "sliding back after leaving cannot press again")
        slide.press(2, key); slide.cancel(2)
        slide.press(3, key); slide.press(4, key); slide.lift(3)
        XCTAssertTrue(caps.down, "last finger owns the up")
        slide.lift(4)
        XCTAssertFalse(caps.down)
        XCTAssertEqual(board.state(.ctrl), .held, "Caps leaves modifiers alone")
        XCTAssertEqual(sent, [d(HID.leftControl)] + Array(repeating: [d(HID.capsLock), u(HID.capsLock)], count: 3).flatMap { $0 })
        slide.press(5, key); slide.reset()
        XCTAssertEqual(sent.suffix(2), [d(HID.capsLock), u(HID.capsLock)], "closing cancels Caps")
    }

    func testLeftHoldsARightSlidesSToD() {
        slide.press(1, ch("a"))
        slide.press(2, ch("s")); slide.move(2, .on(ch("d"))); slide.lift(2)
        slide.lift(1)
        XCTAssertEqual(sent, [d("a"), d("s"), u("s"), d("d"), u("d"), u("a")])
    }

    func testTwoFingersOnFBothSlideToG() {
        slide.press(1, ch("f")); slide.press(2, ch("f"))
        slide.move(1, .on(ch("g")))
        slide.move(2, .on(ch("g")))
        slide.lift(1); slide.lift(2)
        XCTAssertEqual(sent, [d("f"), d("g"), u("f"), u("g")])
    }

    func testHeldCtrlReleasesWhenTheSlideEmptiesTheSet() {
        sent += board.tap(.ctrl, at: 0)
        slide.press(1, ch("a")); slide.move(1, .on(ch("s"))); slide.lift(1)
        XCTAssertEqual(sent, [d(HID.leftControl), d("a"), u("a"), u(HID.leftControl), d("s"), u("s")])
    }

    func testHeldCtrlStaysWhileAnotherFingerHoldsAKey() {
        sent += board.tap(.ctrl, at: 0)
        slide.press(1, ch("a"))
        slide.press(2, ch("s")); slide.lift(2)
        slide.lift(1)
        XCTAssertEqual(sent, [d(HID.leftControl), d("a"), d("s"), u("s"), u("a"), u(HID.leftControl)])
    }

    func testSlideToBackspaceThenOffTheKeyboard() {
        slide.press(1, ch("m")); slide.move(1, .on(hold("backspace", HID.backspace))); slide.move(1, .out); slide.lift(1)
        XCTAssertEqual(sent, [d("m"), u("m"), d(HID.backspace), u(HID.backspace)])
    }

    func testAfterLeavingTheKeyboardTheFingerPressesNothing() {
        slide.press(1, ch("a")); slide.move(1, .out); slide.move(1, .on(ch("s"))); slide.lift(1)
        XCTAssertEqual(sent, [d("a"), u("a")])
        XCTAssertNil(slide.bubble)
    }

    func testTapKeyCancelledByLeavingTheKeyboard() {
        slide.press(1, tap("home", HID.home)); slide.move(1, .out); slide.move(1, .on(tap("end", HID.end))); slide.lift(1)
        XCTAssertEqual(sent, [])
    }

    func testSlidingOverAModifierIsNoKeyNotTheEnd() {
        slide.press(1, ch("z")); slide.move(1, .none); slide.move(1, .on(ch("x"))); slide.lift(1)
        XCTAssertEqual(sent, [d("z"), u("z"), d("x"), u("x")])
    }

    func testTapKeysFireWhereTheFingerLifts() {
        slide.press(1, tap("home", HID.home)); slide.move(1, .on(tap("end", HID.end))); slide.lift(1)
        XCTAssertEqual(sent, [d(HID.end), u(HID.end)])
    }

    func testHoldKeyThenTapKey() {
        slide.press(1, hold("left", HID.left)); slide.move(1, .on(tap("del", HID.forwardDelete))); slide.lift(1)
        XCTAssertEqual(sent, [d(HID.left), u(HID.left), d(HID.forwardDelete), u(HID.forwardDelete)])
    }

    func testTapKeyFiresOnceWhenTheLastFingerLiftsOnIt() {
        slide.press(1, tap("home", HID.home)); slide.press(2, tap("home", HID.home))
        slide.lift(1)
        XCTAssertEqual(sent, [])
        slide.lift(2)
        XCTAssertEqual(sent, [d(HID.home), u(HID.home)])
        sent = []
        slide.press(3, tap("home", HID.home)); slide.move(3, .none); slide.lift(3)
        XCTAssertEqual(sent, [], "the last finger slid away: nothing")
    }

    func testCtrlHeldStaysThroughAClickWhileAKeyIsDown() {
        sent += board.tap(.ctrl, at: 0)
        slide.press(1, hold("left", HID.left))
        slide.press(2, tap("home", HID.home)); slide.lift(2)
        XCTAssertEqual(board.state(.ctrl), .held)
        slide.lift(1)
        XCTAssertEqual(sent, [d(HID.leftControl), d(HID.left), d(HID.home), u(HID.home), u(HID.left), u(HID.leftControl)])
    }

    func testLayoutChangeUnderAFingerLeavesItsKey() {
        slide.press(1, ch("a"))
        slide.relayout()
        XCTAssertEqual(sent, [d("a"), u("a")])
        slide.move(1, .on(ch("1"))); slide.lift(1)
        XCTAssertEqual(sent, [d("a"), u("a"), d("1"), u("1")])
    }

    func testBubbleFollowsTheNewestFingerOnly() {
        slide.press(1, ch("a"))
        XCTAssertEqual(slide.bubble?.key.id, "vk-a")
        slide.press(2, ch("d"))
        XCTAssertEqual(slide.bubble?.key.id, "vk-d")
        slide.move(1, .on(ch("s")))
        XCTAssertEqual(slide.bubble?.key.id, "vk-d", "another finger's slide does not move it")
        slide.move(2, .on(ch("f")))
        XCTAssertEqual(slide.bubble?.glide, true)
        slide.lift(2)
        XCTAssertNil(slide.bubble, "no jump back to the older finger")
        slide.move(1, .on(ch("q")))
        XCTAssertEqual(slide.bubble?.key.id, "vk-q")
        XCTAssertEqual(slide.bubble?.glide, false, "it appears there again")
        XCTAssertEqual(slide.down, ["vk-q"])
    }

    func testHapticsOnlyWhenAKeyGoesDown() {
        slide.press(1, ch("f")); slide.press(2, ch("f")); slide.move(2, .on(ch("g"))); slide.move(1, .on(ch("g")))
        XCTAssertEqual(feel, [false, true])
    }

    func testReleaseAllOnClose() {
        slide.press(1, hold("left", HID.left)); slide.press(2, ch("!"))
        slide.reset()
        XCTAssertEqual(sent, [d(HID.left), d(HID.leftShift), d("1"), u(HID.left), u("1"), u(HID.leftShift)])
        XCTAssertEqual(pressed.releaseAll(), [])
    }

    /// Key repeat (§3.1 按住重复, user 2026-11-12): only the newest held key repeats, 500 ms then
    /// every 50 ms; an older key does not start again when the newest one goes up.
    func testRepeaterRepeatsOnlyTheNewestKey() {
        var timers: [(delay: TimeInterval, interval: TimeInterval, tick: () -> Void, stopped: Bool)] = []
        let repeater = KeyRepeater { delay, interval, tick in
            timers.append((delay: delay, interval: interval, tick: tick, stopped: false))
            let index = timers.count - 1
            return { timers[index].stopped = true }
        }
        var ticks: [String] = []
        let tick: (String) -> Void = { ticks.append($0) }

        repeater.start("vk-a", tick: tick)
        XCTAssertEqual(timers.count, 1)
        XCTAssertEqual(timers[0].delay, 0.5)
        XCTAssertEqual(timers[0].interval, 0.05)
        timers[0].tick(); timers[0].tick()
        XCTAssertEqual(ticks, ["vk-a", "vk-a"])

        repeater.start("left", tick: tick)
        XCTAssertTrue(timers[0].stopped, "a newer key takes the repeat over")
        XCTAssertEqual(repeater.key, "left")

        repeater.stop("vk-a")
        XCTAssertFalse(timers[1].stopped, "an older key going up leaves the newest repeating")
        repeater.stop("left")
        XCTAssertTrue(timers[1].stopped, "up: its repeat stops at once")
        XCTAssertNil(repeater.key)
        XCTAssertEqual(timers.count, 2, "nothing starts again on its own")

        repeater.start("space", tick: tick)
        repeater.stopAll()
        XCTAssertTrue(timers[2].stopped)
        XCTAssertNil(repeater.key)
    }

    func testHitTestGapsStickinessAndOutside() {
        let a = ch("a"), s = ch("s")
        // Keys 30 wide with a 6 gap: a 0–30, s 36–66; Shift (no bubble) below.
        let keys: [(CGRect, SlideKey?)] = [(CGRect(x: 0, y: 0, width: 30, height: 40), a), (CGRect(x: 36, y: 0, width: 30, height: 40), s),
                                           (CGRect(x: 0, y: 50, width: 40, height: 40), nil)]
        let area = [CGRect(x: 0, y: 0, width: 100, height: 100)]
        func at(_ x: CGFloat, _ y: CGFloat, _ cur: String? = nil) -> String {
            switch KeySlide.locate(CGPoint(x: x, y: y), current: cur, keys: keys, areas: area, near: 8, stick: 5) {
            case .on(let k): return k.id
            case .none: return "none"
            case .out: return "out"
            }
        }
        XCTAssertEqual(at(31, 20), "vk-a", "in the gap, nearer a")
        XCTAssertEqual(at(35, 20), "vk-s")
        XCTAssertEqual(at(34, 20, "vk-a"), "vk-a", "a keeps the finger until 5 past its edge")
        XCTAssertEqual(at(36, 20, "vk-a"), "vk-s")
        XCTAssertEqual(at(10, 60), "none", "Shift")
        XCTAssertEqual(at(90, 20), "none", "far from every key")
        XCTAssertEqual(at(50, -2), "out")
    }
}
