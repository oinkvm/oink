package com.hachiwii.oink

import com.hachiwii.oink.ui.NearbyUi
import org.junit.Assert.*
import org.junit.Test
import uniffi.oink_mobile_ffi.*

/** Repository state -> presentation models (OinkScreen): one state source, nothing invented. */
class UiMappingTest {
    private fun on(token: String, command: CardCommand? = CardCommand.SET_LOCAL_OPTION) = ControlAction(token, command)
    private val unsupported = ControlAction(null, unavailable = CardActionUnavailable.UNSUPPORTED)
    private fun resource(mode: ResourceMode?, condition: CardCondition = CardCondition.READY) =
        ResourceState(mode, condition, on("tap"), on("shared"), on("local"), on("disabled"))

    private fun controls(
        mouse: ResourceMode? = ResourceMode.SHARED, keyboard: ResourceMode? = ResourceMode.DISABLED,
        isExit: Boolean = false, activity: SpeakerActivity = SpeakerActivity.IDLE, forwarding: VolumeControl? = null, forwardingEnabled: Boolean? = null,
        mic: MicrophoneMode = MicrophoneMode.RECEIVER, micVolume: VolumeControl? = VolumeControl("mic", "mic-token", 80, false, VolumeKind.Forwarding),
        game: GameModePreference? = GameModePreference.AUTO, gameActive: Boolean? = true, locked: Boolean = false,
    ) = DeviceControls(
        screen = resource(ResourceMode.SHARED), mouse = resource(mouse), keyboard = resource(keyboard), clipboard = resource(ResourceMode.SHARED),
        speaker = SpeakerState(isExit, activity, on("exit", if (isExit) CardCommand.CLEAR_AUDIO_EXIT else CardCommand.MAKE_AUDIO_EXIT),
            on("fwd-on"), on("fwd-off"), forwarding, forwardingEnabled),
        microphone = MicrophoneState(mic, on("mic-share"), on("mic-local"), on("mic-receive"), micVolume),
        game = GameState(game, gameActive, null, on("auto"), on("game-on"), on("game-off")),
        focus = FocusLockState(locked, false, on("focus", CardCommand.SET_FOCUS_LOCK)),
    )

    private fun pc(controls: DeviceControls?, online: Boolean = true) = Device("pc", "Windows PC", online, computer = true, platform = "windows",
        latencyMillis = 4.0, controls = controls, paused = false, pause = on("pause", CardCommand.SET_DEVICE_PAUSED))

    @Test fun pcCardMapsEveryIndicatorFromTheCard() {
        val peer = pc(controls()).toPeer(null)
        assertEquals("shared", peer.mouse)
        assertEquals("blocked", peer.keyboard)
        assertEquals("local", peer.speaker)
        assertEquals("receive", peer.mic)
        assertEquals(80, peer.micLevel)
        assertTrue(peer.micAdjustable)
        assertEquals("auto", peer.gaming)
        assertTrue(peer.gamingActive)
        assertFalse(peer.offline)
    }

    @Test fun unpublishedOrFaultedResourceIsUnavailableNotShared() {
        val c = controls(mouse = null).copy(keyboard = resource(ResourceMode.SHARED, CardCondition.FAULTED))
        val peer = pc(c).toPeer(null)
        assertEquals("unavailable", peer.mouse)
        assertEquals("unavailable", peer.keyboard)
    }

