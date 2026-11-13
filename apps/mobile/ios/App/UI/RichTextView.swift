import SwiftUI
import UIKit

// The rich text bubble's body (native-rewrite.md「富文本气泡（用户 2026-11-12 确认）」): the
// blocks of a RichDocument laid out with the bubble's own type ramp and colours. Block structure
// (paragraphs, headings, lists, quotes, code blocks, rules, tables) is drawn here; only inline
// formatting goes through `AttributedString` (RichText.swift).

/// A rich text bubble's body showing `shown` lines' worth of height (1 line = 23.1); a longer
/// one is clipped there with the last three lines fading, as plain long text (SPEC §1.2), and
/// folds only when more than a quarter line would be hidden. `height` reports the full laid-out
/// height. Links open on a tap: http / https in the default browser, mailto in Mail, tel in Phone.
struct RichTextBody: View {
    let document: RichDocument
    let shown: Int
    @Binding var height: CGFloat?
    /// 长按「选择」: the system selection (with formatting) replaces the drawn blocks.
    var selecting = false
    var onEndSelect: () -> Void = {}
    @Environment(\.oink) private var oink

    var body: some View {
        let full = height ?? 0
        let folded = full > 0 && RichLayout.folds(height: full, shown: shown)
        content
            .fixedSize(horizontal: false, vertical: true)
            .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { if height != $0 { height = $0 } }
            .frame(height: folded ? RichLayout.visibleHeight(shown: shown) : nil, alignment: .top)
            .clipped()
            .mask { FoldMask(line: RichLayout.line, strength: folded ? 1 : 0) }
            .environment(\.openURL, OpenURLAction { url in RichLinks.allows(url) ? .systemAction(url) : .discarded })
    }

    @ViewBuilder private var content: some View {
        if selecting {
            SelectableText(text: document.plainText, color: UIColor(oink.bubbleInk), lineHeight: 0, pitch: 0,
                           onEnd: onEndSelect, attributed: RichSelection.attributed(document, palette: oink))
                .frame(maxWidth: .infinity, alignment: .leading)
        } else {
            // Short content hugs it; content that wraps (or a wide table) takes the bubble's
            // whole width, like a wrapped plain text (TransferView.textBubble).
            ViewThatFits(in: .horizontal) {
                RichBlocks(blocks: document.blocks, ink: oink.bubbleInk)
                RichBlocks(blocks: document.blocks, ink: oink.bubbleInk).frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }
}

/// Metrics of the system fonts the blocks use (line boxes, for the pitch of each kind of line).
enum RichFonts {
    static func lineHeight(_ size: CGFloat, weight: UIFont.Weight = .regular, mono: Bool = false) -> CGFloat {
        (mono ? UIFont.monospacedSystemFont(ofSize: size, weight: weight) : UIFont.systemFont(ofSize: size, weight: weight)).lineHeight
    }
    /// White space a line of body text has above and below its glyph box at 23.1 pitch.
    static var bodyHalfLeading: CGFloat { (RichLayout.line - lineHeight(RichLayout.bodySize)) / 2 }
}

/// Blocks one under the other with collapsed margins (RichLayout.gap).
private struct RichBlocks: View {
    let blocks: [RichBlock]
    /// Text colour here (the bubble's ink; quote ink inside a quote).
    let ink: Color

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(Array(blocks.enumerated()), id: \.offset) { index, block in
                RichBlockView(block: block, ink: ink)
                    .padding(.top, index == 0 ? 0 : RichLayout.gap(blocks[index - 1], block))
            }
        }
    }
}

private struct RichBlockView: View {
    let block: RichBlock
    let ink: Color
    @Environment(\.oink) private var oink

    private var colors: RichInk { RichInk(text: ink, link: oink.richLink, marker: oink.richMarker, codeBg: oink.richCodeBg) }

