package com.hachiwii.oink.control

import org.junit.Assert.assertEquals
import org.junit.Test
import java.util.concurrent.Executor

/** The keyboard follows the focus computer's text fields (docs/control-page.md §9, protocol-v2 §14.7). */
class PcTextFocusTest {
    private val pc = "a".repeat(64)
    private val mac = "b".repeat(64)
    private fun r(seq: Long, editable: Boolean, secure: Boolean = false, device: String = pc) =
        PcTextFocusReport(device, seq, editable, secure)

    /** A gate with the page's keyboard state, applying each action like the page does. */
    private class Page {
        /** 键盘自动弹出 turned on (the default is off: [withAutoPopupOffFocusNeverChangesTheKeyboard]). */
        var gate = PcTextFocusGate().apply { autoPopup = true }
        var open = false
        var secure = false
        var focus: String? = "a".repeat(64)
        var active = true
        /** Uptime ms; the pad has never been touched. */
        var t = 100_000L
        fun apply(action: PcTextFocusAction): PcTextFocusAction {
            when (action) {
                PcTextFocusAction.None -> {}
                is PcTextFocusAction.Open -> { open = true; secure = action.secure }
                is PcTextFocusAction.Follow -> secure = action.secure
                PcTextFocusAction.Close -> { open = false; secure = false }
            }
            return action
        }
        fun report(report: PcTextFocusReport) = apply(gate.report(report, focus, active, open, t))
        fun evaluate() = apply(gate.decide(focus, active, open, t))
        fun touchpad(busy: Boolean) = gate.touchpad(busy, t)
        fun userHide() { gate.userHid(focus); open = false; secure = false }
        fun userOpen() { gate.userOpened(); open = true }
    }

    @Test fun withAutoPopupOffFocusNeverChangesTheKeyboard() {
        val page = Page()
        page.gate = PcTextFocusGate()
        assertEquals("off by default", false, page.gate.autoPopup)
        assertEquals(PcTextFocusAction.None, page.report(r(1, true)))
        assertEquals("no password keyboard either", PcTextFocusAction.None, page.report(r(2, true, secure = true)))
        assertEquals(null, page.gate.retryAt())
        // The user's own keyboard: focus leaving does not hide it.
        page.userOpen()
        assertEquals(PcTextFocusAction.None, page.report(r(2, false)))
        assertEquals(true, page.open)
        page.userHide()
        // Turned on while a field has focus: it pops up (the user hid none during this focus).
        page.gate.autoPopup = true
        assertEquals(PcTextFocusAction.Open(false), page.report(r(3, true)))
        // Turned off again: that keyboard is the user's now and focus no longer hides it.
        page.gate.autoPopup = false
        assertEquals(PcTextFocusAction.None, page.report(r(3, false)))
        page.gate.autoPopup = true
        assertEquals(PcTextFocusAction.None, page.evaluate())
        assertEquals(true, page.open)
    }

    @Test fun aFieldPopsTheKeyboardUpAndLeavingHidesIt() {
        val page = Page()
        assertEquals(PcTextFocusAction.Open(false), page.report(r(1, true)))
        assertEquals(PcTextFocusAction.None, page.evaluate())
        assertEquals(PcTextFocusAction.Close, page.report(r(1, false)))
        assertEquals(false, page.open)
    }

    @Test fun aKeyboardTheUserHidStaysHiddenUntilTheNextInputFocus() {
        val page = Page()
        page.report(r(1, true))
        page.userHide()
        assertEquals(PcTextFocusAction.None, page.evaluate())
        assertEquals(PcTextFocusAction.None, page.report(r(1, true)))
        // Leaving and coming back is a new input focus.
        page.report(r(1, false))
        assertEquals(PcTextFocusAction.Open(false), page.report(r(2, true)))
    }

    @Test fun aKeyboardTheUserOpenedNeverHidesWithFocus() {
        val page = Page()
        page.userOpen()
        assertEquals(PcTextFocusAction.None, page.report(r(1, true)))
        assertEquals(PcTextFocusAction.None, page.report(r(1, false)))
        assertEquals(true, page.open)
        // Even a keyboard that popped up becomes the user's once they reopen it themselves.
        page.userHide()
        page.report(r(2, true))
        page.userHide()
        page.userOpen()
        assertEquals(PcTextFocusAction.None, page.report(r(2, false)))
        assertEquals(true, page.open)
    }

    @Test fun aPasswordFieldPopsTheOinkKeyboardAndTheNextFieldFollowsThePreference() {
        val page = Page()
        assertEquals(PcTextFocusAction.Open(true), page.report(r(1, true, secure = true)))
        page.report(r(1, false))
        assertEquals(PcTextFocusAction.Open(false), page.report(r(2, true)))
        // Tab from a user name to a password field while popped up: the keyboard follows.
        assertEquals(PcTextFocusAction.Follow(true), page.report(r(3, true, secure = true)))
        assertEquals(PcTextFocusAction.Follow(false), page.report(r(4, true)))
    }

