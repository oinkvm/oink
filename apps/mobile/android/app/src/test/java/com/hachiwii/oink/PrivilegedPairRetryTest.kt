package com.hachiwii.oink

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.hasSetTextAction
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performTextInput
import com.hachiwii.oink.ui.Accent
import com.hachiwii.oink.ui.AppActions
import com.hachiwii.oink.ui.AppModel
import com.hachiwii.oink.ui.OinkColors
import com.hachiwii.oink.ui.OinkTheme
import com.hachiwii.oink.ui.Prefs
import com.hachiwii.oink.ui.SettingsPage
import com.hachiwii.oink.ui.UiInit
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * 特权模式 after a failed pairing (Phone report 2026-11-12): 「重新配对」 goes back to 第 3 步, the
 * pairing service is looked for again, a tap on the code cells focuses the field (which asks for the
 * keyboard), and a new code can be entered and sent.
 */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class PrivilegedPairRetryTest {
    @get:Rule val compose = createComposeRule()

    @Test fun failureThenBackThenRetry() {
        var prefs by mutableStateOf(Prefs(scheme = "light", privileged = "failed"))
        val service = mutableListOf<Boolean>()
        val started = mutableListOf<String>()
        val actions = AppActions(pairingService = { service += it }, privilegeStart = { started += it })
        compose.setContent {
            OinkTheme(OinkColors.of(false, Accent.Pink)) {
                SettingsPage(UiInit(settingsStack = listOf("main", "privileged")), prefs, { prefs = it },
                    AppModel(privilegedFailure = "配对码不正确或已过期。"), actions, onClose = {})
            }
        }
        compose.waitForIdle()
        compose.onNodeWithText("配对码不正确或已过期。").assertExists()

        compose.onNodeWithText("重新配对").performClick()
        compose.waitForIdle()
        assertEquals("setup", prefs.privileged)
        // 第 3 步 is shown again and looks for the pairing service (and offers the pairing notification).
        assertEquals(listOf(true), service)

        // The code field takes focus on a tap, so the keyboard is requested again.
        val field = compose.onNode(hasSetTextAction())
        field.performClick()
        field.assertIsFocused()

        // Not six digits yet: 「配对并开启」 does nothing.
        field.performTextInput("12345")
        compose.onNodeWithText("配对并开启").performClick()
        assertEquals(emptyList<String>(), started)

        field.performTextInput("6")
        compose.onNodeWithText("配对并开启").performClick()
        compose.waitForIdle()
        assertEquals(listOf("123456"), started)
        assertEquals("starting", prefs.privileged)
        // Leaving 第 3 步 stops looking for the pairing service.
        assertEquals(listOf(true, false), service)
    }
}