    @Test fun exitSpeakerUsesTheExitVolumeAndForwardingItsGain() {
        val exitControl = VolumeControl("exit", "exit-token", 40, false, VolumeKind.Output)
        val exit = pc(controls(isExit = true, activity = SpeakerActivity.PLAYING)).toPeer(ExitState("pc", "Windows PC", 40, false, exitControl))
        assertEquals("outlet", exit.speaker)
        assertEquals(40, exit.speakerLevel)
        assertTrue(exit.speakerAdjustable)
        val notAdjustable = pc(controls(isExit = true)).toPeer(ExitState("pc", "Windows PC", 40, false, null))
        assertFalse(notAdjustable.speakerAdjustable)

        val gain = VolumeControl("fwd", "fwd-token", 150, false, VolumeKind.Forwarding)
        val forwarding = pc(controls(activity = SpeakerActivity.FORWARDING, forwarding = gain)).toPeer(null)
        assertEquals("forward", forwarding.speaker)
        assertEquals(150, forwarding.speakerLevel)
        assertEquals("unavailable", pc(controls(activity = SpeakerActivity.FAILED)).toPeer(null).speaker)
    }

    @Test fun idleMicrophoneReceivesOnlyMutedIsLocal() {
        assertEquals("receive", pc(controls(mic = MicrophoneMode.IDLE)).toPeer(null).mic)
        assertEquals("local", pc(controls(mic = MicrophoneMode.MUTED)).toPeer(null).mic)
    }

    @Test fun forwardingFollowsThePcSettingNotWhatIsPlaying() {
        // A forwarding PC with nothing playing (or no resolved exit) reports Idle/Unknown activity.
        assertEquals("forward", pc(controls(activity = SpeakerActivity.IDLE, forwardingEnabled = true)).toPeer(null).speaker)
        assertEquals("forward", pc(controls(activity = SpeakerActivity.UNKNOWN, forwardingEnabled = true)).toPeer(null).speaker)
        assertEquals("local", pc(controls(activity = SpeakerActivity.SOUND_SLEEP, forwardingEnabled = false)).toPeer(null).speaker)
    }

    @Test fun phoneShowsItsEndpointsAndNoInputs() {
        val phone = Device("ph", "Pixel", true, platform = "android",
            mobileAudio = MobileAudioState(MobileEndpointState.ACTIVE, MobileEndpointState.FAILED)).toPeer(null)
        assertTrue(phone.mobile)
        assertEquals("outlet", phone.speaker)
        assertEquals("unavailable", phone.mic)
        assertFalse(phone.speakerAdjustable)
    }

    /** The top bar uses the device-card states, so it draws the PC icon of a phone in that state. */
    @Test fun toolbarAudioUsesTheCardStatesOfAPhone() {
        val receiving = Device("ph", "Pixel", true, platform = "android",
            mobileAudio = MobileAudioState(MobileEndpointState.ACTIVE, MobileEndpointState.STARTING)).toPeer(null)
        assertEquals(receiving.speaker, com.hachiwii.oink.ui.toolbarAudioState(speaker = true, on = true, failed = false))
        assertEquals(receiving.mic, com.hachiwii.oink.ui.toolbarAudioState(speaker = false, on = true, failed = false))
        assertEquals("unavailable", com.hachiwii.oink.ui.toolbarAudioState(speaker = true, on = true, failed = true))
        assertEquals("unavailable", com.hachiwii.oink.ui.toolbarAudioState(speaker = false, on = true, failed = true))
        assertNull(com.hachiwii.oink.ui.toolbarAudioState(speaker = true, on = false, failed = false))
        assertNull(com.hachiwii.oink.ui.toolbarAudioState(speaker = false, on = false, failed = false))
    }

    @Test fun offlineIncompatibleCardStaysOfflineWithItsFlag() {
        val peer = Device("pc", "Old", false, computer = true, platform = "windows", incompatible = true).toPeer(null)
        assertTrue(peer.offline)
        assertTrue(peer.incompatible)
    }

    private val tapped = NearbyUi("peer", "Studio", "mac", "192.168.1.42", 47470, "7KQ4-M2PA-9DXF")
    private fun session(phase: PairPhase, outgoing: Boolean, wrong: Boolean = false, left: Int = 3) =
        PairSession("s", "1", 1u, !outgoing && phase == PairPhase.ENTER_CODE, outgoing = outgoing, phase = phase, attemptsLeft = left,
            wrongCode = wrong, peer = PairPeer("peer", "Studio", "macos"))

