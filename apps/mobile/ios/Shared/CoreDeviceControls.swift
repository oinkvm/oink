import Foundation

/// This phone's own speaker / microphone (top-bar switches, requirement 3).
struct IOSLocalAudio: Equatable {
    /// The user's switch state (local intent).
    var speakerOn = false
    var microphoneOn = false
    /// Why the native audio cannot run right now (permission, interruption).
    var issue: String?
    #if OINK_MOBILE_CORE_FFI
    /// Core view from this phone's card: Local / Starting / Active (playing
    /// as the group exit, or sending) / Failed / Unavailable.
    var speaker: MobileEndpointState?
    var microphone: MobileEndpointState?
    #endif
}

#if OINK_MAIN_APP
extension IOSLocalAudio {
    static func issue(_ state: MobileAudioSession.State) -> String? {
        switch state {
        case .permissionDenied: return "未获得麦克风权限，请在系统设置中允许。"
        case .interrupted: return "音频被系统中断。"
        case .needsLocalResume: return "音频已暂停，请重新打开开关。"
        case .failed(let detail): return detail.isEmpty ? "音频不可用。" : detail
        case .stopped, .preparing, .running, .paused: return nil
        }
    }
}
#endif

enum IOSInputChoice { case shared, local, disabled }
enum IOSMicrophoneChoice { case local, receive, share }

/// A device control the core refused or could not apply, with the text to show.
struct IOSControlError: LocalizedError, Equatable {
    let message: String
    var errorDescription: String? { message }
}

#if OINK_MOBILE_CORE_FFI
/// Manual-add (address query) failure, typed so the sheet can tell「该地址上
/// 没有 Oink 设备」from timeouts, unreachable networks and old versions.
struct IOSManualAddProblem: Equatable {
    let network: AddressNetworkProblem?
    let text: String

    init(_ problem: PairProblem) {
        network = problem.network
        text = CoreSession.pairProblemText(problem)
    }

    static func text(_ network: AddressNetworkProblem) -> String {
        switch network {
        case .invalidAddress: return "地址格式不正确"
        case .unreachable: return "目标主机不可达"
        case .timeout: return "连接超时"
        case .refused: return "该地址上没有 Oink 设备"
        case .incompatible: return "对方版本过低，请升级"
        case .notOink: return "该地址上没有 Oink 设备"
        case .selfDevice: return "不能与当前设备配对"
        case .noNetwork: return "手机没有可用的网络"
        case .busy: return "查询过多，请稍后重试"
        }
    }
}

extension CoreSession {
    /// List / sheet state of one card. Desktops report their resources in
    /// `status`; phones only report their audio endpoints (`mobileAudio`).
    nonisolated static func controls(_ card: DeviceCard, exit: ExitVolumeCard?) -> DeviceControls {
        var value = DeviceControls()
        value.paused = card.pause?.paused == true
        if let status = card.status {
            func input(_ resource: ResourceCard) -> DeviceControls.InputShare {
                switch resource.condition {
                case .faulted, .unsupported, .stateUnknown, .stateStale: return .unavailable
                default: break
                }
                switch resource.mode {
                case .shared: return .shared
                case .local: return .local
                case .disabled: return .blocked
                case nil: return .unavailable
                }
            }
            value.mouse = input(status.mouse)
            value.keyboard = input(status.keyboard.resource)
            value.locked = status.focus.locked
            switch status.game.preference {
            case .auto: value.gaming = .auto
            case .on: value.gaming = .on
            case .off, nil: value.gaming = .off
            }
            value.gameActive = status.game.active ?? (status.game.preference == .on)
            let speaker = status.speaker
            if speaker.isExit {
                value.speaker = .outlet
                if let exit, exit.receiverDevice == card.device {
                    value.speakerLevel = Int(exit.volumePercent ?? 0)
                    value.speakerMuted = exit.muted ?? false
                }
            } else {
                switch speaker.activity {
                case .forwarding, .soundSleep, .preparing, .waitingForExit: value.speaker = .forward
                case .failed, .unknown: value.speaker = .unavailable
                case .idle, .playing, .paused, .sleeping: value.speaker = .local
                }
                value.speakerWaiting = speaker.activity == .waitingForExit
                if let forwarding = speaker.forwarding {
                    value.speakerLevel = Int(forwarding.volumePercent)
                    value.speakerMuted = forwarding.muted
                }
            }
            switch status.microphone.mode {
            case .source: value.mic = .send
            case .receiver: value.mic = .receive
            case .unavailable: value.mic = .unavailable
            case .idle, .muted: value.mic = .local
            }
            if let forwarding = status.microphone.forwarding {
                value.micLevel = Int(forwarding.volumePercent)
                value.micMuted = forwarding.muted
            }
        } else if let audio = card.mobileAudio {
            switch audio.speaker {
            case .active, .starting: value.speaker = .outlet
            case .failed, .unavailable: value.speaker = .unavailable
            case .local: value.speaker = .local
            }
            if audio.forward == .active || audio.forward == .starting { value.speaker = .forward }
            // A phone that is the group's exit shows its exit volume like a PC
            // exit: Android's system media volume or an iPhone's playout gain.
            if value.speaker == .outlet, let exit, exit.receiverDevice == card.device {
                value.speakerLevel = Int(exit.volumePercent ?? 0)
                value.speakerMuted = exit.muted ?? false
            }
            switch audio.microphone {
            case .active, .starting: value.mic = .send
            case .failed, .unavailable: value.mic = .unavailable
            case .local: value.mic = .local
            }
        }
        return value
    }
}

