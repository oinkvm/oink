package com.hachiwii.oink.control

import kotlin.math.max
import kotlin.math.min

/**
 * The landscape emulated keyboard's container (user 2026-11-12, docs/control-page.md §1.A): the numbers
 * and mappings of its handle gestures, in dp, dp per second and the morph progress q. The page does the
 * drawing and the animations; everything that decides is here.
 *
 * - Sideways: the container follows the finger 1:1 between [Bounds.lo] and [Bounds.hi] (1/3 past
 *   them); a flick rests at Apple's projection (WWDC18 "Designing Fluid Interfaces"):
 *   projected = position + velocity (pt/s) ÷ 1000 × r ÷ (1 − r), r = UIScrollView.DecelerationRate
 *   .normal = 0.998, kept inside the bounds and snapped to a bound within [SNAP].
 * - Down (dismiss): p = down ÷ [FADE_AT]; the container shrinks toward its centre to 1 − 0.25 p and
 *   fades to 1 − 0.6 p; letting go with a projected travel past [DISMISS_AT] finishes (scale 0.6,
 *   opacity 0, 200 ms ease-in) and hides the keyboard, else it springs back.
 * - Double-tap expands. Upward drags do not expand; vertical dragging only dismisses downward.
 * - The axis locks after [SLOP] of movement: mostly sideways = move, mostly vertical = dismiss.
 * - Springs: critically damped, damping ratio 1, response 0.35 s → stiffness (2π ÷ 0.35)² ≈ 322.
 */
object FloatMotion {
    // Reference phone portrait width measured at 1080 px / (440 dpi / 160) on device.
    // Deliberately fixed: rotating or opening the keyboard must not change its target width.
    const val WIDTH = 392.72727f
    /** Expanded keyboard keeps the approved enlarged metrics. Floating keys scale with their width. */
    const val SCALE = 1.2f
    fun floatingScale(width: Float) = width / 400f

    /** Keep the portrait-width floating container inside the safe areas and edge margins. */
    fun width(screen: Float, safeLeft: Float, safeRight: Float) =
        min(WIDTH, max(0f, screen - safeLeft - safeRight - 2 * EDGE))
    const val EDGE = 8f
    const val SNAP = 24f
    const val SLOP = 10f
    const val DECELERATION = 0.998f
    const val DISMISS_AT = 80f
    const val FADE_AT = 160f
    const val RESPONSE_S = .35f
    val STIFFNESS = (2 * Math.PI / RESPONSE_S).let { (it * it).toFloat() }
    /** The dismiss finish (ease-in, t²). */
    const val DISMISS_FINISH_MS = 200
    /** Content swaps: the full-screen layout fades in over 160 ms after expanding (the icon out over 120 ms); 还原 fades the floating layout in over 120 ms. */
    const val EXPAND_FADE_MS = 160
    const val ICON_FADE_MS = 120
    const val RESTORE_FADE_MS = 120

    /** Where the projection says a flick comes to rest: position + velocity × r ÷ (1 − r) ÷ 1000. */
    fun project(position: Float, velocityPerSecond: Float): Float =
        position + velocityPerSecond / 1000f * DECELERATION / (1f - DECELERATION)

    /**
     * Sideways bounds of the container's left edge in screen dp: the cutout side stops at the safe area,
     * the free side [EDGE] from the screen edge (or from a navigation bar there). [cutoutRight]: the
     * phone is held with the cutout on the right.
     */
    data class Bounds(val lo: Float, val hi: Float) {
        /** The default place: on the side away from the cutout. */
        fun home(cutoutRight: Boolean) = if (cutoutRight) lo else hi
        /** Past a bound the container moves a third of the finger's travel. */
        fun rubber(x: Float) = when {
            x < lo -> lo - (lo - x) / 3f
            x > hi -> hi + (x - hi) / 3f
            else -> x
        }
        /** The resting place of a flick let go at [x] with [velocity] dp/s. */
        fun rest(x: Float, velocity: Float): Float {
            val p = project(x, velocity).coerceIn(lo, max(lo, hi))
            return when {
                p - lo < SNAP -> lo
                hi - p < SNAP -> hi
                else -> p
            }
        }
        companion object {
            fun of(screenWidth: Float, width: Float, cutoutLeft: Float, cutoutRight: Float, freeLeft: Float, freeRight: Float, cutoutOnRight: Boolean): Bounds {
                val lo = if (cutoutOnRight) freeLeft + EDGE else cutoutLeft
                val hi = if (cutoutOnRight) screenWidth - cutoutRight - width else screenWidth - freeRight - EDGE - width
                return Bounds(lo, max(lo, hi))
            }
        }
    }

    /** Dismiss feedback for a downward travel of [down] dp and the finish progress [finish] (0…1): (scale, opacity). */
    fun dismissLook(down: Float, finish: Float): Pair<Float, Float> {
        val p = (down / FADE_AT).coerceIn(0f, 1f)
        val s0 = 1f - .25f * p
        val o0 = 1f - .6f * p
        return (s0 + (.6f - s0) * finish) to o0 * (1f - finish)
    }

    fun dismisses(down: Float, velocity: Float) = project(down, velocity) > DISMISS_AT

    fun smoothstep(a: Float, b: Float, x: Float): Float {
        val t = ((x - a) / (b - a)).coerceIn(0f, 1f)
        return t * t * (3 - 2 * t)
    }

    /** The double-tap expansion animation: floating keys fade out, then the keyboard icon fades in. */
    data class Look(val floating: Float, val icon: Float)

    fun morphLook(q: Float): Look {
        val qc = min(1f, q)
        return Look(1f - smoothstep(.15f, .45f, qc), smoothstep(.30f, .50f, qc))
    }

    /** Linear interpolation of the container box between floating (q 0) and the whole screen (q 1). */
    fun lerp(a: Float, b: Float, q: Float) = a + (b - a) * min(1f, max(0f, q))
}