    @Test fun pairingSheetFollowsTheCoreSession() {
        val code = sessionView(CoreSnapshot(pairingSession = session(PairPhase.SHOW_CODE, true), pairingCode = "482913"), tapped, null)!!
        assertEquals("out", code.dir)
        assertEquals("code", code.status)
        assertEquals("482913", code.code)
        assertEquals("192.168.1.42:47470", code.peer.address)
        // The initiator keeps showing its code while the peer verifies.
        val verifying = sessionView(CoreSnapshot(pairingSession = session(PairPhase.VERIFYING, true)), tapped, code)!!
        assertEquals("code", verifying.status)
        assertEquals("482913", verifying.code)

        val wrong = sessionView(CoreSnapshot(pairingSession = session(PairPhase.ENTER_CODE, false, wrong = true, left = 2)), tapped, null)!!
        assertEquals("wrong", wrong.status)
        assertEquals(2, wrong.attemptsLeft)
        assertEquals("verifying", sessionView(CoreSnapshot(pairingSession = session(PairPhase.VERIFYING, false)), tapped, null)!!.status)
        assertNull(sessionView(CoreSnapshot(pairingSession = session(PairPhase.ENDED, false)), tapped, null))
        assertNull(sessionView(CoreSnapshot(), tapped, null))
    }

    @Test fun enteredCodeStaysWhileVerifiedAndOnSuccess() {
        val verifying = sessionView(CoreSnapshot(pairingSession = session(PairPhase.VERIFYING, false)), tapped, null, "246810")!!
        assertEquals("246810", verifying.input)
        val success = sessionView(CoreSnapshot(pairingSession = session(PairPhase.COMPLETED, false)), tapped, verifying, "246810")!!
        assertEquals("success", success.status)
        assertEquals("246810", success.input)
        // A wrong code clears the cells for the next try.
        assertEquals("", sessionView(CoreSnapshot(pairingSession = session(PairPhase.ENTER_CODE, false, wrong = true, left = 2)), tapped, null, "111111")!!.input)
    }

    @Test fun pairingFailuresReadAsThePcDoes() {
        assertEquals("对方拒绝了配对", pairProblemText(13u))
        assertEquals("尝试次数已用完", pairProblemText(20u))
        assertEquals("已由另一配对会话接替", pairProblemText(23u))
        assertEquals("该地址上没有 Oink 设备", addressProblemText(AddressNetworkProblem.NOT_OINK, 0u))
        assertEquals("连接超时", addressProblemText(null, 6u))
    }

    @Test fun unavailableCommandsGiveTheirReason() {
        assertEquals("设备不在线", unavailableText(ControlAction(null, unavailable = CardActionUnavailable.OFFLINE)))
        assertEquals("请先在电脑上授予 Oink 所需权限", unavailableText(ControlAction(null, needsPermissions = true)))
        assertEquals("对方的 Oink 版本不支持此操作", unavailableText(unsupported))
    }

    private fun message(id: Long, outgoing: Boolean, state: MobileDeliveryState? = null, failure: MobileDeliveryFailure? = null,
                        download: String = "placeholder", kind: String = "file") =
        TransferMessage(id, 1_000, if (outgoing) "self" else "pc", null, "c$id", kind, "a.pdf", "application/pdf", 2_400_000, null,
            download, null, true, ByteArray(0), isOutgoing = outgoing, deliveryState = state, deliveryFailure = failure,
            downloadStatus = if (outgoing) null else receivedStatus(download))

    @Test fun ownMessagesShowSendingProgressAndTypedFailure() {
        val state = AppState(name = "Phone", transferProgress = mapOf(1L to TransferProgress(50, 100)))
        val devices = mapOf("pc" to pc(controls()))
        val sending = message(1, true, MobileDeliveryState.SENDING).toUi(state, devices)
        assertTrue(sending.self)
        assertFalse(sending.sent)
        assertEquals(50, sending.progress)
        assertEquals("Phone", sending.senderName)
        val failed = message(2, true, MobileDeliveryState.FAILED, MobileDeliveryFailure.REJECTED).toUi(state, devices)
        assertEquals("对方拒绝接收", failed.failure)
        assertNull(message(3, true, MobileDeliveryState.SENT).toUi(state, devices).failure)
    }