@MainActor
extension CoreSession {
    /// The full core card of a device (status, actions, tokens).
    func deviceCard(_ device: String) -> DeviceCard? {
        cards?.cards.first(where: { $0.device == device })?.source
    }

    private func status(_ device: String) throws -> DeviceCardStatus {
        guard let card = deviceCard(device) else { throw IOSControlError(message: "设备不在列表中。") }
        guard let status = card.status else { throw IOSControlError(message: "无法远程控制此设备。") }
        return status
    }

    /// Runs one card action and refreshes the cards. Succeeds for Applied
    /// and NoChange; every refusal or failure throws with its reason.
    func invoke(_ action: CardAction) async throws {
        switch action {
        case .invoke(_, let token):
            let owner = try requireLiveSession()
            try await finish(owner.invokeDeviceCardAction(request: UUID().uuidString, token: token), owner: owner)
        case .openPermissions:
            throw IOSControlError(message: "请先在该电脑上授予 Oink 所需的系统权限。")
        case .unavailable(let reason):
            throw IOSControlError(message: Self.text(reason))
        }
    }

    private func finish(_ reply: DeviceControlReply, owner: MobileSession) async throws {
        // The mobile owner answers every control call with its final result
        // (the PC reply, or a typed OutcomeUnknown after its own deadline).
        guard case .completed(let completed) = reply else {
            throw IOSControlError(message: "未收到设备确认，请查看设备状态。")
        }
        switch completed.result {
        case .applied, .noChange:
            try? await refreshCoreSnapshots(owner)
        case .refused(let reason), .failed(let reason):
            try? await refreshCoreSnapshots(owner)
            throw IOSControlError(message: Self.text(reason))
        }
    }

    // MARK: Mouse / keyboard

    /// Single tap: Shared → Local, Local or Disabled → Shared.
    func toggleMouse(_ device: String) async throws { try await invoke(status(device).mouse.action) }
    func toggleKeyboard(_ device: String) async throws { try await invoke(status(device).keyboard.resource.action) }

    /// Long-press menu choice.
    func setMouse(_ device: String, _ choice: IOSInputChoice) async throws {
        let actions = try status(device).actions
        switch choice {
        case .shared: try await invoke(actions.mouseShared)
        case .local: try await invoke(actions.mouseLocal)
        case .disabled: try await invoke(actions.mouseDisabled)
        }
    }

    func setKeyboard(_ device: String, _ choice: IOSInputChoice) async throws {
        let status = try status(device)
        switch choice {
        case .shared:
            // Leaving Disabled re-enables the keyboard first (KeyboardEnabled).
            try await invoke(status.keyboard.resource.mode == .disabled ? status.actions.keyboardEnabled : status.actions.keyboardShared)
        case .local: try await invoke(status.actions.keyboardLocal)
        case .disabled: try await invoke(status.actions.keyboardDisabled)
        }
    }

    // MARK: Game mode / pointer lock / pause

    func setGameMode(_ device: String, _ value: GameModePreference) async throws {
        let actions = try status(device).game.actions
        switch value {
        case .auto: try await invoke(actions.auto)
        case .on: try await invoke(actions.on)
        case .off: try await invoke(actions.off)
        }
    }

    /// Locks the group pointer to `device` or releases it. Locking also turns
    /// game mode off, as the control sheet design specifies.
    func setFocusLock(_ device: String, locked: Bool) async throws {
        let focus = try status(device).focus
        guard focus.locked != locked else { return }
        try await invoke(focus.action)
        if locked, let game = deviceCard(device)?.status?.game, game.preference != .off {
            try await invoke(game.actions.off)
        }
    }

    func setDevicePaused(_ device: String, _ paused: Bool) async throws {
        guard let pause = deviceCard(device)?.pause else { throw IOSControlError(message: "此设备不支持暂停。") }
        guard pause.paused != paused else { return }
        try await invoke(pause.action)
    }

    // MARK: Audio

