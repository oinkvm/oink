import ActivityKit
import AppIntents
import AVFAudio
import UIKit

/// What the Control Center toggles 扬声器 / 麦克风 and the Live Activities'
/// stop buttons do in the app's process (Controls/AudioControlIntents.swift).
/// They change the same switches as the top bar, from the background too,
/// and work without a group connection (user 2026-11-12).
@MainActor
enum AudioControlActions {
    /// How long a toggle holds the session for the audio to start (see
    /// `MobileAudioSession.waitUntilSettled`).
    private static let settleLimit: Duration = .seconds(10)

    static func endForwardActivity() async {
        for activity in Activity<OinkAudioActivityAttributes>.activities
            where activity.attributes.role == .speaker && activity.content.state.forwarding {
            await activity.end(nil, dismissalPolicy: .immediate)
        }
    }

    static func setSpeaker(_ on: Bool) async throws {
        if let directory = ControlsStore.directory(), ForwardControlStore.active(in: directory) {
            ForwardControlStore.reload()
            return
        }
        try await run { owner in
            await owner.setControlIntent(playback: on, microphone: owner.audio.microphoneEnabled)
        }
    }

    static func setMicrophone(_ on: Bool, intent: SetMicrophoneIntent) async throws {
        if on && AVAudioApplication.shared.recordPermission == .undetermined {
            // The permission sheet only shows in front: continue in the app,
            // ask there, then turn the microphone on (the sheet's answer is
            // the user's choice for this toggle).
            if UIApplication.shared.applicationState != .active {
                if #available(iOS 26.0, *) {
                    try await intent.continueInForeground(alwaysConfirm: false)
                } else {
                    try await intent.requestToContinueInForeground()
                }
            }
            guard await AVAudioApplication.requestRecordPermission() else { throw denied }
        }
        try await run { owner in
            await owner.setControlIntent(playback: owner.audio.playbackEnabled, microphone: on)
        }
    }

    private static var denied: ControlIntentError {
        ControlIntentError(message: IOSLocalAudio.issue(.permissionDenied) ?? "")
    }

    /// Holds the session (started for the toggle if the app was not running)
    /// until the switch runs or definitely cannot, then publishes the toggle
    /// state and the Live Activity from inside the intent's perform.
    private static func run(_ change: (MobileAudioOwner) async -> MobileAudioSession.ControlIntentOutcome) async throws {
        let core = CoreSession.shared
        installSceneLessRelease(core)
        core.beginBackgroundOperation()
        do {
            try await core.requireAudioSession()
            guard let owner = core.audioOwner else { throw ControlIntentError.notInApp }
            switch await change(owner) {
            case .applied: break
            case .microphoneDenied: throw denied
            case .microphonePermissionNeeded:
                // Only reachable if the permission changed after the check above.
                throw ControlIntentError(message: AppText.pick("请打开 Oink 允许使用麦克风。", "Open Oink to allow the microphone."))
            }
            await owner.audio.waitUntilSettled(limit: settleLimit)
            core.audioControls?.refresh()
            await core.audioControls?.settle()
        } catch {
            await core.finishBackgroundOperation()
            throw error
        }
        await core.finishBackgroundOperation()
    }

    /// A toggle can launch the app in the background without a scene. Then
    /// AppLifecycle, driven by the scene phase, never runs, and nothing would
    /// release the session when the audio stops by itself later (idle
    /// auto-off, another device taking the role): the app would be suspended
    /// holding the App Group owner lock, which iOS punishes by killing it
    /// (0xdead10cc). Until a scene installs AppLifecycle's own handler, this
    /// one does the background part of `AppLifecycle.reconcile`.
    private static func installSceneLessRelease(_ core: CoreSession) {
        guard core.audioEligibilityChanged == nil else { return }
        core.audioEligibilityChanged = { [weak core] in
            guard let core else { return }
            let task = UIApplication.shared.beginBackgroundTask(withName: "Oink audio release")
            Task { @MainActor in
                if core.audioNeedsSession { await core.resume() }
                await core.audioOwner?.audio.waitForTransition()
                await core.releaseIfUnheld()
                await core.audioControls?.settle()
                if task != .invalid { UIApplication.shared.endBackgroundTask(task) }
            }
        }
    }
}
