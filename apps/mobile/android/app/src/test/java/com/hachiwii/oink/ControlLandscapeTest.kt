package com.hachiwii.oink

import android.content.pm.ActivityInfo
import androidx.activity.ComponentActivity
import androidx.activity.OnBackPressedCallback
import androidx.compose.runtime.mutableStateOf
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithContentDescription
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.doubleClick
import androidx.compose.ui.test.assert
import androidx.compose.ui.test.SemanticsMatcher
import androidx.compose.ui.semantics.SemanticsProperties
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.unit.dp
import com.hachiwii.oink.control.*
import com.hachiwii.oink.ui.*
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * The landscape full screen: the floating top bar stays visible while the pad is in use,
 * later fingers on its buttons join the pad, idle button downs still work, and 恢复
 * turns the interface to portrait, and the floating keyboard's handle expands, restores and dismisses it.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], qualifiers = "w893dp-h412dp-land-xxhdpi", application = android.app.Application::class)
class ControlLandscapeTest {
    @get:Rule val compose = createAndroidComposeRule<ComponentActivity>()
    private val pc = "a".repeat(64)
    private val toggled = ArrayList<String>()

    private fun open(control: ControlInit? = null): FakeControl {
        val fake = FakeControl(ControlState(focusDevice = pc, selectedDevice = pc, targets = listOf(ControlTarget(pc, "Windows PC", "windows", true))))
        val model = AppModel(selfName = "Phone", peers = listOf(PeerUi(pc, "Windows PC", "windows", latency = 4.0)))
        val actions = AppActions(control = fake, toggleSpeaker = { toggled += "speaker" }, toggleMic = { toggled += "mic" })
        compose.setContent { OinkApp(model, UiInit(tab = Tab.Control, freeze = true, control = control), Prefs(scheme = "light"), {}, actions, false) }
        compose.waitForIdle()
        return fake
    }

    @Test
    fun expandedCapsSendsRealTouchEdgesWithoutWaitingForReleaseToPress() {
        val fake = open(ControlInit(keyboard = KeyboardKind.EMULATED, expanded = true))
        compose.waitForIdle()
        fake.sent.clear()
        val caps = compose.onNodeWithContentDescription("Caps Lock")
        caps.performTouchInput { down(center) }
        compose.waitForIdle()
        assertEquals(listOf("key ${Integer.toHexString(Hid.CAPS_LOCK)} true"), fake.sent.filter { it.startsWith("key") })
        caps.performTouchInput { advanceEventTime(1_200); up() }
        compose.waitForIdle()
        assertEquals(listOf("key ${Integer.toHexString(Hid.CAPS_LOCK)} true", "key ${Integer.toHexString(Hid.CAPS_LOCK)} false"), fake.sent.filter { it.startsWith("key") })
    }

    @Test
    fun barStaysVisibleAndNewFingersOnButtonsBelongToThePad() {
        val fake = open()
        val restore = compose.onNodeWithContentDescription("恢复").fetchSemanticsNode().boundsInRoot.center
        fake.sent.clear()
        compose.onRoot().performTouchInput {
            down(0, Offset(200.dp.toPx(), 200.dp.toPx()))
            // No recomposition between downs: the ancestor must take the second finger immediately.
            down(1, restore)
            moveTo(0, restore + Offset(-20f, 0f))
            up(1); up(0)
        }
        compose.waitForIdle()
        compose.onNodeWithContentDescription("恢复").assertIsDisplayed()
        assertEquals(ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED, compose.activity.requestedOrientation)
        assertTrue(fake.sent.toString(), fake.contacts.any { it.size == 2 })
    }

    @Test
    fun movingIntoABarButtonDoesNotActivateItAndTheBarStaysUp() {
        open()
        val restore = compose.onNodeWithContentDescription("恢复").fetchSemanticsNode().boundsInRoot.center
        compose.onRoot().performTouchInput { down(Offset(200.dp.toPx(), 200.dp.toPx())); moveTo(restore); up() }
        compose.waitForIdle()
        compose.onNodeWithContentDescription("恢复").assertIsDisplayed()
        assertEquals(ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED, compose.activity.requestedOrientation)
        // A new gesture that starts on the button still works.
        compose.onRoot().performTouchInput { down(restore); up() }
        compose.waitForIdle()
        assertEquals(ActivityInfo.SCREEN_ORIENTATION_USER_PORTRAIT, compose.activity.requestedOrientation)
    }

    @Test
    fun anIdleLockButtonDownStaysWithTheButton() {
        val fake = open()
        fake.sent.clear(); fake.contacts.clear()
        compose.onNodeWithContentDescription("指针未锁定", substring = true).performTouchInput { down(center); up() }
        compose.waitForIdle()
        assertTrue(fake.sent.toString(), "lock true" in fake.sent)
        assertTrue("button touch must not create pad contacts", fake.contacts.isEmpty())
    }

