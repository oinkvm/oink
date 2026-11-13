import SwiftUI
import XCTest
@testable import OinkAudioTestHost

/// Rich text bubbles (native-rewrite.md「富文本气泡（用户 2026-11-12 确认）」): the cleaned markup
/// parsed into blocks and runs, the layout numbers, the fold by height, the long-press menu and
/// the plain text of 复制文本.
final class RichTextTests: XCTestCase {
    private func doc(_ html: String) -> [RichBlock] { RichDocument.parse(html)?.blocks ?? [] }
    private func run(_ text: String, _ marks: RichMarks = [], link: String? = nil) -> RichRun {
        RichRun(text: text, marks: marks, link: link.flatMap(URL.init(string:)))
    }

    // MARK: Inline

    func testInlineFormattingAndLinks() {
        let blocks = doc(#"<p><b>今晚 8 点</b>发版，<i>请勿</i>再合并到 <code>main</code>。<s>旧包</s> <u>新包</u> <a href="https://example.com/x">清单</a>。</p>"#)
        XCTAssertEqual(blocks, [.paragraph([
            run("今晚 8 点", .bold), run("发版，"), run("请勿", .italic), run("再合并到 "), run("main", .code), run("。"),
            run("旧包", .strike), run(" "), run("新包", .underline), run(" "), run("清单", link: "https://example.com/x"), run("。"),
        ], .p)])
    }

    func testOnlyWebMailAndPhoneLinksAreLinks() {
        let blocks = doc(#"<p><a href="javascript:alert(1)">a</a><a href="file:///etc">b</a><a href="mailto:x@example.com">c</a><a href="tel:+8610">d</a><a href="oink://x">e</a></p>"#)
        guard case .paragraph(let runs, _) = blocks.first else { return XCTFail("\(blocks)") }
        XCTAssertEqual(runs, [run("ab"), run("c", link: "mailto:x@example.com"), run("d", link: "tel:+8610"), run("e")])
        XCTAssertFalse(RichLinks.allows(URL(string: "javascript:alert(1)")!))
        XCTAssertTrue(RichLinks.allows(URL(string: "HTTPS://example.com")!))
    }

    func testWhiteSpaceLineBreaksAndCharacterReferences() {
        let blocks = doc("<p>  a \n  b<br>  c &amp; &lt;d&gt; &#x4E2D;&#25991;&nbsp;e &unknown; <br></p>")
        XCTAssertEqual(blocks, [.paragraph([run("a b\nc & <d> 中文\u{00A0}e &unknown;")], .p)])
    }

    func testPictureIsItsAltTextOnly() {
        XCTAssertEqual(doc(#"<p><img alt="甘特图"></p><p>x<img alt=""></p>"#),
                       [.paragraph([run("[图片：甘特图]", .alt)], .p), .paragraph([run("x")], .p)])
    }

    func testNestedFormattingCombines() {
        XCTAssertEqual(doc("<b>a<i>b</i></b>"), [.paragraph([run("a", .bold), run("b", [.bold, .italic])], .line)])
    }

    // MARK: Blocks

    func testParagraphsDivsAndBlankLines() {
        XCTAssertEqual(doc("<p>a</p><div>b</div>c<p><br></p><p><br></p><p></p><p>d</p>"), [
            .paragraph([run("a")], .p), .paragraph([run("b")], .line), .paragraph([run("c")], .line),
            .blank, .paragraph([run("d")], .p),
        ], "consecutive empty paragraphs keep one line; an empty <p> none")
    }

    func testHeadingsAreThreeLevelsAndBoldInsideListsAndQuotes() {
        XCTAssertEqual(doc("<h1>a</h1><h2>b</h2><h3>c</h3><h5>d</h5>"),
                       [.heading(1, [run("a")]), .heading(2, [run("b")]), .heading(3, [run("c")]), .heading(3, [run("d")])])
        XCTAssertEqual(doc("<ul><li><h2>x</h2></li></ul>"),
                       [.list(RichList(ordered: false, depth: 1, items: [[.paragraph([run("x", .bold)], .line)]]))])
        XCTAssertEqual(doc("<blockquote><h1>y</h1></blockquote>"), [.quote([.paragraph([run("y", .bold)], .line)])])
    }

    func testListsNestUpToThreeLevelsAndKeepTheStart() {
        let blocks = doc(#"<ol start="3"><li>a</li><li>b<ul><li>c<ul><li>d<ul><li>e</li></ul></li></ul></li></ul></li></ol>"#)
        let e = RichList(ordered: false, depth: 3, items: [[.paragraph([run("e")], .line)]])
        let d = RichList(ordered: false, depth: 3, items: [[.paragraph([run("d")], .line), .list(e)]])
        let c = RichList(ordered: false, depth: 2, items: [[.paragraph([run("c")], .line), .list(d)]])
        XCTAssertEqual(blocks, [.list(RichList(ordered: true, start: 3, depth: 1,
                                               items: [[.paragraph([run("a")], .line)], [.paragraph([run("b")], .line), .list(c)]]))])
    }

    func testQuoteCodeBlockAndRule() {
        XCTAssertEqual(doc("<blockquote>q</blockquote><pre><code>\n  a  b\nc\n</code></pre><hr><p>x</p>"),
                       [.quote([.paragraph([run("q")], .line)]), .code("  a  b\nc"), .rule, .paragraph([run("x")], .p)])
    }

    func testTableRowsCellsAndHeader() {
        let blocks = doc("<p>排期：</p><table><tr><th>项目</th><th>日期</th></tr><tr><td>iOS <b>包</b></td><td>11/15<br>上午</td></tr></table>")
        guard case .table(let table) = blocks.last else { return XCTFail("\(blocks)") }
        XCTAssertTrue(table.header)
        XCTAssertEqual(table.columns, 2)
        XCTAssertEqual(table.rows[1], [RichCell(head: false, runs: [run("iOS "), run("包", .bold)]),
                                       RichCell(head: false, runs: [run("11/15\n上午")])])
        XCTAssertFalse(RichTable(rows: [[RichCell(head: true, runs: [run("a")]), RichCell(head: false, runs: [run("b")])]]).header,
                       "a header row is all th")
    }

    func testNothingToShowIsNoDocument() {
        XCTAssertNil(RichDocument.parse(""))
        XCTAssertNil(RichDocument.parse("<p> </p><div>\n</div>"))
        XCTAssertNil(RichContent(rich: TransferRichText(displayHTML: "", html: "<span style=\"color:red\">x</span>", rtf: nil)),
                     "HTML that kept no formatting is plain text")
        XCTAssertNotNil(RichContent(rich: TransferRichText(displayHTML: "<b>x</b>", html: "<b>x</b>", rtf: nil)))
    }

    func testUnbalancedMarkupIsClosed() {
        XCTAssertEqual(doc("<p><b>a</p>b</b>"), [.paragraph([run("a", .bold)], .p), .paragraph([run("b")], .line)])
        XCTAssertEqual(doc("a < b"), [.paragraph([run("a < b")], .line)])
    }

    // MARK: Plain text (复制文本 without the source's text/plain)

    func testPlainText() {
        let document = RichDocument.parse("<h2>买</h2><ul><li>线</li><li>壳</li></ul><ol start=\"2\"><li>x</li></ol>"
            + "<table><tr><td>a</td><td>b</td></tr><tr><td>c</td><td>d</td></tr></table><p><img alt=\"图\"></p>")
        XCTAssertEqual(document?.plainText, "买\n• 线\n• 壳\n2. x\na\tb\nc\td\n[图片：图]")
    }

    // MARK: Layout

    func testBlockMarginsCollapse() {
        let p = RichBlock.paragraph([run("a")], .p)
        XCTAssertEqual(RichLayout.gap(p, .heading(2, [])), 10)
        XCTAssertEqual(RichLayout.gap(.heading(1, []), p), 4)
        XCTAssertEqual(RichLayout.gap(.heading(3, []), .paragraph([], .line)), 0)
        XCTAssertEqual(RichLayout.gap(p, .rule), 8)
        XCTAssertEqual(RichLayout.gap(.list(RichList(ordered: false, depth: 1, items: [])), p), 6)
        XCTAssertEqual(RichLayout.gap(.paragraph([], .line), .list(RichList(ordered: false, depth: 2, items: []))), 2)
        XCTAssertEqual(RichLayout.heading(1).size, 17)
        XCTAssertEqual(RichLayout.heading(2).line, 22)
        XCTAssertEqual(RichLayout.heading(3).line, 23.1)
    }

    func testFoldByHeight() {
        XCTAssertEqual(RichLayout.visibleHeight(shown: 10), 231, accuracy: 0.001)
        XCTAssertEqual(RichLayout.visibleHeight(shown: 30) - RichLayout.visibleHeight(shown: 10), 462, accuracy: 0.001)
        XCTAssertFalse(RichLayout.folds(height: 231 + 5.7, shown: 10), "a quarter line or less does not fold")
        XCTAssertTrue(RichLayout.folds(height: 231 + 5.8, shown: 10))
        // Plain lines × 23.1 fold where plain text does: 11 lines fold at 10, 10 do not.
        XCTAssertTrue(RichLayout.folds(height: 11 * 23.1, shown: 10))
        XCTAssertFalse(RichLayout.folds(height: 10 * 23.1, shown: 10))
    }

    func testInlineAttributes() {
        let ink = RichInk(text: .black, link: .pink, marker: .gray, codeBg: .yellow)
        let string = [run("a", .code), run("b", link: "https://example.com"), run("c", .strike), run("d", .underline)]
            .attributed(size: 14, ink: ink)
        let runs = Array(string.runs)
        XCTAssertEqual(runs.count, 4)
        typealias SwiftUIKeys = AttributeScopes.SwiftUIAttributes
        XCTAssertEqual(runs[0][SwiftUIKeys.BackgroundColorAttribute.self], Color.yellow)
        XCTAssertEqual(runs[1].link, URL(string: "https://example.com"))
        XCTAssertNotNil(runs[1][SwiftUIKeys.UnderlineStyleAttribute.self])
        XCTAssertEqual(runs[1][SwiftUIKeys.ForegroundColorAttribute.self], Color.pink)
        XCTAssertNotNil(runs[2][SwiftUIKeys.StrikethroughStyleAttribute.self])
        XCTAssertNotNil(runs[3][SwiftUIKeys.UnderlineStyleAttribute.self])
        XCTAssertNil(runs[3].link)
    }

    // MARK: Long-press menu

    private func text(sensitive: Bool = false) -> TransferMessage {
        var message = TransferMessage(id: "1", createdAt: Date(), senderDeviceID: "pc", senderName: "PC", offerID: nil, sourceBoot: nil,
                                      operationID: nil, itemID: nil, representationID: nil, originSenderID: nil, proxyDeviceID: nil,
                                      proxyName: nil, kind: .text, text: "x", contentID: nil, attachmentName: nil, byteSize: nil,
                                      uti: nil, state: nil, availability: "Ready", isMine: false, delivery: nil)
        message.sensitive = sensitive
        return message
    }

    func testMenuOffersCopyTextOnlyForRichText() {
        XCTAssertEqual(TransferLayout.menu(text(), rich: true, collapsedStack: false, revealed: false),
                       [.copy, .copyText, .select, .delete], "复制 first, then 复制文本")
        XCTAssertEqual(TransferLayout.menu(text(), rich: false, collapsedStack: false, revealed: false),
                       [.copy, .select, .delete], "plain text and HTML without formatting")
        XCTAssertEqual(TransferLayout.menu(text(sensitive: true), rich: true, collapsedStack: false, revealed: false),
                       [.copy, .copyText, .delete], "a hidden secret has no 选择")
    }
}
