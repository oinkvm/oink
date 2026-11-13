import Foundation
import UIKit
import Security
import UniformTypeIdentifiers
import ImageIO
#if OINK_MAIN_APP
import Combine
#endif

enum DeviceConnection: String, Codable { case thisDevice, online, offline }

/// Why an offline card is offline. `protocolIncompatible` shows the
/// "circle upgrade" icon and「版本过低，请升级」instead of「上次在线」(decision 3).
enum IOSOfflineReason: Equatable { case disconnected, connecting, addressStale, protocolIncompatible }

/// One device row. Plain fields are the list projection; `source` is the
/// full core card (status, actions, tokens) used by the control sheet.
struct IOSDeviceCard: Identifiable, Equatable {
    var device: String
    var name: String
    var connection: DeviceConnection
    var isSelf: Bool
    var isContentTargetPc: Bool
    /// "android", "ios", "macos", "windows", "linux" or "other".
    var os: String = ""
    /// PC-style ID (Crockford base32 `XXXX-XXXX-XXXX`).
    var fingerprint: String = ""
    var latencyMs: Int? = nil
    var lastSeenMs: UInt64? = nil
    var offlineReason: IOSOfflineReason? = nil
    /// Online Android device whose privileged mode is on and running (core
    /// `DeviceCard.androidPrivileged`): its badge wears the crown.
    var androidPrivileged = false
    /// Icon / sheet state projected from `source` (see `CoreSession.controls`).
    var controls = DeviceControls()
    #if OINK_MOBILE_CORE_FFI
    var source: DeviceCard? = nil
    #endif
    var id: String { device }
    var isProtocolIncompatible: Bool { offlineReason == .protocolIncompatible }
}
/// Live link latency (ms) of the devices, observed only by the views that show it.
@MainActor
final class CardLatency: ObservableObject {
    /// Never updated: for a view that must not follow the latency while hidden.
    static let idle = CardLatency()
    @Published private(set) var ms: [String: Int] = [:]

    /// A new card snapshot: its latencies.
    func reset(_ cards: [IOSDeviceCard]) {
        let values = Dictionary(cards.compactMap { card in card.latencyMs.map { (card.device, $0) } }, uniquingKeysWith: { a, _ in a })
        if values != ms { ms = values }
    }

    /// DeviceCardSignals: new latencies of some devices (nil: no measurement).
    func update(_ signals: [(device: String, ms: Int?)]) {
        var values = ms
        for signal in signals { values[signal.device] = signal.ms }
        if values != ms { ms = values }
    }
}

struct IOSDeviceCardsView: Equatable {
    var stream = "local"
    var revision: UInt64 = 0
    var cards: [IOSDeviceCard] = []
}
struct NearbyDevice: Identifiable, Codable, Equatable {
    var device: String
    var name: String
    var compatible: Bool = true
    var reachable: Bool = true
    var os: String = ""
    /// `ip:port` of the selected address.
    var address: String? = nil
    /// Size of the device's group when it has more than one member.
    var groupCount: Int? = nil
    var fingerprint: String = ""
    var id: String { device }
}
struct IOSNearbySnapshot: Codable { var rows: [NearbyDevice] = [] }
struct IOSPairingSnapshot: Codable { var rows: [String] = [] }
struct IOSBlockedDevicesSnapshot: Codable { var devices: [String] = [] }
struct IOSPeerCompatibility: Identifiable, Codable {
    var device: String
    var problem: String?
    var id: String { device }
}
struct IOSFoundPeer: Codable {
    var queryId: String
    var generation: UInt64
    var peer: NearbyDevice
}
enum IOSPairTarget { case nearbyDevice(device: String); case preparedQuery(queryId: String, generation: UInt64) }

/// This phone's identity for the add-device info dialog.
struct IOSLocalIdentity: Equatable {
    var addresses: [String] = []
    var port: UInt16?
    /// PC-style ID (`XXXX-XXXX-XXXX`).
    var fingerprint = ""
}

/// A completed pairing for the「已与 X 配对」toast (no undo, decision 38).
struct IOSPairingCompleted: Identifiable, Equatable {
    let id: UInt64
    let peerName: String
    let peerDevice: String
}

#if OINK_MOBILE_CORE_FFI
/// Hands the core's events to the main actor in batches: whatever arrived since the
/// last main-queue turn is applied in one turn, in order. One main-actor task per
/// event (a download reports every chunk) gave the views a change to process per
/// event while the transfer list scrolled.
private final class IOSMobileEventListener: MobileEventListener, @unchecked Sendable {
    private let handler: @MainActor ([MobileEvent]) -> Void
    private let lock = NSLock()
    private var pending: [MobileEvent] = []

    init(handler: @escaping @MainActor ([MobileEvent]) -> Void) { self.handler = handler }

    nonisolated func onEvent(event: MobileEvent) {
        lock.lock()
        let first = pending.isEmpty
        pending.append(event)
        lock.unlock()
        guard first else { return }
        DispatchQueue.main.async { [self] in
            lock.lock()
            let batch = pending
            pending = []
            lock.unlock()
            MainActor.assumeIsolated { handler(batch) }
        }
    }
}

/// Final outcome of one own share, from the core's single `DeliveryEvent`.
struct IOSDeliveryOutcome: Equatable {
    var state: MobileDeliveryState
    var failure: MobileDeliveryFailure?
    var recordIDs: [Int64]
}
#endif

/// Transfer progress, published on its own: every download chunk and send report
/// changes it, and published on the session each change rebuilt every view that
/// observes the session (all three tabs). The progress views observe this.
@MainActor
final class TransferProgressStore: ObservableObject {
    @Published var records: [Int64: TransferOperationProgress] = [:]
    @Published var operations: [String: TransferOperationProgress] = [:]
    @Published var revision = 0
}

/// The only bridge between SwiftUI and the Rust mobile owner.
///
/// Properties without `private(set)` are written by the CoreSession
/// extensions in the other `Core*.swift` files; views treat every published
/// value as read-only and change state only through the methods.
@MainActor
final class CoreSession: ObservableObject {
    static let shared = CoreSession()
    @Published private(set) var devices: [NativeDevice] = []
    /// Transfer progress lives in `progress` (not published on the session).
    let progress = TransferProgressStore()
    var transferOperations: [String: TransferOperationProgress] {
        get { progress.operations }
        set { progress.operations = newValue }
    }
    /// Download / upload progress of one history record, keyed by record id.
    var recordProgress: [Int64: TransferOperationProgress] {
        get { progress.records }
        set { progress.records = newValue }
    }
    var transferRevision: Int {
        get { progress.revision }
        set { progress.revision = newValue }
    }
    /// Records that changed; TransferModel reloads exactly these.
    @Published var transferChange = IOSTransferChange()
    @Published private(set) var activeShareOperationID: String?
    /// Own shares (operation hex) the core holds back while their offline target is away
    /// (`MobileEvent.transferShareWaiting`, user 2026-11-11). Nothing of them is recorded yet.
    @Published private(set) var sharesWaitingForTarget: Set<String> = []
    /// Bumped whenever a new Rust session starts; history views reload.
    @Published private(set) var sessionRevision = 0
    /// Folder pages and failed listings by (folder message, directory node).
    @Published var folderListings = FolderListings()
    /// Folder pages by (folder message, directory node).
    var transferDirectories: [IOSDirectoryKey: TransferDirectoryPage] {
        get { folderListings.pages }
        set { folderListings.pages = newValue }
    }
    /// A file that finished "download and open"; present it with QuickLook.
    @Published var fileToOpen: IOSFileToOpen?
    @Published var transferError: String?
    @Published private(set) var status = "已离线"
    @Published var busy = false
    /// The latency in each card is the one of the snapshot; the live value is `latency`.
    @Published var cards: IOSDeviceCardsView? {
        didSet { latency.reset(cards?.cards ?? []) }
    }
    /// Link latency by device, renewed about once a second by DeviceCardSignals. Its own
    /// object: published on the session, each renewal rebuilt every view observing the
    /// session (all three tabs) and dropped frames while the transfer list scrolled.
    let latency = CardLatency()
    @Published var nearby: IOSNearbySnapshot?
    @Published var pairings: IOSPairingSnapshot?
    @Published var blocked: IOSBlockedDevicesSnapshot?
    @Published var managementStatus = ""
    /// The saved device group did not contain this phone's identity (new
    /// iPhone or restored backup) and was replaced by a group of this phone
    /// alone; the app asks the user to pair again (review M20, D8).
    @Published var groupReplaced = false
    @Published var foundPeer: IOSFoundPeer?
    /// Typed failure of the last manual address query (manual add sheet).
    @Published var manualAddProblem: IOSManualAddProblem?
    @Published var pairingCompleted: IOSPairingCompleted?
    @Published var compatibility: [IOSPeerCompatibility] = []
    /// iOS denies Local Network access: the add page shows「无法搜索附近的设备」.
    @Published private(set) var discoveryUnavailable = false
    @Published var localIdentity = IOSLocalIdentity()
    /// Core-owned settings (source of truth is Rust, not UserDefaults).
    @Published var settings: IOSCoreSettings?
    @Published var localDeviceName: IOSLocalDeviceName?
    /// This phone's speaker/microphone switches (top bar).
    @Published var localAudio = IOSLocalAudio()
    /// Transfer page proxy, shared with the share extension via the App Group.
    @Published private(set) var transferProxyID: String?
    #if OINK_MOBILE_CORE_FFI
    @Published private(set) var pairingSnapshot: PairingSnapshot? { didSet { updateScreenAwake() } }
    @Published var blockedDevices: [BlockedDevice] = []
    @Published var exitVolume: ExitVolumeCard?
    /// Why no session is running (review M27); nil while one runs.
    @Published private(set) var sessionProblem: IOSSessionProblem?
    /// The screen must not lock by itself (review M28); the app applies it
    /// to `UIApplication.isIdleTimerDisabled`.
    @Published private(set) var keepsScreenAwake = false
    /// Own shares between their start and their DeliveryEvent (M28).
    private var activeShares: Set<String> = [] { didSet { updateScreenAwake() } }
    /// Records the core is downloading (M28): added on progress, removed
    /// once the record is no longer `downloading`.
    var activeDownloads: Set<Int64> = [] { didSet { updateScreenAwake() } }
    /// Own shares the user cancelled (M31).
    private var cancelledShares: Set<String> = []
    private let ownerReleased = OwnerReleaseSignal()
    #endif
    /// Session starts and releases in order, and what holds the session:
    /// a visible scene, kept audio, running background App Intents (M29).
    private let transitions = SessionTransitions()
    /// A scene of the main app is on screen (active or inactive). It holds
    /// the session; `.inactive` (Siri, Control Center, the permission sheet)
    /// does not end it (review M29).
    private var sceneVisible: Bool {
        get { transitions.sceneVisible }
        set { transitions.sceneVisible = newValue }
    }
    var pairingQueryGeneration: UInt64 = 0
    var pairingQueryOperation: String?
    /// `ip:port` of the last manual query (shown on the found card).
    var pairingQueryAddress: String?
    #if OINK_MAIN_APP
    @Published private(set) var audioOwner: MobileAudioOwner?
    private(set) var forwarding: ForwardController!
    func refreshForwardCapabilities() { updateNativeCapabilities() }
    var audioEligibilityChanged: (() -> Void)?
    var keepsAudioBackgroundSession: Bool { audioOwner?.audio.keepsBackgroundSession == true }
    /// An interruption ended while no session ran: the accepted audio intent
    /// needs a session to play again (review H12).
    var audioNeedsSession: Bool { audioOwner?.needsNetworkOwner == true }
    private var needsMainOwner: Bool {
        sceneVisible || transitions.operations > 0 || keepsAudioBackgroundSession || audioNeedsSession
    }
    func broadcastEnded() {
        // A jetsam-killed extension cannot post owner-released. Its kernel
        // liveness lock going away is another release hint, not a timeout.
        guard sessionProblem == .busy, needsMainOwner else { return }
        Task { await resume() }
    }
    func expireBackgroundAudioTransition() { audioOwner?.audio.setSharingPaused(true) }
    private var audioObservation: AnyCancellable?
    /// Control Center toggles' App Group state and the Live Activities.
    private(set) var audioControls: AudioControlsPublisher?
    #endif