    var body: some View {
        switch block {
        case .paragraph(let runs, _):
            RichLine(text: runs.attributed(size: RichLayout.bodySize, ink: colors), size: RichLayout.bodySize, pitch: RichLayout.line)
        case .blank:
            Color.clear.frame(width: 1, height: RichLayout.line)
        case .heading(let level, let runs):
            let type = RichLayout.heading(level)
            RichLine(text: runs.attributed(size: type.size, heading: true, ink: colors), size: type.size, weight: .semibold,
                     pitch: type.line)
                .accessibilityAddTraits(.isHeader)
        case .list(let list):
            RichListView(list: list, ink: ink)
        case .quote(let inner):
            // A 3 wide bar (radius 1.5, 3 short of the top and bottom), the text 10 from it.
            RichBlocks(blocks: inner, ink: oink.richQuoteInk)
                .padding(.leading, 13)
                .overlay(alignment: .leading) {
                    RoundedRectangle(cornerRadius: 1.5).fill(oink.richRule).frame(width: 3).padding(.vertical, 3)
                }
        case .code(let text):
            // Monospace 12.5 / 18.75 on the code tint, wrapped (no sideways scroll in a bubble).
            RichLine(text: AttributedString(text), size: RichLayout.monoSize, pitch: RichLayout.monoLine, mono: true)
                .foregroundStyle(ink)
                .padding(.vertical, 8).padding(.horizontal, 10)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(oink.richCodeBg, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        case .rule:
            Rectangle().fill(oink.richRule).frame(height: 1).frame(maxWidth: .infinity)
                .accessibilityHidden(true)
        case .table(let table):
            RichTableView(table: table, ink: ink)
        }
    }
}

/// One block of text at `pitch` per line: the line spacing and half of it above and below
/// make every line `pitch` high, as CSS line-height does (so block margins are the spec's).
private struct RichLine: View {
    let text: AttributedString
    let size: CGFloat
    var weight: UIFont.Weight = .regular
    let pitch: CGFloat
    var mono = false
    @Environment(\.oink) private var oink

    var body: some View {
        let leading = max(0, pitch - RichFonts.lineHeight(size, weight: weight, mono: mono))
        Text(text)
            .font(mono ? .system(size: size, design: .monospaced) : .system(size: size))
            .lineSpacing(leading)
            .padding(.vertical, leading / 2)
            .fixedSize(horizontal: false, vertical: true)
            // Links: the rich link colour (SwiftUI draws link runs in the tint).
            .tint(oink.richLink)
    }
}

/// A list: marker column 20 (18 for nested levels), items 2 apart; markers • ◦ ▪ by level,
/// ordered ones with tabular digits from `start`, in the marker colour.
private struct RichListView: View {
    let list: RichList
    let ink: Color
    @Environment(\.oink) private var oink

    private var column: CGFloat { list.depth == 1 ? 20 : 18 }

    private func marker(_ index: Int) -> String {
        if list.ordered { return "\(list.start + index)." }
        return ["•", "◦", "▪"][min(2, list.depth - 1)]
    }

    var body: some View {
        let leading = max(0, RichLayout.line - RichFonts.lineHeight(RichLayout.bodySize))
        VStack(alignment: .leading, spacing: 0) {
            ForEach(Array(list.items.enumerated()), id: \.offset) { index, item in
                HStack(alignment: .top, spacing: 0) {
                    Text(marker(index))
                        .font(.system(size: RichLayout.bodySize)).monospacedDigit()
                        .foregroundStyle(oink.richMarker)
                        .lineLimit(1).fixedSize()
                        .padding(.vertical, leading / 2)
                        .frame(width: column - 5, alignment: .trailing)
                        .padding(.trailing, 5)
                        .accessibilityHidden(!list.ordered)
                    if item.isEmpty {
                        Color.clear.frame(width: 1, height: RichLayout.line)
                    } else {
                        RichBlocks(blocks: item, ink: ink)
                    }
                }
                .padding(.top, index == 0 ? 0 : 2)
            }
        }
    }
}

/// A table (user 2026-11-12): a monospace block on the code tint with columns as wide as their
/// widest cell (measured, so full-width CJK lines up), 16 apart, cells left-aligned on one line;
/// a header row (the first row all `th`) bold over a rule. Wider than the bubble, it takes the
/// bubble's width and scrolls sideways inside it, without a scroll bar, the content fading 24 on
/// each side that has more. The scroll view sits in the conversation's vertical scroll view:
/// UIKit locks each drag to the axis it starts in (sideways scrolls the table, up and down the
/// list), a long press that has not moved still opens the message menu, and the position is not
/// kept (a row built again starts at the left).
private struct RichTableView: View {
    let table: RichTable
    let ink: Color
    @Environment(\.oink) private var oink
    /// Which sides have more to scroll to (the fades).
    @State private var more = Edges(leading: false, trailing: true)

    struct Edges: Equatable {
        var leading: Bool
        var trailing: Bool
    }

