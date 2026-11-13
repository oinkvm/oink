import CoreGraphics
import XCTest
@testable import OinkAudioTestHost

/// Control page key and touchpad rules (docs control-page.md §3.1, §4.1, §4.2).
final class ControlKeyTests: XCTestCase {
    private func down(_ c: UInt32) -> KeyEvent { KeyEvent(code: c, down: true) }
    private func up(_ c: UInt32) -> KeyEvent { KeyEvent(code: c, down: false) }

    func testModifierTapHoldsAndTapReleases() {
        var board = ModifierBoard()
        XCTAssertEqual(board.tap(.ctrl, at: 0), [down(HID.leftControl)])
        XCTAssertEqual(board.state(.ctrl), .held)
        XCTAssertEqual(board.tap(.ctrl, at: 2), [up(HID.leftControl)], "a slow second tap cancels")
        XCTAssertNil(board.state(.ctrl))
    }

    /// User 2026-11-12: a double tap (second tap within 300 ms) is one press + release of the
    /// modifier, ending released — not a lock.
    func testModifierDoubleTapIsPressAndRelease() {
        var board = ModifierBoard()
        let events = board.tap(.shift, at: 0) + board.tap(.shift, at: 0.2)
        XCTAssertEqual(events, [down(HID.leftShift), up(HID.leftShift)])
        XCTAssertFalse(board.anyDown)
        XCTAssertEqual(board.tap(.shift, at: 0.3), [down(HID.leftShift)], "a third tap starts afresh: held")
        XCTAssertEqual(board.state(.shift), .held)
    }

    /// A quick tap after a tap released the modifier is the double tap's second half: down + up.
    func testModifierQuickTapAfterReleaseIsPressAndRelease() {
        var board = ModifierBoard()
        _ = board.longPress(.ctrl)
        XCTAssertEqual(board.tap(.ctrl, at: 1), [up(HID.leftControl)])
        XCTAssertEqual(board.tap(.ctrl, at: 1.2), [down(HID.leftControl), up(HID.leftControl)])
        XCTAssertNil(board.state(.ctrl))
    }

    /// User 2026-11-12: a long press locks; a long press on a locked modifier unlocks it, and a
    /// tap on it releases it too.
    func testModifierLongPressLocksAndUnlocks() {
        var board = ModifierBoard()
        XCTAssertEqual(board.longPress(.ctrl), [down(HID.leftControl)], "released → locked goes down")
        XCTAssertEqual(board.state(.ctrl), .locked)
        XCTAssertEqual(board.longPress(.ctrl), [up(HID.leftControl)], "locked → released")
        XCTAssertNil(board.state(.ctrl))
        _ = board.tap(.alt, at: 0)
        XCTAssertEqual(board.longPress(.alt), [], "held → locked: already down on the remote")
        XCTAssertEqual(board.state(.alt), .locked)
        XCTAssertEqual(board.tap(.alt, at: 5), [up(HID.leftAlt)])
        XCTAssertNil(board.state(.alt))
        XCTAssertEqual(ModifierBoard.longPress, 0.45)
    }

    func testKeyReleasesHeldButKeepsLockedModifiers() {
        var board = ModifierBoard()
        _ = board.tap(.ctrl, at: 0)
        _ = board.longPress(.shift)
        _ = board.tap(.alt, at: 3)
        XCTAssertEqual(board.releaseHeld(), [up(HID.leftControl), up(HID.leftAlt)])
        XCTAssertEqual(board.state(.shift), .locked)
        XCTAssertEqual(board.releaseAll(), [up(HID.leftShift)])
    }

    func testFnLayerTapDoubleTapAndFKey() {
        var fn = FnLayer()
        fn.tap(at: 0)
        XCTAssertEqual(fn.state, .held)
        fn.usedFKey()
        XCTAssertNil(fn.state, "a held layer goes back after one F key")
        fn.tap(at: 5); fn.tap(at: 5.2)
        XCTAssertEqual(fn.state, .locked)
        fn.usedFKey()
        XCTAssertEqual(fn.state, .locked)
        fn.tap(at: 9)
        XCTAssertNil(fn.state)
    }

    func testCopyPasteCarryTheirModifierUnlessItIsDown() {
        let empty = ModifierBoard()
        XCTAssertEqual(ComboKey.copy.events(target: .windows, board: empty),
                       [down(HID.leftControl), down(HID.c), up(HID.c), up(HID.leftControl)])
        XCTAssertEqual(ComboKey.paste.events(target: .mac, board: empty),
                       [down(HID.leftGUI), down(HID.v), up(HID.v), up(HID.leftGUI)])
        var ctrl = ModifierBoard()
        _ = ctrl.tap(.ctrl, at: 0)
        XCTAssertEqual(ComboKey.paste.events(target: .windows, board: ctrl), [down(HID.v), up(HID.v)])
    }

    /// 调节 › 键盘布局: the keys come from the shared Rust tables (oink-mobile-core key_layouts,
    /// tested there per layout); these check the wiring: layout, system and modifiers reach it.
    private func typed(_ c: Character, _ layout: String = "us", _ target: KeyTarget = .windows,
                       _ board: ModifierBoard = ModifierBoard()) -> TypedChar? {
        KeyLayouts.typed(c, layout: layout, target: target, board: board)
    }