    @Test
    fun aTouchBetweenTheBarsItemsGoesToThePad() {
        val fake = open()
        fake.sent.clear()
        // 100 dp in: right of 调节 (16–64), left of the centred group; 32 down: in the bar's row.
        compose.onRoot().performTouchInput { down(Offset(100.dp.toPx(), 32.dp.toPx())); up() }
        compose.waitForIdle()
        assertTrue(fake.sent.toString(), fake.sent.any { it.startsWith("button") })
    }

    @Test
    fun restoreTurnsTheInterfaceToPortrait() {
        open()
        compose.onNodeWithContentDescription("恢复").performClick()
        compose.waitForIdle()
        assertEquals(ActivityInfo.SCREEN_ORIENTATION_USER_PORTRAIT, compose.activity.requestedOrientation)
    }

    @Test
    fun handleDoubleTapExpandsRestoreFloatsAndDownDismisses() {
        open(ControlInit(keyboard = KeyboardKind.EMULATED))
        // Floating: the switch key right of ↑ (no 收起键盘 there), Caps instead of the control area's Shift.
        compose.onNodeWithContentDescription("切换键盘：换到系统键盘（手机输入法）").assertExists()
        compose.onAllNodesWithContentDescription("收起键盘").assertCountEquals(0)
        compose.onNodeWithContentDescription("Caps Lock").assertExists()
        val handle = compose.onNodeWithContentDescription("拖动键盘")
        handle.performTouchInput { doubleClick() }
        compose.waitForIdle()
        compose.onAllNodesWithContentDescription("拖动键盘").assertCountEquals(0)
        compose.onNodeWithContentDescription("还原：回到浮动键盘").assertExists()
        compose.onNodeWithContentDescription("Print Screen").assertExists()
        compose.onNodeWithContentDescription("还原：回到浮动键盘").performClick()
        compose.waitForIdle()
        compose.onNodeWithContentDescription("拖动键盘").performTouchInput { down(center); repeat(10) { moveBy(Offset(0f, 12.dp.toPx())) }; up() }
        compose.waitForIdle()
        compose.onAllNodesWithContentDescription("拖动键盘").assertCountEquals(0)
        compose.onAllNodesWithContentDescription("Caps Lock").assertCountEquals(0)
    }

    @Test
    fun evenALargeDragUpStaysFloating() {
        open(ControlInit(keyboard = KeyboardKind.EMULATED))
        compose.onNodeWithContentDescription("拖动键盘").performTouchInput {
            down(center); repeat(10) { moveBy(Offset(0f, -12.dp.toPx())) }
            advanceEventTime(300); moveBy(Offset(0f, -.1f)); advanceEventTime(300); up()
        }
        compose.waitForIdle()
        compose.onNodeWithContentDescription("拖动键盘").assertExists()
        compose.onAllNodesWithContentDescription("还原：回到浮动键盘").assertCountEquals(0)
    }

    @Test
    fun handlePressClearsOnReleaseAndSingleTapDoesNotExpand() {
        open(ControlInit(keyboard = KeyboardKind.EMULATED))
        val handle = compose.onNodeWithContentDescription("拖动键盘")
        handle.performTouchInput { down(center) }
        compose.waitForIdle()
        handle.assert(SemanticsMatcher.expectValue(SemanticsProperties.StateDescription, "已按下"))
        handle.performTouchInput { up() }
        compose.waitForIdle()
        handle.assert(SemanticsMatcher.expectValue(SemanticsProperties.StateDescription, "未按下"))
        compose.onAllNodesWithContentDescription("还原：回到浮动键盘").assertCountEquals(0)
    }

    @Test
    fun expandedKeyboardOmitsOriginalBarEvenWithAnOpenMenu() {
        open(ControlInit(keyboard = KeyboardKind.EMULATED, expanded = true, menu = "adjust"))
        compose.onAllNodesWithContentDescription("恢复").assertCountEquals(0)
        compose.onNodeWithContentDescription("还原：回到浮动键盘").assertExists()
    }

    @Test
    fun landscapeSystemKeyboardOmitsOriginalBarEvenWithAnOpenMenu() {
        open(ControlInit(keyboard = KeyboardKind.SYSTEM, menu = "adjust"))
        compose.onAllNodesWithContentDescription("恢复").assertCountEquals(0)
    }