    var body: some View {
        ViewThatFits(in: .horizontal) {
            grid.padding(.vertical, 8).padding(.horizontal, 10)
            ScrollView(.horizontal, showsIndicators: false) {
                grid.padding(.vertical, 8).padding(.horizontal, 10)
            }
            .onScrollGeometryChange(for: Edges.self) { geometry in
                Edges(leading: geometry.contentOffset.x > 1,
                      trailing: geometry.contentOffset.x + geometry.containerSize.width < geometry.contentSize.width - 1)
            } action: { _, edges in more = edges }
            // Only the content fades; the tint behind it does not.
            .mask {
                HStack(spacing: 0) {
                    LinearGradient(colors: [.clear, .black], startPoint: .leading, endPoint: .trailing)
                        .frame(width: more.leading ? 24 : 0)
                    Color.black
                    LinearGradient(colors: [.black, .clear], startPoint: .leading, endPoint: .trailing)
                        .frame(width: more.trailing ? 24 : 0)
                }
            }
            .accessibilityIdentifier("transfer-rich-table")
        }
        .background(oink.richCodeBg, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
    }

    private var colors: RichInk { RichInk(text: ink, link: oink.richLink, marker: oink.richMarker, codeBg: oink.richCodeBg) }

    private var grid: some View {
        let header = table.header
        let columns = table.columns
        return Grid(alignment: .topLeading, horizontalSpacing: 16, verticalSpacing: 0) {
            ForEach(Array(table.rows.enumerated()), id: \.offset) { row, cells in
                GridRow {
                    ForEach(Array(cells.enumerated()), id: \.offset) { _, cell in
                        RichLine(text: cell.runs.attributed(size: RichLayout.monoSize, heading: cell.head, mono: true, ink: colors),
                                 size: RichLayout.monoSize, weight: cell.head ? .semibold : .regular, pitch: RichLayout.monoLine,
                                 mono: true)
                            .fixedSize()
                            .padding(.top, 1 + (header && row == 1 ? 5 : 0))
                            .padding(.bottom, 1 + (header && row == 0 ? 4 : 0))
                    }
                }
                if header && row == 0 {
                    GridRow {
                        Rectangle().fill(oink.richRule).frame(height: 1)
                            .gridCellColumns(columns)
                            .gridCellUnsizedAxes(.horizontal)
                    }
                }
            }
        }
    }
}

/// The rich text as an `NSAttributedString` for 选择: the system selection over it copies with
/// formatting (user 2026-11-12). Close to the drawn blocks, not the same: lists and quotes are
/// indents, a table is tab-aligned columns that wrap with the bubble instead of scrolling.
enum RichSelection {
    static func attributed(_ document: RichDocument, palette: OinkPalette) -> NSAttributedString {
        let out = NSMutableAttributedString()
        append(document.blocks, to: out, indent: 0, ink: UIColor(palette.bubbleInk), palette: palette)
        // No paragraph break after the last block.
        if out.string.hasSuffix("\n") { out.deleteCharacters(in: NSRange(location: out.length - 1, length: 1)) }
        return out
    }

    private static func style(pitch: CGFloat, font: UIFont, indent: CGFloat, head: CGFloat? = nil,
                              before: CGFloat = 0, after: CGFloat = 0, tabs: [CGFloat] = []) -> NSParagraphStyle {
        let style = NSMutableParagraphStyle()
        style.minimumLineHeight = font.lineHeight
        style.maximumLineHeight = font.lineHeight
        style.lineSpacing = max(0, pitch - font.lineHeight)
        style.firstLineHeadIndent = indent
        style.headIndent = head ?? indent
        style.paragraphSpacingBefore = before
        style.paragraphSpacing = after + max(0, pitch - font.lineHeight)
        style.tabStops = tabs.map { NSTextTab(textAlignment: .left, location: $0) }
        return style
    }

    private static func font(size: CGFloat, marks: RichMarks, heading: Bool, mono: Bool) -> UIFont {
        let weight: UIFont.Weight = heading || marks.contains(.bold) ? .semibold : .regular
        var font = mono || marks.contains(.code) ? UIFont.monospacedSystemFont(ofSize: RichLayout.monoSize, weight: weight)
            : UIFont.systemFont(ofSize: size, weight: weight)
        if marks.contains(.italic), let italic = font.fontDescriptor.withSymbolicTraits(font.fontDescriptor.symbolicTraits.union(.traitItalic)) {
            font = UIFont(descriptor: italic, size: font.pointSize)
        }
        return font
    }

    private static func runs(_ runs: [RichRun], size: CGFloat, heading: Bool = false, mono: Bool = false, ink: UIColor,
                             palette: OinkPalette, paragraph: NSParagraphStyle) -> NSAttributedString {
        let out = NSMutableAttributedString()
        for run in runs {
            var attributes: [NSAttributedString.Key: Any] = [
                .font: font(size: size, marks: run.marks, heading: heading, mono: mono),
                .foregroundColor: run.marks.contains(.alt) ? UIColor(palette.richMarker) : ink,
                .paragraphStyle: paragraph,
            ]
            if run.marks.contains(.code) && !mono { attributes[.backgroundColor] = UIColor(palette.richCodeBg) }
            if run.marks.contains(.strike) { attributes[.strikethroughStyle] = NSUnderlineStyle.single.rawValue }
            if run.marks.contains(.underline) || run.link != nil { attributes[.underlineStyle] = NSUnderlineStyle.single.rawValue }
            if let link = run.link {
                attributes[.link] = link
                attributes[.foregroundColor] = UIColor(palette.richLink)
            }
            out.append(NSAttributedString(string: run.text.replacingOccurrences(of: "\n", with: "\u{2028}"), attributes: attributes))
        }
        return out
    }

