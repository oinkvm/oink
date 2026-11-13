package com.hachiwii.oink.ui

import androidx.compose.foundation.ScrollState
import androidx.compose.foundation.background
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicText
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.BlendMode
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.CompositingStrategy
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.layout.IntrinsicMeasurable
import androidx.compose.ui.layout.IntrinsicMeasureScope
import androidx.compose.ui.layout.Layout
import androidx.compose.ui.layout.Measurable
import androidx.compose.ui.layout.MeasurePolicy
import androidx.compose.ui.layout.MeasureResult
import androidx.compose.ui.layout.MeasureScope
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.LinkAnnotation
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextLinkStyles
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.unit.Constraints
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/*
 * Rich text in a transfer text bubble (user 2026-11-12, native-rewrite.md 「富文本气泡」): the views. The blocks
 * (paragraphs, headings, lists with their marker column, quotes with their bar, code blocks, rules, tables)
 * are laid out here; only inline formatting is styled text. The bubble keeps its own type, colours and
 * width; the source's colours, fonts, sizes and alignment never get here (the core dropped them).
 */

/** The text styles of rich text: body 14 / 23.1 like plain text, headings 17 / 15 / 14 at 600, monospace 12.5 / 18.75. */
private fun bodyStyle(ink: Color) = textStyle(14f, ink, lineHeight = RICH_LINE_DP)
private fun headingStyle(level: Int, ink: Color) = when (level) {
    1 -> textStyle(17f, ink, weight = 600, lineHeight = 24f)
    2 -> textStyle(15f, ink, weight = 600, lineHeight = 22f)
    else -> textStyle(14f, ink, weight = 600, lineHeight = RICH_LINE_DP)
}
private fun monoStyle(ink: Color) = textStyle(12.5f, ink, lineHeight = 18.75f).copy(fontFamily = FontFamily.Monospace)

/** The whole rich body of a bubble, [ink] its text colour. */
@Composable
internal fun RichContent(doc: RichDoc, ink: Color, modifier: Modifier = Modifier) = RichBlocks(doc.blocks, ink, modifier)

/**
 * Blocks in one column (margins per [richGaps]). As wide as the widest block wants (within the bubble's
 * width), and every block that wide: a code block's or table's tint spans the bubble like a CSS block.
 */
@Composable
private fun RichBlocks(blocks: List<RichBlock>, ink: Color, modifier: Modifier = Modifier, inItem: Boolean = false) {
    val gaps = remember(blocks, inItem) { richGaps(blocks).mapIndexed { i, g -> if (inItem && blocks[i + 1] is RichBlock.ListBlock) 2f else g } }
    Layout(content = { blocks.forEach { RichBlockView(it, ink) } }, modifier = modifier) { measurables, constraints ->
        val natural = measurables.maxOfOrNull { it.maxIntrinsicWidth(Constraints.Infinity) } ?: 0
        val w = natural.coerceIn(constraints.minWidth, constraints.maxWidth)
        val placeables = measurables.map { it.measure(Constraints(minWidth = w, maxWidth = w)) }
        val gapPx = gaps.map { it.dp.roundToPx() }
        val h = placeables.sumOf { it.height } + gapPx.sum()
        layout(w, h) {
            var y = 0
            placeables.forEachIndexed { i, p -> p.place(0, y); y += p.height + (gapPx.getOrNull(i) ?: 0) }
        }
    }
}

@Composable
private fun RichBlockView(block: RichBlock, ink: Color) {
    val c = Oink.colors
    when (block) {
        is RichBlock.Para -> RichTextLine(block.content, bodyStyle(ink))
        is RichBlock.Heading -> RichTextLine(block.content, headingStyle(block.level, ink))
        is RichBlock.ListBlock -> RichList(block, ink)
        // Bar 3 wide (radius 1.5, 3 in from the top and bottom), the text 10 from it in the quote colour.
        is RichBlock.Quote -> Box(Modifier.drawBehind {
            val w = 3.dp.toPx(); val inset = 3.dp.toPx()
            drawRoundRect(c.richRule, Offset(0f, inset), Size(w, (size.height - 2 * inset).coerceAtLeast(0f)), CornerRadius(1.5.dp.toPx()))
        }.padding(start = 13.dp)) { RichBlocks(block.blocks, c.richQuoteInk) }
        // Code block: wraps, never scrolls sideways (only tables do).
        is RichBlock.Code -> Box(Modifier.clip(RoundedCornerShape(8.dp)).background(c.richCodeBg).padding(horizontal = 10.dp, vertical = 8.dp)) {
            BasicText(block.text, style = monoStyle(ink))
        }
        RichBlock.Rule -> Box(Modifier.fillMaxWidth().height(1.dp).background(c.richRule))
        is RichBlock.Table -> RichTable(block, ink)
    }
}

/** One run of inline content (a paragraph, a heading, a cell). */
@Composable
private fun RichTextLine(content: RichInline, style: TextStyle, softWrap: Boolean = true) =
    BasicText(richAnnotated(content), style = style, softWrap = softWrap)

