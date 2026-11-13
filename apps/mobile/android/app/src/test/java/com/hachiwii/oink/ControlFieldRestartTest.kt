package com.hachiwii.oink

import android.view.KeyEvent
import com.hachiwii.oink.control.*
import com.hachiwii.oink.ui.*
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import java.util.concurrent.Executor

/**
 * The system keyboard's hidden field mirrors the text in front of the focus computer's caret
 * (docs/control-page.md §3.1.1, user 2026-11-12): whatever else types there or moves that caret
 * starts it afresh; its own Backspace, modifiers and media keys do not.
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], application = android.app.Application::class)
class ControlFieldRestartTest {
    private val pc = "a".repeat(64)
    private val control = FakeControl(ControlState(focusDevice = pc, selectedDevice = pc, targets = listOf(ControlTarget(pc, "Windows PC", "windows", true))))
    private var restarts = 0
    private val holder = ControlHolder(null, ControlPrefs(keyboard = KeyboardKind.SYSTEM)) {}.also { h ->
        val now = Executor { it.run() }
        h.session = ControlSession(control, onFailure = {}, worker = now, main = now)
        h.active = true
        h.reevaluateTextFocus()
        h.restartField = { restarts++ }
    }

    @Test fun theFieldsOwnBackspaceModifiersAndMediaKeysKeepIt() {
        holder.fieldBackspace()
        holder.keyboard.tapModifier(Mod.SHIFT)
        holder.keyboard.media(Hid.VOLUME_UP)
        holder.hardwareKey(KeyEvent.KEYCODE_SHIFT_LEFT, true)
        assertEquals(0, restarts)
        assertTrue(control.sent.contains("key ${Integer.toHexString(Hid.BACKSPACE)} true"))
    }

    @Test fun thePagesKeysAndHardwareKeysRestartIt() {
        holder.keyboard.click(Hid.LEFT)
        assertEquals(1, restarts)
        holder.keyboard.shortcut(Hid.V, mac = false)
        assertEquals(2, restarts)
        holder.hardwareKey(KeyEvent.KEYCODE_A, true)
        holder.hardwareKey(KeyEvent.KEYCODE_A, false)
        assertEquals(3, restarts)
    }

    @Test fun anotherTextFieldOrComputerRestartsIt() {
        holder.pcTextFocus(PcTextFocusReport(pc, 1, editable = true, secure = false))
        assertEquals(1, restarts)
        holder.pcTextFocus(PcTextFocusReport(pc, 1, editable = true, secure = false))
        assertEquals("the same field again", 1, restarts)
        holder.pcTextFocus(PcTextFocusReport(pc, 2, editable = true, secure = false))
        assertEquals(2, restarts)
        holder.reevaluateTextFocus()
        assertEquals("the same computer", 2, restarts)
    }
}
