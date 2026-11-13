package com.hachiwii.oink

import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.text.LinkAnnotation
import com.github.takahirom.roborazzi.captureRoboImage
import com.hachiwii.oink.ui.*
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.ParameterizedRobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * 特权模式 第 3 步 (settings-and-pairing.md, 用户 2026-11-12 确认): the text leads with the
 * notification route and ends in an inline 「打开开发者选项」 link that opens Developer options; the
 * code field is labelled for split screen. Also renders the step into
 * build/outputs/roborazzi/{light,dark}/settings-privileged-step3.png.
 */
@RunWith(ParameterizedRobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class PrivilegedStepThreeTest(private val dark: Boolean) {
    companion object {
        @JvmStatic @ParameterizedRobolectricTestRunner.Parameters(name = "dark={0}")
        fun themes() = listOf(arrayOf<Any>(false), arrayOf<Any>(true))
    }

    @get:Rule val compose = createComposeRule()

    @Test fun stepThree() {
        val opened = mutableListOf<String>()
        compose.setContent {
            OinkTheme(OinkColors.of(dark, Accent.Pink)) {
                SettingsPage(UiInit(settingsStack = listOf("main", "privileged"), privStep = 3), Prefs(scheme = if (dark) "dark" else "light", privileged = "setup"), {},
                    AppModel(pairingPort = 37145), AppActions(openSystem = { opened += it }), onClose = {})
            }
        }
        compose.waitForIdle()
        compose.onNodeWithText("分屏时在这里输入").assertIsDisplayed()
        // The paragraph's own text node (unmerged: the node may also carry the link's own text entry).
        val text = compose.onNode(hasText("留在「设置」里", substring = true), useUnmergedTree = true)
        text.assertIsDisplayed()
        // The link: the paragraph's last words, a clickable link annotation that opens Developer options.
        val annotated = text.fetchSemanticsNode().config[SemanticsProperties.Text].first { it.text.startsWith("留在「设置」里") }
        org.junit.Assert.assertTrue(annotated.text, annotated.text.startsWith("留在「设置」里：在「无线调试」中点按「使用配对码配对设备」，然后在 Oink 的通知里点「输入配对码」，填入系统显示的六位配对码，部分手机可能需要长按通知显示按钮。切回 Oink 会让系统关掉配对窗口；分屏同时打开「设置」和 Oink 时，也可以填在下面。"))
        val links = annotated.getLinkAnnotations(0, annotated.length)
        assertEquals(1, links.size)
        assertEquals("打开开发者选项", annotated.text.substring(links[0].start, links[0].end).trimEnd('↗'))
        assertEquals(annotated.length, links[0].end)
        val link = links[0].item as LinkAnnotation.Clickable
        link.linkInteractionListener!!.onClick(link)
        assertEquals(listOf("developer"), opened)
        compose.onRoot().captureRoboImage("build/outputs/roborazzi/${if (dark) "dark" else "light"}/settings-privileged-step3.png")
    }
}
