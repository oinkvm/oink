import Foundation

/// Settings whose source of truth is the Rust core (persisted there and
/// applied by it). The host keeps no UserDefaults copy of them.
struct IOSCoreSettings: Equatable {
    var microphoneIdleTimeout: AudioIdleTimeout
    var speakerIdleTimeout: AudioIdleTimeout
    var echoCancellation: Bool
}

/// This phone's name as the group sees it.
struct IOSLocalDeviceName: Equatable {
    /// Effective name (custom if set, otherwise the system default).
    var name: String
    /// The default from the system (model name, see SystemDeviceName).
    var systemName: String
    var custom: Bool
}

#if OINK_MOBILE_CORE_FFI
extension AudioIdleTimeout {
    init(_ value: MobileAudioIdleTimeout) {
        switch value {
        case .oneMinute: self = .one
        case .fiveMinutes: self = .five
        case .tenMinutes: self = .ten
        case .twentyMinutes: self = .twenty
        case .thirtyMinutes: self = .thirty
        case .sixtyMinutes: self = .sixty
        case .unlimited: self = .unlimited
        }
    }

    var core: MobileAudioIdleTimeout {
        switch self {
        case .one: return .oneMinute
        case .five: return .fiveMinutes
        case .ten: return .tenMinutes
        case .twenty: return .twentyMinutes
        case .thirty: return .thirtyMinutes
        case .sixty: return .sixtyMinutes
        case .unlimited: return .unlimited
        }
    }
}

@MainActor
extension CoreSession {
    func loadCoreSettings(_ owner: MobileSession) {
        let media = owner.media()
        settings = IOSCoreSettings(
            microphoneIdleTimeout: AudioIdleTimeout(media.idleTimeout(kind: .microphone)),
            speakerIdleTimeout: AudioIdleTimeout(media.idleTimeout(kind: .speaker)),
            echoCancellation: media.echoCancellation())
    }

    /// Microphone / speaker auto-close (decision 8), counted from switching
    /// on while no audio flows.
    func setIdleTimeout(_ kind: MobileMediaKind, _ value: AudioIdleTimeout) throws {
        let owner = try requireLiveSession()
        _ = try owner.media().setIdleTimeout(kind: kind, timeout: value.core)
        loadCoreSettings(owner)
    }

    func setEchoCancellation(_ enabled: Bool) throws {
        let owner = try requireLiveSession()
        try owner.media().setEchoCancellation(enabled: enabled)
        loadCoreSettings(owner)
    }

    func applyLocalDeviceName(_ value: LocalDeviceName) {
        let projected = IOSLocalDeviceName(name: value.name, systemName: value.systemName, custom: value.custom)
        if localDeviceName != projected { localDeviceName = projected }
    }

    /// Settings → device name. `nil` (or blank) returns to the system name.
    func setLocalDeviceName(_ name: String?) throws {
        let owner = try requireLiveSession()
        let trimmed = name?.trimmingCharacters(in: .whitespacesAndNewlines)
        do {
            applyLocalDeviceName(try owner.setLocalDeviceName(name: trimmed?.isEmpty == false ? trimmed : nil))
        } catch MobileError.Command(.invalidValue, _) {
            throw OinkFailure(message: "设备名称长度需为 1–256 字节。")
        }
    }
}
#endif
