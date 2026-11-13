package com.hachiwii.oink

import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.getUnclippedBoundsInRoot
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.LinkAnnotation
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextDecoration
import com.hachiwii.oink.ui.*
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * Rich text bubbles (user 2026-11-12): the styled text built from the model, the table's measured
 * columns, and the long-press menu's 复制 / 复制文本 with the calls they make.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], qualifiers = "zh-rCN-w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class RichTextUiTest {
    @get:Rule val compose = createComposeRule()

    private val peers = listOf(PeerUi("windowsPc", "Windows PC", "windows", latency = 4.0))
    private fun text(id: Long, plain: String, rich: String? = null, self: Boolean = false) =
        MessageUi(id, self, if (self) "self" else "windowsPc", if (self) "Phone" else "Windows PC", if (self) "android" else "windows", true, "text",
            text = plain, copyable = true, sent = true, richHtml = rich)

    @Test fun inlineMarksBecomeStylesAndLinks() {
        var built: AnnotatedString? = null
        val inline = (parseRich("<b>粗</b> <i>it 斜</i> <u>u<s>us</s></u> <a href=\"https://example.com\">ex</a>").blocks.single() as RichBlock.Para).content
        compose.setContent { OinkTheme(OinkColors.of(false, Accent.Pink)) { built = richAnnotated(inline) } }
        compose.waitForIdle()
        val s = built!!
        fun styled(range: String) = s.spanStyles.filter { s.text.substring(it.start, it.end) == range }.map { it.item }
        assertTrue(styled("粗").any { it.fontWeight == FontWeight.SemiBold })
        // Italic on the Latin part only.
        assertTrue(styled("it ").any { it.fontStyle == FontStyle.Italic })
        assertTrue(s.spanStyles.none { it.item.fontStyle == FontStyle.Italic && s.text.substring(it.start, it.end).contains('斜') })
        // Underline alone, then underline + strikethrough where they overlap.
        assertTrue(styled("u").any { it.textDecoration == TextDecoration.Underline })
        assertTrue(styled("us").any { it.textDecoration?.contains(TextDecoration.LineThrough) == true && it.textDecoration?.contains(TextDecoration.Underline) == true })
        val link = s.getLinkAnnotations(0, s.length).single()
        assertEquals("https://example.com", (link.item as LinkAnnotation.Url).url)
        assertEquals("ex", s.text.substring(link.start, link.end))
    }

    private fun page(messages: List<MessageUi>, actions: AppActions = AppActions(), menu: Long? = null) {
        compose.setContent {
            OinkApp(AppModel(peers = peers, messages = messages), UiInit(tab = Tab.Transfer, transferMessageMenu = menu, freeze = true), Prefs(scheme = "light"), {}, actions, false)
        }
        compose.waitForIdle()
    }

    @Test fun tableColumnsLineUp() {
        page(listOf(text(1, "表", "<table><tr><th>项目</th><th>负责人</th></tr><tr><td>Android 包</td><td>Bob</td></tr><tr><td>发版说明</td><td>Alex</td></tr></table>")))
        // Each cell's own text node (the bubble merges its texts into one node for the long press).
        fun cell(text: String) = compose.onNodeWithText(text, useUnmergedTree = true).getUnclippedBoundsInRoot()
        val head = cell("负责人")
        val bob = cell("Bob")
        val alex = cell("Alex")
        assertEquals(head.left.value, bob.left.value, .5f)
        assertEquals(head.left.value, alex.left.value, .5f)
        // The second column starts 16 after the widest first-column cell.
        val first = listOf("项目", "Android 包", "发版说明").maxOf { cell(it).right.value }
        assertEquals(first + 16f, head.left.value, .5f)
    }

    @Test fun richMessagesOfferCopyTextAfterCopy() {
        val calls = ArrayList<String>()
        val actions = AppActions(copyMessage = { calls += "copy ${it.id}"; true }, copyMessageText = { calls += "text ${it.id}"; true })
        page(listOf(text(1, "粗体", "<b>粗体</b>")), actions, menu = 1)
        val copy = compose.onNodeWithText("复制").getUnclippedBoundsInRoot()
        val copyText = compose.onNodeWithText("复制文本").getUnclippedBoundsInRoot()
        assertTrue("复制 first: $copy / $copyText", copy.bottom <= copyText.top)
        compose.onNodeWithText("复制文本").performClick()
        compose.waitForIdle()
        assertEquals(listOf("text 1"), calls)
    }

    @Test fun copyOnARichMessageCopiesRichText() {
        val calls = ArrayList<String>()
        page(listOf(text(1, "粗体", "<b>粗体</b>")), AppActions(copyMessage = { calls += "copy ${it.id}"; true }, copyMessageText = { calls += "text"; true }), menu = 1)
        compose.onNodeWithText("复制").performClick()
        compose.waitForIdle()
        assertEquals(listOf("copy 1"), calls)
    }

    @Test fun plainMessagesHaveNoCopyText() {
        page(listOf(text(1, "只是文字")), menu = 1)
        compose.onNodeWithText("复制").assertExists()
        compose.onAllNodesWithText("复制文本").assertCountEquals(0)
    }
}