    func testUSLayoutPhysicalKeysAndShift() {
        XCTAssertEqual(typed("a"), TypedChar.key(TypedKey(code: HID.key(0x04))))
        XCTAssertEqual(typed("z", "auto"), TypedChar.key(TypedKey(code: HID.key(0x1D))))
        XCTAssertEqual(typed("1"), TypedChar.key(TypedKey(code: HID.key(0x1E))))
        XCTAssertEqual(typed("!"), TypedChar.key(TypedKey(code: HID.key(0x1E), autoShift: true)))
        XCTAssertEqual(typed("_"), TypedChar.key(TypedKey(code: HID.key(0x2D), autoShift: true)))
        XCTAssertEqual(typed("\""), TypedChar.key(TypedKey(code: HID.key(0x34), autoShift: true)))
        XCTAssertEqual(typed("~", "ko"), TypedChar.key(TypedKey(code: HID.key(0x35), autoShift: true)))
        for row in [EmulatedRows.numbers, EmulatedRows.besideSpace] + EmulatedRows.symbols + EmulatedRows.letters {
            for c in row {
                guard case .key? = typed(c) else { return XCTFail("\(c) has no US key") }
            }
        }
        XCTAssertEqual(USLayout.symbolFace("1", shift: true), "!")
        XCTAssertEqual(USLayout.symbolFace("@", shift: true), "@")
        XCTAssertEqual(USLayout.symbolFace("/", shift: false), "/")
    }

    func testLayoutAndSystemReachTheSharedTables() {
        XCTAssertEqual(typed("z", "de"), TypedChar.key(TypedKey(code: HID.key(0x1C))), "QWERTZ")
        // @ = AltGr (right Alt) + Q on Windows, Option (left Alt) + L on a Mac.
        XCTAssertEqual(typed("@", "de"), TypedChar.key(TypedKey(code: HID.key(0x14), autoAlt: HID.key(0xE6))))
        XCTAssertEqual(typed("@", "de", .mac), TypedChar.key(TypedKey(code: HID.key(0x0F), autoAlt: HID.leftAlt)))
        XCTAssertEqual(typed("^", "de"), TypedChar.key(TypedKey(code: HID.key(0x35), dead: true)))
        XCTAssertEqual(typed("[", "fr", .mac), TypedChar.text("["), "no certain key: text")
        var ctrl = ModifierBoard()
        _ = ctrl.tap(.ctrl, at: 0)
        XCTAssertEqual(typed("[", "fr", .mac, ctrl), TypedChar.key(TypedKey(code: HID.key(0x2F))), "a shortcut stays a key")
    }

    func testAutoShiftWrapsOnlyWhenUserShiftIsUp() {
        guard case .key(let bang)? = typed("!") else { return XCTFail() }
        XCTAssertEqual(bang.downEvents, [down(HID.leftShift), down(bang.code)])
        XCTAssertEqual(bang.upEvents, [up(bang.code), up(HID.leftShift)])
        var shift = ModifierBoard()
        _ = shift.tap(.shift, at: 0)
        // The symbol layer's 1 shows ! while Shift is down: the user's Shift + 1, no second Shift.
        guard case .key(let shifted)? = typed("1", "us", .windows, shift) else { return XCTFail() }
        XCTAssertEqual(shifted.downEvents, [down(bang.code)], "never touches the user's Shift")
        XCTAssertEqual(shifted.upEvents, [up(bang.code)])
    }

    func testAltGrGoesInsideTheShiftAndDeadKeysAreTappedWithSpace() {
        let key = TypedKey(code: HID.key(0x24), autoShift: true, autoAlt: HID.leftAlt)
        XCTAssertEqual(key.downEvents, [down(HID.leftShift), down(HID.leftAlt), down(key.code)])
        XCTAssertEqual(key.upEvents, [up(key.code), up(HID.leftAlt), up(HID.leftShift)])
        let dead = TypedKey(code: HID.key(0x2F), autoShift: true, dead: true)
        XCTAssertEqual(dead.deadEvents, [down(HID.leftShift), down(dead.code), up(dead.code), up(HID.leftShift),
                                         down(HID.space), up(HID.space)])
    }

    func testModifierBringsEmulatedKeyboardOnlyTemporarily() {
        let system = KeyboardChoice(preferred: .system)
        XCTAssertEqual(system.shown(modifiersDown: true), .emulated)
        XCTAssertTrue(system.temporary(modifiersDown: true))
        XCTAssertEqual(system.shown(modifiersDown: false), .system)
        let emulated = KeyboardChoice(preferred: .emulated)
        XCTAssertEqual(emulated.shown(modifiersDown: false), .emulated)
        XCTAssertFalse(emulated.temporary(modifiersDown: true))
    }

    func testCommittedTextPieces() {
        XCTAssertEqual(TextPiece.pieces("ab\ncd\te\u{7}"),
                       [.text("ab"), .key(HID.returnKey), .text("cd"), .key(HID.tab), .text("e")])
        XCTAssertEqual(TextPiece.pieces("x\r\ny"), [.text("x"), .key(HID.returnKey), .text("y")])
        let long = String(repeating: "中", count: 1400) + "👍🏽"   // 4200 + 8 bytes
        let pieces = TextPiece.pieces(long)
        XCTAssertEqual(pieces.count, 2)
        for case .text(let t) in pieces { XCTAssertLessThanOrEqual(t.utf8.count, 4096) }
        XCTAssertEqual(pieces.compactMap { if case .text(let t) = $0 { t } else { nil } }.joined(), long)
    }
}

