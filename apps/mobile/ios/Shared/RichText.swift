import Foundation
import SwiftUI

// Rich text in a transfer text bubble (native-rewrite.md「富文本气泡（用户 2026-11-12 确认）」).
//
// The core hands over `display_html`: the source HTML (or RTF converted to HTML) cleaned to a
// fixed subset — `b i u s code pre a h1–h6 ul ol li blockquote hr table tr th td p div br`,
// `img` with only its alt, no attribute but `a href` (http / https / mailto / tel) and
// `ol start` (core rich_text.rs). It is parsed here into blocks and inline runs; the bubble lays
// the blocks out itself (RichTextView) and gives only the inline formatting to an
// `AttributedString`.
//
// Not `NSAttributedString(data:options:[.documentType: .html])`: that imports through WebKit
// (main thread only, slow on a list row, and a web engine that could try to load what the
// markup names), and its block styles (list markers, quotes, table cells) are the web's, not
// the bubble's. The cleaned subset is small and balanced, so a parser of its own fetches
// nothing by construction and runs anywhere.

/// Inline formatting of a run.
struct RichMarks: OptionSet, Hashable {
    let rawValue: Int
    static let bold = RichMarks(rawValue: 1 << 0)
    static let italic = RichMarks(rawValue: 1 << 1)
    static let underline = RichMarks(rawValue: 1 << 2)
    static let strike = RichMarks(rawValue: 1 << 3)
    /// Inline code (`code`): monospace on the code tint.
    static let code = RichMarks(rawValue: 1 << 4)
    /// A picture's alt text (「[图片：alt]」 in the marker colour).
    static let alt = RichMarks(rawValue: 1 << 5)
}

/// A piece of text with one formatting; "\n" is a line break (`<br>`).
struct RichRun: Equatable {
    var text: String
    var marks: RichMarks = []
    /// http, https, mailto or tel only (RichLinks.allows).
    var link: URL? = nil
}

struct RichList: Equatable {
    var ordered: Bool
    /// The first number of an ordered list (`ol start`).
    var start: Int = 1
    /// 1…3: deeper lists are laid out as the third level.
    var depth: Int
    var items: [[RichBlock]]
}

struct RichCell: Equatable {
    var head: Bool
    var runs: [RichRun]
}

struct RichTable: Equatable {
    var rows: [[RichCell]]
    /// The first row is all `th`: bold over a rule.
    var header: Bool { !(rows.first?.isEmpty ?? true) && rows.first!.allSatisfy(\.head) }
    var columns: Int { rows.map(\.count).max() ?? 0 }
}

indirect enum RichBlock: Equatable {
    enum Paragraph: Equatable {
        /// `<p>`: 6 below.
        case p
        /// Text straight in a list item, a quote or the document, and layout boxes (`div`):
        /// only a line break, no gap.
        case line
    }
    case paragraph([RichRun], Paragraph)
    /// An empty paragraph kept as one blank line (`<p><br></p>`); several in a row are one.
    case blank
    /// 1…3 (h4–h6 are 3). Inside a list or a quote a heading is a bold line instead.
    case heading(Int, [RichRun])
    case list(RichList)
    case quote([RichBlock])
    /// `pre`: kept as written, wrapped (no sideways scroll).
    case code(String)
    case rule
    case table(RichTable)
}

/// The bubble's rich text: blocks parsed from the cleaned markup.
struct RichDocument: Equatable {
    var blocks: [RichBlock]

    /// Nil when the markup holds nothing to show.
    static func parse(_ html: String) -> RichDocument? {
        let nodes = HTMLTree.parse(html)
        var flow = RichFlow()
        RichFlow.walk(nodes, RichFlow.Context(), &flow)
        flow.flush(.line)
        let blocks = RichFlow.collapsingBlanks(flow.blocks)
        return blocks.isEmpty ? nil : RichDocument(blocks: blocks)
    }

    /// The text without formatting (复制文本 when the source carried no plain text): list items
    /// with「• 」/「1. 」, table cells separated by tabs and rows by line breaks, pictures as
    /// their alt text.
    var plainText: String { Self.plain(blocks, depth: 0) }

