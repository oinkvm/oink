import XCTest

/// Phone-to-phone transfer and the share device picker (native-rewrite.md「传输页」2026-11-11)
/// on the DEBUG design states (`-oink-ui-scene transfer-p2p-*`, `share-pick-*`), in Chinese.
/// Screenshots go to `OINK_SHOT_DIR` when it is set (`TEST_RUNNER_OINK_SHOT_DIR=…`).
final class TransferP2PUITests: XCTestCase {
    override func setUp() { continueAfterFailure = false }

    private func launch(_ scene: String, scheme: String = "light") -> XCUIApplication {
        let app = XCUIApplication()
        app.launchArguments = ["-oink-ui-scene", scene, "-oink-scheme", scheme, "-oink-theme", "pink",
                               "-AppleLanguages", "(zh-Hans)", "-AppleLocale", "zh_CN"]
        app.launch()
        return app
    }

    private func shot(_ name: String) {
        let png = XCUIScreen.main.screenshot().pngRepresentation
        let attachment = XCTAttachment(data: png, uniformTypeIdentifier: "public.png")
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
        if let dir = ProcessInfo.processInfo.environment["OINK_SHOT_DIR"] {
            try? png.write(to: URL(fileURLWithPath: dir).appendingPathComponent("\(name).png"))
        }
    }

    private func composer(_ app: XCUIApplication) -> XCUIElement {
        app.descendants(matching: .any).matching(identifier: "transfer-composer").firstMatch
    }

    private func text(_ app: XCUIApplication, _ value: String) -> XCUIElement {
        app.staticTexts.matching(NSPredicate(format: "label == %@", value)).firstMatch
    }

    private func placeholder(_ app: XCUIApplication) -> String {
        let field = composer(app)
        XCTAssertTrue(field.waitForExistence(timeout: 15))
        return field.placeholderValue ?? ""
    }

    // MARK: Composer by the group's other devices

    func testComposerFollowsTheOtherDevices() {
        var app = launch("transfer-p2p-only-pc")
        XCTAssertEqual(placeholder(app), "发送给 Office-PC")
        XCTAssertFalse(app.buttons["transfer-target"].exists, "one other device: no picker")
        XCTAssertTrue(composer(app).isEnabled)
        app.terminate()

        app = launch("transfer-p2p-only-offline")
        XCTAssertEqual(placeholder(app), "DESKTOP-9AO8GPM 不在线")
        XCTAssertFalse(composer(app).isEnabled)
        XCTAssertFalse(app.buttons["添加附件"].exists)
        app.terminate()

        app = launch("transfer-p2p-none")
        XCTAssertEqual(placeholder(app), "没有其它设备在线")
        XCTAssertTrue(app.buttons["transfer-target"].exists, "no other device: the picker stays")
        XCTAssertFalse(composer(app).isEnabled)
        app.terminate()

        app = launch("transfer-p2p-all-offline")
        XCTAssertEqual(placeholder(app), "没有其它设备在线")
        XCTAssertFalse(composer(app).isEnabled)
        app.terminate()

        app = launch("transfer-p2p-phone")
        XCTAssertEqual(placeholder(app), "发送给 Android")
        app.terminate()

        app = launch("transfer-p2p-menu")
        XCTAssertEqual(placeholder(app), "发送给 Office-PC，可在所有设备粘贴")
        app.buttons["transfer-target"].tap()
        for name in ["Office-PC", "MacBook-Pro", "iPhone", "iPad", "Android"] {
            XCTAssertTrue(app.buttons[name].waitForExistence(timeout: 5), name)
        }
        XCTAssertFalse(app.buttons["DESKTOP-9AO8GPM"].exists, "offline devices are not listed")
        XCTAssertTrue(app.staticTexts["电脑"].exists && app.staticTexts["移动设备"].exists, "section titles")
        // Picking a phone: files go to it only, no paste hint.
        app.buttons["Android"].tap()
        XCTAssertEqual(placeholder(app), "发送给 Android")
    }