    private var foreground = false
    #if OINK_MOBILE_CORE_FFI
    private(set) var mobileSession: MobileSession?
    private var sessionProfile: SessionProfile?
    private var mobileDiscovery: MobileDiscovery?
    private var mobileEventListener: IOSMobileEventListener?
    private var diagnosticsOwner: MobileDiagnosticsOwner?
    private var diagnosticsAvailable = true
    private let bonjour = BonjourDiscovery()
    private let publisher = BonjourPublisher()
    private var startupError: Error?
    private var deliveryWaiters: [String: CheckedContinuation<IOSDeliveryOutcome, Never>] = [:]
    /// Outcomes that arrived before their waiter was registered.
    private var settledDeliveries: [String: IOSDeliveryOutcome] = [:]
    #endif

    private init() {
        devices = ((try? SharedStore.load([NativeDevice].self, name: "devices.json", deviceOnly: true)) ?? []).map {
            var copy = $0; copy.online = false; return copy
        }
        transferProxyID = SharedStore.defaults.string(forKey: Self.transferProxyKey)
        #if OINK_MOBILE_CORE_FFI
        bonjour.networkChanged = { [weak self] generation in
            self?.mobileDiscovery?.networkChanged(generation: generation)
        }
        bonjour.upsert = { [weak self] service in
            self?.mobileDiscovery?.upsert(generation: service.generation, key: service.key, txt: service.txt,
                endpoints: service.endpoints.map { ResolvedEndpoint(address: $0.ip, port: $0.port, scopeId: $0.scopeID) })
        }
        bonjour.withdraw = { [weak self] key, generation in
            self?.mobileDiscovery?.withdraw(generation: generation, key: key)
        }
        bonjour.failure = { [weak self] error in self?.status = error }
        bonjour.localNetworkDenied = { [weak self] denied in
            guard let self, self.discoveryUnavailable != denied else { return }
            self.discoveryUnavailable = denied
        }
        publisher.failure = { [weak self] error in self?.status = error }
        #if OINK_MAIN_APP
        // The share extension released the session it owned (review M27):
        // start ours now instead of polling or waiting for the next resume.
        ownerReleased.observe { [weak self] in
            guard let self, self.sessionProblem == .busy, self.needsMainOwner else { return }
            Task { await self.resume() }
        }
        #endif
        #endif
        #if OINK_MAIN_APP
        audioOwner = MobileAudioOwner()
        forwarding = ForwardController(core: self)
        audioOwner?.eligibilityChanged = { [weak self] in self?.audioEligibilityChanged?() }
        audioOwner?.capabilitiesChanged = { [weak self] in self?.updateNativeCapabilities() }
        if let audio = audioOwner?.audio {
            // The toolbar observes CoreSession only; mirror the nested audio
            // session so the speaker/microphone switches redraw.
            audioObservation = audio.$playbackEnabled.combineLatest(audio.$microphoneEnabled, audio.$state)
                .sink { [weak self] playback, microphone, state in
                    self?.localAudio.speakerOn = playback
                    self?.localAudio.microphoneOn = microphone
                    self?.localAudio.issue = IOSLocalAudio.issue(state)
                }
        }
        audioControls = AudioControlsPublisher(core: self)
        #if OINK_MOBILE_CORE_FFI
        diagnosticsOwner = MobileDiagnosticsOwner()
        audioOwner?.diagnosticEvent = { [weak self] code in
            self?.diagnosticsOwner?.emit(kind: "audio", code: code)
        }
        #endif
        #endif
    }

    #if DEBUG
    /// Design state transfer-p2p-live-offline: the chosen phone leaves while the page is open.
    /// The UI test takes it offline itself, once it has read the first state (a timer raced
    /// a slow simulator's launch).
    func fixtureTakeOffline(_ device: String) {
        guard var current = cards, let index = current.cards.firstIndex(where: { $0.device == device }) else { return }
        current.cards[index].connection = .offline
        current.cards[index].offlineReason = .disconnected
        current.revision += 1
        cards = current
        transferTargets = Self.fixtureTargets(current.cards, revision: current.revision)
    }

    /// Deterministic data for the design-state screenshots (`-oink-ui-scene <id>`,
    /// DEBUG only). It fills the published snapshots the views read; no session
    /// is started (AppLifecycle skips it) and nothing is written to the core.
    /// The target list the core would report for the fixture cards: online devices,
    /// computers first, in card order.
    private static func fixtureTargets(_ cards: [IOSDeviceCard], revision: UInt64) -> SendTargets {
        let others = cards.filter { !$0.isSelf }.map {
            SendTarget(id: $0.device, name: $0.name, isComputer: $0.isContentTargetPc, os: $0.os,
                       privileged: $0.androidPrivileged, online: $0.connection != .offline)
        }
        let online = others.filter(\.online)
        return SendTargets(revision: revision, online: online.filter(\.isComputer) + online.filter { !$0.isComputer },
                           otherDevices: others.count, onlyDevice: others.count == 1 ? others[0] : nil)
    }