    private static func plain(_ blocks: [RichBlock], depth: Int) -> String {
        blocks.compactMap { block -> String? in
            switch block {
            case .paragraph(let runs, _), .heading(_, let runs): return runs.map(\.text).joined()
            case .blank: return ""
            case .list(let list):
                return list.items.enumerated().map { index, item in
                    let marker = list.ordered ? "\(list.start + index). " : "• "
                    return String(repeating: "  ", count: max(0, list.depth - 1)) + marker + plain(item, depth: list.depth)
                }.joined(separator: "\n")
            case .quote(let inner): return plain(inner, depth: depth)
            case .code(let text): return text
            case .rule: return nil
            case .table(let table):
                return table.rows.map { $0.map { $0.runs.map(\.text).joined() }.joined(separator: "\t") }.joined(separator: "\n")
            }
        }.joined(separator: "\n")
    }
}

/// A message's rich text as the bubble and the long-press menu use it: the core's record and
/// its parsed blocks, built once per record (TransferModel.richContent). Compared by identity,
/// so a list row compares it for free.
final class RichContent {
    let rich: TransferRichText
    let document: RichDocument

    /// Nil when the cleaned markup is empty or shows nothing: the message is plain text then
    /// (no 复制文本, 复制 writes only the text; user 2026-11-12).
    init?(rich: TransferRichText) {
        guard !rich.displayHTML.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              let document = RichDocument.parse(rich.displayHTML) else { return nil }
        self.rich = rich
        self.document = document
    }
}

enum RichLinks {
    /// Only these open on a tap (user 2026-11-12): the browser, mail, phone. Anything else
    /// (`javascript:`, `file:`, `data:`, app schemes) stays plain text.
    static func allows(_ url: URL) -> Bool {
        guard let scheme = url.scheme?.lowercased() else { return false }
        return ["http", "https", "mailto", "tel"].contains(scheme)
    }

    static func url(_ href: String) -> URL? {
        let trimmed = href.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let url = URL(string: trimmed) ?? trimmed.addingPercentEncoding(withAllowedCharacters: .urlFragmentAllowed).flatMap(URL.init(string:)),
              allows(url) else { return nil }
        return url
    }
}

// MARK: - Layout numbers

/// Sizes of the rich bubble (spec table, iOS pt): body 14 / 23.1, headings 17 / 24, 15 / 22,
/// 14 / 23.1, monospace 12.5 / 18.75; block margins collapse like CSS (the larger of the gap
/// below one block and above the next).
enum RichLayout {
    /// One line of body text; the fold counts in these (1 line = 23.1).
    static let line: CGFloat = 23.1
    static let bodySize: CGFloat = 14
    static let monoSize: CGFloat = 12.5
    static let monoLine: CGFloat = 18.75

    /// Font size and line pitch of heading level 1…3.
    static func heading(_ level: Int) -> (size: CGFloat, line: CGFloat) {
        switch level {
        case 1: (17, 24)
        case 2: (15, 22)
        default: (14, 23.1)
        }
    }

    /// Margin above and below a block (`nested`: a list inside a list item).
    static func margins(_ block: RichBlock) -> (top: CGFloat, bottom: CGFloat) {
        switch block {
        case .paragraph(_, .p), .blank: (0, 6)
        case .paragraph(_, .line): (0, 0)
        case .heading(1, _): (12, 4)
        case .heading(2, _): (10, 2)
        case .heading: (8, 0)
        case .list(let list): list.depth > 1 ? (2, 0) : (0, 6)
        case .quote, .code, .table: (0, 6)
        case .rule: (8, 8)
        }
    }

    /// Space between two blocks in a row (collapsed margins). The first block's top and the
    /// last block's bottom margin are never laid out (the bubble's padding is the edge).
    static func gap(_ above: RichBlock, _ below: RichBlock) -> CGFloat {
        max(margins(above).bottom, margins(below).top)
    }

    /// Height a folded rich text shows: `shown` lines of 23.1 (10 → 231, then 462 more each time).
    static func visibleHeight(shown: Int) -> CGFloat { CGFloat(shown) * line }

    /// Folds only when more than a quarter line would be hidden.
    static func folds(height: CGFloat, shown: Int) -> Bool {
        height > visibleHeight(shown: shown) + line / 4
    }
}

// MARK: - Inline formatting

/// Colours of the inline formatting (the palette's bubble ink or quote ink, and the rich tokens).
struct RichInk {
    var text: Color
    var link: Color
    var marker: Color
    var codeBg: Color
}

