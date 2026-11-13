import XCTest
@testable import OinkAudioTestHost

final class QuickLookSequenceTests: XCTestCase {
    private func message(_ id: Int, _ kind: TransferMessageKind, operation: String? = nil, stored: Bool = true,
                         size: Int64 = 1_000_000, thumbnail: Bool = false) -> TransferMessage {
        var m = TransferMessage(id: String(id), createdAt: Date(timeIntervalSince1970: Double(id)), senderDeviceID: "pc",
                                senderName: "pc", offerID: nil, sourceBoot: nil, operationID: operation, itemID: 0,
                                representationID: 0, originSenderID: "pc", proxyDeviceID: nil, proxyName: nil, kind: kind,
                                text: kind == .text ? "hi" : nil, contentID: "c\(id)", attachmentName: "f\(id)", byteSize: size,
                                uti: nil, state: stored ? .stored : .placeholder, availability: "Ready", isMine: false,
                                delivery: nil, deliveryFailure: nil)
        m.localPath = stored ? "/files/\(id)" : nil
        if thumbnail {
            m.imagePreview = .thumbnail
            m.thumbnailPath = "/thumbs/\(id).jpg"
        }
        return m
    }

    private func entries(_ list: [TransferMessage], canPreview: @escaping (URL) -> Bool = { _ in true }) -> [QuickLookSequence.Entry] {
        QuickLookSequence.entries(list, original: { $0.localPath.map { URL(fileURLWithPath: $0) } },
                                  thumbnail: { $0.thumbnailPath.map { URL(fileURLWithPath: $0) } }, canPreview: canPreview)
    }

    func testDownloadedPicturesVideosAndFilesInConversationOrder() {
        let list = [message(1, .image), message(2, .text), message(3, .file), message(4, .folder), message(5, .file)]
        XCTAssertEqual(entries(list).map(\.messageID), ["1", "3", "5"])
        XCTAssertFalse(entries(list).contains { $0.standIn })
    }

    func testABatchPagesAsItsItemsInOrder() {
        let batch = (10...13).map { message($0, $0 == 12 ? .file : .image, operation: "op") }
        let list = [message(1, .image)] + batch + [message(20, .image)]
        XCTAssertEqual(entries(list).map(\.messageID), ["1", "10", "11", "12", "13", "20"])
    }

    func testPicturesNotHereYetPageInWithTheirThumbnailUnder16MB() {
        let small = message(1, .image, stored: false, size: 15_999_999, thumbnail: true)
        let large = message(2, .image, stored: false, size: 16_000_000, thumbnail: true)
        let noThumbnail = message(3, .image, stored: false)
        let file = message(4, .file, stored: false, thumbnail: true)
        let result = entries([small, large, noThumbnail, file])
        XCTAssertEqual(result.map(\.messageID), ["1"])
        XCTAssertEqual(result.first?.url, URL(fileURLWithPath: "/thumbs/1.jpg"))
        XCTAssertEqual(result.first?.standIn, true)
    }

    func testTheOriginalReplacesTheThumbnailOnceDownloaded() {
        let downloaded = message(1, .image, stored: true, thumbnail: true)
        let entry = QuickLookSequence.entry(downloaded, original: { $0.localPath.map { URL(fileURLWithPath: $0) } },
                                            thumbnail: { $0.thumbnailPath.map { URL(fileURLWithPath: $0) } }, canPreview: { _ in true })
        XCTAssertEqual(entry?.url, URL(fileURLWithPath: "/files/1"))
        XCTAssertEqual(entry?.standIn, false)
    }

    func testFilesQuickLookCannotShowAndFolderChildrenAreLeftOut() {
        var child = message(2, .file)
        child.parentID = 7
        let list = [message(1, .file), child, message(3, .file)]
        XCTAssertEqual(entries(list, canPreview: { $0.path != "/files/3" }).map(\.messageID), ["1"])
    }

    func testOpensOnTheTappedItem() {
        let list = [message(1, .image), message(2, .text), message(3, .file), message(4, .image)]
        let result = entries(list)
        XCTAssertEqual(QuickLookSequence.index(of: "4", in: result), 2)
        XCTAssertNil(QuickLookSequence.index(of: "2", in: result))
    }
}
