package com.hachiwii.oink

import org.junit.Assert.*
import org.junit.Test
import uniffi.oink_mobile_ffi.*

/** Host-side mapping of core records into the Repository state the UI wires to. */
class HostMappingTest {
    private fun invoke(token: String, command: CardCommand = CardCommand.SET_LOCAL_OPTION) = CardAction.Invoke(command, token)
    private val none = CardAction.Unavailable(CardActionUnavailable.UNSUPPORTED)
    private fun resource(token: String, mode: ResourceMode?) = ResourceCard(invoke(token), CardCondition.READY, mode)

    private fun pcCard(connection: CardConnection = CardConnection.Online(Signal.Measured(4.0))) = DeviceCard(
        isContentTargetPc = true, device = "aa".repeat(32), name = "Windows PC", os = CardOs.WINDOWS, isSelf = false,
        section = CardSection.ONLINE, connection = connection, lastSeenMs = null,
        status = DeviceCardStatus(
            actions = DeviceCardActions(invoke("screen-shared"), invoke("screen-local"), invoke("mouse-shared"), invoke("mouse-local"),
                invoke("mouse-disabled"), invoke("kb-shared"), invoke("kb-local"), invoke("kb-disabled"), invoke("kb-enabled"),
                invoke("clip-shared"), invoke("clip-local"), invoke("fwd-on"), none),
            screen = resource("screen-tap", ResourceMode.SHARED),
            mouse = resource("mouse-tap", ResourceMode.DISABLED),
            keyboard = KeyboardCard(resource("kb-tap", ResourceMode.LOCAL)),
            speaker = SpeakerCard(invoke("exit-clear", CardCommand.CLEAR_AUDIO_EXIT), true, "spk-volume", SpeakerActivity.PLAYING,
                ForwardingCard(150u, false)),
            microphone = MicrophoneCard(MicrophoneMode.RECEIVER, invoke("mic-share"), invoke("mic-local"), none, "mic-volume",
                ForwardingCard(80u, true)),
            clipboard = resource("clip-tap", null),
            game = GameCard(GameActions(invoke("game-auto", CardCommand.SET_GAME_MODE), invoke("game-on", CardCommand.SET_GAME_MODE), none),
                GameModePreference.AUTO, true, true),
            focus = FocusCard(true, false, invoke("focus", CardCommand.SET_FOCUS_LOCK)),
        ),
        pause = LocalPauseCard(invoke("pause", CardCommand.SET_DEVICE_PAUSED), false),
        mobileAudio = null,
        fingerprint = "ABCD-EFGH-JKMN",
        androidPrivileged = false,
    )

    @Test fun deviceCardCarriesCurrentModesAndTypedActions() {
        val device = pcCard().toDevice()
        val controls = requireNotNull(device.controls)
        assertEquals(ResourceMode.DISABLED, controls.mouse.mode)
        assertEquals("mouse-tap", controls.mouse.tap.token)
        assertEquals("mouse-disabled", controls.mouse.disabled?.token)
        assertEquals(ResourceMode.LOCAL, controls.keyboard.mode)
        assertEquals("kb-enabled", controls.keyboard.enabled?.token)
        assertNull(controls.clipboard.mode)
        assertTrue(controls.speaker.isExit)
        assertEquals(CardCommand.CLEAR_AUDIO_EXIT, controls.speaker.exitToggle.command)
        assertEquals(150, controls.speaker.forwardingVolume?.volume)
        assertEquals(CardActionUnavailable.UNSUPPORTED, controls.speaker.forwardingOff.unavailable)
        assertNull(controls.speaker.forwardingOff.token)
        assertEquals(MicrophoneMode.RECEIVER, controls.microphone.mode)
        assertEquals("mic-volume", controls.microphone.volume?.token)
        assertEquals(GameModePreference.AUTO, controls.game.preference)
        assertEquals(true, controls.game.active)
        assertTrue(controls.focus.locked)
        assertEquals(CardCommand.SET_FOCUS_LOCK, controls.focus.toggle.command)
        assertEquals(false, device.paused)
        assertEquals("ABCD-EFGH-JKMN", device.fingerprint)
        assertEquals(DeviceConnection.ONLINE, device.connection)
        assertEquals(4.0, device.latencyMillis!!, 0.0)
        // Old title-based actions name the exit slot by its command, not always "设为音频出口".
        assertTrue(device.actions.any { it.title == "取消音频出口" && it.token == "exit-clear" })
        assertTrue(device.actions.any { it.title == "解除指针锁定" && it.token == "focus" })
    }

    @Test fun protocolIncompatibleIsTyped() {
        val device = pcCard(CardConnection.Offline(OfflineKind.PROTOCOL_INCOMPATIBLE, Signal.Offline(null))).toDevice()
        assertTrue(device.incompatible)
        assertEquals(DeviceConnection.PROTOCOL_INCOMPATIBLE, device.connection)
        assertFalse(device.online)
    }

