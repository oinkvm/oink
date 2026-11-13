import Foundation

// Session lifecycle decisions of the main app (review H12, M27–M29, M31),
// kept free of UIKit and of the CoreSession singleton so the host unit tests
// run them directly.

#if OINK_MOBILE_CORE_FFI
/// Why the network session is not running (review M27). Published by
/// CoreSession; the device and transfer pages show it with a retry button
/// instead of an empty group or a list of offline devices.
enum IOSSessionProblem: Equatable {
    /// Another Oink process (the share extension sending something) owns the
    /// session. The app starts its own as soon as that process releases it.
    case busy
    /// This phone's identity could not be read from the Keychain.
    case identity(String)
    /// The core refused to start (saved group state, storage). `detail` is
    /// the core's diagnostic text.
    case startup(String)

    init(_ error: Error) {
        switch error {
        case MobileError.OwnerBusy: self = .busy
        case let failure as IdentityUnavailable: self = .identity(failure.message)
        case MobileError.Startup(let detail): self = .startup(detail)
        default: self = .startup(error.localizedDescription)
        }
    }

    var title: String {
        switch self {
        case .busy: AppText.pick("Oink 分享正在使用网络", "An Oink share is using the network")
        case .identity: AppText.pick("无法读取本机身份", "Cannot read this phone's identity")
        case .startup: AppText.pick("网络会话无法启动", "The network session cannot start")
        }
    }

    var message: String {
        switch self {
        case .busy:
            AppText.pick("分享结束后会自动重新连接。", "Oink reconnects when the share has finished.")
        case .identity(let reason):
            reason
        case .startup(let detail):
            detail
        }
    }
}

/// The identity seed could not be read; carried to `IOSSessionProblem`.
struct IdentityUnavailable: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}

/// Whether the screen must not lock by itself (review M28). Auto-lock sends
/// the app to the background, which ends the session and with it every
/// operation still running, so the screen stays on exactly while the core
/// has one: an own share between its start and its single DeliveryEvent, a
/// download between its first progress and the record leaving
/// `downloading`, or a pairing session that has not finished.
enum ScreenAwake {
    static func required(shares: Set<String>, downloads: Set<Int64>, pairing: PairingSnapshot?) -> Bool {
        !shares.isEmpty || !downloads.isEmpty || pairingInProgress(pairing)
    }

    /// A pairing session before Completed (7) / Ended (8) and without a
    /// terminal problem.
    static func pairingInProgress(_ snapshot: PairingSnapshot?) -> Bool {
        snapshot?.sessions.contains { $0.terminal == nil && $0.phase < 7 } ?? false
    }
}
#endif

/// The main app's session transitions, run one after another, and what
/// holds the session (review M29): a visible scene (`.inactive` included),
/// kept audio, and running background App Intents. Whether a release goes
/// ahead is decided when it runs, after the transitions queued before it: a
/// release the lifecycle queued while an intent was starting the session
/// (the audio owner reports its eligibility from inside every start) is
/// dropped instead of ending the session under the intent.
@MainActor
final class SessionTransitions {
    private var last: Task<Void, Never>?
    private(set) var operations = 0
    var sceneVisible = false

    /// Runs `body` after every transition queued before it.
    func run(_ body: @escaping @MainActor () async -> Void) async {
        let previous = last
        let next = Task { @MainActor in
            await previous?.value
            await body()
        }
        last = next
        await next.value
    }

    /// Waits until the transitions queued so far have finished.
    func settle() async {
        await last?.value
    }

    func beginOperation() { operations += 1 }

    /// Ends a `beginOperation`; true when it was the last running one.
    func endOperation() -> Bool {
        guard operations > 0 else { return false }
        operations -= 1
        return operations == 0
    }

    /// Runs `release` unless the session is held once the transitions before
    /// it have finished.
    func releaseIfUnheld(keepsAudio: @escaping @MainActor () -> Bool,
                         release: @escaping @MainActor () async -> Void) async {
        await run { [self] in
            guard operations == 0, !sceneVisible, !keepsAudio() else { return }
            await release()
        }
    }
}
