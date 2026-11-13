package com.hachiwii.oink.ui

/*
 * Rich text in a transfer text bubble (user 2026-11-12, native-rewrite.md 「富文本气泡」): the model.
 *
 * The input is the core's cleaned markup (`MobileRichText.displayHtml`, oink_mobile_core::rich_text): only
 * `b i u s code pre a(href) h1–h6 ul ol(start) li blockquote hr table tr th td p div br img(alt)`, lower
 * case, balanced, attributes quoted, text escaped. That closed set is parsed here directly (no
 * HtmlCompat.fromHtml: its block spans — bullets, quotes — are not used, and a plain parser keeps the model
 * testable on the JVM). Blocks are laid out by the page (RichText.kt); only inline formatting becomes
 * styled text.
 */

/** Inline formatting of a range of a block's text. */
internal sealed interface RichMark {
    data object Bold : RichMark
    /** Never on CJK characters: they stay upright (no synthetic italic). */
    data object Italic : RichMark
    data object Underline : RichMark
    data object Strike : RichMark
    /** Inline code: monospace 12.5 on the code tint. */
    data object Code : RichMark
    /** A picture's alt text, in the marker colour. */
    data object Alt : RichMark
    /** http / https / mailto / tel only; opens on tap. */
    data class Link(val url: String) : RichMark
}

internal data class RichSpan(val start: Int, val end: Int, val mark: RichMark)

/** A run of inline content: its text (with `\n` for `<br>`) and the formatted ranges. */
internal data class RichInline(val text: String, val spans: List<RichSpan> = emptyList()) {
    val blank get() = text.isBlank()
}

internal sealed interface RichBlock {
    /** A paragraph (also `div` text and loose text); an empty one is a blank line. */
    data class Para(val content: RichInline) : RichBlock
    /** h1–h6 outside lists and quotes; [level] 1–3 (h4–h6 are 3). */
    data class Heading(val level: Int, val content: RichInline) : RichBlock
    /** [depth] 1–3 (deeper lists are 3); each item is its blocks. */
    data class ListBlock(val ordered: Boolean, val start: Int, val depth: Int, val items: List<List<RichBlock>>) : RichBlock
    data class Quote(val blocks: List<RichBlock>) : RichBlock
    data class Code(val text: String) : RichBlock
    data object Rule : RichBlock
    /** Cells per row; [header]: the first row is all `th` (bold over a rule). */
    data class Table(val rows: List<List<RichCell>>, val header: Boolean) : RichBlock
}

internal data class RichCell(val head: Boolean, val content: RichInline)

/** The parsed markup of one message. Empty when nothing visible is left. */
internal data class RichDoc(val blocks: List<RichBlock>)

// ---------- Parsing ----------

private sealed interface RichNode
private data class RichTextNode(val text: String) : RichNode
private class RichElement(val tag: String, val attrs: Map<String, String>, val children: MutableList<RichNode> = mutableListOf()) : RichNode

private val VOID_TAGS = setOf("br", "hr", "img")
private val SAFE_LINK = Regex("^(https?:|mailto:|tel:)", RegexOption.IGNORE_CASE)

/** Parses [html] (the core's cleaned markup) into blocks. */
internal fun parseRich(html: String): RichDoc {
    val root = RichElement("#root", emptyMap())
    val stack = ArrayDeque<RichElement>().apply { addLast(root) }
    var i = 0
    val text = StringBuilder()
    fun flushText() { if (text.isNotEmpty()) { stack.last().children += RichTextNode(decodeEntities(text.toString())); text.clear() } }
    while (i < html.length) {
        val ch = html[i]
        if (ch != '<') { text.append(ch); i++; continue }
        val end = html.indexOf('>', i)
        if (end < 0) { text.append(html, i, html.length); break }
        val raw = html.substring(i + 1, end)
        i = end + 1
        if (raw.startsWith("!") || raw.startsWith("?")) continue
        flushText()
        if (raw.startsWith("/")) {
            val name = raw.substring(1).trim().lowercase()
            // Close up to the matching element (the core balances its markup; this only guards).
            val at = stack.indexOfLast { it.tag == name }
            if (at > 0) while (stack.size > at) stack.removeLast()
            continue
        }
        val name = raw.takeWhile { !it.isWhitespace() && it != '/' }.lowercase()
        if (name.isEmpty()) continue
        val element = RichElement(name, attributes(raw.substring(name.length)))
        stack.last().children += element
        if (name !in VOID_TAGS && !raw.trimEnd().endsWith("/")) stack.addLast(element)
    }
    flushText()
    return RichDoc(trimBlanks(RichBuilder().blocks(root.children, RichContext())))
}

private val BLANK = RichBlock.Para(RichInline(""))
private fun RichBlock.isBlank() = this is RichBlock.Para && content.text.isEmpty()

