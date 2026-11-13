import ReplayKit
import UIKit
import AVFAudio
import OSLog

final class SampleHandler: RPBroadcastSampleHandler {
    private var runtime: BroadcastRuntime?
    private var worker: ForwardDriver?
    private let lock = NSLock()
    private var finished = false
    private var capturePaused = false

    override func broadcastStarted(withSetupInfo setupInfo: [String: NSObject]?) {
        lock.withLock { finished = false; capturePaused = false }
        Task { @MainActor [self] in
            guard !self.lock.withLock({ self.finished }) else { return }
            do {
                let runtime = try BroadcastRuntime { [weak self] error in
                    guard let self, !self.lock.withLock({ self.finished }) else { return }
                    self.finishBroadcastWithError(error)
                }
                self.runtime = runtime
                let installed = self.lock.withLock {
                    guard !self.finished else { return false }
                    self.worker = runtime.driver
                    return true
                }
                guard installed else { await runtime.finish(); self.runtime = nil; return }
                runtime.pause(self.lock.withLock { self.capturePaused })
                await runtime.start()
            } catch { self.finishBroadcastWithError(error) }
        }
    }
    override func processSampleBuffer(_ sampleBuffer: CMSampleBuffer, with sampleBufferType: RPSampleBufferType) {
        // Return video immediately: queuing IOSurfaces can exceed ReplayKit's
        // small jetsam budget even though we never encode a video frame.
        guard sampleBufferType == .audioApp else { return }
        lock.lock(); let worker = worker; lock.unlock()
        worker?.capture(sampleBuffer)
    }
    override func broadcastPaused() {
        lock.withLock { capturePaused = true }
        publishPause()
    }
    override func broadcastResumed() {
        lock.withLock { capturePaused = false }
        publishPause()
    }
    private func publishPause() {
        // Setup and Rust start are asynchronous; preserve callbacks that arrive
        // before runtime installation and apply the latest state, not a stale task.
        Task { @MainActor in self.runtime?.pause(self.lock.withLock { self.capturePaused }) }
    }
    override func broadcastFinished() {
        lock.withLock { finished = true; worker = nil }
        Task { @MainActor in await self.runtime?.finish(); self.runtime = nil }
    }
}

@MainActor
private final class BroadcastRuntime {
    let pipe: OinkBroadcastPipe
    let driver: ForwardDriver
    private let root: URL
    private var owner: MobileSession?
    private var eventListener: BroadcastEvents?
    private var interruptionObserver: NSObjectProtocol?
    private var replayKitPaused = false
    private var systemInterrupted = false
    private let discovery = MobileDiscovery()
    private let bonjour = BonjourDiscovery()
    private let publisher = BonjourPublisher()
    private var appSignal: BroadcastSignal?
    private var changedSignal: BroadcastSignal?
    private var identitySeed: Data?
    private var dirty = false
    private let released = OwnerReleaseSignal()
    private var timer: Timer?
    private var transition: Task<Void, Never>?
    private var ending = false
    private var finished = false
    private let ui = BroadcastUIPublisher()
    private var uiExit: String?
    private var footprintPeak: UInt64 = 0
    private var footprintSamples = 0
    private let finishSystem: (Error) -> Void
    private let log = Logger(subsystem: "com.hachiwii.oink", category: "broadcast")