extension Array where Element == RichRun {
    /// The runs as one `AttributedString` in the app's font at `size` (headings: semibold);
    /// the source's fonts, sizes and colours never get here. `mono`: a table cell.
    func attributed(size: CGFloat, heading: Bool = false, mono: Bool = false, ink: RichInk) -> AttributedString {
        var out = AttributedString()
        for run in self {
            var piece = AttributedString(run.text)
            let bold = heading || run.marks.contains(.bold)
            let code = mono || run.marks.contains(.code)
            var font: Font = code
                ? .system(size: RichLayout.monoSize, weight: bold ? .semibold : .regular, design: .monospaced)
                : .system(size: size, weight: bold ? .semibold : .regular)
            if run.marks.contains(.italic) { font = font.italic() }
            piece.font = font
            piece.foregroundColor = run.marks.contains(.alt) ? ink.marker : run.link != nil ? ink.link : ink.text
            if run.marks.contains(.code) && !mono { piece.backgroundColor = ink.codeBg }
            if run.marks.contains(.strike) { piece.strikethroughStyle = Text.LineStyle.single }
            if let link = run.link {
                piece.link = link
                piece.underlineStyle = Text.LineStyle(pattern: .solid, color: ink.link)
            } else if run.marks.contains(.underline) {
                piece.underlineStyle = Text.LineStyle.single
            }
            out += piece
        }
        return out
    }
}

// MARK: - Blocks from the markup

private struct RichFlow {
    struct Context {
        var marks: RichMarks = []
        var link: URL?
        /// List depth (0: not in a list).
        var depth = 0
        var quote = false
        var table = false
        func adding(_ mark: RichMarks) -> Context { var c = self; c.marks.formUnion(mark); return c }
    }

    var blocks: [RichBlock] = []
    var line = InlineBuilder()

    /// The pending inline text becomes a paragraph of `kind`.
    mutating func flush(_ kind: RichBlock.Paragraph) {
        switch line.finish() {
        case .empty: break
        case .blank: blocks.append(.blank)
        case .runs(let runs): blocks.append(.paragraph(runs, kind))
        }
        line = InlineBuilder()
    }

    static func blocks(_ nodes: [HTMLTree.Node], _ ctx: Context) -> [RichBlock] {
        var flow = RichFlow()
        walk(nodes, ctx, &flow)
        flow.flush(.line)
        return collapsingBlanks(flow.blocks)
    }

    /// Text of nodes laid out as one run list (a heading, a table cell): blocks inside become
    /// line breaks.
    static func runs(_ nodes: [HTMLTree.Node], _ ctx: Context) -> [RichRun] {
        var out: [RichRun] = []
        for block in blocks(nodes, ctx) {
            let runs: [RichRun]
            switch block {
            case .paragraph(let r, _), .heading(_, let r): runs = r
            case .blank: runs = []
            case .code(let text): runs = [RichRun(text: text, marks: .code)]
            case .list, .quote, .table: runs = [RichRun(text: RichDocument(blocks: [block]).plainText)]
            case .rule: continue
            }
            if !out.isEmpty { out.append(RichRun(text: "\n")) }
            out += runs
        }
        return out
    }

    /// Several blank paragraphs in a row are one.
    static func collapsingBlanks(_ blocks: [RichBlock]) -> [RichBlock] {
        var out: [RichBlock] = []
        for block in blocks where !(block == .blank && out.last == .blank) { out.append(block) }
        return out
    }

    static func walk(_ nodes: [HTMLTree.Node], _ ctx: Context, _ flow: inout RichFlow) {
        for node in nodes {
            switch node {
            case .text(let text):
                flow.line.text(text, marks: ctx.marks, link: ctx.link)
            case .element(let tag, let attributes, let children):
                element(tag, attributes, children, ctx, &flow)
            }
        }
    }

