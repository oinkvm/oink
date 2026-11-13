import XCTest

/// File cards, picture sizes, the 已发送 rule and the folder page (native-rewrite.md「传输页」
/// 「文件卡片」「图片尺寸」「发送状态行「已发送」」「文件夹页」, user 2026-11-11) on the DEBUG
/// design states, in Chinese. Screenshots go to `OINK_SHOT_DIR` when it is set
/// (`TEST_RUNNER_OINK_SHOT_DIR=…`).
final class TransferFileCardUITests: XCTestCase {
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

    private func texts(_ app: XCUIApplication, _ value: String) -> XCUIElementQuery {
        app.staticTexts.matching(NSPredicate(format: "label == %@", value))
    }

    private func card(_ app: XCUIApplication, _ name: String) -> XCUIElement {
        app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", name)).firstMatch
    }

    /// PC/group messages have no successful caption; a failed send keeps its reason.
    func testPcMessagesHaveNoSentSuccessCaption() {
        let app = launch("transfer-sent-latest")
        XCTAssertTrue(texts(app, "对方已断开，发送失败").firstMatch.waitForExistence(timeout: 15))
        sleep(1)
        XCTAssertEqual(texts(app, "已发送").count, 0)
        let newest = card(app, "排期表.xlsx")
        XCTAssertTrue(newest.exists)
        XCTAssertEqual(texts(app, "已发送给 Android").count, 0)
    }

    func testHistoryRetryIsShownOnlyAfterFailureAndTheWindowBoundaryIsVisible() {
        var app = launch("transfer-history-loading")
        XCTAssertTrue(texts(app, "正在加载更早的记录…").firstMatch.waitForExistence(timeout: 15))
        XCTAssertFalse(app.buttons["加载更早的记录"].exists)
        shot("shared-history-loading")
        app.terminate()
        app = launch("transfer-history-failed", scheme: "dark")
        XCTAssertTrue(app.buttons["加载更早的记录"].waitForExistence(timeout: 15))
        XCTAssertTrue(texts(app, "加载失败").firstMatch.exists)
        shot("shared-history-retry-dark")
        app.terminate()
        app = launch("transfer-history-boundary")
        XCTAssertTrue(texts(app, "已显示最近 100 条共享记录").firstMatch.waitForExistence(timeout: 15))
        XCTAssertTrue(texts(app, "更早的记录仅保存在本机").firstMatch.exists)
        shot("shared-history-boundary")
    }

    /// Received and own pictures are 220 wide; an own photo keeps its 4 : 3 (it was cropped
    /// to 260 : 146), a picture without known proportions takes the 260 : 146 placeholder.
    func testPicturesAre220WideAtTheirProportions() {
        let app = launch("transfer-images")
        let own = app.buttons.matching(identifier: "transfer-image-9242").firstMatch
        XCTAssertTrue(own.waitForExistence(timeout: 15))
        sleep(1)
        XCTAssertEqual(own.frame.width, 220, accuracy: 1)
        XCTAssertEqual(own.frame.height, 165, accuracy: 1)
        for id in ["9241", "9243"] {
            let received = app.buttons.matching(identifier: "transfer-image-\(id)").firstMatch
            XCTAssertTrue(received.exists, id)
            XCTAssertEqual(received.frame.width, 220, accuracy: 1, id)
            XCTAssertEqual(received.frame.height, 124, accuracy: 1, id)
        }
    }

    /// Expanded received files carry the single card's button: download, retry; folders none.
    func testExpandedBatchItemsHaveTheirButtons() {
        let app = launch("transfer-file-batch-received")
        XCTAssertTrue(app.buttons["下载 Q4 设计评审.pdf"].waitForExistence(timeout: 15))
        XCTAssertTrue(app.buttons["重试下载 图标资源（全部尺寸）.zip"].exists)
        XCTAssertFalse(app.buttons["下载 会议纪要.docx"].exists, "a download in flight has no button")
        XCTAssertFalse(app.buttons["下载 项目素材"].exists, "a folder has no download")
        let item = card(app, "Q4 设计评审.pdf")
        XCTAssertEqual(item.frame.maxX - item.frame.minX, 220 - 12 - 8 - 40 - 10, accuracy: 2,
                       "an expanded item is not 220 wide")
    }

    /// Single cards: the states that offer no download do not show one.
    func testProblemCardsOfferNoDownload() {
        let app = launch("transfer-file-problems")
        XCTAssertTrue(texts(app, "对方已断开，接收失败").firstMatch.waitForExistence(timeout: 15))
        sleep(1)
        // 内容已失效: the card does nothing, so it is no button.
        XCTAssertFalse(card(app, "Oink-0.3.4.dmg").exists)
        XCTAssertTrue(app.descendants(matching: .any).matching(NSPredicate(format: "label CONTAINS %@", "暂时无法加载")).firstMatch.exists)
        shot("shared-history-availability")
        for name in ["现场照片（原图）.zip", "合同扫描.pdf", "Oink-0.3.4.dmg", "报价单 v3.docx"] {
            XCTAssertFalse(app.buttons["下载 \(name)"].exists, name)
            XCTAssertFalse(app.buttons["重试下载 \(name)"].exists, name)
        }
    }

    /// Folder rows: download and retry on files, none on the sub-folder.
    func testFolderRowsShareTheCardButton() {
        let app = launch("transfer-folder-states")
        XCTAssertTrue(app.buttons["下载 现场 02.jpg"].waitForExistence(timeout: 15))
        XCTAssertTrue(app.buttons["下载 施工说明（第二版，含图纸目录与变更记录）.docx"].exists)
        XCTAssertTrue(app.buttons["重试下载 原始照片.zip"].exists)
        XCTAssertFalse(app.buttons["下载 现场 01.jpg"].exists, "a downloaded file has no download")
        XCTAssertFalse(app.buttons["下载 图纸"].exists, "a sub-folder has no download")
    }

    // MARK: Screenshots of every state (light and dark)

    func testStateScreenshots() {
        let scenes = ["transfer-file-received", "transfer-file-problems", "transfer-file-sent", "transfer-sent-latest",
                      "transfer-images", "transfer-file-batch-received", "transfer-folder-states", "transfer-folder",
                      "transfer-stack-files", "transfer-stack-expanded-self"]
        for scheme in ["light", "dark"] {
            for scene in scenes {
                let app = launch(scene, scheme: scheme)
                sleep(3)
                shot("\(scene)-\(scheme)")
                app.terminate()
            }
        }
    }
}
