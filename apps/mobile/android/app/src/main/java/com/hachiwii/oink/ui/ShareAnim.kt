package com.hachiwii.oink.ui

import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.ui.geometry.Offset
import kotlin.math.atan2
import kotlin.math.hypot
import kotlin.math.sin

/**
 * Share card stage maths (share-anim SPEC.zh-CN.md §5, `preview.html` `<script id="core">`), as pure
 * functions: the card's time source gives `t`, [ShareAnim.frame] gives everything one frame draws.
 */

/**
 * Where the arrow lands (SPEC §3.6, §5.8), chosen by the receiving device's kind for the whole card:
 * a computer (macOS / Windows) or a phone (iPhone, iPad, Android).
 */
internal enum class ShareTarget(
    /** Flight end E: the arrow's tail point lands here. */
    val end: Offset,
    /** Centre of the target's receive nudge. */
    val nudgeCenter: Offset,
    /** The landed Oink mark: `translate(x, y) · scale(k) · translate(0, 1.5)` in the mark's 64 grid. */
    val landed: Offset, val landedScale: Float,
    /** Origin (Wi-Fi dot centre) of the offline mark while waiting (§3.8). */
    val offlineOrigin: Offset,
) {
    COMPUTER(Offset(195f, 49f), Offset(200f, 54f), Offset(187.3f, 38.3f), .4f, Offset(200f, 57f)),
    PHONE(Offset(199f, 50.5f), Offset(204f, 53f), Offset(193.8f, 42.8f), .32f, Offset(204f, 58.5f)),
}

internal class ShareFrame(val p: Offset, val angle: Float, val scale: Float, val opacity: Float, val trail: List<Offset>, val nudge: Float) {
    /** Length of the tail polyline (SPEC §5.4 「拖尾长」). */
    val trailLength: Float get() = (1 until trail.size).sumOf { hypot(trail[it].x - trail[it - 1].x, trail[it].y - trail[it - 1].y).toDouble() }.toFloat()
}

/** Opacities during the hand-over after the target came online (SPEC §5.9); [t] null: the first flight has not started. */
internal data class ShareHandoff(val offline: Float, val logo: Float, val bar: Float, val t: Float?)

internal object ShareAnim {
    const val D = 1400f
    /** The hand-over takes this long; afterwards the card is the ordinary sending card. */
    const val HANDOFF_MS = 300f
    /** Entering the waiting state from 「正在连接」 (SPEC §5.9 「进入等待」). */
    const val ENTER_WAIT_MS = 150f
    /** The first flight after the hand-over starts this long after the target came online. */
    const val LEAD_MS = 160f
    private val S = Offset(54f, 50f)
    private val C = Offset(116f, 4f)
    private const val FLIGHT_END = .72f
    private const val LAG = .16f
    private const val N = 12
    private val easeFlight = CubicBezierEasing(.35f, 0f, .25f, 1f)
    private val easeIn = CubicBezierEasing(0f, 0f, .2f, 1f)
    private val easeOut = CubicBezierEasing(.4f, 0f, 1f, 1f)
    private val easeShrink = CubicBezierEasing(.4f, 0f, .2f, 1f)
    /** VI `--ease`: the indeterminate bar, entering the wait, the card's height. */
    val easeStandard = CubicBezierEasing(.2f, 0f, 0f, 1f)

    private fun clamp01(v: Float) = v.coerceIn(0f, 1f)
    private fun seg(t: Float, a: Float, b: Float) = clamp01((t - a) / (b - a))
    private fun pathAt(u: Float, e: Offset): Offset { val m = 1 - u; return Offset(m * m * S.x + 2 * m * u * C.x + u * u * e.x, m * m * S.y + 2 * m * u * C.y + u * u * e.y) }
    private fun tangentAt(u: Float, e: Offset) = Offset(2 * (1 - u) * (C.x - S.x) + 2 * u * (e.x - C.x), 2 * (1 - u) * (C.y - S.y) + 2 * u * (e.y - C.y))
    private fun flightU(t: Float) = easeFlight.transform(clamp01(t / FLIGHT_END))

    /** One frame at normalised time [t] in [0, 1) of a 1400 ms round (SPEC §5.3). */
    fun frame(t: Float, target: ShareTarget): ShareFrame {
        val e = target.end
        val u = flightU(t); val tg = tangentAt(u, e)
        val fi = easeIn.transform(seg(t, 0f, .10f)); val fo = easeOut.transform(seg(t, .58f, .74f)); val sh = easeShrink.transform(seg(t, .30f, .60f))
        val trail = (0..N).map { k -> pathAt(flightU(maxOf(0f, t - k * LAG / N)), e) }
        return ShareFrame(pathAt(u, e), Math.toDegrees(atan2(tg.y, tg.x).toDouble()).toFloat(), (.6f + .4f * fi) * (1 - .45f * sh), fi * (1 - fo), trail,
            1 + .05f * sin(Math.PI * seg(t, .62f, .86f)).toFloat())
    }

    /** Reduced-motion pose: u = 0.5 (top of the arc), for either target (SPEC §7). */
    val STATIC_T = run { var lo = 0f; var hi = FLIGHT_END; repeat(40) { val m = (lo + hi) / 2; if (flightU(m) < .5f) lo = m else hi = m }; (lo + hi) / 2 }

    /** [ms] after the target came online (SPEC §5.9 `handoff(ms)`). */
    fun handoff(ms: Float) = ShareHandoff(
        offline = 1 - easeOut.transform(seg(ms, 0f, LEAD_MS)),
        logo = 1 - easeIn.transform(seg(ms, LEAD_MS, HANDOFF_MS)),
        bar = seg(ms, 0f, 150f),
        t = if (ms < LEAD_MS) null else ((ms - LEAD_MS) % D) / D,
    )

    /** How far entering the wait is, [ms] after it began: the marks' opacity, the arrow's is 1 − this. */
    fun enterWait(ms: Float) = easeStandard.transform(seg(ms, 0f, ENTER_WAIT_MS))

    /** The indeterminate bar segment's left edge as a fraction of the track (SPEC §6.1). */
    fun barLeft(t: Float) = -.35f + 1.35f * easeStandard.transform(t)
}
