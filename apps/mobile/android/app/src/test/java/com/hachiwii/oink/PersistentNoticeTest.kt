package com.hachiwii.oink

import android.app.Notification
import android.content.ComponentName
import android.content.Intent
import android.content.pm.ServiceInfo
import android.service.quicksettings.TileService
import com.hachiwii.oink.audio.audioTypesInUse
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config

/** The one persistent notification and its foreground service (user 2026-11-11). */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], qualifiers = "zh-rCN", application = android.app.Application::class)
class PersistentNoticeTest {
    private val context = RuntimeEnvironment.getApplication()
    private val self = Device("me", "Phone", true, isSelf = true, connection = DeviceConnection.THIS_DEVICE)
    private fun peer(id: String, online: Boolean) = Device(id, "PC $id", online, computer = true,
        connection = if (online) DeviceConnection.ONLINE else DeviceConnection.CONNECTING)
    private fun state(connected: Boolean = true, devices: List<Device> = listOf(self, peer("a", true), peer("b", true), peer("c", false)),
                      speaker: Boolean = false, microphone: Boolean = false, tap: NoticeTap = NoticeTap.COPY, copied: Boolean = false) =
        AppState(connected = connected, snapshot = CoreSnapshot(devices = devices),
            localAudio = LocalAudio(speakerOn = speaker, microphoneOn = microphone), noticeTap = tap, noticeCopied = copied)

    @Test fun theLineHasTheFixedFormat() {
        assertEquals("2 在线设备", noticeText(2, speakerExit = false, microphoneSender = false))
        assertEquals("2 在线设备 · 音频出口", noticeText(2, speakerExit = true, microphoneSender = false))
        assertEquals("2 在线设备 · 麦克风共享", noticeText(2, speakerExit = false, microphoneSender = true))
        assertEquals("0 在线设备 · 音频出口 · 麦克风共享", noticeText(0, speakerExit = true, microphoneSender = true))
    }

    @Test fun onlineCountsOtherOnlineDevicesOnlyWhileConnected() {
        // This phone and the connecting device do not count.
        assertEquals("2 在线设备", persistentNotice(state()).text)
        // Not connected: 0, whatever the last snapshot said.
        assertEquals("0 在线设备", persistentNotice(state(connected = false)).text)
        assertEquals("0 在线设备", persistentNotice(state(devices = listOf(self))).text)
    }

    @Test fun audioPartsFollowTheRoles() {
        assertEquals("2 在线设备 · 音频出口 · 麦克风共享", persistentNotice(state(speaker = true, microphone = true)).text)
        assertEquals("2 在线设备 · 麦克风共享", persistentNotice(state(microphone = true)).text)
    }

    @Test fun theTitleIsFixedPerSettingAndSaysCopiedBriefly() {
        assertEquals("Oink · 点按从共享剪贴板复制", persistentNotice(state(tap = NoticeTap.COPY)).title)
        assertEquals("Oink · 运行中", persistentNotice(state(tap = NoticeTap.OPEN)).title)
        assertEquals("已复制", persistentNotice(state(tap = NoticeTap.COPY, copied = true)).title)
        assertEquals("已复制", persistentNotice(state(tap = NoticeTap.OPEN, copied = true)).title)
    }

    private fun build(tap: NoticeTap, tab: HomeTab = HomeTab.DEVICES, speaker: Boolean = false, microphone: Boolean = false): Notification =
        PersistentNotices.build(context, PersistentNotice(noticeTitle(tap, false), "1 在线设备", tap, tab, speaker, microphone))
    private fun labels(n: Notification) = n.actions.orEmpty().map { it.title.toString() }

    @Test fun theNotificationHasTheOinkIconAndItsLines() {
        val notice = build(NoticeTap.COPY)
        assertEquals(R.drawable.ic_stat_oink, notice.smallIcon.resId)
        assertEquals("Oink · 点按从共享剪贴板复制", notice.extras.getCharSequence(Notification.EXTRA_TITLE).toString())
        assertEquals("1 在线设备", notice.extras.getCharSequence(Notification.EXTRA_TEXT).toString())
        assertTrue(notice.flags and Notification.FLAG_ONGOING_EVENT != 0)
        assertEquals(PersistentNotices.CHANNEL, notice.channelId)
    }

    /** Buttons (user 2026-11-12): the action the tap does not do, then the stop buttons for the roles held; at most three. */
    @Test fun buttonsForEveryTapModeAndAudioRole() {
        val open = "打开 Oink"; val copy = "复制内容"; val speaker = "停止接收音频"; val mic = "停止共享麦克风"
        val expected = mapOf(
            Triple(NoticeTap.COPY, false, false) to listOf(open),
            Triple(NoticeTap.COPY, true, false) to listOf(open, speaker),
            Triple(NoticeTap.COPY, false, true) to listOf(open, mic),
            Triple(NoticeTap.COPY, true, true) to listOf(open, speaker, mic),
            Triple(NoticeTap.OPEN, false, false) to listOf(copy),
            Triple(NoticeTap.OPEN, true, false) to listOf(copy, speaker),
            Triple(NoticeTap.OPEN, false, true) to listOf(copy, mic),
            Triple(NoticeTap.OPEN, true, true) to listOf(copy, speaker, mic),
        )
        expected.forEach { (case, labels) ->
            val (tap, sp, mi) = case
            assertEquals("$case", labels, noticeActions(tap, sp, mi).map { it.label })
            assertEquals("$case", labels, labels(build(tap, speaker = sp, microphone = mi)))
        }
    }

