package com.hachiwii.oink.control

/** The focus computer's text-input focus (`ControlTextFocus`, protocol-v2 §14.7). Never any text. */
data class PcTextFocusReport(val device: String, val seq: Long, val editable: Boolean, val secure: Boolean)

/** What the control page does with its keyboard after a text-input focus change. */
sealed interface PcTextFocusAction {
    data object None : PcTextFocusAction
    /** Pop the keyboard up: the Oink keyboard for a password field, else the remembered kind. */
    data class Open(val secure: Boolean) : PcTextFocusAction
    /** The popped-up keyboard follows focus to another field (password field ↔ ordinary field). */
    data class Follow(val secure: Boolean) : PcTextFocusAction
    /** Focus left the field: the popped-up keyboard hides. */
    data object Close : PcTextFocusAction
}

/**
 * The keyboard follows the focus computer's text fields (docs/control-page.md §9):
 * - an editable field gets focus → the keyboard pops up (the remembered kind; the Oink keyboard for
 *   a password field, without changing the remembered kind);
 * - focus leaves it (no editable element, no focus) → only a keyboard that popped up this way hides;
 *   a keyboard the user opened never does;
 * - the user hides a keyboard during an input focus → no pop-up until the next input focus (another
 *   field, or leaving and coming back: a new [PcTextFocusReport.seq]).
 * Only while the page is showing, and only for its current focus computer. The core sends every change
 * and the current state when the page opens.
 *
 * While the touchpad is in use (a finger down, the locked drag held) and until it has been idle for
 * [TouchpadModel.TOUCHPAD_IDLE_DELAY_MS] (1.2 s), nothing changes (user 2026-11-12): moving the pointer across to another computer must not
 * pop the keyboard up or hide it, which would resize the pad under the finger. A change that waits is
 * applied from the latest state once the pad is quiet; changes in between are never applied.
 */
class PcTextFocusGate {
    /**
     * 键盘自动弹出 ([ControlPrefs.keyboardAutoPopup], off by default, user 2026-11-12). Off: focus never pops
     * the keyboard up or hides it (so no password keyboard either); the user's own opening and hiding work
     * as always, and a keyboard popped up before it was turned off counts as the user's.
     */
    var autoPopup = false
    private data class Episode(val device: String, val seq: Long)
    private var latest: PcTextFocusReport? = null
    /** The keyboard is open because focus popped it up, for this episode and kind. */
    private var popped: Pair<Episode, Boolean>? = null
    private var suppressed: Episode? = null
    private var touching = false
    private var lastTouch = Long.MIN_VALUE / 2
    /** A decision waits for the touchpad to be quiet. */
    var pending = false; private set

    /** A report from the core; then [decide]. */
    fun record(report: PcTextFocusReport) { latest = report }

    fun report(report: PcTextFocusReport, focus: String?, active: Boolean, keyboardOpen: Boolean, now: Long): PcTextFocusAction {
        record(report)
        return decide(focus, active, keyboardOpen, now)
    }

    /**
     * The touchpad is in use ([busy]: a finger down, the locked drag held) or just stopped, at [now]
     * (uptime ms). Report every change of [busy]; activity while busy may be reported too.
     */
    fun touchpad(busy: Boolean, now: Long) {
        if (busy || touching) lastTouch = now
        touching = busy
    }

    /** When [decide] should run again for a waiting change; null when nothing waits or the pad is still in use. */
    fun retryAt(): Long? = if (pending && !touching) idleAt() else null

    private fun idleAt() = lastTouch + TouchpadModel.TOUCHPAD_IDLE_DELAY_MS

    /** After a report, a change of the focus computer or of the page's visibility, or at [retryAt]. */
    fun decide(focus: String?, active: Boolean, keyboardOpen: Boolean, now: Long): PcTextFocusAction {
        if (!autoPopup) {
            pending = false
            popped = null
            return PcTextFocusAction.None
        }
        if (touching || now < idleAt()) {
            pending = true
            return PcTextFocusAction.None
        }
        pending = false
        return evaluate(focus, active, keyboardOpen)
    }

    private fun evaluate(focus: String?, active: Boolean, keyboardOpen: Boolean): PcTextFocusAction {
        if (!keyboardOpen) popped = null
        if (!active) return PcTextFocusAction.None
        if (focus == null) return hidePopped()
        // Another computer just became the focus: wait for its state.
        val state = latest?.takeIf { it.device == focus } ?: return PcTextFocusAction.None
        if (!state.editable) return hidePopped()
        val episode = Episode(state.device, state.seq)
        popped?.let { (was, secure) ->
            if (was == episode && secure == state.secure) return PcTextFocusAction.None
            popped = episode to state.secure
            return PcTextFocusAction.Follow(state.secure)
        }
        if (keyboardOpen || suppressed == episode) return PcTextFocusAction.None
        popped = episode to state.secure
        return PcTextFocusAction.Open(state.secure)
    }

    private fun hidePopped(): PcTextFocusAction {
        if (popped == null) return PcTextFocusAction.None
        popped = null
        return PcTextFocusAction.Close
    }

    /** The user hid the keyboard (keyboard button, system dismissal): no pop-up again during this input focus. */
    fun userHid(focus: String?) {
        popped = null
        latest?.takeIf { it.device == focus && it.editable }?.let { suppressed = Episode(it.device, it.seq) }
    }

    /** The user opened the keyboard: focus never hides it. */
    fun userOpened() { popped = null }

}
