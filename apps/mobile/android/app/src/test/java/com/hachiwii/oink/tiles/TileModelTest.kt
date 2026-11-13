package com.hachiwii.oink.tiles

import android.service.quicksettings.Tile
import com.hachiwii.oink.AppState
import com.hachiwii.oink.CoreSnapshot
import com.hachiwii.oink.Device
import com.hachiwii.oink.HomeTab
import com.hachiwii.oink.LaunchRequest
import com.hachiwii.oink.LocalAudio
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** Quick Settings tiles: state shown and where a tap goes (user 2026-11-11). */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [34], application = android.app.Application::class)
class TileModelTest {
    private val running = TileFacts(serviceRunning = true, paired = true, connected = true, inForeground = true,
        privilegedReady = false, speakerOn = false, microphoneOn = false, microphonePermission = true)
    private fun click(kind: TileKind, facts: TileFacts) = tileClick(kind, facts, "r")

    @Test fun factsComeFromTheState() {
        val self = Device("me", "me", true, isSelf = true)
        val state = AppState(serviceEnabled = true, connected = true, snapshot = CoreSnapshot(devices = listOf(self, Device("pc", "Windows PC", false))),
            localAudio = LocalAudio(speakerOn = true), privilegedReady = true)
        val facts = tileFacts(state, inForeground = true, microphonePermission = false)
        assertTrue(facts.paired && facts.serviceRunning && facts.speakerOn && facts.privilegedReady)
        assertFalse(facts.microphoneOn)
        // Only this phone: not paired.
        assertFalse(tileFacts(state.copy(snapshot = CoreSnapshot(devices = listOf(self))), true, true).paired)
    }

    @Test fun unavailableWithoutTheServiceOrAGroup() {
        TileKind.entries.forEach { kind ->
            assertEquals(TileLook(Tile.STATE_UNAVAILABLE, "Oink 未运行"), tileLook(kind, running.copy(serviceRunning = false), "已复制"))
            assertEquals(TileLook(Tile.STATE_UNAVAILABLE, "未配对"), tileLook(kind, running.copy(paired = false), null))
            assertEquals(TileClick.Ignore, click(kind, running.copy(serviceRunning = false)))
            assertEquals(TileClick.Ignore, click(kind, running.copy(paired = false)))
        }
    }

    @Test fun togglesShowTheRoleAndOneShotsTheirFeedback() {
        assertEquals(TileLook(Tile.STATE_ACTIVE, null), tileLook(TileKind.SPEAKER, running.copy(speakerOn = true), null))
        assertEquals(TileLook(Tile.STATE_INACTIVE, null), tileLook(TileKind.SPEAKER, running, null))
        assertEquals(TileLook(Tile.STATE_ACTIVE, null), tileLook(TileKind.MICROPHONE, running.copy(microphoneOn = true), null))
        assertEquals(TileLook(Tile.STATE_INACTIVE, "获取中"), tileLook(TileKind.COPY, running, "获取中"))
        assertEquals(TileLook(Tile.STATE_INACTIVE, "已发送"), tileLook(TileKind.PASTE, running, "已发送"))
    }

    @Test fun copyAlwaysRunsInTheBackground() {
        assertEquals(TileClick.Copy, click(TileKind.COPY, running))
        assertEquals(TileClick.Copy, click(TileKind.COPY, running.copy(inForeground = false)))
    }

    @Test fun pasteOpensTheTransferPageUnlessPrivileged() {
        val open = LaunchRequest(tab = HomeTab.TRANSFER, sendClipboard = true, id = "r")
        assertEquals(TileClick.Open(open), click(TileKind.PASTE, running))
        assertEquals(TileClick.PrivilegedPaste(open), click(TileKind.PASTE, running.copy(privilegedReady = true)))
    }

    @Test fun speakerSwitchesDirectlyInTheForegroundElseOpensDevicesWithTheState() {
        assertEquals(TileClick.SetSpeaker(true), click(TileKind.SPEAKER, running))
        assertEquals(TileClick.SetSpeaker(false), click(TileKind.SPEAKER, running.copy(speakerOn = true)))
        // Not in the foreground, or no core: Oink opens on 设备 with the explicit state, never a toggle.
        assertEquals(TileClick.Open(LaunchRequest(tab = HomeTab.DEVICES, speaker = true, id = "r")),
            click(TileKind.SPEAKER, running.copy(inForeground = false)))
        assertEquals(TileClick.Open(LaunchRequest(tab = HomeTab.DEVICES, speaker = false, id = "r")),
            click(TileKind.SPEAKER, running.copy(speakerOn = true, connected = false)))
    }

    @Test fun microphoneNeedsTheForegroundAndThePermissionToSwitchDirectly() {
        val openOn = LaunchRequest(tab = HomeTab.DEVICES, microphone = true, id = "r")
        assertEquals(TileClick.SetMicrophone(true, openOn), click(TileKind.MICROPHONE, running))
        assertEquals(TileClick.Open(openOn), click(TileKind.MICROPHONE, running.copy(microphonePermission = false)))
        assertEquals(TileClick.Open(openOn), click(TileKind.MICROPHONE, running.copy(inForeground = false)))
        // Turning it off needs no permission.
        val openOff = LaunchRequest(tab = HomeTab.DEVICES, microphone = false, id = "r")
        assertEquals(TileClick.SetMicrophone(false, openOff), click(TileKind.MICROPHONE, running.copy(microphoneOn = true, microphonePermission = false)))
        assertEquals(TileClick.Open(openOff), click(TileKind.MICROPHONE, running.copy(microphoneOn = true, inForeground = false)))
    }
}
