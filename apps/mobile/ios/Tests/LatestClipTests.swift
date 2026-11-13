import AppIntents
import UniformTypeIdentifiers
import XCTest
@testable import OinkAudioTestHost

/// The one-tap copy shortcut's result: text and images become the Shortcuts
/// output file, files and oversized images become readable failures.
final class LatestClipTests: XCTestCase {
    private var directory: URL!

    override func setUpWithError() throws {
        directory = FileManager.default.temporaryDirectory.appendingPathComponent("latest-clip-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        if let directory { try? FileManager.default.removeItem(at: directory) }
    }

    private func failure(_ current: MobileCurrentClipboard?) -> LatestClipFailure? {
        do {
            _ = try LatestClip.from(current)
            return nil
        } catch {
            return error as? LatestClipFailure
        }
    }

    func testTextIsReturnedAsUTF8PlainText() throws {
        let clip = try LatestClip.from(.text(text: "你好 Oink", sensitive: false, marker: nil))
        XCTAssertEqual(clip, .text("你好 Oink"))
        let file = try clip.intentFile()
        XCTAssertEqual(file.type, .utf8PlainText)
        XCTAssertEqual(file.data, Data("你好 Oink".utf8))
    }

    func testImageIsReturnedWithItsTypeAndBytes() throws {
        let path = directory.appendingPathComponent("image.png")
        let bytes = Data([0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3])
        try bytes.write(to: path)
        let clip = try LatestClip.from(.image(path: path.path, mime: "image/png", size: UInt64(bytes.count),
                                              sensitive: false, marker: nil))
        XCTAssertEqual(clip, .image(path: path.path, mime: "image/png"))
        let file = try clip.intentFile()
        XCTAssertEqual(file.type, .png)
        XCTAssertEqual(file.filename, "image.png")
        XCTAssertEqual(file.data, bytes)
    }

    func testImageTypeFallsBackToTheFileExtension() throws {
        let path = directory.appendingPathComponent("photo.jpg")
        try Data([0xFF, 0xD8, 0xFF]).write(to: path)
        let file = try LatestClip.image(path: path.path, mime: nil).intentFile()
        XCTAssertEqual(file.type, .jpeg)
    }

    func testMissingImageBodyIsAnInterruption() {
        XCTAssertThrowsError(try LatestClip.image(path: directory.appendingPathComponent("gone.png").path, mime: "image/png").intentFile()) {
            XCTAssertEqual($0 as? LatestClipFailure, .interrupted)
        }
    }

    func testFilesAndFoldersPointToTheTransferPage() {
        XCTAssertEqual(failure(.unsupported(kind: .file)), .file)
        XCTAssertEqual(failure(.unsupported(kind: .folder)), .file)
        XCTAssertEqual(failure(.unsupported(kind: .richText)), .unsupported)
        XCTAssertTrue(LatestClipFailure.file.message.contains(AppText.pick("传输页", "Transfer page")))
    }

    func testNothingCurrentAndEmptyTextAreEmpty() {
        XCTAssertEqual(failure(nil), .empty)
        XCTAssertEqual(failure(.text(text: "", sensitive: false, marker: nil)), .empty)
    }

    func testOversizedImageReportsSizeAndLimit() {
        let limit: UInt64 = 32 * 1024 * 1024
        XCTAssertEqual(failure(.imageTooLarge(size: 40 * 1024 * 1024, limit: limit)),
                       .imageTooLarge(size: 40 * 1024 * 1024, limit: limit))
        let message = LatestClipFailure.imageTooLarge(size: 40 * 1024 * 1024, limit: limit).message
        XCTAssertTrue(message.contains("40.0 MB"), message)
        XCTAssertTrue(message.contains("32.0 MB"), message)
        XCTAssertEqual(LatestClipFailure.imageTooLarge(size: 1, limit: 2).errorDescription,
                       LatestClipFailure.imageTooLarge(size: 1, limit: 2).message)
    }
}

/// A session shaped like CoreSession's: starts and releases go through the
/// real `SessionTransitions`; the clip read stands in for the core's answer.
@MainActor
private final class FakeClipSession: LatestClipSession {
    let transitions = SessionTransitions()
    private(set) var running: Bool
    var keepsAudio = false
    var answer: Result<LatestClip, Error> = .success(.text("最新"))
    private(set) var starts = 0
    private(set) var releases = 0
    /// Runs inside every start, as the audio owner's eligibility report does
    /// in the app: there it makes the background lifecycle queue a release.
    var duringStart: (() -> Void)?

    init(running: Bool, sceneVisible: Bool) {
        self.running = running
        transitions.sceneVisible = sceneVisible
    }

    func beginBackgroundOperation() { transitions.beginOperation() }

    func latestSharedClip() async throws -> LatestClip {
        await transitions.settle()
        if !running {
            await transitions.run { [self] in
                starts += 1
                duringStart?()
                await Task.yield() // the core's start awaits Rust
                running = true
            }
        }
        guard running else { throw OinkFailure(message: "网络会话尚未启动。") }
        // The read awaits the core; a release running now cuts it off.
        for _ in 0..<3 { await Task.yield() }
        guard running else { throw LatestClipFailure.interrupted }
        return try answer.get()
    }