    @Test
    fun landscapeFloatingKeyboardUsesReferenceWidthAndOneToThreeKeyAreas() {
        open(ControlInit(keyboard = KeyboardKind.EMULATED))
        val bounds = compose.onNodeWithContentDescription("拖动键盘").fetchSemanticsNode().boundsInRoot
        assertEquals(with(compose.density) { FloatMotion.WIDTH.dp.toPx() }, bounds.width, 1f)
        assertEquals(with(compose.density) { (22 * FloatMotion.floatingScale(FloatMotion.WIDTH)).dp.toPx() }, bounds.height, 1f)
        val esc = compose.onNodeWithContentDescription("Esc").fetchSemanticsNode().boundsInRoot
        val ctrl = compose.onNodeWithContentDescription("Ctrl").fetchSemanticsNode().boundsInRoot
        val number = compose.onNodeWithContentDescription("1").fetchSemanticsNode().boundsInRoot
        val enter = compose.onNodeWithContentDescription("回车").fetchSemanticsNode().boundsInRoot
        assertEquals((ctrl.bottom - esc.top) * 3, enter.bottom - number.top, 3f)
    }

    @Test
    fun horizontalFlickMovesTheFloatingKeyboard() {
        open(ControlInit(keyboard = KeyboardKind.EMULATED))
        val handle = compose.onNodeWithContentDescription("拖动键盘")
        val before = handle.fetchSemanticsNode().boundsInRoot.left
        handle.performTouchInput { down(center); repeat(8) { moveBy(Offset(-15.dp.toPx(), 0f)) }; up() }
        compose.waitForIdle()
        assertTrue(handle.fetchSemanticsNode().boundsInRoot.left < before)
        compose.onAllNodesWithContentDescription("还原：回到浮动键盘").assertCountEquals(0)
    }

    @Test
    fun barReturnsImmediatelyAfterClosingExpandedKeyboard() {
        open(ControlInit(keyboard = KeyboardKind.EMULATED, expanded = true))
        compose.mainClock.autoAdvance = false
        compose.onNodeWithContentDescription("收起键盘").performClick()
        compose.waitForIdle()
        compose.mainClock.advanceTimeBy(48) // Render frames only; much less than the 400 ms idle delay.
        compose.waitForIdle()
        compose.onNodeWithContentDescription("恢复").assertExists()
    }

    @Test
    fun systemKeyboardAt134UsesTwoRowsWithoutPadAndNavigationBeforeMedia() {
        val fake = open(ControlInit(keyboard = KeyboardKind.SYSTEM, systemRemaining = 134f))
        compose.onAllNodesWithContentDescription("触摸板").assertCountEquals(0)
        compose.onAllNodesWithContentDescription("恢复").assertCountEquals(0)
        val home = compose.onNodeWithContentDescription("Home").fetchSemanticsNode().boundsInRoot
        val previous = compose.onNodeWithContentDescription("快退 / 上一首").fetchSemanticsNode().boundsInRoot
        val caps = compose.onNodeWithContentDescription("Caps Lock").fetchSemanticsNode().boundsInRoot
        assertEquals(home.top, previous.top, 1f)
        assertTrue(home.left < previous.left)
        assertTrue(caps.top > home.top)
        fake.sent.clear()
        compose.onNodeWithContentDescription("Home").performClick()
        assertEquals(listOf("key ${Integer.toHexString(Hid.HOME)} true", "key ${Integer.toHexString(Hid.HOME)} false"), fake.sent.filter { it.startsWith("key") })
        compose.onNodeWithContentDescription("Fn").performClick()
        compose.waitForIdle()
        (1..12).forEach { compose.onNodeWithContentDescription("F$it").assertExists() }
        compose.onAllNodesWithContentDescription("Home").assertCountEquals(0)
        compose.onNodeWithContentDescription("Esc").assertExists()
        compose.onNodeWithContentDescription("收起键盘").assertExists()
    }

    @Test
    fun systemKeyboardAt135HasLeftPadAndRightThreeRowsWithInvertedT() {
        val fake = open(ControlInit(keyboard = KeyboardKind.SYSTEM, systemRemaining = 135f))
        compose.onAllNodesWithContentDescription("恢复").assertCountEquals(0)
        val pad = compose.onNodeWithContentDescription("触摸板").fetchSemanticsNode().boundsInRoot
        val esc = compose.onNodeWithContentDescription("Esc").fetchSemanticsNode().boundsInRoot
        val up = compose.onNodeWithContentDescription("上").fetchSemanticsNode().boundsInRoot
        val downBounds = compose.onNodeWithContentDescription("下").fetchSemanticsNode().boundsInRoot
        val left = compose.onNodeWithContentDescription("左").fetchSemanticsNode().boundsInRoot
        val right = compose.onNodeWithContentDescription("右").fetchSemanticsNode().boundsInRoot
        assertTrue(pad.right <= esc.left)
        assertEquals(up.center.x, downBounds.center.x, 1f)
        assertTrue(up.top < downBounds.top)
        assertTrue(left.left < downBounds.left && downBounds.left < right.left)
        fake.sent.clear()
        compose.onNodeWithContentDescription("触摸板").performTouchInput { down(center); up() }
        compose.waitForIdle()
        assertTrue(fake.sent.toString(), fake.sent.any { it.startsWith("button") })
        compose.onNodeWithContentDescription("Fn").performClick()
        compose.waitForIdle()
        (1..12).forEach { compose.onNodeWithContentDescription("F$it").assertExists() }
        compose.onNodeWithContentDescription("Home").assertExists()
        compose.onAllNodesWithContentDescription("快退 / 上一首").assertCountEquals(0)
    }