    @Test fun phoneCardCarriesEndpointStates() {
        val phone = pcCard().copy(isContentTargetPc = false, os = CardOs.ANDROID, isSelf = true, status = null, pause = null,
            connection = CardConnection.ThisDevice,
            mobileAudio = MobileAudioCard(MobileEndpointState.ACTIVE, MobileEndpointState.STARTING, MobileEndpointState.LOCAL)).toDevice()
        assertNull(phone.controls)
        assertEquals(MobileEndpointState.ACTIVE, phone.mobileAudio?.speaker)
        assertEquals(MobileEndpointState.STARTING, phone.mobileAudio?.microphone)
        val forwarding = pcCard().copy(isContentTargetPc = false, os = CardOs.ANDROID, status = null,
            mobileAudio = MobileAudioCard(MobileEndpointState.LOCAL, MobileEndpointState.LOCAL, MobileEndpointState.ACTIVE)).toDevice()
        assertEquals(MobileEndpointState.ACTIVE, forwarding.mobileAudio?.forward)
        assertEquals("forward", forwarding.toPeer(null).speaker)
        assertTrue(phone.online)
    }

    @Test fun privilegedAndroidCardWearsTheCrownOnlyWhileOnline() {
        val android = pcCard().copy(isContentTargetPc = false, os = CardOs.ANDROID, status = null, pause = null,
            mobileAudio = MobileAudioCard(MobileEndpointState.LOCAL, MobileEndpointState.LOCAL, MobileEndpointState.LOCAL))
        val plain = android.toDevice()
        assertFalse(plain.androidPrivileged)
        assertFalse(plain.toPeer(null).privileged)
        val running = android.copy(androidPrivileged = true).toDevice()
        assertTrue(running.androidPrivileged)
        assertTrue(running.toPeer(null).privileged)
        assertEquals("android", running.toPeer(null).platform)
        // Offline is never crowned, even from a stale flag.
        val offline = running.copy(online = false)
        assertFalse(offline.toPeer(null).privileged)
    }

    @Test fun aPhoneThatIsTheExitShowsItsExitVolume() {
        val phone = Device("iphone", "iPhone", true, platform = "ios",
            mobileAudio = MobileAudioState(MobileEndpointState.ACTIVE, MobileEndpointState.LOCAL))
        val exit = ExitState("iphone", "iPhone", 50, false, VolumeControl("iPhone 输出音量", "token", 50, false, VolumeKind.Output))
        val peer = phone.toPeer(exit)
        assertEquals("outlet", peer.speaker)
        assertEquals(50, peer.speakerLevel)
        assertTrue(peer.speakerAdjustable)
        // Another device's exit volume is not this phone's.
        val other = phone.toPeer(exit.copy(deviceId = "macMini"))
        assertEquals(0, other.speakerLevel)
        assertFalse(other.speakerAdjustable)
    }

    @Test fun latencySignalsUpdateOnlyListedDevices() {
        val devices = listOf(Device("a", "A", true, latencyMillis = 1.0), Device("b", "B", true, latencyMillis = 2.0))
        val updated = applySignals(devices, DeviceCardSignals("s", 7u, 0u, listOf(DeviceCardSignal("b", 9.5))))
        assertEquals(1.0, updated[0].latencyMillis!!, 0.0)
        assertEquals(9.5, updated[1].latencyMillis!!, 0.0)
    }

    @Test fun pairingCodesMapToReasons() {
        assertEquals(PairFailureReason.DECLINED, PairFailureReason.of(13u))
        assertEquals(PairFailureReason.TIMEOUT, PairFailureReason.of(6u))
        assertEquals(PairFailureReason.TIMEOUT, PairFailureReason.of(45u))
        assertEquals(PairFailureReason.SUPERSEDED, PairFailureReason.of(23u))
        assertEquals(PairFailureReason.TOO_MANY_ATTEMPTS, PairFailureReason.of(20u))
        assertEquals(PairFailureReason.INCOMPATIBLE, PairFailureReason.of(9u))
        assertEquals(PairFailureReason.OTHER, PairFailureReason.of(999u))
        val failure = PairProblem(6u, 1u, "x", AddressNetworkProblem.TIMEOUT).toFailure()
        assertEquals(PairFailureReason.TIMEOUT, failure.reason)
        assertEquals(PairPhase.SHOW_CODE, PairPhase.of(1u))
        assertEquals(PairPhase.VERIFYING, PairPhase.of(5u))
        assertEquals(PairPhase.ENDED, PairPhase.of(8u))
    }

    @Test fun nearbyRowKeepsAddressAndGroupSize() {
        val row = NearbyRow("bb".repeat(32), "Studio", "macos", DiscoveredGroupRef("g", null), 3u,
            listOf(SocketAddress("192.168.1.42", 47470u, 0u), SocketAddress("fe80::1", 47470u, 2u)),
            SocketAddress("192.168.1.42", 47470u, 0u), 0u, IdentityEvidence.UNVERIFIED, true, true, null)
        val device = row.toDevice()
        assertEquals(SocketEndpoint("192.168.1.42", 47470), device.address)
        assertEquals(2, device.addresses.size)
        assertEquals(3, device.groupSize)
        assertEquals("macos", device.platform)
        assertFalse(device.incompatible)
        assertTrue(row.copy(compatible = false).toDevice().incompatible)
    }