    @Test fun onlyTheFocusComputerAndOnlyWhileThePageShows() {
        val page = Page()
        assertEquals(PcTextFocusAction.None, page.report(r(1, true, device = mac)))
        page.active = false
        assertEquals(PcTextFocusAction.None, page.report(r(1, true)))
        // The page shows again while that field still has focus: the keyboard pops up.
        page.active = true
        assertEquals(PcTextFocusAction.Open(false), page.evaluate())
    }

    @Test fun focusMovingToAnotherComputerWaitsForItsState() {
        val page = Page()
        page.report(r(1, true))
        page.focus = mac
        assertEquals(PcTextFocusAction.None, page.evaluate())
        assertEquals(PcTextFocusAction.Close, page.report(r(7, false, device = mac)))
        page.focus = null
        assertEquals(PcTextFocusAction.None, page.evaluate())
    }

    @Test fun aChangeWhileTheTouchpadIsIdleAppliesAtOnce() {
        val page = Page()
        page.touchpad(true); page.t += 50; page.touchpad(false)
        page.t += TouchpadModel.TOUCHPAD_IDLE_DELAY_MS
        assertEquals(PcTextFocusAction.Open(false), page.report(r(1, true)))
        assertEquals(null, page.gate.retryAt())
    }

    @Test fun aChangeDuringTouchWaitsUntilThePadHasBeenIdleFor1200Ms() {
        val page = Page()
        page.touchpad(true)
        assertEquals(PcTextFocusAction.None, page.report(r(1, true)))
        assertEquals("no retry while a finger is down", null, page.gate.retryAt())
        page.t += 400; page.touchpad(true)
        page.t += 300; page.touchpad(false)
        val lift = page.t
        assertEquals(lift + TouchpadModel.TOUCHPAD_IDLE_DELAY_MS, page.gate.retryAt())
        page.t += 1199
        assertEquals(PcTextFocusAction.None, page.evaluate())
        page.t = lift + TouchpadModel.TOUCHPAD_IDLE_DELAY_MS
        assertEquals(PcTextFocusAction.Open(false), page.evaluate())
        assertEquals(null, page.gate.retryAt())
        // Hiding waits too: the pointer crossed to another computer without a field.
        page.touchpad(true)
        page.focus = mac
        assertEquals(PcTextFocusAction.None, page.report(r(4, false, device = mac)))
        page.touchpad(false)
        page.t += TouchpadModel.TOUCHPAD_IDLE_DELAY_MS
        assertEquals(PcTextFocusAction.Close, page.evaluate())
    }

    @Test fun onlyTheLatestOfTheChangesDuringTheWaitApplies() {
        val page = Page()
        page.touchpad(true)
        // Across three computers while the finger is down: a field, none, a password field.
        page.report(r(1, true))
        page.focus = mac
        page.report(r(2, false, device = mac))
        page.focus = pc
        page.report(r(3, true, secure = true))
        page.touchpad(false)
        page.t += TouchpadModel.TOUCHPAD_IDLE_DELAY_MS
        assertEquals(PcTextFocusAction.Open(true), page.evaluate())
        // A field gained and left again during the wait: nothing happens at all.
        page.report(r(3, false))
        assertEquals(false, page.open)
        page.touchpad(true)
        page.report(r(4, true))
        page.report(r(4, false))
        page.touchpad(false)
        page.t += TouchpadModel.TOUCHPAD_IDLE_DELAY_MS
        assertEquals(PcTextFocusAction.None, page.evaluate())
        assertEquals(false, page.open)
    }

    @Test fun theSessionHandsTextFocusToThePageUntilItCloses() {
        var listener: ControlListener? = null
        val opener = ControlOpener { l ->
            listener = l
            object : ControlLink {
                override fun setSettings(pointerSpeed: Int, pointsPerMm: Float) {}
                override fun pointerMove(dx: Float, dy: Float, timeUs: Long, sinceUs: Long) {}
                override fun button(button: ControlButton, down: Boolean) {}
                override fun scroll(phase: PadScrollPhase, dx: Float, dy: Float, timeUs: Long, sinceUs: Long) {}
                override fun touches(timeUs: Long, width: Float, height: Float, touches: List<PadContact>, cancelled: Boolean) {}
                override fun gesture(gesture: PadGesture) {}
                override fun key(code: Int, down: Boolean) {}
                override fun inputText(text: String) {}
                override fun movePointerTo(device: String) {}
                override fun setFocusLock(locked: Boolean) {}
                override fun setGameMode(mode: ControlGameMode) {}
                override fun close() {}
                override fun typedChar(layout: String, targetMac: Boolean, ch: Char, mods: Set<Mod>) = UsKeys.typed(layout, targetMac, ch, mods)
            }
        }
        val now = Executor { it.run() }
        val seen = ArrayList<PcTextFocusReport>()
        val session = ControlSession(opener, onFailure = {}, worker = now, main = now, textFocused = { seen += it })
        listener!!.onTextFocus(r(1, true))
        assertEquals(listOf(r(1, true)), seen)
        session.close()
        listener!!.onTextFocus(r(2, true))
        assertEquals(listOf(r(1, true)), seen)
    }
}