    func installUIFixtureIfRequested() {
        guard let scene = DesignScene.current else { return }
        func controls(_ mouse: DeviceControls.InputShare, _ keyboard: DeviceControls.InputShare,
                      _ speaker: DeviceControls.SpeakerMode, _ mic: DeviceControls.MicMode) -> DeviceControls {
            var value = DeviceControls()
            value.mouse = mouse; value.keyboard = keyboard; value.speaker = speaker; value.mic = mic
            value.speakerLevel = 65; value.micLevel = 65
            return value
        }
        var windowsPc = controls(.shared, .blocked, .forward, .receive)
        if scene.hasPrefix("sheet") {
            windowsPc.speakerLevel = 130; windowsPc.micLevel = 80; windowsPc.locked = true; windowsPc.gaming = .auto; windowsPc.gameActive = true
            if scene == "sheet-speaker-exit" { windowsPc.speaker = .outlet; windowsPc.speakerLevel = 65 }
        }
        func card(_ id: String, _ name: String, _ os: String, _ pc: Bool, _ latency: Int?, _ c: DeviceControls,
                  offline: IOSOfflineReason? = nil, privileged: Bool = false, fingerprint: String) -> IOSDeviceCard {
            IOSDeviceCard(device: id, name: name, connection: offline == nil ? .online : .offline, isSelf: false,
                          isContentTargetPc: pc, os: os, fingerprint: fingerprint, latencyMs: latency,
                          lastSeenMs: offline == nil ? nil : UInt64(Date().addingTimeInterval(-5 * 60).timeIntervalSince1970 * 1000),
                          offlineReason: offline, androidPrivileged: privileged, controls: c)
        }
        var fixtureCards = [
            card("fixture-windowsPc", "Office-PC", "windows", true, 4, windowsPc, fingerprint: "7KQ4-M2PA-9DXF"),
            card("fixture-macMini", "MacBook-Pro", "macos", true, 6, controls(.blocked, .blocked, .forward, .unavailable), fingerprint: "C81H-WZ0R-4TNE"),
            card("fixture-iphone", "iPhone", "ios", false, 8, controls(.local, .local, .outlet, .send), fingerprint: "Q5DJ-7PMV-2KXA"),
            card("fixture-ipad", "iPad", "ios", false, 5, controls(.local, .local, .outlet, .send), fingerprint: "H2RS-K9GB-6WCZ"),
            card("fixture-android", "Android", "android", false, 12, controls(.local, .local, .local, .local),
                 privileged: scene == "devices-android-privileged" || scene == "sheet-android-privileged", fingerprint: "M0VE-3QTY-8JHP"),
            card("fixture-desktop", "DESKTOP-9AO8GPM", "windows", true, nil, DeviceControls(), offline: .disconnected, fingerprint: "R3TB-8WQK-1M5Z"),
        ]
        if scene == "devices-incompatible" {
            fixtureCards.append(card("fixture-old", "Windows-Laptop", "windows", true, nil, DeviceControls(),
                                     offline: .protocolIncompatible, fingerprint: "B6XN-1FKD-QW3S"))
        }
        // Phone-to-phone design states: who is offline (the composer switches, the menu drops them).
        let p2pOffline: Set<String> = switch scene {
        case "transfer-p2p-menu-offline", "transfer-p2p-offline": ["fixture-iphone"]
        case "transfer-p2p-all-offline": Set(fixtureCards.map(\.device))
        default: []
        }
        for index in fixtureCards.indices where p2pOffline.contains(fixtureCards[index].device) {
            fixtureCards[index].connection = .offline
            fixtureCards[index].offlineReason = .disconnected
        }
        let kept = fixtureCards.filter { card in
            switch scene {
            case "devices-empty", "transfer-p2p-none", "settings-alone": return false
            case "devices-online-only": return card.connection != .offline
            case "devices-offline-only": return card.connection == .offline
            // One other device in the group: a computer, a phone, or one that is offline.
            case "transfer-p2p-only-pc": return card.device == "fixture-windowsPc"
            case "transfer-p2p-only-phone": return card.device == "fixture-android"
            case "transfer-p2p-only-offline": return card.device == "fixture-desktop"
            default: return true
            }
        }
        cards = IOSDeviceCardsView(revision: 1, cards: kept)
        transferTargets = Self.fixtureTargets(kept, revision: 1)
        devices = kept.map { NativeDevice(id: $0.device, name: $0.name, online: $0.connection != .offline, computer: $0.isContentTargetPc) }
        localAudio.speakerOn = true
        // This phone's speaker and microphone both on (top bar), or both failed to start.
        if scene == "devices-audio-on" || scene == "devices-audio-failed" { localAudio.microphoneOn = true }
        #if OINK_MOBILE_CORE_FFI
        if scene == "devices-audio-failed" { localAudio.speaker = .failed; localAudio.microphone = .failed }
        #endif
        localIdentity = IOSLocalIdentity(addresses: ["192.0.2.88", "2001:db8::5", "198.51.100.7"], port: 47470,
                                         fingerprint: "4HNM-X8C2-QF7R")
        localDeviceName = IOSLocalDeviceName(name: "iPhone", systemName: "iPhone", custom: false)
        settings = IOSCoreSettings(microphoneIdleTimeout: .five, speakerIdleTimeout: .five,
                                   echoCancellation: true)
        let nearbyRows: [NearbyDevice] = [
            NearbyDevice(device: "fixture-studio", name: "Studio-Mac", os: "macos", address: "192.0.2.42:47470", groupCount: 3, fingerprint: "7KQ4-M2PA-9DXF"),
            NearbyDevice(device: "fixture-k3m8", name: "Windows-PC", os: "windows", address: "192.0.2.37:47470", fingerprint: "C81H-WZ0R-4TNE"),
            NearbyDevice(device: "fixture-android", name: "Phone", os: "android", address: "192.0.2.57:47470", fingerprint: "Q5DJ-7PMV-2KXA"),
            NearbyDevice(device: "fixture-livingipad", name: "客厅的 iPad", os: "ipados", address: "192.0.2.61:47470", fingerprint: "H2RS-K9GB-6WCZ"),
            NearbyDevice(device: "fixture-laptop", name: "Windows-Laptop", compatible: false, os: "windows", address: "192.0.2.19:47470", fingerprint: "M0VE-3QTY-8JHP"),
            NearbyDevice(device: "fixture-mba", name: "MacBook-Air", os: "macos", address: "192.0.2.66:47470", fingerprint: "B6XN-1FKD-QW3S"),
            NearbyDevice(device: "fixture-pixel", name: "Pixel 9", os: "android", address: "192.0.2.73:47470", fingerprint: "T9AE-5HCM-0RVG"),
        ]
        nearby = IOSNearbySnapshot(rows: scene == "add-empty" || scene == "add-unavailable" ? []
                                   : scene == "pair-paired-toast" ? Array(nearbyRows.dropFirst().prefix(4)) : Array(nearbyRows.prefix(5)))
        discoveryUnavailable = scene == "add-unavailable"
        if scene == "manual-found" {
            foundPeer = IOSFoundPeer(queryId: "fixture-query", generation: 1,
                                     peer: NearbyDevice(device: "fixture-office", name: "OFFICE-PC", os: "windows",
                                                        address: "192.0.2.24:47470", fingerprint: "R3TB-8WQK-1M5Z"))
        }
        #if OINK_MOBILE_CORE_FFI
        if scene == "transfer-folder" {
            transferDirectories[IOSDirectoryKey(folderMessageID: 9005, directory: 0)] = TransferDirectoryPage(
                folderMessageID: 9005, directory: 0, entries: [
                    TransferDirectoryEntry(id: 1, parent: 0, name: "界面预览.png", kind: "file", size: 1_800_000, version: 1),
                    TransferDirectoryEntry(id: 2, parent: 0, name: "设计说明.pdf", kind: "file", size: 2_400_000, version: 1),
                    TransferDirectoryEntry(id: 3, parent: 0, name: "图标资源", kind: "directory", size: nil, version: 1),
                ], nextOffset: nil, nextVersion: nil)
        }
        if scene == "transfer-folder-states" {
            // native-rewrite.md「文件夹页」: thumbnail (downloaded / computer's preview),
            // downloaded, not downloaded, downloading, download failed, sub-folder.
            transferDirectories[IOSDirectoryKey(folderMessageID: 9005, directory: 0)] = TransferDirectoryPage(
                folderMessageID: 9005, directory: 0, entries: [
                    TransferDirectoryEntry(id: 1, parent: 0, name: "现场 01.jpg", kind: "file", size: 3_200_000, version: 1),
                    TransferDirectoryEntry(id: 2, parent: 0, name: "现场 02.jpg", kind: "file", size: 2_900_000, version: 1),
                    TransferDirectoryEntry(id: 3, parent: 0, name: "验收单.pdf", kind: "file", size: 1_100_000, version: 1),
                    TransferDirectoryEntry(id: 4, parent: 0, name: "施工说明（第二版，含图纸目录与变更记录）.docx", kind: "file",
                                           size: 640_000, version: 1),
                    TransferDirectoryEntry(id: 5, parent: 0, name: "材料清单.xlsx", kind: "file", size: 88_000, version: 1),
                    TransferDirectoryEntry(id: 6, parent: 0, name: "原始照片.zip", kind: "file", size: 486_000_000, version: 1),
                    TransferDirectoryEntry(id: 7, parent: 0, name: "图纸", kind: "directory", size: nil, version: 1, fileCount: 8),
                ], nextOffset: nil, nextVersion: nil)
            recordProgress[9403] = TransferOperationProgress(received: 40, total: 100)
        }
        if scene == "transfer-folder" { recordProgress[9402] = TransferOperationProgress(received: 40, total: 100) }
        if scene.hasPrefix("transfer") {
            recordProgress[9003] = TransferOperationProgress(received: 40, total: 100)
        }
        var requests: [PendingRequest] = []
        if scene == "add-requests" || scene == "devices-request-badge" {
            requests = [
                PendingRequest(id: "fixture-request-mba", revision: 1,
                               peer: PeerView(device: "fixture-mba", name: "MacBook-Air", os: "macos"),
                               source: GroupRef(id: "mba", name: "MacBook-Air"), target: GroupRef(id: "local", name: "本机"),
                               sourceCount: 2, targetCount: 1),
                PendingRequest(id: "fixture-request-pixel", revision: 1,
                               peer: PeerView(device: "fixture-pixel", name: "Pixel 9", os: "android"),
                               source: GroupRef(id: "pixel", name: "Pixel 9"), target: GroupRef(id: "local", name: "本机"),
                               sourceCount: 1, targetCount: 1),
            ]
        }
        var sessions: [PairView] = []
        if scene.hasPrefix("pair-") && scene != "pair-paired-toast" {
            let incoming = ["pair-entry", "pair-entry-typing", "pair-verifying", "pair-wrong", "pair-wrong-retry",
                            "pair-success", "pair-failed-attempts", "pair-complete-in"].contains(scene)
            let peer = incoming ? PairEndpoint(device: "fixture-mba", name: "MacBook-Air", os: "macos")
                                : PairEndpoint(device: "fixture-studio", name: "Studio-Mac", os: "macos")
            let code: UInt32? = switch scene {
            case "pair-failed-rejected": 13
            case "pair-failed-timeout": 6
            case "pair-failed-superseded": 23
            case "pair-failed-attempts": 20
            default: nil
            }
            let feedback: Feedback = scene == "pair-wrong" ? .wrong : scene.hasPrefix("pair-success") || scene.hasPrefix("pair-complete") ? .correct : .waiting
            let attempts: UInt8 = scene == "pair-failed-attempts" ? 0 : scene.hasPrefix("pair-wrong") ? 2 : 3
            sessions = [PairView(id: "fixture-pair", revision: 1, direction: incoming ? .incoming : .outgoing, peer: peer,
                                 phase: scene == "pair-connecting" ? 0 : 1, attemptsLeft: attempts, feedback: feedback,
                                 terminal: code.map { PairProblem(code: $0, stage: 1, detail: "fixture", network: nil) })]
            nearby?.rows.append(contentsOf: nearbyRows.filter { $0.device == peer.device && !(nearby?.rows.contains($0) ?? false) })
        }
        pairingSnapshot = PairingSnapshot(revision: 1, requests: requests, sessions: sessions)
        #endif
    }
    #endif

