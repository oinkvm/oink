package com.hachiwii.oink

import androidx.compose.ui.test.SemanticsNodeInteraction
import androidx.compose.ui.unit.DpRect
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.getUnclippedBoundsInRoot
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onAllNodesWithContentDescription
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.performClick
import androidx.compose.ui.unit.Dp
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
 * Control page layout and controls approved by the user on 2026-11-12 (docs/control-page.md §1–3):
 * portrait keyboard button beside the narrowed tab bar, 切换键盘 top right, 收起键盘 in the control
 * keys; the lock menu as a second card; the emulated keyboard's number row and bottom row.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class ControlLayoutTest {
    @get:Rule val compose = createAndroidComposeRule<androidx.activity.ComponentActivity>()
    private val pc = "a".repeat(64)

    private fun open(control: ControlInit? = null, game: ControlGameMode = ControlGameMode.OFF, active: Boolean = false): FakeControl {
        val fake = FakeControl(ControlState(focusDevice = pc, selectedDevice = pc, gameMode = game, gameActive = active,
            targets = listOf(ControlTarget(pc, "Windows PC", "windows", true))))
        val model = AppModel(selfName = "Phone", peers = listOf(PeerUi(pc, "Windows PC", "windows", latency = 4.0)))
        compose.setContent { OinkApp(model, UiInit(tab = Tab.Control, freeze = true, control = control), Prefs(scheme = "light"), {}, AppActions(control = fake), false) }
        compose.waitForIdle()
        return fake
    }

    private fun near(a: Dp, b: Dp) = kotlin.math.abs(a.value - b.value) < 1.5f

    @Test
    fun portraitKeyboardButtonBesideTheNarrowedTabBar() {
        open()
        val width = compose.onRoot().getUnclippedBoundsInRoot().right
        // The round button: 68, 16 from the right edge; the bar ends 8 before it.
        val fab = compose.onNodeWithContentDescription("弹出键盘").assertIsDisplayed().getUnclippedBoundsInRoot()
        assertTrue("fab $fab", near(fab.right, width - 16.dp) && near(fab.right - fab.left, 68.dp))
        val lastTab = compose.onNodeWithContentDescription("控制").getUnclippedBoundsInRoot()
        assertTrue("tab $lastTab, fab $fab", lastTab.right <= fab.left - 8.dp)
        // No keyboard yet: the top-right slot is empty (no header keyboard button, no switch).
        compose.onAllNodesWithContentDescription("切换键盘", substring = true).assertCountEquals(0)

        // The button opens the keyboard: 切换键盘 appears top right, the button leaves with the bar.
        compose.onNodeWithContentDescription("弹出键盘").performClick()
        compose.waitForIdle()
        compose.onNodeWithContentDescription("切换键盘：换到模拟键盘（发送原始键码）").assertIsDisplayed()
        compose.onAllNodesWithContentDescription("弹出键盘").assertCountEquals(0)
    }

    @Test
    @Config(qualifiers = "w700dp-h1000dp-xxhdpi")
    fun portraitBottomControlsStayTogetherAndCentredOnWideWindows() {
        open()
        val root = compose.onRoot().getUnclippedBoundsInRoot()
        val fab = compose.onNodeWithContentDescription("弹出键盘").getUnclippedBoundsInRoot()
        val first = compose.onNodeWithContentDescription("设备").getUnclippedBoundsInRoot()
        val last = compose.onNodeWithContentDescription("控制").getUnclippedBoundsInRoot()
        assertTrue("centred max-400 group: $first / $fab", near(first.left - 6.dp, (root.left + root.right) / 2 - 200.dp) && near(fab.right, (root.left + root.right) / 2 + 200.dp))
        assertTrue("8 dp between surfaces: $last / $fab", near(fab.left - (last.right + 6.dp), 8.dp))
    }

    @Test
    fun hideKeyRightOfUpClosesTheKeyboard() {
        open(ControlInit(keyboard = KeyboardKind.EMULATED))
        val header = compose.onNodeWithContentDescription("切换键盘：换到系统键盘（手机输入法）").assertIsDisplayed()
        val width = compose.onRoot().getUnclippedBoundsInRoot().right
        assertTrue(near(header.getUnclippedBoundsInRoot().right, width - 16.dp))
        // 收起键盘 sits right of ↑ (aligned with →).
        val hide = compose.onNodeWithContentDescription("收起键盘").getUnclippedBoundsInRoot()
        val up = compose.onNodeWithContentDescription("上").getUnclippedBoundsInRoot()
        val right = compose.onNodeWithContentDescription("右").getUnclippedBoundsInRoot()
        assertTrue(hide.left > up.right && near(hide.left, right.left))
        compose.onNodeWithContentDescription("收起键盘").performClick()
        compose.waitForIdle()
        compose.onAllNodesWithContentDescription("切换键盘", substring = true).assertCountEquals(0)
        compose.onNodeWithContentDescription("弹出键盘").assertIsDisplayed()
    }

    @Test
    fun numberRowStaysOnBothLayersAndTheBottomRowIsTheSame() {
        open(ControlInit(keyboard = KeyboardKind.EMULATED))
        fun row(labels: List<String>) = labels.map { compose.onNodeWithContentDescription(it).getUnclippedBoundsInRoot() }
        val digits = row(("1234567890").map { it.toString() })
        assertTrue(digits.all { near(it.top, digits[0].top) })
        val q = compose.onNodeWithContentDescription("q").getUnclippedBoundsInRoot()
        assertTrue("digits above the letters", digits[0].bottom < q.top)
        // Letters: #+= , space . return.
        val bottom = row(listOf("符号", ",", "空格", ".", "回车"))
        assertTrue(bottom.zipWithNext().all { (a, b) -> a.right < b.left && near(a.top, b.top) })
        // Five rows of one height.
        assertTrue(near(digits[0].bottom - digits[0].top, q.bottom - q.top) && near(q.bottom - q.top, bottom[0].bottom - bottom[0].top))

        compose.onNodeWithContentDescription("符号").performClick()
        compose.waitForIdle()
        // Symbols: the same number row in the same place, ABC in the switch's place.
        val again = row(("1234567890").map { it.toString() })
        assertTrue(again.zip(digits).all { (a, b) -> near(a.top, b.top) && near(a.left, b.left) })
        compose.onNodeWithContentDescription("字母").assertIsDisplayed()
        compose.onNodeWithContentDescription("-").assertIsDisplayed()
        compose.onAllNodesWithContentDescription("q").assertCountEquals(0)
    }

    @Test
    fun shiftShowsWhatTheNumberRowAndCommaType() {
        open(ControlInit(keyboard = KeyboardKind.EMULATED, mods = mapOf(Mod.SHIFT to Latch.HELD)))
        listOf("!", "@", "#", "(", ")", "<", ">").forEach { compose.onNodeWithContentDescription(it).assertIsDisplayed() }
        compose.onNodeWithContentDescription("Q").assertIsDisplayed()
    }

    @Test
    fun lockMenuOpensGameModeAsASecondCard() {
        val fake = open(ControlInit(menu = "lock"), ControlGameMode.AUTO, active = false)
        compose.onNodeWithText("锁定指针").assertIsDisplayed()
        compose.onNode(hasText("游戏模式").and(hasText("自动"))).performClick()
        compose.waitForIdle()
        // The second card under the row; the first stays open; no back row.
        compose.onNodeWithText("开").assertIsDisplayed()
        compose.onNodeWithText("关").assertIsDisplayed()
        compose.onNodeWithText("锁定指针").assertIsDisplayed()
        compose.onAllNodesWithContentDescription("返回：游戏模式").assertCountEquals(0)
        val row = compose.onNode(hasText("游戏模式").and(hasText("自动"))).getUnclippedBoundsInRoot()
        val on = compose.onNodeWithText("开").getUnclippedBoundsInRoot()
        assertTrue("second card under the row: $row / $on", on.top >= row.bottom)
        compose.onNodeWithText("开").performClick()
        compose.waitForIdle()
        assertTrue(fake.sent.toString(), fake.sent.contains("game ON"))
        compose.onAllNodesWithText("锁定指针").assertCountEquals(0)
    }

    @Test
    fun fullscreenButtonTopRightTurnsTheInterfaceToLandscape() {
        open()
        val width = compose.onRoot().getUnclippedBoundsInRoot().right
        val full = compose.onNodeWithContentDescription("全屏").assertIsDisplayed().getUnclippedBoundsInRoot()
        assertTrue(near(full.right, width - 16.dp))
        compose.onNodeWithContentDescription("全屏").performClick()
        compose.waitForIdle()
        assertTrue(compose.activity.requestedOrientation == android.content.pm.ActivityInfo.SCREEN_ORIENTATION_USER_LANDSCAPE)
    }

    @Test
    fun whileAKeyboardIsUpTheSlotHoldsOnlyTheSwitch() {
        open(ControlInit(keyboard = KeyboardKind.EMULATED))
        compose.onAllNodesWithContentDescription("全屏").assertCountEquals(0)
        compose.onNodeWithContentDescription("切换键盘：换到系统键盘（手机输入法）").assertIsDisplayed()
    }

    @Test
    fun capsLockReplacesTheControlAreaShift() {
        val fake = open(ControlInit(keyboard = KeyboardKind.EMULATED))
        // The letters keep their own Shift; the control area has Caps Lock where Shift was.
        compose.onAllNodesWithContentDescription("Shift").assertCountEquals(1)
        val caps = compose.onNodeWithContentDescription("Caps Lock").getUnclippedBoundsInRoot()
        val home = compose.onNodeWithContentDescription("Home").getUnclippedBoundsInRoot()
        assertTrue(caps.right < home.left && near(caps.top, home.top))
        fake.sent.clear()
        compose.onNodeWithContentDescription("Caps Lock").performClick()
        compose.waitForIdle()
        // Accessibility activation is one complete Caps down/up, without a synthetic hold.
        assertEquals(listOf("key ${Integer.toHexString(Hid.CAPS_LOCK)} true", "key ${Integer.toHexString(Hid.CAPS_LOCK)} false"), fake.sent.filter { it.startsWith("key") })
    }

    @Test
    fun gameModeOnShowsNoAutoTagAndATapTurnsItOff() {
        val fake = open(game = ControlGameMode.AUTO, active = true)
        compose.onAllNodesWithText("AUTO").assertCountEquals(0)
        compose.onNodeWithContentDescription("游戏模式已开启（自动），点按关闭游戏模式，长按更多").performClick()
        compose.waitForIdle()
        assertTrue(fake.sent.toString(), fake.sent.containsAll(listOf("game OFF", "lock false")))
    }
}

