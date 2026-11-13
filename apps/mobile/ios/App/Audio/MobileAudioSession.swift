import AVFAudio
import Combine
import OSLog
import UIKit

/// Main-app-only audio owner. UI and the Control Center toggles supply local
/// actions; remote messages may change sharing pause/demand, never call
/// setLocalIntent/setControlIntent or request permission.
@MainActor
final class MobileAudioSession: ObservableObject {
    private static let log = Logger(subsystem: Bundle.main.bundleIdentifier ?? "com.hachiwii.oink", category: "audio")
    enum State: Equatable {
        case stopped, preparing, running, paused, interrupted, needsLocalResume, permissionDenied
        case failed(String)
    }
    /// Result of a local switch change. `microphoneDenied`: the microphone
    /// stays off because iOS denies the permission; the UI says so (L16).
    enum LocalIntentOutcome: Equatable { case applied, microphoneDenied }
    /// Result of a Control Center toggle. `microphonePermissionNeeded`: the
    /// permission is undecided and its sheet needs the app in front; nothing
    /// changed.
    enum ControlIntentOutcome: Equatable { case applied, microphoneDenied, microphonePermissionNeeded }
    @Published private(set) var state: State = .stopped
    @Published private(set) var playbackEnabled = false
    @Published private(set) var microphoneEnabled = false
    @Published private(set) var route = ""
    private(set) var epoch: UInt64 = 0
    private(set) var actualSampleRate: Double = 0
    private(set) var actualBufferDuration: TimeInterval = 0
    private(set) var actualInputRate: Double = 0
    private(set) var actualInputLatency: TimeInterval = 0
    private(set) var actualOutputLatency: TimeInterval = 0
    private(set) var inputRoute = ""
    private var forwardingCaptureActive = false
    func setForwardingCaptureActive(_ active: Bool) {
        guard forwardingCaptureActive != active else { return }
        forwardingCaptureActive = active
        if microphoneEnabled { rebuildIfRunning() }
    }
    private var paused = false
    private var coreResources: UInt8 = 3
    private var interrupted = false
    private var needsLocalResume = false
    private var permissionRequest: UInt64 = 0
    private var requestingPermission = false
    private var observers: [NSObjectProtocol] = []
    private let io: OinkAudioIO
    private let prepareGeneration: (Bool, Bool) async throws -> UInt64
    private let stoppedGeneration: (UInt64) -> Void
    private var transitionRevision: UInt64 = 0
    private var activeGeneration: UInt64?
    private var sessionActivated = false
    private var settledWaiters: [CheckedContinuation<Void, Never>] = []
    // Called only on state transitions, on main; lifecycle may retain the network
    // owner while true. This does not claim an OS wakeup or extension entitlement.
    var backgroundEligibilityChanged: ((Bool) -> Void)?
    var stateChanged: ((State, UInt64) -> Void)?
    var failureObserved: ((State, UInt64?) -> Void)?
    var recoveryRequested: (() -> Void)?
    var intentChanged: ((Bool, Bool) -> Void)?
    var permissionChanged: (() -> Void)?
    var routeChanged: (() -> Void)?
    /// An interruption ended with shouldResume: the accepted intent wants to
    /// run again. The lifecycle starts a network owner for it if the one
    /// held before the interruption was released (review H12).
    var resumeRequested: (() -> Void)?
    // Only rendering/capturing audio keeps the app executing in background.
    // During an OS interruption nothing runs and iOS suspends the process, so
    // the network owner and its App Group file lock are released then; the
    // accepted intent stays here and the interruption's end starts a new owner
    // (review H12). The switches stay on through an interruption and the audio
    // resumes by itself when it ends (user 2026-11-12); nothing shows it.
    var keepsBackgroundSession: Bool { state == .running }
    var requiresExplicitLocalResume: Bool { needsLocalResume || interrupted }
    var renderedFrames: UInt64 { io.renderedFrames }
    var capturedFrames: UInt64 { io.capturedFrames }
    var maximumInputBatchFrames: UInt32 { io.maximumInputBatchFrames }
    var maximumOutputBatchFrames: UInt32 { io.maximumOutputBatchFrames }