    /// PC-style ID of a device for subtitles (cards, nearby rows, the found
    /// peer, then the core's derivation).
    func displayFingerprint(_ device: String) -> String {
        if let value = cards?.cards.first(where: { $0.device == device })?.fingerprint, !value.isEmpty { return value }
        if let value = nearby?.rows.first(where: { $0.device == device })?.fingerprint, !value.isEmpty { return value }
        if let found = foundPeer, found.peer.device == device, !found.peer.fingerprint.isEmpty { return found.peer.fingerprint }
        #if OINK_MOBILE_CORE_FFI
        return (try? deviceFingerprint(device: device)) ?? ""
        #else
        return ""
        #endif
    }

    /// `ip:port` the phone sees a device at (nearby rows, then the manual query).
    func displayAddress(_ device: String) -> String? {
        if let value = nearby?.rows.first(where: { $0.device == device })?.address { return value }
        if let found = foundPeer, found.peer.device == device { return found.peer.address }
        return nil
    }

    func setForeground(_ value: Bool) {
        foreground = value
        #if OINK_MOBILE_CORE_FFI
        try? mobileSession?.transferSetForeground(foreground: value)
        #endif
    }
    /// The main app's scene is on screen (`.active` or `.inactive`).
    func setSceneVisible(_ value: Bool) { sceneVisible = value }

    /// The background grace task expired (review M31): settle every waiting
    /// share now so the shutdown in progress is not held by one; the session
    /// ends and the core records those shares as interrupted.
    func cancelPendingOperations() {
        #if OINK_MOBILE_CORE_FFI
        settleWaiters(.interrupted)
        #endif
    }

    /// A background App Intent starts (review M29); it holds the session
    /// until the paired `finishBackgroundOperation`.
    func beginBackgroundOperation() {
        transitions.beginOperation()
    }

    func resume() async {
        #if OINK_MOBILE_CORE_FFI
        await transitions.run { [weak self] in await self?.resumeSession() }
        #endif
    }

    #if OINK_MOBILE_CORE_FFI
    private func resumeSession() async {
        #if OINK_MAIN_APP
        forwarding.requestOwner()
        #endif
        startupError = nil
        do {
            let root = try SharedStore.root()
            // Group state and pairing records stay on this iPhone: excluded
            // before the core reads or writes them.
            try DeviceOnlyStorage.prepare(in: root)
            // The process decides the profile, never the scene state (review
            // M29): the main app always runs the app session, also when an
            // App Intent or audio recovery starts it in the background.
            let profile: SessionProfile = Bundle.main.bundlePath.hasSuffix(".appex") ? .shareSession : .mobile
            let systemName = SystemDeviceName.current
            if let owner = mobileSession {
                try await owner.resume()
                // iOS has no notification for a changed device name; check on
                // every resume. It only takes effect without a custom name.
                if owner.localDeviceName().systemName != systemName {
                    applyLocalDeviceName(try owner.updateSystemDeviceName(name: systemName))
                }
                updateNativeCapabilities(owner)
                startDiagnostics(owner)
                try await refreshCoreSnapshots(owner)
            } else {
                // Only the visible main app may create the identity.
                let seed: Data
                do {
                    #if OINK_MAIN_APP
                    seed = try foreground ? IdentityStore.readOrCreateForMainApp() : IdentityStore.requireExisting()
                    #else
                    seed = try IdentityStore.requireExisting()
                    #endif
                } catch {
                    throw IdentityUnavailable(message: error.localizedDescription)
                }
                let cache = root.appendingPathComponent("Cache", isDirectory: true)
                let logs = root.appendingPathComponent("Logs", isDirectory: true)
                try FileManager.default.createDirectory(at: cache, withIntermediateDirectories: true)
                try FileManager.default.createDirectory(at: logs, withIntermediateDirectories: true)
                // The system name is only the default; a custom name set in
                // Settings is persisted and applied by Rust.
                let config = MobileConfig(dataDir: root.path, cacheDir: cache.path, logDir: logs.path,
                    deviceName: systemName, platform: .ios, profile: profile,
                    // PCs draw the iPhone or iPad badge from this fact.
                    formFactor: UIDevice.current.userInterfaceIdiom == .pad ? .tablet : .phone,
                    // PCs list this phone with the version Settings → About shows.
                    appVersion: Bundle.main.infoDictionary?["CFBundleGetInfoString"] as? String
                        ?? Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String)
                #if OINK_MAIN_APP
                if profile == .mobile, try initializeMobileGroup(config: config, identitySeed: seed) == .replaced {
                    groupReplaced = true
                    devices = []
                    cards = nil
                    transferTargets = nil
                }
                #endif
                let discovery = MobileDiscovery()
                let owner = try await MobileSession.start(config: config, discovery: discovery, identitySeed: seed)
                let listener = IOSMobileEventListener { [weak self, weak owner] events in
                    guard let self, let owner, self.mobileSession === owner else { return }
                    events.forEach(self.apply)
                }
                mobileDiscovery = discovery
                mobileEventListener = listener
                mobileSession = owner
                try? owner.transferSetForeground(foreground: foreground)
                sessionProfile = profile
#if OINK_MAIN_APP
                if profile == .mobile {
                    audioOwner?.bind(media: owner.media())
                    await audioOwner?.subscribe()
                    forwarding.bind(owner.media())
                }
#endif
                try await owner.resume()
                updateNativeCapabilities(owner)
                #if OINK_MAIN_APP
                // Switches accepted before the previous session was released
                // run again on this one (review M26), once the route facts
                // that grant the speaker are in.
                if profile == .mobile {
                    audioOwner?.restoreAcceptedIntent()
                    forwarding.sessionReady()
                }
                #endif
                startDiagnostics(owner)
                try await owner.subscribe(listener: listener)
                try await publishAdvertisement(owner)
                bonjour.start()
                try await refreshCoreSnapshots(owner)
                loadCoreSettings(owner)
                sessionRevision &+= 1
                #if OINK_MAIN_APP
                if profile == .mobile {
                    try LocalHandoff.shared.serve { [weak self, weak owner] _, command in
                        guard let self, let owner, self.mobileSession === owner else { throw CancellationError() }
                        return try await self.handleHandoff(command, owner: owner)
                    }
                    // Staging of share extensions that ended without cleaning up (L17).
                    if let parent = try? SharedStore.stagingParent() { ShareStagingLease.removeAbandoned(in: parent) }
                    await audioControls?.sessionChanged(live: true)
                }
                #endif
            }
            status = "网络会话已启动"
            sessionProblem = nil
            updateScreenAwake()
        } catch {
            await suspendSession()
            startupError = error
            status = error.localizedDescription
            // A visible reason with a retry (review M27), never an empty
            // group or a fabricated device list.
            sessionProblem = IOSSessionProblem(error)
        }
    }

    /// Resumes every waiting own share with `failure` (session ending or the
    /// background grace expired). The core ends their records itself.
    private func settleWaiters(_ failure: MobileDeliveryFailure) {
        let waiters = deliveryWaiters
        deliveryWaiters = [:]
        for waiter in waiters.values {
            waiter.resume(returning: IOSDeliveryOutcome(state: .failed, failure: failure, recordIDs: []))
        }
    }

    /// `interruption`: the background release because an OS interruption
    /// stopped the audio (the Live Activities stay, AudioActivityRule).
    private func suspendSession(interruption: Bool = false) async {
        #if OINK_MAIN_APP
        // First, before anything else can wait: the Control Center toggles
        // read off and the Live Activities have ended (or, for an
        // interruption, stay) once this returns. The release runs on a
        // background task that may be the process's last execution time; a
        // Live Activity left for later stayed on the phone after iOS
        // suspended it (user 2026-11-12).
        await audioControls?.sessionChanged(live: false, interruption: interruption)
        #endif
        // A share still running in the core ends with the session; the core
        // marks its records Interrupted on the next start. Its waiter is
        // settled first: a handed-over share's handler returns at once, so
        // stopping the handoff below never waits for a transfer (review M31).
        settleWaiters(.interrupted)
        await LocalHandoff.shared.stopServing()
        publisher.stop()
        bonjour.stop()
        let retiring = mobileSession
        mobileSession = nil
        sessionProfile = nil
        mobileEventListener = nil
        mobileDiscovery = nil
        diagnosticsAvailable = true
        settledDeliveries.removeAll()
        cancelledShares.removeAll()
        folderListings.endSession()
        // Downloads end with the session; the next one marks them failed.
        recordProgress.removeAll()
        activeDownloads.removeAll()
        #if OINK_MAIN_APP
        forwarding.unbind()
        await audioOwner?.shutdown()
        #endif
        await diagnosticsOwner?.stop()
        if let retiring {
            try? await retiring.prepareSuspend()
            try? await retiring.shutdown()
            // The owner lock is free now; a main app waiting on it starts
            // its session (review M27).
            OwnerReleaseSignal.post()
        }
        #if OINK_MAIN_APP
        if !needsMainOwner { forwarding.releaseOwner() }
        #endif
        updateScreenAwake()
        devices = devices.map { device in
            var device = device
            device.online = false
            return device
        }
        projectOffline()
        status = "已离线"
    }