    private static func append(_ blocks: [RichBlock], to out: NSMutableAttributedString, indent: CGFloat, ink: UIColor,
                               palette: OinkPalette) {
        let body = UIFont.systemFont(ofSize: RichLayout.bodySize)
        for (index, block) in blocks.enumerated() {
            let before = index == 0 ? 0 : RichLayout.gap(blocks[index - 1], block) - RichLayout.margins(blocks[index - 1]).bottom
            let after = index == blocks.count - 1 ? 0 : RichLayout.margins(block).bottom
            switch block {
            case .paragraph(let r, _):
                out.append(runs(r, size: RichLayout.bodySize, ink: ink, palette: palette,
                                paragraph: style(pitch: RichLayout.line, font: body, indent: indent, before: before, after: after)))
            case .blank:
                out.append(NSAttributedString(string: "", attributes: [.font: body]))
            case .heading(let level, let r):
                let type = RichLayout.heading(level)
                let font = UIFont.systemFont(ofSize: type.size, weight: .semibold)
                out.append(runs(r, size: type.size, heading: true, ink: ink, palette: palette,
                                paragraph: style(pitch: type.line, font: font, indent: indent, before: before, after: after)))
            case .list(let list):
                let column: CGFloat = list.depth == 1 ? 20 : 18
                for (n, item) in list.items.enumerated() {
                    let marker = list.ordered ? "\(list.start + n)." : ["•", "◦", "▪"][min(2, list.depth - 1)]
                    let paragraph = style(pitch: RichLayout.line, font: body, indent: indent, head: indent + column,
                                          before: n == 0 ? before : 2, tabs: [indent + column])
                    out.append(NSAttributedString(string: marker + "\t", attributes: [
                        .font: body, .foregroundColor: UIColor(palette.richMarker), .paragraphStyle: paragraph,
                    ]))
                    // The item's first paragraph continues the marker's line; the rest are indented.
                    let content = NSMutableAttributedString()
                    append(item, to: content, indent: indent + column, ink: ink, palette: palette)
                    if content.length > 0 {
                        content.addAttribute(.paragraphStyle, value: paragraph,
                                             range: (content.string as NSString).paragraphRange(for: NSRange(location: 0, length: 0)))
                        out.append(content)
                    } else {
                        out.append(NSAttributedString(string: "\n", attributes: [.font: body, .paragraphStyle: paragraph]))
                    }
                }
                continue
            case .quote(let inner):
                let content = NSMutableAttributedString()
                append(inner, to: content, indent: indent + 13, ink: UIColor(palette.richQuoteInk), palette: palette)
                out.append(content)
                continue
            case .code(let text):
                let mono = UIFont.monospacedSystemFont(ofSize: RichLayout.monoSize, weight: .regular)
                out.append(NSAttributedString(string: text.replacingOccurrences(of: "\n", with: "\u{2028}"), attributes: [
                    .font: mono, .foregroundColor: ink, .backgroundColor: UIColor(palette.richCodeBg),
                    .paragraphStyle: style(pitch: RichLayout.monoLine, font: mono, indent: indent, before: before, after: after),
                ]))
            case .rule:
                continue
            case .table(let table):
                let mono = UIFont.monospacedSystemFont(ofSize: RichLayout.monoSize, weight: .regular)
                // Tab stops at each column's widest cell + 16.
                var widths = Array(repeating: CGFloat(0), count: table.columns)
                for row in table.rows {
                    for (c, cell) in row.enumerated() {
                        let text = NSAttributedString(string: cell.runs.map(\.text).joined(),
                                                      attributes: [.font: font(size: RichLayout.monoSize, marks: [], heading: cell.head, mono: true)])
                        widths[c] = max(widths[c], ceil(text.size().width))
                    }
                }
                var tabs: [CGFloat] = []
                var x = indent
                for width in widths.dropLast() { x += width + 16; tabs.append(x) }
                for (r, row) in table.rows.enumerated() {
                    let paragraph = style(pitch: RichLayout.monoLine, font: mono, indent: indent, before: r == 0 ? before : 0,
                                          after: r == table.rows.count - 1 ? after : 0, tabs: tabs)
                    for (c, cell) in row.enumerated() {
                        if c > 0 { out.append(NSAttributedString(string: "\t", attributes: [.font: mono, .paragraphStyle: paragraph])) }
                        out.append(runs(cell.runs, size: RichLayout.monoSize, heading: cell.head, mono: true, ink: ink, palette: palette,
                                        paragraph: paragraph))
                    }
                    out.append(NSAttributedString(string: "\n", attributes: [.font: mono, .paragraphStyle: paragraph]))
                }
                continue
            }
            out.append(NSAttributedString(string: "\n", attributes: [.font: body]))
        }
    }
}
