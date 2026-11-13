package com.hachiwii.oink.control

import android.os.Handler
import android.os.Looper
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue

enum class Mod(val code: Int) { SHIFT(Hid.L_SHIFT), CTRL(Hid.L_CTRL), ALT(Hid.L_ALT), META(Hid.L_GUI) }
enum class Latch { HELD, LOCKED }
enum class KeyboardKind { SYSTEM, EMULATED }

/** Runs [run] once after [ms] on the thread the keyboard lives on; the returned function cancels it. */
fun interface RepeatClock {
    fun after(ms: Long, run: () -> Unit): () -> Unit

    companion object {
        /** No repeats (design scenes and tests that do not drive time). */
        val NONE = RepeatClock { _, _ -> {} }

        /** The main thread, where the keyboard's touches arrive. */
        fun main(): RepeatClock {
            val handler = Handler(Looper.getMainLooper())
            return RepeatClock { ms, run ->
                val r = Runnable { run() }
                handler.postDelayed(r, ms)
                val cancel: () -> Unit = { handler.removeCallbacks(r) }
                cancel
            }
        }
    }
}

/**
 * Keys and modifiers of the control page (docs/control-page.md §3.1): the three-state modifiers
 * shared by both keyboards, the Fn layer, the temporary switch to the emulated keyboard while a
 * modifier is down, and when each key goes out. [send] gets (code, down) in the order the focus
 * computer must see them; [text] gets characters the chosen layout has no key for; [typedChar]
 * says what a character key sends ([ControlLink.typedChar]; null while there is no session).
 *
 * A repeatable key held down repeats like a hardware key (user 2026-11-12): after [REPEAT_DELAY_MS] the newest
 * held key goes down again every [REPEAT_INTERVAL_MS] until it goes up. The phone's core turns each
 * further "down" of a key that is already down into a `KeyRepeat` for the focus computer, as it does
 * for a desktop's own auto-repeat, so the computer injects the repeats.
 */