final class TrackpadRecognizerTests: XCTestCase {
    private let size = CGSize(width: 360, height: 500)
    private func recognizer(scroll: Bool = false) -> TrackpadRecognizer {
        var r = TrackpadRecognizer(slop: 9)
        r.padSize = size
        r.oneFingerScroll = scroll
        return r
    }
    private func buttons(_ out: [TrackpadOutput]) -> [String] {
        out.compactMap {
            if case .button(let b, let down) = $0 { "\(b == .left ? "L" : "R")\(down ? "↓" : "↑")" } else { nil }
        }
    }
    private func ripples(_ out: [TrackpadOutput]) -> [ClickKind] {
        out.compactMap { if case .ripple(let k, _) = $0 { k } else { nil } }
    }
    private func moves(_ out: [TrackpadOutput]) -> [CGVector] {
        out.compactMap { if case .move(let d, _, _) = $0 { d } else { nil } }
    }
    /// (at, since) of each pointer motion.
    private func moveTimes(_ out: [TrackpadOutput]) -> [[TimeInterval]] {
        out.compactMap { if case .move(_, let at, let since) = $0 { [at, since] } else { nil } }
    }
    private func scrolls(_ out: [TrackpadOutput]) -> [CGVector] {
        out.compactMap { if case .scroll(let d, _, _, _) = $0 { d } else { nil } }
    }
    private func scrollPhases(_ out: [TrackpadOutput]) -> [PadScrollPhase] {
        out.compactMap { if case .scroll(_, let phase, _, _) = $0 { phase } else { nil } }
    }
    private func gestures(_ out: [TrackpadOutput]) -> [PadGesture] {
        out.compactMap { if case .gesture(let g) = $0 { g } else { nil } }
    }
    private func edges(_ out: [TrackpadOutput]) -> [EdgeLock?] {
        out.compactMap { if case .edge(let e) = $0 { e } else { nil } }
    }
    private func tap(_ r: inout TrackpadRecognizer, _ id: Int, at t: TimeInterval, _ p: CGPoint = CGPoint(x: 100, y: 100)) -> [TrackpadOutput] {
        r.began(id, at: p, time: t) + r.ended(id, at: p, time: t + 0.08)
    }

    func testEdgeGeometry() {
        XCTAssertNil(EdgeLock.at(CGPoint(x: 180, y: 250), in: size))
        let half = EdgeLock.at(CGPoint(x: 320, y: 250), in: size)!
        XCTAssertEqual(half.kind, .sideV)
        XCTAssertEqual(half.progress, 0.5, accuracy: 0.001)
        XCTAssertEqual(half.center, CGPoint(x: 360, y: 250))
        XCTAssertEqual(half.badge.x, 360 - 40 - 46, accuracy: 0.001)
        XCTAssertTrue(EdgeLock.at(CGPoint(x: 350, y: 250), in: size)!.isFull)
        let corner = EdgeLock.at(CGPoint(x: 326, y: 470), in: size)!
        XCTAssertEqual(corner.kind, .corner)
        XCTAssertEqual(corner.center, CGPoint(x: 360, y: 500))
        XCTAssertEqual(EdgeLock.at(CGPoint(x: 180, y: 10), in: size)?.kind, .sideH)
    }

    func testTapPressesAtOnceAndReleasesAfterTheDragWindow() {
        var r = recognizer()
        let out = tap(&r, 1, at: 0)
        XCTAssertEqual(buttons(out), ["L↓"], "the release is held back for tap-to-drag")
        XCTAssertEqual(ripples(out), [.single])
        XCTAssertTrue(out.contains(.startTapWindow))
        XCTAssertEqual(buttons(r.tapWindowEnded(at: 0.2)), [], "window not over yet")
        XCTAssertEqual(buttons(r.tapWindowEnded(at: 0.381)), ["L↑"])
        XCTAssertFalse(r.leftDown)
    }

    func testSecondTapInTheWindowIsADoubleClick() {
        var r = recognizer()
        _ = tap(&r, 1, at: 0)
        let second = tap(&r, 2, at: 0.25)
        XCTAssertEqual(buttons(second), ["L↑", "L↓", "L↑"])
        XCTAssertEqual(ripples(second), [.double])
        XCTAssertEqual(buttons(r.tapWindowEnded(at: 0.4)), [], "nothing left pending")
        XCTAssertFalse(r.leftDown)
    }

    func testTapThenPressAndMoveDrags() {
        var r = recognizer()
        _ = tap(&r, 1, at: 0)
        _ = r.began(2, at: CGPoint(x: 100, y: 100), time: 0.2)
        XCTAssertEqual(buttons(r.tapWindowEnded(at: 0.4)), [], "a finger on the pad decides first")
        let drag = r.moved(2, to: CGPoint(x: 130, y: 100), time: 0.45)
        XCTAssertEqual(buttons(drag), [], "the tap's button stays down")
        XCTAssertEqual(moves(drag), [CGVector(dx: 30, dy: 0)])
        XCTAssertTrue(r.leftDown)
        XCTAssertEqual(buttons(r.ended(2, at: CGPoint(x: 130, y: 100), time: 0.6)), ["L↑"])
        XCTAssertFalse(r.leftDown)
    }

    func testPressAfterTheWindowIsAPlainMoveWithoutEdgeLock() {
        var r = recognizer()
        _ = tap(&r, 1, at: 0)
        let late = r.began(2, at: CGPoint(x: 200, y: 250), time: 0.5)
        XCTAssertEqual(buttons(late), ["L↑"], "the first click completes")
        let near = r.moved(2, to: CGPoint(x: 352, y: 250), time: 0.6)
        XCTAssertFalse(moves(near).isEmpty)
        XCTAssertTrue(edges(near).isEmpty, "edge lock only while dragging")
        XCTAssertTrue(buttons(r.ended(2, at: CGPoint(x: 352, y: 250), time: 0.7)).isEmpty)
    }

