import UniformTypeIdentifiers
import XCTest
@testable import OinkAudioTestHost

/// User 2026-11-12: shared text goes with every format it has: plain text,
/// HTML and RTF (RTFD flattened to RTF).
final class ShareTextTests: XCTestCase {
    func testEveryTextFormatIsFoundWhateverTheOrder() {
        XCTAssertEqual(
            ShareText.types([UTType.rtf.identifier, UTType.html.identifier, UTType.utf8PlainText.identifier]),
            ShareText.Types(plain: UTType.utf8PlainText.identifier, html: UTType.html.identifier,
                            rtf: UTType.rtf.identifier, rtfd: nil))
        XCTAssertEqual(
            ShareText.types(["com.apple.flat-rtfd", UTType.html.identifier, UTType.plainText.identifier]),
            ShareText.Types(plain: UTType.plainText.identifier, html: UTType.html.identifier,
                            rtf: nil, rtfd: "com.apple.flat-rtfd"))
    }

    func testRichTextWithoutAPlainForm() {
        XCTAssertEqual(ShareText.types([UTType.html.identifier]),
                       ShareText.Types(plain: nil, html: UTType.html.identifier, rtf: nil, rtfd: nil))
        XCTAssertEqual(ShareText.plainText(fromHTML: "<html><head><style>b{}</style></head><p>Hi &amp; <b>you</b></p><p>two</p></html>"),
                       "Hi & you\ntwo")
        XCTAssertEqual(ShareText.plainText(fromRTF: #"{\rtf1\ansi Plain \b bold\b0}"#), "Plain bold")
    }

    func testRtfdIsFlattenedToRtf() throws {
        let text = NSAttributedString(string: "flat", attributes: [.font: UIFont.boldSystemFont(ofSize: 12)])
        let rtfd = try text.data(from: NSRange(location: 0, length: text.length),
                                 documentAttributes: [.documentType: NSAttributedString.DocumentType.rtfd])
        let rtf = try XCTUnwrap(ShareText.rtf(fromRTFD: rtfd))
        XCTAssertTrue(rtf.hasPrefix(#"{\rtf1"#))
        XCTAssertEqual(ShareText.plainText(fromRTF: rtf), "flat")
    }

    func testSeveralTextsKeepHtmlOnlyWhenEveryOneHasIt() {
        XCTAssertEqual(ShareText.rich(htmls: ["<b>a</b>", "<i>b</i>"], rtfs: [nil, nil]).html, "<b>a</b><br><i>b</i>")
        XCTAssertNil(ShareText.rich(htmls: ["<b>a</b>", nil], rtfs: [nil, nil]).html)
        XCTAssertEqual(ShareText.rich(htmls: [nil], rtfs: ["{\\rtf1 a}"]).rtf, "{\\rtf1 a}")
        XCTAssertNil(ShareText.rich(htmls: [nil, nil], rtfs: ["{\\rtf1 a}", "{\\rtf1 b}"]).rtf)
    }

    func testNoTextType() {
        XCTAssertEqual(ShareText.types([UTType.png.identifier, UTType.pdf.identifier]), ShareText.Types())
        XCTAssertEqual(ShareText.types([]), ShareText.Types())
    }
}