/** Blank lines at the start and end of a column of blocks show nothing: dropped. */
private fun trimBlanks(blocks: List<RichBlock>): List<RichBlock> = blocks.dropWhile { it.isBlank() }.dropLastWhile { it.isBlank() }

/** Adds [block]; a blank line right after another blank line is dropped (at most one in a row). */
private fun MutableList<RichBlock>.addBlock(block: RichBlock) {
    if (block.isBlank() && lastOrNull()?.isBlank() == true) return
    add(block)
}

private val ATTRIBUTE = Regex("([a-zA-Z_:][-a-zA-Z0-9_:.]*)\\s*=\\s*(\"([^\"]*)\"|'([^']*)'|([^\\s\"'>]+))")

private fun attributes(raw: String): Map<String, String> = ATTRIBUTE.findAll(raw).associate { m ->
    m.groupValues[1].lowercase() to decodeEntities(m.groups[3]?.value ?: m.groups[4]?.value ?: m.groupValues[5])
}

private val NAMED = mapOf("amp" to "&", "lt" to "<", "gt" to ">", "quot" to "\"", "apos" to "'", "nbsp" to "\u00A0",
    "copy" to "©", "reg" to "®", "trade" to "™", "hellip" to "…", "mdash" to "—", "ndash" to "–", "lsquo" to "‘", "rsquo" to "’",
    "ldquo" to "“", "rdquo" to "”", "bull" to "•", "middot" to "·", "times" to "×", "divide" to "÷", "deg" to "°", "euro" to "€",
    "pound" to "£", "yen" to "¥", "cent" to "¢", "sect" to "§", "para" to "¶", "laquo" to "«", "raquo" to "»", "shy" to "\u00AD",
    "ensp" to "\u2002", "emsp" to "\u2003", "thinsp" to "\u2009", "zwj" to "\u200D", "zwnj" to "\u200C")

/** HTML character references; an unknown one stays as written. */
internal fun decodeEntities(s: String): String {
    if ('&' !in s) return s
    val out = StringBuilder(s.length)
    var i = 0
    while (i < s.length) {
        val c = s[i]
        val end = if (c == '&') s.indexOf(';', i) else -1
        if (end in (i + 2)..(i + 33)) {
            val ref = s.substring(i + 1, end)
            val decoded = when {
                ref.startsWith("#x") || ref.startsWith("#X") -> ref.substring(2).toIntOrNull(16)
                ref.startsWith("#") -> ref.substring(1).toIntOrNull()
                else -> null
            }?.takeIf { it in 1..0x10FFFF }?.let { String(Character.toChars(it)) } ?: NAMED[ref]
            if (decoded != null) { out.append(decoded); i = end + 1; continue }
        }
        out.append(c); i++
    }
    return out.toString()
}

private data class RichContext(val depth: Int = 0, val quote: Boolean = false, val marks: List<RichMark> = emptyList())

/** Inline content being collected for the current block, with its open marks. */
private class RichInlineBuilder {
    val text = StringBuilder()
    val spans = ArrayList<RichSpan>()
    fun append(s: String, marks: List<RichMark>, pre: Boolean = false) {
        var t = if (pre) s else s.replace(WHITESPACE, " ")
        // Collapsed space at a line start, or after a space, goes.
        if (!pre && t.startsWith(" ") && (text.isEmpty() || text.last() == ' ' || text.last() == '\n')) t = t.substring(1)
        if (t.isEmpty()) return
        val start = text.length
        text.append(t)
        marks.forEach { spans += RichSpan(start, text.length, it) }
    }
    fun lineBreak() {
        // A space before a <br> is not content.
        while (text.isNotEmpty() && text.last() == ' ') text.setLength(text.length - 1)
        text.append('\n')
    }
    fun build(): RichInline {
        var end = text.length
        while (end > 0 && text[end - 1] == ' ') end--
        // A <br> that ends a block makes no line of its own.
        if (end > 0 && text[end - 1] == '\n') end--
        val t = text.substring(0, end)
        return RichInline(t, mergeSpans(spans.mapNotNull { s -> if (s.start >= end) null else s.copy(end = minOf(s.end, end)) }))
    }
    val empty get() = text.isEmpty()
}

private val WHITESPACE = Regex("[ \\t\\r\\n\\f]+")
private val BLOCK_TAGS = setOf("p", "div", "h1", "h2", "h3", "h4", "h5", "h6", "ul", "ol", "li", "blockquote", "pre", "hr", "table")

