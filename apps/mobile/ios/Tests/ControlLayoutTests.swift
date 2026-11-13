import XCTest
@testable import OinkAudioTestHost

/// Control page geometry (control-page.md §1–§2, 用户 2026-11-12), on an iPhone in landscape:
/// 852 × 393, safe area 59 on both sides, home indicator 21, system keyboard about 183 with its strip.
final class ControlLayoutTests: XCTestCase {
    private let full = CGSize(width: 852, height: 393)

    private func layout(_ keyboard: ControlLandscapeLayout.Keyboard, islandRight: Bool = false,
                        keyboardHeight: CGFloat = 0) -> ControlLandscapeLayout {
        ControlLandscapeLayout(full: full,
                               insets: LayoutInsets(top: 0, leading: 59, bottom: max(21, keyboardHeight), trailing: 59),
                               home: 21, islandRight: islandRight, keyboard: keyboard)
    }

    func testFloatingBarRowIs12BelowTheScreenTop() {
        let l = layout(.none)
        XCTAssertEqual(l.bar, CGRect(x: 75, y: 12, width: 702, height: 48))
        XCTAssertEqual(layout(.floating, islandRight: true).bar, l.bar, "the bar does not swap")
    }

    func testPadCoversTheWholeScreenUnlessTheSystemKeyboardIsUp() {
        XCTAssertEqual(layout(.none).pad, CGRect(x: 0, y: 0, width: 852, height: 393))
        XCTAssertEqual(layout(.floating).pad, CGRect(x: 0, y: 0, width: 852, height: 393), "the keyboard floats over it")
        XCTAssertNil(layout(.system, keyboardHeight: 259).pad, "H134: two rows without a pad")
        XCTAssertNotNil(layout(.system, keyboardHeight: 258).pad, "H135: split with a pad")
        XCTAssertEqual(layout(.none).releaseBottom, 14 + 21, "release capsule over the home indicator")
    }

    func testFloatingContainerRunsFromTheBarRowToTheBottomSafeEdge() {
        let l = layout(.floating)
        let box = l.floating(x: l.floatBounds.home(islandRight: false))
        XCTAssertEqual(box, CGRect(x: 852 - 59 - 420, y: 12, width: 420, height: 393 - 12 - 21))
        let flipped = layout(.floating, islandRight: true)
        XCTAssertEqual(flipped.floating(x: flipped.floatBounds.home(islandRight: true)).minX, 59)
    }

    func testFloatingKeyboardScalesAndClampsToAvailableSafeWidth() {
        XCTAssertEqual(layout(.floating).floatScale, 1.05)
        var narrow = layout(.floating)
        narrow.full.width = 500
        XCTAssertEqual(narrow.floatWidth, 382, accuracy: 0.001)
        XCTAssertEqual(narrow.floatScale, 382 / 400, accuracy: 0.001)
        XCTAssertEqual(narrow.floatBounds.lo, narrow.floatBounds.hi, accuracy: 0.001)
        XCTAssertEqual(narrow.floating(x: narrow.floatBounds.lo).maxX, 500 - 59, accuracy: 0.001)
        XCTAssertEqual(narrow.floating(x: narrow.floatBounds.lo).height, 360, "height cannot grow beyond screen")
    }

    func testFloatingNormalKeysTakeThreeQuartersAndWidthIsFixed() {
        XCTAssertEqual(ControlLandscapeLayout.floatingReferenceWidth, 420)
        for height: CGFloat in [260, 320, 360, 388] {
            let keys = FloatingKeyHeights(available: height, gap: 6)
            XCTAssertEqual(keys.normal, keys.control * 3)
            XCTAssertEqual(keys.control + keys.normal + 6, height)
        }
        var wider = layout(.floating)
        wider.full.width = 1024
        XCTAssertEqual(wider.floatWidth, 420, "not computed from current screen width")
    }

