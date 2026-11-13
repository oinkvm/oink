package com.hachiwii.oink

import android.content.Context
import android.content.Intent

/**
 * What the persistent notification or a Quick Settings tile asks [MainActivity] to do when it opens
 * Oink (native-rewrite.md 「控制中心快捷开关」): show [tab], send this phone's clipboard once
 * ([sendClipboard]), or set this phone's speaker / microphone to an explicit state ([speaker],
 * [microphone]; never a toggle, so a state change between the tap and Oink opening cannot invert it).
 * [id] names one tap: a one-shot action runs once, also when the Activity is recreated.
 */
data class LaunchRequest(
    val tab: HomeTab? = null,
    val sendClipboard: Boolean = false,
    val speaker: Boolean? = null,
    val microphone: Boolean? = null,
    val id: String? = null,
) {
    val oneShot: Boolean get() = sendClipboard || speaker != null || microphone != null

    companion object {
        const val EXTRA_TAB = "com.hachiwii.oink.OPEN_TAB"
        const val EXTRA_SEND_CLIPBOARD = "com.hachiwii.oink.SEND_CLIPBOARD"
        const val EXTRA_SET_SPEAKER = "com.hachiwii.oink.SET_SPEAKER"
        const val EXTRA_SET_MICROPHONE = "com.hachiwii.oink.SET_MICROPHONE"
        const val EXTRA_REQUEST = "com.hachiwii.oink.REQUEST"

        /**
         * The request an intent carries; null when it carries none, or when the system re-delivers a
         * task's original intent because the user reopened Oink from recent apps (nothing runs again).
         */
        fun from(intent: Intent?): LaunchRequest? {
            if (intent == null || intent.flags and Intent.FLAG_ACTIVITY_LAUNCHED_FROM_HISTORY != 0) return null
            val tab = intent.getStringExtra(EXTRA_TAB)?.let { name -> HomeTab.entries.firstOrNull { it.name == name } }
            fun flag(key: String) = if (intent.hasExtra(key)) intent.getBooleanExtra(key, false) else null
            val request = LaunchRequest(tab, intent.getBooleanExtra(EXTRA_SEND_CLIPBOARD, false),
                flag(EXTRA_SET_SPEAKER), flag(EXTRA_SET_MICROPHONE), intent.getStringExtra(EXTRA_REQUEST))
            return request.takeIf { it.tab != null || it.oneShot }
        }
    }

    fun into(intent: Intent): Intent = intent.apply {
        tab?.let { putExtra(EXTRA_TAB, it.name) }
        if (sendClipboard) putExtra(EXTRA_SEND_CLIPBOARD, true)
        speaker?.let { putExtra(EXTRA_SET_SPEAKER, it) }
        microphone?.let { putExtra(EXTRA_SET_MICROPHONE, it) }
        id?.let { putExtra(EXTRA_REQUEST, it) }
    }
}

/**
 * Opens a [MainActivity] (new or the running one) with [request]. The action keeps its PendingIntents
 * apart from the phone-to-phone notifications' (same component, request codes from record ids): Android
 * matches PendingIntents without their extras, and FLAG_UPDATE_CURRENT would swap them.
 */
internal fun mainLaunchIntent(context: Context, request: LaunchRequest): Intent =
    request.into(Intent(context, MainActivity::class.java).setAction(ACTION_LAUNCH_REQUEST)
        .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TOP or Intent.FLAG_ACTIVITY_SINGLE_TOP))

internal const val ACTION_LAUNCH_REQUEST = "com.hachiwii.oink.LAUNCH_REQUEST"
