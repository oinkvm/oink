package com.hachiwii.oink

import androidx.activity.ComponentActivity
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.assertIsNotFocused
import androidx.compose.ui.test.click
import androidx.compose.ui.test.getUnclippedBoundsInRoot
import androidx.compose.ui.test.hasSetTextAction
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.longClick
import androidx.compose.ui.test.onAllNodesWithContentDescription
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.onRoot
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import com.hachiwii.oink.ui.*
import org.junit.Assert.*
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * The soft keyboard on the transfer page: the tab bar and the composer follow the keyboard's
 * animated height, and taps on the list's background, the message menu and every overlay hide it
 * (by clearing focus, so the IME closes with its own animation).
 */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
// A plain Application: OinkApplication would start the connection service and load the Rust core.
@Config(sdk = [34], qualifiers = "zh-rCN-w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class TransferKeyboardTest {
    @get:Rule val compose = createAndroidComposeRule<ComponentActivity>()

    private val windowsPc = PeerUi("windowsPc", "Windows PC", "windows", latency = 4.0)
    private fun text(id: Long, text: String) = MessageUi(id, false, "windowsPc", "Windows PC", "windows", true, "text", text = text, copyable = true)
    private fun image(id: Long) = MessageUi(id, false, "windowsPc", "Windows PC", "windows", true, "image", name = "照片$id.jpg", progress = 100, imagePath = "/test/photo$id.jpg")
    private fun file(id: Long) = MessageUi(id, false, "windowsPc", "Windows PC", "windows", true, "file", name = "文件$id.pdf", detail = "PDF · 1 MB")

    /** The keyboard's height above the navigation bar, px, as the IME animation reports it frame by frame. */
    private val ime = mutableIntStateOf(0)

    private fun page(messages: List<MessageUi>, actions: AppActions = AppActions()) = compose.setContent {
        CompositionLocalProvider(LocalImeLift provides ImeLift { ime.intValue }) {
            OinkApp(AppModel(peers = listOf(windowsPc), messages = messages), UiInit(tab = Tab.Transfer), Prefs(scheme = "light"), {}, actions, false)
        }
    }

    private val composer get() = compose.onNode(hasSetTextAction())

    private fun focusComposer() {
        composer.performClick()
        composer.assertIsFocused()
    }

    private fun keyboard(height: Dp) {
        ime.intValue = with(compose.density) { height.roundToPx() }
        compose.waitForIdle()
    }

    // ---- tab bar and composer follow the keyboard ----

    @Test fun theKeyboardSlidesOverTheTabBarAndThenCarriesTheComposer() {
        page(listOf(text(1, "你好")))
        val composerBottom = composer.getUnclippedBoundsInRoot().bottom
        val tabBottom = compose.onNodeWithContentDescription("传输").getUnclippedBoundsInRoot().bottom

        // Rising, below the composer: nothing moves, the keyboard covers the bar from below.
        keyboard(40.dp)
        assertEquals(composerBottom.value, composer.getUnclippedBoundsInRoot().bottom.value, 1f)
        assertEquals(tabBottom.value, compose.onNodeWithContentDescription("传输").getUnclippedBoundsInRoot().bottom.value, 1f)

        // The bar is covered: it is no longer offered to touch or screen readers.
        keyboard(TAB_BAR_REACH)
        compose.onAllNodesWithContentDescription("传输").assertCountEquals(0)
        assertEquals(composerBottom.value, composer.getUnclippedBoundsInRoot().bottom.value, 1f)

        // Past the composer's resting place it rides on the keyboard, with no gap and no jump.
        keyboard(300.dp)
        assertEquals((composerBottom - (300.dp - TAB_BAR_SPACE)).value, composer.getUnclippedBoundsInRoot().bottom.value, 1f)
        keyboard(TAB_BAR_SPACE + 1.dp)
        assertEquals((composerBottom - 1.dp).value, composer.getUnclippedBoundsInRoot().bottom.value, 1f)

        // Closing: the same path back, the bar is uncovered where it always was.
        keyboard(20.dp)
        assertEquals(composerBottom.value, composer.getUnclippedBoundsInRoot().bottom.value, 1f)
        assertEquals(tabBottom.value, compose.onNodeWithContentDescription("传输").getUnclippedBoundsInRoot().bottom.value, 1f)
        keyboard(0.dp)
        compose.onNodeWithContentDescription("传输").assertExists()
    }

    // ---- taps that hide the keyboard ----

    @Test fun aTapOnTheListBackgroundHidesTheKeyboard() {
        page(listOf(text(1, "你好")))
        focusComposer()
        // The only message is at the top; the middle of the page is the list's background.
        compose.onRoot().performTouchInput { click(Offset(centerX, height * .45f)) }
        composer.assertIsNotFocused()
    }

    @Test fun messagesAndButtonsKeepTheirTaps() {
        val opened = mutableListOf<Long>()
        page(listOf(file(1)), AppActions(openFile = { opened += it.id }))
        focusComposer()
        compose.onNodeWithText("文件1.pdf").performClick()
        compose.waitForIdle()
        // Every file opens in the media viewer (native-rewrite.md 「全屏查看器」), here a PDF not
        // downloaded yet (its card with 下载), never the system's viewer directly.
        compose.onNodeWithContentDescription("关闭").assertExists()
        compose.onNodeWithText("下载").assertExists()
        assertEquals(emptyList<Long>(), opened)
        // The viewer is a Cover: opening it closes the keyboard.
        composer.assertIsNotFocused()
    }

    @Test fun theMessageMenuHidesTheKeyboardWithoutTakingWindowFocus() {
        page(listOf(text(1, "长按我")))
        focusComposer()
        compose.onNodeWithText("长按我").performTouchInput { longClick() }
        compose.onNodeWithText("删除").assertExists()
        composer.assertIsNotFocused()
        // Back reaches the activity and closes it: the popup window has no focus (a focusable one
        // would take window focus and drop the keyboard without its animation).
        compose.runOnUiThread { compose.activity.onBackPressedDispatcher.onBackPressed() }
        compose.waitForIdle()
        compose.onAllNodesWithText("删除").assertCountEquals(0)
    }

    @Test fun aTapOutsideTheMessageMenuOnlyClosesIt() {
        page(listOf(text(1, "长按我")))
        compose.onNodeWithText("长按我").performTouchInput { longClick() }
        compose.onNodeWithText("删除").assertExists()
        // On the composer: the menu closes, the field underneath does not get the tap.
        composer.performClick()
        compose.onAllNodesWithText("删除").assertCountEquals(0)
        composer.assertIsNotFocused()
    }

    // ---- overlays hide the keyboard ----

    @Test fun openingThePreviewHidesTheKeyboard() {
        page(listOf(image(1)))
        focusComposer()
        compose.onNodeWithContentDescription("打开图片").performClick()
        compose.onNodeWithContentDescription("关闭").assertExists()
        composer.assertIsNotFocused()
    }

    @Test fun openingTheAttachMenuHidesTheKeyboard() {
        page(listOf(text(1, "你好")))
        focusComposer()
        compose.onNodeWithContentDescription("添加附件").performClick()
        compose.onNodeWithText("图库").assertExists()
        composer.assertIsNotFocused()
    }

    @Test fun pagesAndOverlaysHideTheKeyboardWhenTheyOpen() {
        val ui = UiState(UiInit())
        var hidden = 0
        ui.dismissKeyboard = { hidden++ }
        ui.settingsOpen = true; assertEquals(1, hidden)
        ui.settingsOpen = true; assertEquals(1, hidden)
        ui.settingsOpen = false; assertEquals(1, hidden)
        ui.sheetPeer = "windowsPc"; assertEquals(2, hidden)
        ui.viewer = image(1); assertEquals(3, hidden)
        ui.addPage = true; assertEquals(4, hidden)
        // A sheet changing what it shows is not a new cover (the manual sheet focuses its own field).
        ui.pairingSheet = "info"; ui.pairingSheet = "info-all"; assertEquals(5, hidden)
        ui.tab = Tab.Transfer; assertEquals(6, hidden)
        ui.tab = Tab.Transfer; assertEquals(6, hidden)
    }
}