    init(backend: OinkAudioBackend,
         prepareGeneration: @escaping (Bool, Bool) async throws -> UInt64,
         stoppedGeneration: @escaping (UInt64) -> Void) {
        self.prepareGeneration = prepareGeneration
        self.stoppedGeneration = stoppedGeneration
        io = OinkAudioIO(backend: backend)
        observe(UIApplication.didBecomeActiveNotification) { owner, _ in
            owner.permissionChanged?()
            // iOS does not promise an end for every interruption (AVAudioSession docs: "there
            // is no guarantee that a begin interruption will have an end interruption"); on
            // the iPhone, Continuity Camera began one and never ended it (user 2026-11-12,
            // iPhone log 10:34:40, interrupted by "Wombat"). Back in front, the app may take
            // its session again: the accepted switches run.
            if owner.interrupted { owner.resumeAfterInterruption() }
        }
        observe(AVAudioSession.interruptionNotification) { owner, note in owner.interruption(note) }
        observe(AVAudioSession.routeChangeNotification) { owner, note in
            owner.routeChanged?()
            let reason = (note.userInfo?[AVAudioSessionRouteChangeReasonKey] as? NSNumber)?.uintValue
            // Our own category change is already followed by a new graph. Rebuilding
            // for it creates a category/route notification feedback loop.
            if reason != AVAudioSession.RouteChangeReason.categoryChange.rawValue { owner.rebuildIfRunning() }
        }
        observe(.AVAudioEngineConfigurationChange) { owner, note in
            if let engine = note.object as? AVAudioEngine, owner.io.ownsEngine(engine) { owner.rebuildIfRunning() }
        }
        observe(AVAudioSession.mediaServicesWereLostNotification) { owner, _ in owner.mediaServicesUnavailable() }
        observe(AVAudioSession.mediaServicesWereResetNotification) { owner, _ in owner.mediaServicesUnavailable() }
    }
    private func observe(_ name: Notification.Name, _ body: @escaping (MobileAudioSession, Notification) -> Void) {
        observers.append(NotificationCenter.default.addObserver(forName: name, object: nil, queue: .main) { [weak self] note in
            MainActor.assumeIsolated { if let self { body(self, note) } }
        })
    }

    /// Call from the phone's visible UI only. Permission resolution cannot restore
    /// a switch cancelled while the system sheet was open, or bypass sharing pause.
    @discardableResult
    func setLocalIntent(playback: Bool, microphone: Bool) async -> LocalIntentOutcome {
        guard UIApplication.shared.applicationState == .active else { return .applied }
        // An already denied permission has no sheet to wait for: the
        // microphone stays off, the rest of the intent (a playing speaker)
        // is left untouched and the caller shows the reason (review L16).
        if microphone && !microphoneEnabled && AVAudioApplication.shared.recordPermission == .denied {
            permissionChanged?()
            failureObserved?(.permissionDenied, nil)
            if playback != playbackEnabled { await setLocalIntent(playback: playback, microphone: false) }
            return .microphoneDenied
        }
        permissionRequest &+= 1
        let request = permissionRequest
        requestingPermission = false
        interrupted = false
        needsLocalResume = false
        recoveryRequested?()

        // Resolve permission before publishing the final intent or starting a graph.
        // While the sheet is open, stop the old graph and invalidate pending starts.
        var microphoneGranted = microphone
        if microphone && AVAudioApplication.shared.recordPermission != .granted {
            requestingPermission = true
            stopIO()
            publish(paused ? .paused : .preparing)
            microphoneGranted = await AVAudioApplication.requestRecordPermission()
            guard request == permissionRequest else { return .applied }
            requestingPermission = false
        }
        permissionChanged?()
        apply(playback: playback, microphone: microphoneGranted)
        if microphone && !microphoneGranted {
            failureObserved?(.permissionDenied, nil)
        }
        reconcile()
        if microphone && !microphoneGranted && !playback { publish(.permissionDenied) }
        return microphone && !microphoneGranted ? .microphoneDenied : .applied
    }

