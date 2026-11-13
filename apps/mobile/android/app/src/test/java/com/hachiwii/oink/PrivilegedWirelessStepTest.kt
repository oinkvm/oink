package com.hachiwii.oink

import android.provider.Settings
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.platform.LocalContext
import androidx.compose.foundation.text.BasicText
import com.hachiwii.oink.ui.*
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/**
 * 特权模式's guide, 第 2 步 「打开无线调试」 (user 2026-11-12): after a reboot turned wireless debugging
 * off, the step showed as checked. It follows the system switch now: off → unchecked and active,
 * on → checked (第 3 步 active), off again → unchecked.
 */
@RunWith(RobolectricTestRunner::class)
@GraphicsMode(GraphicsMode.Mode.NATIVE)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class PrivilegedWirelessStepTest {
    @get:Rule val compose = createComposeRule()

    private fun stepTwoActive() {
        // Step 2 shows its number (not the check) and its own button; 第 3 步's field is not shown.
        compose.onNodeWithText("2").assertExists()
        compose.onNodeWithText("打开开发者选项").assertExists()
        compose.onNodeWithText("分屏时在这里输入").assertDoesNotExist()
    }

    private fun stepTwoChecked() {
        compose.onNodeWithText("2").assertDoesNotExist()
        compose.onNodeWithText("打开开发者选项").assertDoesNotExist()
        compose.onNodeWithText("分屏时在这里输入").assertExists()
    }

    @Test fun stepTwoFollowsTheSystemSwitch() {
        var wireless by mutableStateOf(false)
        compose.setContent {
            OinkTheme(OinkColors.of(false, Accent.Pink)) {
                // privStep = 3: where 「重新配对」 (or an earlier tap on 「打开开发者选项」) left the guide.
                SettingsPage(UiInit(settingsStack = listOf("main", "privileged"), privStep = 3), Prefs(scheme = "light", privileged = "setup"), {},
                    AppModel(developerOptionsOn = true, wirelessDebuggingOn = wireless), AppActions(), onClose = {})
            }
        }
        compose.waitForIdle()
        stepTwoActive()

        wireless = true
        compose.waitForIdle()
        stepTwoChecked()

        wireless = false
        compose.waitForIdle()
        stepTwoActive()
    }

    @Test fun theSwitchesAreReadLiveFromTheSystem() {
        val app = RuntimeEnvironment.getApplication().contentResolver
        Settings.Global.putInt(app, Settings.Global.DEVELOPMENT_SETTINGS_ENABLED, 1)
        Settings.Global.putInt(app, ADB_WIFI_ENABLED, 0)
        // The resolver the composable observes (the Activity's own).
        lateinit var resolver: android.content.ContentResolver
        compose.setContent {
            resolver = LocalContext.current.contentResolver
            val switches = rememberDebugSwitches()
            BasicText("wireless=${switches.wirelessDebugging} developer=${switches.developerOptions}")
        }
        compose.onNodeWithText("wireless=false developer=true").assertExists()

        fun wireless(on: Boolean) {
            Settings.Global.putInt(resolver, ADB_WIFI_ENABLED, if (on) 1 else 0)
            // The settings provider notifies observers of the key's URI on every write.
            resolver.notifyChange(Settings.Global.getUriFor(ADB_WIFI_ENABLED), null)
            compose.waitForIdle()
        }
        wireless(true)
        compose.onNodeWithText("wireless=true developer=true").assertExists()
        wireless(false)
        compose.onNodeWithText("wireless=false developer=true").assertExists()
    }

    @Test fun liveSwitchesOverrideTheRememberedStep() {
        // Developer options off: always 第 1 步.
        assertEquals(1, privilegedSetupStep(3, developerOptions = false, wirelessDebugging = true))
        // Wireless debugging off: never past 第 2 步, also when 「重新配对」 asked for 3.
        assertEquals(2, privilegedSetupStep(3, developerOptions = true, wirelessDebugging = false))
        assertEquals(2, privilegedSetupStep(1, developerOptions = true, wirelessDebugging = false))
        // Both on: the pairing code.
        assertEquals(3, privilegedSetupStep(1, developerOptions = true, wirelessDebugging = true))
        // Unknown (design scenes, unreadable setting): the step the user's taps chose.
        assertEquals(3, privilegedSetupStep(3, developerOptions = true, wirelessDebugging = null))
        assertEquals(2, privilegedSetupStep(1, developerOptions = true, wirelessDebugging = null))
        assertEquals(1, privilegedSetupStep(1, developerOptions = null, wirelessDebugging = null))
        assertEquals(2, privilegedSetupStep(3, developerOptions = null, wirelessDebugging = false))
    }
}
