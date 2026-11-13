import XCTest

/// Rich text bubbles (native-rewrite.md「富文本气泡（用户 2026-11-12 确认）」) on the DEBUG design
/// states `-oink-ui-scene transfer-rich*`, in Chinese. Screenshots go to `OINK_SHOT_DIR` when it is set (`TEST_RUNNER_OINK_SHOT_DIR=…`).
final class TransferRichTextUITests: XCTestCase {
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

    private func rich(_ app: XCUIApplication, _ id: Int) -> XCUIElement {
        app.descendants(matching: .any).matching(identifier: "transfer-rich-\(9000 + id)").firstMatch
    }

    private func text(_ app: XCUIApplication, prefix: String) -> XCUIElement {
        app.staticTexts.matching(NSPredicate(format: "label BEGINSWITH %@", prefix)).firstMatch
    }

    /// The menu's buttons top to bottom.
    private func menu(_ app: XCUIApplication) -> [String] {
        XCTAssertTrue(app.buttons["删除"].waitForExistence(timeout: 5))
        return ["复制", "复制文本", "选择", "删除"]
            .filter { app.buttons[$0].exists }
            .sorted { app.buttons[$0].frame.minY < app.buttons[$1].frame.minY }
    }

    // MARK: Long-press menu

    func testRichTextMenuHasCopyFirstThenCopyText() {
        var app = launch("transfer-rich-menu")
        let received = rich(app, 301)
        XCTAssertTrue(received.waitForExistence(timeout: 15))
        received.press(forDuration: 1.0)
        XCTAssertEqual(menu(app), ["复制", "复制文本", "选择", "删除"], "received rich text")
        shot("menu-menu-ios-light")
        app.terminate()

        app = launch("transfer-rich-menu-self")
        let own = rich(app, 302)
        XCTAssertTrue(own.waitForExistence(timeout: 15))
        own.press(forDuration: 1.0)
        XCTAssertEqual(menu(app), ["复制", "复制文本", "选择", "删除"], "own rich text is formatted too")
        shot("menu-menu-self-ios-light")
    }

    func testPlainTextAndHTMLWithoutFormattingHaveNoCopyText() {
        var app = launch("transfer-rich-menu-plain")
        let plain = text(app, prefix: "今晚的安排我整理了一下")
        XCTAssertTrue(plain.waitForExistence(timeout: 15))
        plain.press(forDuration: 1.0)
        XCTAssertEqual(menu(app), ["复制", "选择", "删除"])
        shot("menu-menu-plain-ios-light")
        app.terminate()

        app = launch("transfer-rich-menu-noformat")
        let noFormat = text(app, prefix: "这一条只有颜色、字体和字号")
        XCTAssertTrue(noFormat.waitForExistence(timeout: 15))
        XCTAssertFalse(rich(app, 330).exists, "shown as plain text")
        noFormat.press(forDuration: 1.0)
        XCTAssertEqual(menu(app), ["复制", "选择", "删除"])
        shot("menu-menu-noformat-ios-light")
    }

    func testLongPressOnAWideTableOpensTheMenu() {
        let app = launch("transfer-rich-menu-table")
        let wide = rich(app, 341)
        XCTAssertTrue(wide.waitForExistence(timeout: 15))
        // On the table itself: a press that does not move is the message's long press.
        app.scrollViews.matching(identifier: "transfer-rich-table").firstMatch.press(forDuration: 1.0)
        XCTAssertEqual(menu(app), ["复制", "复制文本", "选择", "删除"])
        shot("menu-menu-table-ios-light")
    }

    // MARK: Layout

    func testWideTableScrollsInsideTheBubbleNotTheList() {
        let app = launch("transfer-rich-table")
        let caption = text(app, prefix: "本周构建")
        XCTAssertTrue(caption.waitForExistence(timeout: 15))
        let tables = app.scrollViews.matching(identifier: "transfer-rich-table")
        XCTAssertGreaterThanOrEqual(tables.count, 2, "the two wide tables scroll; the narrow one does not")
        let cell = app.staticTexts["11-11 21:40"]
        XCTAssertTrue(cell.exists)
        let cellX = cell.frame.minX
        let captionY = caption.frame.minY
        tables.element(boundBy: 0).swipeLeft()
        XCTAssertLessThan(cell.frame.minX, cellX - 20, "the table scrolled sideways")
        XCTAssertEqual(caption.frame.minY, captionY, accuracy: 1, "the list did not move")
        shot("snippets-table-ios-light")
    }

    func testLinksAreLinks() {
        let app = launch("transfer-rich-links")
        XCTAssertTrue(rich(app, 320).waitForExistence(timeout: 15))
        XCTAssertTrue(app.links["前台"].exists, "tel link")
        XCTAssertTrue(app.links["team@example.com"].exists, "mailto link")
        XCTAssertTrue(app.links["UIPasteboard | Apple Developer"].exists)
    }

    func testFallbacksShowText() {
        let app = launch("transfer-rich-fallback")
        XCTAssertTrue(rich(app, 331).waitForExistence(timeout: 15))
        XCTAssertTrue(text(app, prefix: "[图片：甘特图]").exists, "a picture is its alt text")
        XCTAssertFalse(app.links["不安全的链接"].exists, "javascript: is no link")
        XCTAssertTrue(app.links["引导页"].exists)
    }

    func testLongRichTextFoldsByHeight() {
        let app = launch("transfer-rich-long")
        XCTAssertTrue(rich(app, 360).waitForExistence(timeout: 15))
        let more = app.buttons["查看更多"]
        XCTAssertTrue(more.waitForExistence(timeout: 5))
        let y = more.frame.minY
        shot("long-long-ios-light")
        more.tap()
        // Everything (or 20 more lines' height) shows: the row moved down or went away.
        let moved = NSPredicate { _, _ in !more.exists || more.frame.minY > y + 100 }
        XCTAssertEqual(XCTWaiter().wait(for: [XCTNSPredicateExpectation(predicate: moved, object: nil)], timeout: 5), .completed)
    }

    // MARK: Screenshots of every state (mockup shots, light and dark)

    func testStateScreenshots() {
        let states: [(scene: String, name: String)] = [
            ("transfer-rich", "snippets-short"), ("transfer-rich-list", "snippets-list"),
            ("transfer-rich-links", "snippets-links"), ("transfer-rich-table", "snippets-table"),
            ("transfer-rich-fallback", "snippets-fallback"), ("transfer-rich-long", "long-long"),
            ("transfer-rich-long-more", "long-long-more"), ("transfer-rich-long-more-end", "long-long-more-end"),
        ]
        for scheme in ["light", "dark"] {
            for state in states {
                let app = launch(state.scene, scheme: scheme)
                XCTAssertTrue(app.descendants(matching: .any).matching(NSPredicate(format: "identifier BEGINSWITH %@", "transfer-rich-"))
                    .firstMatch.waitForExistence(timeout: 15), state.scene)
                // The design states place the list after half a second (TransferView.place).
                Thread.sleep(forTimeInterval: 1.5)
                shot("\(state.name)-ios-\(scheme)")
                app.terminate()
            }
        }
    }
}