    /// Control Center 扬声器 / 麦克风 and the Live Activities' stop buttons:
    /// the same local switch as the UI, also from the background (the
    /// intent's AudioPlaybackIntent / AudioRecordingIntent lets the session
    /// start there). It never asks for the microphone permission: an
    /// undecided one is answered in the app first
    /// (`AudioControlActions.setMicrophone`), a denied one leaves everything
    /// as it is.
    @discardableResult
    func setControlIntent(playback: Bool, microphone: Bool) async -> ControlIntentOutcome {
        if microphone && !microphoneEnabled {
            switch AVAudioApplication.shared.recordPermission {
            case .granted: break
            case .denied:
                permissionChanged?()
                return .microphoneDenied
            default:
                return .microphonePermissionNeeded
            }
        }
        permissionRequest &+= 1
        requestingPermission = false
        interrupted = false
        needsLocalResume = false
        recoveryRequested?()
        permissionChanged?()
        apply(playback: playback, microphone: microphone)
        reconcile()
        return .applied
    }

    /// Waits until the accepted switches run or definitely do not (a pause,
    /// an interruption, a failure, everything off), at most `limit`. A
    /// control intent holds the session until then: released while the
    /// audio is still starting, the background app would end it. The bound
    /// is for a core that never admits the start; the intent's own run time
    /// is limited by the system anyway.
    func waitUntilSettled(limit: Duration) async {
        guard !isSettled(state) else { return }
        // @Published emits the new value before the property changes: judge the emitted one.
        let states = $state.values
        await withTaskGroup(of: Void.self) { group in
            group.addTask { @MainActor [weak self] in
                for await value in states where self?.isSettled(value) ?? true { return }
            }
            group.addTask { try? await Task.sleep(for: limit) }
            await group.next()
            group.cancelAll()
        }
    }

    private func isSettled(_ state: State) -> Bool {
        guard playbackEnabled || microphoneEnabled else { return true }
        switch state {
        // `.stopped` with a switch on waits for the core to admit the start.
        case .stopped, .preparing: return false
        case .running, .paused, .interrupted, .needsLocalResume, .permissionDenied, .failed: return true
        }
    }

    private func apply(playback: Bool, microphone: Bool) {
        playbackEnabled = playback
        microphoneEnabled = microphone
        intentChanged?(playbackEnabled, microphoneEnabled)
    }

    /// Restore only in-process, previously accepted phone intent after the network
    /// owner was released in background (review M26). Never requests permission or
    /// persists it. A pending interruption or a required local resume stays.
    func restoreAcceptedIntent() {
        intentChanged?(playbackEnabled, microphoneEnabled)
        reconcile()
    }

    /// The network owner goes away (background, session rebuild): stop native
    /// I/O but keep the accepted intent and the interruption state for the
    /// next owner (review M26). Unlike `stopFromPhone`, nothing is reported as
    /// switched off.
    func releaseNetworkOwner() {
        stopIO()
        deactivateSession()
        switch state {
        case .interrupted, .needsLocalResume, .permissionDenied, .stopped: break
        default: publish(.stopped)
        }
    }

