import XCTest
@testable import OinkAudioTestHost

/// Control Center toggles (扬声器 / 麦克风): the App Group state the app writes
/// and the controls extension reads, and the app's liveness lock that turns a
/// stale "on" (app killed with a switch on) into off.
final class ControlsStateTests: XCTestCase {
    private var root: URL!

    override func setUpWithError() throws {
        root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: root)
    }

    func testMissingOrUnreadableStateIsOff() throws {
        XCTAssertEqual(ControlsStore.read(in: root), ControlsState())
        try Data("{".utf8).write(to: root.appendingPathComponent(ControlsStore.stateName))
        XCTAssertEqual(ControlsStore.read(in: root), ControlsState())
        XCTAssertFalse(ControlsStore.current(in: root).speaker)
    }

    func testStateShowsOnlyWhileTheAppHoldsItsLock() throws {
        let state = ControlsState(speaker: true, microphone: true, accent: 0x14B8A6)
        try ControlsStore.write(state, in: root)
        XCTAssertEqual(ControlsStore.read(in: root), state)
        // The app is not running (nobody holds the lock): off, accent kept.
        XCTAssertEqual(ControlsStore.current(in: root), ControlsState(speaker: false, microphone: false, accent: 0x14B8A6))

        let liveness = ControlsLiveness()
        liveness.hold(in: root)
        XCTAssertTrue(liveness.isHolding)
        XCTAssertTrue(ControlsLiveness.isHeld(in: root))
        XCTAssertEqual(ControlsStore.current(in: root), state)
        // The probe did not take the lock from the holder.
        XCTAssertTrue(ControlsLiveness.isHeld(in: root))

        liveness.release()
        XCTAssertFalse(ControlsLiveness.isHeld(in: root))
        XCTAssertEqual(ControlsStore.current(in: root).speaker, false)
        // A probe leaves the lock free for the app's next session.
        liveness.hold(in: root)
        XCTAssertTrue(ControlsLiveness.isHeld(in: root))
        liveness.release()
    }

    func testBroadcastStateSurvivesMainSessionReleaseAndClearsWithActualStop() throws {
        let pipe = try XCTUnwrap(OinkBroadcastPipe(root: root.path))
        XCTAssertTrue(pipe.claimBroadcast())
        defer { pipe.endBroadcast() }
        let state = ControlsState(speaker: true, microphone: true, accent: 0x14B8A6)
        try ControlsStore.write(state, in: root)
        try ForwardControlStore.write(ForwardControlsState(active: true, paused: true, exit: "Studio PC"), in: root)
        let current = ControlsStore.current(in: root)
        XCTAssertTrue(current.forwarding)
        XCTAssertTrue(current.forwardingPaused)
        XCTAssertEqual(current.forwardExit, "Studio PC")
        XCTAssertFalse(current.speaker)
        XCTAssertFalse(current.microphone)
        XCTAssertEqual(ControlsStore.read(in: root), state, "extension publication preserves main-owned fields")
        pipe.endBroadcast()
        XCTAssertFalse(ControlsStore.current(in: root).forwarding)
        XCTAssertFalse(ControlsStore.current(in: root).forwardingPaused)
        XCTAssertNil(ControlsStore.current(in: root).forwardExit)
    }

    func testForwardActivityTexts() {
        XCTAssertEqual(AudioActivityText.title(.speaker, forwarding: true), "正在转发音频")
        XCTAssertEqual(AudioActivityText.title(.speaker, forwarding: true, paused: true), "音频转发已暂停")
        XCTAssertEqual(AudioActivityText.subtitle(.speaker, peers: ["Studio PC"], forwarding: true), "出口 Studio PC")
        XCTAssertEqual(AudioActivityText.subtitle(.speaker, peers: [], forwarding: true), "等待音频出口")
        XCTAssertEqual(AudioActivityText.stop(.speaker, forwarding: true), "停止转发")
    }

    func testLiveActivityStaysThroughAnInterruptionRelease() {
        XCTAssertTrue(AudioActivityRule.shows(switchOn: true, sessionLive: true, interruptionRelease: false))
        XCTAssertTrue(AudioActivityRule.shows(switchOn: true, sessionLive: false, interruptionRelease: true),
                      "an interruption released the session: the card stays")
        XCTAssertFalse(AudioActivityRule.shows(switchOn: true, sessionLive: false, interruptionRelease: false),
                       "the session ended for another reason")
        XCTAssertFalse(AudioActivityRule.shows(switchOn: false, sessionLive: false, interruptionRelease: true),
                       "the switch went off")
        XCTAssertFalse(AudioActivityRule.shows(switchOn: false, sessionLive: true, interruptionRelease: false))
    }

    func testLiveActivityTexts() {
        XCTAssertEqual(AudioActivityText.title(.speaker), "正在作为音频出口")
        XCTAssertEqual(AudioActivityText.title(.microphone), "麦克风共享中")
        XCTAssertEqual(AudioActivityText.subtitle(.speaker, peers: ["Studio PC"]), "播放 Studio PC 的声音")
        XCTAssertEqual(AudioActivityText.subtitle(.microphone, peers: ["Studio PC", "MacBook"]), "发送到 Studio PC、MacBook")
        // Neither role needs a connection: without peers it still runs.
        XCTAssertEqual(AudioActivityText.subtitle(.speaker, peers: []), "等待其它设备的声音")
        XCTAssertEqual(AudioActivityText.subtitle(.microphone, peers: []), "等待电脑使用")
        XCTAssertEqual(AudioActivityText.stop(.speaker), "停止接收")
        XCTAssertEqual(AudioActivityText.stop(.microphone), "停止共享")
    }
}
