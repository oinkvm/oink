package com.hachiwii.oink

import uniffi.oink_mobile_ffi.*

internal fun CardAction.toControl(): ControlAction = when (this) {
    is CardAction.Invoke -> ControlAction(token, command)
    is CardAction.OpenPermissions -> ControlAction(null, needsPermissions = true)
    is CardAction.Unavailable -> ControlAction(null, unavailable = reason)
}

private fun ResourceCard.toState(shared: CardAction, local: CardAction, disabled: CardAction? = null, enabled: CardAction? = null) =
    ResourceState(mode, condition, action.toControl(), shared.toControl(), local.toControl(), disabled?.toControl(), enabled?.toControl())

internal fun DeviceCardStatus.toControls(): DeviceControls {
    val a = actions
    val speakerVolume = speaker.forwardingControlsToken?.let { token ->
        speaker.forwarding?.let { VolumeControl("扬声器转发音量", token, it.volumePercent.toInt(), it.muted, VolumeKind.Forwarding) }
    }
    val micVolume = microphone.controlsToken?.let { token ->
        microphone.forwarding?.let { VolumeControl("麦克风转发音量", token, it.volumePercent.toInt(), it.muted, VolumeKind.Forwarding) }
    }
    return DeviceControls(
        screen = screen.toState(a.screenShared, a.screenLocal),
        mouse = mouse.toState(a.mouseShared, a.mouseLocal, a.mouseDisabled),
        keyboard = keyboard.resource.toState(a.keyboardShared, a.keyboardLocal, a.keyboardDisabled, a.keyboardEnabled),
        clipboard = clipboard.toState(a.clipboardShared, a.clipboardLocal),
        speaker = SpeakerState(speaker.isExit, speaker.activity, speaker.makeExitAction.toControl(),
            a.forwardingEnabled.toControl(), a.forwardingDisabled.toControl(), speakerVolume, speaker.forwardingEnabled),
        microphone = MicrophoneState(microphone.mode, microphone.shareAction.toControl(), microphone.localAction.toControl(),
            microphone.receiveAction.toControl(), micVolume),
        game = GameState(game.preference, game.active, game.detected, game.actions.auto.toControl(),
            game.actions.on.toControl(), game.actions.off.toControl()),
        focus = FocusLockState(focus.locked, focus.lockedElsewhere, focus.action.toControl()),
    )
}

internal fun CardConnection.toConnection(): DeviceConnection = when (this) {
    is CardConnection.ThisDevice -> DeviceConnection.THIS_DEVICE
    is CardConnection.Online -> DeviceConnection.ONLINE
    is CardConnection.Offline -> when (reason) {
        OfflineKind.DISCONNECTED -> DeviceConnection.DISCONNECTED
        OfflineKind.CONNECTING -> DeviceConnection.CONNECTING
        OfflineKind.ADDRESS_STALE -> DeviceConnection.ADDRESS_STALE
        OfflineKind.PROTOCOL_INCOMPATIBLE -> DeviceConnection.PROTOCOL_INCOMPATIBLE
    }
}

internal fun DeviceCard.toDevice(): Device {
    val actions = buildList {
        fun action(title: String, value: CardAction) {
            if (value is CardAction.Invoke) add(DeviceAction(title, value.token))
        }
        pause?.let { action(if (it.paused) "恢复此设备" else "暂停此设备", it.action) }
        status?.let { s ->
            val a = s.actions
            action("共享屏幕", a.screenShared); action("屏幕仅本机", a.screenLocal)
            action("共享鼠标", a.mouseShared); action("鼠标仅本机", a.mouseLocal); action("禁用鼠标", a.mouseDisabled)
            action("共享键盘", a.keyboardShared); action("键盘仅本机", a.keyboardLocal)
            action("禁用键盘", a.keyboardDisabled); action("启用键盘", a.keyboardEnabled)
            action("共享剪贴板", a.clipboardShared); action("剪贴板仅本机", a.clipboardLocal)
            action("启用音频转发", a.forwardingEnabled); action("关闭音频转发", a.forwardingDisabled)
            // The same slot carries MakeAudioExit or ClearAudioExit; title it by the command.
            val exit = s.speaker.makeExitAction
            action(if (exit is CardAction.Invoke && exit.command == CardCommand.CLEAR_AUDIO_EXIT) "取消音频出口" else "设为音频出口", exit)
            action("麦克风共享", s.microphone.shareAction); action("麦克风仅本机", s.microphone.localAction)
            action("接收麦克风", s.microphone.receiveAction)
            action("游戏模式自动", s.game.actions.auto); action("开启游戏模式", s.game.actions.on); action("关闭游戏模式", s.game.actions.off)
            action(if (s.focus.locked) "解除指针锁定" else "锁定指针到此设备", s.focus.action)
        }
    }
    val controls = status?.toControls()
    val volumes = listOfNotNull(controls?.speaker?.forwardingVolume, controls?.microphone?.volume)
    val signal = when (val state = connection) {
        is CardConnection.Online -> state.signal
        is CardConnection.Offline -> state.signal
        else -> null
    }
    val latency = when (signal) {
        is Signal.Measured -> signal.rttMs
        is Signal.Unknown -> signal.rttMs
        is Signal.Offline -> signal.rttMs
        else -> null
    }
    val linkState = connection.toConnection()
    val resources = status?.let { "屏幕：${it.screen.condition} · 鼠标：${it.mouse.mode ?: it.mouse.condition} · 键盘：${it.keyboard.resource.mode ?: it.keyboard.resource.condition}\n剪贴板：${it.clipboard.condition} · 音频：${it.speaker.activity}" }.orEmpty()
    return Device(device, name, connection is CardConnection.Online || isSelf, isContentTargetPc,
        actions, detail = when (linkState) {
            DeviceConnection.THIS_DEVICE -> "本机"
            DeviceConnection.ONLINE -> "在线"
            DeviceConnection.PROTOCOL_INCOMPATIBLE -> "版本过低，请升级"
            DeviceConnection.CONNECTING -> "正在连接"
            else -> "离线"
        } + if (resources.isNotEmpty()) "\n$resources" else "", isSelf = isSelf, platform = os.name.lowercase(),
        latencyMillis = latency, lastSeenMillis = lastSeenMs?.toLong(), volumes = volumes,
        fingerprint = fingerprint, connection = linkState, incompatible = linkState == DeviceConnection.PROTOCOL_INCOMPATIBLE,
        paused = pause?.paused, pause = pause?.action?.toControl(), controls = controls,
        mobileAudio = mobileAudio?.let { MobileAudioState(it.speaker, it.microphone, it.forward) },
        androidPrivileged = androidPrivileged)
}

/** Latency updates carry their own revision counter (not the card revision): apply newer ones of the same stream. */
internal fun applySignals(devices: List<Device>, signals: DeviceCardSignals): List<Device> {
    val rtt = signals.signals.associate { it.device to it.rttMs }
    return devices.map { if (it.id in rtt) it.copy(latencyMillis = rtt[it.id]) else it }
}

internal fun NearbyRow.toDevice(): Device {
    val endpoint = SocketEndpoint(selected.address, selected.port.toInt())
    return Device(device, name, false, pairTarget = device.takeIf { compatible },
        detail = if (compatible) "${selected.address}:${selected.port} · $os" else "版本过低，请升级",
        platform = os.lowercase(), fingerprint = runCatching { deviceFingerprint(device) }.getOrDefault(""),
        incompatible = !compatible, address = endpoint,
        addresses = addresses.map { SocketEndpoint(it.address, it.port.toInt()) }, groupSize = count?.toInt())
}
