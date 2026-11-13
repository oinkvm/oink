package com.hachiwii.oink.control

import androidx.compose.ui.geometry.Rect
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Sliding between keys and several fingers (docs/control-page.md §3.1.4, user 2026-11-11): the
 * examples of the spec as key event sequences, plus the bubble owner and the hit test.
 */
class KeySliderTest {
    private val sent = ArrayList<Pair<Int, Boolean>>()
    private val kb = KeyboardModel(KeyboardKind.EMULATED, { 0L }, typedChar = UsKeys::typed) { code, down -> sent += code to down }
    private val feel = ArrayList<Boolean>()
    private val s = KeySlider { feel += it }

    private fun ch(c: Char) = SlideKey("vk-$c", hold = true, down = { kb.pressChar("vk-$c", c) }, up = { kb.release("vk-$c") })
    private fun hold(id: String, code: Int) = SlideKey(id, hold = true, down = { kb.press(id, code) }, up = { kb.release(id) })
    private fun tap(id: String, code: Int) = SlideKey(id, hold = false, fire = { kb.click(code) })
    private fun code(c: Char) = UsKeys.usChar(c)!!.first
    private fun d(c: Char) = code(c) to true
    private fun u(c: Char) = code(c) to false
    private fun on(k: SlideKey) = Hit.On(k)
    private val backspace = hold("backspace", Hid.BACKSPACE)
    private val home = tap("home", Hid.HOME)
    private val end = tap("end", Hid.END)
    private val del = tap("del", Hid.DELETE)
    private val left = hold("left", Hid.LEFT)

    @Test fun leftHoldsARightSlidesSToD() {
        val a = ch('a')
        s.press(1, a)
        s.press(2, ch('s')); s.move(2, on(ch('d'))); s.lift(2)
        s.lift(1)
        assertEquals(listOf(d('a'), d('s'), u('s'), d('d'), u('d'), u('a')), sent)
    }

    @Test fun twoFingersOnFBothSlideToG() {
        s.press(1, ch('f')); s.press(2, ch('f'))
        s.move(1, on(ch('g')))
        s.move(2, on(ch('g')))
        s.lift(1); s.lift(2)
        assertEquals(listOf(d('f'), d('g'), u('f'), u('g')), sent)
    }

    @Test fun heldCtrlReleasesWhenTheSlideEmptiesTheSet() {
        kb.tapModifier(Mod.CTRL)
        s.press(1, ch('a')); s.move(1, on(ch('s'))); s.lift(1)
        assertEquals(listOf(Hid.L_CTRL to true, d('a'), u('a'), Hid.L_CTRL to false, d('s'), u('s')), sent)
    }

    @Test fun heldCtrlStaysWhileAnotherFingerHoldsAKey() {
        kb.tapModifier(Mod.CTRL)
        s.press(1, ch('a'))
        s.press(2, ch('s')); s.lift(2)
        s.lift(1)
        assertEquals(listOf(Hid.L_CTRL to true, d('a'), d('s'), u('s'), u('a'), Hid.L_CTRL to false), sent)
    }

    @Test fun slideToBackspaceThenOffTheKeyboard() {
        s.press(1, ch('m')); s.move(1, on(backspace)); s.move(1, Hit.Out); s.lift(1)
        assertEquals(listOf(d('m'), u('m'), Hid.BACKSPACE to true, Hid.BACKSPACE to false), sent)
    }

    @Test fun afterLeavingTheKeyboardTheFingerPressesNothing() {
        s.press(1, ch('a')); s.move(1, Hit.Out); s.move(1, on(ch('s'))); s.lift(1)
        assertEquals(listOf(d('a'), u('a')), sent)
        assertNull(s.bubble)
    }

    @Test fun tapKeyCancelledByLeavingTheKeyboard() {
        s.press(1, home); s.move(1, Hit.Out); s.move(1, on(end)); s.lift(1)
        assertEquals(emptyList<Pair<Int, Boolean>>(), sent)
    }

