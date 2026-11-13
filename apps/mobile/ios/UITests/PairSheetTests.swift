import XCTest

/// Pairing sheet hit areas, on the DEBUG design-state fixtures (`-oink-ui-scene`).
final class PairSheetTests: XCTestCase {
    /// The whole glass capsule closes a failed pairing, not only its「关闭」glyphs
    /// (user 2026-11-09: after a rejected pairing the close button did not react).
    func testFailedPairingClosesFromTheCapsuleEdge() throws {
        let app = XCUIApplication()
        app.launchArguments = ["-oink-ui-scene", "pair-failed-rejected", "-AppleLanguages", "(zh-Hans)"]
        app.launch()
        let failed = app.staticTexts["配对失败"]
        XCTAssertTrue(failed.waitForExistence(timeout: 10))
        let close = app.buttons["关闭"]
        XCTAssertTrue(close.waitForExistence(timeout: 5))
        // The capsule spans the sheet's content width: tap 24 pt inside its leading
        // end, well outside the centred label.
        let window = app.windows.firstMatch.frame
        let y = close.frame.midY
        app.coordinate(withNormalizedOffset: .zero)
            .withOffset(CGVector(dx: window.minX + 24 + 24 + 24, dy: y)).tap()
        XCTAssertTrue(failed.waitForNonExistence(timeout: 3), "the capsule outside its label did not close the sheet")
    }

    /// User 2026-11-11: once a pairing completes, the success sheet keeps its code, closes by
    /// itself and the app returns to the devices page, without opening a device sheet.
    /// `pair-complete-in`: the other side started (code typed here); `-out`: this phone started.
    private func assertCompletedPairingReturnsToDevices(_ scene: String) {
        let app = XCUIApplication()
        app.launchArguments = ["-oink-ui-scene", scene, "-AppleLanguages", "(zh-Hans)"]
        app.launch()
        // The scene opens the add page with the success sheet on top; the sheet closes 900 ms
        // after it shows (often before the first query here), and only that close leaves the
        // add page.
        XCTAssertTrue(app.buttons["打开设置"].firstMatch.waitForExistence(timeout: 15), "\(scene): not back on the devices page")
        XCTAssertFalse(app.staticTexts["配对成功"].exists, "\(scene): the success sheet did not close")
        XCTAssertFalse(app.buttons["本机信息"].exists, "\(scene): still on the add-device page")
        XCTAssertEqual(app.tabBars.buttons.allElementsBoundByIndex.first { $0.isSelected }?.label, "设备")
        XCTAssertFalse(app.buttons["踢出设备"].waitForExistence(timeout: 2), "\(scene): a device sheet opened")
    }

    func testCompletedIncomingPairingReturnsToDevices() {
        assertCompletedPairingReturnsToDevices("pair-complete-in")
    }

    func testCompletedOutgoingPairingReturnsToDevices() {
        assertCompletedPairingReturnsToDevices("pair-complete-out")
    }

    /// The hidden field that takes the code still takes typing (its own text is drawn clear);
    /// the cells show the digits. Four digits: a sixth would submit the fixture code.
    func testCodeEntryTakesTyping() {
        let app = XCUIApplication()
        app.launchArguments = ["-oink-ui-scene", "pair-entry", "-AppleLanguages", "(zh-Hans)"]
        app.launch()
        let field = app.textFields["配对码"]
        XCTAssertTrue(field.waitForExistence(timeout: 10))
        field.tap()
        field.typeText("2468")
        let digits = app.staticTexts.matching(identifier: "pairing-code-digit").allElementsBoundByIndex.map(\.label)
        XCTAssertEqual(digits.joined(), "2468")
    }

    /// The success state keeps the six digits (typed here, or shown for the other side).
    func testSuccessKeepsTheCode() {
        for scene in ["pair-success", "pair-success-out"] {
            let app = XCUIApplication()
            app.launchArguments = ["-oink-ui-scene", scene, "-AppleLanguages", "(zh-Hans)"]
            app.launch()
            XCTAssertTrue(app.staticTexts["配对成功"].waitForExistence(timeout: 10), "\(scene): no success sheet")
            let digits = app.staticTexts.matching(identifier: "pairing-code-digit").allElementsBoundByIndex.map(\.label)
            XCTAssertEqual(digits.joined(), "246810", "\(scene): the success state lost the code")
        }
    }
}
