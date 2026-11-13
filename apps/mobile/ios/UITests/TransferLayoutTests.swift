import XCTest

/// Transfer page layout on the DEBUG fixtures (`-oink-ui-scene`).
final class TransferLayoutTests: XCTestCase {
    /// A long file name wraps inside the bubble and its download button stays on screen
    /// (2026-11-09: a macOS screen recording pushed the card and its button off the right edge).
    func testLongFileNameKeepsDownloadOnScreen() throws {
        let app = XCUIApplication()
        app.launchArguments = ["-oink-ui-scene", "transfer-layout-edges", "-AppleLanguages", "(zh-Hans)"]
        app.launch()
        let download = app.buttons["下载 Screen Recording 2026-11-09 at 12.37.04 PM.mov"]
        XCTAssertTrue(download.waitForExistence(timeout: 10))
        let screen = app.windows.firstMatch.frame
        XCTAssertLessThanOrEqual(download.frame.maxX, screen.maxX, "the download button is off screen")
        let card = app.buttons.matching(NSPredicate(format: "label BEGINSWITH %@", "Screen Recording")).firstMatch
        XCTAssertLessThanOrEqual(card.frame.maxX, screen.maxX, "the file card runs past the screen edge")
    }

    /// A portrait image only answers taps on its visible bubble (2026-11-09: a phone
    /// screenshot's cropped-away part covered the text above it, so tapping that text
    /// opened the image viewer).
    func testPortraitImageDoesNotCoverTheTextAboveIt() throws {
        let app = XCUIApplication()
        app.launchArguments = ["-oink-ui-scene", "transfer-layout-edges", "-AppleLanguages", "(zh-Hans)"]
        app.launch()
        let text = app.staticTexts["图片上面的文字"]
        XCTAssertTrue(text.waitForExistence(timeout: 10))
        text.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5)).tap()
        XCTAssertFalse(app.buttons["保存图片到图库"].waitForExistence(timeout: 2), "tapping the text opened the image")
    }

    /// A text whose body could not be fetched says so and offers a retry instead of an
    /// empty bubble (2026-11-09: a text copied on an unreachable Windows PC showed as a blank bubble).
    func testTextWithoutBodyIsExplained() throws {
        let app = XCUIApplication()
        app.launchArguments = ["-oink-ui-scene", "transfer-layout-edges", "-AppleLanguages", "(zh-Hans)"]
        app.launch()
        let status = app.descendants(matching: .any).matching(NSPredicate(format: "label CONTAINS %@ OR value CONTAINS %@", "获取失败，点按重试", "获取失败，点按重试")).firstMatch
        XCTAssertTrue(status.waitForExistence(timeout: 10), "the failed text shows no status")
    }
}