    /// User 2026-11-12: a two-finger tap waits 0.3 s for a second one (smart zoom), then right-clicks.
    func testTwoFingerTapIsRightClickAfterTheDoubleTapWindow() {
        var r = recognizer()
        _ = r.began(1, at: CGPoint(x: 100, y: 100), time: 0)
        _ = r.began(2, at: CGPoint(x: 140, y: 100), time: 0.04)
        XCTAssertTrue(buttons(r.ended(1, at: CGPoint(x: 100, y: 100), time: 0.15)).isEmpty)
        let out = r.ended(2, at: CGPoint(x: 140, y: 100), time: 0.2)
        XCTAssertEqual(buttons(out), [], "held back")
        XCTAssertTrue(out.contains(.startRightTapWindow))
        XCTAssertTrue(out.contains(.tapFeedback), "feel the tap now, not when right-click resolves")
        XCTAssertEqual(buttons(r.rightTapWindowEnded(at: 0.4)), [], "window not over yet")
        let late = r.rightTapWindowEnded(at: 0.5)
        XCTAssertEqual(buttons(late), ["R↓", "R↑"])
        XCTAssertEqual(ripples(late), [.right])
        XCTAssertFalse(late.contains(.tapFeedback), "no second haptic for the delayed click")
        XCTAssertEqual(buttons(r.rightTapWindowEnded(at: 0.6)), [], "only once")
    }

    func testTheNextTouchLetsAHeldRightClickGoFirst() {
        // Moving the pointer: the right click lands where the pointer was.
        var r = recognizer()
        _ = r.began(1, at: CGPoint(x: 100, y: 100), time: 0) + r.began(2, at: CGPoint(x: 150, y: 100), time: 0)
            + r.ended(1, at: .zero, time: 0.1) + r.ended(2, at: .zero, time: 0.1)
        _ = r.began(3, at: CGPoint(x: 100, y: 300), time: 0.2)
        let move = r.moved(3, to: CGPoint(x: 130, y: 300), time: 0.25)
        XCTAssertEqual(buttons(move), ["R↓", "R↑"])
        XCTAssertEqual(move.firstIndex { if case .move = $0 { true } else { false } }, 3, "after the click and its ripple")
        XCTAssertEqual(buttons(r.rightTapWindowEnded(at: 0.4)), [])

        // A one-finger tap: right click, then the left click.
        var s = recognizer()
        _ = s.began(1, at: CGPoint(x: 100, y: 100), time: 0) + s.began(2, at: CGPoint(x: 150, y: 100), time: 0)
            + s.ended(1, at: .zero, time: 0.1) + s.ended(2, at: .zero, time: 0.1)
        XCTAssertEqual(buttons(tap(&s, 3, at: 0.2)), ["R↓", "R↑", "L↓"])

        // Leaving the page: the finished two-finger tap still right-clicks.
        var u = recognizer()
        _ = u.began(1, at: CGPoint(x: 100, y: 100), time: 0) + u.began(2, at: CGPoint(x: 150, y: 100), time: 0)
            + u.ended(1, at: .zero, time: 0.1) + u.ended(2, at: .zero, time: 0.1)
        XCTAssertEqual(buttons(u.reset(at: 0.2)), ["R↓", "R↑"])
    }

    func testOneFingerMovesFromItsFirstMovementWithoutLosingDistance() {
        var r = recognizer()
        _ = r.began(1, at: CGPoint(x: 100, y: 100), time: 0)
        XCTAssertEqual(moves(r.moved(1, to: CGPoint(x: 105, y: 100), time: 0.01)), [CGVector(dx: 5, dy: 0)],
                       "inside the slop the pointer already follows")
        XCTAssertEqual(moves(r.moved(1, to: CGPoint(x: 112, y: 100), time: 0.02)), [CGVector(dx: 7, dy: 0)])
        XCTAssertEqual(moves(r.moved(1, to: CGPoint(x: 115, y: 104), time: 0.03)), [CGVector(dx: 3, dy: 4)])
        XCTAssertTrue(buttons(r.ended(1, at: CGPoint(x: 115, y: 104), time: 0.05)).isEmpty, "a move is not a tap")
    }

    /// Like a Mac trackpad: a tap's tiny movement moves the pointer, and it is still a tap.
    func testTapWithinTheSlopStillClicksAndSendsItsTinyMovement() {
        var r = recognizer()
        _ = r.began(1, at: CGPoint(x: 100, y: 100), time: 0)
        XCTAssertTrue(moves(r.moved(1, to: CGPoint(x: 100.5, y: 100), time: 0.02)).isEmpty, "noise (slop / 10) is held back")
        let tiny = r.moved(1, to: CGPoint(x: 103, y: 100), time: 0.04)
        XCTAssertEqual(moves(tiny), [CGVector(dx: 3, dy: 0)])
        XCTAssertEqual(moveTimes(tiny), [[0.04, 0]], "timed from the touch-down")
        let up = r.ended(1, at: CGPoint(x: 103, y: 100), time: 0.1)
        XCTAssertEqual(buttons(up), ["L↓"], "within the slop and quick: a tap")
        XCTAssertEqual(ripples(up), [.single])

        // Within the slop but too long: no tap, the movement still went out.
        var s = recognizer()
        _ = s.began(1, at: CGPoint(x: 100, y: 100), time: 0)
        XCTAssertEqual(moves(s.moved(1, to: CGPoint(x: 104, y: 100), time: 0.1)), [CGVector(dx: 4, dy: 0)])
        XCTAssertTrue(buttons(s.ended(1, at: CGPoint(x: 104, y: 100), time: 0.3)).isEmpty)
    }