    /// Local/group/emergency pause all enter here through the shared media barrier.
    /// Synchronous return means native capture stopped and old callbacks drained.
    func setSharingPaused(_ value: Bool) {
        guard paused != value else { return }
        paused = value
        reconcile()
    }
    /// Apply remote release, inactivity, or core pause without reasserting local intent.
    func applyCoreIntent(playback: Bool, microphone: Bool, paused: Bool, resources: UInt8) {
        guard playbackEnabled != playback || microphoneEnabled != microphone ||
            self.paused != paused || coreResources != resources else { return }
        playbackEnabled = playback
        microphoneEnabled = microphone
        self.paused = paused
        coreResources = resources
        reconcile()
    }
    func resumeForCore() {
        if paused { setSharingPaused(false); return }
        guard playbackEnabled || microphoneEnabled else { return }
        // Called only when the core startMask admits every selected direction.
        // A stale-generation attach can fail while another owner rebuilds; true
        // platform failures remain blocked in the core and never reach here.
        switch state {
        case .stopped, .failed: reconcile()
        default: break
        }
    }
    func stopFromPhone() {
        permissionRequest &+= 1
        requestingPermission = false
        interrupted = false
        needsLocalResume = false
        playbackEnabled = false; microphoneEnabled = false
        intentChanged?(false, false)
        reconcile()
    }
    func waitForTransition() async {
        if state == .preparing { await withCheckedContinuation { settledWaiters.append($0) } }
    }
    private func publish(_ value: State) {
        let wasEligible = keepsBackgroundSession
        state = value
        if value != .preparing {
            let waiters = settledWaiters; settledWaiters.removeAll()
            for waiter in waiters { waiter.resume() }
        }
        stateChanged?(value, epoch)
        if wasEligible != keepsBackgroundSession { backgroundEligibilityChanged?(keepsBackgroundSession) }
    }
    private func stopIO() {
        transitionRevision &+= 1
        io.stop()
        if let generation = activeGeneration {
            activeGeneration = nil
            stoppedGeneration(generation)
        }
    }
    private func deactivateSession() {
        if sessionActivated {
            sessionActivated = false
            try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
        }
    }
    private func reconcile() {
        stopIO()
        guard playbackEnabled || microphoneEnabled || requestingPermission else {
            deactivateSession()
            publish(.stopped)
            return
        }
        guard !paused else { publish(.paused); return }
        guard !requestingPermission else { publish(.preparing); return }
        guard !needsLocalResume else { publish(.needsLocalResume); return }
        guard !interrupted else { publish(.interrupted); return }
        let playback = playbackEnabled && coreResources & 1 != 0
        let microphone = microphoneEnabled && coreResources & 2 != 0
        guard playback || microphone else {
            deactivateSession()
            publish(.stopped)
            return
        }
        if microphone && AVAudioApplication.shared.recordPermission != .granted {
            permissionChanged?()
            failureObserved?(.permissionDenied, activeGeneration)
            deactivateSession()
            publish(.permissionDenied); return
        }
        let revision = transitionRevision
        publish(.preparing)
        Task { @MainActor [weak self] in
            guard let self, revision == self.transitionRevision else { return }
            do {
                // Core reserves starts/rebuilds and supplies the shared media generation.
                // An awaiting grant cannot restart I/O after pause/cancel/interruption.
                let generation = try await prepareGeneration(playback, microphone)
                guard revision == transitionRevision else { stoppedGeneration(generation); return }
                activeGeneration = generation
                try startIO(generation: generation, playback: playback, microphone: microphone)
            } catch {
                guard revision == transitionRevision else { return }
                if error is CancellationError {
                    stopIO(); publish(paused ? .paused : .stopped)
                } else if Self.otherAudioHoldsSession(error) {
                    // Still interrupted, in effect: the switches stay on and resume on the next
                    // interruption end or when the app is in front again.
                    stopIO()
                    interrupted = true
                    failureObserved?(.interrupted, activeGeneration)
                    publish(.interrupted)
                } else {
                    failureObserved?(.failed(error.localizedDescription), activeGeneration)
                    stopIO()
                    deactivateSession()
                    publish(.failed(error.localizedDescription))
                }
            }
        }
    }
    private func startIO(generation: UInt64, playback: Bool, microphone: Bool) throws {
        let session = AVAudioSession.sharedInstance()
        // No voiceChat/voice processing/AEC. System volume is never set here.
        var options: AVAudioSession.CategoryOptions = microphone
            ? [.defaultToSpeaker, .allowBluetoothHFP, .allowBluetoothA2DP] : []
        // Capturing the microphone must not interrupt the other app whose
        // playback ReplayKit is forwarding. No silent playback keeps us alive.
        if microphone && forwardingCaptureActive { options.insert(.mixWithOthers) }
        try session.setCategory(microphone ? .playAndRecord : .playback, mode: .default, options: options)
        // iOS suppresses haptics during recording by default; keep control-page feedback available.
        if microphone { try session.setAllowHapticsAndSystemSoundsDuringRecording(true) }
        try session.setPreferredSampleRate(48_000)
        try session.setPreferredIOBufferDuration(0.005)
        if !sessionActivated {
            try session.setActive(true)
            sessionActivated = true
        }
        actualSampleRate = session.sampleRate
        actualBufferDuration = session.ioBufferDuration
        actualInputLatency = microphone ? session.inputLatency : 0
        actualOutputLatency = session.outputLatency
        inputRoute = session.currentRoute.inputs.map { $0.portName }.joined(separator: ", ")
        route = session.currentRoute.outputs.map { $0.portName }.joined(separator: ", ")
        epoch = generation
        try io.start(withPlayback: playback, microphone: microphone, epoch: epoch)
        actualInputRate = io.inputRate
        // Echo cancellation diagnostics: the I/O timing the Case A alignment
        // starts from. Port types only (no device names); host-time validity
        // per callback is counted by the core (`oink_echo` event=aec_mobile).
        let ports = { (ports: [AVAudioSessionPortDescription]) in
            ports.map { $0.portType.rawValue }.joined(separator: ",")
        }
        Self.log.info("""
            audio io started generation=\(generation, privacy: .public) playback=\(playback, privacy: .public) \
            microphone=\(microphone, privacy: .public) rate=\(self.actualSampleRate, privacy: .public) \
            input_rate=\(self.actualInputRate, privacy: .public) \
            io_buffer_ms=\(self.actualBufferDuration * 1000, privacy: .public) \
            input_latency_ms=\(self.actualInputLatency * 1000, privacy: .public) \
            output_latency_ms=\(self.actualOutputLatency * 1000, privacy: .public) \
            inputs=\(ports(session.currentRoute.inputs), privacy: .public) \
            outputs=\(ports(session.currentRoute.outputs), privacy: .public)
            """)
        publish(.running)
    }
    private func interruption(_ note: Notification) {
        guard let raw = (note.userInfo?[AVAudioSessionInterruptionTypeKey] as? NSNumber)?.uintValue,
              let type = AVAudioSession.InterruptionType(rawValue: raw) else { return }
        if type == .began {
            // Suspension notifications describe an earlier system deactivation and
            // can arrive after foreground recovery. They must not pause a new graph.
            if OinkAudioInterruptionWasSuspended(note) { return }
            sessionActivated = false // The OS has already deactivated this session.
            interrupted = true
            failureObserved?(.interrupted, activeGeneration)
            stopIO()
            if playbackEnabled || microphoneEnabled { publish(.interrupted) }
        } else {
            guard interrupted else { return }
            // With or without shouldResume (user 2026-11-12: receiving resumes by itself and the
            // computers forward to this phone again). shouldResume is the interrupter's hint
            // that resuming is welcome; without it iOS still lets the app activate its session:
            // in front it takes the audio over, in the background a non-mixable session cannot
            // interrupt another app's audio (setActive fails with cannotInterruptOthers), and
            // then the switches wait for the app's next turn in front (startIO's failure).
            resumeAfterInterruption()
        }
    }

