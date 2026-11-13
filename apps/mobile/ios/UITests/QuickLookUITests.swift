import XCTest

/// Quick Look over the conversation on the DEBUG fixtures (`-oink-ui-scene`).
final class QuickLookUITests: XCTestCase {
    /// The item Quick Look shows, by its file name. Quick Look's title is a button with a
    /// menu (iOS 27), not a static text; the name is the label of the shown picture.
    private func shown(_ app: XCUIApplication, _ name: String) -> XCUIElement {
        app.otherElements["QLPreviewControllerView"].descendants(matching: .any)
            .matching(NSPredicate(format: "label == %@", name)).firstMatch
    }

    /// Tapping a picture opens Quick Look on it, and a swipe pages to the next item of the
    /// conversation (user 2026-11-11: the system viewer replaces Oink's own).
    func testPictureOpensQuickLookAndPagesToTheNextItem() throws {
        let app = XCUIApplication()
        app.launchArguments = ["-oink-ui-scene", "transfer-stack-expanded", "-AppleLanguages", "(zh-Hans)"]
        app.launch()
        let picture = app.buttons.matching(identifier: "transfer-image-9020").firstMatch
        XCTAssertTrue(picture.waitForExistence(timeout: 10))
        picture.tap()
        XCTAssertTrue(shown(app, "客厅 · 早上.jpg").waitForExistence(timeout: 10), "Quick Look did not open on the tapped picture")
        app.otherElements["QLPreviewControllerView"].swipeLeft()
        XCTAssertTrue(shown(app, "窗边.jpg").waitForExistence(timeout: 10), "Quick Look did not page to the next item")
    }
}