    /** Render the actual page with an inspectable holder; keyboard presentation is irrelevant here. */
    private fun pageHolder(kind: KeyboardKind = KeyboardKind.EMULATED): ControlHolder {
        val holder = ControlHolder(null, ControlPrefs(keyboardLand = kind)) {}
        val fake = FakeControl(ControlState(focusDevice = pc, selectedDevice = pc, targets = listOf(ControlTarget(pc, "Desktop", "windows", true))))
        val toast = mutableStateOf<ToastData?>(null)
        compose.setContent {
            OinkTheme(OinkColors.of(false, Accent.Pink)) {
                ControlPage(holder, AppModel(), AppActions(control = fake), true, ControlLayout.LANDSCAPE, toast)
            }
        }
        compose.waitForIdle()
        return holder
    }

    @Test
    fun barReturnsAfterKeyboardClosesEvenDuringPadUse() {
        val holder = pageHolder()
        compose.runOnIdle { holder.toggleKeyboard() }
        compose.waitForIdle()
        compose.runOnIdle { holder.pad.down(1L, 100f, 100f, 0L); holder.closeKeyboard() }
        compose.waitForIdle()
        compose.onNodeWithContentDescription("恢复").assertIsDisplayed()
        compose.runOnIdle { holder.pad.up(1L, 600L) }
        compose.onNodeWithContentDescription("恢复").assertIsDisplayed()
    }

    @Test
    fun backClosesEmulatedKeyboardThenRestoresButtonFullscreen() = backClosesKeyboardFirst(KeyboardKind.EMULATED)

    @Test
    fun backClosesPendingSystemKeyboardThenRestoresButtonFullscreen() = backClosesKeyboardFirst(KeyboardKind.SYSTEM)

    private fun backClosesKeyboardFirst(kind: KeyboardKind) {
        val holder = pageHolder(kind)
        compose.runOnIdle { holder.fullscreen.enter(); holder.toggleKeyboard() }
        compose.waitForIdle()
        compose.runOnIdle { compose.activity.onBackPressedDispatcher.onBackPressed() }
        compose.waitForIdle()
        compose.runOnIdle {
            assertTrue(holder.fullscreen.enteredByButton)
            org.junit.Assert.assertFalse(holder.keyboardOpen)
        }
        compose.onNodeWithContentDescription("恢复").assertExists()
        compose.runOnIdle { compose.activity.onBackPressedDispatcher.onBackPressed() }
        compose.waitForIdle()
        compose.runOnIdle { org.junit.Assert.assertFalse(holder.fullscreen.enteredByButton) }
        assertEquals(ActivityInfo.SCREEN_ORIENTATION_USER_PORTRAIT, compose.activity.requestedOrientation)
    }

    @Test
    fun rotationFullscreenKeepsNormalBackBehaviour() {
        var normalBack = 0
        compose.activity.onBackPressedDispatcher.addCallback(compose.activity, object : OnBackPressedCallback(true) {
            override fun handleOnBackPressed() { normalBack++ }
        })
        val holder = pageHolder()
        compose.runOnIdle { compose.activity.onBackPressedDispatcher.onBackPressed() }
        compose.waitForIdle()
        assertEquals(1, normalBack)
        org.junit.Assert.assertFalse(holder.fullscreen.enteredByButton)
        assertEquals(ActivityInfo.SCREEN_ORIENTATION_UNSPECIFIED, compose.activity.requestedOrientation)
    }

    @Test
    fun expandedAudioKeysAreTheDeviceSwitches() {
        open(ControlInit(keyboard = KeyboardKind.EMULATED, expanded = true))
        compose.onNodeWithContentDescription("接收音频").performClick()
        compose.onNodeWithContentDescription("共享麦克风").performClick()
        compose.waitForIdle()
        assertEquals(listOf("speaker", "mic"), toggled)
    }
}
