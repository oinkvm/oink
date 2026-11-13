package com.hachiwii.oink

import com.hachiwii.oink.control.*
import com.hachiwii.oink.ui.*
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import java.util.concurrent.Executor

/**
 * The control page's keyboard follows the focus computer's text fields (docs/control-page.md §9,
 * protocol-v2 §14.7): pops up for a field, hides when focus leaves it unless the user opened it,
 * stays hidden after the user hid it until the next input focus, and a password field gets the
 * Oink keyboard without changing the remembered kind.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], application = android.app.Application::class)
class ControlTextFocusTest {
    private val pc = "a".repeat(64)
    private val mac = "b".repeat(64)
    private fun r(seq: Long, editable: Boolean, secure: Boolean = false, device: String = pc) =
        PcTextFocusReport(device, seq, editable, secure)

    /** [autoPopup]: 键盘自动弹出, on unless a test checks the default. */
    private fun holder(autoPopup: Boolean = true): ControlHolder {
        val holder = ControlHolder(null, ControlPrefs(keyboard = KeyboardKind.SYSTEM, keyboardAutoPopup = autoPopup)) {}
        val now = Executor { it.run() }
        val state = ControlState(focusDevice = pc, selectedDevice = pc, targets = listOf(ControlTarget(pc, "Windows PC", "windows", true)))
        holder.session = ControlSession(FakeControl(state), onFailure = {}, worker = now, main = now)
        holder.active = true
        return holder
    }

    @Test fun keyboardAutoPopupIsOffByDefaultAndFocusThenNeverChangesTheKeyboard() {
        assertFalse(ControlPrefs().keyboardAutoPopup)
        val holder = holder(autoPopup = false)
        holder.pcTextFocus(r(1, true))
        assertFalse(holder.keyboardOpen)
        holder.pcTextFocus(r(2, true, secure = true))
        assertFalse(holder.keyboardOpen)
        // Manual opening is unaffected, and focus leaving does not hide it.
        holder.toggleKeyboard()
        holder.pcTextFocus(r(2, false))
        assertTrue(holder.keyboardOpen)
        holder.toggleKeyboard()
        // Turning the setting on (the 调节 switch) applies at once to the focused field.
        holder.pcTextFocus(r(3, true))
        holder.update(holder.prefs.copy(keyboardAutoPopup = true))
        assertTrue(holder.keyboardOpen)
    }

    @Test fun popsUpForAFieldAndHidesWhenFocusLeaves() {
        val holder = holder()
        holder.pcTextFocus(r(1, true, device = mac))
        assertFalse("another computer's field", holder.keyboardOpen)
        holder.pcTextFocus(r(1, true))
        assertTrue(holder.keyboardOpen)
        assertEquals(KeyboardKind.SYSTEM, holder.keyboard.shown)
        holder.pcTextFocus(r(1, false))
        assertFalse("focus left the field", holder.keyboardOpen)
    }

    @Test fun aKeyboardHiddenWithTheButtonStaysHiddenUntilTheNextField() {
        val holder = holder()
        holder.pcTextFocus(r(1, true))
        holder.toggleKeyboard()
        assertFalse(holder.keyboardOpen)
        holder.reevaluateTextFocus()
        assertFalse("hidden by the user", holder.keyboardOpen)
        holder.pcTextFocus(r(2, true))
        assertTrue("another field", holder.keyboardOpen)
    }

    @Test fun aKeyboardOpenedWithTheButtonNeverHidesWithFocus() {
        val holder = holder()
        holder.toggleKeyboard()
        holder.pcTextFocus(r(1, true))
        holder.pcTextFocus(r(1, false))
        assertTrue(holder.keyboardOpen)
    }

    @Test fun aPasswordFieldGetsTheOinkKeyboardWithoutChangingThePreference() {
        val holder = holder()
        holder.pcTextFocus(r(1, true, secure = true))
        assertTrue(holder.keyboardOpen)
        assertEquals(KeyboardKind.EMULATED, holder.keyboard.shown)
        assertEquals(KeyboardKind.SYSTEM, holder.keyboard.preferred)
        holder.pcTextFocus(r(1, false))
        assertFalse(holder.keyboardOpen)
        holder.pcTextFocus(r(2, true))
        assertEquals("the next field follows the preference again", KeyboardKind.SYSTEM, holder.keyboard.shown)
        // Tab into a password field while popped up: the Oink keyboard; the switch key goes back.
        holder.pcTextFocus(r(3, true, secure = true))
        assertEquals(KeyboardKind.EMULATED, holder.keyboard.shown)
        holder.keyboard.switchKind()
        assertEquals(KeyboardKind.SYSTEM, holder.keyboard.shown)
        assertEquals(KeyboardKind.SYSTEM, holder.keyboard.preferred)
    }

    @Test fun keyboardChangesWaitUntilTheTouchpadHasBeenIdleFor1200Ms() {
        val holder = holder()
        var t = 50_000L
        holder.now = { t }
        holder.touchpadBusy(true)
        holder.pcTextFocus(r(1, true))
        assertFalse("finger on the pad", holder.keyboardOpen)
        assertEquals(null, holder.textFocusRetryAt)
        t += 300
        holder.touchpadBusy(false)
        assertEquals(t + TouchpadModel.TOUCHPAD_IDLE_DELAY_MS, holder.textFocusRetryAt)
        t += 1199
        holder.reevaluateTextFocus()
        assertFalse(holder.keyboardOpen)
        t += 1
        holder.reevaluateTextFocus()
        assertTrue(holder.keyboardOpen)
        assertEquals(null, holder.textFocusRetryAt)
        // Idle pad: the next change applies at once.
        holder.pcTextFocus(r(1, false))
        assertFalse(holder.keyboardOpen)
    }

    @Test fun nothingPopsUpWhileThePageIsNotShowing() {
        val holder = holder()
        holder.active = false
        holder.pcTextFocus(r(1, true))
        assertFalse(holder.keyboardOpen)
    }
}
