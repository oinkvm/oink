import XCTest

/// Device sheet heading, on the DEBUG design-state fixtures (`-oink-ui-scene`).
final class DeviceSheetTests: XCTestCase {
    private func launch(_ scene: String) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-oink-ui-scene", scene, "-AppleLanguages", "(zh-Hans)"]
        app.launch()
        return app
    }

    /// User 2026-11-11: a phone has no paused state, so its sheet offers only kick.
    func testPhoneSheetHasNoPauseButton() {
        let app = launch("sheet-mobile")
        XCTAssertTrue(app.buttons["踢出设备"].waitForExistence(timeout: 10))
        XCTAssertFalse(app.buttons["暂停"].exists, "a phone's sheet shows 暂停")
        XCTAssertFalse(app.buttons["继续"].exists, "a phone's sheet shows 继续")
    }

    func testComputerSheetKeepsPauseButton() {
        let app = launch("sheet")
        XCTAssertTrue(app.buttons["踢出设备"].waitForExistence(timeout: 10))
        XCTAssertTrue(app.buttons["暂停"].exists || app.buttons["继续"].exists, "a computer's sheet lost 暂停")
    }
}
