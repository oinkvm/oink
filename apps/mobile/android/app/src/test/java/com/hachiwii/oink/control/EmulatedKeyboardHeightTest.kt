package com.hachiwii.oink.control

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** The portrait emulated keyboard follows a normal system keyboard and ignores tall panels / floating IMEs (user 2026-11-12). */
class EmulatedKeyboardHeightTest {
    private val base = EmulatedKeyboardHeight.BASELINE_DP

    @Test fun baselineIsTheTestPhonesKeyboard() {
        // Phone, Sogou 9-key: 929 px inset − 44 px gesture bar at 440 dpi.
        assertEquals((929 - 44) / 2.75f, base, 0.5f)
    }

    @Test fun normalHeightsAreFollowed() {
        assertEquals(base, EmulatedKeyboardHeight.keys(null), 0f)
        assertEquals(base, EmulatedKeyboardHeight.keys(0f), 0f)
        // A candidate row more or less (±12 %).
        assertEquals(300f, EmulatedKeyboardHeight.keys(300f), 0f)
        assertEquals(355f, EmulatedKeyboardHeight.keys(355f), 0f)
        assertTrue(EmulatedKeyboardHeight.inRange(base * .88f + .01f))
        assertTrue(EmulatedKeyboardHeight.inRange(base * 1.12f - .01f))
    }

    @Test fun panelsAndFloatingKeyboardsKeepTheBaseline() {
        // Sogou 超级联想: about 510 dp above the gesture bar.
        assertFalse(EmulatedKeyboardHeight.inRange(510f))
        assertEquals(base, EmulatedKeyboardHeight.keys(510f), 0f)
        // A floating / split IME leaves little or no bottom inset.
        assertEquals(base, EmulatedKeyboardHeight.keys(60f), 0f)
    }
}