    init(finishSystem: @escaping (Error) -> Void) throws {
        root = try SharedStore.root()
        try DeviceOnlyStorage.prepare(in: root)
        guard let pipe = OinkBroadcastPipe(root: root.path), pipe.claimBroadcast() else {
            throw OinkFailure(message: "音频广播已在运行或共享容器不可用。")
        }
        self.pipe = pipe; driver = ForwardDriver(pipe: pipe); self.finishSystem = finishSystem
        driver.didDisable = { [weak self] in Task { @MainActor in self?.requestStop() } }
        driver.invalidFormat = { [weak self] in Task { @MainActor in self?.requestStop(message: "无法读取广播音频格式。") } }
        bonjour.networkChanged = { [weak self] in self?.discovery.networkChanged(generation: $0) }
        bonjour.upsert = { [weak self] service in
            self?.discovery.upsert(generation: service.generation, key: service.key, txt: service.txt,
                endpoints: service.endpoints.map { ResolvedEndpoint(address: $0.ip, port: $0.port, scopeId: $0.scopeID) })
        }
        bonjour.withdraw = { [weak self] in self?.discovery.withdraw(generation: $1, key: $0) }
    }
    func start() async {
        appSignal = BroadcastSignal(.appRequested) { [weak self] in self?.reconcile() }
        changedSignal = BroadcastSignal(.changed) { [weak self] in self?.reconcile() }
        released.observe { [weak self] in self?.reconcile() }
        // A killed owner cannot post owner-released. Probe the actual kernel
        // lease while waiting, never infer death from an elapsed heartbeat.
        // Normal releases still wake immediately through the Darwin hint.
        timer = Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.reconcile() }
        }
        interruptionObserver = NotificationCenter.default.addObserver(
            forName: AVAudioSession.interruptionNotification, object: nil, queue: .main) { [weak self] note in
                MainActor.assumeIsolated {
                    guard let self else { return }
                    let began = (note.userInfo?[AVAudioSessionInterruptionTypeKey] as? NSNumber)?.uintValue ==
                        AVAudioSession.InterruptionType.began.rawValue
                    let options = (note.userInfo?[AVAudioSessionInterruptionOptionKey] as? NSNumber)?.uintValue ?? 0
                    if began { self.systemInterrupted = true }
                    else if AVAudioSession.InterruptionOptions(rawValue: options).contains(.shouldResume) {
                        self.systemInterrupted = false
                    }
                    self.applyPause()
                }
            }
        ui.publish(active: true, paused: pipe.paused)
        BroadcastSignal.post(.changed)
        reconcile()
        log.info("broadcast_started phys_footprint=\(OinkBroadcastPhysicalFootprint())")
    }
    func reconcile() {
        guard !ending else { return }
        footprintPeak = max(footprintPeak, OinkBroadcastPhysicalFootprint())
        footprintSamples += 1
        if footprintSamples % 20 == 0 {
            log.info("broadcast_memory phys_footprint=\(OinkBroadcastPhysicalFootprint()) peak=\(self.footprintPeak)")
        }
        if pipe.stopRequested { requestStop(); return }
        dirty = true
        guard transition == nil else { return }
        transition = Task { @MainActor in
            repeat {
                self.dirty = false
                if self.ending { break }
                if self.pipe.appRequested { await self.releaseOwner() }
                else if self.owner == nil { await self.acquireOwner() }
            } while self.dirty && !self.ending
            self.transition = nil
        }
    }
    private func acquireOwner() async {
        do {
            let seed: Data
            if let identitySeed { seed = identitySeed }
            else {
                seed = try IdentityStore.requireExisting()
                identitySeed = seed
            }
            let config = MobileConfig(dataDir: root.path, cacheDir: root.appendingPathComponent("Cache").path,
                logDir: root.appendingPathComponent("Logs").path, deviceName: SystemDeviceName.current,
                platform: .ios, profile: .broadcast,
                formFactor: UIDevice.current.userInterfaceIdiom == .pad ? .tablet : .phone)
            let session = try await MobileSession.start(config: config, discovery: discovery,
                identitySeed: seed)
            owner = session
            if ending || pipe.appRequested || pipe.stopRequested { await releaseOwner(); return }
            try await session.resume()
            session.updatePlatformFacts(facts: MobilePlatformFacts(microphoneSend: false,
                speakerReceive: false, speakerSend: true, privilegedInputReady: false))
            driver.bind(session.media())
            let listener = BroadcastEvents { [weak self, weak session] event in
                if case .deviceCards(let snapshot) = event {
                    Task { @MainActor in
                        guard let self, let session, self.owner === session, !self.ending else { return }
                        self.publishUI(snapshot)
                    }
                }
                guard case .advertisementChanged = event else { return }
                Task { @MainActor in
                    guard let self, let session, self.owner === session, !self.ending else { return }
                    do { try await self.publish(session) }
                    catch { self.requestStop(message: error.localizedDescription) }
                }
            }
            eventListener = listener
            try await session.subscribe(listener: listener)
            if let snapshot = try? await session.deviceCards() { publishUI(snapshot) }
            try await publish(session)
            bonjour.start()
            log.info("broadcast_owner phys_footprint=\(OinkBroadcastPhysicalFootprint())")
        } catch MobileError.OwnerBusy {
            // The share extension or app owns mobile-session.lock. Ring-only
            // mode remains valid; release hints and kernel lease probes resume it.
        } catch {
            await releaseOwner()
            requestStop(message: error.localizedDescription)
        }
    }
    private func publishUI(_ snapshot: DeviceCardsView) {
        uiExit = snapshot.cards.first { card in
            guard !card.isSelf else { return false }
            if case .offline = card.connection { return false }
            return card.status?.speaker.isExit == true || card.mobileAudio?.speaker == .active || card.mobileAudio?.speaker == .starting
        }?.name
        ui.publish(active: pipe.broadcasting, paused: pipe.paused, exit: uiExit)
    }
    private func publish(_ session: MobileSession) async throws {
        let advertisement = try await session.advertisement()
        guard owner === session, !ending else { return }
        try publisher.publish(instance: advertisement.instance, type: advertisement.serviceType,
            port: advertisement.port, txt: advertisement.txt)
    }
    private func releaseOwner() async {
        guard let owner else { return }
        driver.bind(nil)
        publisher.stop(); bonjour.stop()
        self.owner = nil
        eventListener = nil
        try? await owner.prepareSuspend()
        try? await owner.shutdown()
        OwnerReleaseSignal.post()
    }
    func pause(_ paused: Bool) {
        replayKitPaused = paused
        // ReplayKit's explicit resume is authoritative even if the audio
        // session did not send a matching end interruption notification.
        if !paused { systemInterrupted = false }
        applyPause()
    }
    private func applyPause() {
        let paused = replayKitPaused || systemInterrupted
        pipe.paused = paused; driver.setPaused(paused)
        ui.publish(active: pipe.broadcasting, paused: paused, exit: uiExit)
        BroadcastSignal.post(.changed)
    }
    func requestStop(message: String = "Oink 已停止转发音频。") {
        guard !ending else { return }
        ending = true
        pipe.stopRequested = true
        Task { @MainActor in
            await self.transition?.value
            await self.finish()
            self.finishSystem(NSError(domain: "com.hachiwii.oink.broadcast", code: 1,
                userInfo: [NSLocalizedDescriptionKey: message]))
        }
    }
    func finish() async {
        guard !finished else { return }
        finished = true
        pipe.stopRequested = true
        ending = true; timer?.invalidate(); timer = nil; appSignal = nil; changedSignal = nil
        if let interruptionObserver { NotificationCenter.default.removeObserver(interruptionObserver) }
        interruptionObserver = nil
        await transition?.value
        await releaseOwner()
        driver.bind(nil)
        pipe.endBroadcast()
        ui.publish(active: false, paused: false)
        BroadcastSignal.post(.changed)
        log.info("broadcast_finished phys_footprint=\(OinkBroadcastPhysicalFootprint()) peak=\(self.footprintPeak)")
    }
}

private final class BroadcastEvents: MobileEventListener, @unchecked Sendable {
    private let changed: @Sendable (MobileEvent) -> Void
    init(_ changed: @escaping @Sendable (MobileEvent) -> Void) { self.changed = changed }
    func onEvent(event: MobileEvent) { changed(event) }
}
