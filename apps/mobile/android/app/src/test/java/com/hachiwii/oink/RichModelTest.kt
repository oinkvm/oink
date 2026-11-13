package com.hachiwii.oink

import com.hachiwii.oink.ui.*
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** Rich text bubbles (user 2026-11-12, native-rewrite.md 「富文本气泡」): the model parsed from the core's cleaned markup. */
class RichModelTest {
    private fun blocks(html: String) = parseRich(html).blocks
    private fun para(html: String) = (blocks(html).single() as RichBlock.Para).content
    private fun RichInline.marked(mark: RichMark) = spans.filter { it.mark == mark }.map { text.substring(it.start, it.end) }

    @Test fun inlineMarksMapToRanges() {
        val p = para("<p>今晚 <b>8 点</b>开会，<i>bring it</i> <u>u</u> <s>s</s> <code>npm run</code> <a href=\"https://x.test/a?b=1&amp;c=2\">link</a></p>")
        assertEquals("今晚 8 点开会，bring it u s npm run link", p.text)
        assertEquals(listOf("8 点"), p.marked(RichMark.Bold))
        assertEquals(listOf("bring it"), p.marked(RichMark.Italic))
        assertEquals(listOf("u"), p.marked(RichMark.Underline))
        assertEquals(listOf("s"), p.marked(RichMark.Strike))
        assertEquals(listOf("npm run"), p.marked(RichMark.Code))
        val link = p.spans.single { it.mark is RichMark.Link }
        assertEquals(RichMark.Link("https://x.test/a?b=1&c=2"), link.mark)
        assertEquals("link", p.text.substring(link.start, link.end))
    }

    @Test fun onlyWebMailAndPhoneLinksAreLinks() {
        assertTrue(para("<a href=\"mailto:a@b.c\">m</a> <a href=\"tel:+1\">t</a> <a href=\"HTTP://x\">h</a>").spans.all { it.mark is RichMark.Link })
        assertTrue(para("<a href=\"javascript:x()\">j</a> <a href=\"file:///etc\">f</a>").spans.isEmpty())
    }

    @Test fun italicLeavesChineseUpright() {
        val p = para("<i>中文 and English 混排</i>")
        val ranges = italicRanges(p).map { p.text.substring(it.first, it.last + 1) }
        assertEquals(listOf(" and English "), ranges)
    }

    @Test fun whitespaceCollapsesAndBreaksStay() {
        val p = para("<p>  a \n  b<br>c  <br></p>")
        assertEquals("a b\nc", p.text)
        assertEquals("1 < 2 & “q”", para("1 &lt; 2 &amp; &ldquo;q&rdquo;").text)
        assertEquals("😀 A", para("&#x1F600; &#65;").text)
    }

    @Test fun emptyParagraphsLeaveAtMostOneBlankLine() {
        val b = blocks("<p>a</p><p><br></p><p></p><p><br></p><p>b</p>")
        assertEquals(listOf("a", "", "b"), b.map { (it as RichBlock.Para).content.text })
    }

    @Test fun headingsListsAndQuotes() {
        val b = blocks("<h1>T</h1><h5>five</h5><ul><li>one<ul><li>two<ul><li>three<ul><li>four</li></ul></li></ul></li></ul></li></ul><ol start=\"3\"><li>x</li><li>y</li></ol><blockquote><h2>q</h2>said</blockquote>")
        assertEquals(RichBlock.Heading(1, RichInline("T")), b[0])
        assertEquals(3, (b[1] as RichBlock.Heading).level)
        val list = b[2] as RichBlock.ListBlock
        assertEquals(1, list.depth)
        val d2 = list.items[0][1] as RichBlock.ListBlock
        val d3 = d2.items[0][1] as RichBlock.ListBlock
        val d4 = d3.items[0][1] as RichBlock.ListBlock
        assertEquals(listOf(2, 3, 3), listOf(d2.depth, d3.depth, d4.depth))
        assertEquals(listOf("•", "◦", "▪"), listOf(listMarker(list, 0), listMarker(d2, 0), listMarker(d3, 0)))
        val ol = b[3] as RichBlock.ListBlock
        assertEquals(listOf("3.", "4."), listOf(listMarker(ol, 0), listMarker(ol, 1)))
        // A heading inside a quote is only bold text.
        val quote = b[4] as RichBlock.Quote
        val q = (quote.blocks[0] as RichBlock.Para).content
        assertEquals("q", q.text); assertEquals(listOf("q"), q.marked(RichMark.Bold))
    }

    @Test fun codeBlocksKeepTheirLinesRulesAndPictures() {
        val b = blocks("<pre>a  b\n  c\n</pre><hr><p>x<img alt=\"截图\">y<img alt=\"\"></p>")
        assertEquals(RichBlock.Code("a  b\n  c"), b[0])
        assertEquals(RichBlock.Rule, b[1])
        val p = (b[2] as RichBlock.Para).content
        assertEquals("x[图片：截图]y", p.text)
        assertEquals(listOf("[图片：截图]"), p.marked(RichMark.Alt))
    }

    @Test fun tablesKeepTheirCellsAndHeader() {
        val t = blocks("<table><tr><th>项目</th><th>负责人</th></tr><tr><td><b>Android</b> 包</td><td>Windows PC<br>Mac mini</td></tr></table>").single() as RichBlock.Table
        assertTrue(t.header)
        assertEquals(listOf("项目", "负责人"), t.rows[0].map { it.content.text })
        assertEquals("Windows PC\nMac mini", t.rows[1][1].content.text)
        assertEquals(listOf("Android"), t.rows[1][0].content.marked(RichMark.Bold))
        assertFalse((blocks("<table><tr><td>a</td><th>b</th></tr></table>").single() as RichBlock.Table).header)
    }

    @Test fun columnsAreAsWideAsTheirWidestCell() {
        val cols = tableColumns(listOf(listOf(40, 60, 30), listOf(80, 20), listOf(10, 70, 35)))
        assertEquals(listOf(80, 70, 35), cols)
        assertEquals(80 + 70 + 35 + 2 * 16, tableWidth(cols, 16))
        assertEquals(0, tableWidth(emptyList(), 16))
    }

    @Test fun marginsCollapseAndTheEndsHaveNone() {
        val b = listOf(RichBlock.Para(RichInline("a")), RichBlock.Heading(1, RichInline("h")), RichBlock.Para(RichInline("b")), RichBlock.Rule, RichBlock.Heading(3, RichInline("c")))
        // p→h1: max(6, 12); h1→p: max(4, 0) … rule: 8 both sides; then h3's 8.
        assertEquals(listOf(12f, 4f, 8f, 8f), richGaps(b))
    }

    @Test fun richTextFoldsByHeightWithAQuarterLineToSpare() {
        assertFalse(foldsAt(10f))
        assertFalse(foldsAt(10.25f))
        assertTrue(foldsAt(10.3f))
        // Plain text's whole lines give the old rule (more than 10 lines folds).
        assertFalse(foldsAt(10f)); assertTrue(foldsAt(11f))
    }
}