    @Test fun slidingOverAModifierIsNoKeyNotTheEnd() {
        s.press(1, ch('z')); s.move(1, Hit.None); s.move(1, on(ch('x'))); s.lift(1)
        assertEquals(listOf(d('z'), u('z'), d('x'), u('x')), sent)
    }

    @Test fun tapKeysFireWhereTheFingerLifts() {
        s.press(1, home); s.move(1, on(end)); s.lift(1)
        assertEquals(listOf(Hid.END to true, Hid.END to false), sent)
    }

    @Test fun holdKeyThenTapKey() {
        s.press(1, left); s.move(1, on(del)); s.lift(1)
        assertEquals(listOf(Hid.LEFT to true, Hid.LEFT to false, Hid.DELETE to true, Hid.DELETE to false), sent)
    }

    @Test fun tapKeyFiresOnceWhenTheLastFingerLiftsOnIt() {
        s.press(1, home); s.press(2, home)
        s.lift(1)
        assertEquals(emptyList<Pair<Int, Boolean>>(), sent)
        s.lift(2)
        assertEquals(listOf(Hid.HOME to true, Hid.HOME to false), sent)
        sent.clear()
        // The last finger slides away: nothing.
        s.press(3, home); s.move(3, Hit.None); s.lift(3)
        assertEquals(emptyList<Pair<Int, Boolean>>(), sent)
    }

    @Test fun layoutChangeUnderAFingerLeavesItsKey() {
        s.press(1, ch('a'))
        s.relayout()
        assertEquals(listOf(d('a'), u('a')), sent)
        s.move(1, on(ch('1'))); s.lift(1)
        assertEquals(listOf(d('a'), u('a'), d('1'), u('1')), sent)
    }

    @Test fun bubbleFollowsTheNewestFingerOnly() {
        val a = ch('a'); val dKey = ch('d')
        s.press(1, a)
        assertSame(a, s.bubble!!.key)
        s.press(2, dKey)
        assertSame(dKey, s.bubble!!.key)
        s.move(1, on(ch('s')))
        assertSame(dKey, s.bubble!!.key) // another finger's slide does not move it
        s.move(2, on(ch('f')))
        assertTrue(s.bubble!!.glide)
        s.lift(2)
        assertNull(s.bubble) // no jump back to the older finger
        s.move(1, on(ch('q')))
        assertEquals("vk-q", s.bubble!!.key.id)
        assertEquals(false, s.bubble!!.glide) // appears again there
        assertEquals(setOf("vk-q"), s.down)
    }

    @Test fun hapticsOnlyWhenAKeyGoesDown() {
        s.press(1, ch('f')); s.press(2, ch('f')); s.move(2, on(ch('g'))); s.move(1, on(ch('g')))
        assertEquals(listOf(false, true), feel)
    }

    @Test fun hitTestGapsStickinessAndOutside() {
        val a = ch('a'); val sKey = ch('s')
        // Keys 30 wide with a 6 gap: a 0–30, s 36–66; Shift (no bubble) below.
        val keys = listOf(Rect(0f, 0f, 30f, 40f) to a, Rect(36f, 0f, 66f, 40f) to sKey, Rect(0f, 50f, 40f, 90f) to null)
        val area = listOf(Rect(0f, 0f, 100f, 100f))
        fun at(x: Float, y: Float, cur: String? = null) = KeySlider.locate(x, y, cur, keys, area, near = 8f, stick = 5f)
        assertSame(a, (at(31f, 20f) as Hit.On).key) // in the gap, nearer a
        assertSame(sKey, (at(35f, 20f) as Hit.On).key)
        assertSame(a, (at(34f, 20f, "vk-a") as Hit.On).key) // a keeps the finger until 5 past its edge
        assertSame(sKey, (at(36f, 20f, "vk-a") as Hit.On).key)
        assertEquals(Hit.None, at(10f, 60f)) // Shift
        assertEquals(Hit.None, at(90f, 20f)) // far from every key
        assertEquals(Hit.Out, at(50f, -1f))
    }
}