class KeyboardModel(
    initialKind: KeyboardKind = KeyboardKind.SYSTEM,
    private val now: () -> Long = { android.os.SystemClock.uptimeMillis() },
    private val repeat: RepeatClock = RepeatClock.NONE,
    private val text: (String) -> Unit = {},
    private val typedChar: (layout: String, targetMac: Boolean, ch: Char, mods: Set<Mod>) -> TypedChar? = { _, _, _, _ -> null },
    private val send: (Int, Boolean) -> Unit,
) {
    /** The user's own choice (the switch key), remembered by the page. Modifiers never change it. */
    var preferred by mutableStateOf(initialKind); private set
    val mods = mutableStateMapOf<Mod, Latch>()
    var fn by mutableStateOf<Latch?>(null); private set
    /** Emulated keyboard: numbers and symbols instead of letters. */
    var symbols by mutableStateOf(false)
    /** 调节 › 键盘布局 (the setting's id, see [ControlLink.typedChar]) and whether the focus computer is a Mac; set by the page. */
    var layout = "auto"
    var targetMac = false

    /**
     * A password field popped the keyboard up (docs/control-page.md §9): the emulated keyboard is shown
     * whatever the remembered kind, so the password never goes through the phone's input method.
     * Cleared when the keyboard closes or the switch key is tapped; the remembered kind is unchanged.
     */
    var secure by mutableStateOf(false)
    /** Over the system keyboard a modifier needs raw key codes, so the emulated keyboard stands in until all are up. */
    val shown: KeyboardKind get() =
        if (secure || (preferred == KeyboardKind.SYSTEM && mods.isNotEmpty())) KeyboardKind.EMULATED else preferred
    val shiftDown: Boolean get() = mods[Mod.SHIFT] != null

    /** When each modifier was last tapped, while its next tap would complete a double tap. */
    private val modTap = HashMap<Mod, Long>()
    private var fnTap = Long.MIN_VALUE / 2
    /** Keys sent down and not yet up, by the finger / key id that pressed them. */
    private class Held(val code: Int, val autoShift: Boolean, val autoAlt: Int?, val combines: Boolean)
    private val held = LinkedHashMap<Any, Held>()
    /** The key that repeats (the newest held one) and how to stop its timer. */
    private var repeating: Any? = null
    private var stopTimer: () -> Unit = {}

    /**
     * Tap (user 2026-11-12): released → held (down now, up after the next key); held or locked →
     * released. A second tap within [DOUBLE_TAP_MS] completes a double tap, which is the modifier
     * pressed and released once: from held it is the release, from released it is down + up.
     * Locking is [lockModifier] (long press).
     */
    fun tapModifier(mod: Mod) {
        val t = now()
        val quick = modTap[mod]?.let { t - it <= DOUBLE_TAP_MS } == true
        when (mods[mod]) {
            null -> if (quick) {
                modTap.remove(mod)
                send(mod.code, true); send(mod.code, false)
            } else {
                modTap[mod] = t
                mods[mod] = Latch.HELD; send(mod.code, true)
            }
            Latch.HELD -> {
                mods.remove(mod); send(mod.code, false)
                if (quick) modTap.remove(mod) else modTap[mod] = t
            }
            Latch.LOCKED -> {
                mods.remove(mod); send(mod.code, false)
                modTap[mod] = t
            }
        }
    }

    /** Long press (user 2026-11-12): released or held → locked; locked → released. */
    fun lockModifier(mod: Mod) {
        modTap.remove(mod)
        when (mods[mod]) {
            null -> { mods[mod] = Latch.LOCKED; send(mod.code, true) }
            Latch.HELD -> mods[mod] = Latch.LOCKED
            Latch.LOCKED -> { mods.remove(mod); send(mod.code, false) }
        }
    }

    /** Fn only switches the local F-key layer: tap = until one F key, double tap = locked, tap again = off. */
    fun tapFn() {
        val t = now()
        val quick = t - fnTap <= DOUBLE_TAP_MS
        fnTap = t
        fn = when (fn) { null -> Latch.HELD; Latch.HELD -> if (quick) Latch.LOCKED else null; Latch.LOCKED -> null }
    }

    /** After a non-modifier key (or a touchpad click) the held modifiers let go; locked ones stay. */
    fun releaseHeld() {
        Mod.entries.filter { mods[it] == Latch.HELD }.forEach { mods.remove(it); modTap.remove(it); send(it.code, false) }
    }

    private fun releaseAll() {
        held.keys.toList().forEach { release(it) }
        stopRepeat()
        capsLock(false)
        Mod.entries.filter { mods[it] != null }.forEach { mods.remove(it); send(it.code, false) }
        modTap.clear()
    }

    /**
     * The switch key. During the temporary emulated keyboard it means "back to the system keyboard"
     * (modifiers released, nothing remembered); otherwise it flips the remembered choice, and going
     * to the system keyboard releases the modifiers since they would bring the emulated one back.
     */
    fun switchKind() {
        // The password keyboard means "to the system keyboard" like the emulated one.
        if (secure) {
            secure = false
            if (preferred == KeyboardKind.SYSTEM) { releaseAll(); return }
        }
        if (preferred == KeyboardKind.SYSTEM) {
            if (mods.isNotEmpty()) { releaseAll(); return }
            preferred = KeyboardKind.EMULATED
        } else {
            preferred = KeyboardKind.SYSTEM
            releaseAll()
        }
    }

    /**
     * The page turned between portrait and landscape (user 2026-11-12): the kind remembered for the new
     * orientation takes over in place, like the switch key — going to the system keyboard releases the
     * modifiers (they would bring the emulated one straight back).
     */
    fun restore(kind: KeyboardKind) {
        if (kind == preferred) return
        preferred = kind
        if (kind == KeyboardKind.SYSTEM) releaseAll()
    }

    /**
     * Caps follows the real touch (user 2026-11-13): down on entry, up on lift, slide-off or
     * cancellation. Always sends Caps, without the PC's tap/hold mapping. It does not repeat,
     * take/release modifiers, or interrupt another key's repeat.
     */
    fun capsLock(down: Boolean) {
        if (capsDown == down) return
        capsDown = down
        send(Hid.CAPS_LOCK, down)
    }

    private var capsDown = false

    /** Keyboard closed: everything goes up and the F-key layer returns to default. */
    fun closed() {
        releaseAll()
        fn = null
        secure = false
    }

    /**
     * Held modifiers let go once no key that combines with them is down any more (user 2026-11-11,
     * several fingers): after the up that empties the set, or after a click with nothing down.
     */
    private fun releaseHeldIfIdle() {
        if (held.values.none { it.combines }) releaseHeld()
    }

    /** Control-area key that goes out on release: down + up together, then held modifiers release. */
    fun click(code: Int) {
        send(code, true); send(code, false)
        releaseHeldIfIdle()
    }

    fun fKey(n: Int) {
        click(Hid.f(n))
        if (fn == Latch.HELD) fn = null
    }

    /** Copy / paste: Ctrl+C / ⌘C merged with the modifiers already down. */
    fun shortcut(letter: Int, mac: Boolean) {
        val mod = if (mac) Mod.META else Mod.CTRL
        val wrap = mods[mod] == null
        if (wrap) send(mod.code, true)
        send(letter, true); send(letter, false)
        if (wrap) send(mod.code, false)
        releaseHeldIfIdle()
    }

    /** Media key that acts on release; it ignores the modifiers and leaves them as they are. */
    fun media(code: Int) { send(code, true); send(code, false) }

    /**
     * Held key: down now, up on [release]; it repeats while held. [combines] false (media keys)
     * leaves the held modifiers alone.
     */
    fun press(id: Any, code: Int, combines: Boolean = true) {
        if (held.containsKey(id)) return
        held[id] = Held(code, false, null, combines)
        send(code, true)
        startRepeat(id)
    }

    /**
     * A character key of the emulated keyboard ([ch] as on the key without Shift). It types the
     * character its face shows (capital / shifted symbol while the user's Shift is down) on the
     * focus computer's layout; which key that is comes from the core ([typedChar], the shared Rust
     * layout tables): that key, with Shift and AltGr (Option) around just this key when the
     * character needs them and the user does not hold them already; the user's own Shift is never
     * released by it.
     *
     * When the layout has no key for the character it goes as text (a shortcut with Ctrl / Alt / ⌘
     * down still sends a key). Dead keys (^ ~ on European layouts) are typed at once with a Space
     * after them and do not repeat.
     */
    fun pressChar(id: Any, ch: Char) {
        if (held.containsKey(id)) return
        val key = when (val typed = typedChar(layout, targetMac, ch, mods.keys.toSet())) {
            null -> return
            is TypedChar.Text -> {
                text(typed.text)
                releaseHeldIfIdle()
                return
            }
            is TypedChar.Key -> typed
        }
        if (key.dead) {
            if (key.autoShift) send(Mod.SHIFT.code, true)
            key.autoAlt?.let { send(it, true) }
            send(key.code, true); send(key.code, false)
            key.autoAlt?.let { send(it, false) }
            if (key.autoShift) send(Mod.SHIFT.code, false)
            send(Hid.SPACE, true); send(Hid.SPACE, false)
            releaseHeldIfIdle()
            return
        }
        held[id] = Held(key.code, key.autoShift, key.autoAlt, true)
        if (key.autoShift) send(Mod.SHIFT.code, true)
        key.autoAlt?.let { send(it, true) }
        send(key.code, true)
        startRepeat(id)
    }

    fun release(id: Any) {
        val key = held.remove(id) ?: return
        if (repeating == id) stopRepeat()
        send(key.code, false)
        key.autoAlt?.let { send(it, false) }
        if (key.autoShift) send(Mod.SHIFT.code, false)
        if (key.combines) releaseHeldIfIdle()
    }

    /** Only the newest held key repeats, as on a hardware keyboard; an older one does not start again. */
    private fun startRepeat(id: Any) {
        stopRepeat()
        repeating = id
        stopTimer = repeat.after(REPEAT_DELAY_MS) { tick(id) }
    }

    private fun tick(id: Any) {
        if (repeating != id) return
        val key = held[id] ?: return stopRepeat()
        send(key.code, true)
        stopTimer = repeat.after(REPEAT_INTERVAL_MS) { tick(id) }
    }

    private fun stopRepeat() {
        stopTimer()
        stopTimer = {}
        repeating = null
    }

    companion object {
        const val DOUBLE_TAP_MS = 300L
        /**
         * Key repeat (user 2026-11-12): 500 ms until the first repeat (Windows' default repeat delay;
         * Android's own key repeat waits its long-press time, 400–500 ms), then every 50 ms
         * (Android's key repeat interval, 20 per second; Windows' default is about 30 per second).
         */
        const val REPEAT_DELAY_MS = 500L
        const val REPEAT_INTERVAL_MS = 50L
    }
}