    /// The slop still decides drag vs second click after a tap, and two-finger gestures.
    func testFingerAfterATapAndTwoFingerTouchesWaitForTheSlop() {
        var r = recognizer()
        _ = tap(&r, 1, at: 0)
        _ = r.began(2, at: CGPoint(x: 100, y: 100), time: 0.2)
        XCTAssertTrue(moves(r.moved(2, to: CGPoint(x: 105, y: 100), time: 0.22)).isEmpty, "drag or second click: not decided yet")
        XCTAssertEqual(buttons(r.ended(2, at: CGPoint(x: 105, y: 100), time: 0.26)), ["L↑", "L↓", "L↑"])

        var t = recognizer()
        _ = t.began(1, at: CGPoint(x: 100, y: 100), time: 0)
        _ = t.began(2, at: CGPoint(x: 140, y: 100), time: 0.02)
        XCTAssertTrue(moves(t.moved(1, to: CGPoint(x: 104, y: 100), time: 0.04)).isEmpty, "two fingers: scroll or right click first")
        _ = t.ended(2, at: CGPoint(x: 140, y: 100), time: 0.1)
        XCTAssertTrue(moves(t.moved(1, to: CGPoint(x: 106, y: 100), time: 0.12)).isEmpty, "a touch that had two fingers waits for the slop")
    }

    /// Two or more fingers are the core's (its recognizer is shared with Android): nothing goes
    /// out from here for them; once the core claims the touch, the press follows a two-finger
    /// scroll, the touch is no tap and its remaining finger moves no pointer.
    func testTwoFingersAreTheCoresAndAClaimedTouchStaysQuiet() {
        var r = recognizer()
        _ = r.began(1, at: CGPoint(x: 100, y: 300), time: 0)
        _ = r.began(2, at: CGPoint(x: 160, y: 300), time: 0.01)
        XCTAssertTrue(r.coreMayRecognize(fingers: 2))
        let before = r.moved([1: CGPoint(x: 100, y: 280), 2: CGPoint(x: 160, y: 280)], time: 0.02)
        XCTAssertTrue(moves(before).isEmpty && scrolls(before).isEmpty, "the core decides")
        let claimed = r.moved([1: CGPoint(x: 100, y: 260), 2: CGPoint(x: 160, y: 260)], time: 0.03, core: .scroll)
        XCTAssertTrue(moves(claimed).isEmpty && scrolls(claimed).isEmpty)
        XCTAssertTrue(claimed.contains(.press(CGPoint(x: 130, y: 260))), "the press follows the scrolling fingers")
        XCTAssertTrue(r.coreMayRecognize(fingers: 2), "the core keeps the touch it took")
        // The core's scroll ended below two fingers; the remaining finger moves nothing.
        _ = r.ended(1, at: CGPoint(x: 100, y: 260), time: 0.04, core: .gesture)
        XCTAssertTrue(moves(r.moved(2, to: CGPoint(x: 220, y: 200), time: 0.06, core: .gesture)).isEmpty)
        let lift = r.ended(2, at: CGPoint(x: 220, y: 200), time: 0.08)
        XCTAssertTrue(buttons(lift).isEmpty, "a claimed touch is no tap")
        XCTAssertFalse(lift.contains(.startRightTapWindow))
    }

    /// The core may take a touch's fingers only while this recognizer does not use them.
    func testTheCoreMayTakeOnlyFingersThisRecognizerDoesNotUse() {
        var r = recognizer()
        XCTAssertTrue(r.coreMayRecognize(fingers: 1), "a new touch")
        // Dragging: the drag's fingers stay here.
        _ = tap(&r, 1, at: 0)
        _ = r.began(2, at: CGPoint(x: 100, y: 100), time: 0.2)
        _ = r.moved(2, to: CGPoint(x: 130, y: 100), time: 0.25)
        XCTAssertTrue(r.leftDown)
        XCTAssertFalse(r.coreMayRecognize(fingers: 2))
        _ = r.ended(2, at: CGPoint(x: 130, y: 100), time: 0.3)
        // 单指划动 = 滚动: two fingers move the pointer here, three are the core's; the one-finger
        // scroll stays here.
        var s = recognizer(scroll: true)
        _ = s.began(1, at: CGPoint(x: 100, y: 300), time: 1)
        XCTAssertFalse(s.coreMayRecognize(fingers: 2))
        XCTAssertTrue(s.coreMayRecognize(fingers: 3))
        _ = s.moved(1, to: CGPoint(x: 100, y: 280), time: 1.02)
        XCTAssertFalse(s.coreMayRecognize(fingers: 3), "scrolling with one finger")
    }

    /// A held-back right click goes before what the core starts in the next touch.
    func testAClaimLetsAHeldRightClickGoFirst() {
        var r = recognizer()
        _ = r.began(1, at: CGPoint(x: 100, y: 100), time: 0) + r.began(2, at: CGPoint(x: 150, y: 100), time: 0)
            + r.ended(1, at: .zero, time: 0.1) + r.ended(2, at: .zero, time: 0.1)
        _ = r.began(3, at: CGPoint(x: 100, y: 300), time: 0.2) + r.began(4, at: CGPoint(x: 150, y: 300), time: 0.2)
        let claim = r.moved([3: CGPoint(x: 100, y: 280), 4: CGPoint(x: 150, y: 280)], time: 0.25, core: .scroll)
        XCTAssertEqual(buttons(claim), ["R↓", "R↑"])
        XCTAssertEqual(buttons(r.rightTapWindowEnded(at: 0.4)), [])
    }