    private static func element(_ tag: String, _ attributes: [String: String], _ children: [HTMLTree.Node],
                                _ ctx: Context, _ flow: inout RichFlow) {
        switch tag {
        case "b", "strong": walk(children, ctx.adding(.bold), &flow)
        case "i", "em", "cite", "var", "dfn": walk(children, ctx.adding(.italic), &flow)
        case "u", "ins": walk(children, ctx.adding(.underline), &flow)
        case "s", "strike", "del": walk(children, ctx.adding(.strike), &flow)
        case "code", "kbd", "samp", "tt": walk(children, ctx.adding(.code), &flow)
        case "a":
            var inner = ctx
            if let link = attributes["href"].flatMap(RichLinks.url) { inner.link = link }
            walk(children, inner, &flow)
        case "br": flow.line.lineBreak()
        case "img":
            let alt = (attributes["alt"] ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            if !alt.isEmpty { flow.line.text(AppText.pick("[图片：\(alt)]", "[Image: \(alt)]"), marks: ctx.marks.union(.alt), link: nil, raw: true) }
        case "p":
            flow.flush(.line)
            var inner = RichFlow()
            walk(children, ctx, &inner)
            inner.flush(.p)
            flow.blocks += inner.blocks
        case "h1", "h2", "h3", "h4", "h5", "h6":
            flow.flush(.line)
            if ctx.depth > 0 || ctx.quote {
                // Inside a list or a quote a heading is only bold.
                var inner = RichFlow()
                walk(children, ctx.adding(.bold), &inner)
                inner.flush(.line)
                flow.blocks += inner.blocks
            } else {
                let text = runs(children, ctx)
                if !text.isEmpty { flow.blocks.append(.heading(min(3, Int(String(tag.last!)) ?? 3), text)) }
            }
        case "ul", "ol":
            flow.flush(.line)
            let depth = ctx.depth + 1
            var inner = ctx
            inner.depth = depth
            var items: [[RichBlock]] = []
            for child in children {
                if case .element("li", _, let content) = child {
                    items.append(blocks(content, inner))
                } else if case .text(let text) = child, text.allSatisfy(\.isHTMLSpace) {
                    continue
                } else {
                    // A list straight inside a list (or stray text) belongs to the item before.
                    let more = blocks([child], inner)
                    if items.isEmpty { items.append(more) } else { items[items.count - 1] += more }
                }
            }
            let start = tag == "ol" ? attributes["start"].flatMap { Int($0) } ?? 1 : 1
            if !items.isEmpty {
                flow.blocks.append(.list(RichList(ordered: tag == "ol", start: start, depth: min(3, depth), items: items)))
            }
        case "blockquote":
            flow.flush(.line)
            var inner = ctx
            inner.quote = true
            let content = blocks(children, inner)
            if !content.isEmpty { flow.blocks.append(.quote(content)) }
        case "pre":
            flow.flush(.line)
            var text = HTMLTree.text(children)
            if text.hasPrefix("\n") { text.removeFirst() }
            while text.hasSuffix("\n") { text.removeLast() }
            if !text.isEmpty { flow.blocks.append(.code(text)) }
        case "hr":
            flow.flush(.line)
            flow.blocks.append(.rule)
        case "table" where !ctx.table:
            flow.flush(.line)
            var inner = ctx
            inner.table = true
            let rows = tableRows(children).map { cells in
                cells.map { cell in RichCell(head: cell.head, runs: runs(cell.children, inner)) }
            }.filter { !$0.isEmpty }
            if !rows.isEmpty { flow.blocks.append(.table(RichTable(rows: rows))) }
        case "div", "li", "tr", "td", "th", "table",
             "section", "article", "header", "footer", "figure", "figcaption", "main", "aside", "nav", "address",
             "center", "dl", "dt", "dd", "details", "summary":
            // Layout boxes (and table parts outside a table): only line breaks.
            flow.flush(.line)
            var inner = RichFlow()
            walk(children, ctx, &inner)
            inner.flush(.line)
            flow.blocks += inner.blocks
        default:
            // Any other element: its text (the core already removed these).
            walk(children, ctx, &flow)
        }
    }

    /// The rows of a table (wrappers such as `tbody` looked through, nested tables not).
    private static func tableRows(_ nodes: [HTMLTree.Node]) -> [[(head: Bool, children: [HTMLTree.Node])]] {
        var rows: [[(head: Bool, children: [HTMLTree.Node])]] = []
        for node in nodes {
            guard case .element(let tag, _, let children) = node else { continue }
            switch tag {
            case "tr": rows.append(cells(children))
            case "table": continue
            default: rows += tableRows(children)
            }
        }
        return rows
    }

    private static func cells(_ nodes: [HTMLTree.Node]) -> [(head: Bool, children: [HTMLTree.Node])] {
        var out: [(head: Bool, children: [HTMLTree.Node])] = []
        for node in nodes {
            guard case .element(let tag, _, let children) = node else { continue }
            switch tag {
            case "td", "th": out.append((tag == "th", children))
            case "tr", "table": continue
            default: out += cells(children)
            }
        }
        return out
    }
}

/// Inline text of one paragraph with HTML's white-space rules: runs of white space are one
/// space, none at the start or end of a line; `<br>` breaks the line, and one at the very end
/// of a paragraph adds nothing (as in a browser).
private struct InlineBuilder {
    enum Result { case empty, blank, runs([RichRun]) }