/**
 * Inline formatting as styled text: bold 600, italic (never on CJK), underline / strikethrough in the
 * text colour, inline code monospace 12.5 on the code tint, alt text in the marker colour, and links in
 * the link colour with an underline that open on tap through the platform's URI handler (browser, mail,
 * phone; only http / https / mailto / tel ever get here).
 */
@Composable
internal fun richAnnotated(content: RichInline): AnnotatedString {
    val c = Oink.colors
    return remember(content, c) {
        buildAnnotatedString {
            append(content.text)
            // Underline and strikethrough per segment: one style each, combined where they overlap.
            val decorated = content.spans.filter { it.mark == RichMark.Underline || it.mark == RichMark.Strike }
            val cuts = decorated.flatMap { listOf(it.start, it.end) }.toSortedSet().toList()
            cuts.zipWithNext().forEach { (from, to) ->
                val under = decorated.any { it.mark == RichMark.Underline && it.start <= from && it.end >= to }
                val strike = decorated.any { it.mark == RichMark.Strike && it.start <= from && it.end >= to }
                val decoration = when {
                    under && strike -> TextDecoration.combine(listOf(TextDecoration.Underline, TextDecoration.LineThrough))
                    under -> TextDecoration.Underline
                    strike -> TextDecoration.LineThrough
                    else -> null
                }
                if (decoration != null) addStyle(SpanStyle(textDecoration = decoration), from, to)
            }
            content.spans.forEach { s ->
                when (val m = s.mark) {
                    RichMark.Bold -> addStyle(SpanStyle(fontWeight = FontWeight.SemiBold), s.start, s.end)
                    RichMark.Code -> addStyle(SpanStyle(fontFamily = FontFamily.Monospace, fontSize = 12.5.sp, background = c.richCodeBg), s.start, s.end)
                    RichMark.Alt -> addStyle(SpanStyle(color = c.richMarker), s.start, s.end)
                    is RichMark.Link -> addLink(LinkAnnotation.Url(m.url, TextLinkStyles(SpanStyle(color = c.richLink, textDecoration = TextDecoration.Underline))), s.start, s.end)
                    RichMark.Italic, RichMark.Underline, RichMark.Strike -> {}
                }
            }
            italicRanges(content).forEach { addStyle(SpanStyle(fontStyle = FontStyle.Italic), it.first, it.last + 1) }
        }
    }
}

/**
 * A list: a 20 marker column (18 for nested levels) with the marker at its end, items 2 apart; markers
 * • / ◦ / ▪ by depth, or 1. 2. … from the list's start in tabular figures, in the marker colour.
 */
@Composable
private fun RichList(list: RichBlock.ListBlock, ink: Color) {
    val c = Oink.colors
    val column = if (list.depth == 1) 20.dp else 18.dp
    val markerStyle = bodyStyle(c.richMarker).copy(fontFeatureSettings = "tnum", textAlign = TextAlign.End)
    Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
        list.items.forEachIndexed { i, item ->
            Row {
                // The marker ends 5 before the item's text; a wider one ("10.") widens its column.
                Box(Modifier.widthIn(min = column)) {
                    BasicText(listMarker(list, i), Modifier.align(Alignment.TopEnd).padding(end = 5.dp), style = markerStyle, softWrap = false)
                }
                RichBlocks(item, ink, Modifier.weight(1f), inItem = true)
            }
        }
    }
}

/** The marker of item [index] of [list]. */
internal fun listMarker(list: RichBlock.ListBlock, index: Int): String =
    if (list.ordered) "${list.start + index}." else when (list.depth) { 1 -> "•"; 2 -> "◦"; else -> "▪" }

// ---------- Tables ----------

/**
 * A table (user 2026-11-12): one monospace block (12.5 / 18.75) on the code tint, radius 8, padding 8 / 10.
 * Columns as wide as their widest cell (measured, so CJK lines up), 16 apart, everything left-aligned, cells
 * on one line (a cell's own <br> still breaks); the header row (first row all th) 600 over a 1 rule (4
 * above, 5 below). Wider than the bubble: the block takes the bubble's width and scrolls sideways inside
 * it, no scroll bar, the content (not the tint) fading over 24 on each side that has more. The scroll
 * position is not kept. Gestures: horizontalScroll only takes a drag that passes the touch slop sideways,
 * so a mostly vertical drag scrolls the message list; a drag that scrolls the table cancels the bubble's
 * long press; a tap does nothing but what links in it do.
 */