    @Test fun receivedFilesShowDownloadState() {
        val state = AppState(transferProgress = mapOf(5L to TransferProgress(30, 100)))
        val devices = mapOf("pc" to pc(controls()))
        assertNull(message(4, false).toUi(state, devices).progress)
        assertEquals(30, message(5, false, download = "downloading").toUi(state, devices).progress)
        assertEquals(100, message(6, false, download = "complete").toUi(state, devices).progress)
        val received = message(4, false).toUi(state, devices)
        assertEquals("Windows PC", received.senderName)
        assertEquals("PDF · 2.4 MB", received.detail)
    }

    @Test fun passwordMarkedTextIsHiddenAndKnowsWhetherItsTextIsHere() {
        val state = AppState()
        val devices = mapOf("pc" to pc(controls()))
        val pending = message(7, false, kind = "text").copy(sensitive = true).toUi(state, devices)
        assertTrue(pending.sensitive)
        assertTrue(pending.textPending)
        val here = message(8, false, download = "complete", kind = "text").copy(text = "pw", sensitive = true).toUi(state, devices)
        assertFalse(here.textPending)
        // Only text is ever hidden.
        assertFalse(message(9, false).copy(sensitive = true).toUi(state, devices).sensitive)
    }

    @Test fun senderAvatarWearsTheCrownOnlyForALivePrivilegedAndroid() {
        val state = AppState(name = "Phone")
        val phone = Device("pc", "Pixel", true, platform = "android", androidPrivileged = true)
        assertTrue(message(1, false).toUi(state, mapOf("pc" to phone)).senderPrivileged)
        assertFalse(message(1, false).toUi(state, mapOf("pc" to phone.copy(online = false))).senderPrivileged)
        assertFalse(message(1, false).toUi(state, mapOf("pc" to phone.copy(androidPrivileged = false))).senderPrivileged)
        // Own messages follow this phone's own card.
        val self = Device("me", "Phone", true, isSelf = true, platform = "android", androidPrivileged = true)
        assertTrue(message(2, true, MobileDeliveryState.SENT).toUi(state, mapOf("me" to self)).senderPrivileged)
        assertFalse(message(2, true, MobileDeliveryState.SENT).toUi(state, emptyMap()).senderPrivileged)
    }

    /** A folder page whose listing failed is a model of its own (no spinner), with or without entries shown. */
    @Test fun aFailedFolderListingStopsLoading() {
        val gone = FolderPageKey(34, 0)
        val more = FolderPageKey(34, 2)
        val state = AppState(
            transferMessages = listOf(message(34, false, kind = "folder")),
            folderPages = mapOf(more to DirectoryPageState(listOf(DirectoryEntry(5, 2, "a.html", "file", 10, 1)), nextOffset = 1)),
            folderFailures = mapOf(gone to FolderFailure("文件夹内容已失效", retryable = false),
                more to FolderFailure("无法获取文件夹内容：连接中断，请重试", retryable = true)),
        )
        val folders = folderModels(state)
        val root = folders.getValue(34L to 0L)
        assertFalse(root.loading)
        assertTrue(root.items.isEmpty())
        assertEquals("文件夹内容已失效", root.error)
        assertFalse(root.retryable)
        val sub = folders.getValue(34L to 2L)
        assertEquals(listOf("a.html"), sub.items.map { it.name })
        assertEquals("无法获取文件夹内容：连接中断，请重试", sub.error)
        assertTrue(sub.retryable)
        // A page without a failure has no error.
        assertNull(folderModels(state.copy(folderFailures = emptyMap())).getValue(34L to 2L).error)
    }
}
