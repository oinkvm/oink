import XCTest
@testable import OinkAudioTestHost

/// Settings › 传输 › 清理传输记录 (settings-and-pairing.md §3.3, user 2026-11-11).
@MainActor
final class TransferHistoryClearTests: XCTestCase {
    private struct Failure: LocalizedError {
        var errorDescription: String? { "无法清理传输记录" }
    }

    func testRowIsGreyWithoutRecordsAndATapAsksNothing() {
        var cleared = 0
        let model = TransferHistoryClear(hasHistory: { false }, clear: { cleared += 1 })
        model.refresh()
        XCTAssertTrue(model.disabled)
        model.request()
        XCTAssertFalse(model.confirming)
        XCTAssertEqual(cleared, 0)
    }

    func testConfirmClearsThroughTheCoreAndShowsTheToast() {
        var records = true
        var cleared = 0
        let model = TransferHistoryClear(hasHistory: { records }, clear: { cleared += 1; records = false })
        model.refresh()
        XCTAssertFalse(model.disabled)
        model.request()
        XCTAssertTrue(model.confirming, "the confirm dialog opens first")
        XCTAssertEqual(cleared, 0)
        XCTAssertEqual(model.confirm(), "已清理传输记录")
        XCTAssertEqual(cleared, 1)
        XCTAssertFalse(model.confirming)
        XCTAssertFalse(model.clearing)
        // The page is empty now: the row goes grey.
        XCTAssertTrue(model.disabled)
    }

    func testAFailedClearNamesTheReasonAndKeepsTheRow() {
        let model = TransferHistoryClear(hasHistory: { true }, clear: { throw Failure() })
        model.refresh()
        model.request()
        XCTAssertEqual(model.confirm(), "无法清理传输记录")
        XCTAssertFalse(model.disabled)
    }
}
