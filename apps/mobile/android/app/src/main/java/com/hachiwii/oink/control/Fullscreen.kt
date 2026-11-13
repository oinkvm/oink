package com.hachiwii.oink.control

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue

/**
 * 全屏 / 恢复 (user 2026-11-12, replaces the 屏幕方向 setting): the control page's landscape layout is
 * its full screen. It is entered by turning the phone (the system rotates the window) or with 全屏
 * (portrait top right), which turns the interface to landscape even while the phone is upright; 恢复
 * (landscape top right) turns it back to portrait even while the phone is sideways.
 *
 * After 恢复 from a full screen that a rotation entered, turning the phone no longer enters it until
 * the user leaves the control page (another tab, or the app goes to the background). After 恢复 from
 * a full screen that 全屏 entered, rotation works as usual again — once the phone is held upright
 * (until then the interface would turn straight back to landscape).
 *
 * The page applies [request] to the activity while it is shown and calls [left] when it is not.
 */
class Fullscreen {
    enum class Request { SYSTEM, LANDSCAPE, PORTRAIT }

    /** The current full screen was entered with 全屏. */
    private var byButton by mutableStateOf(false)
    /** 恢复 after a rotation: portrait until the user leaves the page. */
    private var noAuto by mutableStateOf(false)
    /** 恢复 after 全屏 with the phone not upright yet: portrait until it is. */
    private var holdPortrait by mutableStateOf(false)

    /** The orientation the page asks for while it is shown. */
    val request: Request get() = when {
        byButton -> Request.LANDSCAPE
        noAuto || holdPortrait -> Request.PORTRAIT
        else -> Request.SYSTEM
    }

    /** Only button-entered fullscreen consumes Back to restore portrait. */
    val enteredByButton: Boolean get() = byButton

    /** Whether the phone's own tilt matters ([tilt] should be reported). */
    val watchesTilt: Boolean get() = holdPortrait

    /** 全屏. */
    fun enter() {
        byButton = true
        holdPortrait = false
    }

    /** 恢复; [upright]: the phone is held upright now (null: unknown, e.g. lying flat). */
    fun restore(upright: Boolean?) {
        if (byButton) {
            byButton = false
            holdPortrait = upright != true
        } else noAuto = true
    }

    /** The phone's tilt (true: upright, false: sideways), while [watchesTilt]. */
    fun tilt(upright: Boolean) {
        if (upright) holdPortrait = false
    }

    /** The user left the control page (another tab, or the app went to the background). */
    fun left() {
        byButton = false
        noAuto = false
        holdPortrait = false
    }

    companion object {
        /**
         * Upright or sideways from [android.view.OrientationEventListener] degrees (0 = upright, 90 /
         * 270 = sideways); null within 15° of the diagonals and when unknown (lying flat, -1).
         */
        fun upright(degrees: Int): Boolean? {
            if (degrees < 0) return null
            val d = degrees % 180
            return when {
                d < 30 || d > 150 -> true
                d in 60..120 -> false
                else -> null
            }
        }
    }
}
