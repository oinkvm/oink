import XCTest

final class AudioForwardUITests: XCTestCase {
    override func setUp() { continueAfterFailure = false }
    override func tearDown() { XCUIDevice.shared.orientation = .portrait }

    private func launch(_ scene: String, active: Bool = false) -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-oink-ui-scene", scene, "-oink-forward-stub", "-AppleLanguages", "(zh-Hans)"]
        if active { app.launchArguments.append("-oink-forward-active") }
        app.launch()
        return app
    }

    func testToolbarCancellationStartStopAndSwitchToExit() {
        let app = launch("devices")
        let speaker = app.buttons["audio-toolbar-speaker"]
        XCTAssertTrue(speaker.waitForExistence(timeout: 10))
        XCTAssertEqual(speaker.label, "扬声器：开")
        speaker.press(forDuration: 0.7)
        XCTAssertTrue(app.alerts["广播控制器测试替身"].waitForExistence(timeout: 5))
        app.alerts.buttons["取消"].tap()
        XCTAssertEqual(speaker.label, "扬声器：开")
        XCTAssertFalse(app.staticTexts["调节本机音量不影响转发"].exists)
        speaker.press(forDuration: 0.7)
        app.alerts.buttons["开始转发"].tap()
        XCTAssertTrue(app.staticTexts["调节本机音量不影响转发"].waitForExistence(timeout: 5))
        XCTAssertEqual(speaker.label, "扬声器：转发中")
        speaker.press(forDuration: 0.7)
        let exit = NSPredicate(format: "label == %@", "扬声器：开")
        expectation(for: exit, evaluatedWith: speaker)
        waitForExpectations(timeout: 5)
        speaker.tap()
        XCTAssertEqual(speaker.label, "扬声器：关")
    }

    func testAdjustReceiveDisabledWhileForwarding() {
        let app = launch("control-adjust", active: true)
        let speaker = app.switches["audio-adjust-speaker"]
        XCTAssertTrue(speaker.waitForExistence(timeout: 10))
        XCTAssertFalse(speaker.isEnabled)
        XCTAssertEqual(speaker.value as? String, "0")
        XCTAssertTrue(app.switches["audio-adjust-microphone"].isEnabled)
    }

    func testExpandedKeyboardReceiveDisabledWhileForwarding() {
        XCUIDevice.shared.orientation = .landscapeLeft
        let app = launch("control-keyboard-expanded", active: true)
        let speaker = app.buttons["audio-key-speaker"]
        XCTAssertTrue(speaker.waitForExistence(timeout: 10))
        XCTAssertFalse(speaker.isEnabled)
        XCTAssertEqual(speaker.label, "接收音频")
        XCTAssertEqual(speaker.value as? String, "关")
        XCTAssertTrue(app.buttons["audio-key-microphone"].isEnabled)
    }

    func testSettingOffersTwoOptionsWithoutLongPressFootnote() {
        let app = launch("settings-audio")
        let row = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "单击扬声器按钮行为")).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 10))
        row.tap()
        XCTAssertTrue(app.buttons["设为出口"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.buttons["转发音频"].exists)
        XCTAssertFalse(app.staticTexts["长按执行另一种"].exists)
        app.buttons["转发音频"].tap()
        app.buttons["返回"].tap()
        app.buttons["关闭设置"].tap()
        let speaker = app.buttons["audio-toolbar-speaker"]
        speaker.tap() // exit -> off, regardless of preference
        speaker.tap() // off -> preferred forward action
        XCTAssertTrue(app.alerts["广播控制器测试替身"].waitForExistence(timeout: 5))
    }
}
