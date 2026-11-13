package com.hachiwii.oink

import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.performTouchInput
import com.hachiwii.oink.control.*
import com.hachiwii.oink.ui.*
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * Modifier keys on the real keyboard (user 2026-11-12): a long press locks, a double tap is one
 * press and release of the modifier.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class ControlModifierKeysTest {
    @get:Rule val compose = createComposeRule()
    private val pc = "a".repeat(64)

    private fun open(): FakeControl {
        val fake = FakeControl(ControlState(focusDevice = pc, selectedDevice = pc, targets = listOf(ControlTarget(pc, "Windows PC", "windows", true))))
        val model = AppModel(selfName = "Phone", peers = listOf(PeerUi(pc, "Windows PC", "windows", latency = 4.0)))
        compose.setContent {
            OinkApp(model, UiInit(tab = Tab.Control, freeze = true, control = ControlInit(keyboard = KeyboardKind.EMULATED)), Prefs(scheme = "light"), {}, AppActions(control = fake), false)
        }
        compose.waitForIdle()
        fake.sent.clear()
        return fake
    }

    private fun keys(fake: FakeControl) = fake.sent.filter { it.startsWith("key") }
    private fun key(code: Int, down: Boolean) = "key ${Integer.toHexString(code)} $down"

    @Test
    fun longPressLocksTheModifier() {
        val fake = open()
        compose.onNodeWithContentDescription("Ctrl").performTouchInput { down(center); advanceEventTime(700); up() }
        compose.waitForIdle()
        compose.onNodeWithContentDescription("Ctrl（锁定）").assertExists()
        assertEquals(listOf(key(Hid.L_CTRL, true)), keys(fake))
        // A key does not release a locked modifier.
        compose.onNodeWithContentDescription("a").performTouchInput { down(center); up() }
        compose.waitForIdle()
        compose.onNodeWithContentDescription("Ctrl（锁定）").assertExists()
        // A tap unlocks it.
        compose.onNodeWithContentDescription("Ctrl（锁定）").performTouchInput { down(center); up() }
        compose.waitForIdle()
        compose.onNodeWithContentDescription("Ctrl").assertExists()
        assertEquals(key(Hid.L_CTRL, false), keys(fake).last())
    }

    @Test
    fun doubleTapPressesAndReleasesTheModifierOnce() {
        val fake = open()
        compose.onNodeWithContentDescription("Alt").performTouchInput {
            down(center); up()
            advanceEventTime(100)
            down(center); up()
        }
        compose.waitForIdle()
        compose.onNodeWithContentDescription("Alt").assertExists()
        assertEquals(listOf(key(Hid.L_ALT, true), key(Hid.L_ALT, false)), keys(fake))
    }
}
