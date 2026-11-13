import Combine
import Foundation

/// Speaker gestures share one state machine across the toolbar and virtual keyboard.
/// Requesting the system picker never changes a role. Only its actual state does.
@MainActor
final class SpeakerActions: ObservableObject {
    enum Mode { case off, exit, forward }
    enum Action: Equatable { case off, exit, forward }
    enum Feedback: Equatable { case success, error }
    nonisolated static let preferenceKey = "speakerTapAction"
    static func action(mode: Mode, preferForward: Bool, longPress: Bool) -> Action {
        switch mode {
        case .off: return preferForward != longPress ? .forward : .exit
        case .exit: return longPress ? .forward : .off
        case .forward: return longPress ? .exit : .off
        }
    }

    @Published private(set) var forwarding: Bool
    @Published private(set) var started = 0
    @Published private(set) var error: String?
    @Published private(set) var errorRevision = 0
    private var stopping = false
    private var applyingExit = false
    private var exitAfterStop = false
    private var longPressPending = false
    private let start: () -> Bool
    private let stop: () -> Void
    private let setExit: (Bool) async throws -> Void
    private let feedback: (Feedback) -> Void
    private var observation: AnyCancellable?

    init(active: Bool, changes: AnyPublisher<Bool, Never>, start: @escaping () -> Bool,
         stop: @escaping () -> Void, setExit: @escaping (Bool) async throws -> Void,
         feedback: @escaping (Feedback) -> Void) {
        forwarding = active
        self.start = start
        self.stop = stop
        self.setExit = setExit
        self.feedback = feedback
        observation = changes.removeDuplicates().sink { [weak self] in self?.changed($0) }
    }

    func press(exitOn: Bool, longPress: Bool, preferForward: Bool) {
        guard !stopping && !applyingExit else { return }
        let mode: Mode = forwarding ? .forward : exitOn ? .exit : .off
        let action = Self.action(mode: mode, preferForward: preferForward, longPress: longPress)
        if action == .forward {
            requestForward(longPress: longPress)
        } else if forwarding {
            stopping = true
            exitAfterStop = action == .exit
            longPressPending = longPress
            stop()
        } else {
            applyExit(action == .exit, longPress: longPress)
        }
    }

    func requestForward(longPress: Bool) {
        guard !forwarding else { return }
        longPressPending = longPress
        if !start() { longPressPending = false; reject("无法打开音频转发，请重试。") }
    }

    private func changed(_ active: Bool) {
        guard forwarding != active else { return }
        forwarding = active
        if active {
            started += 1
            if longPressPending { feedback(.success) }
            longPressPending = false
        } else {
            let makeExit = exitAfterStop
            let longPress = longPressPending
            stopping = false
            exitAfterStop = false
            longPressPending = false
            if makeExit { applyExit(true, longPress: longPress) }
        }
    }

    private func applyExit(_ on: Bool, longPress: Bool) {
        applyingExit = true
        Task { @MainActor in
            defer { applyingExit = false }
            do {
                try await setExit(on)
                if longPress { feedback(.success) }
            } catch { reject(error.localizedDescription) }
        }
    }

    private func reject(_ message: String) {
        error = message
        errorRevision += 1
        feedback(.error)
    }
}