    /// Without a session every other device is offline; this phone keeps its
    /// own card (a startup failure must not list it as an offline device).
    private func projectOffline() {
        guard var current = cards else { return }
        for index in current.cards.indices where !current.cards[index].isSelf {
            current.cards[index].connection = .offline
            current.cards[index].latencyMs = nil
            if current.cards[index].offlineReason == nil { current.cards[index].offlineReason = .disconnected }
        }
        cards = current
    }

    /// Only a running session has operations that auto-lock would end.
    private func updateScreenAwake() {
        let value = mobileSession != nil
            && ScreenAwake.required(shares: activeShares, downloads: activeDownloads, pairing: pairingSnapshot)
        if keepsScreenAwake != value { keepsScreenAwake = value }
    }

    /// The running owner, or the startup failure that prevents one.
    func requireLiveSession() throws -> MobileSession {
        guard let owner = mobileSession else { throw startupError ?? OinkFailure(message: "网络会话尚未启动。") }
        return owner
    }
    #endif

    /// Ends the session unconditionally (the share extension).
    func prepareSuspend() async {
        #if OINK_MOBILE_CORE_FFI
        await transitions.run { [weak self] in await self?.suspendSession() }
        #endif
    }

    /// Ends the session unless something holds it when the release runs: a
    /// visible scene, kept audio or a running background App Intent (review
    /// M29). The main app's lifecycle and the intents release through this.
    func releaseIfUnheld() async {
        #if OINK_MOBILE_CORE_FFI
        await transitions.releaseIfUnheld(keepsAudio: { [weak self] in self?.audioHoldsSession == true },
                                          release: { [weak self] in
            await self?.suspendSession(interruption: self?.audioInterrupted == true)
        })
        #endif
    }

    /// The audio is stopped by an OS interruption and resumes by itself when it ends.
    private var audioInterrupted: Bool {
        #if OINK_MAIN_APP
        audioOwner?.audio.state == .interrupted
        #else
        false
        #endif
    }

    private var audioHoldsSession: Bool {
        #if OINK_MAIN_APP
        keepsAudioBackgroundSession
        #else
        false
        #endif
    }

    /// Ends a `beginBackgroundOperation`; after the last one the session is
    /// released unless the scene or audio holds it.
    func finishBackgroundOperation() async {
        guard transitions.endOperation() else { return }
        await releaseIfUnheld()
    }
    func refreshDevices() async throws {
        #if OINK_MOBILE_CORE_FFI
        if let owner = mobileSession { try await refreshCoreSnapshots(owner) }
        else { await resume() }
        #else
        await resume()
        #endif
    }

    private func refreshLocalProjection() {
        let local = NativeDevice(id: "this-device", name: localDeviceName?.name ?? SystemDeviceName.current,
                                 online: true, computer: false)
        if devices.isEmpty || devices.first?.id == "this-device" { devices = [local] }
        cards = IOSDeviceCardsView(revision: cards?.revision ?? 0, cards: devices.map {
            IOSDeviceCard(device: $0.id, name: $0.name, connection: $0.id == local.id ? .thisDevice : ($0.online ? .online : .offline), isSelf: $0.id == local.id, isContentTargetPc: $0.computer)
        })
        nearby = nearby ?? IOSNearbySnapshot()
    }

    func leaveCurrentGroup() async throws {
        #if OINK_MOBILE_CORE_FFI
        // Design states show fixture devices; the session's real group is left alone.
        if DesignScene.current == nil, let owner = mobileSession { try await owner.leaveGroup() }
        #endif
        devices = []
        cards = IOSDeviceCardsView()
        transferTargets = nil
        try? SharedStore.save(devices, name: "devices.json", deviceOnly: true)
    }

    func queryAddress(_ address: String) async throws {
        let value = address.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !value.isEmpty else { throw OinkFailure(message: "请输入电脑地址。") }
        pairingQueryGeneration &+= 1
        let id = UUID().uuidString
        pairingQueryOperation = id
        manualAddProblem = nil
        foundPeer = nil
        #if OINK_MOBILE_CORE_FFI
        guard let owner = mobileSession else { throw OinkFailure(message: "网络会话尚未启动。") }
        let (host, port) = splitAddress(value)
        pairingQueryAddress = port == nil ? "\(host):47470" : value
        let reply = try await owner.queryPairingAddress(request: id, generation: pairingQueryGeneration, address: host, port: port)
        if case .problem(let rejection) = reply, case .problem(let problem) = rejection {
            let typed = IOSManualAddProblem(problem)
            manualAddProblem = typed
            throw OinkFailure(message: typed.text)
        }
        managementStatus = "正在查询电脑"
        #else
        foundPeer = IOSFoundPeer(queryId: id, generation: pairingQueryGeneration,
            peer: NearbyDevice(device: value, name: value, compatible: true, reachable: true))
        managementStatus = "已找到电脑，请确认连接"
        #endif
    }

    /// Starts a pairing and returns the core's key of the new session (`PairView.id`), the
    /// only reliable way to find it: the snapshot also lists ended sessions.
    func startPairing(_ target: IOSPairTarget) async throws -> String? {
        #if OINK_MOBILE_CORE_FFI
        guard let owner = mobileSession else { throw OinkFailure(message: "网络会话尚未启动。") }
        let request = UUID().uuidString
        let nativeTarget: PairTarget
        switch target {
        case .nearbyDevice(let id): nativeTarget = .nearbyDevice(device: id)
        case .preparedQuery(let id, let generation): nativeTarget = .preparedQuery(queryId: id, generation: generation)
        }
        let reply = try await owner.startPairing(request: request, target: nativeTarget)
        var started: String?
        switch reply {
        case .value(let key): managementStatus = "等待对方确认"; started = key
        case .problem(let rejection):
            if case .problem(let problem) = rejection { throw OinkFailure(message: IOSManualAddProblem(problem).text) }
        }
        try await refreshCoreSnapshots(owner)
        return started
        #else
        let device: NearbyDevice
        switch target {
        case .nearbyDevice(let id):
            device = nearby?.rows.first(where: { $0.device == id }) ?? NearbyDevice(device: id, name: id)
        case .preparedQuery(_, _):
            guard let foundPeer else { throw OinkFailure(message: "配对查询已过期，请重试。") }
            device = foundPeer.peer
        }
        if !devices.contains(where: { $0.id == device.device }) {
            devices.append(NativeDevice(id: device.device, name: device.name, online: device.reachable, computer: true))
            try? SharedStore.save(devices, name: "devices.json", deviceOnly: true)
        }
        refreshLocalProjection()
        managementStatus = "等待对方确认"
        return nil
        #endif
    }

    #if OINK_MOBILE_CORE_FFI
    func acceptPairing(_ request: PendingRequest) async throws {
        guard let owner = mobileSession else { throw OinkFailure(message: "网络会话尚未启动。") }
        try acceptPairingReply(await owner.acceptPairing(request: request.id, revision: request.revision))
        managementStatus = "已接受配对请求，等待配对码确认"
        try await refreshCoreSnapshots(owner)
    }

    func declinePairing(_ request: PendingRequest) async throws {
        guard let owner = mobileSession else { throw OinkFailure(message: "网络会话尚未启动。") }
        try acceptPairingReply(await owner.declinePairing(request: request.id, revision: request.revision))
        managementStatus = "已拒绝配对请求"
        try await refreshCoreSnapshots(owner)
    }

    func submitPairingCode(_ pair: PairView, code: String) async throws {
        let normalized = code.trimmingCharacters(in: .whitespacesAndNewlines)
        guard normalized.count == 6, normalized.allSatisfy({ $0.isNumber }) else {
            throw OinkFailure(message: "请输入六位配对码。")
        }
        guard let owner = mobileSession else { throw OinkFailure(message: "网络会话尚未启动。") }
        let attempt = UInt8(max(1, 4 - min(3, Int(pair.attemptsLeft))))
        try acceptPairingReply(await owner.submitPairingCode(request: pair.id, revision: pair.revision,
            attempt: attempt, code: normalized))
        managementStatus = "配对码已提交，正在完成入组"
        try await refreshCoreSnapshots(owner)
    }

    func declinePairing(_ pair: PairView) async throws {
        guard let owner = mobileSession else { throw OinkFailure(message: "网络会话尚未启动。") }
        try acceptPairingReply(await owner.declinePairing(request: pair.id, revision: pair.revision))
        managementStatus = "已拒绝配对"
        try await refreshCoreSnapshots(owner)
    }

    func cancelPairing(_ pair: PairView) async throws {
        guard let owner = mobileSession else { throw OinkFailure(message: "网络会话尚未启动。") }
        switch try await owner.cancelPairing(request: pair.id, revision: pair.revision) {
        case .value(let result):
            switch result {
            case .cancelled: managementStatus = "配对已取消"
            case .alreadyCommitted: managementStatus = "配对已完成，不能取消"
            case .commitUnconfirmed: managementStatus = "配对提交状态尚未确认，请刷新"
            }
        case .problem:
            throw OinkFailure(message: "配对状态已变化，请刷新后重试。")
        }
        try await refreshCoreSnapshots(owner)
    }

    func pairingCode(for pair: PairView) async throws -> String {
        guard let owner = mobileSession else { throw OinkFailure(message: "网络会话尚未启动。") }
        return try await owner.pairingCode(id: pair.id).sixDigits
    }

    /// Refresh button of the add page: asks Rust to re-probe nearby devices;
    /// the snapshot follows through `NearbyRefreshFinished`.
    func refreshNearby() async throws {
        let owner = try requireLiveSession()
        try acceptPairingReply(await owner.refreshNearby(request: UUID().uuidString))
    }

    private func acceptPairingReply(_ reply: PairAcceptedReply) throws {
        if case .problem(let rejection) = reply, case .problem(let problem) = rejection {
            throw OinkFailure(message: problem.detail)
        }
    }
    #endif

