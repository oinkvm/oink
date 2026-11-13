package com.hachiwii.oink

import android.app.Notification
import android.content.Intent
import android.service.quicksettings.Tile
import com.hachiwii.oink.tiles.*
import org.junit.Assert.*
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.RuntimeEnvironment
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config
import uniffi.oink_mobile_ffi.*

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], application = android.app.Application::class)
class AudioForwardTest {
    @Test fun tapAndHoldMatrix() {
        for (tap in SpeakerTap.entries) for (privileged in listOf(false, true)) {
            fun action(speaker: Boolean = false, forward: Boolean = false, long: Boolean = false) = speakerAction(speaker, forward, privileged, tap, long)
            assertEquals(if (privileged && tap == SpeakerTap.FORWARD) SpeakerAction.FORWARD else SpeakerAction.EXIT, action())
            assertEquals(if (!privileged) SpeakerAction.REJECT else if (tap == SpeakerTap.EXIT) SpeakerAction.FORWARD else SpeakerAction.EXIT, action(long = true))
            assertEquals(SpeakerAction.OFF, action(speaker = true))
            assertEquals(SpeakerAction.OFF, action(forward = true))
            assertEquals(if (privileged) SpeakerAction.FORWARD else SpeakerAction.REJECT, action(speaker = true, long = true))
            assertEquals(SpeakerAction.EXIT, action(forward = true, long = true))
        }
    }
    @Test fun preferenceIsPersistentAndNonPrivilegedTapIgnoresIt() {
        val context = RuntimeEnvironment.getApplication()
        val settings = LocalSettings(context)
        assertEquals(SpeakerTap.EXIT, settings.speakerTap)
        settings.speakerTap = SpeakerTap.FORWARD
        assertEquals(SpeakerTap.FORWARD, LocalSettings(context).speakerTap)
        assertEquals(SpeakerAction.EXIT, speakerAction(false, false, false, settings.speakerTap, false))
    }
    @Test fun captureFeedbackFollowsEffectiveStartAndDoesNotRepeatForSnapshots() {
        val waiting = LocalAudio(forwardOn = true)
        val running = waiting.copy(forwardRunning = true)
        assertTrue(audioFeedback(LocalAudio(), waiting).isEmpty())
        assertEquals(listOf("调节本机音量不影响转发", "转发中本机静音"), audioFeedback(waiting, running))
        assertTrue(audioFeedback(running, running.copy(microphoneOn = true)).isEmpty())
        assertEquals(listOf("转发音频失败：系统音频正被其它功能占用（如投屏）"), audioFeedback(waiting, LocalAudio(forwardFailure = MobileMediaFailure.UNAVAILABLE)))
        assertEquals(listOf("请解锁手机后再开始转发"), audioFeedback(waiting, LocalAudio(forwardFailure = MobileMediaFailure.SYSTEM_BLOCKED)))
        val blocked = LocalAudio(forwardFailure = MobileMediaFailure.SYSTEM_BLOCKED)
        assertTrue(audioFeedback(blocked, blocked).isEmpty())
        assertEquals(listOf("请解锁手机后再开始转发"), audioFeedback(blocked, blocked, completedAttempt = true))
    }
    @Test fun localMediaMasksAndFailureComeFromTheThirdResource() {
        val media = MobileMediaState(9uL, 6u, 6u, emptyList(), 0u, 0u, 0u, 4u,
            listOf(MobileMediaFailure.NONE, MobileMediaFailure.NONE, MobileMediaFailure.UNAVAILABLE),
            MobileMicrophonePermission.NOT_DETERMINED, listOf(0uL, 0uL, 9uL), true, "exit")
        val audio = localAudio(media, MobileAudioState(MobileEndpointState.LOCAL, MobileEndpointState.STARTING, MobileEndpointState.FAILED))
        assertFalse(audio.speakerOn)
        assertTrue(audio.forwardOn && audio.forwardRunning && audio.microphoneOn && audio.audioRole)
        assertEquals(MobileMediaFailure.UNAVAILABLE, audio.forwardFailure)
        assertEquals(MobileEndpointState.FAILED, audio.forward)
        assertEquals(9uL, audio.mediaGeneration)
        assertEquals(0, foregroundServiceTypes(0) and android.content.pm.ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE)
    }
    @Test fun forwardKeepsWifiRoleAndPhonePeersUseTheSendBadgeState() {
        assertTrue(LocalAudio(forwardOn = true, joined = true).audioRole)
        assertFalse(LocalAudio(forwardOn = true).audioRole)
        for (state in listOf(MobileEndpointState.ACTIVE, MobileEndpointState.STARTING)) {
            val peer = Device("phone", "Phone", true, platform = "android", mobileAudio = MobileAudioState(MobileEndpointState.LOCAL, MobileEndpointState.LOCAL, state)).toPeer(null)
            assertEquals("forward", peer.speaker)
        }
    }
    @Test fun forwardNoticeReplacesReceiveAndRetainsAtMostThreeActions() {
        for (tap in NoticeTap.entries) for (speaker in listOf(false, true)) {
            val notice = persistentNotice(AppState(localAudio = LocalAudio(speakerOn = speaker, microphoneOn = true, forwardOn = true), noticeTap = tap))
            assertEquals("0 在线设备 · 转发音频 · 麦克风共享", notice.text)
            val built = PersistentNotices.build(RuntimeEnvironment.getApplication(), notice)
            assertEquals(listOf(if (tap == NoticeTap.COPY) "打开 Oink" else "复制内容", "停止转发音频", "停止共享麦克风"), built.actions.map { it.title.toString() })
            assertEquals(PersistentNotices.STOP_FORWARD, shadowOf(built.actions[1].actionIntent).savedIntent.action)
        }
    }
    private val facts = TileFacts(true, true, true, true, true, false, false, true, captureReady = true)
    @Test fun speakerTileIsDisabledDuringForwardAndForwardTileTogglesDirectly() {
        val forwarding = facts.copy(forwardOn = true)
        assertEquals(TileLook(Tile.STATE_UNAVAILABLE, "转发中"), tileLook(TileKind.SPEAKER, forwarding, "stale"))
        assertEquals(TileClick.Ignore, tileClick(TileKind.SPEAKER, forwarding, "r"))
        assertEquals(TileLook(Tile.STATE_ACTIVE, "转发中"), tileLook(TileKind.FORWARD, forwarding, null))
        assertEquals(TileClick.SetForward(false), tileClick(TileKind.FORWARD, forwarding, "r"))
        assertEquals(TileClick.SetForward(true), tileClick(TileKind.FORWARD, facts, "r"))
        for (blocked in listOf(facts.copy(captureReady = false), facts.copy(inForeground = false), facts.copy(connected = false))) {
            assertEquals(TileClick.Ignore, tileClick(TileKind.FORWARD, blocked, "r"))
            assertEquals(Tile.STATE_UNAVAILABLE, tileLook(TileKind.FORWARD, blocked, null).state)
        }
        assertEquals("需特权模式", tileLook(TileKind.FORWARD, facts.copy(captureReady = false), null).subtitle)
    }
    @Test fun forwardTileManifestHasItsOwnServiceAndIcon() {
        val context = RuntimeEnvironment.getApplication()
        val info = context.packageManager.getServiceInfo(android.content.ComponentName(context, ForwardTileService::class.java), 0)
        assertEquals("音频转发", info.loadLabel(context.packageManager))
        assertEquals("android.permission.BIND_QUICK_SETTINGS_TILE", info.permission)
        assertEquals(R.drawable.ic_tile_forward, info.icon)
    }
}