    func finishBackgroundOperation() async {
        guard transitions.endOperation() else { return }
        await releaseIfUnheld()
    }

    func releaseIfUnheld() async {
        await transitions.releaseIfUnheld(keepsAudio: { [self] in keepsAudio }) { [self] in
            releases += 1
            running = false
        }
    }

    /// AppLifecycle's background reconcile: a release nobody awaits.
    func lifecycleRelease() {
        Task { @MainActor in await self.releaseIfUnheld() }
    }
}

/// The one-tap copy shortcut's run against the app's session (review M29):
/// it uses the live session, or starts one and releases it afterwards, and a
/// background release never ends the session under it.
final class LatestClipShortcutTests: XCTestCase {
    private func text(_ file: IntentFile) -> String? { String(data: file.data, encoding: .utf8) }

    @MainActor func testALiveSessionAnswersAndStaysRunning() async throws {
        let session = FakeClipSession(running: true, sceneVisible: true)
        let file = try await LatestClip.shortcutFile(from: session)
        XCTAssertEqual(text(file), "最新")
        await session.transitions.settle()
        XCTAssertEqual(session.starts, 0)
        XCTAssertEqual(session.releases, 0)
        XCTAssertTrue(session.running)
    }

    @MainActor func testWithoutASessionOneIsStartedAndReleasedAfterTheAnswer() async throws {
        let session = FakeClipSession(running: false, sceneVisible: false)
        let file = try await LatestClip.shortcutFile(from: session)
        XCTAssertEqual(text(file), "最新")
        XCTAssertEqual(session.starts, 1)
        XCTAssertEqual(session.releases, 1)
        XCTAssertFalse(session.running)
    }

    /// The reported bug: the app in the background starts the session for
    /// the intent, the audio owner's report inside that start queues the
    /// lifecycle's release, and that release ended the session before the
    /// read (「网络会话已启动」 surfaced as the error) or during it.
    @MainActor func testABackgroundReleaseQueuedDuringTheStartLeavesTheSessionToTheIntent() async throws {
        let session = FakeClipSession(running: false, sceneVisible: false)
        session.duringStart = { [unowned session] in session.lifecycleRelease() }
        let file = try await LatestClip.shortcutFile(from: session)
        XCTAssertEqual(text(file), "最新")
        await session.transitions.settle()
        XCTAssertEqual(session.starts, 1)
        XCTAssertEqual(session.releases, 1, "released once, after the intent")
        XCTAssertFalse(session.running)
    }

    /// Recently backgrounded: the session still runs and the lifecycle's
    /// release is queued when the intent begins. The intent keeps it and
    /// releases it afterwards; a running session is not left behind.
    @MainActor func testARecentlyBackgroundedSessionIsUsedThenReleased() async throws {
        let session = FakeClipSession(running: true, sceneVisible: false)
        session.lifecycleRelease()
        let file = try await LatestClip.shortcutFile(from: session)
        XCTAssertEqual(text(file), "最新")
        await session.transitions.settle()
        XCTAssertEqual(session.starts, 0)
        XCTAssertEqual(session.releases, 1)
        XCTAssertFalse(session.running)
    }

    @MainActor func testAFailureIsReturnedAndTheStartedSessionReleased() async {
        let session = FakeClipSession(running: false, sceneVisible: false)
        session.answer = .failure(LatestClipFailure.noComputer)
        do {
            _ = try await LatestClip.shortcutFile(from: session)
            XCTFail("expected noComputer")
        } catch {
            XCTAssertEqual(error as? LatestClipFailure, .noComputer)
        }
        XCTAssertEqual(session.releases, 1)
        XCTAssertFalse(session.running)
    }

    @MainActor func testKeptAudioAndAVisibleSceneKeepTheSession() async throws {
        let audio = FakeClipSession(running: true, sceneVisible: false)
        audio.keepsAudio = true
        _ = try await LatestClip.shortcutFile(from: audio)
        XCTAssertEqual(audio.releases, 0)
        XCTAssertTrue(audio.running)

        let visible = FakeClipSession(running: false, sceneVisible: true)
        _ = try await LatestClip.shortcutFile(from: visible)
        XCTAssertEqual(visible.starts, 1)
        XCTAssertEqual(visible.releases, 0)
        XCTAssertTrue(visible.running)
    }

    @MainActor func testOverlappingIntentsShareOneSessionReleasedAfterTheLast() async throws {
        let session = FakeClipSession(running: false, sceneVisible: false)
        async let first = LatestClip.shortcutFile(from: session)
        async let second = LatestClip.shortcutFile(from: session)
        let files = try await [first, second]
        XCTAssertEqual(files.map(text), ["最新", "最新"])
        XCTAssertEqual(session.starts, 1)
        XCTAssertEqual(session.releases, 1)
        XCTAssertFalse(session.running)
    }
}
