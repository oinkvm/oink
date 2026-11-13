package com.hachiwii.oink

import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithContentDescription
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.unit.dp
import com.hachiwii.oink.control.*
import com.hachiwii.oink.ui.*
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * Sliding between keys on the real emulated keyboard (docs/control-page.md §3.1.4): the keys hand
 * their touches to the keyboard's tracker, so a finger sliding from a to s sends a up, then s down.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], qualifiers = "w412dp-h915dp-xxhdpi", application = android.app.Application::class)
class ControlKeySlideTest {
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

    private fun key(c: Char, down: Boolean) = "key ${Integer.toHexString(UsKeys.usChar(c)!!.first)} $down"

    @Test
    fun capsSendsDownBeforeLiftAndUpImmediately() {
        val fake = open()
        val caps = compose.onNodeWithContentDescription("Caps Lock")
        caps.performTouchInput { down(center) }
        compose.waitForIdle()
        assertEquals(listOf("key ${Integer.toHexString(Hid.CAPS_LOCK)} true"), fake.sent.filter { it.startsWith("key") })
        caps.performTouchInput { advanceEventTime(60); up() }
        compose.waitForIdle()
        assertEquals(listOf("key ${Integer.toHexString(Hid.CAPS_LOCK)} true", "key ${Integer.toHexString(Hid.CAPS_LOCK)} false"), fake.sent.filter { it.startsWith("key") })
    }

    @Test
    fun capsSlideOffThenLiftDoesNotStickOrRetap() {
        val fake = open()
        compose.onNodeWithContentDescription("Caps Lock").performTouchInput {
            down(center)
            moveTo(center - Offset(300.dp.toPx(), 0f))
            moveTo(center)
            up()
        }
        compose.waitForIdle()
        assertEquals(listOf("key ${Integer.toHexString(Hid.CAPS_LOCK)} true", "key ${Integer.toHexString(Hid.CAPS_LOCK)} false"), fake.sent.filter { it.startsWith("key") })
    }

    @Test
    fun slideFromAToSThenLift() {
        val fake = open()
        compose.onNodeWithContentDescription("a").performTouchInput {
            down(center)
            // s is the next key to the right (key width + 6 gap).
            moveTo(center + Offset(width / 2f, 0f))
            moveTo(center + Offset(width + 6.dp.toPx(), 0f))
            up()
        }
        compose.waitForIdle()
        assertEquals(listOf(key('a', true), key('a', false), key('s', true), key('s', false)), fake.sent.filter { it.startsWith("key") })
    }

    @Test
    fun slideOffTheKeyboardEndsTheFinger() {
        val fake = open()
        compose.onNodeWithContentDescription("Esc").performTouchInput {
            down(center)
            moveTo(center - Offset(0f, 200.dp.toPx())) // up onto the pad: Esc cancelled
            moveTo(center)                            // back on Esc: presses nothing any more
            up()
        }
        compose.waitForIdle()
        assertEquals(emptyList<String>(), fake.sent.filter { it.startsWith("key") })
        compose.onNodeWithContentDescription("Esc").performTouchInput { down(center); up() }
        compose.waitForIdle()
        assertEquals(listOf("key ${Integer.toHexString(Hid.ESCAPE)} true", "key ${Integer.toHexString(Hid.ESCAPE)} false"), fake.sent.filter { it.startsWith("key") })
    }
}