    @Test fun systemDeviceNamePrefersTheUserVisibleName() {
        assertEquals("示例手机", SystemDeviceName.choose("  示例手机 ", "Xiaomi", "2211133C"))
        assertEquals("Xiaomi 2211133C", SystemDeviceName.choose(null, "Xiaomi", "2211133C"))
        assertEquals("Google Pixel 8", SystemDeviceName.choose("", "Google", "Pixel 8"))
        assertEquals("Samsung SM-S918B", SystemDeviceName.choose(null, "samsung", "SM-S918B"))
        assertEquals("Example Phone", SystemDeviceName.choose(null, "Example", "Example Phone"))
        assertEquals("Android", SystemDeviceName.choose(null, "", ""))
    }

    private fun message(id: Long, created: Long) = TransferMessage(id, created, "s", null, "c$id", "Text", "", null, 0, "t", "Complete", null, true, ByteArray(0))

    @Test fun conversationIsOldestFirstAndPagesPrepend() {
        val newest = listOf(message(5, 50), message(4, 40))
        val first = mergeTransferPage(emptyList(), newest, reset = true)
        assertEquals(listOf(4L, 5L), first.map { it.id })
        val older = mergeTransferPage(first, listOf(message(3, 30), message(2, 20)), reset = false)
        assertEquals(listOf(2L, 3L, 4L, 5L), older.map { it.id })
    }

    @Test fun recordUpdatesKeepOrder() {
        val list = listOf(message(1, 10), message(3, 30))
        assertEquals(listOf(1L, 2L, 3L), upsertTransfer(list, 2, message(2, 20)).map { it.id })
        assertEquals(listOf(1L, 3L, 4L), upsertTransfer(list, 4, message(4, 40)).map { it.id })
        assertEquals("p", upsertTransfer(list, 3, message(3, 30).copy(localPath = "p")).last().localPath)
        assertEquals(listOf(1L), upsertTransfer(list, 3, null).map { it.id })
    }

    @Test fun everyImageMimeIsSentAsImage() {
        val draft = ShareDraft("hi", listOf(
            LocalItem("/a.jpg", "a.jpg", "image/jpeg", 10),
            LocalItem("/b.heic", "b.heic", "image/heic", 10),
            LocalItem("/c.pdf", "c.pdf", "application/pdf", 10),
            LocalItem("/dir", "dir", "", 0),
        ))
        // Pictures carry their size as shown (a portrait photo stays portrait).
        val items = shareItems(draft, imageSize = { if (it == "/a.jpg") 3024 to 4032 else 4000 to 3000 }) { it == "/dir" }
        assertTrue(items[0] is MobileShareItem.Text)
        assertEquals("image/jpeg", (items[1] as MobileShareItem.Image).mime)
        assertEquals(3024u, (items[1] as MobileShareItem.Image).width)
        assertEquals(4032u, (items[1] as MobileShareItem.Image).height)
        assertEquals("image/heic", (items[2] as MobileShareItem.Image).mime)
        assertEquals(4000u, (items[2] as MobileShareItem.Image).width)
        assertTrue(items[3] is MobileShareItem.File)
        assertTrue(items[4] is MobileShareItem.Folder)
    }

    /** User 2026-11-12: text goes with its rich text, one item carrying both. */
    @Test fun textIsSentWithItsHtml() {
        val items = shareItems(ShareDraft("bold", html = "<b>bold</b>")) { false }
        assertEquals(listOf(MobileShareItem.Text("bold", "<b>bold</b>", null)), items)
        assertEquals(listOf(MobileShareItem.Text("plain", null, null)), shareItems(ShareDraft("plain")) { false })
    }

    @Test fun iconAcceptsDesignTokenAccentNames() {
        assertEquals("violet", AppIconManager.accentOf("violet"))
        assertEquals(listOf("pink", "orange", "violet", "blue", "teal"), AppIconManager.accents)
        assertEquals("pink", AppIconManager.accentOf("unknown"))
    }

    @Test fun rejectedEditsAreNeverSuccess() {
        assertEquals("IP 地址格式不正确", editMessage(EditResult.Rejected(UserFacingCode.INVALID_ADDRESS, "x")))
        assertEquals("不能屏蔽本机", editMessage(EditResult.BlockRejected(0u, null)))
        assertEquals("屏蔽列表已满（最多 1024 台）", editMessage(EditResult.BlockRejected(2u, 1024u)))
    }

    @Test fun progressPercent() {
        assertEquals(50, TransferProgress(5, 10).percent)
        assertNull(TransferProgress(5, null).percent)
    }
}