    private var runs: [RichRun] = []
    /// The last thing added was a space or a line start: more white space is dropped.
    private var spaced = true

    mutating func text(_ text: String, marks: RichMarks, link: URL?, raw: Bool = false) {
        for character in text {
            if !raw && character.isHTMLSpace {
                guard !spaced else { continue }
                append(" ", marks, link)
                spaced = true
            } else {
                append(character, marks, link)
                spaced = false
            }
        }
    }

    mutating func lineBreak() {
        trimTrailingSpace()
        append("\n", [], nil)
        spaced = true
    }

    private mutating func append(_ character: Character, _ marks: RichMarks, _ link: URL?) {
        if let last = runs.last, last.marks == marks, last.link == link {
            runs[runs.count - 1].text.append(character)
        } else {
            runs.append(RichRun(text: String(character), marks: marks, link: link))
        }
    }

    private mutating func trimTrailingSpace() {
        while let last = runs.last, last.text.last == " " {
            runs[runs.count - 1].text.removeLast()
            if runs[runs.count - 1].text.isEmpty { runs.removeLast() }
        }
    }

    mutating func finish() -> Result {
        trimTrailingSpace()
        guard !runs.isEmpty else { return .empty }
        // A <br> closing the paragraph adds no line.
        if runs[runs.count - 1].text.last == "\n" {
            runs[runs.count - 1].text.removeLast()
            if runs[runs.count - 1].text.isEmpty { runs.removeLast() }
            trimTrailingSpace()
        }
        if runs.allSatisfy({ $0.text.allSatisfy { $0 == "\n" } }) { return .blank }
        return .runs(runs.filter { !$0.text.isEmpty })
    }
}

private extension Character {
    /// HTML white space (not the no-break space).
    var isHTMLSpace: Bool { self == " " || self == "\t" || self == "\n" || self == "\r" || self == "\u{0C}" || self == "\r\n" }
}

// MARK: - Markup

/// A small HTML reader for the cleaned subset: tags, attributes, text and character
/// references; comments and declarations are skipped. Unbalanced input is closed sensibly
/// (an end tag closes up to its open element; a stray one is ignored).
enum HTMLTree {
    indirect enum Node: Equatable {
        case text(String)
        case element(String, [String: String], [Node])
    }

    private static let void: Set<String> = ["br", "hr", "img", "input", "meta", "link", "wbr", "area", "base", "col",
                                            "param", "source", "track", "embed"]

    static func parse(_ html: String) -> [Node] {
        var stack: [(tag: String, attributes: [String: String], children: [Node])] = [("", [:], [])]
        func add(_ node: Node) { stack[stack.count - 1].children.append(node) }
        func close(to index: Int) {
            while stack.count > index {
                let open = stack.removeLast()
                add(.element(open.tag, open.attributes, open.children))
            }
        }
        let chars = Array(html)
        var i = 0
        var text = ""
        func flushText() {
            if !text.isEmpty { add(.text(decode(text))); text = "" }
        }
        while i < chars.count {
            guard chars[i] == "<" else { text.append(chars[i]); i += 1; continue }
            // A comment, declaration or processing instruction.
            if i + 3 < chars.count, chars[i + 1] == "!", chars[i + 2] == "-", chars[i + 3] == "-" {
                flushText()
                var j = i + 4
                while j + 2 < chars.count, !(chars[j] == "-" && chars[j + 1] == "-" && chars[j + 2] == ">") { j += 1 }
                i = min(chars.count, j + 3)
                continue
            }
            let closing = i + 1 < chars.count && chars[i + 1] == "/"
            var j = i + (closing ? 2 : 1)
            let nameStart = j
            while j < chars.count, chars[j].isLetter || chars[j].isNumber || chars[j] == "-" || chars[j] == ":" { j += 1 }
            let declaration = i + 1 < chars.count && (chars[i + 1] == "!" || chars[i + 1] == "?")
            guard declaration || (j > nameStart && chars[nameStart].isASCII && chars[nameStart].isLetter) else {
                // Not a tag: a literal '<'.
                text.append("<"); i += 1; continue
            }
            // The tag's end, outside quoted values.
            var quote: Character?
            var end = j
            while end < chars.count {
                let c = chars[end]
                if let q = quote { if c == q { quote = nil } } else if c == "\"" || c == "'" { quote = c } else if c == ">" { break }
                end += 1
            }
            guard end < chars.count else { break }
            flushText()
            defer { i = end + 1 }
            if declaration { continue }
            let name = String(chars[nameStart..<j]).lowercased()
            if closing {
                if let index = stack.lastIndex(where: { $0.tag == name }), index > 0 { close(to: index) }
                continue
            }
            let inner = String(chars[j..<end])
            let attributes = parseAttributes(inner)
            if void.contains(name) || inner.trimmingCharacters(in: .whitespaces).hasSuffix("/") {
                add(.element(name, attributes, []))
            } else {
                stack.append((name, attributes, []))
            }
        }
        flushText()
        close(to: 1)
        return stack[0].children
    }