    func testTopBarHitGeometryExcludesGapsAndRoundedCorners() {
        let a = CGRect(x: 0, y: 0, width: 48, height: 48)
        let b = CGRect(x: 58, y: 0, width: 48, height: 48)
        let gap = CGPoint(x: 53, y: 24)
        XCTAssertFalse(ControlBarHitTest.contains(gap, in: a))
        XCTAssertFalse(ControlBarHitTest.contains(gap, in: b))
        XCTAssertFalse(ControlBarHitTest.contains(.zero, in: a))
        XCTAssertTrue(ControlBarHitTest.contains(CGPoint(x: 24, y: 24), in: a))
        XCTAssertTrue(ControlBarHitTest.contains(CGPoint(x: 80, y: 24), in: CGRect(x: 0, y: 0, width: 180, height: 48)))
    }

    func testPortraitBarIsACentredGroupWithEightPointGap() {
        XCTAssertEqual(PortraitControlBarMetrics.gap, 8)
        XCTAssertEqual(PortraitControlBarMetrics.tabTravel, 35)
    }

    func testSystemKeyboardThreshold134And135() throws {
        XCTAssertEqual(ControlLandscapeLayout.systemReferenceWidth, 405)
        XCTAssertEqual(ControlLandscapeLayout.systemThreshold, 135)
        for flipped in [false, true] {
            let small = layout(.system, islandRight: flipped, keyboardHeight: 259)
            XCTAssertEqual(small.systemHeight, 134)
            XCTAssertTrue(small.systemTwoRows)
            XCTAssertNil(small.pad)
            XCTAssertEqual(small.systemPanel, CGRect(x: 59, y: 0, width: 734, height: 134))
            let split = layout(.system, islandRight: flipped, keyboardHeight: 258)
            XCTAssertEqual(split.systemHeight, 135)
            XCTAssertFalse(split.systemTwoRows)
            XCTAssertEqual(split.systemPanel, CGRect(x: 426, y: 0, width: 367, height: 135))
            let pad = try XCTUnwrap(split.pad)
            XCTAssertLessThan(pad.maxX, 426)
            XCTAssertEqual(pad.maxY, 129)
        }
    }

    func testSystemKeyboardRowsKeepAll26KeysAndNavigationBeforeMedia() {
        let two = SystemControlKey.rows(two: true, fn: false, mac: false)
        XCTAssertEqual(two[0], [.esc, .copy, .paste, .home, .end, .pageUp, .pageDown, .previous, .play, .next, .mute, .volumeDown, .volumeUp, .switchKeyboard])
        XCTAssertEqual(two[1], [.caps, .ctrl, .meta, .alt, .tab, .delete, .fn, .left, .up, .down, .right, .hideKeyboard])
        let three = SystemControlKey.rows(two: false, fn: false, mac: false)
        XCTAssertEqual(three.map(\.count), [10, 8, 8])
        XCTAssertEqual(three[1], [.caps, .home, .end, .pageUp, .pageDown, .fn, .up, .hideKeyboard])
        XCTAssertEqual(three[2], [.ctrl, .meta, .alt, .tab, .delete, .left, .down, .right])
        XCTAssertEqual(two.flatMap { $0 }.count, 26)
        XCTAssertEqual(three.flatMap { $0 }.count, 26)
        for compact in [false, true] {
            let normal = SystemControlKey.rows(two: compact, fn: false, mac: true)
            let function = SystemControlKey.rows(two: compact, fn: true, mac: true)
            XCTAssertEqual(function[0], [.esc] + (1...12).map { .function($0) } + [.switchKeyboard])
            XCTAssertEqual(Array(function.dropFirst()), Array(normal.dropFirst()))
            let mods = compact ? Array(normal[1][1...3]) : Array(normal[2][0...2])
            XCTAssertEqual(mods, [.ctrl, .alt, .meta])
        }
    }

