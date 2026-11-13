package com.hachiwii.oink.control

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.geometry.Rect
import kotlin.math.hypot
import kotlin.math.max

/**
 * A key with a preview bubble, which fingers can slide onto (docs/control-page.md §3.1.4). [hold]
 * keys go down when the first finger enters and up when the last one leaves; the others ([hold]
 * false) do nothing on entering or leaving and [fire] once (down + up) when the last finger lifts
 * on them.
 */
class SlideKey(val id: String, val hold: Boolean, val down: () -> Unit = {}, val up: () -> Unit = {}, val fire: () -> Unit = {})

/** What is under a finger: a key with a bubble, nothing (a gap far from keys, a modifier, Fn…), or outside the keyboard. */
sealed interface Hit {
    class On(val key: SlideKey) : Hit
    data object None : Hit
    data object Out : Hit
}

/** The bubble: the key it is on, and whether it glides there from the key it was on (else it appears). */
class Bubble(val key: SlideKey, val glide: Boolean)

/**
 * Fingers on the keyboard (user 2026-11-11). Each finger has its own key under it; sliding to
 * another key = leaving the old one, then entering the new one. A key is down while at least one
 * finger is on it. Leaving the keyboard ends the finger for keys: its key is left (a hold key goes
 * up, a tap key is cancelled) and it presses nothing more until it lifts. The bubble belongs to the
 * finger that came down last; it does not jump back to an older finger, which gets it again when
 * it next moves to another key. [feel] is the haptic for a key going down: first touch or slide.
 */
class KeySlider(private val feel: (slide: Boolean) -> Unit = {}) {
    private class Finger(var key: SlideKey? = null, var dead: Boolean = false)
    private val fingers = LinkedHashMap<Long, Finger>()
    private val count = HashMap<String, Int>()
    private var newest: Long? = null

    /** Keys with a finger on them (drawn pressed). */
    var down by mutableStateOf(emptySet<String>()); private set
    var bubble by mutableStateOf<Bubble?>(null); private set

    fun tracks(finger: Long) = fingers.containsKey(finger)

    /** Key under [finger] now, null for none (for keeping it while the finger is near its edge). */
    fun keyOf(finger: Long): String? = fingers[finger]?.key?.id

    /** A finger lands on [key]. */
    fun press(finger: Long, key: SlideKey) {
        val f = Finger()
        fingers[finger] = f
        newest = finger
        enter(f, key, slide = false)
        bubble = Bubble(key, glide = false)
    }

    fun move(finger: Long, hit: Hit) {
        val f = fingers[finger] ?: return
        if (f.dead) return
        when (hit) {
            Hit.Out -> {
                leave(f, lifted = false)
                f.dead = true
                if (newest == finger) bubble = null
            }
            Hit.None -> if (f.key != null) {
                leave(f, lifted = false)
                if (newest == finger) bubble = null
            }
            is Hit.On -> if (hit.key.id != f.key?.id) {
                val had = f.key != null
                leave(f, lifted = false)
                enter(f, hit.key, slide = true)
                if (newest == finger) bubble = Bubble(hit.key, glide = had && bubble != null)
            }
        }
    }

    /** The finger lifts: a tap key under it fires if it was the last finger on it. */
    fun lift(finger: Long) = end(finger, lifted = true)

    /** The system took the touch away: like leaving, nothing fires. */
    fun cancel(finger: Long) = end(finger, lifted = false)

    private fun end(finger: Long, lifted: Boolean) {
        val f = fingers.remove(finger) ?: return
        leave(f, lifted = lifted && !f.dead)
        if (newest == finger) {
            newest = fingers.keys.lastOrNull()
            bubble = null
        }
    }

    /** The keys under the fingers changed (Fn, 123 / ABC): each finger's key is left; its next move finds the new one. */
    fun relayout() {
        fingers.forEach { (id, f) -> if (f.key != null) { leave(f, lifted = false); if (newest == id) bubble = null } }
    }

    /** The keyboard closed or switched: every finger is cancelled. */
    fun reset() = fingers.keys.toList().forEach { cancel(it) }

    private fun enter(f: Finger, key: SlideKey, slide: Boolean) {
        f.key = key
        val n = (count[key.id] ?: 0) + 1
        count[key.id] = n
        if (n > 1) return // already down under another finger: nothing new goes out
        down = down + key.id
        feel(slide)
        if (key.hold) key.down()
    }

    private fun leave(f: Finger, lifted: Boolean) {
        val key = f.key ?: return
        f.key = null
        val n = (count[key.id] ?: 1) - 1
        if (n > 0) { count[key.id] = n; return } // another finger still holds it
        count.remove(key.id)
        down = down - key.id
        if (key.hold) key.up() else if (lifted) key.fire()
    }

    companion object {
        /**
         * The key under a point: outside every [areas] rect = [Hit.Out]; [current] is kept while the
         * point is within [stick] of it; otherwise the nearest key within [near] (gaps belong to the
         * keys on both sides), [Hit.None] when that is a key without a bubble ([keys] value null) or
         * nothing is that near.
         */
        fun locate(x: Float, y: Float, current: String?, keys: Collection<Pair<Rect, SlideKey?>>, areas: Collection<Rect>, near: Float, stick: Float): Hit {
            if (areas.none { x >= it.left && x <= it.right && y >= it.top && y <= it.bottom }) return Hit.Out
            fun d(r: Rect) = hypot(max(max(r.left - x, 0f), x - r.right), max(max(r.top - y, 0f), y - r.bottom))
            if (current != null) keys.firstOrNull { it.second?.id == current }?.let { (r, k) -> if (d(r) <= stick) return Hit.On(k!!) }
            val best = keys.minByOrNull { d(it.first) } ?: return Hit.None
            if (d(best.first) >= near) return Hit.None
            return best.second?.let { Hit.On(it) } ?: Hit.None
        }
    }
}