    @Test fun theButtonsFollowTheState() {
        val notice = persistentNotice(state(speaker = true, microphone = true, tap = NoticeTap.OPEN))
        assertEquals(listOf(NoticeAction.COPY, NoticeAction.STOP_SPEAKER, NoticeAction.STOP_MICROPHONE),
            noticeActions(notice.tap, notice.speaker, notice.microphone))
        // Speaker turned off: its button goes.
        val after = persistentNotice(state(speaker = false, microphone = true, tap = NoticeTap.OPEN))
        assertEquals(listOf(NoticeAction.COPY, NoticeAction.STOP_MICROPHONE), noticeActions(after.tap, after.speaker, after.microphone))
    }

    @Test fun openButtonOpensOinkAndCopyButtonCopiesWithoutOpeningIt() {
        val openButton = shadowOf(build(NoticeTap.COPY, HomeTab.TRANSFER).actions[0].actionIntent)
        assertTrue(openButton.isActivityIntent)
        assertEquals(ComponentName(context, MainActivity::class.java), openButton.savedIntent.component)
        assertEquals(HomeTab.TRANSFER, LaunchRequest.from(openButton.savedIntent)?.tab)
        val copyButton = shadowOf(build(NoticeTap.OPEN).actions[0].actionIntent)
        assertTrue(copyButton.isActivityIntent)
        assertEquals(ComponentName(context, ClipboardCopyActivity::class.java), copyButton.savedIntent.component)
    }

    @Test fun stopButtonsCommandTheConnectionService() {
        val actions = build(NoticeTap.COPY, speaker = true, microphone = true).actions
        val speaker = shadowOf(actions[1].actionIntent)
        val mic = shadowOf(actions[2].actionIntent)
        listOf(speaker to PersistentNotices.STOP_SPEAKER, mic to PersistentNotices.STOP_MICROPHONE).forEach { (pi, action) ->
            assertTrue(pi.isServiceIntent)
            assertEquals(ComponentName(context, ConnectionService::class.java), pi.savedIntent.component)
            assertEquals(action, pi.savedIntent.action)
        }
    }

    @Test fun copyModeTapsTheInvisibleCopyActivity() {
        val tap = shadowOf(build(NoticeTap.COPY).contentIntent)
        assertTrue(tap.isActivityIntent)
        assertEquals(ComponentName(context, ClipboardCopyActivity::class.java), tap.savedIntent.component)
    }

    @Test fun openModeTapsOinkAtTheChosenPage() {
        val tap = shadowOf(build(NoticeTap.OPEN, HomeTab.CONTROL).contentIntent)
        assertTrue(tap.isActivityIntent)
        assertEquals(ComponentName(context, MainActivity::class.java), tap.savedIntent.component)
        // Its own action: never the same PendingIntent as a phone-to-phone notification with the same request code.
        assertEquals(ACTION_LAUNCH_REQUEST, tap.savedIntent.action)
        val request = LaunchRequest.from(tap.savedIntent)
        assertEquals(HomeTab.CONTROL, request?.tab)
        assertFalse(request!!.oneShot)
    }

    @Test fun foregroundTypesFollowTheAudioBits() {
        val connected = ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE
        assertEquals(connected, foregroundServiceTypes(0))
        assertEquals(connected or ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK, foregroundServiceTypes(AUDIO_PLAYBACK))
        assertEquals(connected or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE, foregroundServiceTypes(AUDIO_MICROPHONE))
        assertEquals(connected or ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK or ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE,
            foregroundServiceTypes(AUDIO_PLAYBACK or AUDIO_MICROPHONE))
    }

    @Test fun audioTypesStayWhileTheirSwitchIsOnOrTheirStreamIsOpen() {
        assertEquals(0, audioTypesInUse(0, speakerOpen = false, microphoneOpen = false))
        assertEquals(AUDIO_PLAYBACK, audioTypesInUse(1, speakerOpen = false, microphoneOpen = false))
        // A paused microphone (switch on, no stream) keeps its type for the next demand.
        assertEquals(AUDIO_MICROPHONE, audioTypesInUse(2, speakerOpen = false, microphoneOpen = false))
        // Switched off while the stream is still closing.
        assertEquals(AUDIO_PLAYBACK or AUDIO_MICROPHONE, audioTypesInUse(0, speakerOpen = true, microphoneOpen = true))
    }

    @Test fun theConnectionServiceIsTheOnlyForegroundService() {
        val info = context.packageManager.getServiceInfo(ComponentName(context, ConnectionService::class.java), 0)
        val all = ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE or ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK or
            ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE
        assertEquals(all, info.foregroundServiceType)
        assertTrue(runCatching { Class.forName("com.hachiwii.oink.audio.AudioService") }.isFailure)
    }

    @Test fun theFiveTilesAreDeclared() {
        @Suppress("DEPRECATION")
        val tiles = context.packageManager.queryIntentServices(Intent(TileService.ACTION_QS_TILE).setPackage(context.packageName), 0)
        val labels = tiles.associate { it.serviceInfo.name.substringAfterLast('.') to it.serviceInfo.loadLabel(context.packageManager).toString() }
        assertEquals(mapOf("CopyTileService" to "复制", "PasteTileService" to "粘贴", "SpeakerTileService" to "接收音频",
            "MicrophoneTileService" to "共享麦克风", "ForwardTileService" to "音频转发"), labels)
        tiles.forEach { assertEquals(android.Manifest.permission.BIND_QUICK_SETTINGS_TILE, it.serviceInfo.permission) }
    }
}
