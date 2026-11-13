package com.hachiwii.oink.control

/**
 * The portrait emulated keyboard's key area height (below its control keys, above the navigation bar),
 * user 2026-11-12: it follows the system keyboard while that one has its normal height, and otherwise
 * keeps the baseline.
 *
 * - [BASELINE_DP]: the system keyboard's key area on the test phone, in dp so that other phones scale with
 *   their density. Measured 2026-11-12 on the Phone (1080 × 2400, 440 dpi) with its Sogou keyboard
 *   (9-key layout with its toolbar row) under Oink's control page: IME inset 929 px = 337.8 dp from the
 *   screen bottom, gesture bar 44 px = 16 dp, so 885 px = 321.8 dp above the navigation bar.
 * - Within [TOLERANCE] of the baseline (±12 %: 283–361 dp) the emulated keyboard takes the system keyboard's
 *   height: that absorbs what a normal keyboard varies by — a toolbar / candidate row (≈ 35–40 dp, about
 *   12 %), the letter / number / 9-key layouts, another IME of similar size — so a switch between the two
 *   keyboards moves nothing. (The navigation bar mode does not matter: heights are taken above it.)
 * - Outside it the height is not a keyboard's normal one — a suggestion panel such as Sogou's 超级联想
 *   (about 510 dp on the test phone, +58 %), a floating or split IME (its bottom inset is far smaller or
 *   0) — and the emulated keyboard keeps the baseline.
 *
 * A phone whose normal IME is itself outside the range (much taller or shorter in dp) always gets the
 * baseline: the emulated keyboard is then a little shorter or taller than its system keyboard and a switch
 * between them changes the height, but both work as usual. Only heights in range are remembered.
 */
object EmulatedKeyboardHeight {
    const val BASELINE_DP = 322f
    const val TOLERANCE = .12f

    fun inRange(keysDp: Float, baseline: Float = BASELINE_DP) =
        keysDp >= baseline * (1 - TOLERANCE) && keysDp <= baseline * (1 + TOLERANCE)

    /** The emulated keyboard's key area for the system keyboard's last [observedDp] (null / 0: not seen). */
    fun keys(observedDp: Float?, baseline: Float = BASELINE_DP): Float =
        observedDp?.takeIf { it > 0f && inRange(it, baseline) } ?: baseline
}
