package com.hachiwii.oink

import android.app.Notification
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import androidx.core.app.NotificationCompat

/**
 * The one persistent notification (user 2026-11-11, native-rewrite.md 「常驻通知」): a fixed title
 * per the 「点按通知」 setting, the line 「X 在线设备 · 音频出口/转发音频 · 麦克风共享」,
 * and up to three action buttons. Forward replaces the exit label and stop button (user 2026-11-13).
 */
internal data class PersistentNotice(val title: String, val text: String, val tap: NoticeTap, val tab: HomeTab,
                                     val speaker: Boolean = false, val microphone: Boolean = false, val forward: Boolean = false)

/** A button of the persistent notification, with its label. */
internal enum class NoticeAction(val label: String) {
    OPEN("打开 Oink"), COPY("复制内容"), STOP_SPEAKER("停止接收音频"), STOP_FORWARD("停止转发音频"), STOP_MICROPHONE("停止共享麦克风")
}

/**
 * The buttons, in this order (user 2026-11-12; at most three): the action the tap does not do (tap =
 * copy → 「打开 Oink」, tap = open → 「复制内容」), then 「停止接收音频」 while this phone receives the
 * group's audio (「停止转发音频」 replaces it while forwarding) and 「停止共享麦克风」 while it shares its microphone.
 */
internal fun noticeActions(tap: NoticeTap, speaker: Boolean, microphone: Boolean, forward: Boolean = false): List<NoticeAction> = buildList {
    add(if (tap == NoticeTap.COPY) NoticeAction.OPEN else NoticeAction.COPY)
    if (forward) add(NoticeAction.STOP_FORWARD) else if (speaker) add(NoticeAction.STOP_SPEAKER)
    if (microphone) add(NoticeAction.STOP_MICROPHONE)
}

internal const val NOTICE_TITLE_COPY = "Oink · 点按从共享剪贴板复制"
internal const val NOTICE_TITLE_OPEN = "Oink · 运行中"
/** Shown for 2 s after a copy from the shared clipboard succeeded. */
internal const val NOTICE_TITLE_COPIED = "已复制"

/** Other devices online now; 0 while the core is not connected (devices connecting count as offline). */
internal fun onlineDevices(state: AppState): Int =
    if (!state.connected) 0 else state.snapshot.devices.count { !it.isSelf && it.online }

/**
 * 「X 在线设备」, then 「音频出口」 while this phone holds the speaker-exit role and 「麦克风共享」 while
 * it holds the microphone-sender role: by role (the top-bar switches), not by actual playback or
 * capture, so the line matches the tiles and does not flicker with sound (user 2026-11-11).
 */
internal fun noticeText(online: Int, speakerExit: Boolean, microphoneSender: Boolean, forward: Boolean = false): String =
    buildList {
        add("$online 在线设备")
        if (forward) add("转发音频") else if (speakerExit) add("音频出口")
        if (microphoneSender) add("麦克风共享")
    }.joinToString(" · ")

/** Title: fixed per the setting (no live "has copyable content" state), 「已复制」 briefly after a copy. */
internal fun noticeTitle(tap: NoticeTap, copied: Boolean): String = when {
    copied -> NOTICE_TITLE_COPIED
    tap == NoticeTap.COPY -> NOTICE_TITLE_COPY
    else -> NOTICE_TITLE_OPEN
}

internal fun persistentNotice(state: AppState): PersistentNotice = PersistentNotice(
    title = noticeTitle(state.noticeTap, state.noticeCopied),
    text = noticeText(onlineDevices(state), state.localAudio.speakerOn, state.localAudio.microphoneOn, state.localAudio.forwardOn),
    tap = state.noticeTap,
    tab = state.noticeTab,
    forward = state.localAudio.forwardOn,
    speaker = state.localAudio.speakerOn,
    microphone = state.localAudio.microphoneOn,
)

/**
 * Foreground service types of the merged connection service (user 2026-11-11): connectedDevice always,
 * plus mediaPlayback (bit 1) while this phone plays the group's audio and microphone (bit 2) while it
 * shares its microphone. Each is declared on ConnectionService in the manifest.
 */
