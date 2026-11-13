import XCTest
@testable import OinkAudioTestHost

/// The share card stage maths against the key frames of share-anim SPEC §5.4, §5.8 and §5.9
/// (positions within 0.1, opacities and scales within 0.005).
final class ShareAnimTests: XCTestCase {
    private func degrees(_ radians: CGFloat) -> CGFloat { radians * 180 / .pi }

    func testComputerKeyFrames() {
        let f = ShareAnim.frame(0.30, .computer)
        XCTAssertEqual(f.x, 139.4, accuracy: 0.1); XCTAssertEqual(f.y, 28.2, accuracy: 0.1)
        XCTAssertEqual(degrees(f.angle), 9.1, accuracy: 0.1); XCTAssertEqual(f.opacity, 1, accuracy: 0.005)
        XCTAssertEqual(ShareAnim.trailLength(f.trail), 68.4, accuracy: 0.1)
        let g = ShareAnim.frame(0.66, .computer)
        XCTAssertEqual(g.opacity, 0.675, accuracy: 0.005); XCTAssertEqual(g.nudge, 1.025, accuracy: 0.005)
    }

    func testPhoneKeyFrames() {
        let f = ShareAnim.frame(0.30, .phone)
        XCTAssertEqual(f.x, 141.0, accuracy: 0.1); XCTAssertEqual(f.y, 28.8, accuracy: 0.1)
        XCTAssertEqual(degrees(f.angle), 9.5, accuracy: 0.1)
        XCTAssertEqual(ShareAnim.trailLength(f.trail), 69.9, accuracy: 0.1)
        let landed = ShareAnim.frame(0.72, .phone)
        XCTAssertEqual(landed.x, 199.0, accuracy: 0.1); XCTAssertEqual(landed.y, 50.5, accuracy: 0.1)
        XCTAssertEqual(degrees(landed.angle), 29.3, accuracy: 0.1); XCTAssertEqual(landed.nudge, 1.048, accuracy: 0.005)
    }

    func testReducedMotionPoseIsTheTopOfTheArc() {
        let computer = ShareAnim.frame(ShareAnim.staticT, .computer), phone = ShareAnim.frame(ShareAnim.staticT, .phone)
        XCTAssertEqual(computer.x, 120.25, accuracy: 0.1); XCTAssertEqual(computer.y, 26.75, accuracy: 0.1)
        XCTAssertEqual(phone.x, 121.25, accuracy: 0.1); XCTAssertEqual(phone.y, 27.13, accuracy: 0.1)
    }

    func testHandoffKeyValues() {
        let start = ShareAnim.handoff(0)
        XCTAssertEqual(start.offline, 1, accuracy: 0.005); XCTAssertEqual(start.logo, 1, accuracy: 0.005); XCTAssertNil(start.t)
        let early = ShareAnim.handoff(80)
        XCTAssertEqual(early.offline, 0.675, accuracy: 0.005); XCTAssertEqual(early.bar, 0.533, accuracy: 0.005); XCTAssertNil(early.t)
        let lead = ShareAnim.handoff(160)
        XCTAssertEqual(lead.offline, 0, accuracy: 0.005); XCTAssertEqual(lead.logo, 1, accuracy: 0.005); XCTAssertEqual(lead.t ?? -1, 0, accuracy: 0.001)
        let mid = ShareAnim.handoff(230)
        XCTAssertEqual(mid.logo, 0.161, accuracy: 0.005); XCTAssertEqual(mid.t ?? -1, 0.050, accuracy: 0.001)
        let end = ShareAnim.handoff(300)
        XCTAssertEqual(end.logo, 0, accuracy: 0.005); XCTAssertEqual(end.t ?? -1, 0.100, accuracy: 0.001)
    }
}
