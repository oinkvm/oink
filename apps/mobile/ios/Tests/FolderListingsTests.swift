import XCTest
@testable import OinkAudioTestHost

/// A folder page ends in its page or in a failure the folder view shows
/// instead of loading forever; 重试 loads again.
final class FolderListingsTests: XCTestCase {
    private let root = IOSDirectoryKey(folderMessageID: 34, directory: 0)
    private let gone = TransferDirectoryFailure(text: "文件夹内容已失效", retryable: false)
    private let lost = TransferDirectoryFailure(text: "无法获取文件夹内容：连接中断，请重试。", retryable: true)
    private func entry(_ id: UInt64) -> TransferDirectoryEntry {
        TransferDirectoryEntry(id: id, parent: 1, name: "f\(id)", kind: "file", size: 1, version: 1)
    }

    func testTheRootPageIsFiledUnderTheNodeItWasRequestedWith() {
        var listings = FolderListings()
        listings.requested("a", key: root, append: false)
        listings.page("a", folderID: 34, directory: 0, entries: [entry(5)], nextOffset: nil, nextVersion: nil)
        XCTAssertEqual(listings.pages[root]?.entries.map(\.id), [5])
        XCTAssertNil(listings.failures[root])
    }

    func testAFailedFirstPageStopsLoadingAndDropsAnEarlierListing() {
        var listings = FolderListings()
        listings.requested("a", key: root, append: false)
        listings.page("a", folderID: 34, directory: 0, entries: [entry(5)], nextOffset: nil, nextVersion: nil)
        // The folder is opened again after its local copy was cleared.
        listings.requested("b", key: root, append: false)
        listings.failed("b", folderID: 34, directory: 0, failure: gone)
        XCTAssertNil(listings.pages[root])
        XCTAssertEqual(listings.failures[root], gone)
        XCTAssertTrue(listings.requests.isEmpty)
    }

    func testAFailedNextPageKeepsTheEntriesShownAndRetryClearsIt() {
        var listings = FolderListings()
        listings.requested("a", key: root, append: false)
        listings.page("a", folderID: 34, directory: 0, entries: [entry(5)], nextOffset: 1, nextVersion: 3)
        listings.requested("b", key: root, append: true)
        listings.failed("b", folderID: 34, directory: 0, failure: lost)
        XCTAssertEqual(listings.pages[root]?.entries.map(\.id), [5])
        XCTAssertEqual(listings.failures[root], lost)
        // 重试: loading again until the page arrives.
        listings.requested("c", key: root, append: true)
        XCTAssertNil(listings.failures[root])
        listings.page("c", folderID: 34, directory: 0, entries: [entry(6)], nextOffset: nil, nextVersion: nil)
        XCTAssertEqual(listings.pages[root]?.entries.map(\.id), [5, 6])
    }

    func testARequestThatWasNotMadeFailsAtOnce() {
        var listings = FolderListings()
        listings.requested("a", key: root, append: false)
        listings.notRequested("a", failure: lost)
        XCTAssertEqual(listings.failures[root], lost)
        XCTAssertTrue(listings.requests.isEmpty)
    }
}