    func testOneFingerScrollSettingSwapsFingersAndHasNoDrag() {
        var r = recognizer(scroll: true)
        _ = r.began(1, at: CGPoint(x: 100, y: 300), time: 0)
        XCTAssertEqual(scrolls(r.moved(1, to: CGPoint(x: 100, y: 280), time: 0.02)), [CGVector(dx: 0, dy: -20)])
        _ = r.ended(1, at: CGPoint(x: 100, y: 280), time: 0.03)
        _ = r.began(2, at: CGPoint(x: 100, y: 300), time: 1)
        _ = r.began(3, at: CGPoint(x: 160, y: 300), time: 1.01)
        _ = r.moved(2, to: CGPoint(x: 120, y: 300), time: 1.02)
        XCTAssertEqual(moves(r.moved(3, to: CGPoint(x: 180, y: 300), time: 1.03)), [CGVector(dx: 10, dy: 0)])
        _ = r.ended(2, at: .zero, time: 1.1); _ = r.ended(3, at: .zero, time: 1.1)
        let click = tap(&r, 4, at: 2)
        XCTAssertEqual(buttons(click), ["L↓", "L↑"], "no tap-to-drag here: the click is not held back")
        XCTAssertFalse(click.contains(.startTapWindow))
        XCTAssertFalse(r.leftDown)
    }

    /// The core takes the finger speed from touch times: one motion per touch event (two fingers
    /// moving in one event are one motion, not two a few microseconds apart), the first timed
    /// from the touch-down.
    func testPointerMotionCarriesTouchTimesOncePerEvent() {
        var r = recognizer()
        _ = r.began(1, at: CGPoint(x: 100, y: 100), time: 5)
        let first = r.moved([1: CGPoint(x: 130, y: 100)], time: 5.2)
        XCTAssertEqual(moveTimes(first), [[5.2, 5]], "the first motion is timed from the touch-down")
        XCTAssertEqual(moveTimes(r.moved([1: CGPoint(x: 140, y: 100)], time: 5.208)), [[5.208, 5.2]])
        _ = r.ended(1, at: CGPoint(x: 140, y: 100), time: 5.3)

        var s = recognizer(scroll: true)
        _ = s.began(1, at: CGPoint(x: 100, y: 300), time: 1)
        _ = s.began(2, at: CGPoint(x: 160, y: 300), time: 1.01)
        let both = s.moved([1: CGPoint(x: 120, y: 300), 2: CGPoint(x: 180, y: 300)], time: 1.05)
        XCTAssertEqual(moves(both), [CGVector(dx: 20, dy: 0)], "two fingers in one event: one motion")
        XCTAssertEqual(moveTimes(both), [[1.05, 1.01]], "timed from the second finger's landing")
    }

    /// The pad moves under resting fingers (keyboard, rotation, the tab bar): the view rebases the
    /// recognizer, so only real finger motion goes out.
    func testALayoutShiftDuringAOneFingerMoveMakesNoJump() {
        var r = recognizer()
        _ = r.began(1, at: CGPoint(x: 100, y: 100), time: 0)
        XCTAssertEqual(moves(r.moved(1, to: CGPoint(x: 130, y: 100), time: 0.02)).map(\.dx).reduce(0, +), 30)
        // The pad moved 200 pt right: the same finger is now at x = -70 in it.
        r.shift(by: CGVector(dx: -200, dy: 0))
        XCTAssertEqual(moves(r.moved(1, to: CGPoint(x: -70, y: 100), time: 0.03)), [.zero], "no motion for the layout change")
        XCTAssertEqual(moves(r.moved(1, to: CGPoint(x: -65, y: 100), time: 0.04)), [CGVector(dx: 5, dy: 0)])
    }

    func testALayoutShiftDuringTheSwappedScrollMakesNoJump() {
        var r = recognizer(scroll: true)
        _ = r.began(1, at: CGPoint(x: 100, y: 300), time: 0)
        XCTAssertEqual(scrolls(r.moved([1: CGPoint(x: 100, y: 280)], time: 0.02)), [CGVector(dx: 0, dy: -20)])
        // The keyboard rose and the pad moved 150 pt up: the finger is 150 pt lower in it.
        r.shift(by: CGVector(dx: 0, dy: 150))
        XCTAssertEqual(scrolls(r.moved([1: CGPoint(x: 100, y: 430)], time: 0.03)), [.zero])
        XCTAssertEqual(scrolls(r.moved([1: CGPoint(x: 100, y: 420)], time: 0.04)), [CGVector(dx: 0, dy: -10)])
    }

