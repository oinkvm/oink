import ImageIO
import UIKit
import UniformTypeIdentifiers
import XCTest
@testable import OinkAudioTestHost

/// Host decisions of the 0.4.0 review fixes (iOS host block), plus the core
/// paths they depend on, run through the real UniFFI session.
final class HostPolicyTests: XCTestCase {
    /// group-state.v2 of the identity whose seed is bytes 0...31 (the audio
    /// lifecycle fixture).
    private static let fixture = "owFQM2twUP3+RC70Hfl/+1ddhwKCowECA6IBAQOiAWdlZDI1NTE5AlggA6EHv/POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbgEowGAAgAEoKMBGRMBA6IBAQOiAWdlZDI1NTE5AlggA6EHv/POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbgEoQGBowGiAWdlZDI1NTE5AlggA6EHv/POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbgCAAMAA6IBUDNrcFD9/kQu9B35f/tXXYcDgaEBqAEAAlAza3BQ/f5ELvQd+X/7V12HA6IBZ2VkMjU1MTkCWCADoQe/884Qvh1w3RjnS8CZZ+TWMJulDV8d3IZkElUxuAQABaIBZ2VkMjU1MTkCWCADoQe/884Qvh1w3RjnS8CZZ+TWMJulDV8d3IZkElUxuAduaU9TIGF1ZGlvIHRlc3QIY2lvcwkB"
    private static let fixtureSeed = Data((0..<32).map { UInt8($0) })

    private func dataDirectory() throws -> URL {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        try XCTUnwrap(Data(base64Encoded: Self.fixture))
            .write(to: DeviceOnlyStorage.prepare(in: root).appendingPathComponent("group-state.v2"))
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        return root
    }

    private func config(_ root: URL) -> MobileConfig {
        MobileConfig(dataDir: root.path, cacheDir: root.appendingPathComponent("Cache").path,
                     logDir: root.appendingPathComponent("Logs").path, deviceName: "Host policy test",
                     platform: .ios, profile: .mobile, formFactor: nil)
    }

    // MARK: M20 (D8)

    /// The App Group group state came from the old iPhone (fixture identity)
    /// while this phone has another Keychain identity: provisioning replaces
    /// it with a group of this phone, so the session starts and the app can
    /// ask for pairing again.
    func testRestoredGroupOfAnotherIdentityIsReplacedForThisPhone() async throws {
        let root = try dataDirectory()
        let newPhone = Data(repeating: 9, count: 32)
        XCTAssertEqual(try initializeMobileGroup(config: config(root), identitySeed: newPhone), .replaced)
        let session = try await MobileSession.start(config: config(root), discovery: MobileDiscovery(), identitySeed: newPhone)
        try await session.shutdown()
        XCTAssertEqual(try initializeMobileGroup(config: config(root), identitySeed: newPhone), .existing)
        // The phone the state belongs to keeps it.
        let own = try dataDirectory()
        XCTAssertEqual(try initializeMobileGroup(config: config(own), identitySeed: Self.fixtureSeed), .existing)
    }

    // MARK: M30