    func prepareShare() async throws {
        #if OINK_MOBILE_CORE_FFI
        do {
            _ = try await requireSession()
        } catch MobileError.OwnerBusy {
            guard case .devices(let current) = try await LocalHandoff.shared.request(.devices) else {
                throw OinkFailure(message: "设备列表不可用。")
            }
            devices = current
        }
        #endif
    }

    /// Who the transfer page and the share picker can send to: the core's
    /// `TransferTargetsView`, pushed as `MobileEvent.transferTargets`.
    @Published private(set) var transferTargets: SendTargets?

    /// Share picker, a cached device tapped: true once the core reached it, false when
    /// this attempt ended without it (the core then drops it from the cached list).
    func confirmOnline(_ device: String) async -> Bool {
        #if OINK_MOBILE_CORE_FFI
        if let owner = mobileSession { return (try? await owner.transferConfirmOnline(device: device)) ?? false }
        if case .valid(let online)? = try? await LocalHandoff.shared.request(.confirmOnline(device)) { return online }
        #endif
        return false
    }

    /// The share picker's first view: the devices online last time any session of this
    /// iPhone saw some (the core's cache in the App Group; readable without a session).
    func lastOnlineTargets() -> [SendTarget] {
        #if OINK_MOBILE_CORE_FFI
        guard let root = try? SharedStore.root() else { return [] }
        return cachedTransferTargets(dataDir: root.path).map(Self.sendTarget)
        #else
        return []
        #endif
    }

    /// The targets of the app's session when the app owns it (share picker); nil when
    /// this process runs its own session, whose list arrives in `transferTargets`.
    func handoffTransferTargets() async -> SendTargets? {
        #if OINK_MOBILE_CORE_FFI
        guard mobileSession == nil, case .targets(let targets)? = try? await LocalHandoff.shared.request(.targets) else { return nil }
        return targets
        #else
        return nil
        #endif
    }

    #if OINK_MOBILE_CORE_FFI
    private static func sendTarget(_ value: TransferTarget) -> SendTarget {
        SendTarget(id: value.device, name: value.name, isComputer: value.kind == .computer, os: osName(value.os),
                   privileged: value.androidPrivileged)
    }

    /// Events arrive in order on the one listener (`subscribe` pushes the current
    /// list first), so the latest event is the current list.
    private func apply(_ view: TransferTargetsView) {
        let only = view.onlyDevice.map {
            SendTarget(id: $0.device, name: $0.name, isComputer: $0.kind == .computer, os: Self.osName($0.os), online: $0.online)
        }
        let next = SendTargets(activeDevice: view.activeDevice, revision: view.revision, online: view.online.map(Self.sendTarget),
                               otherDevices: Int(view.otherDevices), onlyDevice: only, searching: view.searching)
        if next != transferTargets { transferTargets = next }
    }
    #endif

    /// Long-press「取消发送」on an own message in flight (to a computer or a phone):
    /// cancels its whole share; the core then removes the records on both sides.
    func cancelTransferSend(_ message: TransferMessage) throws {
        #if OINK_MOBILE_CORE_FFI
        guard let hex = message.operationID, let operation = Self.dataFromHex(hex) else {
            throw OinkFailure(message: AppText.pick("消息编号无效。", "Invalid message."))
        }
        try requireLiveSession().transferCancelShare(operationId: operation)
        #endif
    }

    /// Long-press「取消接收」on a phone-to-phone receive in flight; both sides show 已取消.
    func cancelTransferReceive(_ message: TransferMessage) throws {
        #if OINK_MOBILE_CORE_FFI
        guard let id = message.recordID else { throw OinkFailure(message: AppText.pick("消息编号无效。", "Invalid message.")) }
        try requireLiveSession().transferCancelReceive(messageId: id)
        #endif
    }

    /// Transfer page proxy selection (nil = automatic). Stored in the App
    /// Group so the share extension sends through the same proxy.
    func setTransferProxy(_ device: String?) {
        transferProxyID = device
        if let device { SharedStore.defaults.set(device, forKey: Self.transferProxyKey) }
        else { SharedStore.defaults.removeObject(forKey: Self.transferProxyKey) }
    }
    private static let transferProxyKey = "transferProxy"

    /// Sends text and/or files. Text and small PNGs are broadcast by the core
    /// to every online device whatever the proxy; everything else goes
    /// through `target` (nil = the transfer page proxy). When the chosen
    /// proxy is offline the core picks another one before sending (D17).
    /// Throws with the localized failure when the core reports Failed.
    /// `waitForTarget`: the group's only other device, shared to without a picker (user
    /// 2026-11-11). While it is offline the core records nothing and waits for it
    /// (`sharesWaitingForTarget`), then starts the share by itself; cancelling meanwhile
    /// ends it unrecorded.
    func share(_ payload: SharePayload, target: String? = nil, origin: String = "ios-share",
               waitForTarget: Bool = false) async throws -> [DeliveryResult] {
        #if OINK_MOBILE_CORE_FFI
        let proxy = target ?? SharedStore.defaults.string(forKey: Self.transferProxyKey)
        do {
            let owner = try await requireSession()
            return try await shareOwned(payload, target: proxy, origin: origin, waitForTarget: waitForTarget, owner: owner)
        } catch MobileError.OwnerBusy {
            guard case .deliveries(let results) = try await LocalHandoff.shared.request(
                .share(payload, target: proxy, waitForTarget: waitForTarget)) else {
                throw OinkFailure(message: "分享结果无效。")
            }
            return results
        }
        #else
        throw OinkFailure(message: "传输核心尚未生成。")
        #endif
    }

    #if OINK_MOBILE_CORE_FFI
    private func requireSession() async throws -> MobileSession {
        // A queued start or release finishes first: the owner checked below
        // is never one that is being shut down.
        await transitions.settle()
        if mobileSession == nil { await resume() }
        guard let owner = mobileSession else { throw startupError ?? OinkFailure(message: "网络会话尚未启动。") }
        return owner
    }

    #if OINK_MAIN_APP
    /// Control Center / Live Activity intents: the app's running session, or
    /// one started for them (in the background too). The share extension
    /// owning the connection is reported, not waited for.
    func requireAudioSession() async throws {
        do {
            _ = try await requireSession()
        } catch MobileError.OwnerBusy {
            throw OinkFailure(message: AppText.pick("Oink 的分享页正在使用连接，请稍后再试。",
                                                    "Oink's share sheet is using the connection. Try again later."))
        }
        guard sessionProfile == .mobile, audioOwner != nil else {
            throw OinkFailure(message: "音频会话尚未就绪。")
        }
    }
    #endif

    private func shareOwned(_ payload: SharePayload, target: String?, origin: String, waitForTarget: Bool = false,
                            owner: MobileSession) async throws -> [DeliveryResult] {
        // Directories go as folders (review M30).
        let items = ShareItems.items(for: payload)
        // A phone target is never replaced by another device: when it is offline the
        // share ends NoTarget, read as「对方不在线」rather than as no computer.
        let toPhone = target.map { id in cards?.cards.first { $0.device == id }.map { !$0.isContentTargetPc } ?? false } ?? false
        func failureText(_ failure: MobileDeliveryFailure?) -> String {
            (toPhone ? TransferFailureText.direct(failure, sending: true) : nil) ?? Self.deliveryFailureText(failure)
        }
        let operation = Data(randomID()), message = Data(randomID())
        let operationID = Self.hex(operation)
        try Task.checkCancellation()
        activeShareOperationID = operationID
        activeShares.insert(operationID)
        defer {
            activeShares.remove(operationID)
            cancelledShares.remove(operationID)
            sharesWaitingForTarget.remove(operationID)
        }
        // Cancelling the calling task (the share card's 取消, a handed-over
        // request cancelled by the extension, the handoff stopping) cancels
        // the share in the core, also while it is still being copied or
        // waits for its first target. The core then ends it with its single
        // DeliveryEvent, which settles the wait below (review M31).
        return try await withTaskCancellationHandler {
            // Copying and packaging run in Rust off this actor. Right after the
            // session started the share waits there for its first online target.
            // An error means nothing was recorded; otherwise the outcome is the
            // share's single DeliveryEvent, pushed to the listener.
            do {
                // With waitForTarget this returns once the target was online and the share recorded.
                try await owner.transferSendShare(operationId: operation, messageId: message, items: items,
                    origin: origin, targetId: target, waitForTarget: waitForTarget, sensitivity: payload.sensitivity == .concealed ? .concealed : payload.sensitivity == .transient ? .transient : payload.sensitivity == .autoGenerated ? .autoGenerated : nil)
                sharesWaitingForTarget.remove(operationID)
            } catch MobileError.Transfer(let reason, _) {
                if cancelledShares.contains(operationID) { throw CancellationError() }
                throw OinkFailure(message: toPhone && reason == .noTarget ? failureText(.noTarget) : TransferFailureText.text(reason))
            } catch is MobileError {
                if cancelledShares.contains(operationID) { throw CancellationError() }
                // Other core details are diagnostic English, not for the card (M30).
                throw OinkFailure(message: Self.deliveryFailureText(nil))
            }
            let outcome = await waitForDelivery(operationID: operationID)
            if outcome.state != .sent && cancelledShares.contains(operationID) { throw CancellationError() }
            guard outcome.state == .sent else { throw OinkFailure(message: failureText(outcome.failure)) }
            return [DeliveryResult(id: operationID, message: "Complete")]
        } onCancel: {
            Task { @MainActor [weak self, weak owner] in
                guard let self, let owner, self.activeShares.contains(operationID) else { return }
                self.cancelledShares.insert(operationID)
                try? owner.transferCancelShare(operationId: operation)
            }
        }
    }

