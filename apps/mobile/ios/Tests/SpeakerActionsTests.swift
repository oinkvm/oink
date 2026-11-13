import Combine
import XCTest
@testable import OinkAudioTestHost

@MainActor
final class SpeakerActionsTests: XCTestCase {
    func testAllSpeakerGestures() {
        for preferForward in [false, true] {
            XCTAssertEqual(SpeakerActions.action(mode: .off, preferForward: preferForward, longPress: false), preferForward ? .forward : .exit)
            XCTAssertEqual(SpeakerActions.action(mode: .off, preferForward: preferForward, longPress: true), preferForward ? .exit : .forward)
            XCTAssertEqual(SpeakerActions.action(mode: .exit, preferForward: preferForward, longPress: false), .off)
            XCTAssertEqual(SpeakerActions.action(mode: .exit, preferForward: preferForward, longPress: true), .forward)
            XCTAssertEqual(SpeakerActions.action(mode: .forward, preferForward: preferForward, longPress: false), .off)
            XCTAssertEqual(SpeakerActions.action(mode: .forward, preferForward: preferForward, longPress: true), .exit)
        }
    }

    func testPickerRequestAndCancellationPreserveExit() async {
        let fixture = Fixture()
        fixture.actions.press(exitOn: true, longPress: true, preferForward: false)
        XCTAssertEqual(fixture.requests, 1)
        XCTAssertFalse(fixture.actions.forwarding)
        XCTAssertEqual(fixture.actions.started, 0)
        XCTAssertTrue(fixture.exits.isEmpty)
        XCTAssertTrue(fixture.feedback.isEmpty)
        // A cancelled picker sends no actual-state event.
        fixture.state.send(false)
        XCTAssertTrue(fixture.exits.isEmpty)
        XCTAssertEqual(fixture.actions.started, 0)
    }

    func testSuccessOnlyAfterActualBroadcastStartAndOncePerStart() {
        let fixture = Fixture()
        fixture.actions.press(exitOn: true, longPress: true, preferForward: false)
        fixture.state.send(true)
        fixture.state.send(true) // paused/resumed remains active
        XCTAssertTrue(fixture.actions.forwarding)
        XCTAssertEqual(fixture.actions.started, 1)
        XCTAssertEqual(fixture.feedback, [.success])
        fixture.state.send(false)
        fixture.state.send(true)
        XCTAssertEqual(fixture.actions.started, 2)
        XCTAssertTrue(fixture.exits.isEmpty)
    }

    func testForwardToExitWaitsForStoppedAndIgnoresRepeatedPresses() async {
        let fixture = Fixture(active: true)
        fixture.actions.press(exitOn: false, longPress: true, preferForward: false)
        fixture.actions.press(exitOn: false, longPress: true, preferForward: false)
        XCTAssertEqual(fixture.stops, 1)
        XCTAssertTrue(fixture.exits.isEmpty)
        XCTAssertTrue(fixture.feedback.isEmpty)
        fixture.state.send(false)
        await fixture.waitForExit()
        XCTAssertEqual(fixture.exits, [true])
        XCTAssertEqual(fixture.feedback, [.success])
    }

    func testForwardTapOnlyStops() async {
        let fixture = Fixture(active: true)
        fixture.actions.press(exitOn: false, longPress: false, preferForward: true)
        XCTAssertEqual(fixture.stops, 1)
        fixture.state.send(false)
        XCTAssertTrue(fixture.exits.isEmpty)
        XCTAssertFalse(fixture.actions.forwarding)
    }

    func testRejectedPickerAndExitGiveErrorFeedback() async {
        let fixture = Fixture()
        fixture.acceptPicker = false
        fixture.actions.press(exitOn: false, longPress: true, preferForward: false)
        XCTAssertEqual(fixture.feedback, [.error])
        XCTAssertNotNil(fixture.actions.error)
        fixture.rejectExit = true
        fixture.actions.press(exitOn: false, longPress: true, preferForward: true)
        await fixture.waitForExit()
        XCTAssertEqual(fixture.feedback, [.error, .error])
    }

    @MainActor
    private final class Fixture {
        let state: CurrentValueSubject<Bool, Never>
        var requests = 0
        var stops = 0
        var exits: [Bool] = []
        var feedback: [SpeakerActions.Feedback] = []
        var acceptPicker = true
        var rejectExit = false
        private let applied = XCTestExpectation(description: "exit applied")
        lazy var actions = SpeakerActions(active: state.value, changes: state.eraseToAnyPublisher(),
            start: { self.requests += 1; return self.acceptPicker }, stop: { self.stops += 1 },
            setExit: {
                defer { self.applied.fulfill() }
                self.exits.append($0)
                if self.rejectExit { throw NSError(domain: "test", code: 1) }
            }, feedback: { self.feedback.append($0) })
        init(active: Bool = false) { state = CurrentValueSubject(active) }
        func waitForExit() async {
            let result = await XCTWaiter().fulfillment(of: [applied], timeout: 3)
            XCTAssertEqual(result, .completed)
        }
    }
}