@Composable
private fun RichTable(table: RichBlock.Table, ink: Color) {
    val c = Oink.colors
    val scroll = rememberScrollState()
    val mono = monoStyle(ink)
    Box(Modifier.clip(RoundedCornerShape(8.dp)).background(c.richCodeBg)) {
        Box(Modifier.edgeFade(scroll).horizontalScroll(scroll).padding(horizontal = 10.dp, vertical = 8.dp)) {
            val counts = table.rows.map { it.size }
            Layout(content = {
                table.rows.forEach { row -> row.forEach { cell -> RichTextLine(cell.content, if (cell.head) mono.copy(fontWeight = FontWeight.SemiBold) else mono, softWrap = false) } }
                if (table.header) Box(Modifier.background(c.richRule))
            }, measurePolicy = remember(counts, table.header) { TableMeasure(counts, table.header) })
        }
    }
}

/** 24 of fade on each side of a sideways scroller that has more content there (the content only, not what is behind). */
private fun Modifier.edgeFade(scroll: ScrollState): Modifier = graphicsLayer { compositingStrategy = CompositingStrategy.Offscreen }
    .drawWithContent {
        drawContent()
        val f = 24.dp.toPx()
        if (scroll.value > 0) drawRect(Brush.horizontalGradient(0f to Color.Transparent, 1f to Color.Black, startX = 0f, endX = f),
            size = Size(f, size.height), blendMode = BlendMode.DstIn)
        if (scroll.value < scroll.maxValue) drawRect(Brush.horizontalGradient(0f to Color.Black, 1f to Color.Transparent, startX = size.width - f, endX = size.width),
            topLeft = Offset(size.width - f, 0f), size = Size(f, size.height), blendMode = BlendMode.DstIn)
    }

/** Column gap and cell paddings of a table (CSS: cells 1 above and below; the header row 4 below, over a 1 rule, the next row 5 above). */
private val TABLE_GAP: Dp = 16.dp

/**
 * Lays out a table's cells ([counts] per row, row by row) in measured columns; with [header], the last
 * child is the rule under the first row. Intrinsic widths are the table's own, so the bubble can size to a
 * narrow table and the scroller knows a wide one.
 */
private class TableMeasure(private val counts: List<Int>, private val header: Boolean) : MeasurePolicy {
    private fun rows(cells: List<Int>): List<List<Int>> {
        var at = 0
        return counts.map { n -> cells.subList(at, at + n).also { at += n } }
    }

    private fun IntrinsicMeasureScope.width(measurables: List<IntrinsicMeasurable>): Int {
        val cells = measurables.take(counts.sum()).map { it.maxIntrinsicWidth(Constraints.Infinity) }
        return tableWidth(tableColumns(rows(cells)), TABLE_GAP.roundToPx())
    }

    private fun IntrinsicMeasureScope.height(measurables: List<IntrinsicMeasurable>): Int {
        val heights = rows(measurables.take(counts.sum()).map { it.maxIntrinsicHeight(Constraints.Infinity) })
        return heights.mapIndexed { r, row -> (row.maxOrNull() ?: 0) + pads(r).let { (a, b) -> a + b } }.sum()
    }

    /** Above and below row [r] (px), the rule included under a header row. */
    private fun androidx.compose.ui.unit.Density.pads(r: Int): Pair<Int, Int> {
        val one = 1.dp.roundToPx()
        return when {
            header && r == 0 -> one to 4.dp.roundToPx() + one
            header && r == 1 -> 5.dp.roundToPx() to one
            else -> one to one
        }
    }

    override fun MeasureScope.measure(measurables: List<Measurable>, constraints: Constraints): MeasureResult {
        val total = counts.sum()
        val cells = measurables.take(total).map { it.measure(Constraints()) }
        val grid = rows(cells.indices.toList()).map { row -> row.map { cells[it] } }
        val columns = tableColumns(grid.map { row -> row.map { it.width } })
        val gap = TABLE_GAP.roundToPx()
        val width = tableWidth(columns, gap)
        val heights = grid.map { row -> row.maxOfOrNull { it.height } ?: 0 }
        val tops = ArrayList<Int>()
        var y = 0
        heights.forEachIndexed { r, h -> val (above, below) = pads(r); tops += y + above; y += above + h + below }
        val rule = if (header) measurables.getOrNull(total)?.measure(Constraints.fixed(width, 1.dp.roundToPx())) else null
        return layout(width, y) {
            grid.forEachIndexed { r, row ->
                var x = 0
                row.forEachIndexed { col, p -> p.place(x, tops[r]); x += columns[col] + gap }
            }
            rule?.place(0, tops[0] + heights[0] + 4.dp.roundToPx())
        }
    }

    override fun IntrinsicMeasureScope.maxIntrinsicWidth(measurables: List<IntrinsicMeasurable>, height: Int) = width(measurables)
    override fun IntrinsicMeasureScope.minIntrinsicWidth(measurables: List<IntrinsicMeasurable>, height: Int) = width(measurables)
    override fun IntrinsicMeasureScope.maxIntrinsicHeight(measurables: List<IntrinsicMeasurable>, width: Int) = height(measurables)
    override fun IntrinsicMeasureScope.minIntrinsicHeight(measurables: List<IntrinsicMeasurable>, width: Int) = height(measurables)
}
