import Combine
import SwiftUI
import UIKit

/// The finite grace task covers audio transitions and awaited shutdown.
/// Only running audio supplies background execution and keeps the session.
/// An OS interruption releases it; the interruption's end (shouldResume)
/// starts a new one for the accepted audio intent (review H12).
@MainActor
final class AppLifecycle: ObservableObject {
    private var transition: Task<Void, Never>?
    private var phase: ScenePhase?
    private var screenAwake: AnyCancellable?

    func update(_ phase: ScenePhase, core: CoreSession) {
        guard self.phase != phase else { return }
        self.phase = phase
        #if DEBUG
        // Design-state screenshots run on fixture data only; no network session.
        if DesignScene.current != nil { return }
        #endif
        // `.inactive` (Siri, Control Center, a system sheet) leaves the
        // session as it is (review M29).
        core.setForeground(phase == .active)
        core.setSceneVisible(phase != .background)
        core.audioEligibilityChanged = { [weak self, weak core] in
            guard let self, let core, self.phase == .background else { return }
            self.reconcile(core: core)
        }
        if screenAwake == nil {
            // Auto-lock would background the app and end the session while a
            // transfer or pairing still runs (review M28).
            screenAwake = core.$keepsScreenAwake.removeDuplicates().sink { value in
                UIApplication.shared.isIdleTimerDisabled = value
            }
        }
        guard phase != .inactive else { return }
        reconcile(core: core)
    }
    private func reconcile(core: CoreSession) {
        let previous = transition
        let grace = phase == .background ? ShutdownGrace(core: core) : nil
        transition = Task { @MainActor in
            await previous?.value
            if self.phase == .active {
                core.setForeground(true)
                await core.resume()
            } else if self.phase == .background {
                // An interruption ended while the session was released: start
                // one so the accepted audio intent plays again (review H12).
                if core.audioNeedsSession { await core.resume() }
                // Startup/rebuild is bounded by the OS grace task. A transient
                // Preparing state must not tear down the network underneath it.
                await core.audioOwner?.audio.waitForTransition()
                // Kept audio and a running background App Intent hold the
                // session; that is decided when the release runs (review M29).
                if self.phase == .background { await core.releaseIfUnheld() }
            }
            // A role that stopped while the session stays held (another
            // device took the exit, the idle auto-off) ends its Live Activity
            // on this same background task: iOS may suspend the app as soon
            // as it ends, and an unfinished end left the card on the phone.
            await core.audioControls?.settle()
            grace?.end()
        }
    }
}

@MainActor
private final class ShutdownGrace {
    private var identifier: UIBackgroundTaskIdentifier = .invalid
    init(core: CoreSession) {
        identifier = UIApplication.shared.beginBackgroundTask(withName: "Oink shutdown") { [weak self] in
            Task { @MainActor in
                core.expireBackgroundAudioTransition()
                core.cancelPendingOperations()
                self?.end()
            }
        }
    }
    func end() {
        guard identifier != .invalid else { return }
        UIApplication.shared.endBackgroundTask(identifier)
        identifier = .invalid
    }
}
