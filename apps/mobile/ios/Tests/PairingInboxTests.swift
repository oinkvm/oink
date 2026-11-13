import XCTest
@testable import OinkAudioTestHost

/// Which session the pairing sheet opens.
final class PairingInboxTests: XCTestCase {
    private func row(_ id: String, incoming: Bool = true, phase: UInt32 = 2, ended: Bool = false, device: String = "") -> PairingInbox.Row {
        PairingInbox.Row(id: id, incoming: incoming, phase: phase, ended: ended, device: device)
    }

    // User 2026-11-11: after a failed or a successful pairing the next attempt opened the old
    // session's sheet (配对失败, or a success sheet that could not be closed). The core keeps
    // ended sessions in its snapshot, sorted by the random pairing key, so the old one can come
    // last; the snapshots below put it there.

    func testANewPairingOpensItsOwnSessionNotAnEarlierFailedOne() {
        let rows = [row("new", incoming: false, phase: 0, device: "studio"),
                    row("old", incoming: false, phase: 1, ended: true, device: "office")]
        XCTAssertEqual(PairingInbox.startedSession(rows, started: "new"), "new")
    }

    func testANewPairingWithTheSameDeviceOpensItsOwnSession() {
        let rows = [row("new", incoming: false, phase: 0, device: "studio"),
                    row("old", incoming: false, phase: 1, ended: true, device: "studio")]
        XCTAssertEqual(PairingInbox.startedSession(rows, started: "new"), "new")
    }

    func testANewPairingAfterASuccessfulOneOpensItsOwnSession() {
        let rows = [row("new", incoming: false, phase: 0, device: "studio"),
                    row("done", incoming: false, phase: PairingInbox.completed, device: "office")]
        XCTAssertEqual(PairingInbox.startedSession(rows, started: "new"), "new")
    }

    // Requests wait in the list until the user accepts one (user 2026-11-11); accepting opens
    // that request's own session, never an earlier one of the same device.
    func testAnAcceptedRequestOpensItsOwnSessionNotAnEarlierOneOfThatDevice() {
        let rows = [row("old", phase: 1, ended: true, device: "studio"),
                    row("done", phase: PairingInbox.completed, device: "studio"),
                    row("request", phase: 2, device: "studio")]
        XCTAssertEqual(PairingInbox.startedSession(rows, started: "request"), "request")
        XCTAssertNil(PairingInbox.startedSession(Array(rows.prefix(2)), started: "request"))
    }

    func testATapDoesNotReopenAFailedOrCompletedSessionOfThatDevice() {
        let rows = [row("failed", incoming: false, phase: 1, ended: true, device: "studio"),
                    row("done", incoming: false, phase: PairingInbox.completed, device: "studio")]
        XCTAssertNil(PairingInbox.runningSession(rows, device: "studio"))
        XCTAssertEqual(PairingInbox.runningSession(rows + [row("live", incoming: false, phase: 1, device: "studio")],
                                                   device: "studio"), "live")
    }
}
