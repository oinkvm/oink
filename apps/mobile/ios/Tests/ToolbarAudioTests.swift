import XCTest
@testable import OinkAudioTestHost

/// The top-bar speaker / microphone of this phone use the device-card states, so
/// they draw the same PC icon and badge as a phone (or a PC) in that state.
final class ToolbarAudioTests: XCTestCase {
    func testSpeakerOnIsTheExitState() {
        XCTAssertNil(DeviceControls.toolbarSpeaker(on: false, failed: false))
        XCTAssertEqual(DeviceControls.toolbarSpeaker(on: true, failed: false), .outlet)
        XCTAssertEqual(DeviceControls.toolbarSpeaker(on: true, failed: true), .unavailable)
    }

    func testMicrophoneOnIsTheSendingState() {
        XCTAssertNil(DeviceControls.toolbarMicrophone(on: false, failed: false))
        XCTAssertEqual(DeviceControls.toolbarMicrophone(on: true, failed: false), .send)
        XCTAssertEqual(DeviceControls.toolbarMicrophone(on: true, failed: true), .unavailable)
    }
}