/** Adjacent ranges with the same mark become one. */
private fun mergeSpans(spans: List<RichSpan>): List<RichSpan> {
    val out = ArrayList<RichSpan>()
    for (s in spans.sortedWith(compareBy({ it.mark.toString() }, { it.start }))) {
        val last = out.lastOrNull()
        if (last != null && last.mark == s.mark && last.end >= s.start) out[out.size - 1] = last.copy(end = maxOf(last.end, s.end))
        else if (s.end > s.start) out += s
    }
    return out.sortedWith(compareBy({ it.start }, { it.end }))
}

private class RichBuilder {
    fun blocks(nodes: List<RichNode>, ctx: RichContext): List<RichBlock> {
        val out = ArrayList<RichBlock>()
        var inline = RichInlineBuilder()
        fun flush() {
            if (inline.empty) return
            val content = inline.build()
            inline = RichInlineBuilder()
            out.addBlock(if (content.blank) BLANK else RichBlock.Para(content))
        }
        fun walk(n: RichNode, marks: List<RichMark>) {
            when (n) {
                is RichTextNode -> inline.append(n.text, marks)
                is RichElement -> when {
                    n.tag == "br" -> inline.lineBreak()
                    n.tag == "img" -> n.attrs["alt"]?.trim()?.takeIf { it.isNotEmpty() }?.let { inline.append("[图片：$it]", marks + RichMark.Alt) }
                    n.tag in BLOCK_TAGS -> { flush(); block(n, ctx.copy(marks = marks)).forEach { out.addBlock(it) } }
                    else -> {
                        val mark = inlineMark(n)
                        val inner = if (mark != null) marks + mark else marks
                        n.children.forEach { walk(it, inner) }
                    }
                }
            }
        }
        nodes.forEach { walk(it, ctx.marks) }
        flush()
        return out
    }

    private fun inlineMark(e: RichElement): RichMark? = when (e.tag) {
        "b" -> RichMark.Bold
        "i" -> RichMark.Italic
        "u" -> RichMark.Underline
        "s" -> RichMark.Strike
        "code" -> RichMark.Code
        "a" -> e.attrs["href"]?.trim()?.takeIf { SAFE_LINK.containsMatchIn(it) }?.let { RichMark.Link(it) }
        else -> null
    }

    private fun inlineOf(nodes: List<RichNode>, marks: List<RichMark>): RichInline {
        val b = RichInlineBuilder()
        fun walk(n: RichNode, m: List<RichMark>) {
            when (n) {
                is RichTextNode -> b.append(n.text, m)
                is RichElement -> when (n.tag) {
                    "br" -> b.lineBreak()
                    "img" -> n.attrs["alt"]?.trim()?.takeIf { it.isNotEmpty() }?.let { b.append("[图片：$it]", m + RichMark.Alt) }
                    else -> {
                        val mark = inlineMark(n)
                        n.children.forEach { walk(it, if (mark != null) m + mark else m) }
                        // A block inside inline-only content (a table cell, a heading) still breaks the line.
                        if (n.tag in BLOCK_TAGS && !b.empty) b.lineBreak()
                    }
                }
            }
        }
        nodes.forEach { walk(it, marks) }
        return b.build()
    }

    private fun block(e: RichElement, ctx: RichContext): List<RichBlock> = when (e.tag) {
        "p", "div" -> blocks(e.children, ctx)
        "h1", "h2", "h3", "h4", "h5", "h6" -> {
            // Inside a list or a quote a heading is only bold text.
            if (ctx.depth > 0 || ctx.quote) blocks(e.children, ctx.copy(marks = ctx.marks + RichMark.Bold))
            else inlineOf(e.children, ctx.marks).let { if (it.blank) emptyList() else listOf(RichBlock.Heading(minOf(3, e.tag[1] - '0'), it)) }
        }
        "ul", "ol" -> {
            val depth = minOf(3, ctx.depth + 1)
            val inner = ctx.copy(depth = depth)
            val items = e.children.mapNotNull { child ->
                when {
                    child is RichElement && child.tag == "li" -> trimBlanks(blocks(child.children, inner))
                    child is RichTextNode && child.text.isBlank() -> null
                    // Loose content in a list: an item of its own.
                    else -> trimBlanks(blocks(listOf(child), inner)).takeIf { it.isNotEmpty() }
                }
            }
            if (items.isEmpty()) emptyList()
            else listOf(RichBlock.ListBlock(e.tag == "ol", e.attrs["start"]?.toIntOrNull() ?: 1, depth, items))
        }
        // An item outside a list is a plain block.
        "li" -> blocks(e.children, ctx)
        "blockquote" -> trimBlanks(blocks(e.children, ctx.copy(quote = true))).let { if (it.isEmpty()) emptyList() else listOf(RichBlock.Quote(it)) }
        "pre" -> {
            val code = preText(e).trimEnd('\n')
            if (code.isEmpty()) emptyList() else listOf(RichBlock.Code(code))
        }
        "hr" -> listOf(RichBlock.Rule)
        "table" -> table(e, ctx)
        else -> blocks(e.children, ctx)
    }

