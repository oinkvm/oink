package com.hachiwii.oink

import android.view.inputmethod.InputMethodManager
import androidx.activity.ComponentActivity
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.assertIsNotFocused
import androidx.compose.ui.test.hasSetTextAction
import androidx.compose.ui.test.junit4.createAndroidComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.performClick
import com.hachiwii.oink.ui.*
import androidx.lifecycle.Lifecycle
import org.junit.Assert.assertFalse
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config

/**
 * Entering 传输 leaves the keyboard closed (user 2026-11-12): the composer is not focused and no soft
 * keyboard is asked for until the user taps the input. The way in that triggered it on phones is from
 * the control page, which holds focus for the hardware keyboard. Run on the app's lowest SDK (30) and a
 * current one. The focus checks are what matter; the InputMethodManager check only catches a direct
 * showSoftInput (from 30 on Compose may ask through the insets controller instead).
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [30, 34], qualifiers = "zh-rCN-w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class TransferComposerFocusTest {
    @get:Rule val compose = createAndroidComposeRule<ComponentActivity>()

    /** The soft keyboard's height for the page (Robolectric has no IME): 0 = closed. */
    private val ime = mutableIntStateOf(0)
    private fun app(tab: Tab) {
        val peers = listOf(PeerUi("windowsPc", "Windows PC", "windows", latency = 4.0))
        compose.setContent {
            CompositionLocalProvider(LocalImeLift provides ImeLift { ime.intValue }) {
                OinkApp(AppModel(peers = peers), UiInit(tab = tab), Prefs(scheme = "light"), {}, AppActions(), false)
            }
        }
        compose.waitForIdle()
    }
    /** The app goes to the background (or the share sheet covers it) and comes back; the keyboard goes away meanwhile. */
    private fun backgroundAndBack() {
        compose.activityRule.scenario.moveToState(Lifecycle.State.CREATED)
        ime.intValue = 0
        compose.waitForIdle()
        compose.activityRule.scenario.moveToState(Lifecycle.State.RESUMED)
        compose.waitForIdle()
    }
    private fun imeShown() = shadowOf(compose.activity.getSystemService(InputMethodManager::class.java)).isSoftInputVisible
    private fun composer() = compose.onNode(hasSetTextAction())

    @Test fun enteringTransferFromAnotherTabLeavesTheKeyboardClosed() {
        app(Tab.Control)
        compose.onNodeWithContentDescription("传输").performClick()
        compose.waitForIdle()
        composer().assertIsNotFocused()
        assertFalse(imeShown())

        compose.onNodeWithContentDescription("设备").performClick()
        compose.waitForIdle()
        compose.onNodeWithContentDescription("传输").performClick()
        compose.waitForIdle()
        composer().assertIsNotFocused()
        assertFalse(imeShown())
    }

    @Test fun tappingTheInputStillFocusesIt() {
        app(Tab.Transfer)
        composer().assertIsNotFocused()
        composer().performClick()
        compose.waitForIdle()
        composer().assertIsFocused()
        // Away and back: the focus is not restored.
        compose.onNodeWithContentDescription("控制").performClick()
        compose.waitForIdle()
        compose.onNodeWithContentDescription("传输").performClick()
        compose.waitForIdle()
        composer().assertIsNotFocused()
    }
    /** User 2026-11-12, as on iOS: typing when the app left (focused, keyboard up) is typing again on return. */
    @Test fun typingWhenTheAppLeftIsTypingAgainOnReturn() {
        app(Tab.Transfer)
        composer().performClick()
        ime.intValue = 300
        compose.waitForIdle()
        composer().assertIsFocused()
        backgroundAndBack()
        composer().assertIsFocused()
    }

    @Test fun notTypingWhenTheAppLeftStaysClosedOnReturn() {
        app(Tab.Transfer)
        backgroundAndBack()
        composer().assertIsNotFocused()
        assertFalse(imeShown())
        // Typing, then the keyboard closed by the user (back) before leaving: closed on return too.
        composer().performClick()
        ime.intValue = 300
        compose.waitForIdle()
        ime.intValue = 0
        compose.waitForIdle()
        backgroundAndBack()
        composer().assertIsNotFocused()
    }
}