    /// The interruption is over (its end, or the app back in front without one): the accepted
    /// switches run again; a network owner released for the interruption is asked for again.
    private func resumeAfterInterruption() {
        interrupted = false
        recoveryRequested?()
        reconcile()
        if playbackEnabled || microphoneEnabled { resumeRequested?() }
    }

    /// The session cannot be activated because other audio holds it: a non-mixable session
    /// started in the background cannot interrupt another app (cannotInterruptOthers), or a
    /// higher-priority one (a call) still runs (insufficientPriority).
    private static func otherAudioHoldsSession(_ error: Error) -> Bool {
        let code = (error as NSError).code
        return code == AVAudioSession.ErrorCode.cannotInterruptOthers.rawValue
            || code == AVAudioSession.ErrorCode.insufficientPriority.rawValue
    }
    private func mediaServicesUnavailable() {
        sessionActivated = false
        needsLocalResume = true
        interrupted = false
        failureObserved?(.needsLocalResume, activeGeneration)
        stopIO()
        if playbackEnabled || microphoneEnabled { publish(.needsLocalResume) }
        // Apple's reset contract requires a user action before reactivation.
        // The next local intent rebuilds session properties and all engine objects.
    }
    private func rebuildIfRunning() {
        guard state == .running else { return }
        reconcile()
    }
    deinit { for token in observers { NotificationCenter.default.removeObserver(token) } }
}
