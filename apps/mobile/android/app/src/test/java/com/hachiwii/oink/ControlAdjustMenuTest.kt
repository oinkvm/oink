package com.hachiwii.oink

import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.semantics.ProgressBarRangeInfo
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.test.SemanticsMatcher
import androidx.compose.ui.test.assert
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.isRoot
import androidx.compose.ui.test.click
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithContentDescription
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performSemanticsAction
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.unit.dp
import com.hachiwii.oink.control.*
import com.hachiwii.oink.ui.*
import androidx.compose.ui.test.getUnclippedBoundsInRoot
import androidx.compose.ui.test.isOff
import androidx.compose.ui.test.isOn
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * 调节 menu (docs/control-page.md §2.1, 用户 2026-11-11 / 2026-11-12): the 指针速度 slider is always shown and
 * follows touches; an option row opens a second menu over the first, and picking an option
 * closes only the second menu and updates the row; a tap outside closes the second menu first.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class ControlAdjustMenuTest {
    @get:Rule val compose = createComposeRule()
    private val pc = "a".repeat(64)

    private val toggled = ArrayList<String>()
    private fun openAdjust(speakerOn: Boolean = false): FakeControl {
        val fake = FakeControl(ControlState(focusDevice = pc, selectedDevice = pc, targets = listOf(ControlTarget(pc, "Windows PC", "windows", true))))
        val model = AppModel(selfName = "Phone", peers = listOf(PeerUi(pc, "Windows PC", "windows", latency = 4.0)), speakerOn = speakerOn)
        val actions = AppActions(control = fake, toggleSpeaker = { toggled += "speaker" }, toggleMic = { toggled += "mic" })
        compose.setContent { OinkApp(model, UiInit(tab = Tab.Control, freeze = true), Prefs(scheme = "light"), {}, actions, false) }
        compose.waitForIdle()
        compose.onNodeWithContentDescription("调节").performClick()
        compose.waitForIdle()
        return fake
    }

    private fun speedIs(value: Int) = SemanticsMatcher.expectValue(SemanticsProperties.ProgressBarRangeInfo, ProgressBarRangeInfo(value.toFloat(), 1f..10f, 8))

    @Test
    fun sliderIsAlwaysShownAndFollowsTouches() {
        val fake = openAdjust()
        val speed = compose.onNodeWithContentDescription("指针速度")
        // Default: the Mac's default tracking speed, step 4 (user 2026-11-12).
        speed.assertIsDisplayed().assert(speedIs(4))
        compose.onNodeWithText("慢").assertIsDisplayed()
        compose.onNodeWithText("快").assertIsDisplayed()
        // The node is inside the row padding: the track is 12 in on each side, the slider strip 24–60 from the top.
        speed.performTouchInput { down(Offset(width - 12.dp.toPx(), 42.dp.toPx())); up() }
        compose.waitForIdle()
        speed.assert(speedIs(10))
        speed.performTouchInput {
            val y = 42.dp.toPx()
            down(Offset(width - 12.dp.toPx(), y)); moveTo(Offset(width / 2f, y)); moveTo(Offset(0f, y)); up()
        }
        compose.waitForIdle()
        speed.assert(speedIs(1))
        // Screen readers: an ordinary adjustable slider.
        speed.performSemanticsAction(SemanticsActions.SetProgress) { it(7f) }
        compose.waitForIdle()
        speed.assert(speedIs(7))
        // The new speed goes to the computer.
        assertTrue(fake.sent.toString(), fake.sent.contains("settings 7"))
        // No hold and no expand: tapping the name row changes nothing and keeps the slider.
        speed.performTouchInput { down(Offset(width / 2f, 10.dp.toPx())); up() }
        compose.waitForIdle()
        speed.assert(speedIs(7))
        compose.onNodeWithText("慢").assertIsDisplayed()
    }

    @Test
    fun optionRowOpensASecondMenuAndPickingKeepsTheFirstOpen() {
        openAdjust()
        compose.onNode(hasText("单指划动").and(hasText("移动指针"))).performClick()
        compose.waitForIdle()
        // Second menu over the first: its options, while the first menu is still there.
        compose.onNodeWithText("滚动").assertIsDisplayed()
        compose.onAllNodesWithText("移动指针").assertCountEquals(2)
        compose.onNodeWithContentDescription("指针速度").assertIsDisplayed()
        compose.onNodeWithText("滚动").performClick()
        compose.waitForIdle()
        // Second menu closed; the row shows the choice; the first menu stays open.
        compose.onAllNodesWithText("移动指针").assertCountEquals(0)
        compose.onNode(hasText("单指划动").and(hasText("滚动"))).assertIsDisplayed()
        compose.onNodeWithContentDescription("指针速度").assertIsDisplayed()

        compose.onNode(hasText("键盘布局").and(hasText("自动"))).performClick()
        compose.waitForIdle()
        compose.onNodeWithText("日语（JIS）").performClick()
        compose.waitForIdle()
        compose.onAllNodesWithText("美式英语（US）").assertCountEquals(0)
        compose.onNode(hasText("键盘布局").and(hasText("日语（JIS）"))).assertIsDisplayed()
        compose.onNodeWithContentDescription("指针速度").assertIsDisplayed()
    }

    @Test
    fun tapOutsideClosesTheSecondMenuFirstThenTheMenu() {
        openAdjust()
        compose.onNode(hasText("单指划动").and(hasText("移动指针"))).performClick()
        compose.waitForIdle()
        compose.onNodeWithText("滚动").assertIsDisplayed()
        // The menu's popup reaches the bottom of the screen; its empty part is "outside".
        val popup = compose.onAllNodes(isRoot())[1]
        popup.performTouchInput { click(Offset(width - 8.dp.toPx(), height - 8.dp.toPx())) }
        compose.waitForIdle()
        compose.onAllNodesWithText("滚动").assertCountEquals(0)
        compose.onNodeWithContentDescription("指针速度").assertIsDisplayed()
        popup.performTouchInput { click(Offset(width - 8.dp.toPx(), height - 8.dp.toPx())) }
        compose.waitForIdle()
        compose.onAllNodesWithContentDescription("指针速度").assertCountEquals(0)
    }
    @Test
    fun switchesComeAfterTheSpeedAndOrientationIsGone() {
        openAdjust(speakerOn = true)
        compose.onAllNodesWithText("屏幕方向").assertCountEquals(0)
        // Order (用户 2026-11-12): 指针速度, 键盘自动弹出, 接收音频, 共享麦克风, 键盘布局, 单指划动.
        val order = listOf("键盘自动弹出", "接收音频", "共享麦克风", "键盘布局", "单指划动")
            .map { compose.onNode(hasText(it), useUnmergedTree = true).getUnclippedBoundsInRoot().top }
        assertEquals(order.sorted(), order)
        assertTrue(compose.onNodeWithContentDescription("指针速度").getUnclippedBoundsInRoot().top < order.first())
        // 键盘自动弹出: off by default (用户 2026-11-12), the persisted ControlPrefs setting.
        compose.onNode(hasText("键盘自动弹出")).assert(isOff())
        compose.onNode(hasText("键盘自动弹出")).performClick()
        compose.waitForIdle()
        compose.onNode(hasText("键盘自动弹出")).assert(isOn())
        // 接收音频 / 共享麦克风: this phone's device settings, through the device page's toggles.
        compose.onNode(hasText("接收音频")).assert(isOn())
        compose.onNode(hasText("共享麦克风")).assert(isOff())
        compose.onNode(hasText("接收音频")).performClick()
        compose.onNode(hasText("共享麦克风")).performClick()
        compose.waitForIdle()
        assertEquals(listOf("speaker", "mic"), toggled)
    }
}
