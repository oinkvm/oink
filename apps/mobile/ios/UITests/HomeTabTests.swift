import XCTest

/// 设置 › 默认页面: the chosen page opens at launch, whichever tab was used last.
final class HomeTabTests: XCTestCase {
    private let app = XCUIApplication()

    private func choose(_ page: String) {
        // The transfer page has no settings button; the devices page does.
        let devices = app.tabBars.buttons["设备"]
        XCTAssertTrue(devices.waitForExistence(timeout: 10))
        devices.tap()
        let gear = app.buttons["打开设置"].firstMatch
        XCTAssertTrue(gear.waitForExistence(timeout: 10))
        gear.tap()
        let row = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "默认页面，")).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 5))
        row.tap()
        // The picker rows sit above the tab bar; the tab bar has a button with the same name.
        let option = app.buttons.matching(NSPredicate(format: "label == %@", page))
            .allElementsBoundByIndex.first { $0.frame.minY < 700 }
        XCTAssertNotNil(option)
        option?.tap()
    }

    private func selectedTab() -> String? {
        app.tabBars.buttons.allElementsBoundByIndex.first { $0.isSelected }?.label
    }

    func testDefaultPageSurvivesSwitchingTabs() throws {
        app.launchArguments = ["-AppleLanguages", "(zh-Hans)"]
        app.launch()
        choose("传输")
        app.launch()
        XCTAssertTrue(app.tabBars.buttons["传输"].waitForExistence(timeout: 10))
        XCTAssertEqual(selectedTab(), "传输")
        // Using another tab is not choosing a new default page.
        app.tabBars.buttons["控制"].tap()
        app.launch()
        XCTAssertTrue(app.tabBars.buttons["传输"].waitForExistence(timeout: 10))
        XCTAssertEqual(selectedTab(), "传输", "switching tabs replaced the chosen default page")
        // Back to the default preference.
        choose("设备")
    }

    func testSettingsLeftPresentationAndRightEdgeBack() {
        continueAfterFailure = false
        XCUIDevice.shared.orientation = .portrait
        app.launchArguments = ["-oink-ui-scene", "devices", "-AppleLanguages", "(zh-Hans)"]
        app.launch()
        defer { app.terminate() }
        let gear = app.buttons["打开设置"].firstMatch
        XCTAssertTrue(gear.waitForExistence(timeout: 10))
        gear.tap()
        let close = app.buttons["关闭设置"]
        XCTAssertTrue(close.waitForExistence(timeout: 5))
        // A short, held drag cancels; it must not accidentally close the root.
        let edge = app.coordinate(withNormalizedOffset: CGVector(dx: 0.99, dy: 0.5))
        edge.press(forDuration: 0.05, thenDragTo: edge.withOffset(CGVector(dx: -30, dy: 0)),
                   withVelocity: .slow, thenHoldForDuration: 0.4)
        XCTAssertTrue(close.isHittable)
        // Push a child without changing a preference; its native left-edge pop is retained.
        let row = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "默认页面，")).firstMatch
        row.tap()
        let back = app.buttons["返回"].firstMatch
        XCTAssertTrue(back.waitForExistence(timeout: 5))
        let left = app.coordinate(withNormalizedOffset: CGVector(dx: 0.01, dy: 0.5))
        left.press(forDuration: 0.05, thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.85, dy: 0.5)))
        XCTAssertTrue(close.waitForExistence(timeout: 5), "child native back still works")
        edge.press(forDuration: 0.05, thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.2, dy: 0.5)))
        XCTAssertTrue(close.waitForNonExistence(timeout: 5), "root dismisses left from the right edge")
        XCTAssertTrue(gear.isHittable)
        // The explicit back button uses the same leftward exit.
        gear.tap()
        XCTAssertTrue(close.waitForExistence(timeout: 5))
        close.tap()
        XCTAssertTrue(close.waitForNonExistence(timeout: 5))
        XCTAssertTrue(gear.isHittable)
    }

}
