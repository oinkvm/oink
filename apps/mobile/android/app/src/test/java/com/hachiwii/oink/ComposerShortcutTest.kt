package com.hachiwii.oink

import android.view.KeyEvent
import com.hachiwii.oink.ui.composerSendShortcut
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class ComposerShortcutTest {
    @Test fun controlEnterSendsOnceAndConsumesBothEdgesAndRepeats() {
        var sent = 0
        for (key in listOf(KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER)) {
            for ((action, repeat) in listOf(KeyEvent.ACTION_DOWN to 0, KeyEvent.ACTION_DOWN to 1, KeyEvent.ACTION_UP to 0)) {
                val event = KeyEvent(1, 2, action, key, repeat, KeyEvent.META_CTRL_ON or KeyEvent.META_CTRL_LEFT_ON)
                assertTrue(composerSendShortcut(event) { sent++ })
            }
        }
        assertEquals(2, sent)
    }
    @Test fun ordinaryEnterAndOtherChordsKeepTheirEditingBehavior() {
        for (meta in listOf(0, KeyEvent.META_SHIFT_ON, KeyEvent.META_META_ON, KeyEvent.META_CTRL_ON or KeyEvent.META_SHIFT_ON)) {
            assertFalse(composerSendShortcut(KeyEvent(1, 2, KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_ENTER, 0, meta)) { fail("unexpected send") })
        }
        assertFalse(composerSendShortcut(KeyEvent(1, 2, KeyEvent.ACTION_DOWN, KeyEvent.KEYCODE_A, 0, KeyEvent.META_CTRL_ON)) { fail("unexpected send") })
    }
}