    static func deliveryFailureText(_ failure: MobileDeliveryFailure?) -> String {
        switch failure {
        case .noTarget: return "没有在线的电脑，未发送。"
        case .notAccepted: return "对方没有响应，发送失败。"
        case .rejected: return "对方拒收了此内容。"
        case .peerFailed: return "对方无法写入此内容。"
        case .interrupted: return "连接中断，发送失败。"
        case .local: return "无法读取要发送的内容。"
        case .storageFull: return "手机存储空间不足，未发送。"
        case .cancelled: return "已取消。"
        case .paused: return "对方已暂停共享，未发送。"
        case .peerDisconnected: return "对方已断开，发送失败"
        case nil: return "发送失败。"
        }
    }

    private func handleHandoff(_ command: HandoffRequest.Command, owner: MobileSession) async throws -> HandoffResult {
        switch command {
        case .devices:
            return .devices(devices)
        case .targets:
            return .targets(transferTargets ?? SendTargets())
        case .confirmOnline(let device):
            return .valid(await confirmOnline(device))
        case .latestClip:
            return .latestClip(try await latestSharedClip(owner))
        case .share(let payload, let target, let waitForTarget):
            return .deliveries(try await shareOwned(payload, target: target, origin: "ios-share", waitForTarget: waitForTarget, owner: owner))
        default:
            throw OinkFailure(message: "无法执行此剪贴板操作。")
        }
    }
    #endif

    /// The group's latest shared text or image for the one-tap copy shortcut.
    func latestSharedClip() async throws -> LatestClip {
        #if OINK_MOBILE_CORE_FFI
        do {
            let owner = try await requireSession()
            return try await latestSharedClip(owner)
        } catch MobileError.OwnerBusy {
            guard case .latestClip(let clip) = try await LocalHandoff.shared.request(.latestClip) else {
                throw OinkFailure(message: AppText.pick("获取结果无效。", "Invalid result."))
            }
            return clip
        }
        #else
        throw OinkFailure(message: "传输核心尚未生成。")
        #endif
    }

    #if OINK_MOBILE_CORE_FFI
    /// Right after the session started this waits (in Rust, on link events)
    /// for the first PC; a typed failure keeps its own reason.
    private func latestSharedClip(_ owner: MobileSession) async throws -> LatestClip {
        let current: MobileCurrentClipboard?
        do {
            current = try await owner.receiveCurrentClipboard()
        } catch MobileError.Transfer(.noTarget, _) {
            throw LatestClipFailure.noComputer
        } catch MobileError.Transfer(let reason, _) {
            throw OinkFailure(message: TransferFailureText.text(reason))
        } catch is MobileError {
            throw LatestClipFailure.interrupted
        }
        guard mobileSession === owner else { throw CancellationError() }
        return try LatestClip.from(current)
    }
    #endif

    func randomID() -> [UInt8] {
        var data = Data(count: 16)
        _ = data.withUnsafeMutableBytes { SecRandomCopyBytes(kSecRandomDefault, 16, $0.baseAddress!) }
        return Array(data)
    }

    #if OINK_MOBILE_CORE_FFI
    /// Records the single DeliveryEvent of an own share for its waiter.
    func settleDelivery(_ operationID: String, _ outcome: IOSDeliveryOutcome) {
        diagnosticsOwner?.emit(kind: "transfer", code: "transfer_finished")
        if let waiter = deliveryWaiters.removeValue(forKey: operationID) {
            waiter.resume(returning: outcome)
        } else {
            settledDeliveries[operationID] = outcome
        }
    }

    func noteTransferFailure() { diagnosticsOwner?.emit(kind: "error", code: "failed") }

    /// The core emits exactly one DeliveryEvent per share (failures and the
    /// 120 s accept window included), so there is no host timeout.
    private func waitForDelivery(operationID: String) async -> IOSDeliveryOutcome {
        if let outcome = settledDeliveries.removeValue(forKey: operationID) { return outcome }
        return await withCheckedContinuation { continuation in
            deliveryWaiters[operationID] = continuation
        }
    }

    static func hex(_ data: Data) -> String {
        data.map { String(format: "%02x", $0) }.joined()
    }

    static func dataFromHex(_ value: String) -> Data? {
        guard value.count.isMultiple(of: 2) else { return nil }
        var data = Data(capacity: value.count / 2)
        var index = value.startIndex
        while index < value.endIndex {
            let next = value.index(index, offsetBy: 2)
            guard let byte = UInt8(value[index..<next], radix: 16) else { return nil }
            data.append(byte)
            index = next
        }
        return data
    }
    #endif

    #if OINK_MOBILE_CORE_FFI
    /// Facts from the native owners. They never change the negotiated
    /// capabilities or drop a link (protocol-v2 1.1): the core turns the audio
    /// facts into the endpoint states' `supported`.
    private func nativeCapabilities() -> MobilePlatformFacts {
        #if OINK_MAIN_APP
        if sessionProfile == .mobile {
            let facts = audioOwner?.capabilityFacts() ?? (microphoneSend: false, speakerReceive: false)
            return MobilePlatformFacts(microphoneSend: facts.microphoneSend,
                speakerReceive: facts.speakerReceive,
                speakerSend: forwarding.state != .stopped,
                privilegedInputReady: false)
        }
        #endif
        return MobilePlatformFacts(microphoneSend: false, speakerReceive: false, speakerSend: false, privilegedInputReady: false)
    }

    private func updateNativeCapabilities(_ owner: MobileSession? = nil) {
        let owner = owner ?? mobileSession
        guard let owner else { return }
        owner.updatePlatformFacts(facts: nativeCapabilities())
    }

    private func startDiagnostics(_ owner: MobileSession) {
        #if OINK_MAIN_APP
        guard sessionProfile == .mobile, diagnosticsAvailable else { return }
        do {
            try diagnosticsOwner?.start(session: owner)
        } catch {
            diagnosticsAvailable = false
            updateNativeCapabilities(owner)
        }
        #endif
    }

    private func publishAdvertisement(_ owner: MobileSession) async throws {
        let advertisement = try await owner.advertisement()
        guard mobileSession === owner else { return }
        try publisher.publish(instance: advertisement.instance, type: advertisement.serviceType,
            port: advertisement.port, txt: advertisement.txt)
        localIdentity.port = advertisement.port
    }

    func refreshCoreSnapshots(_ owner: MobileSession) async throws {
        let cards = try await owner.deviceCards()
        let nearby = try await owner.nearby()
        let pairings = try await owner.pairings()
        let blocked = try await owner.blockedDevices()
        let compatibility = try await owner.negotiationFailures()
        apply(cards)
        apply(nearby)
        self.pairings = IOSPairingSnapshot(rows: pairings.requests.map(\.id) + pairings.sessions.map(\.id))
        self.pairingSnapshot = pairings
        apply(blocked)
        self.compatibility = compatibility.map {
            IOSPeerCompatibility(device: $0.device, problem: $0.problem.map { String(describing: $0) })
        }
        localIdentity = IOSLocalIdentity(addresses: Self.interfaceAddresses(), port: localIdentity.port,
                                         fingerprint: owner.deviceFingerprint())
        applyLocalDeviceName(owner.localDeviceName())
    }

    private func apply(_ snapshot: NearbySnapshot) {
        nearby = IOSNearbySnapshot(rows: snapshot.rows.map {
            NearbyDevice(device: $0.device, name: $0.name, compatible: $0.compatible, reachable: $0.reachable,
                         os: $0.os, address: Self.display($0.selected),
                         groupCount: $0.count.map(Int.init).flatMap { $0 > 1 ? $0 : nil },
                         fingerprint: (try? deviceFingerprint(device: $0.device)) ?? "")
        })
    }

    private func apply(_ snapshot: DeviceCardsView) {
        if let current = cards, current.stream == snapshot.stream, current.revision > snapshot.revision { return }
        let previous = Set(devices.filter { $0.online }.map(\.id))
        cards = IOSDeviceCardsView(stream: snapshot.stream, revision: snapshot.revision,
                                   cards: snapshot.cards.map { Self.card($0, exit: snapshot.exitVolume) })
        exitVolume = snapshot.exitVolume
        devices = snapshot.cards.map {
            NativeDevice(id: $0.device, name: $0.name, online: isOnline($0.connection), computer: $0.isContentTargetPc)
        }
        if let local = snapshot.cards.first(where: \.isSelf), let audio = local.mobileAudio {
            localAudio.speaker = audio.speaker
            localAudio.microphone = audio.microphone
        }
        if devices.isEmpty { refreshLocalProjection() }
        let online = Set(devices.filter { $0.online }.map(\.id))
        if let owner = mobileSession {
            let local = owner.deviceId()
            for peer in online.subtracting(previous) where peer != local {
                diagnosticsOwner?.emit(kind: "link", code: "connected")
            }
            for peer in previous.subtracting(online) where peer != local {
                diagnosticsOwner?.emit(kind: "link", code: "disconnected")
            }
        }
    }

    private static func card(_ value: DeviceCard, exit: ExitVolumeCard?) -> IOSDeviceCard {
        var offline: IOSOfflineReason?
        if case .offline(let reason, _) = value.connection {
            switch reason {
            case .disconnected: offline = .disconnected
            case .connecting: offline = .connecting
            case .addressStale: offline = .addressStale
            case .protocolIncompatible: offline = .protocolIncompatible
            }
        }
        return IOSDeviceCard(device: value.device, name: value.name, connection: connection(value.connection),
            isSelf: value.isSelf, isContentTargetPc: value.isContentTargetPc, os: osName(value.os),
            fingerprint: value.fingerprint, latencyMs: latency(value.connection), lastSeenMs: value.lastSeenMs,
            offlineReason: offline, androidPrivileged: value.androidPrivileged, controls: controls(value, exit: exit), source: value)
    }

