import XCTest

/// 设置 › 退出设备组 (user 2026-11-11), on the DEBUG design-state fixtures (`-oink-ui-scene`).
/// The design states keep the session's real group; leaving only clears the fixture devices.
final class LeaveGroupTests: XCTestCase {
    private func launch(_ scene: String) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-oink-ui-scene", scene, "-AppleLanguages", "(zh-Hans)"]
        app.launch()
        return app
    }

    private func leaveButton(_ app: XCUIApplication) -> XCUIElement {
        app.buttons["退出设备组"].firstMatch
    }

    func testLeaveAsksFirstAndCancelKeepsTheGroup() {
        let app = launch("settings-end")
        let leave = leaveButton(app)
        XCTAssertTrue(leave.waitForExistence(timeout: 10), "no 退出设备组 with other devices in the group")
        leave.tap()
        let alert = app.alerts["退出设备组"]
        XCTAssertTrue(alert.waitForExistence(timeout: 5), "no confirmation")
        XCTAssertTrue(alert.staticTexts["退出后，需要重新配对才能和组内设备共享。"].exists)
        alert.buttons["取消"].tap()
        XCTAssertTrue(alert.waitForNonExistence(timeout: 3))
        XCTAssertTrue(app.buttons["关闭设置"].exists, "cancel left the settings page")
        XCTAssertTrue(leaveButton(app).exists)
    }

    func testConfirmedLeaveReturnsToTheEmptyDevicesPage() {
        let app = launch("settings-end")
        let leave = leaveButton(app)
        XCTAssertTrue(leave.waitForExistence(timeout: 10))
        leave.tap()
        let alert = app.alerts["退出设备组"]
        XCTAssertTrue(alert.waitForExistence(timeout: 5))
        alert.buttons["退出"].tap()
        XCTAssertTrue(app.staticTexts["已退出设备组"].waitForExistence(timeout: 5), "no 已退出设备组 toast")
        XCTAssertTrue(app.buttons["关闭设置"].waitForNonExistence(timeout: 3), "settings stayed open")
        XCTAssertEqual(app.tabBars.buttons.allElementsBoundByIndex.first { $0.isSelected }?.label, "设备")
        XCTAssertFalse(app.staticTexts["Office-PC"].exists, "the group's devices are still listed")
    }

    func testNoLeaveButtonWithoutOtherDevices() {
        let app = launch("settings-alone")
        XCTAssertTrue(app.buttons["关闭设置"].waitForExistence(timeout: 10))
        app.swipeUp(); app.swipeUp()
        let licenses = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "开源许可")).firstMatch
        XCTAssertTrue(licenses.waitForExistence(timeout: 3), "not at the end of the settings page")
        XCTAssertFalse(leaveButton(app).exists, "退出设备组 shown with no other device in the group")
    }
}
