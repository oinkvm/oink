import Combine
import ReplayKit
import UIKit

/// UI seam: `start(from:)` opens the system picker, `stop()` requests ReplayKit
/// termination. `state` changes only after the extension actually starts.
/// A Control Center start intent opens the app, then calls `start(from:)` there.
@MainActor
final class ForwardController: ObservableObject {
    enum State: Equatable { case stopped, forwarding, paused }
    @Published private(set) var state: State = .stopped
    private weak var core: CoreSession?
    private var pipe: OinkBroadcastPipe?
    private var driver: ForwardDriver?
    private var signal: BroadcastSignal?
    private var timer: Timer?
    private var bound: MobileMediaSession?
    private var picker: RPSystemBroadcastPickerView?

    init(core: CoreSession) {
        self.core = core
        if let root = try? SharedStore.root(), let pipe = OinkBroadcastPipe(root: root.path) {
            self.pipe = pipe
            driver = ForwardDriver(pipe: pipe)
            driver?.didDisable = { [weak self] in Task { @MainActor in self?.stop() } }
            // A new main process starts all phone roles locally. An orphaned
            // live broadcast from the previous process must end, not resume.
            if pipe.broadcasting { pipe.stopRequested = true; BroadcastSignal.post(.changed) }
        }
        signal = BroadcastSignal(.changed) { [weak self] in self?.refresh() }
        timer = Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.refresh() }
        }
    }

    /// Also usable directly in a UIViewRepresentable: the system owns the
    /// visible broadcast button and microphone is never requested by Oink.
    static func makePicker() -> RPSystemBroadcastPickerView {
        let picker = RPSystemBroadcastPickerView(frame: CGRect(x: 0, y: 0, width: 44, height: 44))
        picker.preferredExtension = (Bundle.main.bundleIdentifier ?? "com.hachiwii.oink") + ".broadcast"
        picker.showsMicrophoneButton = false
        return picker
    }

    @discardableResult
    func start(from presenter: UIViewController) -> Bool {
        guard pipe != nil, state == .stopped, UIApplication.shared.applicationState == .active else { return false }
        let picker = Self.makePicker()
        picker.isHidden = true
        presenter.view.addSubview(picker)
        picker.center = CGPoint(x: presenter.view.bounds.midX, y: presenter.view.bounds.midY)
        self.picker?.removeFromSuperview()
        self.picker = picker
        // ReplayKit has no public "present picker" method. Sending its real
        // embedded UIButton action opens the system sheet without changing
        // endpoint intent. This relies on UIKit's private view arrangement;
        // return failure if the button is absent. No OS version has yet been
        // device-validated for this adapter; see the broadcast test report.
        // Keep the hidden picker attached for the system's asynchronous
        // presentation. Cancelling leaves the visible UI and audio unchanged.
        guard let button = picker.subviews.compactMap({ $0 as? UIButton }).first else {
            picker.removeFromSuperview(); self.picker = nil
            return false
        }
        button.sendActions(for: .touchUpInside)
        return true
    }

    func stop() {
        guard let pipe, pipe.broadcasting else { return }
        pipe.stopRequested = true
        BroadcastSignal.post(.changed)
    }

    /// Reserve before attempting mobile-session.lock: the extension yields and
    /// cannot steal it back while the main app awaits its shutdown notification.
    func requestOwner() {
        guard pipe?.claimApp() == true else { return }
        BroadcastSignal.post(.appRequested)
    }

    func bind(_ media: MobileMediaSession) {
        guard bound !== media else { return }
        bound = media
        refresh()
    }
    func unbind() {
        driver?.bind(nil)
        bound = nil
    }
    func releaseOwner() { pipe?.releaseApp(); BroadcastSignal.post(.appRequested) }

    private func refresh() {
        guard let pipe else { return }
        let next: State = !pipe.broadcasting ? .stopped : (pipe.paused ? .paused : .forwarding)
        let wasActive = state != .stopped
        if next != state {
            state = next
            core?.audioOwner?.audio.setForwardingCaptureActive(next != .stopped)
            core?.refreshForwardCapabilities()
            if next != .stopped { picker?.removeFromSuperview(); picker = nil }
        }
        if next == .stopped {
            if wasActive {
                driver?.bind(nil)
                if let bound { _ = bound.setEnabled(kind: .forward, enabled: false) }
                core?.broadcastEnded()
            }
            return
        }
        if !wasActive, let bound { driver?.bind(bound, readRing: true) }
        driver?.setPaused(next == .paused)
    }

    /// Called after each new main-app session, even if the broadcast did not
    /// change state during the owner handover.
    func sessionReady() {
        guard state != .stopped, let bound else { return }
        driver?.bind(bound, readRing: true)
        driver?.setPaused(state == .paused)
    }
}