    func testAnOfflineChoiceSwitchesToAComputer() {
        let app = launch("transfer-p2p-live-offline")
        XCTAssertEqual(placeholder(app), "发送给 iPhone")
        // The chosen phone leaves while the page is open (the scene's button, after the first state).
        app.buttons["design-take-iphone-offline"].tap()
        XCTAssertTrue(text(app, "iPhone 已离线，已切回当前活跃").waitForExistence(timeout: 8))
        XCTAssertEqual(placeholder(app), "发送给 Office-PC，可在所有设备粘贴")
        app.terminate()

        // The choice was already offline at launch: the same hint, named from the device list.
        let launched = launch("transfer-p2p-offline")
        XCTAssertTrue(text(launched, "iPhone 已离线，已切回当前活跃").waitForExistence(timeout: 15))
        XCTAssertEqual(placeholder(launched), "发送给 Office-PC，可在所有设备粘贴")
    }

    // MARK: Keyboard on tab entry

    private func editing(_ field: XCUIElement) -> Bool {
        (field.value(forKey: "hasKeyboardFocus") as? Bool) ?? false
    }

    private func startsEditing(_ field: XCUIElement) -> Bool {
        let started = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in self.editing(field) }, object: nil)
        return XCTWaiter().wait(for: [started], timeout: 5) == .completed
    }

    /// The software keyboard covers the tab bar: the way to another tab while typing is to swipe
    /// the conversation down first, which takes the keyboard away (scrollDismissesKeyboard).
    private func swipeKeyboardAway(_ app: XCUIApplication) {
        // A drag on the conversation (not `scrollViews.firstMatch`: that was the keyboard's
        // candidate bar, and the swipe typed a candidate), down into the keyboard.
        app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.3))
            .press(forDuration: 0.05, thenDragTo: app.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.95)))
        XCTAssertTrue(app.keyboards.firstMatch.waitForNonExistence(timeout: 5), "the swipe takes the keyboard away")
    }

    /// Entering 传输 from another tab opens the page with the keyboard closed; only a tap on the
    /// composer opens it (用户 2026-11-12). The composer was being edited when the page was
    /// left, through the system tab bar and through the control page's own tabs.
    func testEnteringTheTabLeavesTheKeyboardClosed() {
        let app = launch("transfer-p2p-only-pc")
        let field = composer(app)
        XCTAssertTrue(field.waitForExistence(timeout: 15))
        XCTAssertTrue(field.isEnabled)

        // Back from 设备.
        field.tap()
        XCTAssertTrue(startsEditing(field), "a tap opens the keyboard")
        swipeKeyboardAway(app)
        app.tabBars.buttons["设备"].tap()
        XCTAssertTrue(app.buttons["打开设置"].firstMatch.waitForExistence(timeout: 5))
        app.tabBars.buttons["传输"].tap()
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        // Give a returning focus time to show.
        _ = app.keyboards.firstMatch.waitForExistence(timeout: 1.5)
        XCTAssertFalse(editing(field), "the composer took focus on entering the tab")
        XCTAssertEqual(app.keyboards.count, 0, "the keyboard opened on entering the tab")

        // Back from 控制 (the control page's own tab bar).
        field.tap()
        XCTAssertTrue(startsEditing(field), "a tap still opens the keyboard")
        swipeKeyboardAway(app)
        app.tabBars.buttons["控制"].tap()
        let back = app.buttons["control-tab-transfer"]
        XCTAssertTrue(back.waitForExistence(timeout: 5))
        back.tap()
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        _ = app.keyboards.firstMatch.waitForExistence(timeout: 1.5)
        XCTAssertFalse(editing(field), "the composer took focus on entering the tab from the control page")
        XCTAssertEqual(app.keyboards.count, 0, "the keyboard opened on entering the tab from the control page")
    }

    func testCurrentActiveMenuEntryIsTheDefault() {
        let app=launch("transfer-p2p-phone")
        XCTAssertTrue(app.buttons["transfer-target"].waitForExistence(timeout:15))
        app.buttons["transfer-target"].tap()
        XCTAssertTrue(app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "当前活跃")).firstMatch.waitForExistence(timeout:5))
        XCTAssertFalse(app.staticTexts["跟随焦点"].exists)
        shot("shared-history-active-target")
    }

    // MARK: Messages

    func testCancelSendingRemovesTheMessage() {
        let app = launch("transfer-p2p-phone")
        XCTAssertTrue(text(app, "正在发送给 Android · 45%").waitForExistence(timeout: 15))
        // Every completed phone-to-phone bubble keeps its recipient while a newer one sends.
        XCTAssertTrue(text(app, "已发送给 Android").exists)
        let zip = text(app, "现场照片（原图）.zip")
        zip.press(forDuration: 1.0)
        XCTAssertTrue(app.buttons["取消发送"].waitForExistence(timeout: 5))
        XCTAssertFalse(app.buttons["删除"].exists, "a send in flight offers only 取消发送")
        app.buttons["取消发送"].tap()
        XCTAssertTrue(zip.waitForNonExistence(timeout: 5))
    }

    func testCancelReceivingLeavesCancelled() {
        let app = launch("transfer-p2p-receiving-menu")
        let zip = text(app, "现场照片（原图）.zip")
        XCTAssertTrue(zip.waitForExistence(timeout: 15))
        zip.press(forDuration: 1.0)
        XCTAssertTrue(app.buttons["取消接收"].waitForExistence(timeout: 5))
        XCTAssertTrue(app.buttons["删除"].exists)
        app.buttons["取消接收"].tap()
        XCTAssertTrue(text(app, "已取消").waitForExistence(timeout: 5))
        XCTAssertTrue(zip.exists)
    }

    func testFailuresSayThePeerLeft() {
        var app = launch("transfer-p2p-send-failed")
        XCTAssertTrue(text(app, "对方已断开，发送失败").waitForExistence(timeout: 15))
        app.terminate()
        app = launch("transfer-p2p-receive-failed")
        XCTAssertTrue(text(app, "对方已断开，接收失败").waitForExistence(timeout: 15))
        XCTAssertTrue(app.buttons["图片接收失败"].exists)
        app.terminate()
        app = launch("transfer-p2p-send-cancelled-by-peer")
        XCTAssertTrue(text(app, "已取消").waitForExistence(timeout: 15))
    }

    // MARK: Share picker

    func testSharePickerConfirmsBeforeSending() {
        let app = launch("share-pick-flow")
        XCTAssertTrue(text(app, "正在查找设备…").waitForExistence(timeout: 15))
        XCTAssertTrue(text(app, "上次在线").exists)
        // Not online: a hint, and the device leaves the list.
        app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "MacBook-Pro")).firstMatch.tap()
        XCTAssertTrue(app.buttons.matching(NSPredicate(format: "label == %@", "正在连接 MacBook-Pro")).firstMatch.waitForExistence(timeout: 2))
        XCTAssertTrue(text(app, "MacBook-Pro 不在线").waitForExistence(timeout: 5))
        XCTAssertFalse(app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "MacBook-Pro")).firstMatch.exists)
        // Online: the card sends to the phone and asks to stay open.
        app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Android")).firstMatch.tap()
        XCTAssertTrue(text(app, "正在发送给 Android…").waitForExistence(timeout: 6))
        XCTAssertTrue(text(app, "传完前请不要关闭此页面").exists)
        XCTAssertTrue(app.buttons["取消"].exists)
        XCTAssertTrue(text(app, "已发送给 Android，耗时 1.52 秒").waitForExistence(timeout: 5))
        XCTAssertFalse(app.buttons["取消"].exists)
    }

    func testSharePickerCancel() {
        let app = launch("share-pick-flow")
        XCTAssertTrue(text(app, "正在查找设备…").waitForExistence(timeout: 15))
        app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Office-PC")).firstMatch.tap()
        XCTAssertTrue(app.buttons["取消"].waitForExistence(timeout: 6))
        app.buttons["取消"].tap()
        XCTAssertTrue(app.buttons["取消"].waitForNonExistence(timeout: 3))
    }

    /// The picker is the page itself, closed by its × (user 2026-11-12); under the card it
    /// stays visible but out of reach.
    func testSharePickerPageCloseAndCardOverIt() {
        var app = launch("share-pick-live")
        var close = app.buttons["share-picker-close"]
        XCTAssertTrue(close.waitForExistence(timeout: 15))
        XCTAssertEqual(close.label, "关闭")
        XCTAssertTrue(text(app, "发送到").exists)
        close.tap()
        XCTAssertTrue(text(app, "发送到").waitForNonExistence(timeout: 3))
        app.terminate()
        // Under the card the × is out of reach. XCUITest's element tree also lists views
        // VoiceOver skips, so its presence proves nothing: tap where it is drawn instead.
        app = launch("share-pick-live")
        close = app.buttons["share-picker-close"]
        XCTAssertTrue(close.waitForExistence(timeout: 15))
        let frame = close.frame
        let spot = app.coordinate(withNormalizedOffset: .zero).withOffset(CGVector(dx: frame.midX, dy: frame.midY))
        app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Office-PC")).firstMatch.tap()
        XCTAssertTrue(app.buttons["取消"].waitForExistence(timeout: 5))
        spot.tap()
        // The × would have closed the page and the card with it; the card runs on to sent.
        XCTAssertTrue(text(app, "已发送给 Office-PC，耗时 1.52 秒").waitForExistence(timeout: 5), "the page's × is under the card")
    }

    /// The only other device is offline: the card waits over a page listing just that device.
    func testSharePickerWaitingForTheOnlyDevice() {
        let app = launch("share-pick-waiting")
        XCTAssertTrue(app.buttons["取消"].waitForExistence(timeout: 15))
        XCTAssertTrue(app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH %@", "对方不在线")).firstMatch.exists)
    }

    func testSharePickerEmptyAndLive() {
        var app = launch("share-pick-empty")
        XCTAssertTrue(text(app, "没有在线的设备").waitForExistence(timeout: 15))
        XCTAssertTrue(text(app, "请在其它设备上打开 Oink").exists)
        app.terminate()
        app = launch("share-pick-live")
        XCTAssertTrue(text(app, "在线").waitForExistence(timeout: 15))
        XCTAssertFalse(text(app, "正在查找设备…").exists)
        XCTAssertFalse(text(app, "上次在线").exists)
    }

    // MARK: Screenshots of every state (light and dark)

    func testStateScreenshots() {
        let plain = ["transfer-p2p-all-offline", "transfer-p2p-phone", "transfer-p2p-offline", "transfer-p2p-cancelled-sender",
                     "transfer-p2p-send-cancelled-by-peer", "transfer-p2p-send-failed", "transfer-p2p-receive",
                     "transfer-p2p-cancelled-receiver", "transfer-p2p-receive-cancelled", "transfer-p2p-receive-failed",
                     "transfer-p2p-none", "transfer-p2p-only-pc", "transfer-p2p-only-phone", "transfer-p2p-only-offline",
                     "share-pick-cached", "share-pick-connecting", "share-pick-offline", "share-pick-live", "share-pick-empty",
                     "share-pick-sending", "share-pick-sent", "share-pick-failed", "share-pick-waiting"]
        for scheme in ["light", "dark"] {
            for scene in plain {
                let app = launch(scene, scheme: scheme)
                // The offline-switch hint lasts 2.5 s: catch it while it shows.
                if scene == "transfer-p2p-offline" {
                    _ = text(app, "iPhone 已离线，已切回当前活跃").waitForExistence(timeout: 15)
                } else {
                    sleep(3)
                }
                shot("\(scene)-\(scheme)")
                app.terminate()
            }
            for scene in ["transfer-p2p-menu", "transfer-p2p-menu-phone", "transfer-p2p-menu-offline"] {
                let app = launch(scene, scheme: scheme)
                XCTAssertTrue(app.buttons["transfer-target"].waitForExistence(timeout: 15))
                sleep(1)
                app.buttons["transfer-target"].tap()
                sleep(1)
                shot("\(scene)-\(scheme)")
                app.terminate()
            }
            for scene in ["transfer-p2p-sending-menu", "transfer-p2p-receiving-menu"] {
                let app = launch(scene, scheme: scheme)
                let zip = text(app, "现场照片（原图）.zip")
                XCTAssertTrue(zip.waitForExistence(timeout: 15))
                sleep(1)
                zip.press(forDuration: 1.0)
                sleep(1)
                shot("\(scene)-\(scheme)")
                app.terminate()
            }
        }
    }
}