    /// Two two-finger taps within 0.3 s are smart zoom (this recognizer's own gesture).
    func testTwoFingerDoubleTapIsSmartZoom() {
        var u = recognizer()
        let first = u.began(1, at: CGPoint(x: 100, y: 100), time: 0) + u.began(2, at: CGPoint(x: 150, y: 100), time: 0)
            + u.ended(1, at: .zero, time: 0.1) + u.ended(2, at: .zero, time: 0.1)
        XCTAssertEqual(buttons(first), [])
        let second = u.began(1, at: CGPoint(x: 100, y: 100), time: 0.3) + u.began(2, at: CGPoint(x: 150, y: 100), time: 0.3)
        // The window ends while the second tap is down: its lift decides.
        XCTAssertEqual(buttons(u.rightTapWindowEnded(at: 0.4)), [])
        let lift2 = u.ended(1, at: .zero, time: 0.45) + u.ended(2, at: .zero, time: 0.45)
        XCTAssertEqual(buttons(second + lift2), [])
        XCTAssertEqual(gestures(second + lift2), [.smartMagnify])
        XCTAssertEqual(buttons(u.rightTapWindowEnded(at: 0.8)), [], "nothing left")

        // The second tap too late: the first right-clicks, the second is held back again.
        var v = recognizer()
        _ = v.began(1, at: CGPoint(x: 100, y: 100), time: 0) + v.began(2, at: CGPoint(x: 150, y: 100), time: 0)
            + v.ended(1, at: .zero, time: 0.1) + v.ended(2, at: .zero, time: 0.1)
        XCTAssertEqual(buttons(v.rightTapWindowEnded(at: 0.4)), ["R↓", "R↑"])
        let again = v.began(1, at: CGPoint(x: 100, y: 100), time: 0.5) + v.began(2, at: CGPoint(x: 150, y: 100), time: 0.5)
            + v.ended(1, at: .zero, time: 0.6) + v.ended(2, at: .zero, time: 0.6)
        XCTAssertTrue(gestures(again).isEmpty)
        XCTAssertTrue(again.contains(.startRightTapWindow))
    }

    /// The system taking a touch the core took away: the core cancels what it runs (it gets the
    /// cancelled frame); nothing to end here.
    func testACancelledClaimedTouchLeavesTheEndToTheCore() {
        var r = recognizer()
        for (id, x) in [(1, 100.0), (2, 150.0), (3, 200.0)] { _ = r.began(id, at: CGPoint(x: x, y: 300), time: 0) }
        _ = r.moved([1: CGPoint(x: 50, y: 300), 2: CGPoint(x: 100, y: 300), 3: CGPoint(x: 150, y: 300)], time: 0.05,
                    core: .gesture)
        XCTAssertEqual(r.cancelled(at: 0.06), [.press(nil)])
    }

    /// Tap, press again and drag to the edge until the lock is full.
    private func dragToEdge(_ r: inout TrackpadRecognizer) -> [TrackpadOutput] {
        _ = tap(&r, 1, at: 0, CGPoint(x: 200, y: 250))
        _ = r.began(2, at: CGPoint(x: 200, y: 250), time: 0.2)
        _ = r.moved(2, to: CGPoint(x: 300, y: 250), time: 0.3)
        return r.moved(2, to: CGPoint(x: 350, y: 250), time: 0.35)
    }

    func testDragEdgeLockKeepsTheButtonDownUntilRelease() {
        var r = recognizer()
        XCTAssertTrue(dragToEdge(&r).contains(.edgeFull))
        XCTAssertFalse(r.moved(2, to: CGPoint(x: 352, y: 250), time: 0.37).contains(.edgeFull), "haptic once per reach")
        let lift = r.ended(2, at: CGPoint(x: 352, y: 250), time: 0.4)
        XCTAssertTrue(lift.contains(.locked(CGPoint(x: 352, y: 250))))
        XCTAssertEqual(buttons(lift), [], "locked: the left button stays down")
        XCTAssertTrue(r.leftDown)

        // A tap while locked is not sent; the lock stays.
        XCTAssertTrue(buttons(tap(&r, 3, at: 1)).isEmpty)
        XCTAssertNotNil(r.lockedAt)

        // 释放 lifts the locked finger and releases the button.
        XCTAssertEqual(buttons(r.releaseLock()), ["L↑"])
        XCTAssertFalse(r.leftDown)
    }

    func testRelayFingerTakesTheDragOver() {
        var r = recognizer()
        _ = dragToEdge(&r)
        _ = r.ended(2, at: CGPoint(x: 350, y: 250), time: 0.4)
        _ = r.began(3, at: CGPoint(x: 100, y: 250), time: 2)
        XCTAssertFalse(r.moved(3, to: CGPoint(x: 104, y: 250), time: 2.01).contains(.unlocked))
        let relay = r.moved(3, to: CGPoint(x: 110, y: 250), time: 2.02)
        XCTAssertTrue(relay.contains(.unlocked))
        XCTAssertEqual(buttons(relay), [], "the button stays down across the relay")
        XCTAssertFalse(moves(relay).isEmpty, "the new finger keeps dragging")
        XCTAssertNil(r.lockedAt)
        XCTAssertTrue(r.leftDown)
    }

    /// User 2026-11-12 (like macOS drag lock): once locked, the drag survives any number of
    /// fingers lifting and landing — each lift locks again, wherever it is — until 释放.
    func testLockedDragContinuesAcrossRelaysUntilRelease() {
        var r = recognizer()
        _ = dragToEdge(&r)
        _ = r.ended(2, at: CGPoint(x: 350, y: 250), time: 0.4)
        for (i, finger) in [3, 4, 5].enumerated() {
            let t = 2.0 + Double(i)
            let land = r.began(finger, at: CGPoint(x: 100, y: 250), time: t)
            XCTAssertEqual(buttons(land), [], "putting a finger down keeps the drag")
            let relay = r.moved(finger, to: CGPoint(x: 120, y: 250), time: t + 0.05)
            XCTAssertTrue(relay.contains(.unlocked))
            XCTAssertEqual(buttons(relay), [])
            XCTAssertFalse(moves(relay).isEmpty)
            let lift = r.ended(finger, at: CGPoint(x: 150, y: 250), time: t + 0.2)
            XCTAssertEqual(buttons(lift), [], "lifting in the middle of the pad keeps the button down")
            XCTAssertTrue(lift.contains(.locked(CGPoint(x: 150, y: 250))))
            XCTAssertTrue(r.leftDown)
        }
        XCTAssertEqual(buttons(r.releaseLock()), ["L↑"])
        XCTAssertFalse(r.leftDown)
        XCTAssertFalse(r.dragLocked)

        // After 释放 a drag is a plain one again: it ends where the finger lifts.
        _ = tap(&r, 6, at: 10, CGPoint(x: 200, y: 250))
        _ = r.began(7, at: CGPoint(x: 200, y: 250), time: 10.2)
        _ = r.moved(7, to: CGPoint(x: 230, y: 250), time: 10.3)
        XCTAssertEqual(buttons(r.ended(7, at: CGPoint(x: 230, y: 250), time: 10.4)), ["L↑"])
    }