    /// Text content, line breaks for `<br>` (for `pre`).
    static func text(_ nodes: [Node]) -> String {
        nodes.map { node in
            switch node {
            case .text(let text): text
            case .element("br", _, _): "\n"
            case .element(_, _, let children): Self.text(children)
            }
        }.joined()
    }

    private static func parseAttributes(_ source: String) -> [String: String] {
        var out: [String: String] = [:]
        let chars = Array(source)
        var i = 0
        while i < chars.count {
            while i < chars.count, chars[i].isWhitespace || chars[i] == "/" { i += 1 }
            let start = i
            while i < chars.count, !chars[i].isWhitespace, chars[i] != "=", chars[i] != "/" { i += 1 }
            guard i > start else { break }
            let name = String(chars[start..<i]).lowercased()
            while i < chars.count, chars[i].isWhitespace { i += 1 }
            var value = ""
            if i < chars.count, chars[i] == "=" {
                i += 1
                while i < chars.count, chars[i].isWhitespace { i += 1 }
                if i < chars.count, chars[i] == "\"" || chars[i] == "'" {
                    let q = chars[i]
                    i += 1
                    let valueStart = i
                    while i < chars.count, chars[i] != q { i += 1 }
                    value = String(chars[valueStart..<min(i, chars.count)])
                    i += 1
                } else {
                    let valueStart = i
                    while i < chars.count, !chars[i].isWhitespace { i += 1 }
                    value = String(chars[valueStart..<i])
                }
            }
            if out[name] == nil { out[name] = decode(value) }
        }
        return out
    }

    private static let named: [String: String] = [
        "amp": "&", "lt": "<", "gt": ">", "quot": "\"", "apos": "'", "nbsp": "\u{00A0}", "ensp": "\u{2002}",
        "emsp": "\u{2003}", "thinsp": "\u{2009}", "zwnj": "\u{200C}", "zwj": "\u{200D}", "shy": "\u{00AD}",
        "copy": "©", "reg": "®", "trade": "™", "hellip": "…", "mdash": "—", "ndash": "–", "lsquo": "‘",
        "rsquo": "’", "ldquo": "“", "rdquo": "”", "laquo": "«", "raquo": "»", "bull": "•", "middot": "·",
        "times": "×", "divide": "÷", "deg": "°", "plusmn": "±", "sect": "§", "para": "¶", "euro": "€",
        "pound": "£", "yen": "¥", "cent": "¢", "larr": "←", "rarr": "→", "uarr": "↑", "darr": "↓", "harr": "↔",
        "le": "≤", "ge": "≥", "ne": "≠", "minus": "−", "check": "✓", "iexcl": "¡", "iquest": "¿",
    ]

    /// Character references: numeric ones and the common named ones; anything else stays as written.
    static func decode(_ text: String) -> String {
        guard text.contains("&") else { return text }
        var out = ""
        var rest = Substring(text)
        while let amp = rest.firstIndex(of: "&") {
            out += rest[..<amp]
            let after = rest[rest.index(after: amp)...]
            if let semi = after.prefix(32).firstIndex(of: ";") {
                let body = after[..<semi]
                var decoded: String?
                if body.hasPrefix("#x") || body.hasPrefix("#X") {
                    decoded = UInt32(body.dropFirst(2), radix: 16).flatMap(Unicode.Scalar.init).map { String(Character($0)) }
                } else if body.hasPrefix("#") {
                    decoded = UInt32(body.dropFirst()).flatMap(Unicode.Scalar.init).map { String(Character($0)) }
                } else {
                    decoded = named[String(body)]
                }
                if let decoded {
                    out += decoded
                    rest = after[after.index(after: semi)...]
                    continue
                }
            }
            out += "&"
            rest = after
        }
        out += rest
        return out
    }
}