    func testExpandedKeyboardKeepsTheSafeAreasAndTheHomeIndicator() {
        XCTAssertEqual(layout(.expanded).expandedPadding, LayoutInsets(top: 8, leading: 59, bottom: 21 + 4, trailing: 59))
    }

    func testFocusCapsuleLeftPaddingMatchesTheRight() {
        // Windows logo: 24 × 0.62 centred in the 26 slot, −2 margin; chevron 4 inside its 16 box.
        let logoInset = (26 - 24 * 0.62) / 2 - 2
        for m in [FocusCapsuleMetrics.portrait, .landscape] {
            XCTAssertEqual(m.leading + logoInset, m.trailing + 4, accuracy: 0.1)
            XCTAssertEqual(m.leadingNoLogo, m.trailing + 4)
        }
        XCTAssertEqual(FocusCapsuleMetrics.portrait.trailing + 4, 16)
        XCTAssertEqual(FocusCapsuleMetrics.landscape.trailing + 4, 20)
    }

    func testLockButtonShowsGameModeOnlyWhileOn() {
        XCTAssertEqual(LockButtonState(game: .on, gameActive: true, locked: false), .gameOn)
        XCTAssertEqual(LockButtonState(game: .auto, gameActive: true, locked: false), .gameAuto)
        XCTAssertEqual(LockButtonState(game: .auto, gameActive: false, locked: false), .unlocked, "auto, idle: no gamepad")
        XCTAssertEqual(LockButtonState(game: .auto, gameActive: false, locked: true), .locked)
        XCTAssertEqual(LockButtonState(game: .off, gameActive: false, locked: true), .locked)
        XCTAssertTrue(LockButtonState.gameAuto.game)
        XCTAssertFalse(LockButtonState.locked.game)
    }

    /// User 2026-11-12: the emulated keyboard follows a standard system keyboard (within ±10 % of
    /// 266), and keeps 266 when an input-method panel makes the system keyboard taller.
    func testEmulatedKeyboardFollowsOnlyAStandardSystemKeyboard() {
        XCTAssertEqual(EmulatedKeyboardHeight.keys(observed: nil), 266, "none seen yet")
        XCTAssertEqual(EmulatedKeyboardHeight.keys(observed: 267), 267, "iPhone 18 Pro simulator")
        XCTAssertEqual(EmulatedKeyboardHeight.keys(observed: 284), 284, "iPhone Air")
        XCTAssertEqual(EmulatedKeyboardHeight.keys(observed: 292), 292, "within 10 %")
        XCTAssertEqual(EmulatedKeyboardHeight.keys(observed: 240), 240)
        XCTAssertEqual(EmulatedKeyboardHeight.keys(observed: 266 + 83), 266, "九键 pinyin + candidate rows")
        XCTAssertEqual(EmulatedKeyboardHeight.keys(observed: 266 + 45), 266, "a toolbar strip")
        XCTAssertEqual(EmulatedKeyboardHeight.keys(observed: 200), 266)
    }

    func testEmulatedKeyboardHasTheNumberRowOnBothLayers() {
        XCTAssertEqual(String(EmulatedRows.numbers), "1234567890")
        XCTAssertEqual(EmulatedRows.letters.count + 2, EmulatedRows.rowCount)
        XCTAssertEqual(EmulatedRows.symbols.count + 2, EmulatedRows.rowCount)
        XCTAssertFalse(EmulatedRows.symbols.joined().contains("1"), "digits only in the number row")
        XCTAssertEqual(EmulatedRows.face("1", shift: true), "!")
        XCTAssertEqual(EmulatedRows.face("0", shift: true), ")")
        XCTAssertEqual(EmulatedRows.face(",", shift: true), "<")
        XCTAssertEqual(EmulatedRows.face(".", shift: false), ".")
        XCTAssertEqual(EmulatedRows.face("a", shift: true), "A")
        XCTAssertEqual(EmulatedRows.face("@", shift: true), "@")
    }
}