    /// Makes `device` the group audio exit (`true`) or returns it to local.
    func setSpeakerExit(_ device: String, _ exit: Bool) async throws {
        let action = try status(device).speaker.makeExitAction
        // The action toggles; tell its direction by command, never by title.
        if case .invoke(let command, _) = action {
            let makes = command == .makeAudioExit
            guard makes == exit else { return }
        }
        try await invoke(action)
    }

    func setSpeakerForwarding(_ device: String, _ enabled: Bool) async throws {
        let actions = try status(device).actions
        try await invoke(enabled ? actions.forwardingEnabled : actions.forwardingDisabled)
    }

    /// Speaker mode page: 本地 (no forwarding), 转发到出口 (forwarding on), 作为出口.
    /// Leaving the exit role clears the exit first; the forwarding option then
    /// decides between local and forward.
    func setSpeakerMode(_ device: String, _ mode: DeviceControls.SpeakerMode) async throws {
        switch mode {
        case .outlet:
            try await setSpeakerExit(device, true)
        case .forward, .local:
            if try status(device).speaker.isExit { try await setSpeakerExit(device, false) }
            try await setSpeakerForwarding(device, mode == .forward)
        case .unavailable:
            break
        }
    }

    /// Volume capsule of the speaker: the exit volume (≤ 100) while the device
    /// is the exit, otherwise its forwarding gain (0–200).
    func setSpeakerLevel(_ device: String, percent: Int) async throws {
        // A phone exit has no desktop status; its capsule is the exit volume.
        if deviceCard(device)?.status == nil, let exit = exitVolume, exit.receiverDevice == device {
            try await setExitVolume(percent: UInt8(clamping: percent), muted: exit.muted ?? false)
            return
        }
        let status = try status(device)
        if status.speaker.isExit {
            guard let exit = exitVolume, exit.receiverDevice == device else {
                throw IOSControlError(message: "当前音频出口不能调节音量。")
            }
            try await setExitVolume(percent: UInt8(clamping: percent), muted: exit.muted ?? false)
        } else {
            try await setSpeakerVolume(device, percent: UInt16(clamping: percent), muted: status.speaker.forwarding?.muted ?? false)
        }
    }

    func setMicrophoneLevel(_ device: String, percent: Int) async throws {
        let muted = try status(device).microphone.forwarding?.muted ?? false
        try await setMicrophoneVolume(device, percent: UInt16(clamping: percent), muted: muted)
    }

    func setMicrophoneMode(_ device: String, _ choice: IOSMicrophoneChoice) async throws {
        let microphone = try status(device).microphone
        switch choice {
        case .local: try await invoke(microphone.localAction)
        case .receive: try await invoke(microphone.receiveAction)
        case .share: try await invoke(microphone.shareAction)
        }
    }

    /// Forwarding gain 0–200 of `device`'s speaker.
    func setSpeakerVolume(_ device: String, percent: UInt16, muted: Bool) async throws {
        guard let token = try status(device).speaker.forwardingControlsToken else {
            throw IOSControlError(message: "此设备当前不能调节转发音量。")
        }
        try await setForwardingControls(token: token, percent: percent, muted: muted)
    }

    /// Microphone slider of a receiving device: changes the sender's gain
    /// (requirement 3). When this phone is the sender the token is local.
    func setMicrophoneVolume(_ device: String, percent: UInt16, muted: Bool) async throws {
        guard let token = try status(device).microphone.controlsToken else {
            throw IOSControlError(message: "此设备当前不能调节麦克风音量。")
        }
        try await setForwardingControls(token: token, percent: percent, muted: muted)
    }

    private func setForwardingControls(token: String, percent: UInt16, muted: Bool) async throws {
        let owner = try requireLiveSession()
        try await finish(owner.setForwardingControls(request: UUID().uuidString, token: token,
                                                     volume: min(percent, 200), muted: muted), owner: owner)
    }

    /// Volume of the current group audio exit, capped at 100.
    func setExitVolume(percent: UInt8, muted: Bool) async throws {
        guard let card = exitVolume, card.canSet, let token = card.versionToken else {
            throw IOSControlError(message: "当前音频出口不能调节音量。")
        }
        let owner = try requireLiveSession()
        try await finish(owner.setExitVolume(request: UUID().uuidString, token: token,
                                             volume: min(percent, 100), muted: muted), owner: owner)
    }

    /// This phone's own microphone gain (0–200) while it is a sender.
    func microphoneControls() -> MobileMicrophoneControls? { mobileSession?.media().microphoneControls() }

    @discardableResult
    func setMicrophoneControls(percent: UInt16, muted: Bool) throws -> MobileMicrophoneControls {
        try requireLiveSession().media().setMicrophoneControls(volumePercent: min(percent, 200), muted: muted)
    }