    /// Every sent picture carries the size the user sees (file-bubble core item 1): the
    /// header's pixels, turned for EXIF orientations 5–8; both or neither.
    func testSharedPicturesCarryTheirShownSize() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        func jpeg(_ name: String, orientation: Int) throws -> URL {
            let url = root.appendingPathComponent(name)
            let format = UIGraphicsImageRendererFormat()
            format.scale = 1
            let image = UIGraphicsImageRenderer(size: CGSize(width: 400, height: 300), format: format).image { context in
                UIColor.systemPink.setFill()
                context.fill(CGRect(x: 0, y: 0, width: 400, height: 300))
            }
            let destination = try XCTUnwrap(CGImageDestinationCreateWithURL(url as CFURL, UTType.jpeg.identifier as CFString, 1, nil))
            CGImageDestinationAddImage(destination, try XCTUnwrap(image.cgImage),
                                       [kCGImagePropertyOrientation: orientation] as CFDictionary)
            XCTAssertTrue(CGImageDestinationFinalize(destination))
            return url
        }
        let upright = try jpeg("upright.jpg", orientation: 1)
        // A phone camera's portrait: stored 400 × 300, shown turned (orientation 6).
        let turned = try jpeg("turned.jpg", orientation: 6)
        let broken = root.appendingPathComponent("broken.png")
        try Data("not a picture".utf8).write(to: broken)
        let items = ShareItems.items(for: SharePayload(text: nil, files: [upright, turned, broken]))
        XCTAssertEqual(items.count, 3)
        guard case .image(_, _, _, _, let w1, let h1) = items[0], case .image(_, _, _, _, let w2, let h2) = items[1],
              case .image(_, _, _, _, let w3, let h3) = items[2] else { return XCTFail("\(items)") }
        XCTAssertEqual([w1, h1], [400, 300])
        XCTAssertEqual([w2, h2], [300, 400])
        XCTAssertNil(w3)
        XCTAssertNil(h3)
    }

    func testDirectoriesAreSharedAsFolders() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let folder = root.appendingPathComponent("Photos", isDirectory: true)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        try Data("a".utf8).write(to: folder.appendingPathComponent("a.txt"))
        let file = root.appendingPathComponent("notes.txt")
        try Data("notes".utf8).write(to: file)
        defer { try? FileManager.default.removeItem(at: root) }
        let items = ShareItems.items(for: SharePayload(text: nil, files: [folder, file]))
        XCTAssertEqual(items.count, 2)
        guard case .folder(let path, let name) = items[0] else { return XCTFail("\(items[0])") }
        XCTAssertEqual(path, folder.path)
        XCTAssertEqual(name, "Photos")
        guard case .file(_, let fileName, let size, _, let staged) = items[1] else { return XCTFail("\(items[1])") }
        XCTAssertEqual(fileName, "notes.txt")
        XCTAssertEqual(size, 5)
        XCTAssertFalse(staged, "a picked file is copied, not taken over")
        let handed = ShareItems.items(for: SharePayload(text: nil, files: [file], staged: true))
        guard case .file(_, _, _, _, true) = handed[0] else { return XCTFail("\(handed[0])") }
    }

    // MARK: L15

    func testAWrongCodeIsRecognisedOnlyFromTheVerdictOfThisAttempt() {
        func view(_ attempts: UInt8, _ feedback: Feedback, terminal: PairProblem? = nil) -> PairView {
            PairView(id: "p", revision: 1, direction: .incoming, peer: PairEndpoint(device: "d", name: "PC", os: "macos"),
                     phase: 1, attemptsLeft: attempts, feedback: feedback, terminal: terminal)
        }
        // First attempt: submitting returns while the core still waits.
        XCTAssertEqual(PairingCodeCheck.outcome(submitted: view(3, .waiting), current: view(2, .waiting)), .pending)
        XCTAssertEqual(PairingCodeCheck.outcome(submitted: view(3, .waiting), current: view(2, .wrong)), .wrong)
        // Second attempt: a snapshot from before the submission still shows
        // the previous verdict; it is not this attempt's.
        XCTAssertEqual(PairingCodeCheck.outcome(submitted: view(2, .wrong), current: view(2, .wrong)), .pending)
        XCTAssertEqual(PairingCodeCheck.outcome(submitted: view(2, .wrong), current: view(1, .wrong)), .wrong)
        XCTAssertEqual(PairingCodeCheck.outcome(submitted: view(2, .wrong), current: view(1, .correct)), .settled)
        XCTAssertEqual(PairingCodeCheck.outcome(submitted: view(1, .wrong), current: nil), .settled)
    }

    // MARK: L17

    func testTheTextFieldClearsOnlyAfterASuccessfulSend() {
        XCTAssertEqual(ComposerDraft.after(sending: "hi", current: "hi", succeeded: true), "")
        XCTAssertEqual(ComposerDraft.after(sending: "hi", current: "hi", succeeded: false), "hi")
        XCTAssertEqual(ComposerDraft.after(sending: "hi", current: "hi there", succeeded: true), "hi there")
    }

    func testOnlyStagingOfEndedExtensionsIsRemoved() throws {
        let parent = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: parent, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: parent) }
        let live = try ShareStagingLease(in: parent)
        // An ended extension: its lock file exists but nobody holds it.
        let ended = parent.appendingPathComponent("ended", isDirectory: true)
        try FileManager.default.createDirectory(at: ended, withIntermediateDirectories: true)
        FileManager.default.createFile(atPath: ended.appendingPathComponent(".owner").path, contents: nil)
        // Left by a build that did not lock its staging.
        let legacy = parent.appendingPathComponent("legacy", isDirectory: true)
        try FileManager.default.createDirectory(at: legacy, withIntermediateDirectories: true)

        ShareStagingLease.removeAbandoned(in: parent)
        XCTAssertTrue(FileManager.default.fileExists(atPath: live.directory.path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: ended.path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: legacy.path))
        live.remove()
        XCTAssertFalse(FileManager.default.fileExists(atPath: live.directory.path))
    }

    func testListImagesAreDownsampledAndCached() throws {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent("\(UUID().uuidString).png")
        defer { try? FileManager.default.removeItem(at: url) }
        let format = UIGraphicsImageRendererFormat()
        format.scale = 1
        let image = UIGraphicsImageRenderer(size: CGSize(width: 4000, height: 3000), format: format).image { context in
            UIColor.systemPink.setFill()
            context.fill(CGRect(x: 0, y: 0, width: 4000, height: 3000))
        }
        try XCTUnwrap(image.pngData()).write(to: url)
        let thumbnail = try XCTUnwrap(ImageThumbnails.image(path: url.path, maxPixel: 1200))
        XCTAssertEqual(max(thumbnail.size.width * thumbnail.scale, thumbnail.size.height * thumbnail.scale), 1200)
        XCTAssertTrue(ImageThumbnails.image(path: url.path, maxPixel: 1200) === thumbnail)
    }

    /// The list lays pictures out from the header and decodes them off the main thread.
    @MainActor
    func testListPicturesSizeFromHeaderAndDecodeOffMain() async throws {
        let url = FileManager.default.temporaryDirectory.appendingPathComponent("\(UUID().uuidString).jpg")
        defer { try? FileManager.default.removeItem(at: url) }
        let format = UIGraphicsImageRendererFormat()
        format.scale = 1
        let image = UIGraphicsImageRenderer(size: CGSize(width: 3000, height: 4000), format: format).image { context in
            UIColor.systemTeal.setFill()
            context.fill(CGRect(x: 0, y: 0, width: 3000, height: 4000))
        }
        try XCTUnwrap(image.jpegData(compressionQuality: 0.8)).write(to: url)
        XCTAssertEqual(ImageThumbnails.pixelSize(path: url.path), CGSize(width: 3000, height: 4000))
        XCTAssertNil(ImageThumbnails.cached(path: url.path, maxPixel: 1200))
        // Two callers during one decode share it.
        async let first = ImageThumbnails.load(path: url.path, maxPixel: 1200)
        async let second = ImageThumbnails.load(path: url.path, maxPixel: 1200)
        let (a, b) = await (first, second)
        let thumbnail = try XCTUnwrap(a)
        XCTAssertTrue(thumbnail === b)
        XCTAssertEqual(thumbnail.size.height * thumbnail.scale, 1200)
        XCTAssertTrue(ImageThumbnails.cached(path: url.path, maxPixel: 1200) === thumbnail)
        XCTAssertNil(ImageThumbnails.pixelSize(path: "/nonexistent.jpg"))
    }

    // MARK: M27

    /// The share extension owns the session: the app's start fails with
    /// OwnerBusy, which is shown as its own reason (and retried when the
    /// extension releases it), not as an empty group.
    func testAnOwnedSessionIsReportedAsBusy() async throws {
        let root = try dataDirectory()
        let extensionSession = try await MobileSession.start(config: config(root), discovery: MobileDiscovery(),
                                                             identitySeed: Self.fixtureSeed)
        do {
            _ = try await MobileSession.start(config: config(root), discovery: MobileDiscovery(), identitySeed: Self.fixtureSeed)
            XCTFail("a second owner started")
        } catch {
            XCTAssertEqual(IOSSessionProblem(error), .busy)
        }
        try await extensionSession.shutdown()
        // Released: the retry starts.
        let app = try await MobileSession.start(config: config(root), discovery: MobileDiscovery(), identitySeed: Self.fixtureSeed)
        try await app.shutdown()
    }

    func testStartupProblemsKeepTheirReason() {
        XCTAssertEqual(IOSSessionProblem(IdentityUnavailable(message: "身份读取失败（-25308）；请先解锁设备。")),
                       .identity("身份读取失败（-25308）；请先解锁设备。"))
        XCTAssertEqual(IOSSessionProblem(MobileError.Startup(detail: "group roster does not contain this device identity")),
                       .startup("group roster does not contain this device identity"))
        XCTAssertFalse(IOSSessionProblem.busy.message.isEmpty)
    }

    // MARK: M28

    func testTheScreenStaysAwakeOnlyWhileAnOperationRuns() {
        func pair(_ phase: UInt32, terminal: PairProblem? = nil) -> PairingSnapshot {
            PairingSnapshot(revision: 1, requests: [], sessions: [
                PairView(id: "p", revision: 1, direction: .outgoing, peer: PairEndpoint(device: "d", name: "PC", os: "macos"),
                         phase: phase, attemptsLeft: 3, feedback: .waiting, terminal: terminal)])
        }
        XCTAssertFalse(ScreenAwake.required(shares: [], downloads: [], pairing: nil))
        XCTAssertTrue(ScreenAwake.required(shares: ["op"], downloads: [], pairing: nil))
        XCTAssertTrue(ScreenAwake.required(shares: [], downloads: [7], pairing: nil))
        // Showing the code while the PC user types it.
        XCTAssertTrue(ScreenAwake.required(shares: [], downloads: [], pairing: pair(1)))
        XCTAssertFalse(ScreenAwake.required(shares: [], downloads: [], pairing: pair(7)))
        XCTAssertFalse(ScreenAwake.required(shares: [], downloads: [], pairing: pair(8)))
        XCTAssertFalse(ScreenAwake.required(shares: [], downloads: [], pairing:
            pair(2, terminal: PairProblem(code: 13, stage: 1, detail: "", network: nil))))
    }

    // MARK: M29

    @MainActor
    func testSessionTransitionsDecideTheReleaseWhenItRuns() async {
        let transitions = SessionTransitions()
        let log = TransitionLog()
        // Two overlapping operations: only the last end counts.
        transitions.beginOperation()
        transitions.beginOperation()
        XCTAssertFalse(transitions.endOperation())
        XCTAssertTrue(transitions.endOperation())
        XCTAssertFalse(transitions.endOperation(), "an unmatched end releases nothing")

        // A release queued behind a start while nothing held the session is
        // dropped when an operation began before it ran.
        let start = Task { @MainActor in
            await transitions.run {
                log.entries.append("start")
                await log.waitForOpen()
            }
        }
        while log.entries.isEmpty { await Task.yield() }
        let release = Task { @MainActor in
            await transitions.releaseIfUnheld(keepsAudio: { false }) { log.entries.append("release") }
        }
        await Task.yield()
        transitions.beginOperation()
        log.open()
        await start.value
        await release.value
        XCTAssertEqual(log.entries, ["start"])
        XCTAssertTrue(transitions.endOperation())
        await transitions.releaseIfUnheld(keepsAudio: { true }) { log.entries.append("release") }
        XCTAssertEqual(log.entries, ["start"], "kept audio holds the session")
        transitions.sceneVisible = true
        await transitions.releaseIfUnheld(keepsAudio: { false }) { log.entries.append("release") }
        XCTAssertEqual(log.entries, ["start"], "a visible scene holds the session")
        transitions.sceneVisible = false
        await transitions.releaseIfUnheld(keepsAudio: { false }) { log.entries.append("release") }
        XCTAssertEqual(log.entries, ["start", "release"])
    }
}

@MainActor
private final class TransitionLog {
    var entries: [String] = []
    private var opened = false
    private var waiter: CheckedContinuation<Void, Never>?

    func waitForOpen() async {
        guard !opened else { return }
        await withCheckedContinuation { waiter = $0 }
    }

    func open() {
        opened = true
        waiter?.resume()
        waiter = nil
    }
}