    /** A code block's text as written (line breaks and spaces kept; inline formatting inside is dropped). */
    private fun preText(e: RichElement): String = buildString {
        fun walk(n: RichNode) {
            when (n) {
                is RichTextNode -> append(n.text)
                is RichElement -> if (n.tag == "br") append('\n') else n.children.forEach(::walk)
            }
        }
        e.children.forEach(::walk)
    }

    private fun table(e: RichElement, ctx: RichContext): List<RichBlock> {
        val rows = e.children.filterIsInstance<RichElement>().filter { it.tag == "tr" }.map { tr ->
            tr.children.filterIsInstance<RichElement>().filter { it.tag == "td" || it.tag == "th" }.map { cell ->
                val head = cell.tag == "th"
                RichCell(head, inlineOf(cell.children, ctx.marks))
            }
        }.filter { it.isNotEmpty() }
        if (rows.isEmpty()) return emptyList()
        return listOf(RichBlock.Table(rows, header = rows.first().all { it.head }))
    }
}

// ---------- Spacing ----------

/** Space above and below a block in dp (CSS margins; adjacent ones collapse to the larger). */
internal fun richMargins(block: RichBlock): Pair<Float, Float> = when (block) {
    is RichBlock.Heading -> when (block.level) { 1 -> 12f to 4f; 2 -> 10f to 2f; else -> 8f to 0f }
    RichBlock.Rule -> 8f to 8f
    else -> 0f to 6f
}

/**
 * The gaps around [blocks] laid out in one column: the first block's top and the last block's bottom
 * margin are 0, and between two blocks the larger of the upper one's bottom and the lower one's top.
 */
internal fun richGaps(blocks: List<RichBlock>): List<Float> =
    (1 until blocks.size).map { i -> maxOf(richMargins(blocks[i - 1]).second, richMargins(blocks[i]).first) }

// ---------- Folding ----------

/** One "line" of a rich bubble for folding: the body line height (14 / 23.1). */
internal const val RICH_LINE_DP = 23.1f

/**
 * Long text folds when it is taller than [TEXT_FOLD_LINES] lines by more than a quarter line (user
 * 2026-11-12: rich text is counted by height, 1 line = 23.1; plain text's whole lines give the same).
 */
internal fun foldsAt(lines: Float): Boolean = lines > TEXT_FOLD_LINES + .25f

// ---------- Italic without CJK ----------

/** Whether [c] is CJK (ideographs, kana, hangul, full-width forms, CJK punctuation): kept upright under italic. */
internal fun isCjk(c: Char): Boolean {
    val block = Character.UnicodeBlock.of(c) ?: return false
    return block == Character.UnicodeBlock.CJK_UNIFIED_IDEOGRAPHS || block == Character.UnicodeBlock.CJK_UNIFIED_IDEOGRAPHS_EXTENSION_A ||
        block == Character.UnicodeBlock.CJK_COMPATIBILITY_IDEOGRAPHS || block == Character.UnicodeBlock.CJK_SYMBOLS_AND_PUNCTUATION ||
        block == Character.UnicodeBlock.HIRAGANA || block == Character.UnicodeBlock.KATAKANA ||
        block == Character.UnicodeBlock.HANGUL_SYLLABLES || block == Character.UnicodeBlock.HANGUL_JAMO ||
        block == Character.UnicodeBlock.HALFWIDTH_AND_FULLWIDTH_FORMS || block == Character.UnicodeBlock.BOPOMOFO ||
        Character.isSurrogate(c)
}

/** The italic ranges of [inline] with CJK characters cut out (no synthetic slant on Chinese, user 2026-11-12). */
internal fun italicRanges(inline: RichInline): List<IntRange> {
    val out = ArrayList<IntRange>()
    inline.spans.filter { it.mark == RichMark.Italic }.forEach { s ->
        var from = -1
        for (i in s.start until s.end) {
            val upright = isCjk(inline.text[i])
            if (!upright && from < 0) from = i
            if (upright && from >= 0) { out += from until i; from = -1 }
        }
        if (from >= 0) out += from until s.end
    }
    return out
}

// ---------- Table columns ----------

/**
 * Column widths of a table from its cells' measured widths ([cellWidths] by row; rows may be shorter):
 * each column as wide as its widest cell; with [gap] between columns, the table's width.
 */
internal fun tableColumns(cellWidths: List<List<Int>>): List<Int> {
    val n = cellWidths.maxOfOrNull { it.size } ?: 0
    return (0 until n).map { col -> cellWidths.maxOf { it.getOrNull(col) ?: 0 } }
}

internal fun tableWidth(columns: List<Int>, gap: Int): Int = if (columns.isEmpty()) 0 else columns.sum() + gap * (columns.size - 1)