    /// Latency-only updates (RTT changed by ≥ 1 ms) between full card snapshots.
    private func apply(_ update: DeviceCardSignals) {
        guard let current = cards, current.stream == update.stream, current.revision <= update.revision else { return }
        latency.update(update.signals.map { (device: $0.device, ms: $0.rttMs.map { max(0, Int($0.rounded())) }) })
    }

    private func apply(_ snapshot: BlockedDevicesSnapshot) {
        blocked = IOSBlockedDevicesSnapshot(devices: snapshot.entries.map(\.device))
        blockedDevices = snapshot.entries
    }

    private func apply(_ event: MobileEvent) {
        switch event {
        case .deviceCards(let snapshot): apply(snapshot)
        case .deviceCardSignals(let signals): apply(signals)
        case .blockedDevices(let snapshot): apply(snapshot)
        case .peerCompatibilityChanged(let status):
            compatibility = [IOSPeerCompatibility(device: status.device, problem: status.problem.map { String(describing: $0) })]
        case .pairing(let event): apply(event)
        // Clipboard metadata is consumed inside Rust (startup current query,
        // fresh-copy observer); the host keeps no copy of shared text.
        case .clipboard: break
        case .transferMessage, .transferAnnouncement, .transferDownloadProgress,
             .transferRateSample, .transferSendProgress, .deliveryEvent, .transferError,
             .transferDirectoryPage, .transferDirectoryFailed, .transferRemoved, .transferDirectIncoming, .transferDirectEnded:
            applyTransferEvent(event)
        case .transferTargets(let targets): apply(targets)
        case .transferShareWaiting(let operationID, let waiting):
            let key = Self.hex(operationID)
            if waiting, activeShares.contains(key) { sharesWaitingForTarget.insert(key) } else { sharesWaitingForTarget.remove(key) }
        case .resyncRequired:
            Task { try? await refreshDevices() }
        case .advertisementChanged:
            if let owner = mobileSession {
                Task { try? await publishAdvertisement(owner) }
            }
        // Device control replies are returned by the awaited call; the focus
        // lock state arrives in the next DeviceCards snapshot.
        case .deviceControlCompleted, .focusChanged, .stopped:
            break
        case .networkFailed(let detail, let retryAfterMs):
            Task { await restartAfterNetworkFailure(detail: detail, afterMilliseconds: retryAfterMs) }
        }
    }

    /// The core's network layer failed (its QUIC endpoint stopped; user 2026-11-12): this
    /// session cannot recover. End it and start a new one through the normal start after the
    /// core's back-off (1 s doubling to 30 s, reset after a stable minute).
    /// The core has logged `detail` (os_log, ERROR) and recorded it in diagnostics.
    private func restartAfterNetworkFailure(detail: String, afterMilliseconds: UInt64) async {
        guard let failed = mobileSession else { return }
        await transitions.run { [weak self] in
            // Only the session that failed; a newer one is left alone.
            guard let self, self.mobileSession === failed else { return }
            await self.suspendSession()
        }
        status = "网络出错，正在重新连接…"
        try? await Task.sleep(nanoseconds: afterMilliseconds * 1_000_000)
        await resume()
    }

    private func apply(_ event: PairingEvent) {
        switch event {
        case .addressQueryFinished(let operationID, let generation, let result):
            guard operationID == pairingQueryOperation, generation == pairingQueryGeneration else { return }
            switch result {
            case .found(let peer):
                manualAddProblem = nil
                foundPeer = IOSFoundPeer(queryId: peer.queryId, generation: peer.generation,
                    peer: NearbyDevice(device: peer.peer.device, name: peer.peer.name, compatible: true, reachable: true,
                                       os: peer.peer.os, address: pairingQueryAddress,
                                       fingerprint: (try? deviceFingerprint(device: peer.peer.device)) ?? ""))
                managementStatus = "已找到电脑，请确认连接"
            case .problem(let problem):
                foundPeer = nil
                let typed = IOSManualAddProblem(problem)
                manualAddProblem = typed
                managementStatus = typed.text
            }
        case .pairingFailed(_, let problem):
            managementStatus = "配对失败：\(Self.pairProblemText(problem))"
            Task { try? await refreshDevices() }
        case .pairingCompleted(let eventID, let peer):
            pairingCompleted = IOSPairingCompleted(id: eventID, peerName: peer.name, peerDevice: peer.device)
            managementStatus = "已与 \(peer.name) 配对"
            if let owner = mobileSession {
                Task { try? await owner.ackPairingEvent(eventId: eventID) }
            }
            Task { try? await refreshDevices() }
        case .peerCompatibilityChanged(let status):
            compatibility = [IOSPeerCompatibility(device: status.device,
                problem: status.problem.map { String(describing: $0) })]
        case .membershipEdited, .nearbyRefreshFinished:
            Task { try? await refreshDevices() }
        }
    }

    /// Pairing failure reason as the PC shows it (`pairProblemText` in
    /// apps/oink/src/pairing/model.ts): the typed network problem first, then
    /// the PairCode. `detail` is diagnostic text and is never shown.
    nonisolated static func pairProblemText(_ problem: PairProblem) -> String {
        if let network = problem.network { return IOSManualAddProblem.text(network) }
        return pairCodeTexts[problem.code] ?? "配对状态未知"
    }

    nonisolated private static let pairCodeTexts: [UInt32: String] = [
        0: "请输入有效的 IPv4 地址", 1: "端口应在 1–65535 之间", 3: "不能与当前设备配对",
        4: "目标主机不可达", 5: "连接被拒绝", 6: "连接超时", 7: "该地址上没有 Oink 设备",
        8: "设备身份已变化，请重新搜索", 9: "对方的协议版本不兼容", 10: "对方不支持所需功能",
        11: "设备正忙，请稍后重试", 12: "操作过于频繁，请稍后重试", 13: "对方拒绝了配对",
        14: "对方取消了配对", 15: "已取消配对", 16: "对方关闭了配对", 17: "连接已断开",
        18: "请输入六位配对码", 19: "配对码不正确，请重新输入", 20: "尝试次数已用完",
        21: "此配对已结束", 22: "设备已经连接", 23: "已由另一配对会话接替", 24: "设备组已变化",
        25: "设备数量已达上限", 26: "设备组数据超过限制", 27: "无法验证设备组成员", 28: "设备组合并冲突",
        30: "无法保存配对结果", 31: "无法确认提交结果", 32: "无法获取合并证明", 33: "设备成员资格已撤销",
        34: "设备组已变化，无法撤销", 35: "不允许此操作", 36: "搜索结果的连接已断开，请重新搜索",
        37: "配对状态已变化，请重试", 38: "配对协议异常", 39: "设备状态已变化，请重新搜索", 40: "配对已经提交",
        41: "无法验证设备身份", 42: "没有共同支持的加密方式", 43: "不支持该屏幕排列", 44: "配对服务不可用",
        45: "网络操作超时", 46: "对方还有没处理的配对请求，请稍后重试",
    ]

    private static func display(_ address: SocketAddress) -> String {
        address.address.contains(":") ? "[\(address.address)]:\(address.port)" : "\(address.address):\(address.port)"
    }

    private static func osName(_ os: CardOs) -> String {
        switch os { case .android: "android"; case .ios: "ios"; case .macos: "macos"; case .windows: "windows"; case .linux: "linux"; case .other: "other" }
    }

    private static func latency(_ value: CardConnection) -> Int? {
        guard case .online(let signal) = value, case .measured(let rtt) = signal else { return nil }
        return max(0, Int(rtt.rounded()))
    }

    private static func connection(_ value: CardConnection) -> DeviceConnection {
        switch value { case .thisDevice: return .thisDevice; case .online: return .online; case .offline: return .offline }
    }

    private func isOnline(_ value: CardConnection) -> Bool {
        switch value { case .thisDevice, .online: return true; case .offline: return false }
    }

    private func splitAddress(_ value: String) -> (String, UInt16?) {
        if value.hasPrefix("["), let end = value.firstIndex(of: "]") {
            let host = String(value[value.index(after: value.startIndex)..<end])
            let suffix = value[value.index(after: end)...]
            return (host, UInt16(suffix.drop(while: { $0 == ":" })))
        }
        let parts = value.split(separator: ":", omittingEmptySubsequences: false)
        if parts.count == 2, let port = UInt16(parts[1]) { return (String(parts[0]), port) }
        return (value, nil)
    }

    /// The phone's own addresses: IPv4 first, then global IPv6; loopback and link-local skipped.
    static func interfaceAddresses() -> [String] {
        var v4: [String] = [], v6: [String] = []
        var head: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&head) == 0, let first = head else { return [] }
        defer { freeifaddrs(head) }
        for pointer in sequence(first: first, next: { $0.pointee.ifa_next }) {
            let entry = pointer.pointee
            guard let address = entry.ifa_addr, (entry.ifa_flags & UInt32(IFF_UP)) != 0,
                  (entry.ifa_flags & UInt32(IFF_LOOPBACK)) == 0 else { continue }
            let family = Int32(address.pointee.sa_family)
            guard family == AF_INET || family == AF_INET6 else { continue }
            var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            let length = socklen_t(family == AF_INET ? MemoryLayout<sockaddr_in>.size : MemoryLayout<sockaddr_in6>.size)
            guard getnameinfo(address, length, &host, socklen_t(host.count), nil, 0, NI_NUMERICHOST) == 0 else { continue }
            let text = String(cString: host)
            if family == AF_INET { if !v4.contains(text) { v4.append(text) } }
            else if !text.lowercased().hasPrefix("fe80"), !text.contains("%"), !v6.contains(text) { v6.append(text) }
        }
        return v4 + v6
    }
    #endif
}