    #if OINK_MAIN_APP
    /// Top-bar speaker switch: this phone plays group audio (exit) or not.
    func setLocalSpeaker(_ on: Bool) async throws {
        guard let owner = audioOwner, mobileSession != nil else { throw IOSControlError(message: "音频会话尚未就绪。") }
        await owner.setLocalIntent(playback: on, microphone: owner.audio.microphoneEnabled)
    }

    /// Top-bar microphone switch: this phone sends its microphone or not.
    /// Asks for the microphone permission the first time.
    func setLocalMicrophone(_ on: Bool) async throws {
        guard let owner = audioOwner, mobileSession != nil else { throw IOSControlError(message: "音频会话尚未就绪。") }
        if await owner.setLocalIntent(playback: owner.audio.playbackEnabled, microphone: on) == .microphoneDenied {
            throw IOSControlError(message: IOSLocalAudio.issue(.permissionDenied) ?? "")
        }
    }
    #endif

    // MARK: Membership

    func removeMember(_ device: String) async throws {
        let owner = try requireLiveSession()
        try await owner.removeMember(device: device)
        try await refreshCoreSnapshots(owner)
    }

    /// Manual address first (if any), then the addresses discovery sees.
    func memberAddresses(_ device: String) async throws -> [String] {
        try await requireLiveSession().memberAddresses(device: device)
    }

    /// Edit IP: `a.b.c.d` (port 47470) or `a.b.c.d:port`; empty clears it.
    /// Throws with the reason; only a return means「IP 已更新」.
    func setMemberAddress(_ device: String, address: String) async throws {
        let owner = try requireLiveSession()
        do {
            try await owner.setMemberAddress(device: device, address: address.trimmingCharacters(in: .whitespacesAndNewlines))
        } catch MobileError.Command(let code, _) {
            switch code {
            case .invalidAddress: throw IOSControlError(message: "地址格式不正确。")
            case .notMember: throw IOSControlError(message: "该设备已不在设备组中。")
            case .persistenceFailed: throw IOSControlError(message: "无法保存地址。")
            default: throw IOSControlError(message: "无法更新地址。")
            }
        }
    }

    func block(_ device: String) async throws {
        let owner = try requireLiveSession()
        try applyBlockReply(await owner.block(request: UUID().uuidString, device: device))
    }

    func unblock(_ device: String) async throws {
        let owner = try requireLiveSession()
        try applyBlockReply(await owner.unblock(request: UUID().uuidString, device: device))
    }

    private func applyBlockReply(_ reply: BlockReply) throws {
        switch reply {
        case .edited(let edited):
            blockedDevices = edited.snapshot.entries
            blocked = IOSBlockedDevicesSnapshot(devices: edited.snapshot.entries.map(\.device))
        case .error(let error):
            switch error.code {
            case 0: throw IOSControlError(message: "不能屏蔽本机。")
            case 2: throw IOSControlError(message: "屏蔽列表已满（最多 \(error.limit ?? 0) 台）。")
            default: throw IOSControlError(message: "无法保存屏蔽列表。")
            }
        }
    }

    // MARK: Texts

    static func text(_ reason: CardActionUnavailable) -> String {
        switch reason {
        case .offline: return "设备已离线。"
        case .unsupported: return "此设备不支持该操作。"
        case .stateUnknown, .stateStale: return "正在获取设备状态，请稍候。"
        case .remoteFault: return "设备出现故障，暂时无法操作。"
        case .notMember: return "该设备不在设备组中。"
        case .busy: return "设备正忙，请稍后重试。"
        }
    }

    static func text(_ reason: DeviceControlFailure) -> String {
        switch reason {
        case .offline: return "设备已离线。"
        case .unsupported: return "此设备不支持该操作。"
        case .staleBoot, .staleRevision, .stateConflict: return "设备状态已变化，请重试。"
        case .notMember: return "该设备不在设备组中。"
        case .resourceFault: return "设备暂时无法执行此操作。"
        case .invalidValue: return "数值无效。"
        case .busy: return "设备已暂停或正忙，请稍后重试。"
        case .persistenceFailed: return "设备未能保存设置。"
        case .applyFailed: return "设备未能应用设置。"
        case .linkLost: return "连接已断开。"
        case .outcomeUnknown: return "未收到设备确认，请查看设备状态。"
        case .notExit: return "该设备不是音频出口。"
        case .routeChanged: return "音频路由已变化，请重试。"
        case .outputChanged: return "输出设备已变化，请重试。"
        case .outputUnavailable: return "输出设备不可用。"
        case .volumeUnsupported: return "此设备不支持调节音量。"
        case .permissionDenied: return "设备缺少所需权限。"
        case .superseded: return "已被更新的操作取代。"
        case .unsupportedOption: return "电脑版本不支持此设置，请升级。"
        case .unrecognized: return "操作失败。"
        }
    }
}
#endif
