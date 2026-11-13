import Foundation
import AVFAudio
import Combine

/// Main app owner for the audio-v2 native seam.
/// AVAudioEngine supplies timestamped PCM through OinkAudioIO. Rust owns the
/// endpoint processing; this adapter owns local intent and generation fencing.
@MainActor
final class MobileAudioOwner: ObservableObject {
    private let backend = OinkRustAudioBackend()
    private var generation: UInt64 = 0
    private var shuttingDown = false
    private var media: MobileMediaSession?
    private var subscription: MobileMediaSubscription?
    private var attachedGeneration: UInt64 = 0
    private var attachedMask: UInt8 = 0
    private var diagnosticRunning = false

    var eligibilityChanged: (() -> Void)?
    var capabilitiesChanged: (() -> Void)?
    var diagnosticEvent: ((String) -> Void)?
    /// An interruption ended with shouldResume after the network owner was
    /// released for it (review H12): the lifecycle starts a session and
    /// `bind` + `restoreAcceptedIntent` run the accepted intent again.
    private(set) var needsNetworkOwner = false

    lazy var audio: MobileAudioSession = {
        let value = MobileAudioSession(backend: backend.callbacks,
            prepareGeneration: { [weak self] playback, microphone in
                guard let self, !self.shuttingDown else { throw CancellationError() }
                return try self.prepare(playback: playback, microphone: microphone)
            }, stoppedGeneration: { [weak self] generation in self?.acknowledgeStopped(generation) })
        value.intentChanged = { [weak self] playback, microphone in
            self?.publishIntent(playback: playback, microphone: microphone)
            self?.eligibilityChanged?()
        }
        value.permissionChanged = { [weak self] in self?.reportMicrophonePermission() }
        value.routeChanged = { [weak self] in self?.capabilitiesChanged?() }
        value.recoveryRequested = { [weak self] in self?.clearAudioFailures() }
        value.failureObserved = { [weak self] state, generation in
            self?.reportFailure(state, generation: generation)
        }
        value.stateChanged = { [weak self] state, generation in
            self?.audioStateChanged(state, generation: generation)
        }
        value.backgroundEligibilityChanged = { [weak self] _ in self?.eligibilityChanged?() }
        value.resumeRequested = { [weak self] in
            guard let self, self.media == nil else { return }
            self.needsNetworkOwner = true
            self.eligibilityChanged?()
        }
        return value
    }()

    init(media: MobileMediaSession? = nil) {
        self.media = media
        backend.attachHandles = { [weak self] generation, output, input in
            guard let self, !self.shuttingDown else { return false }
            guard let media = self.media else { return false }
            let outputAttached = output == 0 || media.attachAudio(kind: .speaker, generation: generation, handle: output)
            let inputAttached = input == 0 || media.attachAudio(kind: .microphone, generation: generation, handle: input)
            guard outputAttached && inputAttached else { return false }
            self.attachedGeneration = generation
            self.attachedMask = (output == 0 ? 0 : 1) | (input == 0 ? 0 : 2)
            self.generation = generation
            return true
        }
        backend.detachHandles = { [weak self] _, _ in
            guard let self else { return }
            self.acknowledgeStopped(self.attachedGeneration)
            self.attachedGeneration = 0
            self.attachedMask = 0
            self.generation = 0
        }
        // Install lifecycle observers before the first native start. The core
        // permission projection must also follow an authorization change while
        // audio is idle, so a lazy session cannot defer observer registration.
        _ = audio
        reportMicrophonePermission()
    }

    /// Binds the already-created Rust media owner. Audio cannot start before
    /// this binding: the native handles must be admitted by MobileMediaSession.
    func bind(media: MobileMediaSession) {
        shuttingDown = false
        needsNetworkOwner = false
        self.media = media
        reportMicrophonePermission()
        publishIntent(playback: audio.playbackEnabled, microphone: audio.microphoneEnabled)
    }

    func subscribe() async {
        guard let media, subscription == nil else { return }
        subscription = media.subscribe(listener: NativeMediaEvents { [weak self] in self?.consumeMediaState() })
        consumeMediaState()
    }

    @discardableResult
    func setLocalIntent(playback: Bool, microphone: Bool) async -> MobileAudioSession.LocalIntentOutcome {
        await audio.setLocalIntent(playback: playback, microphone: microphone)
    }

    /// Control Center toggles (see `MobileAudioSession.setControlIntent`).
    @discardableResult
    func setControlIntent(playback: Bool, microphone: Bool) async -> MobileAudioSession.ControlIntentOutcome {
        await audio.setControlIntent(playback: playback, microphone: microphone)
    }

    /// After `bind` + `subscribe` of a new session: runs the switches the
    /// phone had accepted before the previous owner was released (review M26).
    func restoreAcceptedIntent() {
        audio.restoreAcceptedIntent()
    }

    func resumeFromPhone() async {
        await setLocalIntent(playback: audio.playbackEnabled, microphone: audio.microphoneEnabled)
    }

    func stopFromPhone() { audio.stopFromPhone() }

    /// Releases the session's media owner. The phone's switches stay as
    /// accepted (review M26); only native I/O stops.
    func shutdown() async {
        shuttingDown = true
        audio.releaseNetworkOwner()
        await audio.waitForTransition()
        acknowledgeStopped(generation)
        subscription?.cancel()
        subscription = nil
        media = nil
        generation = 0
    }