internal fun foregroundServiceTypes(audio: Int): Int =
    ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE or
        (if (audio and AUDIO_PLAYBACK != 0) ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK else 0) or
        (if (audio and AUDIO_MICROPHONE != 0) ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE else 0)

internal const val AUDIO_PLAYBACK = 1
internal const val AUDIO_MICROPHONE = 2

internal object PersistentNotices {
    const val CHANNEL = "connection"
    const val ID = 1

    /** 停止接收音频 / 停止共享麦克风: commands to the running [ConnectionService]. */
    const val STOP_FORWARD = "com.hachiwii.oink.STOP_FORWARD"
    const val STOP_SPEAKER = "com.hachiwii.oink.STOP_SPEAKER"
    const val STOP_MICROPHONE = "com.hachiwii.oink.STOP_MICROPHONE"

    /**
     * What a tap does, chosen in Settings 「点按通知」. Both are Activity PendingIntents. The copy goes to
     * [ClipboardCopyActivity] (invisible, finishes at once), not to the service as before: it then runs
     * as the foreground app, which also shows its 「没有可复制的内容」 / failure toast (it no longer
     * opens Oink, user 2026-11-12).
     */
    fun tapIntent(context: Context, tap: NoticeTap, tab: HomeTab): PendingIntent = when (tap) {
        NoticeTap.COPY -> copyIntent(context)
        NoticeTap.OPEN -> openIntent(context, tab)
    }

    /**
     * Copy (the copy tap and the 「复制内容」 button): the invisible [ClipboardCopyActivity], not a
     * receiver or the service. Writing the clipboard would work from the background, but its result is a
     * toast, and an Activity shows it as the foreground app (background toasts are limited since Android
     * 11 and some systems, MIUI among them, drop them); a notification button that starts an Activity also
     * closes the notification shade, so the copied content can be pasted at once. It never opens Oink.
     */
    private fun copyIntent(context: Context): PendingIntent = PendingIntent.getActivity(context, 1,
        Intent(context, ClipboardCopyActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_NO_ANIMATION),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)

    /** Oink at the page chosen in 「点按通知」 (the open tap and the 「打开 Oink」 button): an Activity PendingIntent, no trampoline. */
    private fun openIntent(context: Context, tab: HomeTab): PendingIntent =
        PendingIntent.getActivity(context, 2, mainLaunchIntent(context, LaunchRequest(tab = tab)), PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)

    /**
     * 停止接收音频 / 停止共享麦克风 go to the running connection service (it is in the foreground while
     * this notification shows, so it may be started; like the hidden share's 取消), which turns this
     * phone's own setting off — the same state as the device page, 调节 and the tiles — without opening
     * Oink. The notification follows the state, so the button goes with it.
     */
    private fun serviceIntent(context: Context, action: String, requestCode: Int): PendingIntent = PendingIntent.getService(context, requestCode,
        Intent(context, ConnectionService::class.java).setAction(action), PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)

    fun actionIntent(context: Context, action: NoticeAction, notice: PersistentNotice): PendingIntent = when (action) {
        NoticeAction.OPEN -> openIntent(context, notice.tab)
        NoticeAction.COPY -> copyIntent(context)
        NoticeAction.STOP_FORWARD -> serviceIntent(context, STOP_FORWARD, 5)
        NoticeAction.STOP_SPEAKER -> serviceIntent(context, STOP_SPEAKER, 3)
        NoticeAction.STOP_MICROPHONE -> serviceIntent(context, STOP_MICROPHONE, 4)
    }

    /** The small icon is the one-colour Oink mark; buttons per [noticeActions] (text only). */
    fun build(context: Context, notice: PersistentNotice): Notification =
        NotificationCompat.Builder(context, CHANNEL).setSmallIcon(R.drawable.ic_stat_oink)
            .setContentTitle(notice.title).setContentText(notice.text)
            .setOngoing(true).setOnlyAlertOnce(true).setShowWhen(false)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setContentIntent(tapIntent(context, notice.tap, notice.tab))
            .apply {
                noticeActions(notice.tap, notice.speaker, notice.microphone, notice.forward).forEach { action ->
                    addAction(NotificationCompat.Action.Builder(0, action.label, actionIntent(context, action, notice)).build())
                }
            }
            .build()
}
