import UIKit
import UniformTypeIdentifiers
import XCTest
@testable import OinkAudioTestHost

final class PasteboardCopyTests: XCTestCase {
    private var pasteboard: UIPasteboard!

    override func setUp() {
        pasteboard = UIPasteboard(name: UIPasteboard.Name("oink-copy-test-\(UUID().uuidString)"), create: true)
    }

    override func tearDown() {
        if let pasteboard { UIPasteboard.remove(withName: pasteboard.name) }
    }

    private func message(_ kind: TransferMessageKind, text: String? = nil, path: String? = nil, mime: String? = nil,
                         state: AttachmentState? = nil, marker: Data? = nil) -> TransferMessage {
        TransferMessage(id: "1", createdAt: Date(), senderDeviceID: "pc", senderName: "PC", offerID: nil, sourceBoot: nil,
                        operationID: nil, itemID: nil, representationID: nil, originSenderID: "pc", proxyDeviceID: nil,
                        proxyName: nil, kind: kind, text: text, contentID: nil, localPath: path, attachmentName: nil,
                        byteSize: nil, uti: mime, state: state, availability: "Ready", isMine: false, clipMarker: marker)
    }

    private func copy(_ message: TransferMessage) throws {
        let item = try XCTUnwrap(try PasteboardCopy.item(for: message))
        PasteboardCopy.write(item, to: pasteboard)
    }

    private var marker: Data { Data("OKC1".utf8) + Data(repeating: 7, count: 48) }

    func testSensitiveTextCopiesTheTextWithTheConcealedMarker() throws {
        var secret = message(.text, text: "hunter2", marker: marker)
        secret.sensitive = true
        try copy(secret)
        XCTAssertEqual(pasteboard.string, "hunter2")
        XCTAssertTrue(pasteboard.types.contains(PasteboardCopy.concealedType))
        try copy(message(.text, text: "plain"))
        XCTAssertFalse(pasteboard.types.contains(PasteboardCopy.concealedType))
    }

    func testTextCopyWritesTheTextAndTheClipMarker() throws {
        try copy(message(.text, text: "from pc", marker: marker))
        XCTAssertEqual(pasteboard.numberOfItems, 1)
        XCTAssertEqual(pasteboard.string, "from pc")
        let types = try XCTUnwrap(pasteboard.types(forItemSet: nil)?.first)
        XCTAssertTrue(types.contains(UTType.utf8PlainText.identifier))
        XCTAssertTrue(types.contains(PasteboardCopy.markerType))
        XCTAssertEqual(pasteboard.data(forPasteboardType: PasteboardCopy.markerType), marker)
    }

    /// User 2026-11-12: 复制 writes one item with the rich text (HTML, and
    /// RTF when that was the source) next to the plain text; 复制文本 the
    /// plain text only. Both carry the clip marker.
    func testRichCopyWritesHtmlRtfAndPlainTextInOneItem() throws {
        var bold = message(.text, text: "bold move", marker: marker)
        bold.hasRichText = true
        let rich = TransferRichText(displayHTML: "<b>bold</b> move", html: "<p><b>bold</b> move</p>",
                                    rtf: #"{\rtf1 \b bold\b0  move}"#)
        XCTAssertTrue(try PasteboardCopy.copyRich(bold, rich: rich, to: pasteboard))
        XCTAssertEqual(pasteboard.numberOfItems, 1)
        let types = try XCTUnwrap(pasteboard.types(forItemSet: nil)?.first)
        XCTAssertTrue(types.contains(UTType.utf8PlainText.identifier))
        XCTAssertTrue(types.contains(UTType.html.identifier))
        XCTAssertTrue(types.contains(UTType.rtf.identifier))
        XCTAssertEqual(pasteboard.string, "bold move")
        let html = try XCTUnwrap(pasteboard.data(forPasteboardType: UTType.html.identifier))
        XCTAssertEqual(String(data: html, encoding: .utf8), "<p><b>bold</b> move</p>")
        XCTAssertEqual(pasteboard.data(forPasteboardType: UTType.rtf.identifier), Data(#"{\rtf1 \b bold\b0  move}"#.utf8))
        XCTAssertEqual(pasteboard.data(forPasteboardType: PasteboardCopy.markerType), marker)

        XCTAssertTrue(try PasteboardCopy.copyPlain(bold, to: pasteboard))
        let plain = try XCTUnwrap(pasteboard.types(forItemSet: nil)?.first)
        XCTAssertFalse(plain.contains(UTType.html.identifier))
        XCTAssertEqual(pasteboard.string, "bold move")
        XCTAssertEqual(pasteboard.data(forPasteboardType: PasteboardCopy.markerType), marker)
    }

    func testRecordWithoutClipIdentityHasNoMarker() throws {
        try copy(message(.text, text: "shared", marker: nil))
        XCTAssertEqual(pasteboard.string, "shared")
        XCTAssertFalse(pasteboard.contains(pasteboardTypes: [PasteboardCopy.markerType]))
    }

    func testDownloadedImageCopiesItsDataUnderItsTypeWithTheMarker() throws {
        let png = try XCTUnwrap(UIGraphicsImageRenderer(size: CGSize(width: 2, height: 2)).image { _ in }.pngData())
        let url = FileManager.default.temporaryDirectory.appendingPathComponent("oink-copy-\(UUID().uuidString).png")
        try png.write(to: url)
        defer { try? FileManager.default.removeItem(at: url) }
        try copy(message(.image, path: url.path, mime: "image/png", state: .stored, marker: marker))
        XCTAssertEqual(pasteboard.data(forPasteboardType: UTType.png.identifier), png)
        XCTAssertEqual(pasteboard.data(forPasteboardType: PasteboardCopy.markerType), marker)
    }

    func testOnlyTextAndDownloadedImagesCanBeCopied() {
        XCTAssertTrue(PasteboardCopy.canCopy(message(.text, text: "t")))
        XCTAssertFalse(PasteboardCopy.canCopy(message(.image, state: .placeholder)))
        XCTAssertFalse(PasteboardCopy.canCopy(message(.file, path: "/tmp/a.pdf", state: .stored)))
        XCTAssertFalse(PasteboardCopy.canCopy(message(.folder, state: .stored)))
    }
}