    private func prepare(playback: Bool, microphone: Bool) throws -> UInt64 {
        if let media {
            let mask: UInt8 = (playback ? 1 : 0) | (microphone ? 2 : 0)
            let state = media.requestRebuildResources(resources: mask)
            let speakerClaimed = !playback || media.claimStart(kind: .speaker, generation: state.generation)
            guard speakerClaimed else { throw CancellationError() }
            if microphone && !media.claimStart(kind: .microphone, generation: state.generation) {
                if playback { _ = media.acknowledgeStopped(kind: .speaker, generation: state.generation) }
                throw CancellationError()
            }
            generation = state.generation
            attachedMask = mask
            attachedGeneration = state.generation
            return state.generation
        }
        generation &+= 1
        return generation
    }

    private func publishIntent(playback: Bool, microphone: Bool) {
        guard let media else { return }
        _ = media.setEnabled(kind: .speaker, enabled: playback)
        _ = media.setEnabled(kind: .microphone, enabled: microphone)
    }

    private func acknowledgeStopped(_ generation: UInt64) {
        guard let media, generation != 0 else { return }
        if attachedMask & 1 != 0 {
            _ = media.acknowledgeStopped(kind: .speaker, generation: generation)
        }
        if attachedMask & 2 != 0 {
            _ = media.acknowledgeStopped(kind: .microphone, generation: generation)
        }
    }

    private func audioStateChanged(_ state: MobileAudioSession.State, generation: UInt64) {
        capabilitiesChanged?()
        let running = state == .running
        if running != diagnosticRunning {
            diagnosticRunning = running
            diagnosticEvent?(running ? "endpoint_started" : "endpoint_stopped")
        }
        guard state == .running, let media else { return }
        var admitted = true
        if attachedMask & 1 != 0 {
            admitted = media.acknowledgeStarted(kind: .speaker, generation: generation) && admitted
        }
        if attachedMask & 2 != 0 {
            admitted = media.acknowledgeStarted(kind: .microphone, generation: generation) && admitted
        }
        if !admitted { audio.setSharingPaused(true) }
    }

    private func reportMicrophonePermission() {
        let permission: MobileMicrophonePermission
        switch AVAudioApplication.shared.recordPermission {
        case .undetermined: permission = .notDetermined
        case .granted: permission = .granted
        case .denied: permission = .denied
        @unknown default: permission = .notDetermined
        }
        _ = media?.setMicrophonePermission(permission: permission)
        capabilitiesChanged?()
    }

    /// Reports native endpoint facts (`MobilePlatformFacts`): the available
    /// AVAudio routes, never intent. Microphone permission is not part of it:
    /// the core reports a missing permission as the endpoint direction's
    /// `PermissionRequired` (protocol-v2 1.1). iOS has no input-control helper.
    func capabilityFacts() -> (microphoneSend: Bool, speakerReceive: Bool) {
        let session = AVAudioSession.sharedInstance()
        let speakerReceive = !session.currentRoute.outputs.isEmpty
        let microphoneSend = session.isInputAvailable
        return (microphoneSend, speakerReceive)
    }

    private func clearAudioFailures() {
        guard let media else { return }
        let state = media.snapshot()
        if state.failures.indices.contains(0), state.failures[0] != .none {
            _ = media.clearFailure(kind: .speaker)
        }
        if state.failures.indices.contains(1), state.failures[1] != .none {
            _ = media.clearFailure(kind: .microphone)
        }
    }

    private func reportFailure(_ native: MobileAudioSession.State, generation: UInt64?) {
        guard let media, !shuttingDown else { return }
        let failure: MobileMediaFailure
        switch native {
        case .permissionDenied: failure = .permissionRequired
        case .interrupted: failure = .interrupted
        case .needsLocalResume: failure = .systemBlocked
        default: failure = .unavailable
        }
        let coreGeneration = generation ?? media.snapshot().generation
        if native != .permissionDenied && audio.playbackEnabled {
            _ = media.reportFailure(kind: .speaker, generation: coreGeneration, failure: failure)
        }
        if audio.microphoneEnabled || native == .permissionDenied {
            _ = media.reportFailure(kind: .microphone, generation: coreGeneration, failure: failure)
        }
    }

    private func consumeMediaState() {
        guard !shuttingDown, let media else { return }
        let state = media.snapshot()
        // A reserved direction remains eligible until Rust requests its
        // stop. On iOS an enabled microphone is eligible without a peer's
        // demand (it captures and sends nothing, user 2026-11-12); the core
        // decides that in its start mask (media_session.rs).
        var resources = state.startMask
        for (index, generation) in state.ownerGenerations.prefix(2).enumerated() where generation != 0 {
            resources |= UInt8(1) << index
        }
        audio.applyCoreIntent(playback: state.localEnabledMask & 1 != 0,
            microphone: state.localEnabledMask & 2 != 0, paused: state.pauseMask != 0,
            resources: resources & ~state.stopMask)
    }
}

private final class NativeMediaEvents: MobileMediaListener, @unchecked Sendable {
    private let changed: @MainActor () -> Void

    init(_ changed: @escaping @MainActor () -> Void) { self.changed = changed }

    nonisolated func onMediaState(state: MobileMediaState) {
        DispatchQueue.main.async { [changed] in changed() }
    }
}
