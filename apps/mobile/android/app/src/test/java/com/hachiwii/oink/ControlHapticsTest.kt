package com.hachiwii.oink

import android.view.HapticFeedbackConstants
import android.view.View
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTouchInput
import com.hachiwii.oink.control.KeyboardKind
import com.hachiwii.oink.ui.*
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config

/** Checks native view feedback requests; physical strength and system settings need device validation. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class ControlHapticsTest {
    @get:Rule val compose = createComposeRule()
    private lateinit var view: View

    @Test fun keyboardAndControlRowsRequestLightKeyboardFeedback() {
        compose.setContent {
            view = LocalView.current
            OinkApp(AppModel(), UiInit(tab = Tab.Control, freeze = true, control = ControlInit(keyboard = KeyboardKind.EMULATED)),
                Prefs(scheme = "light"), {}, AppActions(), false)
        }
        compose.waitForIdle()
        listOf("a", "复制（Ctrl+C）", "Ctrl").forEach { label ->
            compose.onNodeWithContentDescription(label).performTouchInput { down(center); up() }
            compose.waitForIdle()
            assertEquals(label, HapticFeedbackConstants.KEYBOARD_TAP, shadowOf(view).lastHapticFeedbackPerformed())
        }
    }

    @Test fun padClickAndPageButtonRequestContextFeedback() {
        compose.setContent {
            view = LocalView.current
            OinkApp(AppModel(), UiInit(tab = Tab.Control, freeze = true), Prefs(scheme = "light"), {}, AppActions(), false)
        }
        compose.waitForIdle()
        compose.onNodeWithContentDescription("触摸板").performTouchInput { down(center); up() }
        compose.waitForIdle()
        assertEquals(HapticFeedbackConstants.CONTEXT_CLICK, shadowOf(view).lastHapticFeedbackPerformed())
        compose.onNodeWithContentDescription("调节").performClick()
        compose.waitForIdle()
        assertEquals(HapticFeedbackConstants.CONTEXT_CLICK, shadowOf(view).lastHapticFeedbackPerformed())
    }
}