/**
 * The control page held sideways = the full screen (用户 2026-11-12): the pad covers the whole screen
 * and the top bar floats over it 8 below the status bar (the tabs in it, the tab bar gone; keyboard
 * toggle then 恢复 at the right); the emulated keyboard floats in a portrait-width container on the side away
 * from the cutout, 8 from that screen edge, and the pad stays full screen beside it.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], qualifiers = "w893dp-h412dp-land-xxhdpi", application = android.app.Application::class)
class ControlLandscapeLayoutTest {
    @get:Rule val compose = createAndroidComposeRule<androidx.activity.ComponentActivity>()
    private val pc = "a".repeat(64)

    private fun open(control: ControlInit? = null) {
        val fake = FakeControl(ControlState(focusDevice = pc, selectedDevice = pc, targets = listOf(ControlTarget(pc, "Windows PC", "windows", true))))
        val model = AppModel(selfName = "Phone", peers = listOf(PeerUi(pc, "Windows PC", "windows", latency = 4.0)))
        compose.setContent { OinkApp(model, UiInit(tab = Tab.Control, freeze = true, control = control), Prefs(scheme = "light"), {}, AppActions(control = fake), false) }
        compose.waitForIdle()
    }
    private fun near(a: Dp, b: Dp) = kotlin.math.abs(a.value - b.value) < 1.5f
    // The floating keyboard scales its local Density with its reference width. Compose's dp bounds
    // helper divides each node by that node's density, so those dp values cannot be compared with
    // the root's dp values. Compare all physical geometry in the root's density instead.
    private fun SemanticsNodeInteraction.rootDpBounds(): DpRect {
        val rect = fetchSemanticsNode().boundsInRoot
        return with(compose.density) { DpRect(rect.left.toDp(), rect.top.toDp(), rect.right.toDp(), rect.bottom.toDp()) }
    }

    @Test
    fun padIsTheWholeScreenAndTheBarFloatsOverIt() {
        open()
        // One set of tabs: the bar's (the tab bar is out of the tree).
        listOf("设备", "传输", "控制").forEach { compose.onAllNodesWithContentDescription(it).assertCountEquals(1) }
        val focus = compose.onNodeWithContentDescription("焦点设备：Windows PC").rootDpBounds()
        val tab = compose.onNodeWithContentDescription("控制").rootDpBounds()
        assertTrue("tabs left of the capsule: $tab / $focus", tab.right < focus.left)
        assertTrue("capsule 48 high", near(focus.bottom - focus.top, 48.dp))
        val root = compose.onRoot().rootDpBounds()
        val pad = compose.onNodeWithContentDescription("触摸板").rootDpBounds()
        assertTrue("pad to every edge: $pad / $root", pad.left <= root.left && pad.top <= root.top && pad.right >= root.right && pad.bottom >= root.bottom)
        // The row 8 below the (status bar of the) app area; 恢复 at the far right, the keyboard toggle left of it.
        val adjust = compose.onNodeWithContentDescription("调节").rootDpBounds()
        assertTrue("row 8 from the top: $adjust", near(adjust.top, 8.dp) && near(adjust.bottom - adjust.top, 48.dp))
        val restore = compose.onNodeWithContentDescription("恢复").rootDpBounds()
        val toggle = compose.onNodeWithContentDescription("弹出键盘").rootDpBounds()
        assertTrue(near(restore.right, root.right - 16.dp) && toggle.right < restore.left)

        // Landscape remembers the emulated keyboard (default): it floats, and the bar hides.
        compose.onNodeWithContentDescription("弹出键盘").performClick()
        compose.waitForIdle()
        compose.onNodeWithContentDescription("拖动键盘").assertExists()
        compose.onAllNodesWithContentDescription("恢复").assertCountEquals(0)
    }

    @Test
    fun floatingKeyboardSitsAwayFromTheCutout() {
        open(ControlInit(keyboard = KeyboardKind.EMULATED))
        val root = compose.onRoot().rootDpBounds()
        val handle = compose.onNodeWithContentDescription("拖动键盘").rootDpBounds()
        assertTrue("portrait width, 8 from the right edge: $handle", near(handle.right, root.right - 8.dp) && near(handle.right - handle.left, FloatMotion.WIDTH.dp))
        val pad = compose.onNodeWithContentDescription("触摸板").rootDpBounds()
        assertTrue("pad stays full screen: $pad", pad.left <= root.left && pad.right >= root.right)
        val esc = compose.onNodeWithContentDescription("Esc").rootDpBounds()
        assertTrue("keys start at the scaled top padding: $esc / $handle", near(esc.top, handle.top + (22 * FloatMotion.floatingScale(FloatMotion.WIDTH)).dp) && esc.left >= handle.left)
    }

    @Test
    fun cutoutOnTheRightPutsItOnTheLeft() {
        open(ControlInit(keyboard = KeyboardKind.EMULATED, flip = true))
        val handle = compose.onNodeWithContentDescription("拖动键盘").rootDpBounds()
        val root = compose.onRoot().rootDpBounds()
        assertTrue("8 from the left edge: $handle", near(handle.left, root.left + 8.dp) && near(handle.right - handle.left, FloatMotion.WIDTH.dp))
    }
}