    func testLockedDragKeepsItsButtonWhenMoreFingersJoinOrTheSystemCancels() {
        var r = recognizer()
        _ = dragToEdge(&r)
        _ = r.ended(2, at: CGPoint(x: 350, y: 250), time: 0.4)
        _ = r.began(3, at: CGPoint(x: 100, y: 250), time: 2)
        _ = r.moved(3, to: CGPoint(x: 120, y: 250), time: 2.05)
        XCTAssertEqual(buttons(r.began(4, at: CGPoint(x: 160, y: 250), time: 2.1)), [], "a second finger does not drop it")
        _ = r.ended(4, at: CGPoint(x: 160, y: 250), time: 2.2)
        let lift = r.ended(3, at: CGPoint(x: 120, y: 250), time: 2.3)
        XCTAssertEqual(buttons(lift), [])
        XCTAssertTrue(lift.contains(.locked(CGPoint(x: 120, y: 250))))

        _ = r.began(5, at: CGPoint(x: 100, y: 250), time: 3)
        _ = r.moved(5, to: CGPoint(x: 130, y: 250), time: 3.05)
        let cancel = r.cancelled(at: 3.1)
        XCTAssertEqual(buttons(cancel), [], "the system taking the touch away keeps the drag")
        XCTAssertTrue(cancel.contains { if case .locked = $0 { true } else { false } })
        XCTAssertEqual(buttons(r.reset(at: 4)), ["L↑"], "leaving the page lets go")
    }

    func testDragLiftWithoutFullProgressReleases() {
        var r = recognizer()
        _ = tap(&r, 1, at: 0, CGPoint(x: 200, y: 250))
        _ = r.began(2, at: CGPoint(x: 200, y: 250), time: 0.2)
        let half = r.moved(2, to: CGPoint(x: 320, y: 250), time: 0.3)
        XCTAssertTrue(edges(half).contains { $0 != nil && $0!.isFull == false })
        let lift = r.ended(2, at: CGPoint(x: 320, y: 250), time: 0.4)
        XCTAssertEqual(buttons(lift), ["L↑"])
        XCTAssertFalse(lift.contains { if case .locked = $0 { true } else { false } })
    }

    func testResetReleasesWhatIsHeld() {
        var r = recognizer()
        _ = dragToEdge(&r)
        _ = r.ended(2, at: CGPoint(x: 350, y: 250), time: 0.4)
        XCTAssertEqual(buttons(r.reset(at: 1)), ["L↑"])
        var s = recognizer()
        _ = tap(&s, 1, at: 0)
        XCTAssertEqual(buttons(s.reset(at: 1)), ["L↑"])
        XCTAssertEqual(buttons(s.reset(at: 1)), [])
        // The swapped one-finger scroll running when the page leaves is cancelled (no glide).
        var t = recognizer(scroll: true)
        _ = t.began(1, at: CGPoint(x: 100, y: 300), time: 0)
        _ = t.moved([1: CGPoint(x: 100, y: 280)], time: 0.02)
        XCTAssertEqual(scrollPhases(t.reset(at: 0.03)), [.cancelled])
    }
}

final class ControlRequestTests: XCTestCase {
    func testExpectationsMergeAndAreMetByTheReportedState() {
        let lock = ControlExpectation(locked: true)
        let both = lock.merged(with: ControlExpectation(locked: false, game: .off))
        XCTAssertEqual(both, ControlExpectation(locked: false, game: .off), "the newer request wins")
        XCTAssertFalse(both.met(focus: "a", locked: true, game: .off))
        XCTAssertTrue(both.met(focus: "a", locked: false, game: .off))
        XCTAssertTrue(ControlExpectation(focus: "b").met(focus: "b", locked: true, game: .on))
    }

    func testFailureTextNamesWhatDidNotFollow() {
        let names = ["b": "MacBook-Pro"]
        XCTAssertEqual(ControlExpectation(focus: "b").failureText(focus: "a", locked: false, game: .off, name: { names[$0] }),
                       "没能把指针移到「MacBook-Pro」")
        XCTAssertEqual(ControlExpectation(locked: true).failureText(focus: nil, locked: false, game: .off, name: { _ in nil }), "指针没有锁定")
        XCTAssertEqual(ControlExpectation(game: .auto).failureText(focus: nil, locked: false, game: .off, name: { _ in nil }), "游戏模式没有切换")
        XCTAssertNil(ControlExpectation(locked: false).failureText(focus: nil, locked: false, game: .off, name: { _ in nil }))
    }

    func testOnlyRealTextLossIsShown() {
        XCTAssertNotNil(TextRefusal.paused.message)
        XCTAssertNotNil(TextRefusal.notJoined.message)
        XCTAssertNotNil(TextRefusal.unsupported.message)
        XCTAssertNil(TextRefusal.duplicate.message, "a duplicate was inserted before")
    }
}
