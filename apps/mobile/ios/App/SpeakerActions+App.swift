import Combine
import UIKit

extension SpeakerActions {
    static let shared: SpeakerActions = {
        let core = CoreSession.shared
        #if DEBUG
        if DesignScene.current != nil, ProcessInfo.processInfo.arguments.contains("-oink-forward-stub") {
            return ForwardUIStub.makeActions(core: core)
        }
        #endif
        return SpeakerActions(active: core.forwarding.state != .stopped,
            changes: core.forwarding.$state.map { $0 != .stopped }.eraseToAnyPublisher(),
            start: {
                guard let presenter = QuickLookPresenter.topController() else { return false }
                return core.forwarding.start(from: presenter)
            }, stop: { core.forwarding.stop() },
            setExit: {
                try await core.requireAudioSession()
                try await core.setLocalSpeaker($0)
            },
            feedback: { if $0 == .success { Haptics.success() } else { Haptics.error() } })
    }()
}

/// A foreground-continuing Control Center intent waits for the app's scene to
/// be active and attached before asking ReplayKit to present its system UI.
@MainActor
final class ForwardLaunchRequest: ObservableObject {
    static let shared = ForwardLaunchRequest()
    @Published var pending = false
    func presentIfReady() {
        guard pending, UIApplication.shared.applicationState == .active,
              QuickLookPresenter.topController() != nil else { return }
        pending = false
        guard CoreSession.shared.forwarding.state == .stopped else { return }
        SpeakerActions.shared.requestForward(longPress: false)
    }
}

#if DEBUG
/// UI tests exercise the normal gesture/state paths without invoking ReplayKit.
@MainActor
private enum ForwardUIStub {
    static let active = CurrentValueSubject<Bool, Never>(false)
    static func makeActions(core: CoreSession) -> SpeakerActions {
        let initiallyActive = ProcessInfo.processInfo.arguments.contains("-oink-forward-active")
        active.send(initiallyActive)
        return SpeakerActions(active: initiallyActive, changes: active.eraseToAnyPublisher(),
            start: {
                guard let presenter = QuickLookPresenter.topController() else { return false }
                let sheet = UIAlertController(title: "广播控制器测试替身", message: nil, preferredStyle: .alert)
                sheet.addAction(UIAlertAction(title: "取消", style: .cancel))
                sheet.addAction(UIAlertAction(title: "开始转发", style: .default) { _ in active.send(true) })
                presenter.present(sheet, animated: false)
                return true
            }, stop: { active.send(false) },
            setExit: { core.localAudio.speakerOn = $0 }, feedback: { _ in })
    }
}
#endif
